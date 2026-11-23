use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use rwm_core::config::HandlersConfig;
use rwm_core::engine::FastMenuEngine;
use rwm_core::model::{BenchmarkReport, HandlerPolicy, HandlerStatus, HandlerTiming};
use serde::Serialize;

use super::config::load_config;
use super::policy_label;
use crate::cli::BenchArgs;
use crate::error::{CliResult, Context};
use crate::output::{print, print_json, truncate_to_width, Align, Table};
use crate::target::{resolve_target, ResolvedTarget, TargetContext, TargetInfo};

const NAME_COLUMN_WIDTH: usize = 40;

/// One COM handler's timing together with the policy the config gives it.
#[derive(Debug, Serialize)]
struct HandlerRow<'a> {
    #[serde(flatten)]
    timing: &'a HandlerTiming,
    duration_ms: f64,
    policy: HandlerPolicy,
}

/// `BenchmarkReport` plus the target and per-handler policy, with handlers slowest first.
#[derive(Debug, Serialize)]
struct BenchJson<'a> {
    target: TargetInfo,
    target_display: &'a str,
    is_background: bool,
    engine_init_us: u64,
    engine_init_ms: f64,
    stage0_us: u64,
    stage0_ms: f64,
    stage0_cold_us: u64,
    stage0_cold_ms: f64,
    stage0_item_count: usize,
    com_handlers: &'a [HandlerRow<'a>],
    total_com_us: u64,
    total_com_ms: f64,
    speedup_factor: f64,
}

pub fn run(args: &BenchArgs) -> CliResult<ExitCode> {
    let resolved = resolve_target(&args.target, &TargetContext::from_process()?)?;
    let config = Arc::new(load_config()?);

    let init_start = Instant::now();
    let engine = FastMenuEngine::new(Arc::clone(&config), false);
    let engine_init_us = init_start.elapsed().as_micros() as u64;
    let outcome = engine.benchmark(&resolved.target);
    engine.shutdown();
    let report = outcome.context(|| {
        format!(
            "benchmark of {} failed",
            resolved.target.primary_path().display()
        )
    })?;

    let rows = handler_rows(&report, &config.handlers);
    if args.json {
        print_json(&BenchJson {
            target: resolved.info(),
            target_display: &report.target_display,
            is_background: report.is_background,
            engine_init_us,
            engine_init_ms: as_ms(engine_init_us),
            stage0_us: report.stage0_us,
            stage0_ms: as_ms(report.stage0_us),
            stage0_cold_us: report.stage0_cold_us,
            stage0_cold_ms: as_ms(report.stage0_cold_us),
            stage0_item_count: report.stage0_item_count,
            com_handlers: &rows,
            total_com_us: report.total_com_us,
            total_com_ms: as_ms(report.total_com_us),
            speedup_factor: report.speedup_factor,
        })?;
    } else {
        print(&render(&report, engine_init_us, &rows, &resolved));
    }
    Ok(ExitCode::SUCCESS)
}

fn as_ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

fn handler_rows<'a>(report: &'a BenchmarkReport, handlers: &HandlersConfig) -> Vec<HandlerRow<'a>> {
    let mut rows: Vec<HandlerRow<'a>> = report
        .com_handlers
        .iter()
        .map(|timing| HandlerRow {
            timing,
            duration_ms: as_ms(timing.duration_us),
            policy: handlers.policy_for(&timing.clsid, &timing.name),
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.timing.duration_us));
    rows
}

fn status_label(status: &HandlerStatus) -> &'static str {
    match status {
        HandlerStatus::Ok => "ok",
        HandlerStatus::Empty => "empty",
        HandlerStatus::SkippedPolicy => "skipped_policy",
        HandlerStatus::Failed(_) => "failed",
    }
}

