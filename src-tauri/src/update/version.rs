//! ESPHome / device-builder version parsing, comparison, and PyPI release
//! selection.
//!
//! Pure logic split out of the update module: no Tauri or platform
//! dependencies, so it is unit-testable without a live interpreter or PyPI.

use std::borrow::Cow;
use std::collections::HashMap;

use super::PyPIRelease;

/// Choose the version to offer on the beta channel.
///
/// Returns the latest beta only when it is strictly newer than the latest
/// stable; otherwise returns `stable`. This prevents a downgrade: after a
/// release cycle closes, the newest beta on PyPI (e.g. "2025.4.0b3") is older
/// than the stable it led to ("2025.4.0"), and `switch_channel(Beta)` installs
/// the returned version unconditionally — without it, a stable user switching
/// to beta would be moved *backwards* onto a stale pre-release.
pub(super) fn select_beta_target(
    releases: &HashMap<String, Vec<PyPIRelease>>,
    stable: &str,
) -> String {
    match find_latest_beta(releases) {
        Some(beta) if is_newer_version(&beta, stable) => beta,
        _ => stable.to_string(),
    }
}

/// Find the highest version among PyPI releases whose version string matches
/// `predicate`.
///
/// Skips version strings that don't start with a digit (not a valid-looking
/// version) and versions with no installable files (fully yanked or files
/// removed): PyPI keeps the version key with an empty/all-yanked file list,
/// and offering it would download nothing or install a pulled release.
fn highest_version(
    releases: &HashMap<String, Vec<PyPIRelease>>,
    predicate: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut best: Option<String> = None;

    for (version_str, files) in releases {
        if !predicate(version_str) {
            continue;
        }

        // Skip if not a valid-looking version
        if !version_str
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }

        // Skip versions with no installable files (fully yanked or removed).
        if !has_active_files(files) {
            continue;
        }

        match &best {
            None => best = Some(version_str.clone()),
            Some(current_best) => {
                if is_newer_version(version_str, current_best) {
                    best = Some(version_str.clone());
                }
            }
        }
    }

    best
}

/// Find the latest beta/pre-release version from PyPI releases.
///
/// Beta versions on PyPI look like "2025.4.0b1", "2025.4.0b2", etc.
/// We find the highest version that contains a beta suffix; ESPHome beta
/// releases always use bN naming.
fn find_latest_beta(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    highest_version(releases, has_beta_suffix)
}

/// Check whether a version string has a beta suffix like "b1", "b2", etc.
/// Matches patterns where a 'b' immediately follows a digit and is followed by
/// one or more digits (e.g. "2025.4.0b1"), which distinguishes it from versions
/// that merely contain the letter 'b' elsewhere.
fn has_beta_suffix(version: &str) -> bool {
    let bytes = version.as_bytes();
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b'b' && bytes[i - 1].is_ascii_digit() && bytes[i + 1].is_ascii_digit() {
            return true;
        }
    }
    false
}

/// Whether a release has at least one installable (non-yanked) file.
///
/// A version present in PyPI's `releases` map is not necessarily installable:
/// once every file is yanked or removed, the key lingers with an empty or
/// all-yanked file list. Such a version must not be offered as an update
/// target.
fn has_active_files(files: &[PyPIRelease]) -> bool {
    files.iter().any(|f| !f.yanked)
}

/// Find the highest version across all releases on PyPI, including
/// pre-releases. Used for the "beta" device-builder channel where any
/// pre-release counts (a/b/rc/dev), not just `bN` like ESPHome itself.
pub(super) fn find_latest_any(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    highest_version(releases, |_| true)
}

/// Pre-release precedence for a version's tag, following PEP 440 ordering:
/// `dev < alpha < beta < rc < release`.
///
/// ESPHome itself only ships `bN` betas (e.g. "2025.4.0b1") and `-dev`
/// builds (e.g. "2026.5.0-dev"), but `esphome-device-builder` is compared
/// with [`find_latest_any`], which can surface any pre-release kind. Ranking
/// them all explicitly avoids mis-selecting an alpha over a beta (both used to
/// share rank 1) or treating a dev build as equal to a beta (both used to be
/// rank 0).
///
/// A bare stable segment never reaches this function — [`parse_version`]
/// assigns it the `255` sentinel directly, so every pre-release tier here
/// sorts below any stable release.
fn prerelease_ord(tag: &str) -> u8 {
    match tag {
        "dev" => 0,
        "a" | "alpha" => 1,
        "b" | "beta" => 2,
        "rc" | "c" | "pre" | "preview" => 3,
        // An unrecognized suffix is treated as the most-final pre-release
        // tier: above every known pre-release but still below a bare stable
        // release. This is conservative — an unexpected tag won't be ranked
        // newer than the stable it precedes.
        _ => 4,
    }
}

