//! Update checking functionality
//!
//! Queries PyPI for the latest ESPHome version and notifies the user
//! if an update is available. Supports stable, beta, and dev release channels.
//!
//! Split in two along what a call does rather than which component it targets:
//! [`check`] reads (PyPI queries, prompts, notifications), [`apply`] writes
//! (installs, channel switches, Python-tree repair). This module holds what
//! both halves speak in — the PyPI response shapes, the [`UpdateChecker`]
//! itself, and the per-component wording.

use serde::Deserialize;
use std::collections::HashMap;

use crate::i18n::t_with;

mod apply;
mod check;
mod install;
mod notify;
mod version;

// Test-only: the repair e2e asserts the abort it plants is the one this
// predicate keys the recovery on, rather than re-hardcoding pip's wording.
#[cfg(test)]
pub(crate) use install::is_missing_record_error;
pub use install::{get_installed_device_builder_version, installed_esphome_version};
pub(crate) use notify::notify_update_available;
pub(crate) use version::is_newer_version;

/// PyPI package info response (used for stable channel)
#[derive(Debug, Deserialize)]
struct PyPIResponse {
    info: PyPIInfo,
    releases: HashMap<String, Vec<PyPIRelease>>,
}

#[derive(Debug, Deserialize)]
struct PyPIInfo {
    version: String,
}

/// A single release file entry from PyPI. We only need to know whether the
/// file has been yanked — PyPI keeps a version's key in `releases` even after
/// every file is yanked or removed, so a lingering entry does not mean the
/// version is actually installable.
#[derive(Debug, Deserialize)]
struct PyPIRelease {
    #[serde(default)]
    yanked: bool,
}

/// Update checker
pub struct UpdateChecker {
    client: reqwest::Client,
}

impl UpdateChecker {
    /// Create a new update checker
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }
}

/// Per-component wording shared by the user-prompt and background-notify
/// update-check tails, so the strings cannot drift between the two flows.
struct UpdateWording<'a> {
    /// Component display name, e.g. "ESPHome" or "ESPHome Device Builder".
    component: &'a str,
    /// Leading words of the "<log_prefix> available: a -> b" info log.
    log_prefix: &'a str,
    /// Release-channel label appended to the offered version, when shown.
    channel_label: Option<&'a str>,
}

/// Wording for the `esphome-device-builder` check tails (no channel label;
/// the backend channel is implied by which backend is configured).
const DEVICE_BUILDER_WORDING: UpdateWording<'static> = UpdateWording {
    component: "ESPHome Device Builder",
    log_prefix: "Device-builder update",
    channel_label: None,
};

impl UpdateWording<'_> {
    /// "<component> <version>" with the channel label appended when present,
    /// e.g. "ESPHome 2025.1.0 (stable)" or "ESPHome Device Builder 1.2.3".
    fn subject(&self, version: &str) -> String {
        match self.channel_label {
            Some(label) => format!("{} {} ({})", self.component, version, label),
            None => format!("{} {}", self.component, version),
        }
    }

    /// Full body of the "would you like to update now?" confirm dialog shown
    /// by [`notify::prompt_if_newer`].
    fn prompt_message(&self, latest: &str, installed: &str) -> String {
        t_with(
            "update.available_prompt",
            &[("subject", &self.subject(latest)), ("installed", installed)],
        )
    }

    /// Title of the background "update available" notification shown by
    /// [`notify::notify_if_newer`].
    fn notification_title(&self) -> String {
        t_with(
            "update.notification_title",
            &[("component", self.component)],
        )
    }
}
