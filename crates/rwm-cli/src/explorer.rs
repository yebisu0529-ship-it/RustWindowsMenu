use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, RegisterWindowMessageW,
    SendMessageTimeoutW, SMTO_ABORTIFHUNG,
};

/// Top-level window classes that mark an Explorer UI thread.
const TOP_LEVEL_CLASSES: [&str; 3] = ["CabinetWClass", "Progman", "WorkerW"];

/// The classes `rwm_hook.dll` subclasses. Each one removes its own subclass, and every other
/// subclass on its thread, when it receives the detach message directly.
const VIEW_CLASSES: [&str; 3] = ["SHELLDLL_DefView", "DirectUIHWND", "SysListView32"];

const EXPLORER_IMAGE_SUFFIX: &str = r"\explorer.exe";
const DETACH_TIMEOUT_MS: u32 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    TopLevel,
    /// A folder view inside a top-level window.
    View,
}

/// A window that belongs to `explorer.exe` and matters to the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellWindow {
    pub hwnd: isize,
    pub kind: WindowKind,
    pub class_name: String,
    pub process_id: u32,
    pub thread_id: u32,
}

pub fn is_top_level_class(class_name: &str) -> bool {
    TOP_LEVEL_CLASSES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(class_name))
}

pub fn is_view_class(class_name: &str) -> bool {
    VIEW_CLASSES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(class_name))
}

pub fn is_explorer_image(image_path: &str) -> bool {
    let Some(start) = image_path.len().checked_sub(EXPLORER_IMAGE_SUFFIX.len()) else {
        return false;
    };
    image_path
        .get(start..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(EXPLORER_IMAGE_SUFFIX))
}

/// The threads the hook must be attached to: every thread that owns a listed window.
pub fn ui_thread_ids(windows: &[ShellWindow]) -> BTreeSet<u32> {
    windows.iter().map(|window| window.thread_id).collect()
}

/// Finds Explorer's top-level UI windows (`CabinetWClass`, `Progman`, `WorkerW` owned by an
/// `explorer.exe` process) and the folder views inside them.
pub fn enumerate_explorer_windows() -> Vec<ShellWindow> {
    let mut is_explorer_process: HashMap<u32, bool> = HashMap::new();
    let mut found = Vec::new();

    for top in top_level_windows() {
        let Some(top_window) = describe(top, WindowKind::TopLevel) else {
            continue;
        };
        if !is_top_level_class(&top_window.class_name) {
            continue;
        }
        let owned_by_explorer = *is_explorer_process
            .entry(top_window.process_id)
            .or_insert_with(|| {
                process_image_path(top_window.process_id)
                    .is_some_and(|image| is_explorer_image(&image))
            });
        if !owned_by_explorer {
            continue;
        }

        let process_id = top_window.process_id;
        found.push(top_window);
        // A preview handler can parent a window from another process under an Explorer
        // window, so views are kept only when Explorer itself owns them.
        found.extend(
            descendant_windows(top)
                .into_iter()
                .filter_map(|child| describe(child, WindowKind::View))
                .filter(|view| is_view_class(&view.class_name) && view.process_id == process_id),
        );
    }
    found
}

/// The message `rwm_hook.dll` treats as "remove your subclasses and go inert".
pub fn detach_message_id() -> u32 {
    unsafe { RegisterWindowMessageW(w!("RustWindowsMenu_Detach_v1")) }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetachDelivery {
    pub windows: usize,
    pub delivered: usize,
}

/// Sends the detach message to every listed window and waits for each to process it.
///
/// Top-level windows come first in an enumeration, so a still-installed hook sees the message
/// there. The views receive it as well because the hook stops reacting on every other thread
/// once one thread has handled it, and only a direct delivery reaches those subclasses.
pub fn send_detach(windows: &[ShellWindow], message_id: u32) -> DetachDelivery {
    send_message_to_windows(windows, message_id)
}

pub fn send_message_to_windows(windows: &[ShellWindow], message_id: u32) -> DetachDelivery {
    let delivered = windows
        .iter()
        .filter(|window| {
            let outcome = unsafe {
                SendMessageTimeoutW(
                    HWND(window.hwnd as *mut c_void),
                    message_id,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG,
                    DETACH_TIMEOUT_MS,
                    None,
                )
            };
            outcome.0 != 0
        })
        .count();
    DetachDelivery {
        windows: windows.len(),
        delivered,
    }
}

fn describe(hwnd: HWND, kind: WindowKind) -> Option<ShellWindow> {
    let mut class_buf = [0u16; 128];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    let class_len = usize::try_from(class_len).ok().filter(|len| *len > 0)?;

    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if thread_id == 0 {
        return None;
    }

    Some(ShellWindow {
        hwnd: hwnd.0 as isize,
        kind,
        class_name: String::from_utf16_lossy(&class_buf[..class_len]),
        process_id,
        thread_id,
    })
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<HWND>);
    windows.push(hwnd);
    TRUE
}

