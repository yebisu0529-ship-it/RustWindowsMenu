use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rwm_core::engine::FastMenuEngine;
use rwm_core::model::EntryAction;
use rwm_ui::popup::{show_popup_menu, PopupOptions, PopupResult};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, TRUE};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetCursorPos, GetSystemMetrics, GetWindowThreadProcessId,
    IsWindowVisible, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN,
};

use super::config::load_config;
use crate::cli::PreviewArgs;
use crate::error::{CliResult, Context};
use crate::output::print_line;
use crate::target::{resolve_target, TargetContext};

/// How long a command gets to open a window of its own before the preview stops waiting.
const WINDOW_APPEAR_GRACE: Duration = Duration::from_millis(600);

pub fn run(args: &PreviewArgs) -> CliResult<ExitCode> {
    let resolved = resolve_target(&args.target, &TargetContext::from_process()?)?;
    let config = Arc::new(load_config()?);

    // Explorer is per-monitor DPI aware. Matching it gives the preview the size and
    // sharpness the menu has inside Explorer.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let engine = FastMenuEngine::new(Arc::clone(&config), false);
    let snapshot = engine.build_instant_snapshot(resolved.target.clone(), false);
    let session_id = snapshot.session_id;
    let stage0_us = snapshot.stage0_us;
    let options = PopupOptions {
        anchor: match args.smoke_test_ms {
            Some(_) => screen_center(),
            None => cursor_position(),
        },
        owner_hwnd: HWND::default(),
        show_timings: args.timings || config.appearance.show_timings,
        submenu_hover_delay_ms: config.behavior.submenu_hover_delay_ms,
        smoke_test_ms: args.smoke_test_ms,
    };

    print_line(format!("Target: {}", resolved.describe()));
    let popup_opened = Instant::now();
    let result = show_popup_menu(&engine, snapshot, options);
    tracing::debug!(
        open_ms = popup_opened.elapsed().as_millis() as u64,
        "popup closed"
    );

    // A smoke test only renders. It never runs a command, whatever the popup reports.
    let dispatched = match (&result.chosen_action, args.smoke_test_ms) {
        (Some(action), None) => {
            print_line(format!("Selected: {}", describe_action(action)));
            let outcome = engine.dispatch_action(action, &resolved.target, HWND::default());
            if outcome.is_ok() {
                wait_for_own_windows();
            }
            outcome
        }
        (None, None) => {
            print_line("Menu closed without a selection.");
            Ok(())
        }
        (_, Some(_)) => Ok(()),
    };
    let teardown_started = Instant::now();
    engine.close_session(session_id);
    engine.shutdown();
    tracing::debug!(
        teardown_ms = teardown_started.elapsed().as_millis() as u64,
        "engine shut down"
    );

    if args.smoke_test_ms.is_some() {
        print_line(format!(
            "preview smoke-test: {}",
            summary(&result, stage0_us)
        ));
    } else if args.timings {
        print_line(format!("preview: {}", summary(&result, stage0_us)));
    }
    dispatched.context(|| "the selected command failed".to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn summary(result: &PopupResult, stage0_us: u64) -> String {
    format!(
        "rendered_with_d2d={} stage0_us={stage0_us} hydrated_groups={} final_item_count={}",
        result.rendered_with_d2d, result.hydrated_groups, result.final_item_count
    )
}

fn describe_action(action: &EntryAction) -> String {
    match action {
        EntryAction::CoreVerb(verb) => format!("built-in verb {verb:?}"),
        EntryAction::StaticCommand { verb, command, .. } => {
            format!("registry verb `{verb}` ({command})")
        }
        EntryAction::CustomCommand { command, args, .. } => {
            format!("custom command {command} {}", args.join(" "))
                .trim_end()
                .to_string()
        }
        EntryAction::ComCommand {
            handler_index,
            cmd_offset,
            ..
        } => format!("COM handler #{handler_index}, command {cmd_offset}"),
        EntryAction::Submenu(_) => "submenu".to_string(),
        EntryAction::Noop => "nothing".to_string(),
    }
}

fn screen_center() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) / 2,
            GetSystemMetrics(SM_CYSCREEN) / 2,
        )
    }
}

fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    match unsafe { GetCursorPos(&mut point) } {
        Ok(()) => (point.x, point.y),
        Err(_) => screen_center(),
    }
}

/// Some commands open their UI inside the calling process after `dispatch_action` returns.
/// The Properties sheet is the common one. Explorer outlives such a window, but this process
/// would take it down on exit, so the preview stays until every window it owns has closed.
fn wait_for_own_windows() {
    let appear_deadline = Instant::now() + WINDOW_APPEAR_GRACE;
    let mut seen_a_window = false;
    loop {
        pump_messages();
        let has_window = owns_visible_window();
        if has_window && !seen_a_window {
            seen_a_window = true;
            print_line("Waiting for the window opened by that command to close.");
        }
        if !has_window && (seen_a_window || Instant::now() >= appear_deadline) {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn pump_messages() {
    let mut message = MSG::default();
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

struct OwnWindowSearch {
    process_id: u32,
    found: bool,
}

unsafe extern "system" fn find_own_visible_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut OwnWindowSearch);
    let mut process_id = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id == search.process_id && IsWindowVisible(hwnd).as_bool() {
        search.found = true;
        return BOOL(0);
    }
    TRUE
}

/// The console window belongs to the console host process, so it never counts.
fn owns_visible_window() -> bool {
    let mut search = OwnWindowSearch {
        process_id: std::process::id(),
        found: false,
    };
    unsafe {
        // Stopping the enumeration early makes EnumWindows report an error.
        let _ = EnumWindows(
            Some(find_own_visible_window),
            LPARAM(&mut search as *mut OwnWindowSearch as isize),
        );
    }
    search.found
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rwm_core::model::CoreVerb;

    use super::*;

    #[test]
    fn smoke_summary_lists_the_four_fields_in_order() {
        let result = PopupResult {
            chosen_action: None,
            rendered_with_d2d: true,
            hydrated_groups: 6,
            final_item_count: 31,
        };
        assert_eq!(
            summary(&result, 412),
            "rendered_with_d2d=true stage0_us=412 hydrated_groups=6 final_item_count=31"
        );
    }

    #[test]
    fn actions_are_described_for_the_user() {
        assert_eq!(
            describe_action(&EntryAction::CoreVerb(CoreVerb::CopyPath)),
            "built-in verb CopyPath"
        );
        assert_eq!(
            describe_action(&EntryAction::StaticCommand {
                verb: "edit".to_string(),
                command: "notepad.exe %1".to_string(),
                working_dir: PathBuf::from(r"C:\work"),
            }),
            "registry verb `edit` (notepad.exe %1)"
        );
        assert_eq!(
            describe_action(&EntryAction::CustomCommand {
                command: "code".to_string(),
                args: vec!["--new-window".to_string(), "{path}".to_string()],
                working_dir: PathBuf::from(r"C:\work"),
            }),
            "custom command code --new-window {path}"
        );
        assert_eq!(
            describe_action(&EntryAction::CustomCommand {
                command: "wt.exe".to_string(),
                args: Vec::new(),
                working_dir: PathBuf::from(r"C:\work"),
            }),
            "custom command wt.exe"
        );
        assert_eq!(
            describe_action(&EntryAction::ComCommand {
                session_id: 9,
                handler_index: 2,
                cmd_offset: 14,
            }),
            "COM handler #2, command 14"
        );
    }

    #[test]
    fn a_test_process_without_windows_owns_no_visible_window() {
        assert!(!owns_visible_window());
    }

    #[test]
    fn anchors_are_on_the_primary_screen() {
        let (x, y) = screen_center();
        assert!(x > 0 && y > 0);
    }
}
