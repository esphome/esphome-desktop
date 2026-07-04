// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{CommandFactory, Parser};
use esphome_desktop_lib::Cli;

fn main() -> std::process::ExitCode {
    // Attach to the invoking console before clap parses: release builds use
    // `windows_subsystem = "windows"`, so without this, --help/--version and
    // usage errors for mistyped subcommands print nothing.
    #[cfg(windows)]
    esphome_desktop_lib::attach_parent_console();

    let cli = Cli::parse();
    // A subcommand means "control the running app": run the short-lived CLI
    // client and exit without ever starting Tauri.
    if let Some(command) = cli.command.clone() {
        return esphome_desktop_lib::run_cli(command);
    }
    // A bare `esphome-desktop` typed in a terminal is someone exploring the
    // CLI, not a request to launch another app instance (a second launch is
    // heavyweight: it runs a full startup before single-instance forwards
    // it). Desktop launches — Finder, the applications menu, autostart —
    // have no controlling terminal and fall through to a normal launch, as
    // does any explicit launch flag.
    if esphome_desktop_lib::bare_terminal_invocation(&cli) {
        let _ = Cli::command().print_help();
        return std::process::ExitCode::SUCCESS;
    }
    esphome_desktop_lib::run(cli);
    std::process::ExitCode::SUCCESS
}
