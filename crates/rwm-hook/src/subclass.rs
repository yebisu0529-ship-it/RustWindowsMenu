//! Per-thread attachment state and the subclass procedure shared by `SHELLDLL_DefView` and its
//! item list (`DirectUIHWND` in File Explorer, `SysListView32` on the desktop). Default
//! processing, COM calls and menu tracking all re-enter this procedure, so the state is
//! borrowed only for bookkeeping and menus are dropped after the borrow ends.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem;
use std::time::Instant;

use rwm_core::config::RwmConfig;
use rwm_core::shell::{NativeMenu, SelectionKey, ShellError};
use rwm_core::warm_pool::WarmDllPin;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_UPDATENOW};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_RBUTTON, VK_SHIFT,
};
// Not GetWindowSubclass: System32's comctl32 5.82 does not export it by name, so importing it
// would stop this DLL loading in any process without a common controls v6 manifest.
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    EndMenu, GetParent, GetSystemMetrics, IsChild, KillTimer, PostMessageW, SetTimer, SM_CXDRAG,
    SM_CYDRAG, WM_CANCELMODE, WM_CAPTURECHANGED, WM_CONTEXTMENU, WM_CREATE, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MEASUREITEM, WM_MENUCHAR,
    WM_MOUSEMOVE, WM_NCDESTROY, WM_PARENTNOTIFY, WM_RBUTTONDOWN, WM_TIMER,
};

use crate::gesture::{self, Gesture, Hit, READY_TTL};
use crate::settings::ConfigCache;
use crate::shell_view::{self, ShellView};
use crate::{Protocol, FAULT};

pub(crate) const VIEW_SUBCLASS_ID: usize = 0x5257_4D01;
pub(crate) const CHILD_SUBCLASS_ID: usize = 0x5257_4D02;
const ARM_TIMER_BASE: usize = 0x5257_4D00;
const READY_TIMER_BASE: usize = 0x5257_4E00;
const TIMER_SLOTS: usize = 256;

fn timer_id_for(base: usize, gen: u64) -> usize {
    base + (gen as usize & (TIMER_SLOTS - 1))
}

fn in_timer_band(id: usize, base: usize) -> bool {
    (base..base + TIMER_SLOTS).contains(&id)
}

struct View {
    defview: HWND,
    gesture: Gesture<NativeMenu>,
    gen: u64,
}

#[derive(Default)]
struct ThreadState {
    views: Vec<View>,
    warm: Option<WarmDllPin>,
    deferred_warm: Option<WarmDllPin>,
    deferred_detach: Vec<HWND>,
    config: ConfigCache,
    /// Between an attach and a detach, views and item lists this thread creates are adopted as
    /// they appear: navigation replaces the `SHELLDLL_DefView`, and the new view's item list
    /// is created later without announcing itself through `WM_PARENTNOTIFY`.
    adopt_new_views: bool,
    seq: u64,
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        // Only thread exit gets here, under the loader lock, where COM releases and FreeLibrary
        // can deadlock. Detach and WM_NCDESTROY have normally emptied these already.
        if !self.views.is_empty() {
            mem::forget(mem::take(&mut self.views));
        }
        if let Some(warm) = self.warm.take() {
            mem::forget(warm);
        }
        if let Some(warm) = self.deferred_warm.take() {
            mem::forget(warm);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Building,
    Tracking {
        owner: HWND,
        menu: *const NativeMenu,
    },
}

thread_local! {
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState::default());
    static PHASE: Cell<Phase> = const { Cell::new(Phase::Idle) };
    static DECLINED_CHILD: Cell<usize> = const { Cell::new(0) };
}

/// One menu build or tracked menu per thread, so a nested `WM_CONTEXTMENU` goes to Explorer.
/// The guard borrows the tracked menu, so the pointer in `PHASE` cannot outlive it.
struct PhaseGuard<'menu>(PhantomData<&'menu NativeMenu>);

impl<'menu> PhaseGuard<'menu> {
    fn enter(next: Phase) -> Option<Self> {
        let entered = PHASE.try_with(|phase| {
            let idle = phase.get() == Phase::Idle;
            if idle {
                phase.set(next);
            }
            idle
        });
        // Not `then_some`: it builds the guard eagerly, and dropping that guard resets the phase.
        match entered {
            Ok(true) => Some(Self(PhantomData)),
            _ => None,
        }
    }

