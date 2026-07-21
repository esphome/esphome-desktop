//! Windows-specific initialization: a guided first-run flow that opens
//! Windows Defender Firewall for peer-link pairing.
//!
//! The bundled Python backend listens for peer-link connections on all
//! interfaces (TCP 6055, falling forward when the port is busy) and answers
//! mDNS queries on UDP 5353. Windows Defender Firewall blocks both inbound
//! paths by default, so pairing another dashboard to this machine fails
//! until an allow rule exists (#384). The installer runs per-user without
//! elevation and cannot add the rule itself, so on startup the app offers to
//! add a program-scoped inbound allow rule for the managed Python
//! interpreter, behind one UAC prompt.

use std::path::Path;

use anyhow::{Context, Result};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use super::process::configure_no_window_command;

/// Name of the inbound allow rule. The uninstaller deletes rules by this
/// exact name (`installer-hooks.nsi`); a drift test below keeps the two
/// spellings in sync.
const FIREWALL_RULE_NAME: &str = "ESPHome Device Builder";

/// Marker file recording that the flow already settled, so later launches
/// stop at one file stat. It lives in the machine-local data dir
/// (`get_python_parent_dir`), not the roaming one: the rule is per machine,
/// and a marker that roamed to another machine would suppress the prompt
/// where the rule does not exist.
const MARKER_NAME: &str = ".windows_firewall_prompt";

pub fn init(app_handle: &AppHandle) {
    ensure_firewall_rule(app_handle);
}

/// Offer to add the firewall rule if it is missing and the user has not been
/// asked before. Never blocks startup: everything past the marker stat, the
/// `netsh` probe included, runs off the setup thread, and every failure is
/// logged and dropped.
fn ensure_firewall_rule(app_handle: &AppHandle) {
    let local_dir = match super::get_python_parent_dir(app_handle) {
        Ok(d) => d,
        Err(e) => {
            debug!(
                "Skipping firewall prompt; local data dir unavailable: {}",
                e
            );
            return;
        }
    };
    let marker = local_dir.join(MARKER_NAME);
    if marker.exists() {
        return;
    }

    // The managed copy of the interpreter is what the daemon runs, so that is
    // the path the rule must be scoped to. It may not exist yet this early on
    // the very first launch; netsh records the path without checking it.
    let python_exe = match super::managed_interpreter_path(app_handle) {
        Ok(p) => p,
        Err(e) => {
            debug!("Skipping firewall prompt; python dir unavailable: {}", e);
            return;
        }
    };

    let app = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        // netsh spawns a subprocess; keep it off the async executor.
        let exists = tokio::task::spawn_blocking(firewall_rule_exists)
            .await
            .unwrap_or(false);
        if exists {
            debug!("Firewall rule {:?} already present", FIREWALL_RULE_NAME);
        } else {
            info!(
                "Firewall rule {:?} missing; prompting user to add it",
                FIREWALL_RULE_NAME
            );
            let confirmed = crate::dialog::confirm(
                &app,
                &crate::i18n::t("platform.firewall_title"),
                crate::i18n::t("platform.firewall_prompt"),
                &crate::i18n::t("platform.firewall_allow"),
                &crate::i18n::t("platform.firewall_decline"),
            )
            .await;

            if confirmed {
                let added = tokio::task::spawn_blocking(move || add_firewall_rule(&python_exe))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r);
                match added {
                    Ok(()) => info!("Added firewall rule {:?}", FIREWALL_RULE_NAME),
                    // The user wanted the rule but did not get it — a
                    // declined or mis-clicked UAC prompt, a transient netsh
                    // failure. Skip the marker so the next launch retries;
                    // only a settled outcome (rule present, or an explicit
                    // decline of the dialog) is final.
                    Err(e) => {
                        warn!("Failed to add firewall rule: {}", e);
                        return;
                    }
                }
            }
        }

        // Rule present or dialog declined: settled, so the user is not
        // nagged and later launches stop at the marker check above. A dialog
        // that failed to show also lands here as a decline; accepted, the
        // two cannot be told apart through `confirm`.
        if let Err(e) = std::fs::write(&marker, "") {
            warn!("Failed to write firewall-prompt marker: {}", e);
        }
    });
}

