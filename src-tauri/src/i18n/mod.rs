//! Runtime UI translations.
//!
//! `translations/en.json` is the committed source of truth for every
//! user-facing string (tray menu, dialogs, notifications). Other locales are
//! translated on Lokalise, downloaded by the release workflow, and embedded
//! into the binary at compile time by `build.rs` (see
//! `EMBEDDED_TRANSLATIONS`), so dev builds are English-only and release
//! builds carry every locale.
//!
//! Keys are dot-separated paths into the nested JSON
//! (e.g. `tray.open_dashboard`). Lookups resolve with a per-key English
//! fallback, so a partially translated locale shows English for the missing
//! keys rather than raw key names. Values may contain named `{placeholder}`
//! tokens filled by [`t_with`].
//!
//! The locale is auto-detected from the system, with the
//! `ESPHOME_DESKTOP_LANGUAGE` environment variable as an explicit override.
//! (Deliberately no per-app language setting for now; if users report the
//! detected language mismatching the device-builder's, revisit syncing the
//! choice across the stack.)
//!
//! Logs and CLI/control-channel output intentionally stay English — this
//! module is only for strings a user sees in the UI.

use std::collections::HashMap;
use std::sync::OnceLock;

use tracing::warn;

include!(concat!(env!("OUT_DIR"), "/embedded_translations.rs"));

/// The base locale: always embedded, always complete.
const BASE_LOCALE: &str = "en";

/// Environment override for the UI language, checked before every other
/// source. Mainly for testing a locale different from the OS one.
const LOCALE_ENV_VAR: &str = "ESPHOME_DESKTOP_LANGUAGE";

/// The active key → message table, built once on first use.
static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();

fn table() -> &'static HashMap<String, String> {
    TABLE.get_or_init(|| {
        let locale = choose_locale(std::env::var(LOCALE_ENV_VAR).ok(), sys_locale::get_locale());
        build_table(test_pinned(locale).as_deref(), EMBEDDED_TRANSLATIONS)
    })
}

/// Pin unit tests to English regardless of the host machine's locale or env,
/// or of extra locale files a release-style build has downloaded next to
/// en.json. The locale-selection pieces (`choose_locale`, `pick_locale`,
/// `build_table`) are tested directly.
#[cfg(test)]
fn test_pinned(_locale: Option<String>) -> Option<String> {
    None
}

#[cfg(not(test))]
fn test_pinned(locale: Option<String>) -> Option<String> {
    locale
}

/// Look up the message for `key` in the active locale (English fallback).
///
/// A key missing from `en.json` is a bug caught by the consistency tests
/// below; at runtime it degrades to returning the key itself.
pub(crate) fn t(key: &str) -> String {
    match table().get(key) {
        Some(message) => message.clone(),
        None => {
            warn!("missing translation key: {}", key);
            key.to_string()
        }
    }
}

/// Look up the message for `key` and fill its named `{placeholder}` tokens.
pub(crate) fn t_with(key: &str, args: &[(&str, &str)]) -> String {
    interpolate(&t(key), args)
}

