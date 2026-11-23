use std::process::ExitCode;

use rwm_core::classic::{
    self, ClassicApplyResult, ClassicError, ClassicRegistryScope, ClassicStatus,
};

use crate::cli::ClassicAction;
use crate::error::CliResult;
use crate::output::{fields, print, print_json};

/// The outcome of an enable or disable, with the classic-menu state read back from the
/// registry afterwards. `classic::disable` restores the state from before the first enable,
/// and that state can itself be "on", so the answer has to come from the registry and not
/// from which function was called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedClassic {
    /// Whether the registry was modified.
    pub changed: bool,
    pub explorer_restarted: bool,
    pub enabled: bool,
    pub menu_show_delay_ms: Option<String>,
}

impl AppliedClassic {
    fn observe(
        scope: &ClassicRegistryScope,
        result: ClassicApplyResult,
    ) -> Result<Self, ClassicError> {
        let now = classic::status(scope)?;
        Ok(Self {
            changed: result.changed,
            explorer_restarted: result.explorer_restarted,
            enabled: now.classic_menu_enabled,
            menu_show_delay_ms: now.menu_show_delay_ms,
        })
    }

    /// `wanted_on` is what the command set out to do: true for an enable.
    pub fn render(&self, wanted_on: bool) -> String {
        let state = if self.enabled { "on" } else { "off" };
        let note = if self.changed {
            ""
        } else {
            " (no change needed)"
        };
        let mut text = format!(
            "Classic context menu: {state}{note}\nMenuShowDelay: {}\n",
            describe_menu_delay(self.menu_show_delay_ms.as_deref())
        );
        if self.enabled && !wanted_on {
            text.push_str(
                "The classic menu was already on before rwm enabled it, and that earlier state \
                 is what was restored. Run `rwm classic disable` again to turn it off.\n",
            );
        }
        if self.explorer_restarted {
            text.push_str("Explorer was restarted.\n");
        } else if self.changed {
            text.push_str(
                "Explorer picks this up after it restarts: add --restart-explorer to \
                 `rwm classic enable` or `rwm classic disable`, or sign out and back in.\n",
            );
        }
        text
    }
}

pub fn enable(
    scope: &ClassicRegistryScope,
    optimize_delay: bool,
    restart_explorer: bool,
    notify_shell: bool,
) -> Result<AppliedClassic, ClassicError> {
    let result = classic::enable(scope, optimize_delay, restart_explorer, notify_shell)?;
    AppliedClassic::observe(scope, result)
}

pub fn disable(
    scope: &ClassicRegistryScope,
    restart_explorer: bool,
    notify_shell: bool,
) -> Result<AppliedClassic, ClassicError> {
    let result = classic::disable(scope, restart_explorer, notify_shell)?;
    AppliedClassic::observe(scope, result)
}

