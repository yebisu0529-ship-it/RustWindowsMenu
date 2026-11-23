use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::io::Write;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rwm_core::config::RwmConfig;
use rwm_core::engine::FastMenuEngine;
use rwm_core::model::SelectionTarget;
use rwm_ui::popup::{show_popup_menu, PopupOptions};
use serde::Serialize;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, FillRect, GetStockObject, InvalidateRect, RoundRect, SelectObject,
    SetBkMode, SetTextColor, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_SWISS, HBRUSH, HGDIOBJ, NULL_BRUSH,
    OUT_DEFAULT_PRECIS, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToParent, SHParseDisplayName,
    CMF_EXPLORE, CMF_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EndMenu, EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, KillTimer, PeekMessageW, PostMessageW,
    RegisterClassExW, SendMessageTimeoutW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    ShowWindow, TrackPopupMenuEx, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HMENU, MSG,
    PM_REMOVE, SMTO_ABORTIFHUNG, SW_SHOW, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_TOPALIGN,
    WM_CANCELMODE, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NCCREATE,
    WM_NULL, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

#[link(name = "user32")]
extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: windows::Win32::Graphics::Gdi::HDC, flags: u32) -> BOOL;
}

const W: i32 = 640;
const H: i32 = 720;
const ANCHOR_DX: i32 = 205;
const ANCHOR_DY: i32 = 108;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DemoMode {
    Windows,
    Rwm,
}

struct AppState {
    mode: DemoMode,
    file_name: String,
    clicked: bool,
    status_line: String,
    active_cm2: RefCell<Option<IContextMenu2>>,
    active_cm3: RefCell<Option<IContextMenu3>>,
}

struct Args {
    mode: DemoMode,
    file: PathBuf,
    x: i32,
    y: i32,
    hold_ms: u32,
    record_dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut mode = DemoMode::Rwm;
    let mut file = PathBuf::from(r"M:\Tools\RustWindowsMenu\Cargo.toml");
    let mut x = 120;
    let mut y = 60;
    let mut hold_ms = 1600;
    let mut record_dir = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => match args.next().as_deref() {
                Some("windows") => mode = DemoMode::Windows,
                Some("rwm") => mode = DemoMode::Rwm,
                other => panic!("unknown --mode {other:?}"),
            },
            "--file" => file = PathBuf::from(args.next().expect("missing --file")),
            "--x" => x = args.next().expect("missing --x").parse().unwrap(),
            "--y" => y = args.next().expect("missing --y").parse().unwrap(),
            "--hold-ms" => hold_ms = args.next().expect("missing --hold-ms").parse().unwrap(),
            "--record-dir" => record_dir = Some(PathBuf::from(args.next().expect("missing --record-dir"))),
            other => panic!("unexpected argument {other}"),
        }
    }

    Args {
        mode,
        file,
        x,
        y,
        hold_ms,
        record_dir,
    }
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn make_font(height: i32, weight: i32) -> HGDIOBJ {
    unsafe {
        HGDIOBJ(
            CreateFontW(
                height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                w!("Segoe UI"),
            )
            .0,
        )
    }
}

