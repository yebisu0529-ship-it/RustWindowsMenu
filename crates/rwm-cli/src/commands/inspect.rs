use std::process::ExitCode;
use std::time::Duration;

use crate::cli::InspectArgs;
use crate::error::CliResult;
use crate::output::{print, print_json, Align, Table};
use crate::probe::{self, ProbeRequest};
use crate::target;
use rwm_core::inspect::InspectReport;

pub fn run(args: &InspectArgs) -> CliResult<ExitCode> {
    let resolved = target::resolve(&args.target)?;
    let value = probe::run(
        &ProbeRequest::Inspect(clone_args(args)),
        Duration::from_millis(args.timeout_ms),
        "inspect",
    )?;
    let report: InspectReport = serde_json::from_value(value)?;
    if args.json {
        print_json(&report)?;
    } else {
        print(&render(&report));
    }
    let _ = resolved;
    Ok(ExitCode::SUCCESS)
}

fn clone_args(args: &InspectArgs) -> InspectArgs {
    InspectArgs {
        target: args.target.clone(),
        extended: args.extended,
        json: true,
        timeout_ms: args.timeout_ms,
    }
}

fn render(report: &InspectReport) -> String {
    let mut text = format!(
        "Target: {}\nCold query: {:.3} ms; warm query: {:.3} ms\n\nRoot items:\n",
        report.target.display(),
        report.cold_query_ms,
        report.warm_query_ms
    );
    for item in &report.root_items {
        text.push_str(&format!(
            "  {}{}\n",
            item.clean_label,
            if item.is_separator {
                " (separator)"
            } else {
                ""
            }
        ));
    }
    text.push_str("\nHandlers (warm root ms):\n");
    let mut table = Table::new(&[
        ("ms", Align::Right),
        ("CLSID", Align::Left),
        ("Name", Align::Left),
    ]);
    for handler in &report.handlers {
        table.row([
            handler
                .warm_ms
                .map_or_else(|| "-".into(), |ms| format!("{ms:.3}")),
            handler.clsid.clone(),
            handler.name.clone(),
        ]);
    }
    text.push_str(&table.render());
    if !report.slow_culprits.is_empty() {
        text.push_str("\nSlow culprits (>= 30 ms):\n");
        for handler in &report.slow_culprits {
            text.push_str(&format!(
                "  {} — rwm handlers block {}\n",
                handler.name, handler.clsid
            ));
        }
    }
    text.push_str(&format!(
        "\nMSIX packaged verbs: {}\n",
        report.packaged_verbs.len()
    ));
    for verb in &report.packaged_verbs {
        text.push_str(&format!(
            "  {} {} {}\n",
            verb.package_full_name, verb.item_type, verb.verb_id
        ));
    }
    text
}