fn render(
    report: &BenchmarkReport,
    engine_init_us: u64,
    rows: &[HandlerRow<'_>],
    target: &ResolvedTarget,
) -> String {
    let cold_suffix = if report.stage0_cold_us > 0 {
        format!(
            " (cold first click: {} us / {:.3} ms)",
            report.stage0_cold_us,
            as_ms(report.stage0_cold_us)
        )
    } else {
        String::new()
    };
    let mut text = format!(
        "Target: {}\n\n\
         Engine prewarm (registry verb cache): {} us ({:.3} ms)\n\
         Stage 0 (Rust instant snapshot): {} us ({:.3} ms){cold_suffix}, {} items\n\n",
        target.describe(),
        engine_init_us,
        as_ms(engine_init_us),
        report.stage0_us,
        as_ms(report.stage0_us),
        report.stage0_item_count
    );

    if rows.is_empty() {
        text.push_str("No COM shell extension handlers are registered for this target.\n");
    } else {
        text.push_str("COM shell extension handlers, slowest first:\n");
        let mut table = Table::new(&[
            ("name", Align::Left),
            ("clsid", Align::Left),
            ("status", Align::Left),
            ("items", Align::Right),
            ("duration_ms", Align::Right),
            ("policy", Align::Left),
            ("dll_path", Align::Left),
        ]);
        for row in rows {
            table.row([
                truncate_to_width(&row.timing.name, NAME_COLUMN_WIDTH),
                row.timing.clsid.clone(),
                status_label(&row.timing.status).to_string(),
                row.timing.item_count.to_string(),
                format!("{:.3}", row.duration_ms),
                policy_label(row.policy).to_string(),
                row.timing
                    .dll_path
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            ]);
        }
        text.push_str(&table.render());
    }

    let failures: Vec<String> = rows
        .iter()
        .filter_map(|row| match &row.timing.status {
            HandlerStatus::Failed(detail) => Some(format!(
                "  {} {}: {detail}\n",
                row.timing.name, row.timing.clsid
            )),
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        text.push_str("\nFailed handlers:\n");
        text.push_str(&failures.concat());
    }

    let total = format!("{:.3}", as_ms(report.total_com_us));
    let stage0 = format!("{:.3}", as_ms(report.stage0_us));
    let width = total.len().max(stage0.len());
    text.push_str(&format!(
        "\nSummary\n  \
         Sync COM handlers total  {total:>width$} ms  ({} handlers)\n  \
         Rust Stage 0             {stage0:>width$} ms\n  \
         Speedup                  {:.1}x faster\n",
        rows.len(),
        report.speedup_factor
    ));
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rwm_core::model::SelectionTarget;

    use super::*;
    use crate::target::TargetOrigin;

    fn timing(name: &str, clsid: &str, duration_us: u64, status: HandlerStatus) -> HandlerTiming {
        HandlerTiming {
            clsid: clsid.to_string(),
            name: name.to_string(),
            dll_path: (name != "Broken").then(|| format!(r"C:\ext\{name}.dll")),
            duration_us,
            item_count: if status == HandlerStatus::Ok { 3 } else { 0 },
            status,
        }
    }

    fn report() -> BenchmarkReport {
        BenchmarkReport {
            target_display: r"C:\work\notes.txt".to_string(),
            is_background: false,
            stage0_us: 412,
            stage0_cold_us: 1280,
            stage0_item_count: 14,
            com_handlers: vec![
                timing(
                    "Fast",
                    "{00000000-0000-0000-0000-000000000001}",
                    5_000,
                    HandlerStatus::Ok,
                ),
                timing(
                    "Slow",
                    "{00000000-0000-0000-0000-000000000002}",
                    120_250,
                    HandlerStatus::Ok,
                ),
                timing(
                    "Broken",
                    "{00000000-0000-0000-0000-000000000003}",
                    40_000,
                    HandlerStatus::Failed("class not registered".to_string()),
                ),
                timing(
                    "Muted",
                    "{00000000-0000-0000-0000-000000000004}",
                    0,
                    HandlerStatus::SkippedPolicy,
                ),
            ],
            total_com_us: 165_250,
            speedup_factor: 402.092,
        }
    }

    fn target() -> ResolvedTarget {
        ResolvedTarget {
            target: SelectionTarget::SingleFile(PathBuf::from(r"C:\work\notes.txt")),
            origin: TargetOrigin::Explicit,
        }
    }

    fn handlers_config() -> HandlersConfig {
        let mut handlers = HandlersConfig::default();
        handlers
            .overrides
            .insert("Slow".to_string(), HandlerPolicy::Submenu);
        handlers.overrides.insert(
            "{00000000-0000-0000-0000-000000000004}".to_string(),
            HandlerPolicy::Disabled,
        );
        handlers
    }

    #[test]
    fn handlers_are_listed_slowest_first_with_their_policy() {
        let report = report();
        let rows = handler_rows(&report, &handlers_config());

        let order: Vec<(&str, HandlerPolicy)> = rows
            .iter()
            .map(|row| (row.timing.name.as_str(), row.policy))
            .collect();
        assert_eq!(
            order,
            vec![
                ("Slow", HandlerPolicy::Submenu),
                ("Broken", HandlerPolicy::Inline),
                ("Fast", HandlerPolicy::Inline),
                ("Muted", HandlerPolicy::Disabled),
            ]
        );
    }

    #[test]
    fn text_report_shows_stage0_table_failures_and_speedup() {
        let report = report();
        let rows = handler_rows(&report, &handlers_config());
        let text = render(&report, 11_500, &rows, &target());

        assert!(text.starts_with("Target: file C:\\work\\notes.txt\n"));
        assert!(text.contains("Engine prewarm (registry verb cache): 11500 us (11.500 ms)"));
        assert!(text.contains(
            "Stage 0 (Rust instant snapshot): 412 us (0.412 ms) (cold first click: 1280 us / 1.280 ms), 14 items"
        ));
        assert!(text.contains(
            "name    clsid                                   status          items  duration_ms  policy    dll_path"
        ));
        assert!(text.contains(
            "Slow    {00000000-0000-0000-0000-000000000002}  ok                  3      120.250  submenu   C:\\ext\\Slow.dll"
        ));
        assert!(text.contains(
            "Muted   {00000000-0000-0000-0000-000000000004}  skipped_policy      0        0.000  disabled  C:\\ext\\Muted.dll"
        ));
        assert!(text.contains(
            "Failed handlers:\n  Broken {00000000-0000-0000-0000-000000000003}: class not registered\n"
        ));
        assert!(text.contains("Sync COM handlers total  165.250 ms  (4 handlers)"));
        assert!(text.contains("Rust Stage 0               0.412 ms"));
        assert!(text.contains("Speedup                  402.1x faster"));

        let slow = text.find("Slow    {").unwrap();
        let fast = text.find("Fast    {").unwrap();
        assert!(slow < fast, "slowest handler comes first");
    }

    #[test]
    fn text_report_without_handlers_says_so() {
        let mut report = report();
        report.com_handlers.clear();
        report.total_com_us = 0;
        report.speedup_factor = 1.0;
        let text = render(&report, 9_000, &[], &target());
        assert!(text.contains("No COM shell extension handlers are registered for this target."));
        assert!(text.contains("Speedup                  1.0x faster"));
    }

    #[test]
    fn json_report_is_a_superset_of_the_core_benchmark_report() {
        let report = report();
        let rows = handler_rows(&report, &handlers_config());
        let json = serde_json::to_value(BenchJson {
            target: target().info(),
            target_display: &report.target_display,
            is_background: report.is_background,
            engine_init_us: 11_500,
            engine_init_ms: 11.5,
            stage0_us: report.stage0_us,
            stage0_ms: as_ms(report.stage0_us),
            stage0_cold_us: report.stage0_cold_us,
            stage0_cold_ms: as_ms(report.stage0_cold_us),
            stage0_item_count: report.stage0_item_count,
            com_handlers: &rows,
            total_com_us: report.total_com_us,
            total_com_ms: as_ms(report.total_com_us),
            speedup_factor: report.speedup_factor,
        })
        .unwrap();

        assert_eq!(json["target"]["kind"], "file");
        assert_eq!(json["engine_init_us"], 11_500);
        assert_eq!(json["engine_init_ms"], 11.5);
        assert_eq!(json["com_handlers"][0]["name"], "Slow");
        assert_eq!(json["com_handlers"][0]["policy"], "submenu");
        assert_eq!(json["com_handlers"][0]["duration_ms"], 120.25);
        assert_eq!(json["com_handlers"][1]["status"]["status"], "failed");

        let round_trip: BenchmarkReport =
            serde_json::from_value(json).expect("still parses as the core report");
        assert_eq!(round_trip.stage0_us, 412);
        assert_eq!(round_trip.com_handlers.len(), 4);
        assert_eq!(round_trip.com_handlers[0].duration_us, 120_250);
    }
}
