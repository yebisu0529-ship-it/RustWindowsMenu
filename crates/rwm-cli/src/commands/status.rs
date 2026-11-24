use std::path::PathBuf;
use std::process::ExitCode;

use rwm_core::build_info::{ABI_VERSION, BUILD_ID};
use rwm_core::{classic, handlers};
use serde::Serialize;

use super::config::{self, ConfigState};
use crate::error::CliResult;
use crate::hookdll::{self, HookDllProbe};
use crate::host;
use crate::output::{fields, print, print_json};

#[derive(Serialize)]
struct StatusReport {
    version: &'static str,
    build_id: &'static str,
    abi_version: u32,
    config: ConfigState,
    hook_dll: Option<HookDllProbe>,
    searched: Vec<PathBuf>,
    host_running: bool,
    classic: Option<classic::ClassicStatus>,
    classic_error: Option<String>,
    blocked_handlers: usize,
    blocked_handlers_error: Option<String>,
}

pub fn run(json: bool) -> CliResult<ExitCode> {
    let exe = std::env::current_exe()?;
    let probe = hookdll::locate(&exe).map(|path| hookdll::probe(&path));
    let classic = classic::status().map_err(|error| error.to_string());
    let blocked = handlers::list_blocked().map_err(|error| error.to_string());
    let host = host::host_state(host::HOST_MUTEX_NAME);
    let report = StatusReport {
        version: env!("CARGO_PKG_VERSION"),
        build_id: BUILD_ID,
        abi_version: ABI_VERSION,
        config: config::state(),
        searched: hookdll::candidate_paths(&exe),
        host_running: host != host::HostState::Stopped,
        classic: classic.as_ref().ok().cloned(),
        classic_error: classic.err(),
        blocked_handlers: blocked.as_ref().map_or(0, Vec::len),
        blocked_handlers_error: blocked.err(),
        hook_dll: probe,
    };
    if json {
        print_json(&report)?;
    } else {
        let dll = report.hook_dll.as_ref().map_or_else(
            || "not found".to_string(),
            |probe| {
                format!(
                    "{} ({})",
                    probe.path.display(),
                    if probe.is_usable() {
                        "OK".into()
                    } else {
                        probe.problems.join("; ")
                    }
                )
            },
        );
        print(&fields(&[
            (
                "Version",
                format!("{} / {BUILD_ID} / ABI {ABI_VERSION}", report.version),
            ),
            (
                "Config",
                if report.config.valid {
                    report.config.path.display().to_string()
                } else {
                    report.config.error.clone().unwrap_or_default()
                },
            ),
            ("Hook DLL", dll),
            (
                "Host",
                match host {
                    host::HostState::Running => "running".into(),
                    host::HostState::RunningOtherIntegrity => {
                        "running (another integrity level)".into()
                    }
                    host::HostState::Stopped => "stopped".into(),
                },
            ),
            (
                "Classic menu",
                report.classic.as_ref().map_or_else(
                    || "unknown".into(),
                    |status| {
                        if status.classic_menu_enabled {
                            "on".into()
                        } else {
                            "off".into()
                        }
                    },
                ),
            ),
            ("Blocked handlers", report.blocked_handlers.to_string()),
        ]));
    }
    Ok(ExitCode::SUCCESS)
}
