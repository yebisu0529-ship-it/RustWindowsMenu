use std::ffi::c_void;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::ExitCode;

use rwm_core::config::RwmConfig;
use rwm_core::shell::{enable_dark_mode_menus, NativeMenu, OleGuard, CUSTOMIZE_MENU_CMD_ID};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, PostMessageW, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, GWLP_USERDATA, WM_NULL, WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::cli::PreviewArgs;
use crate::error::{CliResult, Context};
use crate::output::print_line;
use crate::target;

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        let menu = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *const NativeMenu;
        if !menu.is_null() {
            if let Some(result) = (*menu).handle_menu_msg(message, wparam, lparam) {
                return Some(result);
            }
        }
        None
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.0);
        }
    }
}

struct MenuBindingGuard<'a>(HWND, PhantomData<&'a NativeMenu>);
impl<'a> MenuBindingGuard<'a> {
    fn bind(hwnd: HWND, menu: &'a NativeMenu) -> Self {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, menu as *const NativeMenu as isize);
        }
        Self(hwnd, PhantomData)
    }
}
impl Drop for MenuBindingGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
        }
    }
}

pub fn run(args: &PreviewArgs) -> CliResult<ExitCode> {
    let target = target::resolve(&args.target)?;
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _dark = enable_dark_mode_menus();
    let _ole = OleGuard::initialize()?;
    let instance =
        unsafe { GetModuleHandleW(None) }.context(|| "GetModuleHandleW failed".to_string())?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE::from(instance),
        lpszClassName: w!("RustWindowsMenuPreview"),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&class);
    }
    let owner = Window(
        unsafe {
            CreateWindowExW(
                Default::default(),
                w!("RustWindowsMenuPreview"),
                w!("RustWindowsMenu preview"),
                WS_OVERLAPPED,
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
        .context(|| "cannot create preview owner window".to_string())?,
    );
    print_line(format!("Target: {}", target.describe()));
    let mut menu = NativeMenu::build_from_paths(
        owner.0,
        std::slice::from_ref(&target.path),
        target.background,
        args.extended,
    )?;
    if !args.raw {
        let cfg = RwmConfig::load_or_default()?;
        menu.apply_customizations(&cfg);
    }
    let _binding = MenuBindingGuard::bind(owner.0, &menu);
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }.context(|| "cannot read cursor position".to_string())?;
    let _ = unsafe { SetForegroundWindow(owner.0) };
    let selected = match menu.track_only(owner.0, (cursor.x, cursor.y), args.no_animation)? {
        Some(cmd_id) if cmd_id == CUSTOMIZE_MENU_CMD_ID => {
            crate::gui::spawn_manager_for_selection(menu.selection());
            Some(cmd_id)
        }
        Some(cmd_id) => {
            menu.invoke_id(cmd_id, owner.0, (cursor.x, cursor.y))?;
            Some(cmd_id)
        }
        None => None,
    };
    let _ = unsafe { PostMessageW(owner.0, WM_NULL, WPARAM(0), LPARAM(0)) };
    print_line(selected.map_or_else(
        || "Menu closed without a selection.".to_string(),
        |id| format!("Invoked menu command ID {id}."),
    ));
    Ok(ExitCode::SUCCESS)
}

#[allow(dead_code)]
fn _assert_hwnd_sendable(_: *mut c_void) {}
