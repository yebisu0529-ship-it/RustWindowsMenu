use std::collections::HashSet;
use std::fs;
use std::marker::PhantomData;
use std::mem::size_of;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rwm_core::shell::{enable_dark_mode_menus, NativeMenu, OleGuard};
use serde::Serialize;
use windows::core::w;
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject, InvalidateRect,
    RoundRect, SelectObject, SetBkMode, SetTextColor, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_SWISS, HBRUSH, HDC, HGDIOBJ,
    NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EndMenu, EnumWindows,
    GetClassNameW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    KillTimer, PeekMessageW, PostMessageW, RegisterClassExW, SendMessageTimeoutW,
    SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    GWLP_USERDATA, MSG, PM_REMOVE, SMTO_ABORTIFHUNG, SW_SHOW, WM_CANCELMODE, WM_NULL, WM_PAINT,
    WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

#[link(name = "user32")]
extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> BOOL;
}

const W: i32 = 640;
const H: i32 = 740;
const ANCHOR_DX: i32 = 190;
const ANCHOR_DY: i32 = 104;
const TIMER_CAPTURE: usize = 98;
const TIMER_END_MENU: usize = 99;
const WM_PRINT: u32 = 0x0317;
const PRF_CHECKVISIBLE: isize = 0x01;
const PRF_NONCLIENT: isize = 0x02;
const PRF_CLIENT: isize = 0x04;
const PRF_ERASEBKGND: isize = 0x08;
const PRF_CHILDREN: isize = 0x10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DemoMode {
    Windows,
    Rwm,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Holding,
    Released,
}

struct CaptureCollector {
    pid: u32,
    origin: (i32, i32),
    t_origin: Instant,
    release_us: u64,
    frames: Vec<Vec<u8>>,
    samples: Vec<(f64, i32, i32, i32, i32, usize)>,
}

struct WindowState {
    mode: DemoMode,
    file_name: String,
    phase: Phase,
    status_line: String,
    menu: *const NativeMenu,
    collector: Option<CaptureCollector>,
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.0);
        }
    }
}

struct MenuBindingGuard<'a>(&'a mut WindowState, PhantomData<&'a NativeMenu>);
impl<'a> MenuBindingGuard<'a> {
    fn bind(state: &'a mut WindowState, menu: &'a NativeMenu) -> Self {
        state.menu = menu as *const NativeMenu;
        Self(state, PhantomData)
    }
}
impl Drop for MenuBindingGuard<'_> {
    fn drop(&mut self) {
        self.0.menu = std::ptr::null();
    }
}

struct Args {
    mode: DemoMode,
    file: PathBuf,
    background: bool,
    warm: bool,
    x: i32,
    y: i32,
    hold_ms: u32,
    button_hold_ms: u32,
    record_dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut parsed = Args {
        mode: DemoMode::Rwm,
        file: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"),
        background: false,
        warm: true,
        x: 120,
        y: 40,
        hold_ms: 1600,
        button_hold_ms: 100,
        record_dir: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--background" => {
                parsed.background = true;
                continue;
            }
            "--cold" => {
                parsed.warm = false;
                continue;
            }
            _ => {}
        }
        let value = args
            .next()
            .unwrap_or_else(|| panic!("missing value for {arg}"));
        match arg.as_str() {
            "--mode" => {
                parsed.mode = match value.as_str() {
                    "windows" => DemoMode::Windows,
                    "rwm" => DemoMode::Rwm,
                    _ => panic!("--mode must be windows or rwm"),
                };
            }
            "--file" => parsed.file = value.into(),
            "--x" => parsed.x = value.parse().unwrap(),
            "--y" => parsed.y = value.parse().unwrap(),
            "--hold-ms" => parsed.hold_ms = value.parse().unwrap(),
            "--button-hold-ms" => parsed.button_hold_ms = value.parse().unwrap(),
            "--record-dir" => parsed.record_dir = Some(value.into()),
            _ => panic!("unexpected argument {arg}"),
        }
    }
    parsed
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

