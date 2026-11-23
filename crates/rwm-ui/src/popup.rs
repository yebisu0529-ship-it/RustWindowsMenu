use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::size_of;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Once;
use std::time::{Duration, Instant};

use rwm_core::config::RwmConfig;
use rwm_core::engine::FastMenuEngine;
use rwm_core::model::{
    parse_mnemonic_label, CoreVerb, EntryAction, EntrySource, HandlerPolicy, HydrationEvent,
    MenuEntry, MenuItem, MenuSnapshot,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUNDSMALL,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MonitorFromPoint, UpdateWindow, HDC, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VIRTUAL_KEY, VK_DOWN, VK_END, VK_ESCAPE, VK_F10,
    VK_HOME, VK_LEFT, VK_MENU, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, UnregisterClassW, CREATESTRUCTW, CS_DROPSHADOW, GWLP_USERDATA, HWND_TOPMOST,
    IDC_ARROW, MA_NOACTIVATE, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SW_HIDE, SW_SHOW,
    SW_SHOWNOACTIVATE, WA_INACTIVE, WHEEL_DELTA, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CANCELMODE,
    WM_CHAR, WM_CLOSE, WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::renderer::{
    clamp_popup_origin, clamp_submenu_origin, compute_layout, reflow_origin_anchored,
    submenu_overlap, HitTarget, MenuFrame, MenuLayout, MenuRenderer, PixelRect,
};
use crate::theme::{compute_metrics, resolve_theme, MenuMetrics, ThemeColors};
use crate::WM_APP_HYDRATE;

/// Lives in `Win32_UI_Controls`, a large binding module this crate does not otherwise need.
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_PRINTCLIENT: u32 = 0x0318;

const CLASS_NAME: PCWSTR = w!("RustWindowsMenuPopup");
const DEFAULT_FONT_FAMILY: &str = "Segoe UI";

const TIMER_HOVER: usize = 1;
const TIMER_SCROLL: usize = 2;
const TIMER_SMOKE: usize = 3;
const TIMER_HYDRATION_POLL: usize = 4;
/// Safety net for hydration notifications that never arrive, such as a stream handed over from
/// another window.
const HYDRATION_POLL_MS: u32 = 100;
const SCROLL_REPEAT_MS: u32 = 40;
const WHEEL_ROWS: f32 = 3.0;
/// Prevents accidental activation when async hydration or scroll moves a different row under a
/// stationary pointer right before mouse-up.
const SHIFT_CLICK_GUARD: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy)]
pub struct PopupOptions {
    pub anchor: (i32, i32),
    pub owner_hwnd: HWND,
    pub show_timings: bool,
    pub submenu_hover_delay_ms: u32,
    pub smoke_test_ms: Option<u32>,
}

#[derive(Debug)]
pub struct PopupResult {
    pub chosen_action: Option<EntryAction>,
    pub rendered_with_d2d: bool,
    pub hydrated_groups: usize,
    /// Items in the root menu when it closed, separators excluded.
    pub final_item_count: usize,
}

/// Shows the popup at `options.anchor` and runs a modal loop until an item is chosen or the menu
/// is dismissed. COM shell extensions hydrate into the open menu as `WM_APP_HYDRATE` arrives.
pub fn show_popup_menu(
    engine: &FastMenuEngine,
    snapshot: MenuSnapshot,
    options: PopupOptions,
) -> PopupResult {
    let config = engine.config();
    let hydrate = config.behavior.enable_async_com_hydration && snapshot.pending_handlers > 0;
    let session_id = snapshot.session_id;
    let target = snapshot.target.clone();
    run_popup(config, snapshot, options, move |root| {
        hydrate.then(|| engine.start_hydration(session_id, target, root.0 as isize, WM_APP_HYDRATE))
    })
}

/// `start_hydration` runs once the root window exists and before it is shown. It either starts a
/// stream that notifies the root with `WM_APP_HYDRATE` or hands over a receiver that already runs.
fn run_popup(
    config: &RwmConfig,
    snapshot: MenuSnapshot,
    options: PopupOptions,
    start_hydration: impl FnOnce(HWND) -> Option<Receiver<HydrationEvent>>,
) -> PopupResult {
    let instance = register_popup_class();
    let (work_area, dpi) = monitor_at(options.anchor);
    let metrics = compute_metrics(config.appearance.density, dpi);
    let theme = resolve_theme(config.appearance.theme);
    let font_family = config
        .appearance
        .font_family
        .as_deref()
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .unwrap_or(DEFAULT_FONT_FAMILY)
        .to_owned();

    let cell = SessionCell {
        state: RefCell::new(Session {
            instance,
            theme,
            metrics,
            font_family,
            work_area,
            anchor: options.anchor,
            round_corners: config.appearance.corner_radius > 0,
            outline_radius: 0.0,
            hover_delay_ms: options.submenu_hover_delay_ms,
            show_timings: options.show_timings,
            show_loading_footer: config.behavior.show_loading_footer,
            japanese: ui_is_japanese(),
            submenu_title: config.handlers.submenu_title.clone(),
            session_id: snapshot.session_id,
            stage0_us: snapshot.stage0_us,
            hydration: None,
            pending: 0,
            hydrated_groups: 0,
            levels: Vec::new(),
            windows: Vec::new(),
            last_cursor: None,
            mouse_owns_selection: false,
            shift_guard_until: None,
            #[cfg(test)]
            suppress_show: false,
            button_armed: false,
            outcome: None,
        }),
        aborted: Cell::new(false),
    };
    let _teardown = Teardown(&cell);

    let Some(root) = create_popup_window(instance, options.owner_hwnd, &cell, false) else {
        tracing::error!("failed to create the popup window");
        return PopupResult {
            chosen_action: None,
            rendered_with_d2d: false,
            hydrated_groups: 0,
            final_item_count: count_items(&snapshot.entries),
        };
    };

    cell.state.borrow_mut().windows.push(root);
    let hydration = start_hydration(root);
    let hydrating = hydration.is_some();
    let mut session = cell.state.borrow_mut();
    if hydrating {
        session.pending = snapshot.pending_handlers;
    }
    session.hydration = hydration;
    if apply_window_chrome(root, &session.theme, session.round_corners) && session.round_corners {
        session.outline_radius = session.metrics.corner_radius;
    }
    let renderer = MenuRenderer::new(root, &session.theme, &session.metrics, &session.font_family);
    let entries = snapshot.entries;
    let footer_visible = session.footer_visible();
    let layout = compute_layout(
        &renderer.measure_rows(&entries),
        &session.metrics,
        footer_visible,
        work_area.height() as f32,
    );
    let size = layout.pixel_size();
    let rect =
        PixelRect::from_origin_size(clamp_popup_origin(options.anchor, size, work_area), size);
    session
        .levels
        .push(Level::new(root, entries, None, layout, renderer, rect));
    session.place(0);
    session.last_cursor = cursor_position();
    drop(session);

    unsafe {
        let _ = ShowWindow(root, SW_SHOW);
        let _ = SetForegroundWindow(root);
        let _ = UpdateWindow(root);
        if let Some(ms) = options.smoke_test_ms {
            SetTimer(root, TIMER_SMOKE, ms.max(1), None);
        }
        if hydrating {
            SetTimer(root, TIMER_HYDRATION_POLL, HYDRATION_POLL_MS, None);
        }
    }

    let quit_code = run_modal_loop(&cell);
    let result = cell.state.borrow_mut().take_result();
    cell.destroy_windows();
    if let Some(code) = quit_code {
        unsafe { PostQuitMessage(code) };
    }
    result
}

fn run_modal_loop(cell: &SessionCell) -> Option<i32> {
    let mut message = MSG::default();
    while !cell.is_finished() {
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) };
        match status.0 {
            0 => return Some(message.wParam.0 as i32),
            -1 => break,
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
    None
}

enum Outcome {
    Dismissed,
    Chosen(EntryAction),
}

/// One open popup window: the root menu or a cascading submenu.
struct Level {
    hwnd: HWND,
    entries: Vec<MenuEntry>,
    /// Entry in the previous level whose submenu this level shows.
    owner: Option<usize>,
    layout: MenuLayout,
    renderer: MenuRenderer,
    rect: PixelRect,
    selected: Option<usize>,
    scroll: f32,
    /// Signed scroll distance per `TIMER_SCROLL` tick while a scroll button is hovered.
    scroll_step: f32,
    tracking_leave: bool,
}

impl Level {
    fn new(
        hwnd: HWND,
        entries: Vec<MenuEntry>,
        owner: Option<usize>,
        layout: MenuLayout,
        renderer: MenuRenderer,
        rect: PixelRect,
    ) -> Self {
        Self {
            hwnd,
            entries,
            owner,
            layout,
            renderer,
            rect,
            selected: None,
            scroll: 0.0,
            scroll_step: 0.0,
            tracking_leave: false,
        }
    }
}

