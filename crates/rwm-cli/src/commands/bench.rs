use std::process::ExitCode;
use std::time::Duration;

use crate::cli::BenchArgs;
use crate::error::CliResult;
use crate::output::{print, print_json};
use crate::probe::{self, ProbeRequest};
use crate::target;
use rwm_core::inspect::BenchReport;

pub fn run(args: &BenchArgs) -> CliResult<ExitCode> {
    let _resolved = target::resolve(&args.target)?;
    let request = BenchArgs {
        target: args.target.clone(),
        runs: args.runs,
        json: true,
        timeout_ms: args.timeout_ms,
    };
    let value = probe::run(
        &ProbeRequest::Bench(request),
        Duration::from_millis(args.timeout_ms),
        "bench",
    )?;
    let report: BenchReport = serde_json::from_value(value)?;
    if args.json {
        print_json(&report)?;
    } else {
        print(&render(&report));
    }
    Ok(ExitCode::SUCCESS)
}

fn render(report: &BenchReport) -> String {
    let mut text = format!("Target: {}\nCold root: {:.3} ms (query {:.3} ms)\nWarm root p50/p95: {:.3}/{:.3} ms\nWarm query p50/p95: {:.3}/{:.3} ms\nEager submenu: {:.3} ms; lazy loading saves this on open\nMSIX packaged verbs: {}\n\nPerceived remaining latency:\n", report.target.display(), report.native_cold_root_ms, report.native_cold_query_ms, report.native_warm_root.p50_ms, report.native_warm_root.p95_ms, report.native_warm_query.p50_ms, report.native_warm_query.p95_ms, report.eager_submenu_ms, report.msix_count);
    for latency in &report.simulated_latency {
        text.push_str(&format!(
            "  {} ms hold: p50 {:.3} ms, p95 {:.3} ms\n",
            latency.hold_ms, latency.p50_ms, latency.p95_ms
        ));
    }
    text.push_str("\nSlow handlers:\n");
    for handler in report
        .handlers
        .iter()
        .filter(|handler| handler.warm_ms.is_some_and(|ms| ms >= 30.0))
    {
        text.push_str(&format!(
            "  {:.3} ms {} {}\n",
            handler.warm_ms.unwrap(),
            handler.name,
            handler.clsid
        ));
    }
    text
}