    fn tracking(owner: HWND, menu: &'menu NativeMenu) -> Option<Self> {
        Self::enter(Phase::Tracking { owner, menu })
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        let _ = PHASE.try_with(|phase| phase.set(Phase::Idle));
    }
}

fn phase() -> Phase {
    PHASE.try_with(Cell::get).unwrap_or(Phase::Building)
}

fn with_state<R>(f: impl FnOnce(&mut ThreadState) -> R) -> Option<R> {
    let state = STATE.try_with(|state| state.try_borrow_mut().ok().map(|mut state| f(&mut state)));
    state.ok().flatten()
}

fn with_view<R>(defview: HWND, f: impl FnOnce(&mut View) -> R) -> Option<R> {
    with_state(|state| {
        state
            .views
            .iter_mut()
            .find(|view| view.defview == defview)
            .map(f)
    })
    .flatten()
}

fn kill_gesture_timers(gesture: &Gesture<NativeMenu>) {
    if let Some((child, timer_id)) = gesture.armed_timer() {
        let _ = unsafe { KillTimer(child, timer_id) };
    }
    if let Some((child, timer_id)) = gesture.ready_timer() {
        let _ = unsafe { KillTimer(child, timer_id) };
    }
}

/// Installs `next` and returns the previous gesture with its timers killed, for the caller
/// to drop after the borrow.
fn swap_gesture(defview: HWND, next: Gesture<NativeMenu>) -> Gesture<NativeMenu> {
    let mut next = Some(next);
    let previous = with_state(|state| {
        state.seq = state.seq.wrapping_add(1).max(1);
        let next_gen = state.seq;
        state
            .views
            .iter_mut()
            .find(|view| view.defview == defview)
            .map(|view| {
                view.gen = next_gen;
                mem::replace(&mut view.gesture, next.take().unwrap_or_default())
            })
    })
    .flatten()
    .unwrap_or_default();
    kill_gesture_timers(&previous);
    previous
}

fn reset(defview: HWND) {
    drop(swap_gesture(defview, Gesture::Idle));
}

fn set_adopting(adopt: bool) {
    with_state(|state| state.adopt_new_views = adopt);
}

fn config() -> RwmConfig {
    with_state(|state| state.config.current(Instant::now()).clone()).unwrap_or_default()
}

fn key_down(key: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(i32::from(key.0)) < 0 }
}

/// Disabled in the config, or Ctrl+Shift held to get Explorer's own menu.
fn bypassed(config: &RwmConfig) -> bool {
    !config.enabled || (key_down(VK_CONTROL) && key_down(VK_SHIFT))
}

/// The hook sees the attach message before the window does, so the subclass is in place when
/// Windows dispatches that same message to it.
pub(crate) fn on_hooked_message(window: HWND, msg: u32) {
    match crate::protocol_message(msg) {
        Some(Protocol::Attach) if shell_view::is_defview(window) => attach(window),
        Some(Protocol::Detach) => set_adopting(false),
        None if msg == WM_CREATE && with_state(|state| state.adopt_new_views) == Some(true) => {
            created(window)
        }
        _ => {}
    }
}

fn created(window: HWND) {
    if shell_view::is_defview(window) {
        if shell_view::has_explorer_frame(window) {
            attach(window);
        }
    } else if let Ok(defview) = unsafe { GetParent(window) } {
        if shell_view::child_kind(window).is_some() && with_view(defview, |_| ()).is_some() {
            adopt_later(defview);
        }
    }
}

/// DirectUI replaces an item list's window procedure once creation finishes, without chaining
/// to whatever was there, so a subclass installed during creation is silently cut off. The
/// view adopts its item lists when this posted attach message comes back around instead.
fn adopt_later(defview: HWND) {
    let _ = unsafe { PostMessageW(defview, crate::attach_id(), WPARAM(0), LPARAM(0)) };
}

