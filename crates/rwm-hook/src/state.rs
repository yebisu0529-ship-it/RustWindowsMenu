use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rwm_core::config::RwmConfig;
use rwm_core::engine::FastMenuEngine;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;

pub static DLL_HINSTANCE: AtomicUsize = AtomicUsize::new(0);
pub static IS_EXPLORER_PROCESS: AtomicBool = AtomicBool::new(false);
pub static HOOK_ACTIVE: AtomicBool = AtomicBool::new(true);
pub static FAULT_TRIPPED: AtomicBool = AtomicBool::new(false);

static DETACH_MSG_ID: AtomicU32 = AtomicU32::new(0);
static ATTACH_MSG_ID: AtomicU32 = AtomicU32::new(0);
static MODULE_PINNED: Mutex<bool> = Mutex::new(false);
static ENGINE: Mutex<Option<Arc<FastMenuEngine>>> = Mutex::new(None);

thread_local! {
    static SUBCLASSED_HWNDS: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    static IN_MENU: Cell<bool> = const { Cell::new(false) };
}

pub struct MenuReentrancyGuard;

impl MenuReentrancyGuard {
    pub fn enter() -> Option<Self> {
        IN_MENU.with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(Self)
            }
        })
    }
}

impl Drop for MenuReentrancyGuard {
    fn drop(&mut self) {
        IN_MENU.with(|flag| flag.set(false));
    }
}

pub fn is_explorer_process() -> bool {
    IS_EXPLORER_PROCESS.load(Ordering::Acquire)
}

pub fn is_hook_active() -> bool {
    HOOK_ACTIVE.load(Ordering::Acquire)
}

pub fn deactivate_hook() {
    HOOK_ACTIVE.store(false, Ordering::Release);
}

pub fn activate_hook() {
    FAULT_TRIPPED.store(false, Ordering::Release);
    HOOK_ACTIVE.store(true, Ordering::Release);
}

/// Pins `rwm_hook.dll` inside `explorer.exe` so an unexpected `rwm.exe` exit cannot leave
/// Explorer threads or subclass procedures executing unmapped code pages.
pub fn pin_module_in_explorer() -> bool {
    if !is_explorer_process() {
        return false;
    }
    let mut pinned = MODULE_PINNED.lock().unwrap_or_else(|e| e.into_inner());
    if *pinned {
        return true;
    }
    let mut hmod = HMODULE::default();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(pin_module_in_explorer as *const () as *const u16),
            &mut hmod,
        )
    }
    .is_ok();
    if ok {
        *pinned = true;
    }
    ok
}

pub fn is_fault_tripped() -> bool {
    FAULT_TRIPPED.load(Ordering::Acquire)
}

pub fn trip_fault() {
    FAULT_TRIPPED.store(true, Ordering::Release);
}

pub fn is_in_menu() -> bool {
    IN_MENU.with(Cell::get)
}

pub fn detach_message_id() -> u32 {
    let cached = DETACH_MSG_ID.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let msg = unsafe { RegisterWindowMessageW(w!("RustWindowsMenu_Detach_v1")) };
    if msg != 0 {
        DETACH_MSG_ID.store(msg, Ordering::Relaxed);
    }
    msg
}

pub fn attach_message_id() -> u32 {
    let cached = ATTACH_MSG_ID.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let msg = unsafe { RegisterWindowMessageW(w!("RustWindowsMenu_Attach_v1")) };
    if msg != 0 {
        ATTACH_MSG_ID.store(msg, Ordering::Relaxed);
    }
    msg
}

pub fn get_or_init_engine() -> Arc<FastMenuEngine> {
    let mut guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref engine) = *guard {
        return Arc::clone(engine);
    }
    let config = Arc::new(RwmConfig::load_or_default().unwrap_or_default());
    let engine = Arc::new(FastMenuEngine::new(config, true));
    *guard = Some(Arc::clone(&engine));
    engine
}

pub fn shutdown_engine() {
    let engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(engine) = engine {
        engine.request_shutdown();
    }
}

pub fn is_subclassed(hwnd: HWND) -> bool {
    let raw = hwnd.0 as isize;
    SUBCLASSED_HWNDS.with(|set| set.borrow().contains(&raw))
}

pub fn mark_subclassed(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    SUBCLASSED_HWNDS.with(|set| {
        set.borrow_mut().insert(raw);
    });
}

pub fn unmark_subclassed(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    SUBCLASSED_HWNDS.with(|set| {
        set.borrow_mut().remove(&raw);
    });
}

pub fn take_subclassed_hwnds() -> Vec<HWND> {
    SUBCLASSED_HWNDS.with(|set| {
        let mut guard = set.borrow_mut();
        guard
            .drain()
            .map(|raw| HWND(raw as *mut core::ffi::c_void))
            .collect()
    })
}

/// Loader-lock-safe UTF-16 suffix check against `\explorer.exe` without heap allocation.
pub fn path_ends_with_explorer_exe(utf16_path: &[u16]) -> bool {
    const SUFFIX: &[u8] = b"\\explorer.exe";
    if utf16_path.len() < SUFFIX.len() {
        return false;
    }
    let tail = &utf16_path[utf16_path.len() - SUFFIX.len()..];
    tail.iter().zip(SUFFIX.iter()).all(|(&unit, &expected)| {
        if unit > 0x7F {
            return false;
        }
        (unit as u8).to_ascii_lowercase() == expected
    })
}

pub fn kill_switch_exists_at(path: &Path) -> bool {
    path.exists()
}

pub fn is_kill_switch_active() -> bool {
    kill_switch_exists_at(&RwmConfig::kill_switch_path())
}

pub fn is_shift_down() -> bool {
    unsafe { GetKeyState(i32::from(VK_SHIFT.0)) < 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn to_utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn explorer_exe_suffix_matches_case_insensitively() {
        assert!(path_ends_with_explorer_exe(&to_utf16(
            r"C:\Windows\explorer.exe"
        )));
        assert!(path_ends_with_explorer_exe(&to_utf16(
            r"C:\WINDOWS\EXPLORER.EXE"
        )));
        assert!(path_ends_with_explorer_exe(&to_utf16(r"\Explorer.Exe")));

        assert!(!path_ends_with_explorer_exe(&to_utf16(
            r"C:\Tools\notexplorer.exe"
        )));
        assert!(!path_ends_with_explorer_exe(&to_utf16(
            r"C:\Windows\explorer.exe.bak"
        )));
        assert!(!path_ends_with_explorer_exe(&to_utf16(r"explorer.exe")));
        assert!(!path_ends_with_explorer_exe(&to_utf16("")));
    }

    #[test]
    fn kill_switch_helper_tracks_file_presence_and_default_name() {
        assert_eq!(
            RwmConfig::kill_switch_path()
                .file_name()
                .and_then(|n| n.to_str()),
            Some("DISABLE_HOOK")
        );

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("rwm_hook_ks_{unique}"));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let marker = temp_dir.join("DISABLE_HOOK");

        assert!(!kill_switch_exists_at(&marker));
        fs::write(&marker, b"1").expect("write marker");
        assert!(kill_switch_exists_at(&marker));

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