fn fill_solid(hdc: windows::Win32::Graphics::Gdi::HDC, rect: &RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn draw_text(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    font: HGDIOBJ,
    text: &str,
    rect: &mut RECT,
    rgb: u32,
) {
    unsafe {
        let old = SelectObject(hdc, font);
        SetTextColor(hdc, COLORREF(rgb));
        let mut units: Vec<u16> = text.encode_utf16().collect();
        DrawTextW(hdc, &mut units, rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, old);
    }
}

fn paint_backdrop(hwnd: HWND, state: &AppState) {
    use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        paint_backdrop_dc(hdc, state);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn paint_backdrop_dc(hdc: windows::Win32::Graphics::Gdi::HDC, state: &AppState) {
    unsafe {
        // GDI COLORREF is 0x00BBGGRR
        let bg_dark = 0x001B1818;
        let header_bg = 0x00272220;
        let border_col = 0x003F3A36;
        let row_bg = if state.clicked { 0x0038302A } else { 0x002A2523 };
        let accent = match state.mode {
            DemoMode::Windows => 0x004444EF, // coral-red (#EF4444)
            DemoMode::Rwm => 0x0081B910,     // emerald (#10B981)
        };

        fill_solid(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: W,
                bottom: H,
            },
            bg_dark,
        );
        fill_solid(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: W,
                bottom: 66,
            },
            header_bg,
        );
        fill_solid(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: W,
                bottom: 4,
            },
            accent,
        );
        fill_solid(
            hdc,
            &RECT {
                left: 0,
                top: 66,
                right: W,
                bottom: 67,
            },
            border_col,
        );

        SetBkMode(hdc, TRANSPARENT);
        let title_font = make_font(22, 700);
        let sub_font = make_font(15, 400);
        let row_font = make_font(16, 600);
        let badge_font = make_font(14, 700);

        let (title, sub) = match state.mode {
            DemoMode::Windows => (
                "Windows Classic Menu (Standard)",
                "Synchronous COM QueryContextMenu on UI Thread",
            ),
            DemoMode::Rwm => (
                "RustWindowsMenu (Rust + Direct2D)",
                "Stage 0 Instant Cache + Async STA COM Hydration",
            ),
        };
        draw_text(
            hdc,
            title_font,
            title,
            &mut RECT {
                left: 20,
                top: 10,
                right: W - 20,
                bottom: 36,
            },
            0x00FAFAFA,
        );
        draw_text(
            hdc,
            sub_font,
            sub,
            &mut RECT {
                left: 20,
                top: 36,
                right: W - 20,
                bottom: 58,
            },
            0x00B4B4B0,
        );

        // Explorer-like selected file row
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border_col));
        let brush = CreateSolidBrush(COLORREF(row_bg));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let _ = RoundRect(hdc, 24, 84, W - 24, 124, 8, 8);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));

        draw_text(
            hdc,
            row_font,
            &state.file_name,
            &mut RECT {
                left: 42,
                top: 84,
                right: 360,
                bottom: 124,
            },
            0x00F4F4F5,
        );
        draw_text(
            hdc,
            sub_font,
            "TOML Source File",
            &mut RECT {
                left: 440,
                top: 84,
                right: W - 36,
                bottom: 124,
            },
            0x009CA3AF,
        );

        // Bright sync marker inside (186..206, 98..112) when right-clicked!
        if state.clicked {
            // Vivid cyan (#00E5FF -> COLORREF 0x00FFE500)
            fill_solid(
                hdc,
                &RECT {
                    left: 186,
                    top: 96,
                    right: 206,
                    bottom: 112,
                },
                0x00FFE500,
            );
            let ring_pen = CreatePen(PS_SOLID, 2, COLORREF(0x00FFE500));
            let null_brush: HBRUSH = HBRUSH(GetStockObject(NULL_BRUSH).0);
            let op = SelectObject(hdc, HGDIOBJ(ring_pen.0));
            let ob = SelectObject(hdc, HGDIOBJ(null_brush.0));
            let _ = RoundRect(hdc, 180, 90, 294, 118, 6, 6);
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            let _ = DeleteObject(HGDIOBJ(ring_pen.0));

            draw_text(
                hdc,
                badge_font,
                "CLICK!",
                &mut RECT {
                    left: 212,
                    top: 90,
                    right: 292,
                    bottom: 118,
                },
                0x00FFE500,
            );
        }

        if !state.status_line.is_empty() {
            draw_text(
                hdc,
                sub_font,
                &state.status_line,
                &mut RECT {
                    left: 24,
                    top: H - 38,
                    right: W - 24,
                    bottom: H - 10,
                },
                0x00D4D4D8,
            );
        }

        let _ = DeleteObject(badge_font);
        let _ = DeleteObject(row_font);
        let _ = DeleteObject(sub_font);
        let _ = DeleteObject(title_font);
    }
}

