mod interceptor;
mod state;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{BOOL, HINSTANCE, HMODULE, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, CWPSTRUCT};

/// DLL entry point called by the Windows loader.
///
/// # Safety
/// Must only be invoked by the OS loader with a valid `HINSTANCE` while holding the loader lock;
/// performs only lock-free atomic stores and stack operations.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hinst: HINSTANCE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        // Loader lock rules forbid heap allocations, mutexes, OnceLock, threads, or COM here.
        DLL_PROCESS_ATTACH => {
            state::DLL_HINSTANCE.store(hinst.0 as usize, Ordering::Release);
            state::HOOK_ACTIVE.store(true, Ordering::Release);
            state::FAULT_TRIPPED.store(false, Ordering::Release);

            let mut stack_buf = [0u16; 512];
            let len = GetModuleFileNameW(None, &mut stack_buf) as usize;
            let is_explorer =
                len > 0 && len <= stack_buf.len() && state::path_ends_with_explorer_exe(&stack_buf[..len]);
            state::IS_EXPLORER_PROCESS.store(is_explorer, Ordering::Release);

            let _ = DisableThreadLibraryCalls(HMODULE(hinst.0));
        }
        DLL_PROCESS_DETACH => {
            state::deactivate_hook();
        }
        _ => {}
    }
    TRUE
}

/// `WH_CALLWNDPROC` hook procedure attached to `explorer.exe` UI threads.
///
/// # Safety
/// Called by Win32 message dispatch with valid `code`, `wparam`, and `lparam` pointing to a
/// `CWPSTRUCT` when `code >= 0` and `lparam != 0`.
#[no_mangle]
pub unsafe extern "system" fn rwm_call_wnd_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if !state::is_explorer_process() {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        if code >= 0 && lparam.0 != 0 {
            let cwp = &*(lparam.0 as *const CWPSTRUCT);
            let attach_id = state::attach_message_id();
            if attach_id != 0 && cwp.message == attach_id {
                state::activate_hook();
                if !cwp.hwnd.is_invalid() {
                    interceptor::maybe_subclass_explorer_window(cwp.hwnd);
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }

            if !state::is_hook_active() || state::is_fault_tripped() {
                return CallNextHookEx(None, code, wparam, lparam);
            }

            let detach_id = state::detach_message_id();
            if detach_id != 0 && cwp.message == detach_id {
                state::deactivate_hook();
                interceptor::detach_thread_subclasses();
                state::shutdown_engine();
            } else if !cwp.hwnd.is_invalid() {
                interceptor::maybe_subclass_explorer_window(cwp.hwnd);
            }
        }

        CallNextHookEx(None, code, wparam, lparam)
    }));

    match outcome {
        Ok(res) => res,
        Err(_) => {
            state::trip_fault();
            CallNextHookEx(None, code, wparam, lparam)
        }
    }
}

#[no_mangle]
pub extern "C" fn rwm_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn rwm_detach_message_id() -> u32 {
    state::detach_message_id()
}

#[no_mangle]
pub extern "C" fn rwm_attach_message_id() -> u32 {
    state::attach_message_id()
}
