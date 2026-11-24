use std::process::ExitCode;

use crate::cli::ClassicAction;
use crate::error::CliResult;
use crate::output::{fields, print, print_json, print_line};
use rwm_core::classic;

pub fn run(action: ClassicAction) -> CliResult<ExitCode> {
    match action {
        ClassicAction::Status { json } => {
            let status = classic::status()?;
            if json {
                print_json(&status)?;
            } else {
                print(&fields(&[
                    (
                        "Windows",
                        format!(
                            "{} (build {})",
                            status.windows_display_version, status.windows_build
                        ),
                    ),
                    (
                        "Classic menu",
                        if status.classic_menu_enabled {
                            "on".into()
                        } else {
                            "off".into()
                        },
                    ),
                    ("Registry key", status.clsid_registry_path.clone()),
                    (
                        "MenuShowDelay",
                        status
                            .menu_show_delay_ms
                            .clone()
                            .unwrap_or_else(|| "not set".into()),
                    ),
                    (
                        "Backup",
                        if status.has_backup {
                            "saved".into()
                        } else {
                            "none".into()
                        },
                    ),
                ]));
            }
        }
        ClassicAction::Enable { restart } => {
            if restart {
                crate::host::ensure_matching_elevation("classic enable --restart")?;
            }
            let result = classic::enable()?;
            print_line(format!(
                "Classic menu enabled (changed: {}).",
                result.changed
            ));
            if restart {
                classic::restart_explorer()?;
                print_line("Explorer restarted.");
            }
        }
        ClassicAction::Disable { force, restart } => {
            if restart {
                crate::host::ensure_matching_elevation("classic disable --restart")?;
            }
            let result = classic::disable(force)?;
            print_line(format!(
                "Classic menu disabled/restored (changed: {}).",
                result.changed
            ));
            if restart {
                classic::restart_explorer()?;
                print_line("Explorer restarted.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