fn render_backdrop_bgra(state: &AppState) -> Vec<u8> {
    unsafe {
        let hdc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection");
        let old = SelectObject(hdc, HGDIOBJ(bmp.0));
        paint_backdrop_dc(hdc, state);
        let bytes = std::slice::from_raw_parts(bits.cast::<u8>(), (W * H * 4) as usize).to_vec();
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        bytes
    }
}

struct EnumTarget {
    pid: u32,
    class_name: &'static str,
    found: Vec<HWND>,
}

unsafe extern "system" fn collect_matching_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let target = &mut *(lparam.0 as *mut EnumTarget);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != target.pid {
        return TRUE;
    }
    let mut cls = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut cls);
    if len > 0 && String::from_utf16_lossy(&cls[..len as usize]) == target.class_name {
        target.found.push(hwnd);
    }
    TRUE
}

fn find_own_menu_window(class_name: &'static str) -> Option<HWND> {
    let mut state = EnumTarget {
        pid: unsafe { GetCurrentProcessId() },
        class_name,
        found: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_matching_window),
            LPARAM(&mut state as *mut EnumTarget as isize),
        );
    }
    state.found.into_iter().next()
}

fn capture_menu_window(hwnd: HWND, use_print_window: bool) -> Option<(i32, i32, i32, i32, Vec<u8>)> {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return None;
    }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut res = 0usize;
    let alive = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            15,
            Some(&mut res),
        )
    };
    if alive.0 == 0 {
        return None;
    }
    unsafe {
        let hdc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc);
            return None;
        }
        let old = SelectObject(hdc, HGDIOBJ(bmp.0));
        let ok = if use_print_window {
            PrintWindow(hwnd, hdc, 2).as_bool()
        } else {
            SendMessageTimeoutW(
                hwnd,
                0x0318, // WM_PRINTCLIENT
                WPARAM(hdc.0 as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                40,
                Some(&mut res),
            )
            .0 != 0
        };
        let mut out = None;
        if ok {
            let len = (w * h * 4) as usize;
            let bytes = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
            let u32_pixels = std::slice::from_raw_parts(bits.cast::<u32>(), (w * h) as usize);
            let mut distinct = HashSet::new();
            for &px in u32_pixels.iter().step_by(3) {
                distinct.insert(px);
                if distinct.len() > 4 {
                    break;
                }
            }
            if distinct.len() > 4 {
                out = Some((rc.left, rc.top, w, h, bytes));
            }
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        out
    }
}

#[derive(Serialize)]
struct RecordedSample {
    rel_ms: f64,
    rel_x: i32,
    rel_y: i32,
    width: i32,
    height: i32,
    file: String,
}

#[derive(Serialize)]
struct RecordingMeta {
    mode: String,
    w: i32,
    h: i32,
    first_menu_ms: Option<f64>,
    metric_label: String,
    get_ui_ms: Option<f64>,
    query_ms: Option<f64>,
    total_ms: Option<f64>,
    stage0_us: Option<u64>,
    hydrated_groups: Option<usize>,
    final_item_count: Option<usize>,
    rendered_with_d2d: Option<bool>,
    samples: Vec<RecordedSample>,
}

