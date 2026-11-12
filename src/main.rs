//! StarStrings Updater: keeps MrKraken's StarStrings up to date in the Star
//! Citizen folder without keeping anything running in the background.
//!
//! Built for the Windows GUI subsystem so that the hourly scheduled run can
//! never flash a console window. (The test harness stays a console program.)
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod archive;
mod cli;
mod config;
mod detect;
mod http;
mod install;
mod logger;
mod state;
mod task;
mod ui;
mod update;
mod usercfg;
mod win;

fn main() {
    std::process::exit(cli::main());
}