fn attach(defview: HWND) {
    let installed = crate::pin_module_for_subclassing()
        && unsafe { SetWindowSubclass(defview, Some(subclass_proc), VIEW_SUBCLASS_ID, 0) }
            .as_bool();
    if installed {
        with_state(|state| {
            state.adopt_new_views = true;
            state.deferred_detach.retain(|&hw| hw != defview);
            if !state.views.iter().any(|view| view.defview == defview) {
                state.seq = state.seq.wrapping_add(1).max(1);
                let gen = state.seq;
                let gesture = Gesture::Idle;
                state.views.push(View {
                    defview,
                    gesture,
                    gen,
                });
            }
        });
        let _ = adopt_children(defview);
    }
}

/// Idempotent: subclassing an item list again only refreshes its reference data. Returns `false`
/// if any discovered child failed subclass installation.
fn adopt_children(defview: HWND) -> bool {
    let mut all_ok = true;
    for child in shell_view::view_children(defview) {
        let data = defview.0 as usize;
        if !unsafe { SetWindowSubclass(child, Some(subclass_proc), CHILD_SUBCLASS_ID, data) }
            .as_bool()
        {
            all_ok = false;
        }
    }
    all_ok
}

fn detach(defview: HWND) {
    for child in shell_view::view_children(defview) {
        let _ = unsafe { RemoveWindowSubclass(child, Some(subclass_proc), CHILD_SUBCLASS_ID) };
    }
    let _ = unsafe { RemoveWindowSubclass(defview, Some(subclass_proc), VIEW_SUBCLASS_ID) };
    forget(defview, phase() != Phase::Idle);
}

/// Drops the view's state. When the last view leaves while a menu is still building or
/// tracking, `WarmDllPin` release is deferred until that menu drops so no handler DLL unloads
/// under a live menu interface.
fn forget(defview: HWND, defer_warm: bool) {
    let (view, warm, displaced_deferred) = with_state(|state| {
        state.deferred_detach.retain(|&hw| hw != defview);
        let index = state.views.iter().position(|view| view.defview == defview);
        let view = index.map(|index| state.views.swap_remove(index));
        if state.views.is_empty() {
            state.views = Vec::new();
        }
        let warm = state.views.is_empty().then(|| state.warm.take()).flatten();
        if defer_warm {
            if let Some(pin) = warm {
                let old = state.deferred_warm.replace(pin);
                return (view, None, old);
            }
            return (view, None, None);
        }
        (view, warm, None)
    })
    .unwrap_or_default();
    if let Some(ref view) = view {
        kill_gesture_timers(&view.gesture);
    }
    drop(view);
    drop(displaced_deferred);
    drop(warm);
}

fn drain_deferred_cleanup() {
    if phase() != Phase::Idle {
        return;
    }
    let pending_views =
        with_state(|state| mem::take(&mut state.deferred_detach)).unwrap_or_default();
    for defview in pending_views {
        detach(defview);
    }
    let warm = with_state(|state| {
        let deferred = state.deferred_warm.take();
        let orphan = state.views.is_empty().then(|| state.warm.take()).flatten();
        (deferred, orphan)
    });
    drop(warm);
}

/// # Safety
/// Called by comctl32 for windows subclassed with this procedure. For an item list, `data` is
/// its `SHELLDLL_DefView`.
pub(crate) unsafe extern "system" fn subclass_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let defview = if id == VIEW_SUBCLASS_ID {
        window
    } else {
        HWND(data as *mut c_void)
    };
    match FAULT.contain(|| route(window, defview, id, msg, wparam, lparam)) {
        Some(Some(result)) => result,
        other => {
            let declined = other.is_some()
                && id == CHILD_SUBCLASS_ID
                && msg == WM_CONTEXTMENU
                && wparam.0 == window.0 as usize;
            if declined {
                let _ = DECLINED_CHILD.try_with(|c| c.set(window.0 as usize));
            }
            let result = DefSubclassProc(window, msg, wparam, lparam);
            if declined {
                let _ = DECLINED_CHILD.try_with(|c| c.set(0));
            }
            result
        }
    }
}