fn fill_solid(hdc: HDC, rect: &RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn draw_text(hdc: HDC, font: HGDIOBJ, text: &str, rect: &mut RECT, rgb: u32, right_align: bool) {
    unsafe {
        let old = SelectObject(hdc, font);
        SetTextColor(hdc, COLORREF(rgb));
        let mut units: Vec<u16> = text.encode_utf16().collect();
        let align = if right_align { DT_RIGHT } else { DT_LEFT };
        DrawTextW(hdc, &mut units, rect, align | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, old);
    }
}

fn paint_backdrop_dc(hdc: HDC, state: &WindowState) {
    unsafe {
        let bg_dark = 0x001B_1818;
        let header_bg = 0x0027_2220;
        let border_col = 0x003F_3A36;
        let row_bg = match state.phase {
            Phase::Waiting => 0x002A_2523,
            Phase::Holding => 0x0032_2D2A,
            Phase::Released => 0x0038_302A,
        };
        let accent = match state.mode {
            DemoMode::Windows => 0x0044_44EF,
            DemoMode::Rwm => 0x0081_B910,
        };

        fill_solid(hdc, &RECT { left: 0, top: 0, right: W, bottom: H }, bg_dark);
        fill_solid(hdc, &RECT { left: 0, top: 0, right: W, bottom: 68 }, header_bg);
        fill_solid(hdc, &RECT { left: 0, top: 0, right: W, bottom: 4 }, accent);
        fill_solid(hdc, &RECT { left: 0, top: 68, right: W, bottom: 69 }, border_col);

        SetBkMode(hdc, TRANSPARENT);
        let title_font = make_font(22, 700);
        let sub_font = make_font(15, 400);
        let row_font = make_font(16, 600);
        let badge_font = make_font(14, 700);

        let (title, sub) = match state.mode {
            DemoMode::Windows => (
                "Stock Windows 11 Classic Menu",
                "Builds synchronously on release + fade animation",
            ),
            DemoMode::Rwm => (
                "RustWindowsMenu v0.2.0 (Time-Shifted Native)",
                "Prebuilt during hold + TPM_NOANIMATION (100% OS / MSIX / COM)",
            ),
        };
        draw_text(hdc, title_font, title, &mut RECT { left: 20, top: 10, right: W - 20, bottom: 38 }, 0x00FA_FAFA, false);
        draw_text(hdc, sub_font, sub, &mut RECT { left: 20, top: 38, right: W - 20, bottom: 62 }, 0x00B4_B4B0, false);

        let row_border = if state.phase == Phase::Waiting { border_col } else { accent };
        let pen = CreatePen(PS_SOLID, 1, COLORREF(row_border));
        let brush = CreateSolidBrush(COLORREF(row_bg));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let _ = RoundRect(hdc, 20, 82, W - 20, 126, 8, 8);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));

        fill_solid(hdc, &RECT { left: 34, top: 92, right: 54, bottom: 116 }, 0x00FF_AA44);
        draw_text(hdc, row_font, &state.file_name, &mut RECT { left: 66, top: 88, right: W - 210, bottom: 120 }, 0x00F4_F4F5, false);

        let null_brush = HBRUSH(GetStockObject(NULL_BRUSH).0);
        let ring = CreatePen(PS_SOLID, 2, COLORREF(accent));
        let op = SelectObject(hdc, HGDIOBJ(ring.0));
        let ob = SelectObject(hdc, HGDIOBJ(null_brush.0));
        let _ = RoundRect(hdc, ANCHOR_DX - 8, ANCHOR_DY - 8, ANCHOR_DX + 8, ANCHOR_DY + 8, 16, 16);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(ring.0));

        let badge = match state.phase {
            Phase::Waiting => None,
            Phase::Holding => Some(("HOLDING (100 ms)", 0x0000_C4FF)),
            Phase::Released => Some(("RELEASED -> POP!", 0x0081_B910)),
        };
        if let Some((text, col)) = badge {
            draw_text(hdc, badge_font, text, &mut RECT { left: W - 210, top: 90, right: W - 32, bottom: 118 }, col, true);
        }

        if !state.status_line.is_empty() {
            draw_text(hdc, sub_font, &state.status_line, &mut RECT { left: 20, top: H - 34, right: W - 20, bottom: H - 8 }, 0x00D4_D4D8, false);
        }

        let _ = DeleteObject(badge_font);
        let _ = DeleteObject(row_font);
        let _ = DeleteObject(sub_font);
        let _ = DeleteObject(title_font);
    }
}

fn paint_backdrop(hwnd: HWND, state: &WindowState) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if !hdc.is_invalid() {
            paint_backdrop_dc(hdc, state);
            let _ = EndPaint(hwnd, &ps);
        }
    }
}

fn render_backdrop_bgra(state: &WindowState) -> Vec<u8> {
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
    found: Vec<HWND>,
}

unsafe extern "system" fn collect_menu_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let target = &mut *(lparam.0 as *mut EnumTarget);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != target.pid {
            return TRUE;
        }
        let mut cls = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut cls);
        if len > 0 && String::from_utf16_lossy(&cls[..len as usize]) == "#32768" {
            target.found.push(hwnd);
        }
        TRUE
    }));
    TRUE
}

fn find_own_menu_window(pid: u32) -> Option<HWND> {
    let mut target = EnumTarget {
        pid,
        found: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_menu_window),
            LPARAM(&mut target as *mut EnumTarget as isize),
        );
    }
    target.found.into_iter().next()
}

