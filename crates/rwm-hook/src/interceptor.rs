use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use rwm_core::model::SelectionTarget;
use rwm_ui::popup::{show_popup_menu, PopupOptions};
use windows::core::{Interface, HRESULT, PWSTR, VARIANT};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RPC_E_CHANGED_MODE, S_FALSE, S_OK, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Ole::IOleWindow;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IFolderView2, IPersistFolder2, IShellBrowser, IShellView, IShellWindows,
    IWebBrowserApp, RemoveWindowSubclass, SHGetPathFromIDListW, SetWindowSubclass, ShellWindows,
    SID_STopLevelBrowser, SIGDN_FILESYSPATH, SVGIO_SELECTION, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetCursorPos, GetParent, GA_ROOT, WM_CONTEXTMENU, WM_NCDESTROY,
};

use crate::state;

const RWM_SUBCLASS_ID: usize = 0x5257_4D31;

struct ComApartmentGuard {
    should_uninit: bool,
}

impl ComApartmentGuard {
    fn enter_sta() -> Self {
        let hr: HRESULT = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let should_uninit = hr == S_OK || hr == S_FALSE;
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            tracing::debug!(?hr, "CoInitializeEx returned unexpected HRESULT");
        }
        Self { should_uninit }
    }
}

impl Drop for ComApartmentGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn is_def_view_class(class_name: &str) -> bool {
    class_name.eq_ignore_ascii_case("SHELLDLL_DefView")
}

pub fn is_view_child_class(class_name: &str) -> bool {
    class_name.eq_ignore_ascii_case("DirectUIHWND")
        || class_name.eq_ignore_ascii_case("SysListView32")
}

pub fn is_desktop_shell_class(class_name: &str) -> bool {
    class_name.eq_ignore_ascii_case("Progman") || class_name.eq_ignore_ascii_case("WorkerW")
}

pub fn should_subclass_classes(hwnd_class: &str, parent_class: Option<&str>) -> bool {
    is_def_view_class(hwnd_class)
        || (is_view_child_class(hwnd_class) && parent_class.is_some_and(is_def_view_class))
}

pub fn extract_context_menu_lparam_coords(lparam: isize) -> Option<(i32, i32)> {
    let x = (lparam as u16) as i16 as i32;
    let y = ((lparam >> 16) as u16) as i16 as i32;
    if x == -1 && y == -1 {
        None
    } else {
        Some((x, y))
    }
}

pub fn build_selection_target(
    current_dir: PathBuf,
    items: Vec<PathBuf>,
    is_desktop: bool,
) -> SelectionTarget {
    match items.len() {
        0 => {
            if is_desktop {
                SelectionTarget::DesktopBackground(current_dir)
            } else {
                SelectionTarget::FolderBackground(current_dir)
            }
        }
        1 => {
            let only = items.into_iter().next().expect("checked len == 1");
            SelectionTarget::from_path(only, false)
        }
        _ => SelectionTarget::MultipleItems {
            parent: current_dir,
            items,
        },
    }
}

fn class_name_of(hwnd: HWND) -> Option<String> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut buf = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

pub fn maybe_subclass_explorer_window(hwnd: HWND) {
    if hwnd.is_invalid() || state::is_subclassed(hwnd) {
        return;
    }
    let Some(hwnd_class) = class_name_of(hwnd) else {
        return;
    };
    let parent_class = if is_view_child_class(&hwnd_class) {
        unsafe { GetParent(hwnd) }.ok().and_then(class_name_of)
    } else {
        None
    };

    if !should_subclass_classes(&hwnd_class, parent_class.as_deref()) {
        return;
    }

    if !state::pin_module_in_explorer() {
        return;
    }
    let attached =
        unsafe { SetWindowSubclass(hwnd, Some(explorer_subclass_proc), RWM_SUBCLASS_ID, 0) };
    if attached.as_bool() {
        state::mark_subclassed(hwnd);
    }
}

pub fn detach_thread_subclasses() {
    for hwnd in state::take_subclassed_hwnds() {
        if !hwnd.is_invalid() {
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(explorer_subclass_proc), RWM_SUBCLASS_ID);
            }
        }
    }
}

fn find_def_view_hwnd(hwnd: HWND) -> Option<HWND> {
    let cls = class_name_of(hwnd)?;
    if is_def_view_class(&cls) {
        return Some(hwnd);
    }
    if is_view_child_class(&cls) {
        let parent = unsafe { GetParent(hwnd) }.ok()?;
        if class_name_of(parent).is_some_and(|c| is_def_view_class(&c)) {
            return Some(parent);
        }
    }
    None
}

unsafe fn utf16_z_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

unsafe fn pwstr_to_string_and_free(pwstr: PWSTR) -> String {
    if pwstr.is_null() {
        return String::new();
    }
    let s = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void));
    s
}

fn query_folder_view_from_browser(browser: &IShellBrowser) -> Option<IFolderView2> {
    let view: IShellView = unsafe { browser.QueryActiveShellView() }.ok()?;
    view.cast::<IFolderView2>().ok()
}

