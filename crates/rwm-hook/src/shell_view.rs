//! Queries against Explorer's view windows, made on the thread that owns them. No COM pointer
//! outlives the message that looked it up.

use std::ffi::c_void;

use rwm_core::shell::{capture_from_view, OwnedPidl, SelectionKey};
use windows::core::{w, IUnknown, Interface, PCWSTR, VARIANT};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::{CoCreateInstance, IDispatch, IServiceProvider, CLSCTX_ALL};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{VariantChangeType, VT_UNKNOWN};
use windows::Win32::UI::Accessibility::{
    AccessibleObjectFromPoint, AccessibleObjectFromWindow, IAccessible, ROLE_SYSTEM_LISTITEM,
};
use windows::Win32::UI::Controls::{
    LVHITTESTINFO, LVIR_BOUNDS, LVM_GETITEMRECT, LVM_GETNEXTITEM, LVM_HITTEST, LVNI_FOCUSED,
};
use windows::Win32::UI::Shell::{
    IFolderView, IFolderView2, IShellBrowser, IShellView, IShellWindows, IWebBrowserApp,
    SID_STopLevelBrowser, ShellWindows, SVGIO_SELECTION, SVSI_EDIT, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetAncestor, GetClassNameW, GetWindowRect, GetWindowThreadProcessId,
    SendMessageW, WindowFromPoint, CHILDID_SELF, GA_ROOT, HTCLIENT, OBJID_CLIENT, WM_NCHITTEST,
    WM_USER,
};

use crate::gesture::{self, Hit};

/// Asks a browser frame for its `IShellBrowser`, which comes back without a reference.
pub(crate) const CWM_GETISHELLBROWSER: u32 = WM_USER + 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildKind {
    /// `SysListView32`, the desktop's item list.
    ListView,
    /// `DirectUIHWND`, File Explorer's items view.
    ItemsView,
}

fn class_is(window: HWND, names: &[&str]) -> bool {
    let mut class = [0u16; 64];
    let len = usize::try_from(unsafe { GetClassNameW(window, &mut class) }).unwrap_or(0);
    names
        .iter()
        .any(|name| crate::ascii_eq_ignore_case(&class[..len], name))
}

fn root_is(window: HWND, names: &[&str]) -> bool {
    class_is(unsafe { GetAncestor(window, GA_ROOT) }, names)
}

pub(crate) fn is_defview(window: HWND) -> bool {
    class_is(window, &["SHELLDLL_DefView"])
}

/// True for views inside an Explorer frame or on the desktop, the windows the hook serves.
pub(crate) fn has_explorer_frame(window: HWND) -> bool {
    root_is(
        window,
        &["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"],
    )
}

pub(crate) fn child_kind(window: HWND) -> Option<ChildKind> {
    if class_is(window, &["DirectUIHWND"]) {
        Some(ChildKind::ItemsView)
    } else {
        class_is(window, &["SysListView32"]).then_some(ChildKind::ListView)
    }
}

pub(crate) fn view_children(defview: HWND) -> Vec<HWND> {
    let mut children = Vec::new();
    for class in [w!("DirectUIHWND"), w!("SysListView32")] {
        let mut after = HWND::default();
        while let Ok(child) = unsafe { FindWindowExW(defview, after, class, PCWSTR::null()) } {
            children.push(child);
            after = child;
        }
    }
    children
}

/// The `IShellView` behind one `SHELLDLL_DefView`, looked up for a single message.
pub(crate) struct ShellView {
    pub(crate) view: IShellView,
    pub(crate) folder_view: Option<IFolderView2>,
}

impl ShellView {
    pub(crate) fn find(defview: HWND) -> Option<Self> {
        // ShellWindows lists only Explorer's own windows, and asking it from any other
        // process would reach into the live explorer.exe.
        let registered = || crate::is_explorer_process().then(|| from_shell_windows(defview));
        let view = from_frame(defview).or_else(|| registered().flatten())?;
        Some(Self {
            folder_view: view.cast().ok(),
            view,
        })
    }

    pub(crate) fn selection_count(&self) -> i32 {
        let folder_view = self.view.cast::<IFolderView>().ok();
        folder_view
            .and_then(|view| unsafe { view.ItemCount(SVGIO_SELECTION) }.ok())
            .unwrap_or(0)
    }
}

fn from_frame(defview: HWND) -> Option<IShellView> {
    let root = unsafe { GetAncestor(defview, GA_ROOT) };
    let same_thread = unsafe { GetWindowThreadProcessId(root, None) == GetCurrentThreadId() };
    if !same_thread || !class_is(root, &["CabinetWClass", "ExploreWClass", "Progman"]) {
        return None;
    }
    let raw = unsafe { SendMessageW(root, CWM_GETISHELLBROWSER, WPARAM(0), LPARAM(0)) }.0;
    active_view(
        unsafe { IShellBrowser::from_raw_borrowed(&(raw as *mut c_void)) }?,
        defview,
    )
}