/// Absolute path of a binary under `System32`. The firewall flow launches an
/// *elevated* subprocess, and both `CreateProcessW` and `ShellExecuteEx`
/// include the current directory in their by-name search order — a planted
/// `netsh.exe` in a user-writable CWD would run as administrator behind the
/// one UAC prompt the user expects. Resolved via `GetSystemDirectoryW`
/// rather than `%SystemRoot%`, since the environment is user-writable state
/// too; the unelevated query uses it as well for consistency.
fn system32(tail: &str) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;

    let mut buf = [0u16; 260];
    let len =
        unsafe { ::windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buf)) }
            as usize;
    let dir = if len > 0 && len <= buf.len() {
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]))
    } else {
        std::path::PathBuf::from(r"C:\Windows\System32")
    };
    dir.join(tail)
}

/// Whether a rule named [`FIREWALL_RULE_NAME`] exists. Querying the firewall
/// needs no elevation; `netsh` exits non-zero when no rule matches.
fn firewall_rule_exists() -> bool {
    let mut cmd = std::process::Command::new(system32("netsh.exe"));
    cmd.args([
        "advfirewall",
        "firewall",
        "show",
        "rule",
        &format!("name={FIREWALL_RULE_NAME}"),
    ]);
    configure_no_window_command(&mut cmd);
    matches!(cmd.output(), Ok(out) if out.status.success())
}

/// The argument string handed to the elevated `netsh.exe`. One program-scoped
/// rule with no port or protocol restriction covers the peer-link TCP port
/// (6055 with fall-forward) and mDNS on UDP 5353 in one go.
/// `profile=private,domain` deliberately leaves public networks blocked,
/// matching the default of Windows' own allow popup.
fn netsh_add_rule_args(python_exe: &Path) -> String {
    format!(
        "advfirewall firewall add rule name=\"{FIREWALL_RULE_NAME}\" dir=in action=allow \
         program=\"{}\" enable=yes profile=private,domain",
        python_exe.display()
    )
}

/// Add the inbound allow rule via an elevated `netsh`, triggering one UAC
/// prompt. `Start-Process -Verb RunAs` is the way to elevate from an
/// unelevated process; `-Wait` holds until netsh exits. The rule is
/// re-queried afterwards because neither a declined prompt nor a failed
/// netsh reliably shows in `Start-Process`'s own exit status.
fn add_firewall_rule(python_exe: &Path) -> Result<()> {
    // PowerShell single-quoted strings escape ' by doubling it.
    let netsh_args = netsh_add_rule_args(python_exe).replace('\'', "''");
    let netsh = system32("netsh.exe")
        .display()
        .to_string()
        .replace('\'', "''");
    let command =
        format!("Start-Process -FilePath '{netsh}' -ArgumentList '{netsh_args}' -Verb RunAs -Wait");

    let mut cmd = std::process::Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"));
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &command]);
    configure_no_window_command(&mut cmd);
    let output = cmd.output().context("Failed to run powershell")?;

    if !output.status.success() {
        // Start-Process throws when the UAC prompt is declined.
        anyhow::bail!(
            "elevation failed or was declined: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !firewall_rule_exists() {
        anyhow::bail!("netsh did not create the rule");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_rule_args_quote_the_program_path() {
        let args = netsh_add_rule_args(Path::new(
            r"C:\Users\Jane Doe\AppData\Local\io.esphome.builder\python\python.exe",
        ));
        assert!(args.contains("name=\"ESPHome Device Builder\""), "{args}");
        assert!(
            args.contains(
                "program=\"C:\\Users\\Jane Doe\\AppData\\Local\\io.esphome.builder\\python\\python.exe\""
            ),
            "{args}"
        );
        assert!(args.contains("dir=in"), "{args}");
        assert!(args.contains("action=allow"), "{args}");
        assert!(args.contains("profile=private,domain"), "{args}");
    }

    /// The uninstaller deletes rules by name; its spelling lives in
    /// `installer-hooks.nsi` and must match [`FIREWALL_RULE_NAME`].
    #[test]
    fn installer_hook_rule_name_matches() {
        let hooks = include_str!("../../installer-hooks.nsi");
        assert!(
            hooks.contains(&format!(
                "!define FIREWALL_RULE_NAME \"{FIREWALL_RULE_NAME}\""
            )),
            "installer-hooks.nsi must define the same firewall rule name"
        );
    }
}
