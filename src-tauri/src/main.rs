// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{CommandFactory, Parser};
use esphome_desktop_lib::Cli;

fn main() -> std::process::ExitCode {
    // Decide whether we were started from a terminal before clap parses.
    // On Windows this must go through AttachConsole (release builds start
    // with no console), which both makes --help/usage output visible and,
    // via its success, tells us the launcher had a console. On Unix we check
    // the std handles directly, including stderr so a `cmd >out 2>&1`-style
    // launch with stdin/stdout redirected but stderr on the tty still counts.
    #[cfg(windows)]
    let from_terminal = esphome_desktop_lib::attach_parent_console();
    #[cfg(not(windows))]
    let from_terminal = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
            || std::io::stdout().is_terminal()
            || std::io::stderr().is_terminal()
    };

    // Count args before clap consumes them; a bare invocation is just the
    // program name (count 1).
    let arg_count = std::env::args_os().count();

    let cli = Cli::parse();
    // A subcommand means "control the running app": run the short-lived CLI
    // client and exit without ever starting Tauri.
    if let Some(command) = cli.command.clone() {
        return esphome_desktop_lib::run_cli(command);
    }
    // A bare `esphome-desktop` typed in a terminal is someone exploring the
    // CLI, not a request to launch another app instance (a second launch is
    // heavyweight: it runs a full startup before single-instance forwards
    // it). Any explicit flag, or a launch from outside a terminal (Finder,
    // the applications menu, autostart), falls through to a normal launch.
    if esphome_desktop_lib::is_bare_terminal_launch(from_terminal, arg_count) {
        // Best-effort: a write failure here is almost always a broken pipe
        // (`esphome-desktop | head`), which is not worth reporting; clap's
        // help already ends with a newline.
        let _ = Cli::command().print_help();
        return std::process::ExitCode::SUCCESS;
    }
    esphome_desktop_lib::run(cli);
    std::process::ExitCode::SUCCESS
}