struct Session {
    instance: HINSTANCE,
    theme: ThemeColors,
    metrics: MenuMetrics,
    font_family: String,
    work_area: PixelRect,
    anchor: (i32, i32),
    round_corners: bool,
    outline_radius: f32,
    hover_delay_ms: u32,
    show_timings: bool,
    show_loading_footer: bool,
    japanese: bool,
    submenu_title: String,
    session_id: u64,
    stage0_us: u64,
    hydration: Option<Receiver<HydrationEvent>>,
    pending: usize,
    hydrated_groups: usize,
    /// Root first, then each open cascade level.
    levels: Vec<Level>,
    /// Every live window created for this session, destroyed in reverse order at teardown.
    windows: Vec<HWND>,
    last_cursor: Option<(i32, i32)>,
    mouse_owns_selection: bool,
    shift_guard_until: Option<Instant>,
    #[cfg(test)]
    suppress_show: bool,
    button_armed: bool,
    outcome: Option<Outcome>,
}

/// Shared by every popup window of one session through `GWLP_USERDATA`. Window procedures borrow
/// `state` per message; a message that re-enters while it is borrowed falls back to `DefWindowProcW`.
struct SessionCell {
    state: RefCell<Session>,
    aborted: Cell<bool>,
}

impl SessionCell {
    fn is_finished(&self) -> bool {
        self.aborted.get()
            || self
                .state
                .try_borrow()
                .is_ok_and(|session| session.outcome.is_some())
    }

    fn abort(&self, hwnd: HWND) {
        self.aborted.set(true);
        unsafe {
            let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }

    fn destroy_windows(&self) {
        let Ok(mut session) = self.state.try_borrow_mut() else {
            return;
        };
        let windows = std::mem::take(&mut session.windows);
        let levels = std::mem::take(&mut session.levels);
        drop(session);
        for hwnd in windows.into_iter().rev() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        drop(levels);
    }
}

/// Windows store a pointer to the `SessionCell` on the stack, so they must be gone before it is.
struct Teardown<'a>(&'a SessionCell);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        self.0.destroy_windows();
    }
}