fn top_level_windows() -> Vec<HWND> {
    let mut windows: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut windows as *mut Vec<HWND> as isize),
        );
    }
    windows
}

fn descendant_windows(parent: HWND) -> Vec<HWND> {
    let mut windows: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(collect_window),
            LPARAM(&mut windows as *mut Vec<HWND> as isize),
        );
    }
    windows
}

fn process_image_path(process_id: u32) -> Option<String> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    queried.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(kind: WindowKind, thread_id: u32) -> ShellWindow {
        ShellWindow {
            hwnd: 0x1000 + thread_id as isize,
            kind,
            class_name: "CabinetWClass".to_string(),
            process_id: 4242,
            thread_id,
        }
    }

    #[test]
    fn explorer_image_matches_by_file_name_only() {
        assert!(is_explorer_image(r"C:\Windows\explorer.exe"));
        assert!(is_explorer_image(r"C:\WINDOWS\Explorer.EXE"));
        assert!(!is_explorer_image(r"C:\Tools\notexplorer.exe"));
        assert!(!is_explorer_image(r"C:\Windows\explorer.exe.bak"));
        assert!(!is_explorer_image("explorer.exe"));
        assert!(!is_explorer_image(r"C:\ツール\エクスプローラー.exe"));
        assert!(!is_explorer_image(""));
    }

    #[test]
    fn window_classes_match_whole_names_ignoring_case() {
        for class_name in ["CabinetWClass", "Progman", "WorkerW", "workerw"] {
            assert!(is_top_level_class(class_name), "{class_name}");
        }
        for class_name in ["Shell_TrayWnd", "CabinetWClassX", "SHELLDLL_DefView", ""] {
            assert!(!is_top_level_class(class_name), "{class_name}");
        }
        for class_name in ["SHELLDLL_DefView", "DirectUIHWND", "SysListView32"] {
            assert!(is_view_class(class_name), "{class_name}");
        }
        assert!(!is_view_class("CabinetWClass"));
    }

    #[test]
    fn ui_threads_are_deduplicated_across_windows() {
        let windows = [
            window(WindowKind::TopLevel, 30),
            window(WindowKind::View, 30),
            window(WindowKind::TopLevel, 10),
            window(WindowKind::View, 20),
        ];
        assert_eq!(
            ui_thread_ids(&windows).into_iter().collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn live_enumeration_only_reports_explorer_windows() {
        let windows = enumerate_explorer_windows();
        for window in &windows {
            // A process that exited since the enumeration has no image path to check.
            if let Some(image) = process_image_path(window.process_id) {
                assert!(is_explorer_image(&image), "{window:?} is owned by {image}");
            }
            match window.kind {
                WindowKind::TopLevel => assert!(is_top_level_class(&window.class_name)),
                WindowKind::View => assert!(is_view_class(&window.class_name)),
            }
            assert_ne!(window.thread_id, 0);
        }
        let first_view = windows.iter().position(|w| w.kind == WindowKind::View);
        if let Some(index) = first_view {
            assert!(index > 0, "a view is listed after its top-level window");
        }
    }

    #[test]
    fn detach_message_is_a_stable_registered_message() {
        let id = detach_message_id();
        assert!((0xC000..=0xFFFF).contains(&id));
        assert_eq!(id, detach_message_id());
    }

    #[test]
    fn detach_to_no_windows_delivers_nothing() {
        assert_eq!(
            send_detach(&[], detach_message_id()),
            DetachDelivery::default()
        );
    }
}
