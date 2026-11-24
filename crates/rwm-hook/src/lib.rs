//! `rwm_hook.dll`, the in-process half of RustWindowsMenu. `rwm run` hooks Explorer UI
//! threads with [`rwm_call_wnd_proc`] and sends the attach message to each `SHELLDLL_DefView`.
//! The DLL then subclasses the view and shows the native Shell menu itself, prebuilding it
//! while the right button is held. It never starts a thread.

mod gesture;
mod settings;
mod shell_view;
mod subclass;

#[cfg(test)]
mod explorer_browser_tests;

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use rwm_core::build_info::{ABI_VERSION, BUILD_ID};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HMODULE, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleExW,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, RegisterWindowMessageW, CWPSTRUCT, WM_CREATE,
};

static DLL_INSTANCE: AtomicUsize = AtomicUsize::new(0);
static IS_EXPLORER_PROCESS: AtomicBool = AtomicBool::new(false);
static MODULE_PINNED: AtomicBool = AtomicBool::new(false);
static ATTACH_ID: AtomicU32 = AtomicU32::new(0);
static DETACH_ID: AtomicU32 = AtomicU32::new(0);
static ANY_BUILD_DETACH_ID: AtomicU32 = AtomicU32::new(0);
pub(crate) static FAULT: Breaker = Breaker(AtomicBool::new(false));

/// Sticky circuit breaker: after the first panic in any entry point, every later call fails
/// open to Explorer's own behavior. There is no reset, so an attach message cannot revive
/// code that already failed inside Explorer.
pub(crate) struct Breaker(AtomicBool);

impl Breaker {
    pub(crate) fn is_tripped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Runs `body` unless tripped. `None` tells the caller to fail open.
    pub(crate) fn run<R>(&self, body: impl FnOnce() -> R) -> Option<R> {
        (!self.is_tripped()).then(|| self.contain(body)).flatten()
    }

    /// Runs `body` even when tripped, for cleanup that must still happen.
    pub(crate) fn contain<R>(&self, body: impl FnOnce() -> R) -> Option<R> {
        let outcome = catch_unwind(AssertUnwindSafe(body));
        outcome
            .map_err(|_| self.0.store(true, Ordering::Release))
            .ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    Attach,
    Detach,
}

/// Ordinary traffic is below the registered-message range and returns at the first compare.
pub(crate) fn protocol_message(msg: u32) -> Option<Protocol> {
    if msg < 0xC000 {
        None
    } else if msg == attach_id() {
        Some(Protocol::Attach)
    } else if msg == detach_id() || msg == any_build_detach_id() {
        Some(Protocol::Detach)
    } else {
        None
    }
}

pub(crate) fn attach_id() -> u32 {
    registered(&ATTACH_ID, || {
        format!("RustWindowsMenu.AttachHook.{BUILD_ID}")
    })
}

fn detach_id() -> u32 {
    registered(&DETACH_ID, || {
        format!("RustWindowsMenu.DetachHook.{BUILD_ID}")
    })
}

/// `rwm stop` broadcasts this unversioned name so an orphaned hook of any build detaches.
fn any_build_detach_id() -> u32 {
    registered(&ANY_BUILD_DETACH_ID, || {
        "RustWindowsMenu.DetachHook".to_owned()
    })
}

fn registered(cache: &AtomicU32, name: impl FnOnce() -> String) -> u32 {
    let mut id = cache.load(Ordering::Relaxed);
    if id == 0 {
        let wide: Vec<u16> = name().encode_utf16().chain([0]).collect();
        id = unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) };
        cache.store(id, Ordering::Relaxed);
    }
    id
}

pub(crate) fn is_explorer_process() -> bool {
    IS_EXPLORER_PROCESS.load(Ordering::Acquire)
}

/// Pins the DLL in explorer.exe before its first subclass exists. A host that dies without
/// detaching takes its hooks along, and Windows would then unmap code that subclassed
/// Explorer windows still call. Other processes (tests, `rwm` probes) need no pin.
pub(crate) fn pin_module_for_subclassing() -> bool {
    if !is_explorer_process() || MODULE_PINNED.load(Ordering::Acquire) {
        return true;
    }
    let base = PCWSTR(DLL_INSTANCE.load(Ordering::Acquire) as *const u16);
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN;
    let mut module = HMODULE::default();
    let pinned = !base.is_null() && unsafe { GetModuleHandleExW(flags, base, &mut module) }.is_ok();
    MODULE_PINNED.store(pinned, Ordering::Release);
    pinned
}

pub(crate) fn ascii_eq_ignore_case(units: &[u16], ascii: &str) -> bool {
    units.len() == ascii.len()
        && units.iter().zip(ascii.bytes()).all(|(&unit, byte)| {
            u8::try_from(unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(&byte))
        })
}

fn is_explorer_image(path: &[u16]) -> bool {
    let suffix = r"\explorer.exe";
    let start = path.len().checked_sub(suffix.len());
    start.is_some_and(|start| ascii_eq_ignore_case(&path[start..], suffix))
}

/// Runs an exported function's body behind the breaker; `fallback` answers after a panic.
fn ffi<R>(fallback: impl FnOnce() -> R, body: impl FnOnce() -> R) -> R {
    FAULT.contain(body).unwrap_or_else(fallback)
}

/// Records two facts without locks, heap, COM or I/O, which the loader lock forbids here.
///
/// # Safety
/// Called only by the Windows loader.
#[no_mangle]
pub unsafe extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        ffi(
            || (),
            || {
                DLL_INSTANCE.store(instance.0 as usize, Ordering::Release);
                let mut path = [0u16; 1024];
                let len = GetModuleFileNameW(None, &mut path) as usize;
                let explorer = path.get(..len).is_some_and(is_explorer_image);
                IS_EXPLORER_PROCESS.store(explorer, Ordering::Release);
                let _ = DisableThreadLibraryCalls(HMODULE(instance.0));
            },
        );
    }
    TRUE
}