fn route(
    window: HWND,
    defview: HWND,
    id: usize,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let is_view = id == VIEW_SUBCLASS_ID;
    if msg == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(window, Some(subclass_proc), id) };
        if is_view {
            forget(defview, phase() != Phase::Idle);
        } else if with_view(defview, |view| {
            view.gesture.armed_timer().is_some_and(|(c, _)| c == window)
                || view.gesture.ready_timer().is_some_and(|(c, _)| c == window)
        }) == Some(true)
        {
            reset(defview);
        }
        return None;
    }
    match crate::protocol_message(msg) {
        Some(Protocol::Detach) => {
            set_adopting(false);
            if matches!(phase(), Phase::Tracking { owner, .. } if owner == defview) {
                unsafe {
                    let _ = EndMenu();
                }
                with_state(|state| {
                    if !state.deferred_detach.contains(&defview) {
                        state.deferred_detach.push(defview);
                    }
                });
            } else {
                detach(defview);
            }
            return Some(LRESULT(1));
        }
        _ if FAULT.is_tripped() => return None,
        Some(Protocol::Attach) => {
            if is_view {
                return Some(LRESULT(isize::from(adopt_children(defview))));
            }
            return Some(LRESULT(1));
        }
        None => {}
    }
    match (is_view, msg) {
        (true, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) => match phase() {
            // SAFETY: `PhaseGuard::tracking` borrows `menu` and binds `owner == defview`.
            Phase::Tracking { owner, menu } if owner == defview => {
                unsafe { &*menu }.handle_menu_msg(msg, wparam, lparam)
            }
            _ => None,
        },
        // Sent before CreateWindowEx returns, so adoption waits like the hook's WM_CREATE path.
        (true, WM_PARENTNOTIFY) if wparam.0 & 0xFFFF == WM_CREATE as usize => {
            adopt_later(defview);
            None
        }
        (false, WM_RBUTTONDOWN) => Some(right_button_down(window, defview, msg, wparam, lparam)),
        (false, WM_MOUSEMOVE) => {
            let drag = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
            let at = gesture::point_from_lparam(lparam.0);
            if with_view(defview, |view| view.gesture.is_drag(at, drag)) == Some(true) {
                reset(defview);
            }
            None
        }
        (false, WM_TIMER) if in_timer_band(wparam.0, ARM_TIMER_BASE) => {
            arm_timer_fired(window, defview, wparam.0);
            Some(LRESULT(0))
        }
        (false, WM_TIMER) if in_timer_band(wparam.0, READY_TIMER_BASE) => {
            let _ = unsafe { KillTimer(window, wparam.0) };
            let expired = with_view(defview, |view| view.gesture.expire(window, wparam.0)).flatten();
            drop(expired);
            Some(LRESULT(0))
        }
        // Explorer releases capture on the button-up before WM_CONTEXTMENU, so this cancels
        // only a pending arm; a prebuilt menu stays for that message.
        (false, WM_CAPTURECHANGED | WM_CANCELMODE) => {
            if with_view(defview, |view| view.gesture.armed_timer().is_some()) == Some(true) {
                reset(defview);
            }
            None
        }
        (false, WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_KEYDOWN) => {
            reset(defview);
            None
        }
        (false, WM_CONTEXTMENU) if wparam.0 == window.0 as usize => {
            context_menu(defview, window, lparam)
        }
        (true, WM_CONTEXTMENU) => {
            let declined = DECLINED_CHILD.try_with(Cell::get).unwrap_or(0);
            if declined != 0 && wparam.0 == declined {
                let _ = DECLINED_CHILD.try_with(|c| c.set(0));
                return None;
            }
            let source = if wparam.0 == window.0 as usize {
                Some(window)
            } else if wparam.0 != 0 {
                let child = HWND(wparam.0 as *mut c_void);
                (shell_view::child_kind(child).is_some()
                    && unsafe { IsChild(defview, child) }.as_bool())
                .then_some(child)
            } else {
                None
            };
            source.and_then(|source| context_menu(defview, source, lparam))
        }
        _ => None,
    }
}