fn capture_popup_window(hwnd: HWND) -> Option<(i32, i32, i32, i32, Vec<u8>)> {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return None;
    }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
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
            let _ = DeleteDC(mem_dc_fallback(hdc));
            return None;
        };
        let mut out = None;
        if !bits.is_null() {
            let old = SelectObject(hdc, HGDIOBJ(bmp.0));
            // Same-thread `WM_PRINT` renders full menu item hierarchy without partial cross-thread abort.
            let _ = SendMessageW(
                hwnd,
                WM_PRINT,
                WPARAM(hdc.0 as usize),
                LPARAM(PRF_CHECKVISIBLE | PRF_NONCLIENT | PRF_CLIENT | PRF_ERASEBKGND | PRF_CHILDREN),
            );
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
            if distinct.len() <= 4 {
                let _ = PrintWindow(hwnd, hdc, 0);
                let bytes = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
                let u32_pixels = std::slice::from_raw_parts(bits.cast::<u32>(), (w * h) as usize);
                let mut d2 = HashSet::new();
                for &px in u32_pixels.iter().step_by(3) {
                    d2.insert(px);
                    if d2.len() > 4 {
                        out = Some((rc.left, rc.top, w, h, bytes));
                        break;
                    }
                }
            } else {
                out = Some((rc.left, rc.top, w, h, bytes));
            }
            SelectObject(hdc, old);
        }
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        out
    }
}

unsafe fn mem_dc_fallback(hdc: HDC) -> HDC {
    hdc
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
    query_ms: f64,
    items: usize,
    first_menu_ms: Option<f64>,
    hold_ms: u32,
    samples: Vec<RecordedSample>,
}

