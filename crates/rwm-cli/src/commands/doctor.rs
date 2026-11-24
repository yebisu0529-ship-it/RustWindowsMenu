use std::process::ExitCode;
use std::time::Duration;

use rwm_core::{classic, handlers};
use serde::Serialize;

use super::config;
use crate::cli::{DoctorArgs, InspectArgs};
use crate::error::CliResult;
use crate::hookdll;
use crate::host;
use crate::output::{print, print_json};
use crate::probe::{self, ProbeRequest};

#[derive(Serialize)]
struct Check {
    name: String,
    ok: bool,
    warning: bool,
    detail: String,
}
impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            warning: false,
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            warning: false,
            detail: detail.into(),
        }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            warning: true,
            detail: detail.into(),
        }
    }
}

#[derive(Serialize)]
struct DoctorReport {
    checks: Vec<Check>,
    ok: bool,
}

pub fn run(args: &DoctorArgs) -> CliResult<ExitCode> {
    let mut checks = Vec::new();
    match classic::status() {
        Ok(status) if status.is_windows_11 => checks.push(Check::pass(
            "OS",
            format!(
                "Windows 11 {} build {}",
                status.windows_display_version, status.windows_build
            ),
        )),
        Ok(status) => checks.push(Check::fail(
            "OS",
            format!("build {} is older than Windows 11", status.windows_build),
        )),
        Err(error) => checks.push(Check::fail("OS", error.to_string())),
    }
    let config = config::state();
    if config.legacy_v01_keys {
        checks.push(Check::warn(
            "Config",
            "legacy v0.1 keys detected; run `rwm config reset` to migrate to v0.2",
        ));
    } else if !config.valid {
        checks.push(Check::fail("Config", config.error.unwrap_or_default()));
    } else {
        checks.push(Check::pass("Config", config.path.display().to_string()));
    }
    let exe = std::env::current_exe()?;
    match hookdll::locate(&exe) {
        None => checks.push(Check::fail(
            "Hook DLL",
            hookdll::not_found_error(&exe).to_string(),
        )),
        Some(path) => {
            let probe = hookdll::probe(&path);
            if probe.is_usable() {
                checks.push(Check::pass(
                    "Hook DLL",
                    format!("{} ABI 2 BUILD_ID match", path.display()),
                ));
            } else {
                checks.push(Check::fail("Hook DLL", probe.problems.join("; ")));
            }
        }
    }
    match host::host_state(host::HOST_MUTEX_NAME) {
        host::HostState::Running => checks.push(Check::pass("Host", "running")),
        host::HostState::RunningOtherIntegrity => checks.push(Check::warn(
            "Host",
            "running at another integrity level (stop it from that terminal)",
        )),
        host::HostState::Stopped => checks.push(Check::pass("Host", "stopped")),
    }
    match classic::status() {
        Ok(status) => checks.push(Check::pass(
            "Classic registry",
            format!(
                "enabled={} backup={}",
                status.classic_menu_enabled, status.has_backup
            ),
        )),
        Err(error) => checks.push(Check::fail("Classic registry", error.to_string())),
    }
    match handlers::list_blocked() {
        Ok(entries) => checks.push(Check::pass("Blocked handlers", entries.len().to_string())),
        Err(error) => checks.push(Check::fail("Blocked handlers", error.to_string())),
    }
    let inspect = InspectArgs {
        target: args.target.clone(),
        extended: false,
        json: true,
        timeout_ms: args.timeout_ms,
    };
    match probe::run(
        &ProbeRequest::Inspect(inspect),
        Duration::from_millis(args.timeout_ms),
        "doctor shell",
    ) {
        Ok(_) => checks.push(Check::pass(
            "Shell probe",
            "watchdog-isolated probe completed",
        )),
        Err(error) => checks.push(Check::fail("Shell probe", error.to_string())),
    }
    let report = DoctorReport {
        ok: checks.iter().all(|check| check.ok),
        checks,
    };
    if args.json {
        print_json(&report)?;
    } else {
        let mut text = String::new();
        for check in &report.checks {
            text.push_str(&format!(
                "{} {}: {}\n",
                if !check.ok {
                    "[FAIL]"
                } else if check.warning {
                    "[WARN]"
                } else {
                    "[ OK ]"
                },
                check.name,
                check.detail
            ));
        }
        print(&text);
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