fn from_shell_windows(defview: HWND) -> Option<IShellView> {
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;
    if root_is(defview, &["Progman", "WorkerW"]) {
        let (desktop_csidl, mut hwnd) = (VARIANT::from(0i32), 0);
        let none = VARIANT::new();
        let desktop = unsafe {
            windows.FindWindowSW(
                &desktop_csidl,
                &none,
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )
        };
        return view_of_window(&desktop.ok()?, defview);
    }
    let thread = unsafe { GetCurrentThreadId() };
    (0..unsafe { windows.Count() }.ok()?).find_map(|index| {
        let window = unsafe { windows.Item(&VARIANT::from(index)) }.ok()?;
        let frame = unsafe { window.cast::<IWebBrowserApp>().ok()?.HWND() }.ok()?;
        let frame_thread = unsafe { GetWindowThreadProcessId(HWND(frame.0 as *mut c_void), None) };
        (frame_thread == thread)
            .then(|| view_of_window(&window, defview))
            .flatten()
    })
}

fn view_of_window(window: &IDispatch, defview: HWND) -> Option<IShellView> {
    let provider = window.cast::<IServiceProvider>().ok()?;
    active_view(
        &unsafe { provider.QueryService(&SID_STopLevelBrowser) }.ok()?,
        defview,
    )
}

fn active_view(browser: &IShellBrowser, defview: HWND) -> Option<IShellView> {
    let view = unsafe { browser.QueryActiveShellView() }.ok()?;
    (unsafe { view.GetWindow() }.ok()? == defview).then_some(view)
}

/// What `screen` lands on inside `child`, or `None` when that cannot be told. A point that
/// another window covers is never probed, so hit testing stays inside this view.
pub(crate) fn hit_test(child: HWND, kind: ChildKind, screen: POINT) -> Option<Hit> {
    if unsafe { WindowFromPoint(screen) } != child {
        return None;
    }
    if kind == ChildKind::ListView {
        let mut info = LVHITTESTINFO {
            pt: screen,
            ..Default::default()
        };
        unsafe { ScreenToClient(child, &mut info.pt) }.ok().ok()?;
        let info = LPARAM(&mut info as *mut LVHITTESTINFO as isize);
        let item = unsafe { SendMessageW(child, LVM_HITTEST, WPARAM(0), info) }.0;
        return Some(if item >= 0 {
            Hit::Item
        } else {
            Hit::Background
        });
    }
    let (mut accessible, mut id) = (None, VARIANT::new());
    unsafe { AccessibleObjectFromPoint(screen, &mut accessible, &mut id) }.ok()?;
    let mut node = Some((accessible?, id));
    let roles = std::iter::from_fn(move || {
        let (accessible, id) = node.take()?;
        let role = unsafe { accessible.get_accRole(&id) }.ok()?;
        // A simple element's parent is the object that reported it.
        node = if is_self(&id) {
            let parent = unsafe { accessible.accParent() }
                .ok()
                .and_then(|parent| parent.cast().ok());
            parent.map(|parent| (parent, self_id()))
        } else {
            Some((accessible, self_id()))
        };
        u32::try_from(&role).ok()
    });
    gesture::classify_roles(roles.take(6))
}

fn self_id() -> VARIANT {
    VARIANT::from(CHILDID_SELF as i32)
}

fn is_self(id: &VARIANT) -> bool {
    id.is_empty() || i32::try_from(id).is_ok_and(|id| id == CHILDID_SELF as i32)
}

/// False on the window's frame and scroll bars, which keep Explorer's own menus.
pub(crate) fn in_client_area(window: HWND, screen: POINT) -> bool {
    let at = LPARAM(gesture::lparam_from_point(screen));
    unsafe { SendMessageW(window, WM_NCHITTEST, WPARAM(0), at) }.0 == HTCLIENT as isize
}

pub(crate) fn client_to_screen(window: HWND, mut point: POINT) -> Option<POINT> {
    unsafe { ClientToScreen(window, &mut point) }
        .as_bool()
        .then_some(point)
}

pub(crate) fn root_window(window: HWND) -> HWND {
    unsafe { GetAncestor(window, GA_ROOT) }
}

pub(crate) fn window_rect(window: HWND) -> RECT {
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(window, &mut rect) };
    rect
}

