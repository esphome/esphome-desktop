//! Small helpers around `tauri-plugin-dialog`.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// Show a modal two-button confirmation dialog and wait for the user's choice.
///
/// `blocking_show` is synchronous, so it runs on a blocking thread to keep the
/// async executor free. Returns `true` only when the user picks the confirm
/// button; a cancel, a closed dialog, or a join error all map to `false`.
pub(crate) async fn confirm(
    app_handle: &AppHandle,
    title: &str,
    message: String,
    confirm_label: &str,
    cancel_label: &str,
) -> bool {
    let app = app_handle.clone();
    let title = title.to_string();
    let confirm_label = confirm_label.to_string();
    let cancel_label = cancel_label.to_string();
    tokio::task::spawn_blocking(move || {
        app.dialog()
            .message(message)
            .title(title)
            .buttons(MessageDialogButtons::OkCancelCustom(
                confirm_label,
                cancel_label,
            ))
            .blocking_show()
    })
    .await
    .unwrap_or(false)
}

/// Show a modal single-button notice (informational or error) and wait for the
/// user to dismiss it. Like [`confirm`], `blocking_show` is synchronous so it
/// runs on a blocking thread; the result is discarded since a notice has nothing
/// to report back. A join error just means the dialog never showed, which is
/// fine for a best-effort notice.
pub(crate) async fn notice(
    app_handle: &AppHandle,
    title: &str,
    message: String,
    kind: MessageDialogKind,
) {
    let app = app_handle.clone();
    let title = title.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        app.dialog()
            .message(message)
            .title(title)
            .kind(kind)
            .blocking_show()
    })
    .await;
}
