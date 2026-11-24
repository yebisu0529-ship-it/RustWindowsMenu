use std::process::ExitCode;

use crate::cli::HandlersAction;
use crate::error::CliResult;
use crate::output::{print_json, print_line, Align, Table};
use rwm_core::handlers;

pub fn run(action: HandlersAction) -> CliResult<ExitCode> {
    match action {
        HandlersAction::List { json } => {
            let entries = handlers::list_blocked()?;
            if json {
                print_json(&entries)?;
            } else if entries.is_empty() {
                print_line("No blocked shell extension handlers.");
            } else {
                let mut table = Table::new(&[
                    ("CLSID", Align::Left),
                    ("Name", Align::Left),
                    ("Added by RWM", Align::Left),
                ]);
                for entry in entries {
                    table.row([
                        entry.clsid,
                        entry.name.unwrap_or_default(),
                        if entry.added_by_rwm {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                    ]);
                }
                crate::output::print(&table.render());
            }
        }
        HandlersAction::Block { clsid, name } => {
            handlers::block_clsid(&clsid, name.as_deref())?;
            print_line(format!("Blocked {clsid}"));
        }
        HandlersAction::Unblock { clsid, force } => {
            handlers::unblock_clsid(&clsid, force)?;
            print_line(format!("Unblocked {clsid}"));
        }
        HandlersAction::Reset => print_line(format!(
            "Restored {} RWM-owned handler block(s).",
            handlers::restore_rwm_blocks()?
        )),
    }
    Ok(ExitCode::SUCCESS)
}
