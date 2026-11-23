mod cli;
mod commands;
mod error;
mod explorer;
mod hookdll;
mod host;
mod output;
mod reg;
mod target;
#[cfg(test)]
mod testutil;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_logging();
    match commands::dispatch(cli.command) {
        Ok(code) => code,
        Err(err) => {
            output::print_error_line(format!("error: {err}"));
            ExitCode::FAILURE
        }
    }
}

/// Diagnostics go to stderr and stay quiet unless `RWM_LOG` asks for more, for example
/// `RWM_LOG=debug` or `RWM_LOG=rwm_core=trace`.
fn init_logging() {
    let filter = EnvFilter::try_from_env("RWM_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