fn pump_for(ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut msg = MSG::default();
    while Instant::now() < deadline {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
        if !state.is_null() {
            let menu = (*state).menu;
            if !menu.is_null() {
                if let Some(result) = (*menu).handle_menu_msg(message, wparam, lparam) {
                    return Some(result);
                }
            }
            if message == WM_PAINT {
                paint_backdrop(hwnd, &*state);
                return Some(LRESULT(0));
            }
            if message == WM_TIMER && wparam.0 == TIMER_CAPTURE {
                if let Some(ref mut col) = (*state).collector {
                    if let Some(hm) = find_own_menu_window(col.pid) {
                        if let Some((left, top, w, h, bytes)) = capture_popup_window(hm) {
                            let now_us = col.t_origin.elapsed().as_micros() as u64;
                            let rel_ms = (now_us as f64 - col.release_us as f64) / 1000.0;
                            let file_idx = match col.frames.last() {
                                Some(prev) if *prev == bytes => col.frames.len() - 1,
                                _ => {
                                    col.frames.push(bytes);
                                    col.frames.len() - 1
                                }
                            };
                            col.samples.push((
                                rel_ms,
                                left - col.origin.0,
                                top - col.origin.1,
                                w,
                                h,
                                file_idx,
                            ));
                        }
                    }
                }
                return Some(LRESULT(0));
            }
        }
        if message == WM_TIMER && wparam.0 == TIMER_END_MENU {
            let _ = KillTimer(hwnd, TIMER_END_MENU);
            let _ = KillTimer(hwnd, TIMER_CAPTURE);
            let _ = EndMenu();
            return Some(LRESULT(0));
        }
        None
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    if let Some(dir) = &args.record_dir {
        fs::create_dir_all(dir)?;
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    enable_dark_mode_menus();
    let _ole = OleGuard::initialize()?;
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = w!("RustWindowsMenuDemoV02");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE::from(instance),
        lpszClassName: class,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    let owner = Window(unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("RustWindowsMenu v0.2 Demo"),
            WS_POPUP,
            args.x,
            args.y,
            W,
            H,
            None,
            None,
            instance,
            None,
        )?
    });
    let file_name = args
        .file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.file.display().to_string());
    let mut state = Box::new(WindowState {
        mode: args.mode,
        file_name,
        phase: Phase::Waiting,
        status_line: "Waiting for right-click...".into(),
        menu: std::ptr::null(),
        collector: None,
    });
    unsafe {
        SetWindowLongPtrW(owner.0, GWLP_USERDATA, (&mut *state as *mut WindowState) as isize);
        let _ = ShowWindow(owner.0, SW_SHOW);
        let _ = UpdateWindow(owner.0);
    }

    if args.warm {
        let _ = NativeMenu::build_from_paths(
            owner.0,
            std::slice::from_ref(&args.file),
            args.background,
            false,
        )?;
    }

    if let Some(ref dir) = args.record_dir {
        fs::write(dir.join("idle_bg.bgra"), render_backdrop_bgra(&state))?;
    }

    let t_origin = Instant::now();
    pump_for(200);

    let press_origin_us = t_origin.elapsed().as_micros() as u64;
    let scheduled_release_us = press_origin_us + u64::from(args.button_hold_ms) * 1000;

    state.phase = Phase::Holding;
    state.status_line = match args.mode {
        DemoMode::Windows => "Right-button pressed (holding 100 ms)...".into(),
        DemoMode::Rwm => "Button pressed -> 15 ms arm -> prebuilding menu during hold...".into(),
    };
    unsafe {
        let _ = InvalidateRect(owner.0, None, false);
        let _ = UpdateWindow(owner.0);
        let _ = SetForegroundWindow(owner.0);
    }
    if let Some(ref dir) = args.record_dir {
        fs::write(dir.join("holding_bg.bgra"), render_backdrop_bgra(&state))?;
    }

    let anchor = (args.x + ANCHOR_DX, args.y + ANCHOR_DY);
    let press_start = Instant::now();
    let hold = Duration::from_millis(u64::from(args.button_hold_ms));
    let (actual_release_us, menu) = match args.mode {
        DemoMode::Rwm => {
            pump_for(15);
            let menu = NativeMenu::build_from_paths(
                owner.0,
                std::slice::from_ref(&args.file),
                args.background,
                false,
            )?;
            let elapsed = press_start.elapsed();
            if elapsed < hold {
                pump_for((hold - elapsed).as_millis() as u64);
            }
            (scheduled_release_us, menu)
        }
        DemoMode::Windows => {
            pump_for(u64::from(args.button_hold_ms));
            let rel = t_origin.elapsed().as_micros() as u64;
            state.phase = Phase::Released;
            state.status_line = "Released -> running QueryContextMenu synchronously...".into();
            unsafe {
                let _ = InvalidateRect(owner.0, None, false);
                let _ = UpdateWindow(owner.0);
            }
            let menu = NativeMenu::build_from_paths(
                owner.0,
                std::slice::from_ref(&args.file),
                args.background,
                false,
            )?;
            (rel, menu)
        }
    };

    state.phase = Phase::Released;
    state.status_line = match args.mode {
        DemoMode::Windows => "Menu ready after synchronous block -> fading in...".into(),
        DemoMode::Rwm => "Released -> instant TrackPopupMenuEx(TPM_NOANIMATION)!".into(),
    };
    unsafe {
        let _ = InvalidateRect(owner.0, None, false);
        let _ = UpdateWindow(owner.0);
    }
    if let Some(ref dir) = args.record_dir {
        fs::write(dir.join("clicked_bg.bgra"), render_backdrop_bgra(&state))?;
        state.collector = Some(CaptureCollector {
            pid: unsafe { GetCurrentProcessId() },
            origin: (args.x, args.y),
            t_origin,
            release_us: actual_release_us,
            frames: Vec::new(),
            samples: Vec::new(),
        });
    }

    let query_ms = menu.build_us() as f64 / 1000.0;
    let items = menu.root_item_count();
    let has_collector = state.collector.is_some();
    {
        let _binding = MenuBindingGuard::bind(&mut state, &menu);
        let hwnd_val = owner.0 .0 as usize;
        let close_ms = u64::from(args.hold_ms);
        unsafe {
            if has_collector {
                let _ = SetTimer(owner.0, TIMER_CAPTURE, 16, None);
            }
            let _ = SetTimer(owner.0, TIMER_END_MENU, args.hold_ms, None);
        }
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(close_ms + 120));
            let _ = unsafe { PostMessageW(HWND(hwnd_val as *mut _), WM_CANCELMODE, WPARAM(0), LPARAM(0)) };
        });
        let no_animation = args.mode == DemoMode::Rwm;
        let _ = menu.track_and_invoke(owner.0, anchor, no_animation)?;
        let _ = unsafe { PostMessageW(owner.0, WM_NULL, WPARAM(0), LPARAM(0)) };
    }

    let recorded = state.collector.take();
    let first_menu_ms = recorded
        .as_ref()
        .and_then(|r| r.samples.first().map(|s| s.0));

    println!(
        "RESULT mode={} query_ms={:.3} items={} first_menu_ms={:.3}",
        match args.mode {
            DemoMode::Windows => "windows",
            DemoMode::Rwm => "rwm",
        },
        query_ms,
        items,
        first_menu_ms.unwrap_or(query_ms)
    );

    if let (Some(ref dir), Some(rec)) = (&args.record_dir, recorded) {
        let mut frame_names = Vec::with_capacity(rec.frames.len());
        for (i, bytes) in rec.frames.iter().enumerate() {
            let name = format!("frame_{i:04}.bgra");
            fs::write(dir.join(&name), bytes)?;
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
                DemoMode::Windows => "windows".into(),
                DemoMode::Rwm => "rwm".into(),
            },
            w: W,
            h: H,
            query_ms,
            items,
            first_menu_ms,
            hold_ms: args.button_hold_ms,
            samples,
        };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    }

    let _ = unsafe { SendMessageTimeoutW(owner.0, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 50, None) };
    Ok(())
}