pub fn run(action: ClassicAction) -> CliResult<ExitCode> {
    let scope = ClassicRegistryScope::live();
    match action {
        ClassicAction::Enable {
            no_delay_opt,
            restart_explorer,
        } => {
            let applied = enable(&scope, !no_delay_opt, restart_explorer, true)?;
            print(&applied.render(true));
        }
        ClassicAction::Disable { restart_explorer } => {
            let applied = disable(&scope, restart_explorer, true)?;
            print(&applied.render(false));
        }
        ClassicAction::Status { json } => {
            let status = classic::status(&scope)?;
            if json {
                print_json(&status)?;
            } else {
                print(&fields(&status_fields(&status)));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn describe_windows(status: &ClassicStatus) -> String {
    let build = status.windows_build;
    let version = &status.windows_display_version;
    if status.is_windows_11 {
        format!("Windows 11 {version} (build {build})")
    } else {
        format!("build {build} ({version}), which is not Windows 11")
    }
}

pub fn describe_menu_delay(delay: Option<&str>) -> String {
    match delay {
        Some(millis) => format!("{millis} ms"),
        None => "not set (Windows default, 400 ms)".to_string(),
    }
}

/// The classic-menu lines shared by `rwm classic status` and `rwm status`.
pub fn status_fields(status: &ClassicStatus) -> Vec<(&'static str, String)> {
    vec![
        ("Windows", describe_windows(status)),
        (
            "Classic menu",
            if status.classic_menu_enabled {
                "on".to_string()
            } else {
                "off (Windows 11 menu is shown first)".to_string()
            },
        ),
        ("Registry key", status.clsid_registry_path.clone()),
        (
            "MenuShowDelay",
            describe_menu_delay(status.menu_show_delay_ms.as_deref()),
        ),
        (
            "Backup",
            if status.has_backup {
                "saved (rwm classic disable restores it)".to_string()
            } else {
                "none".to_string()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::{RegistryRoot, TempDir};

    fn status(enabled: bool) -> ClassicStatus {
        ClassicStatus {
            windows_build: 26100,
            windows_display_version: "24H2".to_string(),
            is_windows_11: true,
            classic_menu_enabled: enabled,
            menu_show_delay_ms: enabled.then(|| "0".to_string()),
            has_backup: enabled,
            clsid_registry_path: r"HKCU\Software\Classes\CLSID\{x}\InprocServer32".to_string(),
        }
    }

    fn applied(changed: bool, explorer_restarted: bool, enabled: bool) -> AppliedClassic {
        AppliedClassic {
            changed,
            explorer_restarted,
            enabled,
            menu_show_delay_ms: enabled.then(|| "0".to_string()),
        }
    }

    #[test]
    fn status_lines_describe_both_states() {
        let on = fields(&status_fields(&status(true)));
        assert!(on.contains("Windows        Windows 11 24H2 (build 26100)"));
        assert!(on.contains("Classic menu   on"));
        assert!(on.contains("MenuShowDelay  0 ms"));
        assert!(on.contains("Backup         saved"));

        let off = fields(&status_fields(&status(false)));
        assert!(off.contains("Classic menu   off"));
        assert!(off.contains("MenuShowDelay  not set (Windows default, 400 ms)"));
        assert!(off.contains("Backup         none"));
    }

    #[test]
    fn older_windows_is_called_out() {
        let mut windows_10 = status(false);
        windows_10.windows_build = 19045;
        windows_10.windows_display_version = "22H2".to_string();
        windows_10.is_windows_11 = false;
        assert_eq!(
            describe_windows(&windows_10),
            "build 19045 (22H2), which is not Windows 11"
        );
    }

    #[test]
    fn apply_output_reports_the_state_and_the_pending_restart() {
        let pending = applied(true, false, true).render(true);
        assert!(pending.starts_with("Classic context menu: on\nMenuShowDelay: 0 ms\n"));
        assert!(pending.contains("--restart-explorer"));

        let restarted = applied(true, true, true).render(true);
        assert!(restarted.contains("Explorer was restarted."));
        assert!(!restarted.contains("--restart-explorer"));

        assert_eq!(
            applied(false, false, true).render(true),
            "Classic context menu: on (no change needed)\nMenuShowDelay: 0 ms\n"
        );
        assert!(applied(true, false, false)
            .render(false)
            .starts_with("Classic context menu: off\nMenuShowDelay: not set"));
    }

    #[test]
    fn disable_that_restores_an_earlier_on_state_says_so() {
        let still_on = applied(true, false, true).render(false);
        assert!(still_on.starts_with("Classic context menu: on\n"));
        assert!(still_on.contains("Run `rwm classic disable` again to turn it off."));

        assert!(!applied(true, false, false).render(false).contains("again"));
        assert!(!applied(true, false, true).render(true).contains("again"));
    }

    #[test]
    fn applied_state_is_read_back_from_the_registry() {
        let root = RegistryRoot::new("classic_applied");
        let dir = TempDir::new("classic_applied");
        let scope = ClassicRegistryScope::isolated(root.path(), dir.path().join("backup.json"));

        let on = enable(&scope, true, false, false).expect("enable");
        assert_eq!(
            on,
            AppliedClassic {
                changed: true,
                explorer_restarted: false,
                enabled: true,
                menu_show_delay_ms: Some("0".to_string()),
            }
        );
        assert!(
            !enable(&scope, true, false, false)
                .expect("enable again")
                .changed
        );

        let off = disable(&scope, false, false).expect("disable");
        assert!(off.changed);
        assert!(!off.enabled);
    }

    #[test]
    fn disable_reports_on_when_the_menu_was_on_before_rwm() {
        let root = RegistryRoot::new("classic_prior");
        let dir = TempDir::new("classic_prior");
        let scope = ClassicRegistryScope::isolated(root.path(), dir.path().join("backup.json"));

        // A user's own tweak: the key exists, but rwm holds no backup for it.
        enable(&scope, false, false, false).expect("user tweak");
        fs::remove_file(&scope.backup_path).expect("drop the backup");

        enable(&scope, true, false, false).expect("rwm enable");
        let restored = disable(&scope, false, false).expect("rwm disable");
        assert!(
            restored.enabled,
            "the earlier state was on, and it is on again"
        );

        let off = disable(&scope, false, false).expect("second disable");
        assert!(!off.enabled, "without a backup, disable turns the menu off");
    }
}