/// Replace each `{name}` token with its value. Unknown tokens are left
/// verbatim so a template/args mismatch stays visible instead of vanishing.
fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    // Single pass over the original template, so substituted values are never
    // re-scanned — an arg value that happens to contain `{other}` (say, a
    // brace-y error message) must land verbatim, not trigger a second
    // substitution.
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // Unmatched '{' — keep the tail verbatim.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match args.iter().find(|(arg, _)| *arg == name) {
            Some((_, value)) => out.push_str(value),
            None => {
                // Unknown token: kept verbatim so a mismatch stays visible.
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Pick the locale to use: a non-empty env override wins, then the system
/// locale. `None` means English.
fn choose_locale(env_value: Option<String>, system_locale: Option<String>) -> Option<String> {
    match env_value {
        Some(v) if !v.is_empty() => Some(v),
        _ => system_locale,
    }
}

/// Normalize a locale tag for comparison: lowercase, hyphen-separated, and
/// stripped of any encoding/variant suffix (`pt_BR.UTF-8` → `pt-br`).
/// Sources disagree only on separator and case (BCP 47 `zh-CN` vs POSIX
/// `zh_CN`), so this avoids a per-locale mapping table.
fn normalize_locale(locale: &str) -> String {
    let stripped = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    stripped.to_lowercase().replace('_', "-")
}

/// Fallback chain for a locale tag, most specific first:
/// `zh-Hans-CN` → `["zh-hans-cn", "zh-hans", "zh"]`.
fn locale_candidates(locale: &str) -> Vec<String> {
    let normalized = normalize_locale(locale);
    if normalized.is_empty() {
        return Vec::new();
    }
    let parts: Vec<&str> = normalized.split('-').collect();
    (1..=parts.len())
        .rev()
        .map(|n| parts[..n].join("-"))
        .collect()
}

/// The script subtag of a normalized locale, if present: `hans` in
/// `zh-hans-cn`. Scripts are the only four-letter subtags in BCP 47
/// (languages are 2–3 letters, regions two letters or three digits), so after
/// normalization a four-letter alphabetic segment identifies the script
/// without a lookup table.
fn locale_script(normalized: &str) -> Option<&str> {
    normalized
        .split('-')
        .find(|segment| segment.len() == 4 && segment.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Match the requested locale against the available locale stems.
///
/// Tries each fallback candidate for an exact (normalized) match, then falls
/// back to the first available stem for the same language (`fr` requested,
/// only `fr-CA` shipped). Returns the stem exactly as embedded.
///
/// The language-only fallback never crosses a *script* boundary: a
/// Simplified-Chinese (`zh-Hans`) user must not be served Traditional
/// (`zh-Hant`) just because they share the `zh` language — the two are
/// different writing systems, so falling through to English (the base) is the
/// safer miss (esphome/esphome-desktop#373). A request that carries no script
/// keeps the permissive same-language fallback.
fn pick_locale<'a>(requested: &str, available: &[&'a str]) -> Option<&'a str> {
    let candidates = locale_candidates(requested);
    for candidate in &candidates {
        if let Some(stem) = available
            .iter()
            .find(|stem| normalize_locale(stem) == *candidate)
        {
            return Some(stem);
        }
    }
    // Language-only fallback: any available stem whose language matches the
    // broadest candidate (which is the bare language code) and whose script,
    // if both sides name one, agrees.
    let language = candidates.last()?;
    let requested_normalized = normalize_locale(requested);
    let requested_script = locale_script(&requested_normalized);
    available
        .iter()
        .find(|stem| {
            let normalized = normalize_locale(stem);
            let language_matches = normalized.split('-').next() == Some(language.as_str());
            let script_agrees = match (requested_script, locale_script(&normalized)) {
                (Some(requested), Some(available)) => requested == available,
                _ => true,
            };
            language_matches && script_agrees
        })
        .copied()
}

/// Flatten nested JSON into dot-separated keys. Non-string leaves are
/// ignored — the translation files only carry strings.
fn flatten(prefix: &str, value: &serde_json::Value, out: &mut HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (name, child) in map {
                let key = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(&key, child, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        _ => {}
    }
}

/// Parse one embedded locale into a flat key → message map. The JSON was
/// validated by `build.rs`, so a parse failure here is unreachable in
/// practice; degrade to an empty map (English fallback) rather than panic.
fn parse_locale(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => flatten("", &value, &mut out),
        Err(e) => warn!("failed to parse embedded translation: {}", e),
    }
    out
}