impl Session {
    fn handle(
        &mut self,
        cell: &SessionCell,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let level = self.levels.iter().position(|level| level.hwnd == hwnd)?;
        match message {
            WM_PAINT => self.paint(level),
            WM_PRINTCLIENT => self.paint_to_dc(level, HDC(wparam.0 as *mut c_void)),
            WM_MOUSEMOVE => self.on_mouse_move(level, lparam),
            WM_MOUSELEAVE => self.on_mouse_leave(level),
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let (x, y) = client_point(lparam);
                let rect = self.levels[level].rect;
                self.last_cursor = Some((rect.left + x, rect.top + y));
                self.mouse_owns_selection = true;
                self.button_armed = true;
            }
            WM_LBUTTONUP | WM_RBUTTONUP => self.on_button_up(cell, level, lparam),
            WM_MOUSEWHEEL => self.on_wheel(level, wparam, lparam),
            WM_KEYDOWN => {
                if !self.on_key_down(cell, VIRTUAL_KEY(wparam.0 as u16)) {
                    return None;
                }
            }
            WM_SYSKEYDOWN => {
                let key = VIRTUAL_KEY(wparam.0 as u16);
                if key != VK_MENU && key != VK_F10 {
                    return None;
                }
                self.finish(Outcome::Dismissed);
            }
            WM_CHAR => self.on_char(cell, wparam),
            WM_TIMER => {
                if !self.on_timer(cell, level, wparam.0) {
                    return None;
                }
            }
            WM_ACTIVATE => {
                let activated = HWND(lparam.0 as *mut c_void);
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE && !self.owns(activated) {
                    self.finish(Outcome::Dismissed);
                }
                return None;
            }
            WM_ACTIVATEAPP => {
                if wparam.0 == 0 {
                    self.finish(Outcome::Dismissed);
                }
                return None;
            }
            WM_CANCELMODE | WM_DISPLAYCHANGE => {
                self.finish(Outcome::Dismissed);
                return None;
            }
            WM_CLOSE => self.finish(Outcome::Dismissed),
            WM_APP_HYDRATE => self.drain_hydration(),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn owns(&self, hwnd: HWND) -> bool {
        self.levels.iter().any(|level| level.hwnd == hwnd)
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(outcome);
        // Hide now rather than at teardown: a message this loop dispatches next can open another
        // context menu with its own modal loop before this one gets to destroy its windows.
        for level in self.levels.iter().rev() {
            unsafe {
                let _ = ShowWindow(level.hwnd, SW_HIDE);
            }
        }
        // The outcome may be decided inside a message sent during `GetMessageW`; wake the loop.
        if let Some(root) = self.levels.first() {
            unsafe {
                let _ = PostMessageW(root.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn take_result(&mut self) -> PopupResult {
        let root = self.levels.first();
        PopupResult {
            chosen_action: match self.outcome.take() {
                Some(Outcome::Chosen(action)) => Some(action),
                Some(Outcome::Dismissed) | None => None,
            },
            rendered_with_d2d: root.is_some_and(|level| level.renderer.is_direct2d()),
            hydrated_groups: self.hydrated_groups,
            final_item_count: root.map_or(0, |level| count_items(&level.entries)),
        }
    }

    fn footer_texts(&self) -> (Option<&'static str>, Option<String>) {
        let status = (self.show_loading_footer && self.pending > 0).then_some(if self.japanese {
            "読み込み中..."
        } else {
            "Loading extensions..."
        });
        let badge = self
            .show_timings
            .then(|| format!("Stage 0 {:.2} ms", self.stage0_us as f64 / 1000.0));
        (status, badge)
    }

    fn footer_visible(&self) -> bool {
        let (status, badge) = self.footer_texts();
        status.is_some() || badge.is_some()
    }

    fn paint(&mut self, level: usize) {
        let (status, badge) = if level == 0 {
            self.footer_texts()
        } else {
            (None, None)
        };
        let outline_radius = self.outline_radius;
        let current = &mut self.levels[level];
        let frame = MenuFrame {
            entries: &current.entries,
            layout: &current.layout,
            highlighted: current.selected,
            scroll: current.scroll,
            outline_radius,
            footer_status: status,
            footer_badge: badge.as_deref(),
        };
        current.renderer.paint(current.hwnd, &frame);
    }

    fn paint_to_dc(&mut self, level: usize, hdc: HDC) {
        let (status, badge) = if level == 0 {
            self.footer_texts()
        } else {
            (None, None)
        };
        let outline_radius = self.outline_radius;
        let current = &mut self.levels[level];
        let frame = MenuFrame {
            entries: &current.entries,
            layout: &current.layout,
            highlighted: current.selected,
            scroll: current.scroll,
            outline_radius,
            footer_status: status,
            footer_badge: badge.as_deref(),
        };
        current.renderer.paint_to_dc(hdc, &frame);
    }

    fn invalidate(&self, level: usize) {
        unsafe {
            let _ = InvalidateRect(self.levels[level].hwnd, None, false);
        }
    }

    fn select(&mut self, level: usize, selected: Option<usize>) {
        if self.levels[level].selected != selected {
            self.levels[level].selected = selected;
            self.invalidate(level);
        }
    }

    /// Owner entry of the submenu open beside `level`, if any.
    fn open_owner(&self, level: usize) -> Option<usize> {
        self.levels.get(level + 1).and_then(|child| child.owner)
    }

    /// Keyboard input applies to the deepest level that has a selection.
    fn focus_level(&self) -> usize {
        self.levels
            .iter()
            .rposition(|level| level.selected.is_some())
            .unwrap_or(0)
    }

    fn opens_submenu(&self, level: usize, index: usize) -> bool {
        submenu_children(&self.levels[level].entries, index).is_some()
    }

    fn kill_timer(&self, level: usize, timer: usize) {
        unsafe {
            let _ = KillTimer(self.levels[level].hwnd, timer);
        }
    }

    fn on_mouse_move(&mut self, level: usize, lparam: LPARAM) {
        let (x, y) = client_point(lparam);
        let current = &mut self.levels[level];
        if !current.tracking_leave {
            track_mouse_leave(current.hwnd);
            current.tracking_leave = true;
        }
        let screen = (current.rect.left + x, current.rect.top + y);
        // Windows also sends WM_MOUSEMOVE when a window appears under a still cursor; only a real
        // move may take the selection away from the keyboard.
        if self.last_cursor == Some(screen) {
            return;
        }
        self.last_cursor = Some(screen);
        self.mouse_owns_selection = true;
        self.shift_guard_until = None;

        let hit = current.layout.hit_test(x as f32, y as f32, current.scroll);
        self.set_scroll_hover(level, hit);
        match hit {
            HitTarget::Row(index) if is_item(&self.levels[level].entries[index]) => {
                self.select(level, Some(index));
                let wanted = self.opens_submenu(level, index).then_some(index);
                if wanted == self.open_owner(level) {
                    self.kill_timer(level, TIMER_HOVER);
                } else {
                    unsafe {
                        SetTimer(
                            self.levels[level].hwnd,
                            TIMER_HOVER,
                            self.hover_delay_ms.max(1),
                            None,
                        );
                    }
                }
            }
            _ => self.rest_selection(level),
        }
    }

    fn on_mouse_leave(&mut self, level: usize) {
        self.levels[level].tracking_leave = false;
        if self.last_cursor.is_some_and(|cursor| {
            self.hit_at_screen(level, cursor).is_some()
                && !self
                    .levels
                    .iter()
                    .enumerate()
                    .any(|(depth, _)| depth != level && self.hit_at_screen(depth, cursor).is_some())
        }) {
            self.last_cursor = None;
        }
        self.set_scroll_hover(level, HitTarget::Nothing);
        self.rest_selection(level);
    }

    /// Without the pointer on an item, a level highlights the owner of its open submenu, if any.
    fn rest_selection(&mut self, level: usize) {
        self.kill_timer(level, TIMER_HOVER);
        self.select(level, self.open_owner(level));
    }

    fn hit_at_screen(&self, level: usize, screen: (i32, i32)) -> Option<HitTarget> {
        let current = self.levels.get(level)?;
        let r = current.rect;
        if screen.0 < r.left || screen.0 >= r.right || screen.1 < r.top || screen.1 >= r.bottom {
            return None;
        }
        Some(current.layout.hit_test(
            (screen.0 - r.left) as f32,
            (screen.1 - r.top) as f32,
            current.scroll,
        ))
    }

    fn cursor_row_at(&self, level: usize, screen: (i32, i32)) -> Option<usize> {
        match self.hit_at_screen(level, screen) {
            Some(HitTarget::Row(index)) => Some(index),
            _ => None,
        }
    }

    /// Re-hit-tests `last_cursor` across `from_level..` after a layout reflow or scroll, updating
    /// mouse-owned hover selection and arming a short click guard if the row under the cursor moved.
    fn sync_cursor_after_shift(&mut self, from_level: usize, expected_rows: &[Option<usize>]) {
        let Some(cursor) = self.last_cursor else {
            return;
        };
        let hit = (from_level..self.levels.len())
            .rev()
            .find_map(|depth| self.hit_at_screen(depth, cursor).map(|target| (depth, target)));
        let hit_depth = hit.map(|(depth, _)| depth);

        for depth in from_level..self.levels.len() {
            let expected = expected_rows.get(depth - from_level).copied().flatten();
            let target = (Some(depth) == hit_depth).then(|| hit.unwrap().1);
            let actual = match target {
                Some(HitTarget::Row(index)) => Some(index),
                _ => None,
            };
            if actual != expected {
                self.shift_guard_until = Some(Instant::now() + SHIFT_CLICK_GUARD);
            }
            if !self.mouse_owns_selection {
                continue;
            }
            match target {
                Some(hit_target) => {
                    self.set_scroll_hover(depth, hit_target);
                    match hit_target {
                        HitTarget::Row(index) if is_item(&self.levels[depth].entries[index]) => {
                            self.select(depth, Some(index));
                            if actual != expected {
                                self.kill_timer(depth, TIMER_HOVER);
                            }
                        }
                        _ => self.rest_selection(depth),
                    }
                }
                None => {
                    self.set_scroll_hover(depth, HitTarget::Nothing);
                    self.rest_selection(depth);
                }
            }
        }
    }

    fn set_scroll_hover(&mut self, level: usize, hit: HitTarget) {
        let direction = match hit {
            HitTarget::ScrollUp => -1.0,
            HitTarget::ScrollDown => 1.0,
            _ => 0.0,
        };
        let step = direction * (self.metrics.row_height / 2.0).round();
        let current = &mut self.levels[level];
        if current.scroll_step == step {
            return;
        }
        current.scroll_step = step;
        unsafe {
            if step == 0.0 {
                let _ = KillTimer(current.hwnd, TIMER_SCROLL);
            } else {
                SetTimer(current.hwnd, TIMER_SCROLL, SCROLL_REPEAT_MS, None);
            }
        }
    }

    fn scroll_by(&mut self, level: usize, delta: f32) -> bool {
        let before_row = self
            .last_cursor
            .and_then(|cursor| self.cursor_row_at(level, cursor));
        let current = &mut self.levels[level];
        let scroll = current.layout.clamp_scroll(current.scroll + delta);
        if scroll == current.scroll {
            return false;
        }
        current.scroll = scroll;
        // Open submenus are anchored to rows that just moved.
        self.close_levels_above(level);
        self.invalidate(level);
        self.sync_cursor_after_shift(level, &[before_row]);
        true
    }

    fn on_button_up(&mut self, cell: &SessionCell, level: usize, lparam: LPARAM) {
        if !std::mem::take(&mut self.button_armed) {
            return;
        }
        if self
            .shift_guard_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let (x, y) = client_point(lparam);
        let current = &self.levels[level];
        match current.layout.hit_test(x as f32, y as f32, current.scroll) {
            HitTarget::Row(index) => self.activate(cell, level, index, false),
            HitTarget::ScrollUp => {
                self.scroll_by(level, -self.metrics.row_height);
            }
            HitTarget::ScrollDown => {
                self.scroll_by(level, self.metrics.row_height);
            }
            HitTarget::Footer | HitTarget::Nothing => {}
        }
    }

    fn on_wheel(&mut self, level: usize, wparam: WPARAM, lparam: LPARAM) {
        let delta = f32::from((wparam.0 >> 16) as u16 as i16);
        let (x, y) = client_point(lparam);
        let target = self
            .levels
            .iter()
            .rposition(|candidate| {
                let r = candidate.rect;
                x >= r.left && x < r.right && y >= r.top && y < r.bottom
            })
            .unwrap_or(level);
        let rows = -delta / WHEEL_DELTA as f32 * WHEEL_ROWS;
        self.scroll_by(target, rows * self.metrics.row_height);
    }

    fn on_key_down(&mut self, cell: &SessionCell, key: VIRTUAL_KEY) -> bool {
        let level = self.focus_level();
        let selected = self.levels[level].selected;
        match key {
            VK_DOWN | VK_UP => {
                let next = step_selection(&self.levels[level].entries, selected, key == VK_DOWN);
                self.keyboard_select(level, next);
            }
            VK_HOME | VK_END => {
                let edge = step_selection(&self.levels[level].entries, None, key == VK_HOME);
                self.keyboard_select(level, edge);
            }
            VK_RIGHT => {
                if let Some(index) = selected.filter(|&index| self.opens_submenu(level, index)) {
                    self.activate(cell, level, index, true);
                }
            }
            VK_RETURN => {
                if let Some(index) = selected {
                    self.activate(cell, level, index, true);
                }
            }
            VK_LEFT => {
                if self.levels.len() > 1 {
                    self.close_deepest();
                }
            }
            VK_ESCAPE => {
                if self.levels.len() > 1 {
                    self.close_deepest();
                } else {
                    self.finish(Outcome::Dismissed);
                }
            }
            _ => return false,
        }
        true
    }

    fn on_char(&mut self, cell: &SessionCell, wparam: WPARAM) {
        let Some(key) = char::from_u32(wparam.0 as u32).filter(|key| !key.is_control()) else {
            return;
        };
        let level = self.focus_level();
        let current = &self.levels[level];
        match find_mnemonic(&current.entries, key, current.selected) {
            Some(MnemonicHit::Unique(index)) => {
                self.keyboard_select(level, Some(index));
                self.activate(cell, level, index, true);
            }
            Some(MnemonicHit::Next(index)) => self.keyboard_select(level, Some(index)),
            None => {}
        }
    }

    fn on_timer(&mut self, cell: &SessionCell, level: usize, timer: usize) -> bool {
        match timer {
            TIMER_HOVER => {
                self.kill_timer(level, TIMER_HOVER);
                self.sync_submenu(cell, level);
            }
            TIMER_SCROLL => {
                let step = self.levels[level].scroll_step;
                if step == 0.0 || !self.scroll_by(level, step) {
                    self.set_scroll_hover(level, HitTarget::Nothing);
                }
            }
            TIMER_SMOKE => self.finish(Outcome::Dismissed),
            TIMER_HYDRATION_POLL => self.drain_hydration(),
            _ => return false,
        }
        true
    }

    fn keyboard_select(&mut self, level: usize, selected: Option<usize>) {
        self.mouse_owns_selection = false;
        self.kill_timer(level, TIMER_HOVER);
        if self
            .open_owner(level)
            .is_some_and(|owner| Some(owner) != selected)
        {
            self.close_levels_above(level);
        }
        self.select(level, selected);
        if let Some(index) = selected {
            let current = &mut self.levels[level];
            current.scroll = current.layout.scroll_to_reveal(index, current.scroll);
            self.invalidate(level);
        }
    }

    /// Opens the submenu of the selected item after the hover delay, or closes a stale one.
    fn sync_submenu(&mut self, cell: &SessionCell, level: usize) {
        let wanted = self.levels[level]
            .selected
            .filter(|&index| self.opens_submenu(level, index));
        if wanted == self.open_owner(level) {
            return;
        }
        self.close_levels_above(level);
        if let Some(index) = wanted {
            self.open_submenu(cell, level, index);
        }
    }

    fn activate(&mut self, cell: &SessionCell, level: usize, index: usize, from_keyboard: bool) {
        let command = match self.levels[level].entries.get(index) {
            Some(MenuEntry::Item(item)) if item.enabled => match &item.action {
                EntryAction::Submenu(_) => None,
                action => Some(action.clone()),
            },
            _ => return,
        };
        if let Some(action) = command {
            self.finish(Outcome::Chosen(action));
            return;
        }
        if !self.opens_submenu(level, index) {
            return;
        }
        self.kill_timer(level, TIMER_HOVER);
        if self.open_owner(level) != Some(index) {
            self.close_levels_above(level);
            self.open_submenu(cell, level, index);
        }
        if from_keyboard && self.open_owner(level) == Some(index) {
            let first = step_selection(&self.levels[level + 1].entries, None, true);
            self.keyboard_select(level + 1, first);
        }
    }

    fn close_deepest(&mut self) {
        let parent = self.levels.len() - 2;
        let owner = self.open_owner(parent);
        self.close_levels_above(parent);
        self.select(parent, owner);
    }

    fn close_levels_above(&mut self, level: usize) {
        while self.levels.len() > level + 1 {
            let Some(closed) = self.levels.pop() else {
                break;
            };
            self.windows.retain(|&hwnd| hwnd != closed.hwnd);
            unsafe {
                let _ = DestroyWindow(closed.hwnd);
            }
        }
    }

    fn open_submenu(&mut self, cell: &SessionCell, level: usize, index: usize) {
        let Some(children) = submenu_children(&self.levels[level].entries, index) else {
            return;
        };
        let entries = children.to_vec();
        let Some(hwnd) = create_popup_window(self.instance, self.levels[level].hwnd, cell, true)
        else {
            return;
        };
        self.windows.push(hwnd);
        apply_window_chrome(hwnd, &self.theme, self.round_corners);
        let renderer = MenuRenderer::new(hwnd, &self.theme, &self.metrics, &self.font_family);
        let layout = compute_layout(
            &renderer.measure_rows(&entries),
            &self.metrics,
            false,
            self.work_area.height() as f32,
        );
        let size = layout.pixel_size();
        let rect = PixelRect::from_origin_size(self.submenu_origin(level, index, size), size);
        self.levels.push(Level::new(
            hwnd,
            entries,
            Some(index),
            layout,
            renderer,
            rect,
        ));
        let child = self.levels.len() - 1;
        self.place(child);
        #[cfg(test)]
        let show = !self.suppress_show;
        #[cfg(not(test))]
        let show = true;
        if show {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
        self.select(level, Some(index));
    }

    fn submenu_origin(&self, level: usize, index: usize, size: (i32, i32)) -> (i32, i32) {
        let parent = &self.levels[level];
        let top = parent.rect.top + parent.layout.row_top(index, parent.scroll).round() as i32;
        let height = parent.layout.rows[index].height.round() as i32;
        let owner_row = PixelRect::new(parent.rect.left, top, parent.rect.right, top + height);
        clamp_submenu_origin(
            owner_row,
            size,
            self.work_area,
            submenu_overlap(&self.metrics),
            self.metrics.padding_y as i32,
        )
    }

    /// Moves and sizes a level's window to its `rect` and matches the render target to it.
    fn place(&mut self, level: usize) {
        let current = &mut self.levels[level];
        let rect = current.rect;
        unsafe {
            let _ = SetWindowPos(
                current.hwnd,
                HWND_TOPMOST,
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE,
            );
        }
        current
            .renderer
            .resize(rect.width() as u32, rect.height() as u32);
        self.invalidate(level);
    }

    /// Re-measures a level after its entries or footer changed, keeping it inside the work area.
    /// Deeper levels follow their owner rows, and cursor hover selection stays synced to what sits
    /// under `last_cursor`.
    fn relayout(
        &mut self,
        level: usize,
        pin_row: Option<(usize, f32)>,
        expected_level_row: Option<usize>,
    ) {
        let before_rows: Vec<Option<usize>> = (level..self.levels.len())
            .map(|depth| {
                if depth == level && expected_level_row.is_some() {
                    expected_level_row
                } else {
                    self.last_cursor
                        .and_then(|cursor| self.cursor_row_at(depth, cursor))
                }
            })
            .collect();

        let footer_visible = level == 0 && self.footer_visible();
        let max_height = self.work_area.height() as f32;
        let current = &mut self.levels[level];
        let specs = current.renderer.measure_rows(&current.entries);
        current.layout = compute_layout(&specs, &self.metrics, footer_visible, max_height);
        current.scroll = current.layout.clamp_scroll(current.scroll);
        for depth in level..self.levels.len() {
            let size = self.levels[depth].layout.pixel_size();
            let origin = match self.levels[depth].owner {
                Some(owner) => self.submenu_origin(depth - 1, owner, size),
                None => {
                    let pinned = if depth == level {
                        pin_row.and_then(|(idx, desired_y)| {
                            (idx < self.levels[depth].layout.rows.len()).then(|| {
                                (
                                    desired_y,
                                    self.levels[depth].layout.row_top(idx, self.levels[depth].scroll),
                                )
                            })
                        })
                    } else {
                        None
                    };
                    reflow_origin_anchored(
                        self.levels[depth].rect,
                        self.anchor,
                        size,
                        self.work_area,
                        pinned,
                    )
                }
            };
            self.levels[depth].rect = PixelRect::from_origin_size(origin, size);
            self.place(depth);
        }
        self.sync_cursor_after_shift(level, &before_rows);
    }

    fn drain_hydration(&mut self) {
        let Some(receiver) = self.hydration.take() else {
            return;
        };
        let hit_root_only = self.last_cursor.is_some_and(|cursor| {
            !(1..self.levels.len()).any(|depth| self.hit_at_screen(depth, cursor).is_some())
        });
        let mut pin_row = if hit_root_only {
            self.last_cursor.and_then(|cursor| {
                let idx = self.cursor_row_at(0, cursor)?;
                let top = self.levels[0].rect.top as f32
                    + self.levels[0].layout.row_top(idx, self.levels[0].scroll);
                Some((idx, top))
            })
        } else {
            None
        };

        let mut connected = true;
        let mut changed = false;
        let mut extended: Option<usize> = None;
        loop {
            match receiver.try_recv() {
                Ok(HydrationEvent::GroupReady {
                    session_id,
                    policy,
                    entries,
                    ..
                }) if session_id == self.session_id => {
                    if policy == HandlerPolicy::Disabled {
                        continue;
                    }
                    self.pending = self.pending.saturating_sub(1);
                    changed = true;
                    let root = &mut self.levels[0].entries;
                    let outcome = merge_group(root, entries, policy, &self.submenu_title);
                    if outcome != MergeOutcome::Unchanged {
                        self.hydrated_groups += 1;
                    }
                    match outcome {
                        MergeOutcome::Unchanged => {}
                        MergeOutcome::Inserted { at, len } => {
                            shift_index(&mut self.levels[0].selected, at, len);
                            shift_index(&mut extended, at, len);
                            if let Some((ref mut idx, _)) = pin_row {
                                if *idx >= at {
                                    *idx += len;
                                }
                            }
                            if let Some(child) = self.levels.get_mut(1) {
                                shift_index(&mut child.owner, at, len);
                            }
                        }
                        MergeOutcome::Extended { container } => extended = Some(container),
                    }
                }
                Ok(HydrationEvent::Completed { session_id, .. })
                    if session_id == self.session_id =>
                {
                    connected = false;
                    changed = true;
                    self.pending = 0;
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    connected = false;
                    changed |= self.pending > 0;
                    self.pending = 0;
                    break;
                }
            }
        }
        if connected {
            self.hydration = Some(receiver);
        } else {
            self.kill_timer(0, TIMER_HYDRATION_POLL);
        }
        if !changed {
            return;
        }
        let expected_root_row = pin_row.map(|(idx, _)| idx);
        self.relayout(0, pin_row, expected_root_row);
        if let Some(container) = extended.filter(|&container| self.open_owner(0) == Some(container))
        {
            if let Some(children) = submenu_children(&self.levels[0].entries, container) {
                self.levels[1].entries = children.to_vec();
            }
            self.relayout(1, None, None);
        }
    }
}

fn shift_index(index: &mut Option<usize>, at: usize, len: usize) {
    if let Some(value) = index {
        if *value >= at {
            *value += len;
        }
    }
}

fn count_items(entries: &[MenuEntry]) -> usize {
    entries.iter().filter(|entry| is_item(entry)).count()
}

fn is_item(entry: &MenuEntry) -> bool {
    matches!(entry, MenuEntry::Item(_))
}

fn is_separator(entry: &MenuEntry) -> bool {
    matches!(entry, MenuEntry::Separator)
}

/// Children of an enabled, non-empty submenu item.
fn submenu_children(entries: &[MenuEntry], index: usize) -> Option<&[MenuEntry]> {
    match entries.get(index) {
        Some(MenuEntry::Item(MenuItem {
            enabled: true,
            action: EntryAction::Submenu(children),
            ..
        })) if !children.is_empty() => Some(children),
        _ => None,
    }
}

/// Next item after `current` in the given direction, wrapping and skipping separators.
fn step_selection(entries: &[MenuEntry], current: Option<usize>, forward: bool) -> Option<usize> {
    let count = entries.len();
    if count == 0 {
        return None;
    }
    let start = current.unwrap_or(if forward { count - 1 } else { 0 });
    (1..=count)
        .map(|step| {
            if forward {
                (start + step) % count
            } else {
                (start + count - step) % count
            }
        })
        .find(|&index| is_item(&entries[index]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MnemonicHit {
    /// The only matching item: activate it.
    Unique(usize),
    /// One of several matching items: move the selection to it.
    Next(usize),
}

/// Explicit `&` mnemonics win; without any, the first letter of each label is used, as in Win32 menus.
fn find_mnemonic(entries: &[MenuEntry], key: char, current: Option<usize>) -> Option<MnemonicHit> {
    let key = fold_case(key);
    let items = || {
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                MenuEntry::Item(item) => Some((index, item)),
                MenuEntry::Separator => None,
            })
    };
    let mut matches: Vec<usize> = items()
        .filter(|(_, item)| item.mnemonic.map(fold_case) == Some(key))
        .map(|(index, _)| index)
        .collect();
    if matches.is_empty() {
        matches = items()
            .filter(|(_, item)| item.label.chars().next().map(fold_case) == Some(key))
            .map(|(index, _)| index)
            .collect();
    }
    match matches.as_slice() {
        [] => None,
        [only] => Some(MnemonicHit::Unique(*only)),
        [first, ..] => {
            let next = matches
                .iter()
                .copied()
                .find(|&index| current.is_none_or(|current| index > current));
            Some(MnemonicHit::Next(next.unwrap_or(*first)))
        }
    }
}

fn fold_case(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeOutcome {
    Unchanged,
    /// `len` root entries were inserted at `at`; root indices at or after `at` moved by `len`.
    Inserted {
        at: usize,
        len: usize,
    },
    /// The extension submenu at `container` gained children.
    Extended {
        container: usize,
    },
}

/// One handler's entries without leading, trailing, or repeated separators. `lead` records that
/// the handler asked to be set apart from whatever precedes it.
struct Group {
    items: Vec<MenuEntry>,
    lead: bool,
}

impl Group {
    fn new(entries: Vec<MenuEntry>) -> Option<Self> {
        let lead = entries.first().is_some_and(is_separator);
        let mut items: Vec<MenuEntry> = Vec::with_capacity(entries.len());
        for entry in entries {
            if is_separator(&entry) && items.last().is_none_or(is_separator) {
                continue;
            }
            items.push(entry);
        }
        if items.last().is_some_and(is_separator) {
            items.pop();
        }
        (!items.is_empty()).then_some(Self { items, lead })
    }
}

/// Where hydrated entries go in the root menu. All of them share one extension section before
/// Properties: the first group opens it inside separator boundaries, later groups join it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    NewSection(usize),
    JoinSection(usize),
}

/// Merges one hydrated handler group into the root menu. Inline items join the extension section
/// in arrival order, ahead of the extension submenu; submenu groups collect under one entry titled
/// `submenu_title` at the end of the section. Inside the section a handler is set apart only when
/// its own entries start with a separator. Separators are never leading, trailing, or doubled.
fn merge_group(
    entries: &mut Vec<MenuEntry>,
    group: Vec<MenuEntry>,
    policy: HandlerPolicy,
    submenu_title: &str,
) -> MergeOutcome {
    let Some(group) = Group::new(group) else {
        return MergeOutcome::Unchanged;
    };
    let (title, mnemonic) = parse_mnemonic_label(submenu_title);
    match policy {
        HandlerPolicy::Disabled => MergeOutcome::Unchanged,
        HandlerPolicy::Inline => {
            let placement = section_placement(entries, &title, true);
            insert_group(entries, placement, group)
        }
        HandlerPolicy::Submenu => {
            let existing = entries
                .iter()
                .position(|entry| is_extension_submenu(entry, &title));
            if let Some(container) = existing {
                if let MenuEntry::Item(MenuItem {
                    action: EntryAction::Submenu(children),
                    ..
                }) = &mut entries[container]
                {
                    if group.lead {
                        children.push(MenuEntry::Separator);
                    }
                    children.extend(group.items);
                }
                return MergeOutcome::Extended { container };
            }
            let container = MenuEntry::Item(MenuItem {
                id: 0,
                label: title.clone(),
                mnemonic,
                shortcut_hint: None,
                icon: None,
                enabled: true,
                checked: false,
                is_default: false,
                source: EntrySource::Custom,
                action: EntryAction::Submenu(group.items),
            });
            let container = Group {
                items: vec![container],
                lead: false,
            };
            let placement = section_placement(entries, &title, false);
            insert_group(entries, placement, container)
        }
    }
}

fn section_placement(
    entries: &[MenuEntry],
    submenu_title: &str,
    before_submenu: bool,
) -> Placement {
    let anchor = properties_index(entries).unwrap_or(entries.len());
    let closed = anchor < entries.len() && anchor > 0 && is_separator(&entries[anchor - 1]);
    let end = if closed { anchor - 1 } else { anchor };
    match end.checked_sub(1).map(|last| &entries[last]) {
        Some(last) if before_submenu && is_extension_submenu(last, submenu_title) => {
            Placement::JoinSection(end - 1)
        }
        Some(last) if is_hydrated(last) => Placement::JoinSection(end),
        _ => Placement::NewSection(anchor),
    }
}

fn insert_group(entries: &mut Vec<MenuEntry>, placement: Placement, group: Group) -> MergeOutcome {
    let (at, boundary) = match placement {
        Placement::NewSection(at) => (at, true),
        Placement::JoinSection(at) => (at, false),
    };
    let mut block = Vec::with_capacity(group.items.len() + 2);
    if at > 0 && !is_separator(&entries[at - 1]) && (boundary || group.lead) {
        block.push(MenuEntry::Separator);
    }
    block.extend(group.items);
    if boundary && at < entries.len() && !is_separator(&entries[at]) {
        block.push(MenuEntry::Separator);
    }
    let len = block.len();
    entries.splice(at..at, block);
    MergeOutcome::Inserted { at, len }
}

fn is_hydrated(entry: &MenuEntry) -> bool {
    matches!(
        entry,
        MenuEntry::Item(MenuItem {
            source: EntrySource::ComHandler { .. },
            ..
        })
    )
}

fn is_extension_submenu(entry: &MenuEntry, title: &str) -> bool {
    matches!(entry, MenuEntry::Item(item)
        if item.source == EntrySource::Custom
            && item.label == title
            && matches!(item.action, EntryAction::Submenu(_)))
}

fn properties_index(entries: &[MenuEntry]) -> Option<usize> {
    let core = entries.iter().position(|entry| {
        matches!(
            entry,
            MenuEntry::Item(MenuItem {
                action: EntryAction::CoreVerb(CoreVerb::Properties),
                ..
            })
        )
    });
    core.or_else(|| {
        entries.iter().position(|entry| {
            matches!(entry, MenuEntry::Item(item)
                if item.label == "プロパティ" || item.label.eq_ignore_ascii_case("Properties"))
        })
    })
}

static REGISTER_CLASS: Once = Once::new();
static CLASS_INSTANCE: AtomicUsize = AtomicUsize::new(0);

fn register_popup_class() -> HINSTANCE {
    REGISTER_CLASS.call_once(|| {
        let instance = module_instance();
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(popup_wndproc),
            hInstance: instance,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        unsafe {
            if RegisterClassExW(&class) == 0 {
                // A previously unloaded copy of this DLL can leave the class registered with a
                // window procedure that no longer exists.
                let _ = UnregisterClassW(CLASS_NAME, instance);
                if RegisterClassExW(&class) == 0 {
                    tracing::error!("failed to register the popup window class");
                }
            }
        }
        CLASS_INSTANCE.store(instance.0 as usize, Ordering::Release);
    });
    HINSTANCE(CLASS_INSTANCE.load(Ordering::Acquire) as *mut c_void)
}

/// The module that contains this code, which is the hook DLL when loaded into Explorer.
fn module_instance() -> HINSTANCE {
    let mut module = HMODULE::default();
    let address = PCWSTR(popup_wndproc as *const () as *const u16);
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address,
            &mut module,
        )
    };
    if found.is_err() {
        module = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    }
    module.into()
}

fn create_popup_window(
    instance: HINSTANCE,
    owner: HWND,
    cell: &SessionCell,
    cascade: bool,
) -> Option<HWND> {
    let mut ex_style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
    if cascade {
        // Submenus never take activation, so the root keeps keyboard focus throughout.
        ex_style |= WS_EX_NOACTIVATE;
    }
    unsafe {
        CreateWindowExW(
            ex_style,
            CLASS_NAME,
            w!("RustWindowsMenu"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            owner,
            None,
            instance,
            Some(cell as *const SessionCell as *const c_void),
        )
    }
    .ok()
}

/// Requests Win11 small rounded (or square) corners and theme-matched DWM chrome. Returns whether
/// DWM accepted the corner attribute, which it does not before Windows 11.
fn apply_window_chrome(hwnd: HWND, theme: &ThemeColors, round_corners: bool) -> bool {
    unsafe {
        let corner = if round_corners {
            DWMWCP_ROUNDSMALL
        } else {
            DWMWCP_DONOTROUND
        };
        let rounded = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
        .is_ok();
        if theme.is_dark {
            let dark = BOOL::from(true);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const BOOL).cast(),
                size_of::<BOOL>() as u32,
            );
        }
        // DWM draws its own 1px edge on rounded windows; matching it keeps the outline single.
        let border = COLORREF(theme.border.to_colorref());
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border as *const COLORREF).cast(),
            size_of::<COLORREF>() as u32,
        );
        rounded
    }
}

/// Work area and effective DPI of the monitor nearest `point`.
fn monitor_at(point: (i32, i32)) -> (PixelRect, u32) {
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: point.0,
                y: point.1,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work_area = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            PixelRect::from(info.rcWork)
        } else {
            PixelRect::new(
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        };
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let dpi = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
            dpi_x
        } else {
            GetDpiForSystem()
        };
        (work_area, dpi)
    }
}

