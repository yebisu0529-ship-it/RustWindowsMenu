use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rwm_core::classic::{self, ClassicRegistryScope, ClassicStatus};
use rwm_core::config::RwmConfig;

use super::classic::describe_windows;
use super::config::{ConfigHealth, ConfigState};
use crate::error::{CliResult, Context};
use crate::explorer;
use crate::hookdll::{self, HookDllProbe, HOOK_DLL_FILE, SUPPORTED_ABI};
use crate::output::{display_width, print};
use crate::reg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    /// Worth knowing, but RustWindowsMenu still works.
    Warn,
    /// Critical: `rwm doctor` exits non-zero.
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    name: &'static str,
    outcome: Outcome,
    detail: String,
}

impl Check {
    fn new(name: &'static str, outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            name,
            outcome,
            detail: detail.into(),
        }
    }
}

pub fn run() -> CliResult<ExitCode> {
    let exe =
        std::env::current_exe().context(|| "cannot locate the running executable".to_string())?;
    let probe = hookdll::locate(&exe).map(|path| hookdll::probe(&path));
    let windows = classic::status(&ClassicRegistryScope::live()).map_err(|err| err.to_string());

    let mut checks = vec![
        windows_check(windows.as_ref().map_err(String::as_str)),
        direct2d_check(rwm_ui::renderer::check_direct2d_available()),
    ];
    checks.extend(hook_dll_checks(
        &hookdll::candidate_paths(&exe),
        probe.as_ref(),
        explorer::detach_message_id(),
    ));
    checks.push(config_check(&ConfigState::current()));
    checks.push(kill_switch_check(&RwmConfig::kill_switch_path()));
    checks.push(registry_check(reg::probe_write_access()));

    print(&render(&checks));
    Ok(if has_failure(&checks) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn has_failure(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.outcome == Outcome::Fail)
}

fn windows_check(status: Result<&ClassicStatus, &str>) -> Check {
    const NAME: &str = "Windows 11";
    match status {
        Ok(status) if status.is_windows_11 => {
            Check::new(NAME, Outcome::Pass, describe_windows(status))
        }
        Ok(status) => Check::new(
            NAME,
            Outcome::Fail,
            format!(
                "{}; build 22000 or later is required",
                describe_windows(status)
            ),
        ),
        Err(err) => Check::new(
            NAME,
            Outcome::Fail,
            format!("cannot read the Windows version: {err}"),
        ),
    }
}

fn direct2d_check(available: Result<(), String>) -> Check {
    const NAME: &str = "Direct2D + DirectWrite";
    match available {
        Ok(()) => Check::new(NAME, Outcome::Pass, "both factories were created"),
        Err(err) => Check::new(NAME, Outcome::Fail, err),
    }
}

/// One check per part of the hook DLL contract, stopping at the first part that makes the
/// later ones meaningless.
fn hook_dll_checks(
    searched: &[PathBuf],
    probe: Option<&HookDllProbe>,
    host_detach_message: u32,
) -> Vec<Check> {
    let Some(probe) = probe else {
        let searched: Vec<String> = searched
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return vec![Check::new(
            "Hook DLL present",
            Outcome::Fail,
            format!(
                "{HOOK_DLL_FILE} was not found (looked for {})",
                searched.join(", ")
            ),
        )];
    };

    let mut checks = vec![Check::new(
        "Hook DLL present",
        Outcome::Pass,
        probe.path.display().to_string(),
    )];
    if let Some(error) = &probe.load_error {
        checks.push(Check::new("Hook DLL loads", Outcome::Fail, error.clone()));
        return checks;
    }
    checks.push(Check::new(
        "Hook DLL loads",
        Outcome::Pass,
        "LoadLibraryW succeeded",
    ));

    checks.push(match probe.abi_version {
        Some(SUPPORTED_ABI) => Check::new(
            "Hook ABI version",
            Outcome::Pass,
            format!("rwm_abi_version() is {SUPPORTED_ABI}"),
        ),
        Some(other) => Check::new(
            "Hook ABI version",
            Outcome::Fail,
            format!("rwm_abi_version() is {other}, but this rwm.exe needs {SUPPORTED_ABI}"),
        ),
        None => Check::new(
            "Hook ABI version",
            Outcome::Fail,
            "export rwm_abi_version is missing",
        ),
    });
    checks.push(if probe.has_call_wnd_proc {
        Check::new(
            "Hook procedure",
            Outcome::Pass,
            "rwm_call_wnd_proc is exported",
        )
    } else {
        Check::new(
            "Hook procedure",
            Outcome::Fail,
            "export rwm_call_wnd_proc is missing",
        )
    });
    checks.push(detach_message_check(probe, host_detach_message));
    checks
}

fn detach_message_check(probe: &HookDllProbe, host_detach_message: u32) -> Check {
    const NAME: &str = "Detach message";
    if !probe.has_detach_message_id {
        return Check::new(
            NAME,
            Outcome::Fail,
            "export rwm_detach_message_id is missing",
        );
    }
    match probe.detach_message_id {
        // Only a DLL with the supported ABI is asked for its message, and the ABI check
        // above already failed.
        None => Check::new(NAME, Outcome::Warn, "not queried because the ABI version differs"),
        Some(id) if !hookdll::is_registered_message(id) => Check::new(
            NAME,
            Outcome::Fail,
            format!("rwm_detach_message_id() returned {id:#x}, which is not a registered window message"),
        ),
        Some(id) if id != host_detach_message => Check::new(
            NAME,
            Outcome::Fail,
            format!(
                "rwm_detach_message_id() returned {id:#x}, but rwm.exe sends {host_detach_message:#x}"
            ),
        ),
        Some(id) => Check::new(
            NAME,
            Outcome::Pass,
            format!("rwm_detach_message_id() is {id:#x}, the message rwm.exe sends"),
        ),
    }
}

fn config_check(state: &ConfigState) -> Check {
    let outcome = match state.health {
        ConfigHealth::Missing | ConfigHealth::Valid => Outcome::Pass,
        ConfigHealth::Invalid(_) => Outcome::Fail,
    };
    Check::new("Config", outcome, state.describe())
}

fn kill_switch_check(path: &Path) -> Check {
    const NAME: &str = "Kill switch";
    if path.exists() {
        Check::new(
            NAME,
            Outcome::Warn,
            format!(
                "{} exists, so the hook leaves every right-click to Windows. Delete the file \
                 to turn the menu back on.",
                path.display()
            ),
        )
    } else {
        Check::new(NAME, Outcome::Pass, "off")
    }
}

fn registry_check(probe: std::io::Result<()>) -> Check {
    const NAME: &str = "HKCU registry write";
    match probe {
        Ok(()) => Check::new(
            NAME,
            Outcome::Pass,
            r"created and deleted a scratch key under HKCU\Software",
        ),
        Err(err) => Check::new(NAME, Outcome::Fail, err.to_string()),
    }
}

fn render(checks: &[Check]) -> String {
    let name_width = checks
        .iter()
        .map(|check| display_width(check.name))
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for check in checks {
        let tag = match check.outcome {
            Outcome::Pass => "[ OK ]",
            Outcome::Warn => "[WARN]",
            Outcome::Fail => "[FAIL]",
        };
        let pad = " ".repeat(name_width - display_width(check.name));
        text.push_str(&format!("{tag} {}{pad}  {}\n", check.name, check.detail));
    }

    let count = |outcome| {
        checks
            .iter()
            .filter(|check| check.outcome == outcome)
            .count()
    };
    text.push_str(&format!(
        "\n{} passed, {} warnings, {} failed\n",
        count(Outcome::Pass),
        count(Outcome::Warn),
        count(Outcome::Fail)
    ));
    if has_failure(checks) {
        text.push_str(
            "RustWindowsMenu will not work correctly until the failed checks are fixed.\n",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::testutil::TempDir;

    const HOST_MESSAGE: u32 = 0xC1A4;

    fn good_probe() -> HookDllProbe {
        HookDllProbe {
            path: PathBuf::from(r"C:\rwm\rwm_hook.dll"),
            loaded: true,
            load_error: None,
            abi_version: Some(SUPPORTED_ABI),
            has_call_wnd_proc: true,
            has_detach_message_id: true,
            detach_message_id: Some(HOST_MESSAGE),
        }
    }

    fn outcomes(checks: &[Check]) -> Vec<(&'static str, Outcome)> {
        checks
            .iter()
            .map(|check| (check.name, check.outcome))
            .collect()
    }

    fn windows(build: u32) -> ClassicStatus {
        ClassicStatus {
            windows_build: build,
            windows_display_version: "24H2".to_string(),
            is_windows_11: build >= 22000,
            classic_menu_enabled: false,
            menu_show_delay_ms: None,
            has_backup: false,
            clsid_registry_path: String::new(),
        }
    }

    #[test]
    fn a_contract_abiding_hook_dll_passes_every_check() {
        let checks = hook_dll_checks(&[], Some(&good_probe()), HOST_MESSAGE);
        assert_eq!(
            outcomes(&checks),
            vec![
                ("Hook DLL present", Outcome::Pass),
                ("Hook DLL loads", Outcome::Pass),
                ("Hook ABI version", Outcome::Pass),
                ("Hook procedure", Outcome::Pass),
                ("Detach message", Outcome::Pass),
            ]
        );
        assert!(!has_failure(&checks));
    }

    #[test]
    fn a_missing_hook_dll_is_critical_and_names_where_it_was_looked_for() {
        let searched = [PathBuf::from(r"C:\rwm\rwm_hook.dll")];
        let checks = hook_dll_checks(&searched, None, HOST_MESSAGE);
        assert_eq!(outcomes(&checks), vec![("Hook DLL present", Outcome::Fail)]);
        assert!(checks[0].detail.contains(r"C:\rwm\rwm_hook.dll"));
        assert!(has_failure(&checks));
    }

    #[test]
    fn a_dll_that_does_not_load_skips_the_export_checks() {
        let mut probe = good_probe();
        probe.loaded = false;
        probe.load_error = Some("%1 is not a valid Win32 application.".to_string());
        let checks = hook_dll_checks(&[], Some(&probe), HOST_MESSAGE);
        assert_eq!(
            outcomes(&checks),
            vec![
                ("Hook DLL present", Outcome::Pass),
                ("Hook DLL loads", Outcome::Fail),
            ]
        );
    }

    #[test]
    fn contract_violations_fail_their_own_check() {
        let mut old = good_probe();
        old.abi_version = Some(2);
        old.detach_message_id = None;
        assert_eq!(
            outcomes(&hook_dll_checks(&[], Some(&old), HOST_MESSAGE))[2..],
            [
                ("Hook ABI version", Outcome::Fail),
                ("Hook procedure", Outcome::Pass),
                ("Detach message", Outcome::Warn),
            ]
        );

        let mut stripped = good_probe();
        stripped.has_call_wnd_proc = false;
        stripped.has_detach_message_id = false;
        stripped.detach_message_id = None;
        assert_eq!(
            outcomes(&hook_dll_checks(&[], Some(&stripped), HOST_MESSAGE))[3..],
            [
                ("Hook procedure", Outcome::Fail),
                ("Detach message", Outcome::Fail),
            ]
        );

        let mut other_message = good_probe();
        other_message.detach_message_id = Some(0xC999);
        let check = detach_message_check(&other_message, HOST_MESSAGE);
        assert_eq!(check.outcome, Outcome::Fail);
        assert!(check.detail.contains("0xc999") && check.detail.contains("0xc1a4"));
    }

    #[test]
    fn windows_10_and_unreadable_versions_are_critical() {
        assert_eq!(windows_check(Ok(&windows(26100))).outcome, Outcome::Pass);

        let old = windows_check(Ok(&windows(19045)));
        assert_eq!(old.outcome, Outcome::Fail);
        assert!(old.detail.contains("build 22000 or later is required"));

        assert_eq!(windows_check(Err("access denied")).outcome, Outcome::Fail);
    }

    #[test]
    fn environment_checks_map_to_pass_warn_or_fail() {
        assert_eq!(direct2d_check(Ok(())).outcome, Outcome::Pass);
        let no_d2d = direct2d_check(Err("D2D1CreateFactory failed".to_string()));
        assert_eq!(
            (no_d2d.outcome, no_d2d.detail.as_str()),
            (Outcome::Fail, "D2D1CreateFactory failed")
        );

        assert_eq!(registry_check(Ok(())).outcome, Outcome::Pass);
        let denied = registry_check(Err(io::Error::from_raw_os_error(5)));
        assert_eq!(denied.outcome, Outcome::Fail);

        let dir = TempDir::new("doctor_kill_switch");
        let marker = dir.path().join("DISABLE_HOOK");
        assert_eq!(kill_switch_check(&marker).outcome, Outcome::Pass);
        std::fs::write(&marker, b"").unwrap();
        let active = kill_switch_check(&marker);
        assert_eq!(
            active.outcome,
            Outcome::Warn,
            "a kill switch is a choice, not a fault"
        );
        assert!(active.detail.contains("DISABLE_HOOK"));

        let config = |health| ConfigState {
            path: PathBuf::from(r"C:\cfg\config.toml"),
            health,
        };
        assert_eq!(
            config_check(&config(ConfigHealth::Missing)).outcome,
            Outcome::Pass
        );
        assert_eq!(
            config_check(&config(ConfigHealth::Valid)).outcome,
            Outcome::Pass
        );
        assert_eq!(
            config_check(&config(ConfigHealth::Invalid("bad".to_string()))).outcome,
            Outcome::Fail
        );
    }

    #[test]
    fn report_aligns_names_and_counts_outcomes() {
        let checks = [
            Check::new("Windows 11", Outcome::Pass, "Windows 11 24H2 (build 26100)"),
            Check::new("Kill switch", Outcome::Warn, "on"),
            Check::new("Hook DLL present", Outcome::Fail, "missing"),
        ];
        assert_eq!(
            render(&checks),
            "[ OK ] Windows 11        Windows 11 24H2 (build 26100)\n\
             [WARN] Kill switch       on\n\
             [FAIL] Hook DLL present  missing\n\
             \n\
             1 passed, 1 warnings, 1 failed\n\
             RustWindowsMenu will not work correctly until the failed checks are fixed.\n"
        );
        assert!(!render(&checks[..2]).contains("will not work"));
    }
}