/// Build the active table: the English base overlaid with the requested
/// locale's non-empty messages (Lokalise exports untranslated keys as empty
/// strings; those must fall back to English, not blank the UI).
fn build_table(requested: Option<&str>, embedded: &[(&str, &str)]) -> HashMap<String, String> {
    let base_raw = embedded
        .iter()
        .find(|(stem, _)| *stem == BASE_LOCALE)
        .map(|(_, raw)| *raw)
        .unwrap_or("{}");
    let mut table = parse_locale(base_raw);

    let available: Vec<&str> = embedded
        .iter()
        .map(|(stem, _)| *stem)
        .filter(|stem| *stem != BASE_LOCALE)
        .collect();
    let Some(stem) = requested.and_then(|req| pick_locale(req, &available)) else {
        return table;
    };
    let raw = embedded
        .iter()
        .find(|(s, _)| *s == stem)
        .map(|(_, raw)| *raw)
        .unwrap_or("{}");
    for (key, message) in parse_locale(raw) {
        // Only overlay keys that exist in English: a stale Lokalise key must
        // not resurrect a string the code no longer uses (harmless) or, worse,
        // mask a typo'd lookup with a stale message.
        if !message.is_empty() && table.contains_key(&key) {
            table.insert(key, message);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_is_embedded() {
        assert!(EMBEDDED_TRANSLATIONS
            .iter()
            .any(|(stem, _)| *stem == BASE_LOCALE));
    }

    #[test]
    fn t_returns_english_message() {
        assert_eq!(t("tray.open_dashboard"), "Open Dashboard");
    }

    #[test]
    fn t_missing_key_returns_key() {
        // Uppercase on purpose: the used-keys consistency scan below skips
        // non-snake-case literals, so this deliberately-missing key doesn't
        // trip `all_used_keys_exist_in_en_json`.
        assert_eq!(t("no.such.KEY"), "no.such.KEY");
    }

    #[test]
    fn t_with_fills_placeholders() {
        assert_eq!(t_with("tray.port", &[("port", "6052")]), "Port: 6052");
    }

    #[test]
    fn interpolate_replaces_all_and_repeated() {
        assert_eq!(
            interpolate("{a} and {b} and {a}", &[("a", "1"), ("b", "2")]),
            "1 and 2 and 1"
        );
    }

    #[test]
    fn interpolate_leaves_unknown_tokens() {
        assert_eq!(interpolate("hello {name}", &[]), "hello {name}");
    }

    #[test]
    fn interpolate_never_rescans_substituted_values() {
        // A value that itself looks like a placeholder (e.g. a brace-y error
        // message) must land verbatim, not trigger a second substitution.
        assert_eq!(
            interpolate("{a}", &[("a", "{b}"), ("b", "X")]),
            "{b}",
            "substituted value was re-processed as a template"
        );
    }

    #[test]
    fn interpolate_keeps_unmatched_brace_verbatim() {
        assert_eq!(interpolate("50% {done", &[("done", "X")]), "50% {done");
        assert_eq!(interpolate("a } b", &[]), "a } b");
    }

    #[test]
    fn choose_locale_prefers_env_override() {
        assert_eq!(
            choose_locale(Some("fr".into()), Some("de".into())),
            Some("fr".into())
        );
    }

    #[test]
    fn choose_locale_empty_env_falls_back_to_system() {
        assert_eq!(
            choose_locale(Some(String::new()), Some("de".into())),
            Some("de".into())
        );
        assert_eq!(choose_locale(None, Some("de".into())), Some("de".into()));
        assert_eq!(choose_locale(None, None), None);
    }

    #[test]
    fn normalize_locale_handles_posix_tags() {
        assert_eq!(normalize_locale("pt_BR.UTF-8"), "pt-br");
        assert_eq!(normalize_locale("en_US@euro"), "en-us");
        assert_eq!(normalize_locale("zh-CN"), "zh-cn");
        assert_eq!(normalize_locale(""), "");
    }

    #[test]
    fn locale_candidates_most_specific_first() {
        assert_eq!(
            locale_candidates("zh-Hans-CN"),
            vec!["zh-hans-cn", "zh-hans", "zh"]
        );
        assert_eq!(locale_candidates("en"), vec!["en"]);
        assert!(locale_candidates("").is_empty());
    }

    #[test]
    fn pick_locale_exact_match() {
        assert_eq!(pick_locale("fr", &["de", "fr"]), Some("fr"));
    }

    #[test]
    fn pick_locale_region_falls_back_to_language() {
        assert_eq!(pick_locale("fr-CA", &["de", "fr"]), Some("fr"));
    }

    #[test]
    fn pick_locale_normalizes_separators() {
        assert_eq!(pick_locale("zh_CN.UTF-8", &["zh-CN"]), Some("zh-CN"));
    }

    #[test]
    fn pick_locale_language_matches_regional_stem() {
        // Only a regional variant is shipped; a bare-language request should
        // still land on it.
        assert_eq!(pick_locale("fr", &["de", "fr-CA"]), Some("fr-CA"));
    }

    #[test]
    fn pick_locale_no_match() {
        assert_eq!(pick_locale("ja", &["de", "fr"]), None);
        assert_eq!(pick_locale("", &["de", "fr"]), None);
    }

    #[test]
    fn pick_locale_does_not_cross_script_boundary() {
        // A Simplified-Chinese user must not be handed Traditional via the
        // language-only fallback (and vice versa): different writing systems,
        // so fall through to English instead (esphome-desktop#373).
        assert_eq!(pick_locale("zh-Hans-CN", &["de", "zh-Hant"]), None);
        assert_eq!(pick_locale("zh-Hant-TW", &["de", "zh-Hans"]), None);
    }

    #[test]
    fn pick_locale_same_script_still_falls_back() {
        // Same script, different region — the fallback should still land.
        assert_eq!(
            pick_locale("zh-Hans-CN", &["de", "zh-Hans-SG"]),
            Some("zh-Hans-SG")
        );
        assert_eq!(pick_locale("zh-Hans", &["zh-Hans-CN"]), Some("zh-Hans-CN"));
    }

    #[test]
    fn pick_locale_scriptless_request_keeps_permissive_fallback() {
        // A request without a script keeps the same-language fallback: a bare
        // `zh` has no script preference, and non-scripted languages (`fr`) are
        // unaffected.
        assert_eq!(pick_locale("zh", &["zh-Hant"]), Some("zh-Hant"));
        assert_eq!(pick_locale("fr", &["de", "fr-CA"]), Some("fr-CA"));
    }

    #[test]
    fn locale_script_extracts_only_the_script_subtag() {
        assert_eq!(locale_script("zh-hans-cn"), Some("hans"));
        assert_eq!(locale_script("sr-latn"), Some("latn"));
        // Region (two letters / three digits) and bare language carry no script.
        assert_eq!(locale_script("zh-cn"), None);
        assert_eq!(locale_script("en"), None);
    }

    #[test]
    fn flatten_ignores_non_string_leaves() {
        let value = serde_json::json!({
            "a": {"b": "msg", "n": 3, "arr": ["x"]},
            "top": "level"
        });
        let mut out = HashMap::new();
        flatten("", &value, &mut out);
        assert_eq!(out.get("a.b").map(String::as_str), Some("msg"));
        assert_eq!(out.get("top").map(String::as_str), Some("level"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn parse_locale_bad_json_is_empty() {
        assert!(parse_locale("not json").is_empty());
    }

    #[test]
    fn build_table_overlays_requested_locale() {
        let embedded: &[(&str, &str)] = &[
            ("en", r#"{"a": "english", "b": "base"}"#),
            ("fr", r#"{"a": "français", "b": ""}"#),
        ];
        let table = build_table(Some("fr-FR"), embedded);
        assert_eq!(table.get("a").map(String::as_str), Some("français"));
        // Empty (untranslated) values fall back to English.
        assert_eq!(table.get("b").map(String::as_str), Some("base"));
    }

    #[test]
    fn build_table_ignores_stale_locale_keys() {
        let embedded: &[(&str, &str)] = &[
            ("en", r#"{"a": "english"}"#),
            ("fr", r#"{"a": "français", "gone": "stale"}"#),
        ];
        let table = build_table(Some("fr"), embedded);
        assert_eq!(table.get("a").map(String::as_str), Some("français"));
        assert!(!table.contains_key("gone"));
    }

    #[test]
    fn build_table_unknown_locale_is_english() {
        let embedded: &[(&str, &str)] = &[("en", r#"{"a": "english"}"#)];
        let table = build_table(Some("ja"), embedded);
        assert_eq!(table.get("a").map(String::as_str), Some("english"));
        let table = build_table(None, embedded);
        assert_eq!(table.get("a").map(String::as_str), Some("english"));
    }

    #[test]
    fn build_table_missing_base_is_empty() {
        // Unreachable in practice (build.rs asserts en.json exists), but the
        // defensive default must not panic.
        let table = build_table(None, &[]);
        assert!(table.is_empty());
    }

    #[test]
    fn en_json_placeholders_are_well_formed() {
        // Every `{...}` token in en.json must be a simple snake_case
        // identifier — anything else is a typo that interpolation would
        // silently leave in the UI.
        let en = EMBEDDED_TRANSLATIONS
            .iter()
            .find(|(stem, _)| *stem == BASE_LOCALE)
            .map(|(_, raw)| *raw)
            .expect("en.json embedded");
        for (key, message) in parse_locale(en) {
            let mut rest = message.as_str();
            while let Some(start) = rest.find('{') {
                let after = &rest[start + 1..];
                let end = after
                    .find('}')
                    .unwrap_or_else(|| panic!("unclosed '{{' in {key}"));
                let name = &after[..end];
                assert!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "bad placeholder {{{name}}} in {key}"
                );
                rest = &after[end + 1..];
            }
            assert!(!rest.contains('}'), "stray '}}' in {key}");
        }
    }

    /// True for a dot-separated snake_case key path whose segments are all
    /// non-empty and start with a letter (`tray.open_dashboard`), which is
    /// what distinguishes translation keys from other string literals (`...`,
    /// file names, etc.).
    fn is_key_shaped(key: &str) -> bool {
        key.contains('.')
            && key.split('.').all(|segment| {
                segment
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase())
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
    }

    /// Extract translation keys from `t(` / `t_with(` call sites in a source
    /// string. The char before the `t` must not be part of an identifier so
    /// e.g. `format!(` or `split(` don't match, and whitespace between the
    /// paren and the key literal is skipped so rustfmt's line breaks don't
    /// hide a call site.
    fn extract_keys(source: &str, out: &mut std::collections::BTreeSet<String>) {
        for needle in ["t(", "t_with("] {
            let mut from = 0;
            while let Some(pos) = source[from..].find(needle) {
                let at = from + pos;
                from = at + needle.len();
                if at > 0 {
                    let prev = source.as_bytes()[at - 1];
                    if prev.is_ascii_alphanumeric() || prev == b'_' {
                        continue;
                    }
                }
                let rest = source[at + needle.len()..].trim_start();
                let Some(literal) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(key_end) = literal.find('"') else {
                    continue;
                };
                let key = &literal[..key_end];
                if is_key_shaped(key) {
                    out.insert(key.to_string());
                }
            }
        }
    }

    /// Collect every translation key referenced from the crate's sources.
    ///
    /// Skips this module's own file: it is the only source whose `t(...)`
    /// literals live in test code (the tests right here), and counting those
    /// as usage would let a key stay "used" after the last production
    /// reference is removed, defeating the drift checks below.
    fn used_keys() -> std::collections::BTreeSet<String> {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let own_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("i18n")
            .join("mod.rs");
        let mut keys = std::collections::BTreeSet::new();
        let mut stack = vec![src_dir];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("read dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && path != own_file {
                    let source = std::fs::read_to_string(&path).expect("read source file");
                    extract_keys(&source, &mut keys);
                }
            }
        }
        keys
    }

    #[test]
    fn all_used_keys_exist_in_en_json() {
        let en = EMBEDDED_TRANSLATIONS
            .iter()
            .find(|(stem, _)| *stem == BASE_LOCALE)
            .map(|(_, raw)| *raw)
            .expect("en.json embedded");
        let table = parse_locale(en);
        let missing: Vec<String> = used_keys()
            .into_iter()
            .filter(|key| !table.contains_key(key))
            .collect();
        assert!(
            missing.is_empty(),
            "keys used in code but missing from en.json: {missing:?}"
        );
    }

    #[test]
    fn all_en_json_keys_are_used() {
        let en = EMBEDDED_TRANSLATIONS
            .iter()
            .find(|(stem, _)| *stem == BASE_LOCALE)
            .map(|(_, raw)| *raw)
            .expect("en.json embedded");
        let table = parse_locale(en);
        let used = used_keys();
        let unused: Vec<&String> = table.keys().filter(|key| !used.contains(*key)).collect();
        assert!(
            unused.is_empty(),
            "keys in en.json but never used in code: {unused:?}"
        );
    }

    #[test]
    fn extract_keys_skips_identifier_prefixes_and_non_keys() {
        let mut out = std::collections::BTreeSet::new();
        extract_keys(
            r#"
            let a = t("tray.quit");
            let b = crate::i18n::t_with("tray.port", &[("port", "1")]);
            let c = format!("not.a.key");
            let d = split("also.not");
            let e = t("no_dot");
            let f = t("Bad.Case");
            "#,
            &mut out,
        );
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["tray.port".to_string(), "tray.quit".to_string()]
        );
    }
}
