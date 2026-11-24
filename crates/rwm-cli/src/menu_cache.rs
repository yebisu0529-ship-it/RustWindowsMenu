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
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleFlushClipboard;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetWindowLongPtrW, IsWindow, KillTimer, PostMessageW, RegisterClassExW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, TranslateMessage, GWLP_USERDATA,
    HWND_TOPMOST, MSG, SWP_NOACTIVATE, SWP_NOSIZE, WM_APP, WM_CANCELMODE, WM_NULL, WM_QUIT,
    WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_PROCESS_WORK: u32 = WM_APP + 1;
const WM_APP_CONFIG_RELOAD: u32 = WM_APP + 2;
const CACHE_PRUNE_TIMER: usize = 0x5257_4D50;
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

struct PopupWindowState {
    menu: *const NativeMenu,
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
    fn bind(state: &'a mut PopupWindowState, menu: &'a NativeMenu) -> Self {
        state.menu = std::ptr::from_ref(menu);
        Self(state, PhantomData)
    }
}
impl Drop for MenuBindingGuard<'_> {
    fn drop(&mut self) {
        self.0.menu = std::ptr::null();
    }
}

struct TrackingGuard<'a>(&'a AtomicBool);
impl Drop for TrackingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
                    true
                } else {
                    false
                }
            }
        };
        if accepted {
            self.wake_sta();
        }
        accepted
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
            let menu = (*state).menu;
            if !menu.is_null() {
                if let Some(result) = (*menu).handle_menu_msg(message, wparam, lparam) {
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
            WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(class_wide.as_ptr()),
            w!("RustWindowsMenu Popup"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW popup owner failed: {e}"))?;
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
        let ready_hit = take_matching_show(coordinator, cache);
        if let Some((req, entry)) = ready_hit {
            sync_cache_views(shared_host, coordinator, cache, None);
            show_and_invoke(popup_hwnd, win_state, &entry.menu, &req, is_tracking);
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
                }
            }
        }
    }
}

fn take_matching_show(
    coordinator: &Mutex<CoordinatorState>,
    cache: &mut CacheCore<NativeMenu>,
) -> Option<(ShowRequest, CachedEntry<NativeMenu>)> {
    let now = Instant::now();
    let pending = {
        let mut state = coordinator.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state.pending_show.as_ref()?;
        if now.saturating_duration_since(pending.received_at) > MAX_PENDING_SHOW_AGE {
            state.pending_show = None;
            return None;
        }
        if cache.contains(pending.req.key_hash, pending.req.clipboard_seq, now) {
            state.pending_show.take()
        } else {
            None
        }
    }?;
    let entry = cache.take(pending.req.key_hash, pending.req.clipboard_seq, now)?;
    Some((pending.req, entry))
}

fn show_and_invoke(
    popup_hwnd: HWND,
    win_state: &mut PopupWindowState,
    menu: &NativeMenu,
    req: &ShowRequest,
    is_tracking: &AtomicBool,
) {
    is_tracking.store(true, Ordering::Release);
    let _tracking_guard = TrackingGuard(is_tracking);
    let _binding = MenuBindingGuard::bind(win_state, menu);

    unsafe {
        let _ = SetWindowPos(
            popup_hwnd,
            HWND_TOPMOST,
            req.anchor_x,
            req.anchor_y,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOSIZE,
        );
        let _ = SetForegroundWindow(popup_hwnd);
    }

    let selected = menu.track_only(popup_hwnd, (req.anchor_x, req.anchor_y), req.no_animation);
    unsafe {
        let _ = PostMessageW(popup_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    }

    let root = HWND(req.explorer_root_hwnd as *mut c_void);
    let defview = HWND(req.defview_hwnd as *mut c_void);

    match selected {
        Ok(Some(cmd_id)) if cmd_id == CUSTOMIZE_MENU_CMD_ID => {
            restore_foreground(popup_hwnd, root);
            crate::gui::spawn_manager_for_selection(menu.selection());
        }
        Ok(Some(cmd_id)) => {
            if menu
                .canonical_verb_for_id(cmd_id)
                .is_some_and(|v| v.eq_ignore_ascii_case("rename"))
                && !defview.is_invalid()
                && unsafe { IsWindow(defview) }.as_bool()
            {
                restore_foreground(popup_hwnd, root);
                unsafe {
                    let _ = PostMessageW(defview, edit_label_msg_id(), WPARAM(0), LPARAM(0));
                }
            } else if let Err(error) = menu.invoke_id(cmd_id, popup_hwnd, (req.anchor_x, req.anchor_y))
            {
                tracing::debug!(%error, "cached menu InvokeCommand failed");
            }
        }
        Ok(None) => restore_foreground(popup_hwnd, root),
        Err(error) => {
            tracing::debug!(%error, "cached menu TrackPopupMenuEx failed");
            restore_foreground(popup_hwnd, root);
        }
    }
}

fn restore_foreground(popup_hwnd: HWND, explorer_root: HWND) {
    if !explorer_root.is_invalid()
        && unsafe { IsWindow(explorer_root) }.as_bool()
        && unsafe { GetForegroundWindow() } == popup_hwnd
    {
        unsafe {
            let _ = SetForegroundWindow(explorer_root);
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
}