/// Re-attach a PEP 440 `.devN` developmental segment to the numeric release
/// segment that precedes it.
///
/// PyPI's JSON API and `importlib.metadata.version()` report developmental
/// releases in normalized PEP 440 form with a **dot** separator
/// (`"2025.5.0.dev3"`), not the hyphenated form (`"2025.5.0-dev"`) the segment
/// parser handles. Without this, `parse_version` splits `"dev3"` off as its own
/// dot-segment, finds no leading digit, and drops it entirely — so the dev
/// build parses identically to the stable `"2025.5.0"`. That silently breaks
/// the device-builder beta channel: a user on one `.devN` build is never
/// notified of a newer `.devN` build of the same base (they compare equal), and
/// `find_latest_any` ranks a dev equal-to-stable / above a beta of the same
/// base, inverting the PEP 440 ordering that [`prerelease_ord`] is meant to
/// enforce.
///
/// Converting `".dev"` → `"-dev"` routes the dev tag through the hyphenated path
/// the tier logic already ranks correctly. Only `.dev` is normalized: among PEP
/// 440 pre-release kinds it is the only one that uses a dot separator (`aN`,
/// `bN`, `rcN` attach directly), so this fully closes the dot-separator gap.
///
/// Returns a borrowed `Cow` when the input has no `.dev` segment (the common
/// case while scanning PyPI releases), allocating only when a substitution is
/// needed. PEP 440 permits at most one `.devN` segment, so only the first
/// occurrence is replaced.
fn normalize_dev_separator(s: &str) -> Cow<'_, str> {
    if s.contains(".dev") {
        Cow::Owned(s.replacen(".dev", "-dev", 1))
    } else {
        Cow::Borrowed(s)
    }
}

/// Parse a version string like "2024.1.0b1", "2026.5.0-dev", or the PEP 440
/// normalized "2026.5.0.dev1" into a comparable representation.
/// Each dot-separated segment becomes (numeric_part, prerelease_order, prerelease_num).
/// A stable segment like "0" becomes (0, 255, 0) so it sorts higher than any pre-release.
fn parse_version(s: &str) -> Vec<(u32, u8, u32)> {
    normalize_dev_separator(s)
        .split('.')
        .filter_map(|part| {
            // Split on pre-release tag boundaries: "0b1", "0-dev"
            // Take the leading digits first
            let num_end = part
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(part.len());
            let numeric: u32 = part[..num_end].parse().ok()?;

            if num_end < part.len() {
                // There's a pre-release suffix
                let suffix = &part[num_end..];
                // Strip a leading hyphen (e.g. "-dev" -> "dev")
                let suffix = suffix.strip_prefix('-').unwrap_or(suffix);
                // Find where the tag name ends and the pre-release number begins
                let tag_end = suffix
                    .find(|c: char| c.is_ascii_digit())
                    .unwrap_or(suffix.len());
                let tag = &suffix[..tag_end];
                let pre_num: u32 = if tag_end < suffix.len() {
                    suffix[tag_end..].parse().unwrap_or(0)
                } else {
                    0
                };
                Some((numeric, prerelease_ord(tag), pre_num))
            } else {
                // Stable segment — sorts higher than any pre-release
                Some((numeric, 255, 0))
            }
        })
        .collect()
}

