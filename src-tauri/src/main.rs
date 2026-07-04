// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
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
    esphome_desktop_lib::run(cli);
    std::process::ExitCode::SUCCESS
}