fn find_explorer_folder_view(
    shell_windows: &IShellWindows,
    top_hwnd: HWND,
    def_view_hwnd: Option<HWND>,
) -> Option<IFolderView2> {
    let count = unsafe { shell_windows.Count() }.unwrap_or(0);
    let mut first_top_match: Option<IFolderView2> = None;

    for i in 0..count {
        let idx = VARIANT::from(i);
        let Ok(disp) = (unsafe { shell_windows.Item(&idx) }) else {
            continue;
        };
        let Ok(web_app) = disp.cast::<IWebBrowserApp>() else {
            continue;
        };
        let Ok(win_hwnd) = (unsafe { web_app.HWND() }) else {
            continue;
        };
        if win_hwnd.0 != top_hwnd.0 as isize {
            continue;
        }
        let Ok(sp) = disp.cast::<IServiceProvider>() else {
            continue;
        };
        let Ok(browser) = (unsafe { sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) }) else {
            continue;
        };
        let Ok(view) = (unsafe { browser.QueryActiveShellView() }) else {
            continue;
        };
        let Ok(folder_view) = view.cast::<IFolderView2>() else {
            continue;
        };

        // Windows 11 tabbed Explorer shares one CabinetWClass top_hwnd across tabs;
        // match IShellView's HWND against SHELLDLL_DefView when available.
        if let Some(target_def_view) = def_view_hwnd {
            if let Ok(ole_win) = view.cast::<IOleWindow>() {
                if let Ok(view_hwnd) = unsafe { ole_win.GetWindow() } {
                    if view_hwnd == target_def_view {
                        return Some(folder_view);
                    }
                }
            }
        }

        if def_view_hwnd.is_none() && first_top_match.is_none() {
            first_top_match = Some(folder_view);
        }
    }

    if def_view_hwnd.is_some() {
        None
    } else {
        first_top_match
    }
}

fn extract_explorer_selection(hwnd: HWND) -> Option<SelectionTarget> {
    let _com_guard = ComApartmentGuard::enter_sta();

    let top_hwnd = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let top_hwnd = if top_hwnd.is_invalid() { hwnd } else { top_hwnd };
    let is_desktop = class_name_of(top_hwnd).is_some_and(|c| is_desktop_shell_class(&c));

    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;

    let folder_view = if is_desktop {
        let empty = VARIANT::default();
        let mut desktop_hwnd_i32: i32 = 0;
        let disp = unsafe {
            shell_windows.FindWindowSW(
                &empty,
                &empty,
                SWC_DESKTOP,
                &mut desktop_hwnd_i32,
                SWFO_NEEDDISPATCH,
            )
        }
        .ok()?;
        let sp: IServiceProvider = disp.cast().ok()?;
        let browser: IShellBrowser =
            unsafe { sp.QueryService(&SID_STopLevelBrowser) }.ok()?;
        query_folder_view_from_browser(&browser)?
    } else {
        let def_view_hwnd = find_def_view_hwnd(hwnd);
        find_explorer_folder_view(&shell_windows, top_hwnd, def_view_hwnd)?
    };

    let persist: IPersistFolder2 = unsafe { folder_view.GetFolder() }.ok()?;
    let pidl = unsafe { persist.GetCurFolder() }.ok()?;
    if pidl.is_null() {
        return None;
    }

    let mut dir_buf = [0u16; 260];
    let path_ok = unsafe { SHGetPathFromIDListW(pidl, &mut dir_buf) };
    unsafe { CoTaskMemFree(Some(pidl as *const core::ffi::c_void)) };
    if !path_ok.as_bool() {
        return None;
    }
    let current_dir = PathBuf::from(unsafe { utf16_z_to_string(&dir_buf) });
    if current_dir.as_os_str().is_empty() {
        return None;
    }

    let sel_count = unsafe { folder_view.ItemCount(SVGIO_SELECTION) }.ok()?;
    let mut selected_items = Vec::new();
    if sel_count > 0 {
        let item_array = unsafe { folder_view.GetSelection(false) }.ok()?;
        let count = unsafe { item_array.GetCount() }.ok()?;
        for idx in 0..count {
            let item = unsafe { item_array.GetItemAt(idx) }.ok()?;
            let pwstr = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
            let item_path = unsafe { pwstr_to_string_and_free(pwstr) };
            if item_path.is_empty() {
                return None;
            }
            selected_items.push(PathBuf::from(item_path));
        }
    }

    Some(build_selection_target(current_dir, selected_items, is_desktop))
}

fn should_handle_context_menu_source(hwnd: HWND, wparam: WPARAM) -> bool {
    if wparam.0 == 0 {
        return true;
    }
    let source_hwnd = HWND(wparam.0 as *mut core::ffi::c_void);
    if source_hwnd == hwnd {
        return true;
    }
    class_name_of(source_hwnd)
        .is_some_and(|c| is_def_view_class(&c) || is_view_child_class(&c))
}

