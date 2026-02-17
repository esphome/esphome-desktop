// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use esphome_desktop_lib::Cli;

fn main() {
    let cli = Cli::parse();
    esphome_desktop_lib::run(cli)
}
