//! Dedicated STA daemon inside `rwm-host.exe` for speculative off-thread `NativeMenu` prebuilding
//! and 1-5 ms right-click menu presentation. Never runs inside `explorer.exe`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rwm_core::config::RwmConfig;
use rwm_core::ipc_cache::{
    edit_label_msg_id, now_tick_ms, PrefetchPayload, SharedCacheHost, ShowRequest, MAX_READY_SLOTS,
};
use rwm_core::shell::{
    clipboard_sequence_number, enable_dark_mode_menus, NativeMenu, OleGuard, SelectionKey,
    CUSTOMIZE_MENU_CMD_ID,
};
use rwm_core::warm_pool::WarmDllPin;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleFlushClipboard;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetActiveWindow, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EndMenu, GetAncestor, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowThreadProcessId, IsWindow, KillTimer, PostMessageW,
    RegisterClassExW, SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
    SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
    WindowFromPoint, GA_ROOT, GWLP_USERDATA, HC_ACTION, HHOOK, HWND_TOP, KBDLLHOOKSTRUCT,
    LWA_ALPHA, MSG, MSLLHOOKSTRUCT, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WA_INACTIVE,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_ACTIVATE, WM_ACTIVATEAPP, WM_APP, WM_CANCELMODE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_NULL, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_TIMER,
    WM_XBUTTONDOWN, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_PROCESS_WORK: u32 = WM_APP + 1;
const WM_APP_CONFIG_RELOAD: u32 = WM_APP + 2;
const WM_APP_CANCEL_MENU: u32 = WM_APP + 3;
const CACHE_PRUNE_TIMER: usize = 0x5257_4D50;
const TRACK_WATCHDOG_TIMER: usize = 0x5257_4D51;
const POPUP_CLASS_NAME: &str = "RustWindowsMenuPopup_v3";
const MAX_PENDING_SHOW_AGE: Duration = Duration::from_millis(2000);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadyKey {
    key_hash: u64,
    clipboard_seq: u32,
    expires_at: Instant,
}

#[derive(Debug)]
struct PendingShow {
    req: ShowRequest,
    received_at: Instant,
}

#[derive(Default)]
struct CoordinatorState {
    ready: Vec<ReadyKey>,
    in_flight: Option<(u64, u32)>,
    pending_prefetch: Option<PrefetchPayload>,
    pending_show: Option<PendingShow>,
}

struct CachedEntry<M> {
    key_hash: u64,
    clipboard_seq: u32,
    expires_at: Instant,
    expires_tick_ms: u64,
    menu: M,
}

/// Pure LRU/TTL store generic over menu type `M`, allowing zero-COM unit tests.
struct CacheCore<M> {
    entries: VecDeque<CachedEntry<M>>,
    max_entries: usize,
    ttl: Duration,
}

impl<M> CacheCore<M> {
    fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.clamp(1, MAX_READY_SLOTS)),
            max_entries: max_entries.clamp(1, MAX_READY_SLOTS),
            ttl,
        }
    }

    fn reconfigure(&mut self, max_entries: usize, ttl: Duration) {
        self.max_entries = max_entries.clamp(1, MAX_READY_SLOTS);
        self.ttl = ttl;
        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn prune_expired(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at > now);
        before - self.entries.len()
    }

    fn contains(&self, key_hash: u64, clipboard_seq: u32, now: Instant) -> bool {
        self.entries.iter().any(|entry| {
            entry.key_hash == key_hash
                && entry.clipboard_seq == clipboard_seq
                && entry.expires_at > now
        })
    }

    fn insert(&mut self, key_hash: u64, clipboard_seq: u32, menu: M, now: Instant, now_ms: u64) {
        self.prune_expired(now);
        if let Some(pos) = self.entries.iter().position(|e| e.key_hash == key_hash) {
            self.entries.remove(pos);
        }
        while self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        let ttl_ms = u64::try_from(self.ttl.as_millis()).unwrap_or(8_000);
        self.entries.push_back(CachedEntry {
            key_hash,
            clipboard_seq,
            expires_at: now + self.ttl,
            expires_tick_ms: now_ms.saturating_add(ttl_ms),
            menu,
        });
    }

    fn take(&mut self, key_hash: u64, clipboard_seq: u32, now: Instant) -> Option<CachedEntry<M>> {
        self.prune_expired(now);
        let pos = self
            .entries
            .iter()
            .position(|entry| entry.key_hash == key_hash && entry.clipboard_seq == clipboard_seq)?;
        self.entries.remove(pos)
    }

    fn ready_keys(&self) -> Vec<ReadyKey> {
        self.entries
            .iter()
            .map(|entry| ReadyKey {
                key_hash: entry.key_hash,
                clipboard_seq: entry.clipboard_seq,
                expires_at: entry.expires_at,
            })
            .collect()
    }

    fn shm_slots(&self) -> Vec<(u64, u64)> {
        self.entries
            .iter()
            .map(|entry| (entry.key_hash, entry.expires_tick_ms))
            .collect()
    }
}

