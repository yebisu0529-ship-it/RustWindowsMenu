use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use rwm_core::classic::{self, ClassicRegistryScope, ClassicStatus};
use rwm_core::config::RwmConfig;
use serde::Serialize;

use super::classic::status_fields;
use super::config::ConfigState;
use super::install::{RUN_KEY, RUN_VALUE};
use crate::error::{CliResult, Context};
use crate::explorer::{self, ShellWindow, WindowKind};
use crate::hookdll::{self, HookDllProbe};
use crate::host::{self, HOST_EVENT_NAME};
use crate::output::{fields, print, print_json};
use crate::reg;

#[derive(Debug, Serialize)]
struct StatusReport {
    rwm_version: &'static str,
    /// Same shape as `rwm classic status --json`. Null when the registry could not be read.
    classic: Option<ClassicStatus>,
    classic_error: Option<String>,
    config: ConfigState,
    kill_switch: KillSwitch,
    hook_dll: HookDll,
    explorer: ExplorerCounts,
    hook_host_running: bool,
    /// The sign-in command in the HKCU Run key, when `rwm install --autostart` set one.
    autostart: Option<String>,
}

#[derive(Debug, Serialize)]
struct KillSwitch {
    path: PathBuf,
    active: bool,
}

#[derive(Debug, Serialize)]
struct HookDll {
    found: bool,
    usable: bool,
    searched: Vec<PathBuf>,
    probe: Option<HookDllProbe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct ExplorerCounts {
    ui_threads: usize,
    windows: usize,
    processes: usize,
}

impl ExplorerCounts {
    fn of(windows: &[ShellWindow]) -> Self {
        let top_level = windows
            .iter()
            .filter(|window| window.kind == WindowKind::TopLevel);
        Self {
            ui_threads: explorer::ui_thread_ids(windows).len(),
            windows: top_level.clone().count(),
            processes: top_level
                .map(|window| window.process_id)
                .collect::<BTreeSet<_>>()
                .len(),
        }
    }
}

pub fn run(json: bool) -> CliResult<ExitCode> {
    let report = gather()?;
    if json {
        print_json(&report)?;
    } else {
        print(&render(&report));
    }
    Ok(ExitCode::SUCCESS)
}

fn gather() -> CliResult<StatusReport> {
    let exe =
        std::env::current_exe().context(|| "cannot locate the running executable".to_string())?;
    let (classic, classic_error) = match classic::status(&ClassicRegistryScope::live()) {
        Ok(status) => (Some(status), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let kill_switch_path = RwmConfig::kill_switch_path();
    let probe = hookdll::locate(&exe).map(|path| hookdll::probe(&path));

    Ok(StatusReport {
        rwm_version: env!("CARGO_PKG_VERSION"),
        classic,
        classic_error,
        config: ConfigState::current(),
        kill_switch: KillSwitch {
            active: kill_switch_path.exists(),
            path: kill_switch_path,
        },
        hook_dll: HookDll {
            found: probe.is_some(),
            usable: probe.as_ref().is_some_and(HookDllProbe::is_usable),
            searched: hookdll::candidate_paths(&exe),
            probe,
        },
        explorer: ExplorerCounts::of(&explorer::enumerate_explorer_windows()),
        hook_host_running: host::is_host_running(HOST_EVENT_NAME),
        autostart: reg::read_string(RUN_KEY, RUN_VALUE).ok().flatten(),
    })
}

fn describe_hook_dll(dll: &HookDll) -> String {
    let Some(probe) = &dll.probe else {
        let searched: Vec<String> = dll
            .searched
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return format!("NOT FOUND (looked for {})", searched.join(", "));
    };
    let path = probe.path.display();
    let problems = probe.problems();
    if problems.is_empty() {
        let abi = probe.abi_version.unwrap_or_default();
        format!("{path} (ABI {abi}, OK)")
    } else {
        format!("{path} (UNUSABLE: {})", problems.join("; "))
    }
}

fn describe_explorer(counts: ExplorerCounts) -> String {
    if counts.windows == 0 {
        return "no explorer.exe windows found".to_string();
    }
    format!(
        "{} UI threads across {} top-level windows in {} explorer.exe process{}",
        counts.ui_threads,
        counts.windows,
        counts.processes,
        if counts.processes == 1 { "" } else { "es" }
    )
}

fn render(report: &StatusReport) -> String {
    let mut rows: Vec<(&str, String)> = match (&report.classic, &report.classic_error) {
        (Some(status), _) => status_fields(status),
        (None, error) => vec![(
            "Classic menu",
            format!(
                "UNKNOWN: {}",
                error.as_deref().unwrap_or("the registry could not be read")
            ),
        )],
    };
    rows.extend([
        ("Config", report.config.describe()),
        (
            "Kill switch",
            if report.kill_switch.active {
                format!(
                    "ON: {} exists, so the hook leaves every right-click to Windows",
                    report.kill_switch.path.display()
                )
            } else {
                format!("off ({} does not exist)", report.kill_switch.path.display())
            },
        ),
        ("Hook DLL", describe_hook_dll(&report.hook_dll)),
        ("Explorer", describe_explorer(report.explorer)),
        (
            "Hook host",
            if report.hook_host_running {
                "running".to_string()
            } else {
                "not running (start it with `rwm run`)".to_string()
            },
        ),
        (
            "Autostart",
            match &report.autostart {
                Some(command) => command.clone(),
                None => "off".to_string(),
            },
        ),
    ]);
    format!(
        "RustWindowsMenu {}\n\n{}",
        report.rwm_version,
        fields(&rows)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::config::ConfigHealth;

    fn window(kind: WindowKind, process_id: u32, thread_id: u32) -> ShellWindow {
        ShellWindow {
            hwnd: thread_id as isize,
            kind,
            class_name: "CabinetWClass".to_string(),
            process_id,
            thread_id,
        }
    }

    fn report(probe: Option<HookDllProbe>) -> StatusReport {
        StatusReport {
            rwm_version: "0.1.0",
            classic: Some(ClassicStatus {
                windows_build: 26100,
                windows_display_version: "24H2".to_string(),
                is_windows_11: true,
                classic_menu_enabled: true,
                menu_show_delay_ms: Some("0".to_string()),
                has_backup: true,
                clsid_registry_path: r"HKCU\Software\Classes\CLSID\{x}\InprocServer32".to_string(),
            }),
            classic_error: None,
            config: ConfigState {
                path: PathBuf::from(r"C:\Users\me\AppData\Roaming\RustWindowsMenu\config.toml"),
                health: ConfigHealth::Missing,
            },
            kill_switch: KillSwitch {
                path: PathBuf::from(r"C:\Users\me\AppData\Local\RustWindowsMenu\DISABLE_HOOK"),
                active: false,
            },
            hook_dll: HookDll {
                found: probe.is_some(),
                usable: probe.as_ref().is_some_and(HookDllProbe::is_usable),
                searched: vec![PathBuf::from(r"C:\rwm\rwm_hook.dll")],
                probe,
            },
            explorer: ExplorerCounts {
                ui_threads: 3,
                windows: 4,
                processes: 1,
            },
            hook_host_running: false,
            autostart: None,
        }
    }

    fn usable_probe() -> HookDllProbe {
        HookDllProbe {
            path: PathBuf::from(r"C:\rwm\rwm_hook.dll"),
            loaded: true,
            load_error: None,
            abi_version: Some(1),
            has_call_wnd_proc: true,
            has_detach_message_id: true,
            detach_message_id: Some(0xC1A4),
        }
    }

    #[test]
    fn explorer_counts_separate_threads_windows_and_processes() {
        let windows = [
            window(WindowKind::TopLevel, 100, 1),
            window(WindowKind::View, 100, 1),
            window(WindowKind::View, 100, 2),
            window(WindowKind::TopLevel, 100, 3),
            window(WindowKind::TopLevel, 200, 4),
        ];
        assert_eq!(
            ExplorerCounts::of(&windows),
            ExplorerCounts {
                ui_threads: 4,
                windows: 3,
                processes: 2
            }
        );
        assert_eq!(
            describe_explorer(ExplorerCounts::of(&[])),
            "no explorer.exe windows found"
        );
    }

    #[test]
    fn text_report_covers_every_section() {
        let text = render(&report(Some(usable_probe())));
        for expected in [
            "RustWindowsMenu 0.1.0",
            "Windows        Windows 11 24H2 (build 26100)",
            "Classic menu   on",
            "MenuShowDelay  0 ms",
            "Backup         saved",
            "config.toml (not created yet, built-in defaults apply)",
            "Kill switch    off",
            r"Hook DLL       C:\rwm\rwm_hook.dll (ABI 1, OK)",
            "Explorer       3 UI threads across 4 top-level windows in 1 explorer.exe process",
            "Hook host      not running",
            "Autostart      off",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn text_report_flags_a_missing_or_incompatible_hook_dll() {
        let missing = render(&report(None));
        assert!(missing.contains(r"Hook DLL       NOT FOUND (looked for C:\rwm\rwm_hook.dll)"));

        let mut old = usable_probe();
        old.abi_version = Some(7);
        let incompatible = render(&report(Some(old)));
        assert!(incompatible.contains("UNUSABLE: rwm_abi_version() is 7"));
    }

    #[test]
    fn json_report_exposes_the_documented_keys() {
        let mut status = report(Some(usable_probe()));
        status.autostart = Some(r#""C:\rwm\rwm.exe" run"#.to_string());
        let json = serde_json::to_value(&status).unwrap();

        assert_eq!(json["rwm_version"], "0.1.0");
        assert_eq!(json["classic"]["windows_build"], 26100);
        assert_eq!(json["classic"]["classic_menu_enabled"], true);
        assert_eq!(json["classic"]["menu_show_delay_ms"], "0");
        assert_eq!(json["config"]["exists"], false);
        assert_eq!(json["config"]["valid"], true);
        assert_eq!(json["kill_switch"]["active"], false);
        assert_eq!(json["hook_dll"]["found"], true);
        assert_eq!(json["hook_dll"]["usable"], true);
        assert_eq!(json["hook_dll"]["probe"]["abi_version"], 1);
        assert_eq!(json["explorer"]["ui_threads"], 3);
        assert_eq!(json["hook_host_running"], false);
        assert_eq!(json["autostart"], r#""C:\rwm\rwm.exe" run"#);
    }
}
