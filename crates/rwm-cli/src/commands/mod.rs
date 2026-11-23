mod bench;
mod classic;
mod config;
mod doctor;
mod handlers;
mod install;
mod preview;
mod status;

use std::process::ExitCode;

use rwm_core::model::HandlerPolicy;

use crate::cli::{Command, HandlersAction};
use crate::error::CliResult;

/// The policy names users write in `config.toml`.
fn policy_label(policy: HandlerPolicy) -> &'static str {
    match policy {
        HandlerPolicy::Inline => "inline",
        HandlerPolicy::Submenu => "submenu",
        HandlerPolicy::Disabled => "disabled",
    }
}

pub fn dispatch(command: Command) -> CliResult<ExitCode> {
    match command {
        Command::Status { json } => status::run(json),
        Command::Classic { action } => classic::run(action),
        Command::Bench(args) => bench::run(&args),
        Command::Handlers {
            action: HandlersAction::List(args),
        } => handlers::list(&args),
        Command::Preview(args) => preview::run(&args),
        Command::Run { hidden } => crate::host::run(hidden),
        Command::Stop => crate::host::stop(),
        Command::Install(args) => install::install(&args),
        Command::Uninstall(args) => install::uninstall(&args),
        Command::Config { action } => config::run(action),
        Command::Doctor => doctor::run(),
    }
}