fn ui_is_japanese() -> bool {
    const PRIMARY_LANGUAGE_MASK: u16 = 0x03FF;
    const LANG_JAPANESE: u16 = 0x11;
    unsafe { GetUserDefaultUILanguage() & PRIMARY_LANGUAGE_MASK == LANG_JAPANESE }
}

fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }
        .ok()
        .map(|()| (point.x, point.y))
}

/// Signed x and y packed into a mouse message's `LPARAM`.
fn client_point(lparam: LPARAM) -> (i32, i32) {
    (
        i32::from(lparam.0 as i16),
        i32::from((lparam.0 >> 16) as i16),
    )
}

fn track_mouse_leave(hwnd: HWND) {
    let mut request = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe {
        let _ = TrackMouseEvent(&mut request);
    }
}

unsafe fn session_cell<'a>(hwnd: HWND) -> Option<&'a SessionCell> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SessionCell).as_ref()
}

unsafe extern "system" fn popup_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| {
        handle_message(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            tracing::error!(
                message,
                "panic in the popup window procedure; closing the menu"
            );
            if let Some(cell) = session_cell(hwnd) {
                cell.abort(hwnd);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }
}

unsafe fn handle_message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let Some(cell) = session_cell(hwnd) else {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };
    match message {
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        _ => {}
    }
    let handled = match cell.state.try_borrow_mut() {
        Ok(mut session) => session.handle(cell, hwnd, message, wparam, lparam),
        Err(_) if message == WM_PAINT => {
            // Re-entered mid-handler: validate now and paint once the handler returns.
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            let _ = InvalidateRect(hwnd, None, false);
            return result;
        }
        Err(_) => None,
    };
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

#[cfg(test)]
#[path = "popup_smoke.rs"]
mod smoke;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn item(label: &str) -> MenuEntry {
        let (label, mnemonic) = parse_mnemonic_label(label);
        MenuEntry::Item(MenuItem {
            id: 1,
            label,
            mnemonic,
            shortcut_hint: None,
            icon: None,
            enabled: true,
            checked: false,
            is_default: false,
            source: EntrySource::StaticVerb,
            action: EntryAction::StaticCommand {
                verb: "verb".into(),
                command: "cmd.exe".into(),
                working_dir: PathBuf::from(r"C:\"),
            },
        })
    }

    fn properties() -> MenuEntry {
        let MenuEntry::Item(mut entry) = item("プロパティ") else {
            unreachable!()
        };
        entry.source = EntrySource::Core;
        entry.action = EntryAction::CoreVerb(CoreVerb::Properties);
        MenuEntry::Item(entry)
    }

    /// An entry as the COM hydration worker produces it.
    fn hydrated(label: &str) -> MenuEntry {
        let MenuEntry::Item(mut entry) = item(label) else {
            unreachable!()
        };
        entry.source = EntrySource::ComHandler {
            clsid: "{00000000-0000-0000-0000-000000000000}".into(),
            name: "handler".into(),
        };
        entry.action = EntryAction::ComCommand {
            session_id: 1,
            handler_index: 0,
            cmd_offset: 0,
        };
        MenuEntry::Item(entry)
    }

    fn sep() -> MenuEntry {
        MenuEntry::Separator
    }

    /// Compact rendering: separators as `-`, submenus as `label{children}`.
    fn shape(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Separator => "-".to_owned(),
                MenuEntry::Item(item) => match &item.action {
                    EntryAction::Submenu(children) => {
                        format!("{}{{{}}}", item.label, shape(children).join(","))
                    }
                    _ => item.label.clone(),
                },
            })
            .collect()
    }

    const TITLE: &str = "拡張メニュー";

    fn merge(entries: &mut Vec<MenuEntry>, policy: HandlerPolicy, group: &[&str]) -> MergeOutcome {
        let group = group
            .iter()
            .map(|label| {
                if *label == "-" {
                    sep()
                } else {
                    hydrated(label)
                }
            })
            .collect();
        merge_group(entries, group, policy, TITLE)
    }

    #[test]
    fn first_inline_group_opens_a_section_before_properties() {
        let mut entries = vec![item("Open"), item("Copy"), properties()];
        let outcome = merge(&mut entries, HandlerPolicy::Inline, &["A", "B"]);
        assert_eq!(
            shape(&entries),
            ["Open", "Copy", "-", "A", "B", "-", "プロパティ"]
        );
        assert_eq!(outcome, MergeOutcome::Inserted { at: 2, len: 4 });
    }

    #[test]
    fn without_properties_the_section_goes_last_after_a_separator() {
        let mut entries = vec![item("Open"), item("Copy")];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        merge(&mut entries, HandlerPolicy::Inline, &["B"]);
        assert_eq!(shape(&entries), ["Open", "Copy", "-", "A", "B"]);
    }

    #[test]
    fn inline_group_into_an_empty_menu_adds_no_separators() {
        let mut entries = Vec::new();
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        assert_eq!(shape(&entries), ["A"]);
    }

    #[test]
    fn group_separators_are_trimmed_and_collapsed() {
        let mut entries = vec![item("Open"), properties()];
        merge(
            &mut entries,
            HandlerPolicy::Inline,
            &["-", "A", "-", "-", "B", "-"],
        );
        assert_eq!(
            shape(&entries),
            ["Open", "-", "A", "-", "B", "-", "プロパティ"]
        );
    }

    #[test]
    fn existing_separators_are_reused_rather_than_doubled() {
        let mut entries = vec![item("Open"), sep(), properties()];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        assert_eq!(shape(&entries), ["Open", "-", "A", "-", "プロパティ"]);
    }

    #[test]
    fn later_inline_groups_join_the_section_in_arrival_order() {
        let mut entries = vec![item("Open"), properties(), sep(), item("Static")];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        let outcome = merge(&mut entries, HandlerPolicy::Inline, &["B"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "A", "B", "-", "プロパティ", "-", "Static"]
        );
        assert_eq!(outcome, MergeOutcome::Inserted { at: 3, len: 1 });
    }

    #[test]
    fn a_handler_with_a_leading_separator_is_set_apart_inside_the_section() {
        let mut entries = vec![item("Open"), properties()];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        merge(&mut entries, HandlerPolicy::Inline, &["-", "B", "-"]);
        merge(&mut entries, HandlerPolicy::Inline, &["C"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "A", "-", "B", "C", "-", "プロパティ"]
        );
    }

    #[test]
    fn properties_is_recognised_by_label_when_it_is_not_a_core_verb() {
        let mut entries = vec![item("Open"), item("Properties"), item("Tail")];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "A", "-", "Properties", "Tail"]
        );
    }

    #[test]
    fn submenu_groups_collect_under_one_titled_entry_before_properties() {
        let mut entries = vec![item("Open"), properties()];
        let first = merge(&mut entries, HandlerPolicy::Submenu, &["A"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "拡張メニュー{A}", "-", "プロパティ"]
        );
        assert_eq!(first, MergeOutcome::Inserted { at: 1, len: 3 });

        let second = merge(&mut entries, HandlerPolicy::Submenu, &["B", "-", "C"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "拡張メニュー{A,B,-,C}", "-", "プロパティ"]
        );
        assert_eq!(second, MergeOutcome::Extended { container: 2 });

        merge(&mut entries, HandlerPolicy::Submenu, &["-", "D"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "拡張メニュー{A,B,-,C,-,D}", "-", "プロパティ"]
        );
    }

    #[test]
    fn inline_items_stay_ahead_of_the_extension_submenu() {
        let mut entries = vec![item("Open"), properties()];
        merge(&mut entries, HandlerPolicy::Inline, &["A"]);
        merge(&mut entries, HandlerPolicy::Submenu, &["S"]);
        merge(&mut entries, HandlerPolicy::Inline, &["B"]);
        assert_eq!(
            shape(&entries),
            ["Open", "-", "A", "B", "拡張メニュー{S}", "-", "プロパティ"]
        );

        let mut submenu_first = vec![item("Open"), properties()];
        merge(&mut submenu_first, HandlerPolicy::Submenu, &["S"]);
        let outcome = merge(&mut submenu_first, HandlerPolicy::Inline, &["A"]);
        assert_eq!(
            shape(&submenu_first),
            ["Open", "-", "A", "拡張メニュー{S}", "-", "プロパティ"]
        );
        assert_eq!(outcome, MergeOutcome::Inserted { at: 2, len: 1 });
    }

    #[test]
    fn submenu_title_mnemonic_is_parsed() {
        let mut entries = vec![item("Open")];
        merge_group(
            &mut entries,
            vec![hydrated("A")],
            HandlerPolicy::Submenu,
            "E&xtensions",
        );
        let MenuEntry::Item(container) = &entries[2] else {
            panic!("expected the extension submenu entry");
        };
        assert_eq!(container.label, "Extensions");
        assert_eq!(container.mnemonic, Some('X'));
        let outcome = merge_group(
            &mut entries,
            vec![hydrated("B")],
            HandlerPolicy::Submenu,
            "E&xtensions",
        );
        assert_eq!(outcome, MergeOutcome::Extended { container: 2 });
    }

    #[test]
    fn empty_and_disabled_groups_change_nothing() {
        let mut entries = vec![item("Open"), properties()];
        let before = shape(&entries);
        assert_eq!(
            merge(&mut entries, HandlerPolicy::Inline, &[]),
            MergeOutcome::Unchanged
        );
        assert_eq!(
            merge(&mut entries, HandlerPolicy::Submenu, &["-", "-"]),
            MergeOutcome::Unchanged
        );
        assert_eq!(
            merge(&mut entries, HandlerPolicy::Disabled, &["A"]),
            MergeOutcome::Unchanged
        );
        assert_eq!(shape(&entries), before);
    }

    #[test]
    fn indices_after_an_insertion_point_shift() {
        let mut selected = Some(5);
        shift_index(&mut selected, 2, 3);
        assert_eq!(selected, Some(8));
        let mut before = Some(1);
        shift_index(&mut before, 2, 3);
        assert_eq!(before, Some(1));
        let mut at = Some(2);
        shift_index(&mut at, 2, 3);
        assert_eq!(at, Some(5));
    }

    #[test]
    fn selection_steps_skip_separators_and_wrap() {
        let entries = vec![sep(), item("A"), sep(), item("B"), sep()];
        assert_eq!(step_selection(&entries, None, true), Some(1));
        assert_eq!(step_selection(&entries, Some(1), true), Some(3));
        assert_eq!(step_selection(&entries, Some(3), true), Some(1));
        assert_eq!(step_selection(&entries, None, false), Some(3));
        assert_eq!(step_selection(&entries, Some(1), false), Some(3));
        assert_eq!(step_selection(&entries, Some(3), false), Some(1));
    }

    #[test]
    fn selection_steps_find_nothing_without_items() {
        assert_eq!(step_selection(&[], None, true), None);
        assert_eq!(step_selection(&[sep(), sep()], Some(0), true), None);
    }

    #[test]
    fn explicit_mnemonics_win_over_first_letters() {
        let entries = vec![item("Open"), item("Copy &as path"), item("Add")];
        assert_eq!(
            find_mnemonic(&entries, 'a', None),
            Some(MnemonicHit::Unique(1))
        );
        assert_eq!(
            find_mnemonic(&entries, 'A', None),
            Some(MnemonicHit::Unique(1))
        );
    }

    #[test]
    fn first_letters_activate_unique_matches_and_cycle_shared_ones() {
        let entries = vec![item("Copy"), sep(), item("Cut"), item("Paste")];
        assert_eq!(
            find_mnemonic(&entries, 'c', None),
            Some(MnemonicHit::Next(0))
        );
        assert_eq!(
            find_mnemonic(&entries, 'C', Some(0)),
            Some(MnemonicHit::Next(2))
        );
        assert_eq!(
            find_mnemonic(&entries, 'c', Some(2)),
            Some(MnemonicHit::Next(0))
        );
        assert_eq!(
            find_mnemonic(&entries, 'p', Some(0)),
            Some(MnemonicHit::Unique(3))
        );
        assert_eq!(find_mnemonic(&entries, 'z', None), None);
    }

    #[test]
    fn only_enabled_non_empty_submenus_open() {
        let mut submenu = item("More");
        if let MenuEntry::Item(entry) = &mut submenu {
            entry.action = EntryAction::Submenu(vec![item("Child")]);
        }
        let mut disabled = submenu.clone();
        if let MenuEntry::Item(entry) = &mut disabled {
            entry.enabled = false;
        }
        let mut empty = submenu.clone();
        if let MenuEntry::Item(entry) = &mut empty {
            entry.action = EntryAction::Submenu(Vec::new());
        }
        let entries = vec![submenu, disabled, empty, item("Plain")];
        assert_eq!(
            submenu_children(&entries, 0).map(<[MenuEntry]>::len),
            Some(1)
        );
        assert!(submenu_children(&entries, 1).is_none());
        assert!(submenu_children(&entries, 2).is_none());
        assert!(submenu_children(&entries, 3).is_none());
        assert!(submenu_children(&entries, 9).is_none());
    }

    struct HeadlessPopup {
        cell: Box<SessionCell>,
    }

    impl HeadlessPopup {
        fn new(
            entries: Vec<MenuEntry>,
            anchor: (i32, i32),
            work_area: PixelRect,
            pending: usize,
            receiver: Option<Receiver<HydrationEvent>>,
        ) -> Self {
            use rwm_core::config::{Density, ThemePreference};
            let instance = register_popup_class();
            let metrics = compute_metrics(Density::Compact, 96);
            let theme = resolve_theme(ThemePreference::Light);
            let session = Session {
                instance,
                theme: theme.clone(),
                metrics,
                font_family: DEFAULT_FONT_FAMILY.to_owned(),
                work_area,
                anchor,
                round_corners: false,
                outline_radius: 0.0,
                hover_delay_ms: 140,
                show_timings: false,
                show_loading_footer: true,
                japanese: true,
                submenu_title: TITLE.to_owned(),
                session_id: 1,
                stage0_us: 100,
                hydration: receiver,
                pending,
                hydrated_groups: 0,
                levels: Vec::new(),
                windows: Vec::new(),
                last_cursor: None,
                mouse_owns_selection: false,
                shift_guard_until: None,
                suppress_show: true,
                button_armed: false,
                outcome: None,
            };
            let cell = Box::new(SessionCell {
                state: RefCell::new(session),
                aborted: Cell::new(false),
            });
            let root = create_popup_window(instance, HWND::default(), &cell, false)
                .expect("create hidden popup");
            {
                let mut s = cell.state.borrow_mut();
                s.windows.push(root);
                let renderer =
                    MenuRenderer::new(root, &s.theme, &s.metrics, &s.font_family);
                let footer_visible = s.footer_visible();
                let layout = compute_layout(
                    &renderer.measure_rows(&entries),
                    &s.metrics,
                    footer_visible,
                    work_area.height() as f32,
                );
                let size = layout.pixel_size();
                let rect = PixelRect::from_origin_size(
                    clamp_popup_origin(anchor, size, work_area),
                    size,
                );
                s.levels
                    .push(Level::new(root, entries, None, layout, renderer, rect));
            }
            Self { cell }
        }

        fn row_client_center(&self, level: usize, index: usize) -> (i32, i32) {
            let s = self.cell.state.borrow();
            let lvl = &s.levels[level];
            let top = lvl.layout.row_top(index, lvl.scroll);
            let mid_y = (top + lvl.layout.rows[index].height * 0.5).round() as i32;
            (24, mid_y)
        }

        fn move_cursor_to_row(&self, level: usize, index: usize) -> (i32, i32) {
            let (cx, cy) = self.row_client_center(level, index);
            let lparam = LPARAM(((cy as u16 as usize) << 16 | (cx as u16 as usize)) as isize);
            let mut s = self.cell.state.borrow_mut();
            s.on_mouse_move(level, lparam);
            let rect = s.levels[level].rect;
            (rect.left + cx, rect.top + cy)
        }
    }

    impl Drop for HeadlessPopup {
        fn drop(&mut self) {
            self.cell.destroy_windows();
        }
    }

    fn ready_event(policy: HandlerPolicy, labels: &[&str]) -> HydrationEvent {
        use rwm_core::model::{HandlerStatus, HandlerTiming};
        let entries: Vec<MenuEntry> = labels.iter().map(|l| hydrated(l)).collect();
        HydrationEvent::GroupReady {
            session_id: 1,
            group_name: "TestExt".to_owned(),
            clsid: "{11111111-1111-1111-1111-111111111111}".to_owned(),
            policy,
            timing: HandlerTiming {
                clsid: "{11111111-1111-1111-1111-111111111111}".to_owned(),
                name: "TestExt".to_owned(),
                dll_path: None,
                duration_us: 1000,
                item_count: entries.len(),
                status: HandlerStatus::Ok,
            },
            entries,
        }
    }

    fn static_item_with_verb(label: &str, verb: &str) -> MenuEntry {
        let mut entry = item(label);
        if let MenuEntry::Item(ref mut mi) = entry {
            mi.action = EntryAction::StaticCommand {
                verb: verb.to_owned(),
                command: "cmd.exe".to_owned(),
                working_dir: PathBuf::from(r"C:\"),
            };
        }
        entry
    }

    #[test]
    fn upward_hydration_pins_hovered_row_or_syncs_and_guards_click() {
        let (tx, rx) = std::sync::mpsc::channel();
        // Anchor near bottom edge (500, 1010) on 1040px work area so menu flips upward on open.
        let work = PixelRect::new(0, 0, 1920, 1040);
        let entries = vec![
            static_item_with_verb("Open", "open"),
            static_item_with_verb("Copy", "copy"),
            static_item_with_verb("Delete", "delete"),
            properties(),
        ];
        let popup = HeadlessPopup::new(entries, (500, 1010), work, 1, Some(rx));
        let cursor_screen = popup.move_cursor_to_row(0, 1); // hovering Copy
        assert_eq!(popup.cell.state.borrow().levels[0].selected, Some(1));

        tx.send(ready_event(HandlerPolicy::Inline, &["Ext1", "Ext2", "Ext3"]))
            .unwrap();
        popup.cell.state.borrow_mut().drain_hydration();

        {
            let s = popup.cell.state.borrow();
            let lvl = &s.levels[0];
            let under_cursor = s
                .cursor_row_at(0, cursor_screen)
                .filter(|&i| is_item(&lvl.entries[i]));
            assert_eq!(
                lvl.selected, under_cursor,
                "highlight must always match the item under cursor after reflow"
            );
        }

        // Now force work_area clamp by placing hover near top of a tight work_area where
        // inserting before Properties pushes Properties off if pinned -> work_area forces shift!
        let (tx2, rx2) = std::sync::mpsc::channel();
        let tight = PixelRect::new(0, 0, 1920, 130);
        let entries2 = vec![
            static_item_with_verb("Open", "open"),
            static_item_with_verb("Copy", "copy"),
            properties(),
        ];
        let popup2 = HeadlessPopup::new(entries2, (500, 0), tight, 1, Some(rx2));
        let cursor2 = popup2.move_cursor_to_row(0, 2); // hovering Properties
        assert_eq!(popup2.cell.state.borrow().levels[0].selected, Some(2));

        tx2.send(ready_event(HandlerPolicy::Inline, &["Ext1", "Ext2", "Ext3"]))
            .unwrap();
        popup2.cell.state.borrow_mut().drain_hydration();

        {
            let mut s = popup2.cell.state.borrow_mut();
            let under_cursor = s
                .cursor_row_at(0, cursor2)
                .filter(|&i| is_item(&s.levels[0].entries[i]));
            assert_eq!(
                s.levels[0].selected, under_cursor,
                "when work area prevents pinning, highlight resyncs to actual item under cursor"
            );
            // Attempting immediate click right after involuntary row shift is guarded.
            s.button_armed = true;
            let rect = s.levels[0].rect;
            let lparam = LPARAM(
                (((cursor2.1 - rect.top) as u16 as usize) << 16
                    | ((cursor2.0 - rect.left) as u16 as usize)) as isize,
            );
            s.on_button_up(&popup2.cell, 0, lparam);
            assert!(
                s.outcome.is_none(),
                "click immediately after row shift under stationary cursor must be ignored"
            );
        }
    }

    #[test]
    fn downward_hydration_over_properties_pins_hovered_row_when_room_exists() {
        let (tx, rx) = std::sync::mpsc::channel();
        let work = PixelRect::new(0, 0, 1920, 1040);
        let entries = vec![
            static_item_with_verb("Open", "open"),
            static_item_with_verb("Rename", "rename"),
            properties(),
        ];
        let popup = HeadlessPopup::new(entries, (500, 400), work, 1, Some(rx));
        let cursor_screen = popup.move_cursor_to_row(0, 2); // hovering プロパティ

        tx.send(ready_event(HandlerPolicy::Inline, &["Ext1", "Ext2"]))
            .unwrap();
        popup.cell.state.borrow_mut().drain_hydration();

        let mut s = popup.cell.state.borrow_mut();
        let under = s.cursor_row_at(0, cursor_screen).expect("still over a row");
        assert_eq!(s.levels[0].selected, Some(under));
        // Space above anchor (400px) allowed pinning Properties in place at its shifted index!
        match &s.levels[0].entries[under] {
            MenuEntry::Item(i) => {
                assert!(matches!(i.action, EntryAction::CoreVerb(CoreVerb::Properties)))
            }
            other => panic!("expected Properties pinned under cursor, got {other:?}"),
        }
        // Because Properties stayed under the cursor, clicking runs Properties (never Ext1!).
        s.button_armed = true;
        let rect = s.levels[0].rect;
        let lparam = LPARAM(
            (((cursor_screen.1 - rect.top) as u16 as usize) << 16
                | ((cursor_screen.0 - rect.left) as u16 as usize)) as isize,
        );
        s.on_button_up(&popup.cell, 0, lparam);
        assert!(matches!(
            s.outcome,
            Some(Outcome::Chosen(EntryAction::CoreVerb(CoreVerb::Properties)))
        ));
    }

    #[test]
    fn scroll_resyncs_mouse_selection_and_guards_immediate_click() {
        let work = PixelRect::new(0, 0, 1920, 150);
        let entries: Vec<MenuEntry> = (0..20)
            .map(|i| static_item_with_verb(&format!("Item {i}"), &format!("v{i}")))
            .collect();
        let popup = HeadlessPopup::new(entries, (100, 10), work, 0, None);
        let cursor_screen = popup.move_cursor_to_row(0, 0);
        let initial_sel = popup.cell.state.borrow().levels[0].selected;
        assert_eq!(initial_sel, Some(0));

        {
            let mut s = popup.cell.state.borrow_mut();
            let step = s.metrics.row_height * 2.0;
            assert!(s.scroll_by(0, step));
            let after_sel = s.levels[0].selected;
            let actual = s.cursor_row_at(0, cursor_screen);
            assert_eq!(after_sel, actual);
            assert_ne!(after_sel, initial_sel, "scroll must update hover selection");

            s.button_armed = true;
            let rect = s.levels[0].rect;
            let lparam = LPARAM(
                (((cursor_screen.1 - rect.top) as u16 as usize) << 16
                    | ((cursor_screen.0 - rect.left) as u16 as usize)) as isize,
            );
            s.on_button_up(&popup.cell, 0, lparam);
            assert!(
                s.outcome.is_none(),
                "scroll shifted the row under cursor so immediate button-up is guarded"
            );
        }
    }

    #[test]
    fn drain_hydration_with_open_extension_submenu_and_inline_insert_does_not_panic() {
        let (tx, rx) = std::sync::mpsc::channel();
        let work = PixelRect::new(0, 0, 1920, 1040);
        let entries = vec![
            static_item_with_verb("Open", "open"),
            properties(),
        ];
        let popup = HeadlessPopup::new(entries, (200, 200), work, 3, Some(rx));

        // Open the extension submenu with an initial Submenu hydration event.
        tx.send(ready_event(HandlerPolicy::Submenu, &["SubInit"]))
            .unwrap();
        popup.cell.state.borrow_mut().drain_hydration();
        let sub_owner = {
            let s = popup.cell.state.borrow();
            s.levels[0]
                .entries
                .iter()
                .position(|e| is_extension_submenu(e, TITLE))
                .expect("extension submenu exists")
        };
        {
            let mut s = popup.cell.state.borrow_mut();
            s.open_submenu(&popup.cell, 0, sub_owner);
            assert_eq!(s.levels.len(), 2);
            assert_eq!(s.levels[1].owner, Some(sub_owner));
        }

        // Now deliver 3 inline rows AND another submenu group in ONE drain batch.
        // Previously relayout(1) ran before relayout(0) and panicked indexing stale rows.
        tx.send(ready_event(
            HandlerPolicy::Inline,
            &["In1", "In2", "In3"],
        ))
        .unwrap();
        tx.send(ready_event(HandlerPolicy::Submenu, &["SubSecond"]))
            .unwrap();
        tx.send(HydrationEvent::Completed {
            session_id: 1,
            total_us: 5000,
        })
        .unwrap();
        popup.cell.state.borrow_mut().drain_hydration();

        let s = popup.cell.state.borrow();
        assert_eq!(s.levels.len(), 2);
        assert_eq!(s.levels[1].owner, Some(sub_owner + 3));
        assert_eq!(count_items(&s.levels[1].entries), 2);
        assert_eq!(s.pending, 0);
        assert_eq!(s.hydrated_groups, 3);
    }

    #[test]
    fn disabled_hydration_event_does_not_decrement_pending_or_count_hydrated() {
        let (tx, rx) = std::sync::mpsc::channel();
        let work = PixelRect::new(0, 0, 1920, 1040);
        let entries = vec![static_item_with_verb("Open", "open"), properties()];
        let popup = HeadlessPopup::new(entries, (200, 200), work, 1, Some(rx));

        tx.send(ready_event(HandlerPolicy::Disabled, &["HiddenExt"]))
            .unwrap();
        popup.cell.state.borrow_mut().drain_hydration();

        let s = popup.cell.state.borrow();
        assert_eq!(s.pending, 1, "disabled handler must not decrement pending");
        assert_eq!(
            s.hydrated_groups, 0,
            "disabled handler must not count as hydrated"
        );
        assert_eq!(shape(&s.levels[0].entries), ["Open", "プロパティ"]);
    }

    #[test]
    fn wm_printclient_renders_visible_popup_pixels() {
        use std::collections::HashSet;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

        let work = PixelRect::new(0, 0, 1920, 1040);
        let entries = vec![
            static_item_with_verb("Open", "open"),
            static_item_with_verb("Copy", "copy"),
            MenuEntry::Separator,
            properties(),
        ];
        let popup = HeadlessPopup::new(entries, (200, 200), work, 0, None);
        let (hwnd, width, height) = {
            let s = popup.cell.state.borrow();
            let size = s.levels[0].layout.pixel_size();
            (s.levels[0].hwnd, size.0, size.1)
        };

        unsafe {
            let dc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("create 32bpp DIB");
            let prev = SelectObject(dc, bmp);

            SendMessageW(hwnd, WM_PRINTCLIENT, WPARAM(dc.0 as usize), LPARAM(0));

            let slice =
                std::slice::from_raw_parts(bits.cast::<u32>(), (width * height) as usize);
            let distinct: HashSet<u32> = slice.iter().copied().collect();
            assert!(
                distinct.len() > 8,
                "WM_PRINTCLIENT should draw background, text, separator and borders (got {} colors)",
                distinct.len()
            );

            SelectObject(dc, prev);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(dc);
        }
    }
}