static TRACK_HOOK_POPUP: AtomicIsize = AtomicIsize::new(0);
static TRACK_HOOK_OUTSIDE_CLICK: AtomicBool = AtomicBool::new(false);

struct PopupWindowState {
    menu: *const NativeMenu,
    had_fg: bool,
}

struct PopupWindowGuard(HWND);
impl Drop for PopupWindowGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.0);
        }
    }
}

struct MenuBindingGuard<'a>(&'a mut PopupWindowState, PhantomData<&'a NativeMenu>);
impl<'a> MenuBindingGuard<'a> {
    fn bind(state: &'a mut PopupWindowState, menu: &'a NativeMenu, had_fg: bool) -> Self {
        state.menu = std::ptr::from_ref(menu);
        state.had_fg = had_fg;
        Self(state, PhantomData)
    }
}
impl Drop for MenuBindingGuard<'_> {
    fn drop(&mut self) {
        self.0.menu = std::ptr::null();
        self.0.had_fg = false;
    }
}

fn is_win32_menu_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut buf = [0u16; 16];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len != 6 {
        return false;
    }
    const MENU_CLASS: [u16; 6] = [
        b'#' as u16,
        b'3' as u16,
        b'2' as u16,
        b'7' as u16,
        b'6' as u16,
        b'8' as u16,
    ];
    buf[..6] == MENU_CLASS
}

fn is_point_on_menu_or_owner(pt: POINT, popup_hwnd: HWND) -> bool {
    let hit = unsafe { WindowFromPoint(pt) };
    if hit.is_invalid() {
        return false;
    }
    if hit == popup_hwnd || is_win32_menu_window(hit) {
        return true;
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    !root.is_invalid() && (root == popup_hwnd || is_win32_menu_window(root))
}

unsafe extern "system" fn track_mouse_ll_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if code == HC_ACTION as i32
            && matches!(
                wparam.0 as u32,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            )
        {
            let raw = TRACK_HOOK_POPUP.load(Ordering::Acquire);
            if raw != 0 && lparam.0 != 0 {
                let popup = HWND(raw as *mut c_void);
                let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                if !is_point_on_menu_or_owner(info.pt, popup) {
                    TRACK_HOOK_OUTSIDE_CLICK.store(true, Ordering::Release);
                    let _ = PostMessageW(popup, WM_APP_CANCEL_MENU, WPARAM(0), LPARAM(0));
                }
            }
        }
    }));
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn track_key_ll_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if code == HC_ACTION as i32
            && matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN)
        {
            let raw = TRACK_HOOK_POPUP.load(Ordering::Acquire);
            if raw != 0 && lparam.0 != 0 {
                let popup = HWND(raw as *mut c_void);
                let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                if info.vkCode == u32::from(VK_ESCAPE.0) && GetForegroundWindow() != popup {
                    let _ = PostMessageW(popup, WM_APP_CANCEL_MENU, WPARAM(0), LPARAM(0));
                }
            }
        }
    }));
    CallNextHookEx(None, code, wparam, lparam)
}

struct TrackDismissHookGuard {
    mouse_hook: Option<HHOOK>,
    key_hook: Option<HHOOK>,
}

impl TrackDismissHookGuard {
    fn install(popup_hwnd: HWND) -> Self {
        TRACK_HOOK_OUTSIDE_CLICK.store(false, Ordering::Release);
        TRACK_HOOK_POPUP.store(popup_hwnd.0 as isize, Ordering::Release);
        let hmod = unsafe { GetModuleHandleW(None) }
            .ok()
            .map(HINSTANCE::from)
            .unwrap_or_default();
        let mouse_hook =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(track_mouse_ll_proc), hmod, 0) }.ok();
        let key_hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(track_key_ll_proc), hmod, 0) }.ok();
        Self {
            mouse_hook,
            key_hook,
        }
    }

    fn dismissed_by_outside_click(&self) -> bool {
        TRACK_HOOK_OUTSIDE_CLICK.load(Ordering::Acquire)
    }
}

impl Drop for TrackDismissHookGuard {
    fn drop(&mut self) {
        TRACK_HOOK_POPUP.store(0, Ordering::Release);
        if let Some(hook) = self.mouse_hook.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        if let Some(hook) = self.key_hook.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
    }
}

struct TrackingGuard<'a>(&'a AtomicBool, &'a SharedCacheHost);
impl<'a> TrackingGuard<'a> {
    fn enter(is_tracking: &'a AtomicBool, shared_host: &'a SharedCacheHost) -> Self {
        is_tracking.store(true, Ordering::Release);
        shared_host.set_tracking(true);
        Self(is_tracking, shared_host)
    }
}
impl Drop for TrackingGuard<'_> {
    fn drop(&mut self) {
        self.1.set_tracking(false);
        self.0.store(false, Ordering::Release);
    }
}

