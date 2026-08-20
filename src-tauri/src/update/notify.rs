//! Shared "update available" prompt/notify tails and their wording.
//!
//! Split out of the update module so the user-prompt and background-notify
//! flows keep a single source of wording and cannot drift apart.

use tauri::AppHandle;
use tauri_plugin_dialog::MessageDialogKind;
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, error, info};

use super::{is_newer_version, UpdateWording};
use crate::i18n::{t, t_with};

/// `latest` is newer, log it and ask the user whether to update now. Returns
/// `Some(latest)` only when an update is available and the user confirms.
/// When already up to date, logs that at info level and, if
/// `dialog_when_up_to_date` is set, also shows the "No Updates Available"
/// notice (the device-builder flow stays silent; its caller owns that UX).
pub(super) async fn prompt_if_newer(
    app_handle: &AppHandle,
    wording: &UpdateWording<'_>,
    title: &str,
    latest: String,
    installed: &str,
    dialog_when_up_to_date: bool,
) -> Option<String> {
    if !is_newer_version(&latest, installed) {
        info!("{} is up to date ({})", wording.component, installed);
        if dialog_when_up_to_date {
            crate::dialog::notice(
                app_handle,
                &t("update.none_title"),
                t_with(
                    "update.latest",
                    &[("component", wording.component), ("installed", installed)],
                ),
                MessageDialogKind::Info,
            )
            .await;
        }
        return None;
    }

    info!(
        "{} available: {} -> {} (installed: {})",
        wording.log_prefix, installed, latest, installed
    );

    let msg = wording.prompt_message(&latest, installed);
    if crate::dialog::confirm(
        app_handle,
        title,
        msg,
        &t("common.update_now"),
        &t("common.later"),
    )
    .await
    {
        Some(latest)
    } else {
        None
    }
}

/// Shared tail of the background update checks: compare versions and, when
/// `latest` is newer, log it and show the "<component> Update Available"
/// notification pointing at the updates menu. Logs the up-to-date state at
/// debug level otherwise.
pub(super) fn notify_if_newer(
    app_handle: &AppHandle,
    wording: &UpdateWording<'_>,
    latest: &str,
    installed: &str,
    tray_available: bool,
) {
    if !is_newer_version(latest, installed) {
        debug!("{} is up to date ({})", wording.component, installed);
        return;
    }

    info!(
        "{} available: {} -> {} (installed: {})",
        wording.log_prefix, installed, latest, installed
    );

    if let Err(e) = notify_update_available(
        app_handle,
        &wording.notification_title(),
        &wording.subject(latest),
        installed,
        NotificationHint::InApp { tray_available },
    ) {
        error!("Failed to show notification: {}", e);
    }
}

/// Which follow-through the "update available" notification's trailing hint
/// points at. The hint exists so the notification never instructs a route
/// that cannot perform the update: no tray means "open the tray menu" is
/// pointing at nothing (issue #87), and an externally managed desktop update
/// means the in-app flow — tray or CLI — cannot install it either.
#[derive(Clone, Copy)]
pub(crate) enum NotificationHint {
    /// The in-app flow: the tray's Check for Updates, or the CLI when no
    /// tray is available.
    InApp { tray_available: bool },
    /// Only the system package manager can install this update — a deb/rpm
    /// repackage without its package tool (`app_update`'s externally-managed
    /// case).
    PackageManager,
}

impl NotificationHint {
    fn text(self) -> String {
        match self {
            NotificationHint::InApp { tray_available } => crate::updates_menu_hint(tray_available),
            NotificationHint::PackageManager => crate::i18n::t("hint.updates_package_manager"),
        }
    }
}

/// Build and show the standard "update available" notification:
/// "<subject> is available (you have <installed>). <hint>".
/// Returns the show error so each caller keeps its own failure log wording.
pub(crate) fn notify_update_available(
    app_handle: &AppHandle,
    title: &str,
    subject: &str,
    installed: &str,
    hint: NotificationHint,
) -> tauri_plugin_notification::Result<()> {
    app_handle
        .notification()
        .builder()
        .title(title)
        .body(update_notification_body(subject, installed, hint))
        .show()
}

/// Body of the standard "update available" notification, shared by every
/// caller of [`notify_update_available`].
fn update_notification_body(subject: &str, installed: &str, hint: NotificationHint) -> String {
    t_with(
        "update.notification_body",
        &[
            ("subject", subject),
            ("installed", installed),
            ("hint", &hint.text()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::super::{PypiChannel, DEVICE_BUILDER_WORDING};
    use super::*;

    /// The ESPHome wording exactly as the check tails build it. Going through
    /// the real constructor is the point: a hand-built copy would keep these
    /// assertions passing while production drifted away from them.
    fn esphome_wording() -> UpdateWording<'static> {
        UpdateWording::esphome(PypiChannel::Stable)
    }

    #[test]
    fn subject_appends_channel_label_when_present() {
        assert_eq!(
            esphome_wording().subject("2025.1.0"),
            "ESPHome 2025.1.0 (stable)"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.subject("1.2.3"),
            "ESPHome Device Builder 1.2.3"
        );
    }

    #[test]
    fn prompt_message_pins_exact_dialog_text() {
        assert_eq!(
            esphome_wording().prompt_message("2025.1.0", "2024.12.2"),
            "ESPHome 2025.1.0 (stable) is available.\n\n\
             You currently have version 2024.12.2.\n\n\
             Would you like to update now?"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.prompt_message("1.2.3", "1.2.2"),
            "ESPHome Device Builder 1.2.3 is available.\n\n\
             You currently have version 1.2.2.\n\n\
             Would you like to update now?"
        );
    }

    #[test]
    fn notification_title_pins_exact_text() {
        assert_eq!(
            esphome_wording().notification_title(),
            "ESPHome Update Available"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.notification_title(),
            "ESPHome Device Builder Update Available"
        );
    }

    #[test]
    fn notification_body_pins_exact_text_for_both_tray_states() {
        let with_tray = NotificationHint::InApp {
            tray_available: true,
        };
        let no_tray = NotificationHint::InApp {
            tray_available: false,
        };
        // With a tray, the hint points at the tray menu.
        assert_eq!(
            update_notification_body(
                &esphome_wording().subject("2025.1.0"),
                "2024.12.2",
                with_tray
            ),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             Open the tray menu and choose \"Check for Updates\" to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", with_tray),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             Open the tray menu and choose \"Check for Updates\" to update."
        );
        // Without a tray, the hint falls back to the CLI.
        assert_eq!(
            update_notification_body(&esphome_wording().subject("2025.1.0"), "2024.12.2", no_tray),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", no_tray),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
    }

    /// The externally-managed hint must never point at the in-app flow: for a
    /// deb/rpm repackage without its package tool, neither the tray's Check
    /// for Updates nor the CLI can install the desktop update, so the daily
    /// notification instructs the package manager instead.
    #[test]
    fn notification_body_pins_package_manager_hint() {
        assert_eq!(
            update_notification_body("Version 1.2.0", "1.1.2", NotificationHint::PackageManager),
            "Version 1.2.0 is available (you have 1.1.2). \
             Update it through your system package manager."
        );
    }
}