struct RecordedBuffers {
    frames: Vec<Vec<u8>>,
    samples: Vec<(f64, i32, i32, i32, i32, usize)>,
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AppState;
    if !ptr.is_null()
        && matches!(
            msg,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        )
    {
        let st = &*ptr;
        if let Some(ref cm3) = *st.active_cm3.borrow() {
            let mut lr = LRESULT::default();
            if cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut lr)).is_ok() {
                return lr;
            }
        }
        if let Some(ref cm2) = *st.active_cm2.borrow() {
            if cm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                return LRESULT(0);
            }
        }
    }
    if msg == WM_PAINT && !ptr.is_null() {
        paint_backdrop(hwnd, &*ptr);
        return LRESULT(0);
    }
    if msg == WM_TIMER && wparam.0 == 99 {
        let _ = KillTimer(hwnd, 99);
        let _ = EndMenu();
        let _ = PostMessageW(hwnd, WM_CANCELMODE, WPARAM(0), LPARAM(0));
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn pump_for(ms: u64) {
    let deadline = Instant::now() + std::time::Duration::from_millis(ms);
    let mut msg = MSG::default();
    while Instant::now() < deadline {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn trigger_windows_menu(
    hwnd: HWND,
    state: &AppState,
    file: &Path,
    anchor: (i32, i32),
    hold_ms: u32,
) -> (f64, f64, f64) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let t0 = Instant::now();

        let wide_path = wide_null(&file.to_string_lossy());
        let mut pidl = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR(wide_path.as_ptr()), None, &mut pidl, 0, None)
            .expect("SHParseDisplayName");
        let mut child = std::ptr::null_mut();
        let folder: IShellFolder = SHBindToParent(pidl, Some(&mut child)).expect("SHBindToParent");
        let cm: IContextMenu = folder
            .GetUIObjectOf(hwnd, &[child.cast_const()], None)
            .expect("GetUIObjectOf(IContextMenu)");
        CoTaskMemFree(Some(pidl.cast()));
        let get_ui_ms = t0.elapsed().as_secs_f64() * 1000.0;

        *state.active_cm3.borrow_mut() = cm.cast::<IContextMenu3>().ok();
        *state.active_cm2.borrow_mut() = cm.cast::<IContextMenu2>().ok();

        let hmenu: HMENU = CreatePopupMenu().expect("CreatePopupMenu");
        let t1 = Instant::now();
        cm.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL | CMF_EXPLORE)
            .expect("QueryContextMenu");
        let query_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;

        SetTimer(hwnd, 99, hold_ms, None);
        let hwnd_val = hwnd.0 as usize;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms as u64 + 120));
            let _ = PostMessageW(HWND(hwnd_val as *mut _), WM_CANCELMODE, WPARAM(0), LPARAM(0));
        });

        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenuEx(
            hmenu,
            (TPM_LEFTALIGN | TPM_TOPALIGN | TPM_NONOTIFY | TPM_RETURNCMD).0,
            anchor.0,
            anchor.1,
            hwnd,
            None,
        );
        *state.active_cm3.borrow_mut() = None;
        *state.active_cm2.borrow_mut() = None;
        let _ = DestroyMenu(hmenu);
        drop(cm);
        CoUninitialize();

        (get_ui_ms, query_ms, total_ms)
    }
}

fn trigger_rwm_menu(
    hwnd: HWND,
    engine: &FastMenuEngine,
    target: SelectionTarget,
    anchor: (i32, i32),
    hold_ms: u32,
) -> (u64, usize, usize, bool) {
    let snapshot = engine.build_instant_snapshot(target, false);
    let session_id = snapshot.session_id;
    let stage0_us = snapshot.stage0_us;

    let result = show_popup_menu(
        engine,
        snapshot,
        PopupOptions {
            anchor,
            owner_hwnd: hwnd,
            show_timings: true,
            submenu_hover_delay_ms: 140,
            smoke_test_ms: Some(hold_ms),
        },
    );
    engine.close_session(session_id);

    (
        stage0_us,
        result.hydrated_groups,
        result.final_item_count,
        result.rendered_with_d2d,
    )
}

