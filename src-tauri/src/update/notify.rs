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
        tray_available,
    ) {
        error!("Failed to show notification: {}", e);
    }
}

/// Build and show the standard "update available" notification:
/// "<subject> is available (you have <installed>). <updates menu hint>".
/// Returns the show error so each caller keeps its own failure log wording.
pub(crate) fn notify_update_available(
    app_handle: &AppHandle,
    title: &str,
    subject: &str,
    installed: &str,
    tray_available: bool,
) -> tauri_plugin_notification::Result<()> {
    app_handle
        .notification()
        .builder()
        .title(title)
        .body(update_notification_body(subject, installed, tray_available))
        .show()
}

/// Body of the standard "update available" notification, shared by every
/// caller of [`notify_update_available`].
fn update_notification_body(subject: &str, installed: &str, tray_available: bool) -> String {
    t_with(
        "update.notification_body",
        &[
            ("subject", subject),
            ("installed", installed),
            ("hint", &crate::updates_menu_hint(tray_available)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::super::DEVICE_BUILDER_WORDING;
    use super::*;

    /// The ESPHome wording as built by the check tails (channel label present).
    fn esphome_wording() -> UpdateWording<'static> {
        UpdateWording {
            component: "ESPHome",
            log_prefix: "Update",
            channel_label: Some("stable"),
        }
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
        // With a tray, the hint points at the tray menu.
        assert_eq!(
            update_notification_body(&esphome_wording().subject("2025.1.0"), "2024.12.2", true),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             Open the tray menu and choose \"Check for Updates...\" to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", true),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             Open the tray menu and choose \"Check for Updates...\" to update."
        );
        // Without a tray, the hint falls back to the CLI.
        assert_eq!(
            update_notification_body(&esphome_wording().subject("2025.1.0"), "2024.12.2", false),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", false),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
    }
}