fn resolve_context_menu_coords(lparam: LPARAM) -> (i32, i32) {
    if let Some(coords) = extract_context_menu_lparam_coords(lparam.0) {
        return coords;
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
        (pt.x, pt.y)
    } else {
        (0, 0)
    }
}

fn handle_context_menu(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if !state::is_hook_active()
        || state::is_fault_tripped()
        || state::is_in_menu()
        || !should_handle_context_menu_source(hwnd, wparam)
        || state::is_kill_switch_active()
    {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }

    let engine = state::get_or_init_engine();
    let shift_held = state::is_shift_down();
    if engine.config().behavior.shift_bypass_to_native && shift_held {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }

    let Some(_menu_guard) = state::MenuReentrancyGuard::enter() else {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    };

    let Some(target) = extract_explorer_selection(hwnd) else {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    };

    let anchor = resolve_context_menu_coords(lparam);
    let snapshot = engine.build_instant_snapshot(target.clone(), shift_held);
    let session_id = snapshot.session_id;

    let cfg = engine.config();
    let options = PopupOptions {
        anchor,
        owner_hwnd: hwnd,
        show_timings: cfg.appearance.show_timings,
        submenu_hover_delay_ms: cfg.behavior.submenu_hover_delay_ms,
        smoke_test_ms: None,
    };

    let result = show_popup_menu(&engine, snapshot, options);
    if let Some(ref action) = result.chosen_action {
        if let Err(err) = engine.dispatch_action(action, &target, hwnd) {
            tracing::warn!(?err, "rwm-hook failed to dispatch menu action");
        }
    }
    engine.close_session(session_id);

    LRESULT(0)
}

pub unsafe extern "system" fn explorer_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid_subclass: usize,
    _dw_ref_data: usize,
) -> LRESULT {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if msg == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(explorer_subclass_proc), RWM_SUBCLASS_ID);
            state::unmark_subclassed(hwnd);
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }

        let detach_id = state::detach_message_id();
        if detach_id != 0 && msg == detach_id {
            state::deactivate_hook();
            let _ = RemoveWindowSubclass(hwnd, Some(explorer_subclass_proc), RWM_SUBCLASS_ID);
            state::unmark_subclassed(hwnd);
            detach_thread_subclasses();
            state::shutdown_engine();
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }

        if msg == WM_CONTEXTMENU {
            return handle_context_menu(hwnd, msg, wparam, lparam);
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }));

    match outcome {
        Ok(res) => res,
        Err(_) => {
            state::trip_fault();
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_matching_helpers_identify_explorer_views() {
        assert!(should_subclass_classes("SHELLDLL_DefView", None));
        assert!(should_subclass_classes("shelldll_defview", Some("CabinetWClass")));
        assert!(should_subclass_classes("DirectUIHWND", Some("SHELLDLL_DefView")));
        assert!(should_subclass_classes("SysListView32", Some("SHELLDLL_DefView")));

        assert!(!should_subclass_classes("DirectUIHWND", Some("CabinetWClass")));
        assert!(!should_subclass_classes("SysListView32", None));
        assert!(!should_subclass_classes("SysHeader32", Some("SHELLDLL_DefView")));

        assert!(is_desktop_shell_class("Progman"));
        assert!(is_desktop_shell_class("WorkerW"));
        assert!(!is_desktop_shell_class("CabinetWClass"));
    }

    #[test]
    fn context_menu_lparam_coords_handles_mouse_and_keyboard_sentinel() {
        let pack = |x: i16, y: i16| -> isize {
            ((y as u16 as isize) << 16) | (x as u16 as isize)
        };

        assert_eq!(
            extract_context_menu_lparam_coords(pack(320, 480)),
            Some((320, 480))
        );
        assert_eq!(
            extract_context_menu_lparam_coords(pack(-120, -45)),
            Some((-120, -45))
        );
        assert_eq!(extract_context_menu_lparam_coords(pack(-1, -1)), None);
        assert_eq!(
            extract_context_menu_lparam_coords(pack(-1, 100)),
            Some((-1, 100))
        );
    }

    #[test]
    fn selection_target_mapping_covers_desktop_folder_single_and_multi() {
        let dir = PathBuf::from(r"C:\Temp");
        assert_eq!(
            build_selection_target(dir.clone(), Vec::new(), true),
            SelectionTarget::DesktopBackground(dir.clone())
        );
        assert_eq!(
            build_selection_target(dir.clone(), Vec::new(), false),
            SelectionTarget::FolderBackground(dir.clone())
        );

        let file = PathBuf::from(r"C:\Temp\a.txt");
        assert_eq!(
            build_selection_target(dir.clone(), vec![file.clone()], false),
            SelectionTarget::SingleFile(file.clone())
        );

        let file2 = PathBuf::from(r"C:\Temp\b.txt");
        assert_eq!(
            build_selection_target(dir.clone(), vec![file.clone(), file2.clone()], false),
            SelectionTarget::MultipleItems {
                parent: dir,
                items: vec![file, file2],
            }
        );
    }
}
