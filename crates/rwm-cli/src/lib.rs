pub mod cli;
pub mod commands;
pub mod error;
pub mod explorer;
pub mod gui;
pub mod hookdll;
pub mod host;
pub mod menu_cache;
pub mod output;
pub mod probe;
pub mod reg;
pub mod target;

#[cfg(test)]
mod testutil;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

pub fn init_logging() {
    let filter = EnvFilter::try_from_env("RWM_LOG").unwrap_or_else(|_| {
        let level = if rwm_core::config::RwmConfig::load_or_default()
            .is_ok_and(|config| config.debug_logging)
        {
            "debug"
        } else {
            "warn"
        };
        EnvFilter::new(level)
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

pub fn main_entry() -> ExitCode {
    let cli = cli::Cli::parse();
    init_logging();
    match commands::dispatch(cli.command) {
        Ok(code) => code,
        Err(error) => {
            output::print_error_line(format!("error: {error}"));
            ExitCode::FAILURE
        }
    }
}