/// # Safety
/// `out_len` must be null or valid for writing one `usize`.
#[no_mangle]
pub unsafe extern "C" fn rwm_build_id(out_len: *mut usize) -> *const u8 {
    let write_len = || out_len.as_mut().map(|out_len| *out_len = BUILD_ID.len());
    ffi(|| None, write_len);
    BUILD_ID.as_ptr()
}

#[no_mangle]
pub extern "C" fn rwm_abi_version() -> u32 {
    ffi(|| ABI_VERSION, || ABI_VERSION)
}

#[no_mangle]
pub extern "C" fn rwm_attach_message_id() -> u32 {
    ffi(|| 0, attach_id)
}

#[no_mangle]
pub extern "C" fn rwm_detach_message_id() -> u32 {
    ffi(|| 0, detach_id)
}

/// `WH_CALLWNDPROC` hook. Only `WM_CREATE` and registered messages reach the breaker-guarded
/// handler; everything else costs two compares.
///
/// # Safety
/// Called by Windows with a `CWPSTRUCT` pointer in `lparam` whenever `code >= 0`.
#[no_mangle]
pub unsafe extern "system" fn rwm_call_wnd_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let message = &*(lparam.0 as *const CWPSTRUCT);
        if message.message == WM_CREATE || message.message >= 0xC000 {
            FAULT.run(|| subclass::on_hooked_message(message.hwnd, message.message));
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// `WH_GETMESSAGE` hook kept for the host contract; it never looks at messages.
///
/// # Safety
/// Called by Windows as a hook procedure.
#[no_mangle]
pub unsafe extern "system" fn rwm_get_msg_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    ffi(|| LRESULT(0), || CallNextHookEx(None, code, wparam, lparam))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    fn register(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) }
    }

    #[test]
    fn exports_identify_this_build_and_its_messages() {
        let mut len = 0;
        let id = unsafe { rwm_build_id(&mut len) };
        assert_eq!(
            unsafe { std::slice::from_raw_parts(id, len) },
            BUILD_ID.as_bytes()
        );
        assert_eq!(unsafe { rwm_build_id(std::ptr::null_mut()) }, id);
        assert_eq!(rwm_abi_version(), 2);
        let (attach, detach) = (rwm_attach_message_id(), rwm_detach_message_id());
        assert_eq!(
            attach,
            register(&format!("RustWindowsMenu.AttachHook.{BUILD_ID}"))
        );
        assert_eq!(
            detach,
            register(&format!("RustWindowsMenu.DetachHook.{BUILD_ID}"))
        );
        assert!(attach >= 0xC000 && detach >= 0xC000 && attach != detach);
        assert_eq!(protocol_message(attach), Some(Protocol::Attach));
        assert_eq!(protocol_message(detach), Some(Protocol::Detach));
        let any_build = register("RustWindowsMenu.DetachHook");
        assert_eq!(protocol_message(any_build), Some(Protocol::Detach));
        assert_eq!(
            protocol_message(register("RustWindowsMenu.Unrelated")),
            None
        );
        assert_eq!(protocol_message(WM_CREATE), None);
    }

    #[test]
    fn the_breaker_trips_on_the_first_panic_and_never_resets() {
        let breaker = Breaker(AtomicBool::new(false));
        assert_eq!(breaker.run(|| 7), Some(7));
        assert_eq!(breaker.run(|| -> i32 { panic!("simulated fault") }), None);
        let mut ran = false;
        assert_eq!(breaker.run(|| ran = true), None);
        assert!(
            breaker.is_tripped() && !ran,
            "a tripped breaker runs no hook logic"
        );
        assert_eq!(
            breaker.contain(|| 3),
            Some(3),
            "cleanup still runs after a trip"
        );
    }

    #[test]
    fn explorer_is_recognized_by_file_name_ignoring_case() {
        let utf16 = |text: &str| text.encode_utf16().collect::<Vec<_>>();
        assert!(is_explorer_image(&utf16(r"C:\WINDOWS\Explorer.EXE")));
        for other in [
            r"C:\Tools\notexplorer.exe",
            r"C:\Windows\explorer.exe.bak",
            "explorer.exe",
        ] {
            assert!(!is_explorer_image(&utf16(other)), "{other}");
        }
    }

    #[test]
    fn the_hook_passes_everything_on_and_attaches_only_folder_views() {
        unsafe {
            assert_eq!(rwm_call_wnd_proc(-1, WPARAM(0), LPARAM(0)), LRESULT(0));
            assert_eq!(rwm_get_msg_proc(0, WPARAM(0), LPARAM(0)), LRESULT(0));
            let style = (WINDOW_EX_STYLE(0), WINDOW_STYLE(0));
            let window = CreateWindowExW(
                style.0,
                w!("Static"),
                w!(""),
                style.1,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .expect("a plain window");
            let attach = CWPSTRUCT {
                message: rwm_attach_message_id(),
                hwnd: window,
                ..Default::default()
            };
            rwm_call_wnd_proc(0, WPARAM(0), LPARAM(&attach as *const CWPSTRUCT as isize));
            let answer = SendMessageW(window, attach.message, WPARAM(0), LPARAM(0));
            assert_eq!(answer, LRESULT(0), "a Static window is not a folder view");
            let _ = DestroyWindow(window);
        }
    }
}