fn right_button_down(
    child: HWND,
    defview: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let stale = swap_gesture(defview, Gesture::Idle);
    // Explorer moves the selection and focus on the press itself, so it goes first.
    let result = unsafe { DefSubclassProc(child, msg, wparam, lparam) };
    drop(stale);
    FAULT.run(|| {
        let config = config();
        // The desktop list view tracks the press in a modal loop, so the button can already be
        // up, and the menu already shown, when default processing returns.
        let arm = config.prebuild_on_rbuttondown && !bypassed(&config) && key_down(VK_RBUTTON);
        if arm && phase() == Phase::Idle {
            let down = gesture::point_from_lparam(lparam.0);
            let armed_timer = with_state(|state| {
                state.seq = state.seq.wrapping_add(1).max(1);
                let gen = state.seq;
                let timer_id = timer_id_for(ARM_TIMER_BASE, gen);
                let old = state
                    .views
                    .iter_mut()
                    .find(|view| view.defview == defview)
                    .map(|view| {
                        view.gen = gen;
                        mem::replace(
                            &mut view.gesture,
                            Gesture::Armed {
                                child,
                                down,
                                timer_id,
                                gen,
                            },
                        )
                    });
                old.map(|old| (old, timer_id))
            })
            .flatten();
            if let Some((old, timer_id)) = armed_timer {
                kill_gesture_timers(&old);
                drop(old);
                let delay = config.arm_delay_ms.clamp(1, 100);
                if unsafe { SetTimer(child, timer_id, delay, None) } == 0 {
                    reset(defview);
                }
            }
        }
    });
    result
}

/// Builds the menu while the button is still held. Until `WM_RBUTTONUP` Explorer still
/// reports the old selection for a press on empty space, so the press point is hit tested.
fn arm_timer_fired(child: HWND, defview: HWND, timer_id: usize) -> Option<()> {
    let _ = unsafe { KillTimer(child, timer_id) };
    let _building = PhaseGuard::enter(Phase::Building)?;
    let (down, gen) = with_view(defview, |view| view.gesture.fire(child, timer_id))??;
    let config = config();
    if bypassed(&config) || !key_down(VK_RBUTTON) {
        return None;
    }
    let screen = shell_view::client_to_screen(child, down)?;
    let background = match shell_view::hit_test(child, shell_view::child_kind(child)?, screen)? {
        Hit::Item => false,
        Hit::Background => true,
        Hit::Other => return None,
    };
    let view = ShellView::find(defview)?;
    let mut built = build_menu_in_phase(child, &view, background, key_down(VK_SHIFT), &config);
    let ready_timer_id = timer_id_for(READY_TIMER_BASE, gen);
    // Reject publication if anything reset `view.gen`, detached, or replaced the gesture while
    // COM extensions ran; drop any rejected `NativeMenu` outside the `RefCell` borrow.
    let published = with_state(|state| {
        if state.deferred_detach.contains(&defview) {
            return false;
        }
        let Some(view) = state.views.iter_mut().find(|v| v.defview == defview) else {
            return false;
        };
        if view.gen != gen || !matches!(view.gesture, Gesture::Idle) {
            return false;
        }
        if let Some(menu) = built.take() {
            view.gesture = Gesture::Ready {
                child,
                down,
                menu,
                built_at: Instant::now(),
                timer_id: ready_timer_id,
            };
            return true;
        }
        false
    })
    .unwrap_or(false);
    drop(built);
    if published {
        let ttl = READY_TTL.as_millis().min(u128::from(u32::MAX)) as u32;
        let _ = unsafe { SetTimer(child, ready_timer_id, ttl, None) };
    }
    None
}

fn build_menu_in_phase(
    source: HWND,
    view: &ShellView,
    background: bool,
    shift: bool,
    config: &RwmConfig,
) -> Option<NativeMenu> {
    // Paints the new selection before Shell extensions block this thread.
    let _ = unsafe { RedrawWindow(source, None, None, RDW_UPDATENOW) };
    let before = config
        .pin_handler_dlls
        .then(WarmDllPin::snapshot_loaded_modules);
    let menu =
        NativeMenu::build_from_view(&view.view, view.folder_view.as_ref(), background, shift);
    let menu = menu.ok()?;
    if let Some(before) = before {
        with_state(|state| {
            let warm = state
                .warm
                .get_or_insert_with(|| WarmDllPin::new(config.max_pinned_dlls));
            warm.pin_newly_loaded(&before)
        });
    }
    Some(menu)
}