/// Compare two version strings and return true if `latest` is newer than `installed`
pub(crate) fn is_newer_version(latest: &str, installed: &str) -> bool {
    let latest_parts = parse_version(latest);
    let installed_parts = parse_version(installed);

    // An installed version we cannot parse (e.g. "None", "") must not be treated
    // as infinitely old, or every check would offer an update forever (#190).
    if installed_parts.is_empty() {
        return false;
    }
    // Symmetric: an unparseable "latest" is never newer than a real installed one.
    if latest_parts.is_empty() {
        return false;
    }

    latest_parts > installed_parts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One non-yanked file — a normally installable release.
    fn active() -> Vec<PyPIRelease> {
        vec![PyPIRelease { yanked: false }]
    }

    /// All files yanked — present on PyPI but not installable.
    fn yanked() -> Vec<PyPIRelease> {
        vec![PyPIRelease { yanked: true }]
    }

    #[test]
    fn test_version_comparison() {
        assert!(is_newer_version("2024.2.0", "2024.1.0"));
        assert!(is_newer_version("2024.1.1", "2024.1.0"));
        assert!(is_newer_version("2025.1.0", "2024.12.0"));
        assert!(!is_newer_version("2024.1.0", "2024.1.0"));
        assert!(!is_newer_version("2024.1.0", "2024.2.0"));
        // Stable is newer than beta with same base version
        assert!(is_newer_version("2024.1.0", "2024.1.0b1"));
        // Higher beta number is newer
        assert!(is_newer_version("2024.1.0b2", "2024.1.0b1"));
        // Beta is not newer than stable
        assert!(!is_newer_version("2024.1.0b1", "2024.1.0"));
        // Dev versions use hyphenated suffix: "2026.5.0-dev"
        // Stable is newer than dev with same base version
        assert!(is_newer_version("2026.5.0", "2026.5.0-dev"));
        // Dev is not newer than stable with same base version
        assert!(!is_newer_version("2026.5.0-dev", "2026.5.0"));
        // A newer base version dev is still newer than an older stable
        assert!(is_newer_version("2026.5.0-dev", "2026.4.0"));
    }

    #[test]
    fn test_unparseable_installed_version_is_not_offered_an_update() {
        // Regression for #190: duplicate dist-info dirs make the version lookup
        // return "None"/"", which must never be treated as infinitely old.
        assert!(!is_newer_version("1.0.10", "None"));
        assert!(!is_newer_version("1.0.10", ""));
        assert!(!is_newer_version("2025.5.0", "None"));
        // An unparseable "latest" is never newer than a real installed version.
        assert!(!is_newer_version("None", "1.0.10"));
        // Sanity: real comparisons still work.
        assert!(is_newer_version("1.0.10", "1.0.9"));
        assert!(!is_newer_version("1.0.10", "1.0.10"));
    }

    #[test]
    fn test_prerelease_precedence_ordering() {
        // PEP 440 ordering within the same base version:
        //   dev < alpha < beta < rc < release
        assert!(is_newer_version("2025.4.0a1", "2025.4.0-dev"));
        assert!(is_newer_version("2025.4.0b1", "2025.4.0a1"));
        assert!(is_newer_version("2025.4.0rc1", "2025.4.0b1"));
        assert!(is_newer_version("2025.4.0", "2025.4.0rc1"));

        // Transitivity check across the full chain.
        assert!(is_newer_version("2025.4.0rc1", "2025.4.0-dev"));
        assert!(is_newer_version("2025.4.0b1", "2025.4.0-dev"));

        // Long-form tags rank identically to their short forms.
        assert!(is_newer_version("2025.4.0beta1", "2025.4.0alpha1"));
        assert!(!is_newer_version("2025.4.0alpha2", "2025.4.0beta1"));

        // A dev build is no longer considered equal to a beta of the same
        // base (they previously both mapped to rank 0).
        assert!(is_newer_version("2025.4.0b1", "2025.4.0-dev"));
        assert!(!is_newer_version("2025.4.0-dev", "2025.4.0b1"));

        // "c" is an accepted alias for "rc".
        assert!(is_newer_version("2025.4.0c1", "2025.4.0b9"));
    }

    #[test]
    fn test_pep440_dot_dev_separator() {
        // PyPI / importlib.metadata report dev releases with a dot separator.
        // These must rank identically to the hyphenated form.

        // Stable is newer than a dot-form dev of the same base.
        assert!(is_newer_version("2025.5.0", "2025.5.0.dev3"));
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0"));

        // A newer dev build of the same base is detected (the bug: both used to
        // collapse to the stable representation and compare equal).
        assert!(is_newer_version("2025.5.0.dev5", "2025.5.0.dev3"));
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0.dev5"));

        // Dot-form dev sorts below a beta/rc of the same base (PEP 440 order).
        assert!(is_newer_version("2025.5.0b1", "2025.5.0.dev9"));
        assert!(is_newer_version("2025.5.0rc1", "2025.5.0.dev9"));

        // Dot and hyphen forms of the same dev build are equivalent.
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0-dev3"));
        assert!(!is_newer_version("2025.5.0-dev3", "2025.5.0.dev3"));

        // A newer base version dev is still newer than an older stable.
        assert!(is_newer_version("2025.6.0.dev1", "2025.5.0"));
    }

    #[test]
    fn test_has_beta_suffix() {
        assert!(has_beta_suffix("2025.4.0b1"));
        assert!(has_beta_suffix("2025.4.0b12"));
        assert!(!has_beta_suffix("2025.4.0"));
        assert!(!has_beta_suffix("2025.4.0-dev"));
        // Should not match 'b' that isn't a digit-b-digit pattern
        assert!(!has_beta_suffix("abc"));
    }

    #[test]
    fn test_find_latest_beta() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), active());
        releases.insert("2025.3.0b1".to_string(), active());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b2".to_string()));
    }

    #[test]
    fn test_find_latest_beta_none() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, None);
    }

    #[test]
    fn test_find_latest_beta_skips_yanked() {
        // The newest beta on PyPI was yanked — fall back to the next
        // installable beta rather than offering the pulled release.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), yanked());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b1".to_string()));
    }

    #[test]
    fn test_find_latest_beta_skips_empty_file_list() {
        // A version key with no files (all removed from PyPI) is not
        // installable and must be ignored.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), vec![]);

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b1".to_string()));
    }

    #[test]
    fn test_find_latest_any_skips_yanked() {
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), yanked());

        // The only newer candidate is yanked, so the highest installable
        // version wins.
        assert_eq!(find_latest_any(&releases), Some("2025.4.0".to_string()));
    }

    #[test]
    fn test_select_beta_target_prefers_newer_beta() {
        // A beta for the next release exists and is newer than stable.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.5.0b1".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_avoids_downgrade_to_old_beta() {
        // The release cycle finished: the newest beta on PyPI is the
        // pre-release that led to the current stable. Offering it would
        // downgrade a beta-channel user — fall back to stable instead.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_falls_back_when_newest_beta_yanked() {
        // The next-cycle beta exists but was yanked: don't offer it, fall
        // back to the current stable instead of an uninstallable release.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), yanked());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_no_beta_uses_stable() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }
}
