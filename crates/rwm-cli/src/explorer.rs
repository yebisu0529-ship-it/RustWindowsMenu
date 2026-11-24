use std::collections::{BTreeSet, HashMap};
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindow,
    SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT,
};

use crate::hookdll;

const TOP_LEVEL_CLASSES: [&str; 4] = ["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"];
const MESSAGE_TIMEOUT_MS: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    TopLevel,
    View,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellWindow {
    pub hwnd: isize,
    pub kind: WindowKind,
    pub class_name: String,
    pub process_id: u32,
    pub thread_id: u32,
}

impl ShellWindow {
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }
}

pub fn is_top_level_class(name: &str) -> bool {
    TOP_LEVEL_CLASSES
        .iter()
        .any(|class| class.eq_ignore_ascii_case(name))
}

pub fn is_view_class(name: &str) -> bool {
    name.eq_ignore_ascii_case("SHELLDLL_DefView")
}

pub fn ui_thread_ids(windows: &[ShellWindow]) -> BTreeSet<u32> {
    windows
        .iter()
        .filter(|window| window.kind == WindowKind::View)
        .map(|window| window.thread_id)
        .collect()
}

pub fn enumerate_explorer_windows() -> Vec<ShellWindow> {
    let mut processes = HashMap::new();
    let mut found = Vec::new();
    for hwnd in top_level_windows() {
        let Some(top) = describe(hwnd, WindowKind::TopLevel) else {
            continue;
        };
        if !is_top_level_class(&top.class_name) {
            continue;
        }
        let explorer = *processes
            .entry(top.process_id)
            .or_insert_with(|| is_explorer_process(top.process_id));
        if !explorer {
            continue;
        }
        found.extend(
            descendant_windows(hwnd)
                .into_iter()
                .filter_map(|child| describe(child, WindowKind::View))
                .filter(|view| {
                    is_view_class(&view.class_name) && view.process_id == top.process_id
                }),
        );
        found.push(top);
    }
    found
}

pub fn explorer_views(windows: &[ShellWindow]) -> Vec<ShellWindow> {
    windows
        .iter()
        .filter(|window| window.kind == WindowKind::View)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivery {
    pub attempted: usize,
    pub acknowledged: usize,
    pub skipped_dead: usize,
}

pub fn send_message(windows: &[ShellWindow], message: u32) -> Delivery {
    let mut delivery = Delivery::default();
    for window in windows {
        if !unsafe { IsWindow(window.hwnd()) }.as_bool() {
            delivery.skipped_dead += 1;
            continue;
        }
        delivery.attempted += 1;
        let mut result = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                window.hwnd(),
                message,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                MESSAGE_TIMEOUT_MS,
                Some(&mut result),
            )
        };
        if sent.0 != 0 && result == 1 {
            delivery.acknowledged += 1;
        }
    }
    delivery
}

pub fn orphan_detach_sweep() -> Delivery {
    let windows = enumerate_explorer_windows();
    let mut messages = BTreeSet::new();
    for name in [
        hookdll::detach_message_name(),
        hookdll::LEGACY_DETACH_MESSAGE.to_string(),
        hookdll::LEGACY_V01_DETACH_MESSAGE.to_string(),
    ] {
        if let Ok(id) = hookdll::register_message(&name) {
            messages.insert(id);
        }
    }
    let mut version_roots = vec![rwm_core::config::local_data_dir().join("versions")];
    if let Some(custom) = std::env::var_os("RWM_INSTALL_DIR").filter(|v| !v.is_empty()) {
        version_roots.push(PathBuf::from(custom).join("versions"));
    }
    for root in version_roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(id) =
                        hookdll::register_message(&format!("RustWindowsMenu.DetachHook.{name}"))
                    {
                        messages.insert(id);
                    }
                }
            }
        }
    }
    let mut total = Delivery::default();
    for message in messages {
        let sent = send_message(&windows, message);
        total.attempted += sent.attempted;
        total.acknowledged += sent.acknowledged;
        total.skipped_dead += sent.skipped_dead;
    }
    total
}

fn describe(hwnd: HWND, kind: WindowKind) -> Option<ShellWindow> {
    let mut class = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    if len <= 0 {
        return None;
    }
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    (thread_id != 0).then(|| ShellWindow {
        hwnd: hwnd.0 as isize,
        kind,
        class_name: String::from_utf16_lossy(&class[..len as usize]),
        process_id,
        thread_id,
    })
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    catch_unwind(AssertUnwindSafe(|| {
        let windows = &mut *(lparam.0 as *mut Vec<HWND>);
        windows.push(hwnd);
        TRUE
    }))
    .unwrap_or(TRUE)
}

fn top_level_windows() -> Vec<HWND> {
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect),
            LPARAM(&mut windows as *mut Vec<HWND> as isize),
        );
    }
    windows
}

fn descendant_windows(parent: HWND) -> Vec<HWND> {
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(collect),
            LPARAM(&mut windows as *mut Vec<HWND> as isize),
        );
    }
    windows
}

fn expected_explorer_path() -> Option<PathBuf> {
    let mut buf = [0u16; 32768];
    let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
    (len > 0 && len < buf.len())
        .then(|| PathBuf::from(OsString::from_wide(&buf[..len])).join("explorer.exe"))
}

fn is_explorer_process(process_id: u32) -> bool {
    let Some(expected) = expected_explorer_path() else {
        return false;
    };
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return false;
    };
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    queried.is_ok()
        && OsString::from_wide(&path[..len as usize]).eq_ignore_ascii_case(expected.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_contract_classes_are_hook_targets() {
        assert!(is_top_level_class("ExploreWClass"));
        assert!(is_top_level_class("WorkerW"));
        assert!(is_view_class("SHELLDLL_DefView"));
        assert!(!is_view_class("DirectUIHWND"));
    }
}
