mod bench;
mod classic;
mod config;
mod doctor;
mod handlers;
mod inspect;
mod install;
mod optimize;
mod preview;
mod status;

use std::process::ExitCode;

use crate::cli::{Command, ProbeWorkerAction};
use crate::error::CliResult;
use crate::output::print_json;

pub fn dispatch(command: Command) -> CliResult<ExitCode> {
    match command {
        Command::Install(args) => install::install(&args),
        Command::Uninstall(args) => install::uninstall(&args),
        Command::Start => crate::host::start(),
        Command::Stop => crate::host::stop(),
        Command::Status { json } => status::run(json),
        Command::Doctor(args) => doctor::run(&args),
        Command::Bench(args) => bench::run(&args),
        Command::Inspect(args) => inspect::run(&args),
        Command::Optimize(args) => optimize::run(&args),
        Command::Preview(args) => preview::run(&args),
        Command::Handlers { action } => handlers::run(action),
        Command::Classic { action } => classic::run(action),
        Command::Config { action } => config::run(action),
        Command::Run => crate::host::run(),
        Command::ProbeWorker(args) => match args.action {
            ProbeWorkerAction::Inspect(args) => {
                let target = crate::target::resolve(&args.target)?;
                print_json(&rwm_core::inspect::inspect_target(
                    &target.path,
                    target.background,
                    args.extended,
                )?)?;
                Ok(ExitCode::SUCCESS)
            }
            ProbeWorkerAction::Bench(args) => {
                let target = crate::target::resolve(&args.target)?;
                print_json(&rwm_core::inspect::benchmark_target(
                    &target.path,
                    target.background,
                    args.runs,
                )?)?;
                Ok(ExitCode::SUCCESS)
            }
            ProbeWorkerAction::Sleep { sleep_ms } => {
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                print_json(&serde_json::json!({ "slept_ms": sleep_ms }))?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
