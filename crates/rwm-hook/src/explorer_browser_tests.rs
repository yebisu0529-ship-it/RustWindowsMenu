//! Drives the attach, menu and detach protocol against an `IExplorerBrowser` owned by this
//! test process. Nothing here touches explorer.exe: the frame window stands in for Explorer's
//! `CabinetWClass` by answering `CWM_GETISHELLBROWSER` the same way.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use rwm_core::shell::{OleGuard, OwnedPidl};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    DefSubclassProc, ExplorerBrowser, IExplorerBrowser, IShellBrowser, IShellView,
    RemoveWindowSubclass, SetWindowSubclass, EBO_NOPERSISTVIEWSTATE, SBSP_ABSOLUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EndMenu, EnumChildWindows,
    KillTimer, PeekMessageW, RegisterClassW, RegisterWindowMessageW, SendMessageTimeoutW, SetTimer,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_REMOVE, SMTO_NORMAL, WH_CALLWNDPROC,
    WM_CONTEXTMENU, WM_ENTERMENULOOP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::shell_view::{self, CWM_GETISHELLBROWSER};
use crate::{rwm_attach_message_id, rwm_call_wnd_proc, rwm_detach_message_id, subclass};

static FRAME_BROWSER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static TRACKED_BY_HOOK: AtomicBool = AtomicBool::new(false);
static DETACH_ON_ENTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static DETACH_ACK: AtomicBool = AtomicBool::new(false);
static TRACKED_DURING_DEFERRED_DETACH: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn frame_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        CWM_GETISHELLBROWSER => LRESULT(FRAME_BROWSER.load(Ordering::Acquire) as isize),
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

/// Ends a menu as its modal loop starts and notes whether the hook was tracking it.
unsafe extern "system" fn end_menu_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    if msg == WM_ENTERMENULOOP {
        TRACKED_BY_HOOK.store(subclass::is_tracking_menu(), Ordering::Release);
        let detach_msg = DETACH_ON_ENTER.swap(0, Ordering::AcqRel);
        if detach_msg != 0 {
            DETACH_ACK.store(send(window, detach_msg, 0, 0) == Some(1), Ordering::Release);
            TRACKED_DURING_DEFERRED_DETACH
                .store(subclass::is_tracking_menu(), Ordering::Release);
        } else {
            let _ = EndMenu();
        }
    }
    DefSubclassProc(window, msg, wparam, lparam)
}

/// Dismisses a menu that never reached `end_menu_proc`, so the test fails instead of hanging.
unsafe extern "system" fn end_menu_later(_: HWND, _: u32, _: usize, _: u32) {
    let _ = EndMenu();
}

unsafe extern "system" fn collect(window: HWND, found: LPARAM) -> BOOL {
    (*(found.0 as *mut Vec<HWND>)).push(window);
    TRUE
}

struct Frame {
    window: HWND,
    browser: IExplorerBrowser,
    _shell_browser: IShellBrowser,
}

impl Frame {
    unsafe fn open(folder: &Path) -> windows::core::Result<Self> {
        let instance = GetModuleHandleW(None)?.into();
        let class_name = w!("CabinetWClass");
        let class = WNDCLASSW {
            lpfnWndProc: Some(frame_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&class);
        let window = CreateWindowExW(
            Default::default(),
            class_name,
            w!("rwm-hook test frame"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            800,
            600,
            None,
            None,
            instance,
            None,
        )?;
        let browser: IExplorerBrowser =
            CoCreateInstance(&ExplorerBrowser, None, CLSCTX_INPROC_SERVER)?;
        browser.Initialize(
            window,
            &RECT {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            None,
        )?;
        browser.SetOptions(EBO_NOPERSISTVIEWSTATE)?;
        let shell_browser: IShellBrowser = browser.cast()?;
        FRAME_BROWSER.store(shell_browser.as_raw(), Ordering::Release);
        let folder = OwnedPidl::parse(folder).expect("folder PIDL");
        browser.BrowseToIDList(folder.as_ptr(), SBSP_ABSOLUTE)?;
        Ok(Self {
            window,
            browser,
            _shell_browser: shell_browser,
        })
    }
}

impl Frame {
    /// The current `SHELLDLL_DefView` and its item list, once both exist.
    fn folder_view(&self) -> Option<(HWND, HWND)> {
        unsafe { self.browser.GetCurrentView::<IShellView>() }.ok()?;
        let mut found = Vec::new();
        let found_ptr = LPARAM(&mut found as *mut Vec<HWND> as isize);
        let _ = unsafe { EnumChildWindows(self.window, Some(collect), found_ptr) };
        let defview = found
            .into_iter()
            .find(|&window| shell_view::is_defview(window))?;
        Some((defview, *shell_view::view_children(defview).first()?))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        FRAME_BROWSER.store(std::ptr::null_mut(), Ordering::Release);
        unsafe {
            let _ = self.browser.Destroy();
            let _ = DestroyWindow(self.window);
        }
    }
}

fn pump_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&message) };
        }
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send(window: HWND, msg: u32, wparam: usize, lparam: isize) -> Option<usize> {
    let (wparam, lparam, mut result) = (WPARAM(wparam), LPARAM(lparam), 0);
    let sent = unsafe {
        SendMessageTimeoutW(
            window,
            msg,
            wparam,
            lparam,
            SMTO_NORMAL,
            5000,
            Some(&mut result),
        )
    };
    (sent.0 != 0).then_some(result)
}