/// Resolves a local-filesystem `SelectionKey` from a hovered item without changing Explorer's focus
/// or querying COM context menus on the UI thread.
pub(crate) fn selection_from_hover(
    defview: HWND,
    child: HWND,
    kind: ChildKind,
    screen: POINT,
    shift: bool,
) -> Option<SelectionKey> {
    if unsafe { WindowFromPoint(screen) } != child {
        return None;
    }
    match kind {
        ChildKind::ListView => {
            let mut info = LVHITTESTINFO {
                pt: screen,
                ..Default::default()
            };
            unsafe { ScreenToClient(child, &mut info.pt) }.ok().ok()?;
            let info_ptr = LPARAM(&mut info as *mut LVHITTESTINFO as isize);
            let item = unsafe { SendMessageW(child, LVM_HITTEST, WPARAM(0), info_ptr) }.0;
            if item < 0 {
                return None;
            }
            let view = ShellView::find(defview)?;
            let fv: IFolderView = view.view.cast().ok()?;
            let folder_key =
                capture_from_view(&view.view, view.folder_view.as_ref(), true, shift).ok()?;
            if !folder_key.is_local_filesystem() {
                return None;
            }
            let rel_raw = unsafe { fv.Item(item as i32) }.ok()?;
            let rel = unsafe { OwnedPidl::from_raw_owned(rel_raw) };
            let full = unsafe { folder_key.folder_pidl().combine_child_raw(rel.as_ptr()) }.ok()?;
            let key = SelectionKey::new(folder_key.folder_pidl().clone(), vec![full], shift);
            key.is_local_filesystem().then_some(key)
        }
        ChildKind::ItemsView => {
            let (mut accessible, mut id) = (None, VARIANT::new());
            unsafe { AccessibleObjectFromPoint(screen, &mut accessible, &mut id) }.ok()?;
            let mut node = Some((accessible?, id));
            let mut item_name = None;
            for _ in 0..6 {
                let Some((acc, cur_id)) = node.take() else {
                    break;
                };
                let role = unsafe { acc.get_accRole(&cur_id) }
                    .ok()
                    .and_then(|v| u32::try_from(&v).ok());
                if role == Some(ROLE_SYSTEM_LISTITEM) {
                    item_name = unsafe { acc.get_accName(&cur_id) }.ok().map(|b| b.to_string());
                    break;
                }
                if matches!(
                    role.and_then(|r| gesture::classify_roles([r])),
                    Some(gesture::Hit::Background | gesture::Hit::Other)
                ) {
                    break;
                }
                node = if is_self(&cur_id) {
                    unsafe { acc.accParent() }
                        .ok()
                        .and_then(|parent| parent.cast().ok())
                        .map(|parent| (parent, self_id()))
                } else {
                    Some((acc, self_id()))
                };
            }
            let name = item_name.filter(|s| !s.is_empty() && !s.contains(['\\', '/', '\0']))?;
            let view = ShellView::find(defview)?;
            let folder_key =
                capture_from_view(&view.view, view.folder_view.as_ref(), true, shift).ok()?;
            if !folder_key.is_local_filesystem() {
                return None;
            }
            let candidate = folder_key.folder_fs_path()?.join(name);
            let key = SelectionKey::from_paths(&[candidate], false, shift).ok()?;
            key.is_local_filesystem().then_some(key)
        }
    }
}

/// Starts inline rename in the live Explorer view only if the single item matches `expected_hash`.
pub(crate) fn trigger_inline_rename(defview: HWND, expected_hash: u64) {
    let Some(view) = ShellView::find(defview) else {
        return;
    };
    let Ok(selection) = capture_from_view(&view.view, view.folder_view.as_ref(), false, false)
    else {
        return;
    };
    if selection.item_count() != 1 {
        return;
    }
    if expected_hash != 0 && selection.item_identity_hash() != expected_hash {
        return;
    }
    if let Some(child_pidl) = selection.primary_child_pidl() {
        let _ = unsafe { view.view.SelectItem(child_pidl, SVSI_EDIT.0 as u32) };
    }
}

/// The screen center of the focused item, where a keyboard-raised menu opens.
pub(crate) fn focused_item_center(child: HWND, kind: ChildKind) -> Option<POINT> {
    if kind == ChildKind::ListView {
        let focused = LPARAM(LVNI_FOCUSED as isize);
        let item = unsafe { SendMessageW(child, LVM_GETNEXTITEM, WPARAM(usize::MAX), focused) }.0;
        let mut rect = RECT {
            left: LVIR_BOUNDS as i32,
            ..Default::default()
        };
        let rect_ptr = LPARAM(&mut rect as *mut RECT as isize);
        let found = item >= 0
            && unsafe { SendMessageW(child, LVM_GETITEMRECT, WPARAM(item as usize), rect_ptr) }.0
                != 0;
        let center = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        return found.then(|| client_to_screen(child, center)).flatten();
    }
    let mut raw = std::ptr::null_mut();
    let client = OBJID_CLIENT.0 as u32;
    unsafe { AccessibleObjectFromWindow(child, client, &IAccessible::IID, &mut raw) }.ok()?;
    let accessible = unsafe { IAccessible::from_raw(raw) };
    let focus = unsafe { accessible.accFocus() }.ok()?;
    // The focus is either a child id of `accessible` or an accessible object of its own.
    let mut object = VARIANT::new();
    let (target, id) =
        match unsafe { VariantChangeType(&mut object, &focus, Default::default(), VT_UNKNOWN) } {
            Ok(()) => (IUnknown::try_from(&object).ok()?.cast().ok()?, self_id()),
            Err(_) => (accessible, focus),
        };
    let (mut left, mut top, mut width, mut height) = (0, 0, 0, 0);
    unsafe { target.accLocation(&mut left, &mut top, &mut width, &mut height, &id) }.ok()?;
    (width > 0 && height > 0).then_some(POINT {
        x: left + width / 2,
        y: top + height / 2,
    })
}
