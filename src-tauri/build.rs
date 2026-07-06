use std::path::Path;

fn main() {
    embed_translations();
    tauri_build::build()
}

/// Generate `$OUT_DIR/embedded_translations.rs` with the contents of every
/// `translations/*.json` present at compile time.
///
/// `en.json` is the committed source of truth and always present; the other
/// locales are gitignored and only exist when the release workflow downloads
/// them from Lokalise before building, so a dev build is English-only while a
/// release build embeds every translated locale. Embedding (rather than
/// bundling as resources) means the lookup can never fail at runtime on a
/// missing or unreadable file.
fn embed_translations() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let translations_dir = Path::new(&manifest_dir).join("translations");

    // Re-run when locale files are added or removed, not just edited.
    println!("cargo:rerun-if-changed={}", translations_dir.display());

    let mut locales: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&translations_dir)
        .expect("translations/ directory is missing; en.json must exist")
        .filter_map(|entry| {
            let path = entry.expect("failed to read translations/ entry").path();
            let stem = path.file_stem()?.to_str()?.to_string();
            (path.extension()? == "json").then_some((stem, path))
        })
        .collect();

    // Deterministic embed order (and thus deterministic builds) regardless of
    // filesystem iteration order.
    locales.sort();

    assert!(
        locales.iter().any(|(stem, _)| stem == "en"),
        "translations/en.json is missing"
    );

    let mut generated = String::from(
        "/// Locale stem → raw JSON, for every translation file present at compile time.\n\
         pub(crate) static EMBEDDED_TRANSLATIONS: &[(&str, &str)] = &[\n",
    );
    for (stem, path) in &locales {
        // Validate at build time so a corrupt download fails the build rather
        // than panicking (or silently falling back) at runtime.
        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        serde_json::from_str::<serde_json::Value>(&contents)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
        // Reference the file relative to CARGO_MANIFEST_DIR rather than by
        // the absolute path this build ran from, so the generated source is
        // stable across machines (reproducible builds, no local paths leaking
        // into build artifacts). Forward slashes work on Windows here too.
        generated.push_str(&format!(
            "    ({stem:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/translations/{stem}.json\"))),\n",
        ));
    }
    generated.push_str("];\n");

    std::fs::write(
        Path::new(&out_dir).join("embedded_translations.rs"),
        generated,
    )
    .expect("failed to write embedded_translations.rs");
}