/// Raises a keyboard menu on `list` and reports whether the hook built and tracked it.
fn tracks_keyboard_menu(defview: HWND, list: HWND) -> bool {
    TRACKED_BY_HOOK.store(false, Ordering::Release);
    let _ = unsafe { SetWindowSubclass(defview, Some(end_menu_proc), 0x7E57, 0) };
    let safety = unsafe { SetTimer(None, 0, 3000, Some(end_menu_later)) };
    let handled = send(list, WM_CONTEXTMENU, list.0 as usize, -1) == Some(0);
    let _ = unsafe { KillTimer(None, safety) };
    let _ = unsafe { RemoveWindowSubclass(defview, Some(end_menu_proc), 0x7E57) };
    handled && TRACKED_BY_HOOK.load(Ordering::Acquire)
}

#[test]
#[ignore = "hosts an IExplorerBrowser window on the local desktop"]
fn protocol_round_trip_on_an_owned_explorer_browser() {
    let _ole = OleGuard::initialize().expect("OleInitialize");
    // A config file that does not exist means the defaults, whatever this machine has set.
    std::env::set_var(
        "RWM_CONFIG",
        std::env::temp_dir().join("rwm-hook-test-no-config.toml"),
    );
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root");
    let frame = unsafe { Frame::open(repo) }.expect("open the test frame");
    let (defview, child) = pump_until("the folder view", || frame.folder_view());
    let thread = unsafe { GetCurrentThreadId() };
    let hook = unsafe { SetWindowsHookExW(WH_CALLWNDPROC, Some(rwm_call_wnd_proc), None, thread) }
        .expect("hook this thread");
    let (attach, detach) = (rwm_attach_message_id(), rwm_detach_message_id());
    let any_build = unsafe { RegisterWindowMessageW(w!("RustWindowsMenu.DetachHook")) };

    assert_eq!(
        send(defview, attach, 0, 0),
        Some(1),
        "attach is acknowledged"
    );
    assert_eq!(
        send(defview, attach, 0, 0),
        Some(1),
        "a repeated attach is acknowledged"
    );
    assert_eq!(
        send(child, attach, 0, 0),
        Some(1),
        "the item list is subclassed too"
    );
    assert_eq!(subclass::attached_view_count(), 1);

    assert!(
        tracks_keyboard_menu(defview, child),
        "the hook builds and tracks the item list's menu"
    );

    assert_eq!(
        send(defview, detach, 0, 0),
        Some(1),
        "detach is acknowledged"
    );
    assert_eq!(subclass::attached_view_count(), 0);
    assert_eq!(
        send(defview, detach, 0, 0),
        Some(0),
        "a detached view stops answering"
    );
    assert_eq!(
        send(child, attach, 0, 0),
        Some(0),
        "and so does its item list"
    );

    assert_eq!(send(defview, attach, 0, 0), Some(1));
    assert_eq!(
        send(child, any_build, 0, 0),
        Some(1),
        "the unversioned detach reaches any build"
    );
    assert_eq!(subclass::attached_view_count(), 0);

    assert_eq!(send(defview, attach, 0, 0), Some(1));
    let crates = OwnedPidl::parse(repo.join("crates")).expect("crates PIDL");
    unsafe { frame.browser.BrowseToIDList(crates.as_ptr(), SBSP_ABSOLUTE) }.expect("navigate");
    let (next, next_list) = pump_until(
        "navigation to attach the new view and release the old one",
        || {
            // Only an item list that is already subclassed acknowledges, so this probe cannot
            // attach anything itself.
            let (view, list) = frame.folder_view().filter(|&(view, _)| view != defview)?;
            let adopted = send(list, attach, 0, 0) == Some(1);
            (adopted && subclass::attached_view_count() == 1).then_some((view, list))
        },
    );
    assert!(
        tracks_keyboard_menu(next, next_list),
        "a view created by navigation works like an attached one"
    );

    // Send detach synchronously inside TrackPopupMenuEx and confirm teardown waits until menu drop.
    DETACH_ACK.store(false, Ordering::Release);
    TRACKED_DURING_DEFERRED_DETACH.store(false, Ordering::Release);
    DETACH_ON_ENTER.store(detach, Ordering::Release);
    assert!(
        tracks_keyboard_menu(next, next_list),
        "re-entrant detach unwinds menu tracking safely"
    );
    assert!(DETACH_ACK.load(Ordering::Acquire));
    assert!(
        TRACKED_DURING_DEFERRED_DETACH.load(Ordering::Acquire),
        "owner subclass and pins must remain live until menu drops"
    );
    assert_eq!(subclass::attached_view_count(), 0);
    assert_eq!(
        send(next, detach, 0, 0),
        Some(0),
        "deferred detach finished once tracking unwound"
    );
    assert_eq!(send(next, attach, 0, 0), Some(1));

    unsafe { UnhookWindowsHookEx(hook) }.expect("unhook while the view is still attached");
    drop(frame);
    pump_until("WM_NCDESTROY to release the view", || {
        (subclass::attached_view_count() == 0).then_some(())
    });
}
