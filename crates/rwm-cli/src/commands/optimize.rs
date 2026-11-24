use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use rwm_core::handlers;
use rwm_core::inspect::{HandlerTiming, InspectReport};
use rwm_core::warm_pool::is_system_module;
use serde::Serialize;

use crate::cli::{InspectArgs, OptimizeArgs};
use crate::error::CliResult;
use crate::output::{print, print_json, Align, Table};
use crate::probe::{self, ProbeRequest};
use crate::target;

#[derive(Debug, Clone, Serialize)]
pub struct OptimizeCandidate {
    pub clsid: String,
    pub name: String,
    pub dll_path: Option<PathBuf>,
    pub warm_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct OptimizeReport {
    pub target: PathBuf,
    pub threshold_ms: f64,
    pub dry_run: bool,
    pub current_warm_query_ms: f64,
    pub estimated_saved_ms: f64,
    pub estimated_warm_query_ms: f64,
    pub candidates: Vec<OptimizeCandidate>,
    pub applied_blocks: usize,
}

pub fn run(args: &OptimizeArgs) -> CliResult<ExitCode> {
    let _resolved = target::resolve(&args.target)?;
    let threshold_ms = if args.threshold_ms.is_finite() && args.threshold_ms >= 0.0 {
        args.threshold_ms
    } else {
        25.0
    };
    let request = InspectArgs {
        target: args.target.clone(),
        extended: false,
        json: true,
        timeout_ms: args.timeout_ms,
    };
    let value = probe::run(
        &ProbeRequest::Inspect(request),
        Duration::from_millis(args.timeout_ms),
        "optimize",
    )?;
    let inspect: InspectReport = serde_json::from_value(value)?;
    let candidates = select_candidates(&inspect.handlers, threshold_ms);
    let estimated_saved_ms: f64 = candidates.iter().map(|c| c.warm_ms).sum();
    let estimated_warm_query_ms = (inspect.warm_query_ms - estimated_saved_ms).max(0.0);

    let mut applied_blocks = 0usize;
    if args.apply {
        for candidate in &candidates {
            handlers::block_clsid(&candidate.clsid, Some(&candidate.name))?;
            applied_blocks += 1;
        }
    }

    let report = OptimizeReport {
        target: inspect.target,
        threshold_ms,
        dry_run: !args.apply,
        current_warm_query_ms: inspect.warm_query_ms,
        estimated_saved_ms,
        estimated_warm_query_ms,
        candidates,
        applied_blocks,
    };

    if args.json {
        print_json(&report)?;
    } else {
        print(&render(&report));
    }
    Ok(ExitCode::SUCCESS)
}

fn select_candidates(handlers: &[HandlerTiming], threshold_ms: f64) -> Vec<OptimizeCandidate> {
    let mut selected: Vec<_> = handlers
        .iter()
        .filter(|handler| !handler.blocked)
        .filter(|handler| {
            handler
                .dll_path
                .as_deref()
                .is_none_or(|path| !is_system_module(path))
        })
        .filter_map(|handler| {
            let ms = handler.warm_ms?;
            (ms >= threshold_ms).then(|| OptimizeCandidate {
                clsid: handler.clsid.clone(),
                name: handler.name.clone(),
                dll_path: handler.dll_path.clone(),
                warm_ms: ms,
            })
        })
        .collect();
    selected.sort_by(|a, b| b.warm_ms.total_cmp(&a.warm_ms));
    selected
}

fn render(report: &OptimizeReport) -> String {
    let mode = if report.dry_run {
        "DRY RUN (pass --apply to block via HKCU Shell Extensions\\Blocked)"
    } else {
        "APPLIED (rollback with `rwm handlers reset`)"
    };
    let mut text = format!(
        "Target: {}\nMode: {mode}\nThreshold: {:.1} ms\nCurrent warm query: {:.3} ms\nEstimated post-block warm query: {:.3} ms (saves ~{:.3} ms)\n\n",
        report.target.display(),
        report.threshold_ms,
        report.current_warm_query_ms,
        report.estimated_warm_query_ms,
        report.estimated_saved_ms,
    );
    if report.candidates.is_empty() {
        text.push_str("No slow third-party handlers above threshold.\n");
        return text;
    }
    let mut table = Table::new(&[
        ("warm ms", Align::Right),
        ("CLSID", Align::Left),
        ("Handler", Align::Left),
        ("DLL", Align::Left),
    ]);
    for c in &report.candidates {
        table.row([
            format!("{:.3}", c.warm_ms),
            c.clsid.clone(),
            c.name.clone(),
            c.dll_path
                .as_ref()
                .map_or_else(|| "-".into(), |p| p.display().to_string()),
        ]);
    }
    text.push_str(&table.render());
    if report.dry_run {
        text.push_str("\nSuggested commands:\n");
        for c in &report.candidates {
            text.push_str(&format!(
                "  rwm handlers block {} --name \"{}\"\n",
                c.clsid, c.name
            ));
        }
    } else {
        text.push_str(&format!(
            "\nBlocked {} third-party handler(s). Run `rwm handlers reset` to undo.\n",
            report.applied_blocks
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use rwm_core::inspect::HandlerPhases;

    #[test]
    fn skips_system_and_blocked_handlers_and_sorts_third_party_culprits() {
        let windows = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let make = |clsid: &str, name: &str, dll: PathBuf, blocked: bool, warm_ms: f64| HandlerTiming {
            clsid: clsid.into(),
            name: name.into(),
            dll_path: Some(dll),
            scopes: vec!["*".into()],
            blocked,
            cold: None,
            warm: Some(HandlerPhases::default()),
            warm_ms: Some(warm_ms),
        };
        let handlers = vec![
            make(
                "{11111111-1111-1111-1111-111111111111}",
                "SystemSlow",
                windows.join(r"System32\shell32.dll"),
                false,
                120.0,
            ),
            make(
                "{22222222-2222-2222-2222-222222222222}",
                "AlreadyBlocked",
                PathBuf::from(r"C:\Program Files\Vendor\a.dll"),
                true,
                80.0,
            ),
            make(
                "{33333333-3333-3333-3333-333333333333}",
                "VendorFast",
                PathBuf::from(r"C:\Program Files\Vendor\fast.dll"),
                false,
                5.0,
            ),
            make(
                "{44444444-4444-4444-4444-444444444444}",
                "VendorSlow",
                PathBuf::from(r"C:\Program Files\Vendor\slow.dll"),
                false,
                95.0,
            ),
        ];
        let picked = select_candidates(&handlers, 25.0);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "VendorSlow");
        assert_eq!(picked[0].warm_ms, 95.0);
    }
}
