#![windows_subsystem = "windows"]

use std::ffi::OsString;
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    rwm_cli::init_logging();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|arg| arg == "--ui") {
        let mut background = false;
        let mut target: Option<PathBuf> = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            if arg == "--ui" {
                continue;
            } else if arg == "--background" {
                background = true;
            } else if arg == "--target" {
                if let Some(val) = iter.next() {
                    target = Some(PathBuf::from(val));
                }
            } else if target.is_none() && !arg.to_string_lossy().starts_with('-') {
                target = Some(PathBuf::from(arg));
            }
        }
        return match rwm_cli::gui::run(target, background) {
            Ok(code) => code,
            Err(error) => {
                rwm_cli::output::print_error_line(format!("rwm-host --ui: {error}"));
                std::process::ExitCode::FAILURE
            }
        };
    }

    match rwm_cli::host::run() {
        Ok(code) => code,
        Err(error) => {
            rwm_cli::output::print_error_line(format!("rwm-host: {error}"));
            std::process::ExitCode::FAILURE
        }
    }
}