struct WatchdogTimerGuard(HWND);
impl WatchdogTimerGuard {
    fn arm(hwnd: HWND) -> Self {
        unsafe {
            let _ = SetTimer(hwnd, TRACK_WATCHDOG_TIMER, 100, None);
        }
        Self(hwnd)
    }
}
impl Drop for WatchdogTimerGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(self.0, TRACK_WATCHDOG_TIMER);
        }
    }
}

struct FinishNotifier {
    done: Mutex<bool>,
    cvar: Condvar,
}

impl FinishNotifier {
    fn new() -> Self {
        Self {
            done: Mutex::new(false),
            cvar: Condvar::new(),
        }
    }

    fn mark_done(&self) {
        let mut guard = self.done.lock().unwrap_or_else(|e| e.into_inner());
        *guard = true;
        self.cvar.notify_all();
    }

    fn wait_timeout(&self, timeout: Duration) -> bool {
        let guard = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .cvar
            .wait_timeout_while(guard, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

pub struct MenuCacheWorker {
    shared_host: Arc<SharedCacheHost>,
    coordinator: Arc<Mutex<CoordinatorState>>,
    is_tracking: Arc<AtomicBool>,
    popup_hwnd: Arc<AtomicIsize>,
    stop_requested: Arc<AtomicBool>,
    finished: Arc<FinishNotifier>,
    thread: Option<JoinHandle<()>>,
}

impl MenuCacheWorker {
    pub fn start(shared_host: Arc<SharedCacheHost>) -> Result<Self, String> {
        let coordinator = Arc::new(Mutex::new(CoordinatorState::default()));
        let is_tracking = Arc::new(AtomicBool::new(false));
        let popup_hwnd = Arc::new(AtomicIsize::new(0));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(FinishNotifier::new());
        let init_signal = Arc::new((Mutex::new(Option::<Result<(), String>>::None), Condvar::new()));

        let thread = std::thread::Builder::new()
            .name("rwm-menu-sta".into())
            .spawn({
                let shared_host = Arc::clone(&shared_host);
                let coordinator = Arc::clone(&coordinator);
                let is_tracking = Arc::clone(&is_tracking);
                let popup_hwnd = Arc::clone(&popup_hwnd);
                let stop_requested = Arc::clone(&stop_requested);
                let finished = Arc::clone(&finished);
                let init_signal = Arc::clone(&init_signal);
                move || {
                    sta_thread_main(
                        shared_host,
                        coordinator,
                        is_tracking,
                        popup_hwnd,
                        stop_requested,
                        finished,
                        init_signal,
                    );
                }
            })
            .map_err(|error| format!("cannot spawn menu STA thread: {error}"))?;

        let (lock, cvar) = &*init_signal;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = cvar
            .wait_timeout_while(guard, Duration::from_secs(5), |res| res.is_none())
            .unwrap_or_else(|e| e.into_inner());
        match guard.take() {
            Some(Ok(())) => Ok(Self {
                shared_host,
                coordinator,
                is_tracking,
                popup_hwnd,
                stop_requested,
                finished,
                thread: Some(thread),
            }),
            Some(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            None => {
                stop_requested.store(true, Ordering::Release);
                Err("menu STA thread did not finish initialization in 5s".into())
            }
        }
    }

    pub fn shared_host(&self) -> &SharedCacheHost {
        &self.shared_host
    }

    pub fn on_prefetch_signal(&self) {
        let Some(payload) = self.shared_host.read_prefetch() else {
            return;
        };
        {
            let mut state = self.coordinator.lock().unwrap_or_else(|e| e.into_inner());
            let supersede = state
                .pending_prefetch
                .as_ref()
                .is_none_or(|cur| payload.request_seq >= cur.request_seq);
            if supersede {
                state.pending_prefetch = Some(payload);
            }
        }
        self.wake_sta();
    }

    /// Sub-millisecond handoff gate run on `host.rs`'s window loop when Explorer sends `ClaimShow`.
    pub fn on_claim_show_signal(&self) -> bool {
        if self.is_tracking.load(Ordering::Acquire) {
            self.cancel_active_menu();
            return false;
        }
        let Some(req) = self.shared_host.read_show() else {
            return false;
        };
        let now = Instant::now();
        let accepted = {
            let mut state = self.coordinator.lock().unwrap_or_else(|e| e.into_inner());
            if self.is_tracking.load(Ordering::Acquire) {
                false
            } else {
                let is_in_flight = state.in_flight == Some((req.key_hash, req.clipboard_seq));
                let is_ready = state.ready.iter().any(|entry| {
                    entry.key_hash == req.key_hash
                        && entry.clipboard_seq == req.clipboard_seq
                        && entry.expires_at > now
                });
                let is_queued = state.pending_prefetch.as_ref().is_some_and(|p| {
                    p.key_hash == req.key_hash && p.clipboard_seq == req.clipboard_seq
                });
                if is_in_flight || is_ready || is_queued {
                    state.pending_show = Some(PendingShow {
                        req,
                        received_at: now,
                    });
                    self.shared_host.set_pending_show(true);
                    true
                } else {
                    false
                }
            }
        };
        if !accepted && self.is_tracking.load(Ordering::Acquire) {
            self.cancel_active_menu();
        }
        if accepted {
            self.wake_sta();
        }
        accepted
    }

    pub fn cancel_active_menu(&self) {
        {
            let mut state = self.coordinator.lock().unwrap_or_else(|e| e.into_inner());
            state.pending_show = None;
        }
        self.shared_host.set_pending_show(false);
        if self.is_tracking.load(Ordering::Acquire) {
            let raw = self.popup_hwnd.load(Ordering::Acquire);
            if raw != 0 {
                let hwnd = HWND(raw as *mut c_void);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_CANCELMODE, WPARAM(0), LPARAM(0));
                    let _ = PostMessageW(hwnd, WM_APP_CANCEL_MENU, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    fn wake_sta(&self) {
        let raw = self.popup_hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let _ = unsafe {
                PostMessageW(
                    HWND(raw as *mut c_void),
                    WM_APP_PROCESS_WORK,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
    }

    pub fn on_config_reload_signal(&self) {
        let raw = self.popup_hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let _ = unsafe {
                PostMessageW(
                    HWND(raw as *mut c_void),
                    WM_APP_CONFIG_RELOAD,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
    }

    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        let raw = self.popup_hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut c_void);
            unsafe {
                let _ = PostMessageW(hwnd, WM_CANCELMODE, WPARAM(0), LPARAM(0));
                let _ = PostMessageW(hwnd, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if self.finished.wait_timeout(SHUTDOWN_WAIT) {
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for MenuCacheWorker {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

unsafe extern "system" fn popup_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupWindowState;
        if !state.is_null() {
            if message == WM_APP_CANCEL_MENU {
                let _ = EndMenu();
                return Some(LRESULT(0));
            }
            if message == WM_TIMER && wparam.0 == TRACK_WATCHDOG_TIMER {
                if !(*state).menu.is_null() && (*state).had_fg && GetForegroundWindow() != hwnd {
                    let _ = EndMenu();
                }
                return Some(LRESULT(0));
            }
            let menu = (*state).menu;
            if !menu.is_null() {
                if (message == WM_ACTIVATE && (wparam.0 & 0xFFFF) == WA_INACTIVE as usize)
                    || (message == WM_ACTIVATEAPP && wparam.0 == 0)
                    || message == WM_CANCELMODE
                {
                    let _ = EndMenu();
                } else if let Some(result) = (*menu).handle_menu_msg(message, wparam, lparam) {
                    return Some(result);
                }
            }
        }
        None
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

fn create_popup_window() -> Result<PopupWindowGuard, String> {
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let class_wide: Vec<u16> = POPUP_CLASS_NAME.encode_utf16().chain([0]).collect();
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(popup_window_proc),
        hInstance: HINSTANCE::from(instance),
        lpszClassName: windows::core::PCWSTR(class_wide.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassExW(&class);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            windows::core::PCWSTR(class_wide.as_ptr()),
            w!("RustWindowsMenu Popup"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            instance,
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW popup owner failed: {e}"))?;
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
    }
    Ok(PopupWindowGuard(hwnd))
}

type InitSignal = (Mutex<Option<Result<(), String>>>, Condvar);

struct ExitMarkGuard(Arc<FinishNotifier>);
impl Drop for ExitMarkGuard {
    fn drop(&mut self) {
        self.0.mark_done();
    }
}

fn sta_thread_main(
    shared_host: Arc<SharedCacheHost>,
    coordinator: Arc<Mutex<CoordinatorState>>,
    is_tracking: Arc<AtomicBool>,
    popup_hwnd_out: Arc<AtomicIsize>,
    stop_requested: Arc<AtomicBool>,
    finished: Arc<FinishNotifier>,
    init_signal: Arc<InitSignal>,
) {
    let _exit_guard = ExitMarkGuard(finished);
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _dark = enable_dark_mode_menus();
    let _ole = match OleGuard::initialize() {
        Ok(guard) => guard,
        Err(error) => {
            notify_init(&init_signal, Err(format!("OleInitialize failed: {error}")));
            return;
        }
    };
    let popup_guard = match create_popup_window() {
        Ok(guard) => guard,
        Err(error) => {
            notify_init(&init_signal, Err(error));
            return;
        }
    };
    let popup_hwnd = popup_guard.0;
    let mut win_state = Box::new(PopupWindowState {
        menu: std::ptr::null(),
        had_fg: false,
    });
    unsafe {
        SetWindowLongPtrW(
            popup_hwnd,
            GWLP_USERDATA,
            std::ptr::from_mut(&mut *win_state) as isize,
        );
    }
    popup_hwnd_out.store(popup_hwnd.0 as isize, Ordering::Release);
    notify_init(&init_signal, Ok(()));

    let mut cfg = RwmConfig::load_or_default().unwrap_or_default();
    let mut cfg_checked_at = Instant::now();
    let mut cache = CacheCore::<NativeMenu>::new(
        cfg.host_cache_entries,
        Duration::from_millis(u64::from(cfg.host_cache_ttl_ms)),
    );
    let mut warm_pin = WarmDllPin::new(cfg.max_pinned_dlls);
    let mut last_seq = 0u64;
    let mut prune_timer_armed = false;

    let mut msg = MSG::default();
    while !stop_requested.load(Ordering::Acquire) {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if ret <= 0 || msg.message == WM_QUIT {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        let mut config_changed = false;
        if msg.message == WM_APP_CONFIG_RELOAD {
            if let Ok(latest) = RwmConfig::load_or_default() {
                cfg = latest;
            }
            cfg_checked_at = Instant::now();
            config_changed = true;
        } else if cfg_checked_at.elapsed() >= Duration::from_secs(1) {
            cfg_checked_at = Instant::now();
            if let Ok(latest) = RwmConfig::load_or_default() {
                if latest != cfg {
                    cfg = latest;
                    config_changed = true;
                }
            }
        }
        if config_changed {
            cache.reconfigure(
                cfg.host_cache_entries,
                Duration::from_millis(u64::from(cfg.host_cache_ttl_ms)),
            );
            cache.clear();
            sync_cache_views(&shared_host, &coordinator, &cache, None);
        }
        if msg.message == WM_TIMER && msg.wParam.0 == CACHE_PRUNE_TIMER {
            if cache.prune_expired(Instant::now()) > 0 {
                sync_cache_views(&shared_host, &coordinator, &cache, None);
            }
        } else if msg.message == WM_APP_PROCESS_WORK {
            process_work(
                popup_hwnd,
                &mut win_state,
                &shared_host,
                &coordinator,
                &is_tracking,
                &mut cache,
                &mut warm_pin,
                &cfg,
                &mut last_seq,
                &stop_requested,
            );
        }
        if cache.entries.is_empty() {
            if prune_timer_armed {
                let _ = unsafe { KillTimer(popup_hwnd, CACHE_PRUNE_TIMER) };
                prune_timer_armed = false;
            }
        } else if !prune_timer_armed {
            let _ = unsafe { SetTimer(popup_hwnd, CACHE_PRUNE_TIMER, 2000, None) };
            prune_timer_armed = true;
        }
    }

    if prune_timer_armed {
        let _ = unsafe { KillTimer(popup_hwnd, CACHE_PRUNE_TIMER) };
    }
    popup_hwnd_out.store(0, Ordering::Release);
    shared_host.set_pending_show(false);
    shared_host.set_tracking(false);
    shared_host.set_in_flight(0);
    shared_host.publish_slots(&[]);
    drop(cache);
    drop(win_state);
    drop(popup_guard);
    drop(warm_pin);
    unsafe {
        let _ = OleFlushClipboard();
    }
}

fn notify_init(signal: &InitSignal, outcome: Result<(), String>) {
    let mut guard = signal.0.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(outcome);
    signal.1.notify_all();
}

fn sync_cache_views(
    shared_host: &SharedCacheHost,
    coordinator: &Mutex<CoordinatorState>,
    cache: &CacheCore<NativeMenu>,
    in_flight: Option<(u64, u32)>,
) {
    let slots = cache.shm_slots();
    let ready = cache.ready_keys();
    {
        let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight = in_flight;
        state.ready = ready;
    }
    shared_host.set_in_flight(in_flight.map_or(0, |(h, _)| h));
    shared_host.publish_slots(&slots);
}

#[allow(clippy::too_many_arguments)]
fn process_work(
    popup_hwnd: HWND,
    win_state: &mut PopupWindowState,
    shared_host: &SharedCacheHost,
    coordinator: &Mutex<CoordinatorState>,
    is_tracking: &AtomicBool,
    cache: &mut CacheCore<NativeMenu>,
    warm_pin: &mut WarmDllPin,
    cfg: &RwmConfig,
    last_seq: &mut u64,
    stop_requested: &AtomicBool,
) {
    loop {
        if stop_requested.load(Ordering::Acquire) {
            return;
        }
        // 1. Check if a pending show request matches an entry already sitting in our LRU cache.
        let ready_hit = take_matching_show(coordinator, cache, shared_host);
        if let Some((req, entry)) = ready_hit {
            sync_cache_views(shared_host, coordinator, cache, None);
            show_and_invoke(
                popup_hwnd,
                win_state,
                &entry.menu,
                &req,
                is_tracking,
                shared_host,
            );
            drop(entry);
            continue;
        }

        // 2. Otherwise inspect the latest unstarted prefetch request.
        let payload = {
            let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
            state.pending_prefetch.take()
        };
        let Some(payload) = payload else {
            // Drop stale pending_show when there is no cache hit and no queued/in-flight prefetch.
            let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
            state.pending_show = None;
            shared_host.set_pending_show(false);
            return;
        };
        if payload.request_seq <= *last_seq {
            continue;
        }
        *last_seq = payload.request_seq;

        let now = Instant::now();
        if cache.contains(payload.key_hash, payload.clipboard_seq, now) {
            continue;
        }
        let Ok(selection) =
            SelectionKey::from_serialized_pidls(&payload.pidl_bytes, payload.shift_held)
        else {
            continue;
        };
        if !selection.is_local_filesystem() {
            continue;
        }
        let current_clip = clipboard_sequence_number();
        let effective_hash = if current_clip == payload.clipboard_seq {
            payload.key_hash
        } else {
            selection.cache_key_hash(current_clip)
        };
        if cache.contains(effective_hash, current_clip, now) {
            continue;
        }

        sync_cache_views(
            shared_host,
            coordinator,
            cache,
            Some((effective_hash, current_clip)),
        );

        let before = (cfg.pin_handler_dlls && warm_pin.should_snapshot())
            .then(WarmDllPin::snapshot_loaded_modules);
        let built = NativeMenu::build_from_selection(popup_hwnd, selection).map(|mut menu| {
            menu.apply_customizations(cfg);
            menu
        });
        if let Some(before) = before {
            warm_pin.pin_newly_loaded(&before);
        }

        match built {
            Ok(menu) => {
                let built_now = Instant::now();
                cache.insert(
                    effective_hash,
                    current_clip,
                    menu,
                    built_now,
                    now_tick_ms(),
                );
                sync_cache_views(shared_host, coordinator, cache, None);
            }
            Err(error) => {
                tracing::debug!(%error, "speculative menu build failed in rwm-host");
                sync_cache_views(shared_host, coordinator, cache, None);
                let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
                if state
                    .pending_show
                    .as_ref()
                    .is_some_and(|p| p.req.key_hash == effective_hash)
                {
                    state.pending_show = None;
                    shared_host.set_pending_show(false);
                }
            }
        }
    }
}

fn take_matching_show(
    coordinator: &Mutex<CoordinatorState>,
    cache: &mut CacheCore<NativeMenu>,
    shared_host: &SharedCacheHost,
) -> Option<(ShowRequest, CachedEntry<NativeMenu>)> {
    let now = Instant::now();
    let pending = {
        let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state.pending_show.as_ref()?;
        if now.saturating_duration_since(pending.received_at) > MAX_PENDING_SHOW_AGE {
            state.pending_show = None;
            shared_host.set_pending_show(false);
            return None;
        }
        if cache.contains(pending.req.key_hash, pending.req.clipboard_seq, now) {
            state.pending_show.take()
        } else {
            None
        }
    }?;
    let Some(entry) = cache.take(pending.req.key_hash, pending.req.clipboard_seq, now) else {
        shared_host.set_pending_show(false);
        return None;
    };
    Some((pending.req, entry))
}

fn activate_popup_owner(
    popup_hwnd: HWND,
    explorer_root: HWND,
    anchor_x: i32,
    anchor_y: i32,
) -> bool {
    unsafe {
        let _ = SetWindowPos(
            popup_hwnd,
            HWND_TOP,
            anchor_x,
            anchor_y,
            1,
            1,
            SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(popup_hwnd, SW_SHOW);
        let _ = SetForegroundWindow(popup_hwnd);
        if GetForegroundWindow() == popup_hwnd {
            return true;
        }
        let _ = BringWindowToTop(popup_hwnd);
        let _ = SetForegroundWindow(popup_hwnd);
        if GetForegroundWindow() == popup_hwnd {
            return true;
        }
        let fg = GetForegroundWindow();
        let target = if !fg.is_invalid() { fg } else { explorer_root };
        if !target.is_invalid() {
            let fg_tid = GetWindowThreadProcessId(target, None);
            let cur_tid = GetCurrentThreadId();
            if fg_tid != 0
                && fg_tid != cur_tid
                && AttachThreadInput(cur_tid, fg_tid, true).as_bool()
            {
                let _ = BringWindowToTop(popup_hwnd);
                let _ = SetForegroundWindow(popup_hwnd);
                let _ = SetActiveWindow(popup_hwnd);
                let _ = AttachThreadInput(cur_tid, fg_tid, false);
            }
        }
        GetForegroundWindow() == popup_hwnd
    }
}

fn show_and_invoke(
    popup_hwnd: HWND,
    win_state: &mut PopupWindowState,
    menu: &NativeMenu,
    req: &ShowRequest,
    is_tracking: &AtomicBool,
    shared_host: &SharedCacheHost,
) {
    let root = HWND(req.explorer_root_hwnd as *mut c_void);
    let defview = HWND(req.defview_hwnd as *mut c_void);

    let _tracking_guard = TrackingGuard::enter(is_tracking, shared_host);
    shared_host.set_pending_show(false);
    let got_fg = activate_popup_owner(popup_hwnd, root, req.anchor_x, req.anchor_y);
    let binding = MenuBindingGuard::bind(win_state, menu, got_fg);
    let watchdog = got_fg.then(|| WatchdogTimerGuard::arm(popup_hwnd));
    let dismiss_hooks = TrackDismissHookGuard::install(popup_hwnd);

    let selected = menu.track_only(popup_hwnd, (req.anchor_x, req.anchor_y), req.no_animation);
    let dismissed_by_click = dismiss_hooks.dismissed_by_outside_click();
    let was_fg = unsafe { GetForegroundWindow() } == popup_hwnd;
    drop(dismiss_hooks);
    drop(watchdog);
    drop(binding);
    unsafe {
        let _ = ShowWindow(popup_hwnd, SW_HIDE);
        let _ = PostMessageW(popup_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    }

    match selected {
        Ok(Some(cmd_id)) if cmd_id == CUSTOMIZE_MENU_CMD_ID => {
            if was_fg {
                restore_foreground_if_needed(popup_hwnd, root);
            }
            crate::gui::spawn_manager_for_selection(menu.selection());
        }
        Ok(Some(cmd_id)) => {
            if menu
                .canonical_verb_for_id(cmd_id)
                .is_some_and(|v| v.eq_ignore_ascii_case("rename"))
                && !defview.is_invalid()
                && unsafe { IsWindow(defview) }.as_bool()
            {
                if was_fg {
                    focus_explorer(root);
                }
                unsafe {
                    let _ = PostMessageW(defview, edit_label_msg_id(), WPARAM(0), LPARAM(0));
                }
            } else if let Err(error) = menu.invoke_id(cmd_id, popup_hwnd, (req.anchor_x, req.anchor_y))
            {
                tracing::debug!(%error, "cached menu InvokeCommand failed");
            }
        }
        Ok(None) => {
            if was_fg && !dismissed_by_click {
                restore_foreground_if_needed(popup_hwnd, root);
            }
        }
        Err(error) => {
            tracing::debug!(%error, "cached menu TrackPopupMenuEx failed");
            if was_fg && !dismissed_by_click {
                restore_foreground_if_needed(popup_hwnd, root);
            }
        }
    }
}

fn focus_explorer(explorer_root: HWND) {
    if !explorer_root.is_invalid() && unsafe { IsWindow(explorer_root) }.as_bool() {
        unsafe {
            let _ = SetForegroundWindow(explorer_root);
        }
    }
}

fn restore_foreground_if_needed(popup_hwnd: HWND, explorer_root: HWND) {
    if !explorer_root.is_invalid() && unsafe { IsWindow(explorer_root) }.as_bool() {
        let fg = unsafe { GetForegroundWindow() };
        if fg == popup_hwnd || fg.is_invalid() {
            unsafe {
                let _ = SetForegroundWindow(explorer_root);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rwm_core::ipc_cache::SharedCacheClient;
    use std::path::Path;
    use std::sync::atomic::AtomicU64;

    static NEXT_MAP: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn cache_core_enforces_lru_ttl_and_clipboard_seq() {
        let mut cache = CacheCore::new(2, Duration::from_millis(500));
        let t0 = Instant::now();
        cache.insert(1, 10, "one", t0, 1000);
        cache.insert(2, 10, "two", t0, 1000);
        cache.insert(3, 10, "three", t0, 1000);
        assert!(!cache.contains(1, 10, t0));
        assert!(cache.contains(2, 10, t0));
        assert!(cache.contains(3, 10, t0));
        // Wrong clipboard sequence never hits.
        assert!(!cache.contains(2, 11, t0));
        // Take consumes the entry (never reuse an HMENU twice).
        let got = cache.take(2, 10, t0).unwrap();
        assert_eq!(got.menu, "two");
        assert!(!cache.contains(2, 10, t0));
        // TTL expires remaining entry.
        assert!(!cache.contains(3, 10, t0 + Duration::from_millis(600)));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.prune_expired(t0 + Duration::from_millis(600)), 1);
    }

    #[test]
    #[ignore = "hosts a real Menu STA thread and builds native COM menus"]
    fn sta_worker_prebuilds_and_marks_shared_cache_ready() {
        let name = format!(
            r"Local\RwmStaWorkerTest_{}_{}",
            std::process::id(),
            NEXT_MAP.fetch_add(1, Ordering::Relaxed)
        );
        let shared = Arc::new(SharedCacheHost::create_named(&name).unwrap());
        shared.set_host_info(std::process::id(), 0x1010);
        let worker = MenuCacheWorker::start(Arc::clone(&shared)).unwrap();
        let client = SharedCacheClient::open_named(&name).unwrap();

        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let manifest = repo.join("Cargo.toml");
        let _ole = OleGuard::initialize().unwrap();
        let sel = SelectionKey::from_paths(&[manifest], false, false).unwrap();
        let clip = clipboard_sequence_number();
        let hash = sel.cache_key_hash(clip);
        let mut buf = Vec::new();
        assert!(sel.serialize_pidls(&mut buf));
        client.try_write_prefetch(hash, false, clip, &buf).unwrap();
        worker.on_prefetch_signal();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if client.is_ready_or_in_flight(hash, now_tick_ms()) {
                let state = worker.coordinator.lock().unwrap();
                if state.ready.iter().any(|r| r.key_hash == hash) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(client.is_ready_or_in_flight(hash, now_tick_ms()));
        let t_check = Instant::now();
        assert!(worker.coordinator.lock().unwrap().ready.iter().any(|r| r.key_hash == hash));
        assert!(t_check.elapsed() < Duration::from_millis(20));

        worker.shutdown();
    }

    #[test]
    #[ignore = "hosts a real Menu STA thread and tests live popup foreground & dismissal"]
    fn sta_worker_menu_dismisses_on_cancel_and_foreground_loss() {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_REMOVE, WS_VISIBLE};

        let pump_messages = || {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        };

        let name = format!(
            r"Local\RwmStaDismissTest_{}_{}",
            std::process::id(),
            NEXT_MAP.fetch_add(1, Ordering::Relaxed)
        );
        let shared = Arc::new(SharedCacheHost::create_named(&name).unwrap());
        let _ole = OleGuard::initialize().unwrap();
        let instance = unsafe { GetModuleHandleW(None) }.unwrap();
        let dummy_hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("RwmDismissTestDummy"),
                WS_POPUP | WS_VISIBLE,
                10,
                10,
                120,
                40,
                None,
                None,
                instance,
                None,
            )
        }
        .unwrap();
        struct DummyGuard(HWND);
        impl Drop for DummyGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        let _dummy = DummyGuard(dummy_hwnd);
        unsafe {
            let _ = SetForegroundWindow(dummy_hwnd);
        }

        shared.set_host_info(std::process::id(), dummy_hwnd.0 as isize);
        let worker = MenuCacheWorker::start(Arc::clone(&shared)).unwrap();
        let client = SharedCacheClient::open_named(&name).unwrap();

        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let manifest = repo.join("Cargo.toml");
        let sel = SelectionKey::from_paths(&[manifest], false, false).unwrap();
        let clip = clipboard_sequence_number();
        let hash = sel.cache_key_hash(clip);
        let mut buf = Vec::new();
        assert!(sel.serialize_pidls(&mut buf));

        let prefetch_and_wait_ready = || {
            client.try_write_prefetch(hash, false, clip, &buf).unwrap();
            worker.on_prefetch_signal();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                pump_messages();
                if worker
                    .coordinator
                    .lock()
                    .unwrap()
                    .ready
                    .iter()
                    .any(|r| r.key_hash == hash)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("menu did not reach ready state within 5s");
        };

        // 1. Open popup menu and verify cancel_active_menu() dismisses it cleanly.
        prefetch_and_wait_ready();
        let req = ShowRequest {
            request_seq: 0,
            key_hash: hash,
            clipboard_seq: clip,
            anchor_x: 120,
            anchor_y: 120,
            defview_hwnd: dummy_hwnd.0 as isize,
            explorer_root_hwnd: dummy_hwnd.0 as isize,
            no_animation: true,
        };
        assert!(client.try_write_show(&req).is_some());
        assert!(worker.on_claim_show_signal());

        let deadline = Instant::now() + Duration::from_secs(3);
        while !worker.is_tracking.load(Ordering::Acquire) && Instant::now() < deadline {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            worker.is_tracking.load(Ordering::Acquire),
            "popup menu did not enter tracking"
        );
        assert!(client.is_host_tracking());
        assert!(
            !worker.on_claim_show_signal(),
            "claim while tracking must be rejected & cancel"
        );

        let dismiss_deadline = Instant::now() + Duration::from_secs(1);
        while (worker.is_tracking.load(Ordering::Acquire) || client.is_host_tracking())
            && Instant::now() < dismiss_deadline
        {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !worker.is_tracking.load(Ordering::Acquire) && !client.is_host_tracking(),
            "menu remained stuck after cancel"
        );

        // 2. Open popup menu again and verify inactivation / foreground loss dismisses it cleanly.
        prefetch_and_wait_ready();
        assert!(client.try_write_show(&req).is_some());
        assert!(worker.on_claim_show_signal());

        let deadline = Instant::now() + Duration::from_secs(3);
        while !worker.is_tracking.load(Ordering::Acquire) && Instant::now() < deadline {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            worker.is_tracking.load(Ordering::Acquire),
            "second popup did not enter tracking"
        );
        let popup_hwnd = HWND(worker.popup_hwnd.load(Ordering::Acquire) as *mut c_void);
        assert!(!popup_hwnd.is_invalid());
        if unsafe { GetForegroundWindow() } == popup_hwnd {
            unsafe {
                let _ = SetForegroundWindow(dummy_hwnd);
            }
        } else {
            unsafe {
                let _ = PostMessageW(popup_hwnd, WM_ACTIVATE, WPARAM(WA_INACTIVE as usize), LPARAM(0));
            }
        }
        let fg_deadline = Instant::now() + Duration::from_secs(1);
        while (worker.is_tracking.load(Ordering::Acquire) || client.is_host_tracking())
            && Instant::now() < fg_deadline
        {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !worker.is_tracking.load(Ordering::Acquire) && !client.is_host_tracking(),
            "menu remained stuck after losing foreground focus"
        );

        worker.shutdown();
    }
}
