//! Browser hand-off for the ESPHome dashboard.
//!
//! Shared by every surface that can put the dashboard in front of the user:
//! the tray's Open Dashboard item, the control server's `open` subcommand,
//! and the post-install restart in `control::ops`.

use tracing::{error, info, warn};

/// Open the ESPHome dashboard in the default browser. Detached: `open::that`
/// waits for the opener process to exit, which can block the calling thread
/// (including a tokio worker when invoked from the control server).
pub(crate) fn open_dashboard(port: u16) {
    let url = format!("http://localhost:{}", port);
    if let Err(e) = open::that_detached(&url) {
        error!("Failed to open browser: {}", e);
    }
}

/// Wait for the dashboard to be ready by polling the health endpoint
pub(crate) async fn wait_for_dashboard_ready(port: u16, timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let url = crate::daemon::loopback_url(port);
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                info!("Backend is ready");
                return true;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    warn!("Timeout waiting for backend to be ready");
    false
}