fn spawn_recorder(
    mode: DemoMode,
    origin_pos: (i32, i32),
    t_origin: Instant,
    click_us: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<RecordedBuffers> {
    let class_name = match mode {
        DemoMode::Windows => "#32768",
        DemoMode::Rwm => "RustWindowsMenuPopup",
    };
    let use_print_window = mode == DemoMode::Windows;
    std::thread::spawn(move || {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut samples: Vec<(f64, i32, i32, i32, i32, usize)> = Vec::new();
        while !stop_flag.load(Ordering::Acquire) {
            let cu = click_us.load(Ordering::Acquire);
            if cu == u64::MAX {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            if let Some(hm) = find_own_menu_window(class_name) {
                if let Some((left, top, w, h, bytes)) = capture_menu_window(hm, use_print_window) {
                    let now_us = t_origin.elapsed().as_micros() as u64;
                    let rel_ms = (now_us.saturating_sub(cu)) as f64 / 1000.0;
                    let file_idx = match frames.last() {
                        Some(prev) if *prev == bytes => frames.len() - 1,
                        _ => {
                            frames.push(bytes);
                            frames.len() - 1
                        }
                    };
                    samples.push((
                        rel_ms,
                        left - origin_pos.0,
                        top - origin_pos.1,
                        w,
                        h,
                        file_idx,
                    ));
                }
            }
            let elapsed_after_click_ms = (t_origin.elapsed().as_micros() as u64)
                .saturating_sub(cu)
                / 1000;
            let step_ms = if elapsed_after_click_ms < 120 { 3 } else { 14 };
            std::thread::sleep(std::time::Duration::from_millis(step_ms));
        }
        RecordedBuffers { frames, samples }
    })
}

fn main() {
    let args = parse_args();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let target = SelectionTarget::from_path(&args.file, false);
    let rwm_engine = if args.mode == DemoMode::Rwm {
        let mut cfg = RwmConfig::load_or_default().unwrap_or_default();
        cfg.appearance.show_timings = true;
        let engine = FastMenuEngine::new(Arc::new(cfg), false);
        let warm = engine.build_instant_snapshot(target.clone(), false);
        engine.close_session(warm.session_id);
        Some(engine)
    } else {
        None
    };

    let mut state = Box::new(AppState {
        mode: args.mode,
        file_name: args
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| args.file.display().to_string()),
        clicked: false,
        status_line: "Waiting for right-click...".to_string(),
        active_cm2: RefCell::new(None),
        active_cm3: RefCell::new(None),
    });

    let hwnd = unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let cls = w!("RustWindowsMenuDemoBackdrop");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: cls,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls,
            w!("RustWindowsMenu Demo"),
            WS_POPUP,
            args.x,
            args.y,
            W,
            H,
            None,
            None,
            hinst,
            Some((&*state as *const AppState) as *const c_void),
        )
        .expect("CreateWindowExW");
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        hwnd
    };

    if let Some(ref dir) = args.record_dir {
        fs::create_dir_all(dir).expect("create record-dir");
        fs::write(dir.join("idle_bg.bgra"), render_backdrop_bgra(&state)).expect("write idle_bg");
    }

    let t_origin = Instant::now();
    let click_us = Arc::new(AtomicU64::new(u64::MAX));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let recorder = args.record_dir.as_ref().map(|_| {
        spawn_recorder(
            args.mode,
            (args.x, args.y),
            t_origin,
            Arc::clone(&click_us),
            Arc::clone(&stop_flag),
        )
    });

    pump_for(700);

    // Paint vivid click sync marker and immediately trigger real menu.
    state.clicked = true;
    state.status_line = match args.mode {
        DemoMode::Windows => "Right-clicked! Blocking UI thread on COM QueryContextMenu...".to_string(),
        DemoMode::Rwm => "Right-clicked! Stage 0 open -> hydrating COM asynchronously...".to_string(),
    };
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
        let _ = UpdateWindow(hwnd);
    }
    if let Some(ref dir) = args.record_dir {
        fs::write(dir.join("clicked_bg.bgra"), render_backdrop_bgra(&state))
            .expect("write clicked_bg");
    }

    let anchor = (args.x + ANCHOR_DX, args.y + ANCHOR_DY);
    click_us.store(t_origin.elapsed().as_micros() as u64, Ordering::Release);
    println!("EVENT click");
    let _ = std::io::stdout().flush();

    let mut win_stats = None;
    let mut rwm_stats = None;
    match args.mode {
        DemoMode::Windows => {
            win_stats = Some(trigger_windows_menu(
                hwnd,
                &state,
                &args.file,
                anchor,
                args.hold_ms,
            ));
        }
        DemoMode::Rwm => {
            let engine = rwm_engine.as_ref().expect("engine initialized");
            rwm_stats = Some(trigger_rwm_menu(
                hwnd,
                engine,
                target,
                anchor,
                args.hold_ms,
            ));
            engine.shutdown();
        }
    }

    stop_flag.store(true, Ordering::Release);
    let recorded = recorder.map(|h| h.join().expect("recorder thread"));
    let first_menu_ms = recorded
        .as_ref()
        .and_then(|r| r.samples.first().map(|s| s.0));

    if let Some((get_ui_ms, query_ms, total_ms)) = win_stats {
        println!(
            "RESULT mode=windows get_ui_ms={get_ui_ms:.3} query_ms={query_ms:.3} total_ms={total_ms:.3} first_menu_ms={:.3}",
            first_menu_ms.unwrap_or(total_ms)
        );
    }
    if let Some((stage0_us, hydrated_groups, final_item_count, rendered_with_d2d)) = rwm_stats {
        println!(
            "RESULT mode=rwm stage0_us={stage0_us} hydrated_groups={hydrated_groups} final_item_count={final_item_count} rendered_with_d2d={rendered_with_d2d} first_menu_ms={:.3}",
            first_menu_ms.unwrap_or(0.0)
        );
    }

    if let (Some(ref dir), Some(rec)) = (&args.record_dir, recorded) {
        let mut frame_names = Vec::with_capacity(rec.frames.len());
        for (i, bytes) in rec.frames.iter().enumerate() {
            let name = format!("frame_{i:04}.bgra");
            fs::write(dir.join(&name), bytes).expect("write recorded frame");
            frame_names.push(name);
        }
        let samples = rec
            .samples
            .into_iter()
            .map(|(rel_ms, rel_x, rel_y, width, height, file_idx)| RecordedSample {
                rel_ms,
                rel_x,
                rel_y,
                width,
                height,
                file: frame_names[file_idx].clone(),
            })
            .collect();
        let meta = RecordingMeta {
            mode: match args.mode {
                DemoMode::Windows => "windows".to_string(),
                DemoMode::Rwm => "rwm".to_string(),
            },
            w: W,
            h: H,
            first_menu_ms,
            metric_label: match win_stats {
                Some((_, _, total_ms)) => format!("{total_ms:.0} ms COM block"),
                None => match rwm_stats {
                    Some((stage0_us, _, _, _)) => {
                        format!("{stage0_us} us ({:.3} ms) Stage 0", stage0_us as f64 / 1000.0)
                    }
                    None => String::new(),
                },
            },
            get_ui_ms: win_stats.map(|v| v.0),
            query_ms: win_stats.map(|v| v.1),
            total_ms: win_stats.map(|v| v.2),
            stage0_us: rwm_stats.map(|v| v.0),
            hydrated_groups: rwm_stats.map(|v| v.1),
            final_item_count: rwm_stats.map(|v| v.2),
            rendered_with_d2d: rwm_stats.map(|v| v.3),
            samples,
        };
        let json = serde_json::to_string_pretty(&meta).expect("serialize meta");
        fs::write(dir.join("meta.json"), json).expect("write meta.json");
    }

    state.clicked = false;
    state.status_line = "Done".to_string();
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
        let _ = UpdateWindow(hwnd);
    }
    pump_for(200);

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}
