use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::cli::BenchArgs;
use crate::error::CliResult;
use crate::output::{print, print_json};
use crate::probe::{self, ProbeRequest};
use crate::target;
use rwm_core::inspect::BenchReport;
use rwm_core::ipc_cache::{now_tick_ms, SharedCacheClient, SharedCacheHost, ShowRequest};

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
        let ipc_ms = measure_cache_handoff_ms();
        print(&render(&report, ipc_ms));
    }
    Ok(ExitCode::SUCCESS)
}

fn measure_cache_handoff_ms() -> Option<f64> {
    let name = format!(r"Local\RwmBenchShm_{}", std::process::id());
    let host = SharedCacheHost::create_named(&name).ok()?;
    host.set_host_info(std::process::id(), 0x1010);
    let now = now_tick_ms();
    let hash = 0xDEAD_BEEF_CAFE_BABEu64;
    host.publish_slots(&[(hash, now + 60_000)]);
    let client = SharedCacheClient::open_named(&name)?;
    let req = ShowRequest {
        request_seq: 0,
        key_hash: hash,
        clipboard_seq: 1,
        anchor_x: 100,
        anchor_y: 100,
        defview_hwnd: 1,
        explorer_root_hwnd: 2,
        no_animation: true,
    };
    const ROUNDS: u32 = 200;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        let ready = client.is_ready_or_in_flight(hash, now);
        std::hint::black_box(ready);
        let _ = client.try_write_show(&req);
        let read = host.read_show();
        std::hint::black_box(read);
    }
    Some(start.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS))
}

fn render(report: &BenchReport, ipc_ms: Option<f64>) -> String {
    let ipc_text = ipc_ms.map_or_else(String::new, |ms| format!(" (IPC handoff {ms:.4} ms)"));
    let mut text = format!(
        "Target: {}\nCold root: {:.3} ms (query {:.3} ms)\nWarm root p50/p95: {:.3}/{:.3} ms\nWarm query p50/p95: {:.3}/{:.3} ms\nEager submenu: {:.3} ms; lazy loading saves this on open\nMSIX packaged verbs: {}\n\nv0.3 Speculative cache hit (hover / selection prebuilt):\n  0.000 ms shell query wait{ipc_text}\n\nPerceived remaining latency after button hold:\n",
        report.target.display(),
        report.native_cold_root_ms,
        report.native_cold_query_ms,
        report.native_warm_root.p50_ms,
        report.native_warm_root.p95_ms,
        report.native_warm_query.p50_ms,
        report.native_warm_query.p95_ms,
        report.eager_submenu_ms,
        report.msix_count
    );
    for latency in &report.simulated_latency {
        let hold = f64::from(latency.hold_ms);
        let v03_p50 = (report.native_warm_root.p50_ms - hold).max(0.0);
        let v03_p95 = (report.native_warm_root.p95_ms - hold).max(0.0);
        text.push_str(&format!(
            "  {:>3} ms hold: v0.3 (t=0 ms) p50 {:.3} / p95 {:.3} ms | fallback (15 ms arm) p50 {:.3} / p95 {:.3} ms\n",
            latency.hold_ms, v03_p50, v03_p95, latency.p50_ms, latency.p95_ms
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