/// Shows the native menu for one `WM_CONTEXTMENU`. `None` leaves the message to Explorer.
fn context_menu(defview: HWND, source: HWND, lparam: LPARAM) -> Option<LRESULT> {
    let prebuilt = swap_gesture(defview, Gesture::Idle).into_fresh_menu(Instant::now());
    let config = config();
    if bypassed(&config) {
        return None;
    }
    let point = gesture::context_menu_point(lparam.0);
    let kind = shell_view::child_kind(source);
    let prepared = (|| -> Option<(NativeMenu, windows::Win32::Foundation::POINT)> {
        let building = PhaseGuard::enter(Phase::Building)?;
        let hit = match (point, kind) {
            (Some(point), Some(kind)) if shell_view::in_client_area(source, point) => {
                // Fail open to Explorer when a mouse hit inside client area cannot be resolved.
                Some(shell_view::hit_test(source, kind, point)?)
            }
            (Some(_), Some(_)) => return None,
            _ => None,
        };
        if hit == Some(Hit::Other) {
            return None;
        }
        let view = ShellView::find(defview)?;
        // Explorer has applied the click by now, so an empty selection means the background.
        let background = hit == Some(Hit::Background) || view.selection_count() == 0;
        let shift = key_down(VK_SHIFT);
        let key = SelectionKey::capture_from_view(
            &view.view,
            view.folder_view.as_ref(),
            background,
            shift,
        )
        .ok()?;
        let menu = match prebuilt.filter(|menu| menu.selection().matches(&key)) {
            Some(menu) => menu,
            None => build_menu_in_phase(source, &view, background, shift, &config)?,
        };
        let still_attached = with_state(|state| {
            !state.deferred_detach.contains(&defview)
                && state.views.iter().any(|v| v.defview == defview)
        })
        .unwrap_or(false);
        if !still_attached {
            return None;
        }
        let anchor = point.unwrap_or_else(|| {
            let focus = kind.and_then(|kind| shell_view::focused_item_center(source, kind));
            gesture::keyboard_anchor(focus, shell_view::window_rect(defview))
        });
        drop(building);
        Some((menu, anchor))
    })();

    let outcome = if let Some((menu, anchor)) = prepared {
        let tracking = PhaseGuard::tracking(defview, &menu);
        let result = tracking.map(|_tracking| {
            menu.track_and_invoke(defview, (anchor.x, anchor.y), config.no_animation)
        });
        drop(menu);
        match result {
            Some(Err(ShellError::Windows(_))) | None => None,
            Some(Err(error)) => {
                tracing::debug!(%error, "the chosen menu command failed");
                Some(LRESULT(0))
            }
            Some(Ok(_)) => Some(LRESULT(0)),
        }
    } else {
        None
    };
    drain_deferred_cleanup();
    outcome
}

#[cfg(test)]
pub(crate) fn attached_view_count() -> usize {
    with_state(|state| state.views.len()).unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn is_tracking_menu() -> bool {
    matches!(phase(), Phase::Tracking { .. })
}

#[cfg(test)]
#[test]
fn one_menu_phase_at_a_time_per_thread() {
    let building = PhaseGuard::enter(Phase::Building).expect("an idle thread can build");
    assert!(
        PhaseGuard::enter(Phase::Building).is_none(),
        "no nested build"
    );
    assert!(
        phase() == Phase::Building,
        "a refused entry leaves the phase alone"
    );
    drop(building);
    assert!(phase() == Phase::Idle && !is_tracking_menu());

    let owner_a = HWND(0x1001usize as *mut _);
    let owner_b = HWND(0x2002usize as *mut _);
    let menu_ptr = std::ptr::NonNull::<NativeMenu>::dangling().as_ptr();
    let tracking = PhaseGuard::enter(Phase::Tracking {
        owner: owner_a,
        menu: menu_ptr,
    })
    .expect("can enter owner-scoped tracking");
    assert!(matches!(phase(), Phase::Tracking { owner, .. } if owner == owner_a));
    assert!(!matches!(phase(), Phase::Tracking { owner, .. } if owner == owner_b));
    drop(tracking);
    assert!(phase() == Phase::Idle);
}
