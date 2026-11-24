//! Lightweight native Win32 Management GUI (`rwm-host.exe --ui` / `rwm ui`).
//! Lets users inspect right-click menu items, toggle visibility, and block slow COM shell extensions.

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rwm_core::build_info::ABI_VERSION;
use rwm_core::classic::{self, ClassicStatus};
use rwm_core::config::{config_path, RwmConfig};
use rwm_core::handlers::{self, BlockedHandler};
use rwm_core::inspect::{inspect_target, HandlerTiming, InspectReport};
use rwm_core::ipc_cache::{notify_host_config_reload, SharedCacheClient};
use rwm_core::shell::{
    enable_dark_mode_menus, item_matches_hidden, menu_item_filter_key, InspectedMenuItem, OleGuard,
    SelectionKey,
};
use rwm_core::warm_pool::is_builtin_windows_handler;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, HINSTANCE,
    HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, FillRect, FrameRect, GetDC, GetMonitorInfoW, MonitorFromWindow,
    ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HBITMAP, HBRUSH, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::Threading::{CreateMutexW, CREATE_NO_WINDOW};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, SetWindowTheme, CDDS_ITEMPREPAINT, CDDS_PREPAINT, CDRF_DODEFAULT,
    CDRF_NOTIFYITEMDRAW, CDRF_SKIPDEFAULT, DRAWITEMSTRUCT, EM_SETCUEBANNER, ICC_LISTVIEW_CLASSES,
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, LVCFMT_LEFT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH,
    LVCOLUMNW, LVIF_STATE, LVIF_TEXT, LVIS_STATEIMAGEMASK, LVITEMW, LVM_DELETEALLITEMS,
    LVM_GETHEADER, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETBKCOLOR, LVM_SETCOLUMNWIDTH,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMW, LVM_SETTEXTBKCOLOR,
    LVM_SETTEXTCOLOR, LVN_ITEMCHANGED, LVS_EX_CHECKBOXES, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_NOCOLUMNHEADER, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL,
    NMCUSTOMDRAW, NMHDR, NMLISTVIEW, NMLVCUSTOMDRAW, NM_CUSTOMDRAW, ODS_SELECTED,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, FileOpenDialog, IFileOpenDialog, RemoveWindowSubclass, SetWindowSubclass,
    ShellExecuteW, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, FindWindowW, GetClientRect, GetMessageW, GetParent, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, KillTimer, LoadCursorW, MessageBoxW, MoveWindow,
    PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, BS_OWNERDRAW,
    EN_CHANGE, ES_AUTOHSCROLL, GWLP_USERDATA, HICON, ICONINFO, IDC_ARROW, IDYES, MB_DEFBUTTON2,
    MB_ICONQUESTION, MB_ICONWARNING, MB_YESNO, MINMAXINFO, MSG, SW_HIDE, SW_RESTORE, SW_SHOW,
    SW_SHOWNORMAL, WM_APP, WM_CLOSE, WM_COMMAND, WM_COPYDATA, WM_CREATE, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_NCDESTROY,
    WM_NOTIFY, WM_SETFONT, WM_SETICON, WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_BORDER, WS_CHILD,
    WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::cli::{InspectArgs, TargetArgs};
use crate::error::{CliError, CliResult, Context};
use crate::probe::{self, ProbeRequest};

pub const MANAGER_WINDOW_CLASS: &str = "RustWindowsMenuManager_v1";
const COPYDATA_TARGET_ITEM: usize = 0x5257_4D01;
const COPYDATA_TARGET_BG: usize = 0x5257_4D02;
const WM_APP_INSPECT_DONE: u32 = WM_APP + 20;
const SEARCH_DEBOUNCE_TIMER: usize = 0x5257_4D60;
const SLOW_THRESHOLD_MS: f64 = 25.0;

const ID_BTN_FILE: usize = 1001;
const ID_BTN_FOLDER: usize = 1002;
const ID_BTN_BG: usize = 1003;
const ID_BTN_REFRESH: usize = 1004;
const ID_TAB_ITEMS: usize = 1010;
const ID_TAB_HANDLERS: usize = 1011;
const ID_TAB_SETTINGS: usize = 1012;
const ID_EDIT_SEARCH: usize = 1020;
const ID_LV_ITEMS: usize = 1021;
const ID_BTN_SHOW_ALL: usize = 1022;
const ID_BTN_PREVIEW: usize = 1023;
const ID_LV_HANDLERS: usize = 1030;
const ID_BTN_BLOCK_SLOW: usize = 1031;
const ID_BTN_UNBLOCK_ALL: usize = 1032;
const ID_LV_SETTINGS: usize = 1040;
const ID_BTN_OPEN_CONFIG: usize = 1041;
const ID_BTN_RESET_CONFIG: usize = 1042;

const ICON_BIG: usize = 1;
const ICON_SMALL: usize = 0;

const SS_LEFT: u32 = 0x0000;
const SS_CENTERIMAGE: u32 = 0x0200;
const SS_ENDELLIPSIS: u32 = 0x4000;

fn wide_null(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

#[inline]
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

#[inline]
fn is_ja() -> bool {
    rwm_core::shell::is_japanese_ui()
}

fn t<'a>(ja: &'a str, en: &'a str) -> &'a str {
    if is_ja() {
        ja
    } else {
        en
    }
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

/// Launches the native Management GUI (`rwm-host.exe --ui`) detached from the caller.
pub fn spawn_manager(target: Option<&Path>, background: bool) -> CliResult<()> {
    let exe = std::env::current_exe().context(|| "cannot locate current executable".to_string())?;
    let host_exe = exe.with_file_name("rwm-host.exe");
    let launcher = if host_exe.is_file() { host_exe } else { exe };
    let mut command = Command::new(&launcher);
    command.arg("--ui");
    if background {
        command.arg("--background");
    }
    if let Some(path) = target {
        command.arg("--target").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .context(|| format!("cannot launch manager GUI via {}", launcher.display()))?;
    Ok(())
}

/// Launches the Management GUI targeting the given [`SelectionKey`].
pub fn spawn_manager_for_selection(selection: &SelectionKey) {
    let _ = spawn_manager(selection.primary_fs_path(), selection.is_background());
}

/// Generates a crisp procedural 32-bit RGBA app icon (rounded blue card with 3 menu bars).
pub fn create_brand_icon(size: i32) -> Option<HICON> {
    let size = size.clamp(16, 128);
    let hdc_screen = unsafe { GetDC(None) };
    let hdc = unsafe { CreateCompatibleDC(hdc_screen) };
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm_color: HBITMAP =
        unsafe { CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbm_color.0));
            let _ = DeleteDC(hdc);
            let _ = ReleaseDC(None, hdc_screen);
        }
        return None;
    }

    let len = (size * size) as usize;
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u32>(), len) };
    let s = size as f32;
    let pad = s * 0.06;
    let radius = s * 0.22;
    let min_x = pad + radius;
    let max_x = s - pad - radius;
    let min_y = pad + radius;
    let max_y = s - pad - radius;

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let cx = fx.clamp(min_x, max_x);
            let cy = fy.clamp(min_y, max_y);
            let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - radius;
            let alpha = (0.5 - dist).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                pixels[(y * size + x) as usize] = 0;
                continue;
            }
            let bar_x0 = s * 0.24;
            let bar_x1 = s * 0.76;
            let in_bar_x = fx >= bar_x0 && fx <= bar_x1;
            let bar1 = in_bar_x && fy >= s * 0.28 && fy <= s * 0.37;
            let bar2 = in_bar_x && fy >= s * 0.46 && fy <= s * 0.55;
            let bar3 = in_bar_x && fy >= s * 0.64 && fy <= s * 0.73;
            let dot = fx >= s * 0.24 && fx <= s * 0.34 && fy >= s * 0.28 && fy <= s * 0.37;

            let (r, g, b) = if dot {
                (96u8, 205u8, 255u8)
            } else if bar1 || bar2 || bar3 {
                (255u8, 255u8, 255u8)
            } else {
                // Smooth top-to-bottom Fluent blue gradient.
                let t = (fy / s).clamp(0.0, 1.0);
                (
                    (14.0 + (0.0 - 14.0) * t) as u8,
                    (138.0 + (103.0 - 138.0) * t) as u8,
                    (230.0 + (192.0 - 230.0) * t) as u8,
                )
            };
            let a = (alpha * 255.0).round() as u32;
            let pr = ((u32::from(r) * a) / 255) & 0xFF;
            let pg = ((u32::from(g) * a) / 255) & 0xFF;
            let pb = ((u32::from(b) * a) / 255) & 0xFF;
            pixels[(y * size + x) as usize] = (a << 24) | (pr << 16) | (pg << 8) | pb;
        }
    }

    let hbm_mask = unsafe { CreateBitmap(size, size, 1, 1, None) };
    let icon_info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let icon = unsafe { CreateIconIndirect(&icon_info) }.ok();
    unsafe {
        if !hbm_mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        }
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        let _ = DeleteDC(hdc);
        let _ = ReleaseDC(None, hdc_screen);
    }
    icon
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItemRow {
    pub label: String,
    pub kind_label: String,
    pub rule_key: String,
    pub canonical_verb: Option<String>,
    pub clean_label: String,
    pub visible: bool,
    pub from_current_target: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HandlerRow {
    pub clsid: String,
    pub name: String,
    pub dll_path: Option<PathBuf>,
    pub warm_ms: Option<f64>,
    pub is_system: bool,
    pub blocked: bool,
    pub added_by_rwm: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    pub title_ja: &'static str,
    pub title_en: &'static str,
    pub desc_ja: &'static str,
    pub desc_en: &'static str,
    pub get: fn(&RwmConfig) -> bool,
    pub set: fn(&mut RwmConfig, bool),
}

pub const SETTING_SPECS: &[SettingSpec] = &[
    SettingSpec {
        title_ja: "RWM を有効にする",
        title_en: "Enable RustWindowsMenu",
        desc_ja: "右クリック高速化・メニューカスタマイズ全体のマスタースイッチ",
        desc_en: "Master switch for accelerated and customized Explorer menus",
        get: |c| c.enabled,
        set: |c, v| c.enabled = v,
    },
    SettingSpec {
        title_ja: "右クリックに「メニューをカスタマイズ」を表示",
        title_en: "Show 'Customize Menu (RWM)...' in Right-Click Menu",
        desc_ja: "右クリックメニューの最下部にこの管理画面を開く項目を追加します",
        desc_en: "Appends a quick launcher for this Manager window at the bottom",
        get: |c| c.show_customize_entry,
        set: |c, v| c.show_customize_entry = v,
    },
    SettingSpec {
        title_ja: "ホバー先読みキャッシュ (推奨)",
        title_en: "Hover Speculative Prebuild",
        desc_ja: "ファイル上でカーソルを少し止めると裏でメニューを事前構築します",
        desc_en: "Prebuilds menus off Explorer's UI thread after brief cursor hover",
        get: |c| c.hover_prebuild,
        set: |c, v| c.hover_prebuild = v,
    },
    SettingSpec {
        title_ja: "選択変更時の先読みキャッシュ",
        title_en: "Selection Speculative Prebuild",
        desc_ja: "左クリックや矢印キーで選択が移動した際に先読み構築を行います",
        desc_en: "Prebuilds menus shortly after selection changes in Explorer",
        get: |c| c.selection_prebuild,
        set: |c, v| c.selection_prebuild = v,
    },
    SettingSpec {
        title_ja: "右クリック押下時 (t=0 ms) ビルド",
        title_en: "Prebuild on Right-Button Down (t=0 ms)",
        desc_ja: "右ボタンを押した瞬間に構築を開始し、離した時の待ちを減らします",
        desc_en: "Starts building as soon as RMB is pressed before release",
        get: |c| c.prebuild_on_rbuttondown,
        set: |c, v| c.prebuild_on_rbuttondown = v,
    },
    SettingSpec {
        title_ja: "メニュー表示アニメーションを無効化",
        title_en: "Disable Menu Popup Animation",
        desc_ja: "フェードアニメーションを省略して即座にメニューを表示します",
        desc_en: "Renders popup menus immediately without fade delay",
        get: |c| c.no_animation,
        set: |c, v| c.no_animation = v,
    },
    SettingSpec {
        title_ja: "常駐ホスト (rwm-host) の投機キャッシュ",
        title_en: "Background Host Speculative Cache",
        desc_ja: "UI スレッド外で事前構築したメニューを共有メモリ経由で即時表示します",
        desc_en: "Presents off-thread prebuilt menus with near-zero UI wait",
        get: |c| c.host_menu_cache,
        set: |c, v| c.host_menu_cache = v,
    },
    SettingSpec {
        title_ja: "シェル拡張 DLL をメモリにピン留め",
        title_en: "Pin Shell Extension DLLs",
        desc_ja: "拡張 DLL のアンロード・再ロードによる遅延を防ぎます",
        desc_en: "Keeps handler DLLs resident to avoid repeated cold loads",
        get: |c| c.pin_handler_dlls,
        set: |c, v| c.pin_handler_dlls = v,
    },
];

pub fn encode_copydata_payload(path: &Path, background: bool) -> (usize, Vec<u8>) {
    let tag = if background {
        COPYDATA_TARGET_BG
    } else {
        COPYDATA_TARGET_ITEM
    };
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    (tag, bytes)
}

pub fn decode_copydata_payload(dw_data: usize, bytes: &[u8]) -> Option<(PathBuf, bool)> {
    let background = match dw_data {
        COPYDATA_TARGET_ITEM => false,
        COPYDATA_TARGET_BG => true,
        _ => return None,
    };
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    Some((PathBuf::from(text), background))
}

fn matches_search(query: &str, haystack_a: &str, haystack_b: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let q_lower = q.to_lowercase();
    haystack_a.to_lowercase().contains(&q_lower) || haystack_b.to_lowercase().contains(&q_lower)
}

pub fn build_menu_item_rows(
    root_items: &[InspectedMenuItem],
    hidden_items: &[String],
    search_query: &str,
    ja: bool,
) -> Vec<MenuItemRow> {
    let mut rows = Vec::new();
    let mut matched_rules = vec![false; hidden_items.len()];

    for item in root_items {
        if item.is_separator {
            continue;
        }
        let display = if !item.clean_label.is_empty() {
            item.clean_label.clone()
        } else {
            item.label.clone()
        };
        let Some(rule_key) = menu_item_filter_key(item.canonical_verb.as_deref(), &item.clean_label)
        else {
            continue;
        };
        let hidden = item_matches_hidden(
            hidden_items,
            item.canonical_verb.as_deref(),
            &item.clean_label,
        );
        if hidden {
            for (idx, rule) in hidden_items.iter().enumerate() {
                if item_matches_hidden(
                    std::slice::from_ref(rule),
                    item.canonical_verb.as_deref(),
                    &item.clean_label,
                ) {
                    matched_rules[idx] = true;
                }
            }
        }
        let kind_label = match (item.is_submenu, ja) {
            (true, true) => "サブメニュー",
            (true, false) => "Submenu",
            (false, true) => "コマンド",
            (false, false) => "Command",
        }
        .to_string();

        if matches_search(search_query, &display, &rule_key) {
            rows.push(MenuItemRow {
                label: display,
                kind_label,
                rule_key,
                canonical_verb: item.canonical_verb.clone(),
                clean_label: item.clean_label.clone(),
                visible: !hidden,
                from_current_target: true,
            });
        }
    }

    for (idx, rule) in hidden_items.iter().enumerate() {
        if matched_rules[idx] {
            continue;
        }
        let display = if let Some((prefix, rest)) = rule.split_once(':') {
            if prefix.eq_ignore_ascii_case("label") || prefix.eq_ignore_ascii_case("verb") {
                rest.trim().to_string()
            } else {
                rule.trim().to_string()
            }
        } else {
            rule.trim().to_string()
        };
        let kind_label = if ja {
            "他ターゲットで非表示"
        } else {
            "Hidden elsewhere"
        }
        .to_string();
        if matches_search(search_query, &display, rule) {
            rows.push(MenuItemRow {
                label: display,
                kind_label,
                rule_key: rule.clone(),
                canonical_verb: None,
                clean_label: String::new(),
                visible: false,
                from_current_target: false,
            });
        }
    }

    rows
}

fn handler_tier(row: &HandlerRow) -> u8 {
    if !row.is_system && row.warm_ms.is_some_and(|ms| ms >= SLOW_THRESHOLD_MS) {
        0
    } else if row.blocked {
        1
    } else if !row.is_system {
        2
    } else {
        3
    }
}

pub fn merge_and_sort_handlers(
    inspected: &[HandlerTiming],
    blocked_list: &[BlockedHandler],
) -> Vec<HandlerRow> {
    let mut rows = Vec::with_capacity(inspected.len() + blocked_list.len());
    let mut seen_clsids = Vec::<String>::with_capacity(inspected.len() + blocked_list.len());

    for h in inspected {
        let added_by_rwm = blocked_list
            .iter()
            .find(|b| b.clsid.eq_ignore_ascii_case(&h.clsid))
            .is_some_and(|b| b.added_by_rwm);
        let is_system = h
            .dll_path
            .as_deref()
            .is_some_and(is_builtin_windows_handler);
        seen_clsids.push(h.clsid.to_ascii_uppercase());
        rows.push(HandlerRow {
            clsid: h.clsid.clone(),
            name: h.name.clone(),
            dll_path: h.dll_path.clone(),
            warm_ms: h.warm_ms,
            is_system,
            blocked: h.blocked,
            added_by_rwm,
        });
    }

    for b in blocked_list {
        let upper = b.clsid.to_ascii_uppercase();
        if seen_clsids.iter().any(|existing| existing == &upper) {
            continue;
        }
        seen_clsids.push(upper);
        rows.push(HandlerRow {
            clsid: b.clsid.clone(),
            name: b
                .name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| b.clsid.clone()),
            dll_path: None,
            warm_ms: None,
            is_system: false,
            blocked: true,
            added_by_rwm: b.added_by_rwm,
        });
    }

    rows.sort_by(|a, b| {
        handler_tier(a)
            .cmp(&handler_tier(b))
            .then_with(|| {
                b.warm_ms
                    .unwrap_or(-1.0)
                    .total_cmp(&a.warm_ms.unwrap_or(-1.0))
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.clsid.cmp(&b.clsid))
    });
    rows
}

#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    bg: COLORREF,
    card_bg: COLORREF,
    btn_bg: COLORREF,
    btn_pressed: COLORREF,
    tab_bg: COLORREF,
    tab_active_bg: COLORREF,
    accent: COLORREF,
    accent_fg: COLORREF,
    fg: COLORREF,
    muted: COLORREF,
    warn_fg: COLORREF,
    border: COLORREF,
    list_bg: COLORREF,
    header_bg: COLORREF,
}

impl Theme {
    fn detect() -> Self {
        let mut val = 1u32;
        let mut size = size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
                w!("AppsUseLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut val).cast()),
                Some(&mut size),
            )
        };
        let dark = status == ERROR_SUCCESS && val == 0;

        if dark {
            Self {
                dark: true,
                bg: rgb(0x20, 0x20, 0x20),
                card_bg: rgb(0x2A, 0x2A, 0x2A),
                btn_bg: rgb(0x35, 0x35, 0x35),
                btn_pressed: rgb(0x45, 0x45, 0x45),
                tab_bg: rgb(0x24, 0x24, 0x24),
                tab_active_bg: rgb(0x32, 0x32, 0x32),
                accent: rgb(0x60, 0xCD, 0xFF),
                accent_fg: rgb(0x10, 0x10, 0x10),
                fg: rgb(0xF3, 0xF3, 0xF3),
                muted: rgb(0xA6, 0xA6, 0xA6),
                warn_fg: rgb(0xFF, 0xC8, 0x3D),
                border: rgb(0x44, 0x44, 0x44),
                list_bg: rgb(0x25, 0x25, 0x25),
                header_bg: rgb(0x2E, 0x2E, 0x2E),
            }
        } else {
            Self {
                dark: false,
                bg: rgb(0xF3, 0xF3, 0xF3),
                card_bg: rgb(0xFF, 0xFF, 0xFF),
                btn_bg: rgb(0xE8, 0xE8, 0xE8),
                btn_pressed: rgb(0xD4, 0xD4, 0xD4),
                tab_bg: rgb(0xEB, 0xEB, 0xEB),
                tab_active_bg: rgb(0xFF, 0xFF, 0xFF),
                accent: rgb(0x00, 0x67, 0xC0),
                accent_fg: rgb(0xFF, 0xFF, 0xFF),
                fg: rgb(0x1B, 0x1B, 0x1B),
                muted: rgb(0x5E, 0x5E, 0x5E),
                warn_fg: rgb(0x9D, 0x4E, 0x00),
                border: rgb(0xCF, 0xCF, 0xCF),
                list_bg: rgb(0xFF, 0xFF, 0xFF),
                header_bg: rgb(0xEE, 0xEE, 0xEE),
            }
        }
    }
}

struct GdiResources {
    bg_brush: HBRUSH,
    card_brush: HBRUSH,
    btn_brush: HBRUSH,
    btn_pressed_brush: HBRUSH,
    tab_brush: HBRUSH,
    tab_active_brush: HBRUSH,
    accent_brush: HBRUSH,
    border_brush: HBRUSH,
    header_brush: HBRUSH,
    ui_font: HFONT,
    bold_font: HFONT,
    small_font: HFONT,
    big_icon: Option<HICON>,
    small_icon: Option<HICON>,
}

impl GdiResources {
    fn new(theme: &Theme, dpi: u32) -> Self {
        Self {
            bg_brush: unsafe { CreateSolidBrush(theme.bg) },
            card_brush: unsafe { CreateSolidBrush(theme.card_bg) },
            btn_brush: unsafe { CreateSolidBrush(theme.btn_bg) },
            btn_pressed_brush: unsafe { CreateSolidBrush(theme.btn_pressed) },
            tab_brush: unsafe { CreateSolidBrush(theme.tab_bg) },
            tab_active_brush: unsafe { CreateSolidBrush(theme.tab_active_bg) },
            accent_brush: unsafe { CreateSolidBrush(theme.accent) },
            border_brush: unsafe { CreateSolidBrush(theme.border) },
            header_brush: unsafe { CreateSolidBrush(theme.header_bg) },
            ui_font: create_ui_font(dpi, 15, 400),
            bold_font: create_ui_font(dpi, 15, 600),
            small_font: create_ui_font(dpi, 13, 400),
            big_icon: create_brand_icon(32),
            small_icon: create_brand_icon(16),
        }
    }
}

impl Drop for GdiResources {
    fn drop(&mut self) {
        unsafe {
            for brush in [
                self.bg_brush,
                self.card_brush,
                self.btn_brush,
                self.btn_pressed_brush,
                self.tab_brush,
                self.tab_active_brush,
                self.accent_brush,
                self.border_brush,
                self.header_brush,
            ] {
                if !brush.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(brush.0));
                }
            }
            for font in [self.ui_font, self.bold_font, self.small_font] {
                if !font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(font.0));
                }
            }
            if let Some(icon) = self.big_icon {
                let _ = DestroyIcon(icon);
            }
            if let Some(icon) = self.small_icon {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

fn create_ui_font(dpi: u32, px_height: i32, weight: i32) -> HFONT {
    let height = -px(px_height, dpi);
    let face = wide_null(t("Yu Gothic UI", "Segoe UI"));
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            5, // CLEARTYPE_QUALITY
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    }
}

#[inline]
fn px(logical: i32, dpi: u32) -> i32 {
    (logical * dpi as i32 + 48) / 96
}

struct InspectResult {
    gen: u64,
    report: Result<InspectReport, String>,
    blocked: Vec<BlockedHandler>,
    classic: Option<ClassicStatus>,
    host_running: bool,
    host_cache_active: bool,
}

#[derive(Default)]
struct Controls {
    lbl_target: HWND,
    btn_file: HWND,
    btn_folder: HWND,
    btn_bg: HWND,
    btn_refresh: HWND,
    tab_items: HWND,
    tab_handlers: HWND,
    tab_settings: HWND,
    // Tab 0
    lbl_items_hint: HWND,
    edit_search: HWND,
    lbl_items_count: HWND,
    lv_items: HWND,
    hdr_items: HWND,
    btn_show_all: HWND,
    btn_preview: HWND,
    // Tab 1
    lbl_handlers_hint: HWND,
    lv_handlers: HWND,
    hdr_handlers: HWND,
    btn_block_slow: HWND,
    btn_unblock_all: HWND,
    // Tab 2
    lv_settings: HWND,
    lbl_system_info: HWND,
    btn_open_config: HWND,
    btn_reset_config: HWND,
    // Footer
    lbl_status: HWND,
}

const MANAGER_MUTEX_NAME: &str = r"Local\RustWindowsMenu_ManagerUI_v1";

struct MutexHandleGuard(HANDLE);
impl Drop for MutexHandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ManagerState {
    hwnd: HWND,
    dpi: u32,
    theme: Theme,
    gdi: GdiResources,
    controls: Controls,
    active_tab: usize,
    target_path: PathBuf,
    background: bool,
    config: RwmConfig,
    config_load_error: Option<String>,
    report: Option<InspectReport>,
    blocked_list: Vec<BlockedHandler>,
    classic_status: Option<ClassicStatus>,
    host_running: bool,
    host_cache_active: bool,
    rows_items: Vec<MenuItemRow>,
    rows_handlers: Vec<HandlerRow>,
    inspect_gen: u64,
    inspect_loading: bool,
    populating: bool,
    modal_open: bool,
    pending_inspect: Arc<Mutex<Option<InspectResult>>>,
    status_override: Option<String>,
}

fn default_target_path() -> PathBuf {
    let cfg = config_path();
    if cfg.is_file() {
        return cfg;
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(r"C:\Windows\notepad.exe"))
}

fn resolve_target(path: Option<PathBuf>, background: bool) -> (PathBuf, bool) {
    let resolved = path
        .filter(|p| p.exists())
        .unwrap_or_else(default_target_path);
    let is_dir = resolved.is_dir();
    (resolved, background && is_dir)
}

fn send_target_to_existing(existing: HWND, path: Option<PathBuf>, background: bool) {
    unsafe {
        let _ = ShowWindow(existing, SW_RESTORE);
        let _ = SetForegroundWindow(existing);
    }
    let Some(path) = path else {
        return;
    };
    let (target, bg) = resolve_target(Some(path), background);
    let (tag, bytes) = encode_copydata_payload(&target, bg);
    let cds = COPYDATASTRUCT {
        dwData: tag,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut c_void,
    };
    unsafe {
        let _ = SendMessageW(
            existing,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(std::ptr::from_ref(&cds) as isize),
        );
    }
}

fn find_existing_manager_window() -> Option<HWND> {
    unsafe { FindWindowW(w!("RustWindowsMenuManager_v1"), PCWSTR::null()) }
        .ok()
        .filter(|h| !h.is_invalid())
}

pub fn run(initial_target: Option<PathBuf>, initial_background: bool) -> CliResult<ExitCode> {
    let mutex_name = wide_null(MANAGER_MUTEX_NAME);
    let mutex_handle = unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) }
        .context(|| "cannot create manager single-instance mutex".to_string())?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _mutex_guard = MutexHandleGuard(mutex_handle);
    if already_exists {
        for _ in 0..50 {
            if let Some(hwnd) = find_existing_manager_window() {
                send_target_to_existing(hwnd, initial_target, initial_background);
                return Ok(ExitCode::SUCCESS);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(hwnd) = find_existing_manager_window() {
        send_target_to_existing(hwnd, initial_target, initial_background);
        return Ok(ExitCode::SUCCESS);
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
    let _dark_menus = enable_dark_mode_menus();
    let _ole = OleGuard::initialize().map_err(|e| CliError::new(e.to_string()))?;

    let instance =
        unsafe { GetModuleHandleW(None) }.context(|| "GetModuleHandleW failed".to_string())?;
    let class_name = wide_null(MANAGER_WINDOW_CLASS);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let theme = Theme::detect();
    let wnd_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(manager_wnd_proc),
        hInstance: HINSTANCE::from(instance),
        hCursor: cursor,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wnd_class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }

    let (target_path, background) = resolve_target(initial_target, initial_background);
    let (config, config_load_error) = match RwmConfig::load_or_default() {
        Ok(cfg) => (cfg, None),
        Err(err) => (RwmConfig::default(), Some(err.to_string())),
    };
    let title = wide_null(t(
        "RustWindowsMenu - 右クリックメニュー管理",
        "RustWindowsMenu - Context Menu Manager",
    ));
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            100,
            100,
            920,
            660,
            None,
            None,
            instance,
            None,
        )
    }
    .context(|| "cannot create manager window".to_string())?;

    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let gdi = GdiResources::new(&theme, dpi);
    let mut state = Box::new(ManagerState {
        hwnd,
        dpi,
        theme,
        gdi,
        controls: Controls::default(),
        active_tab: 0,
        target_path,
        background,
        config,
        config_load_error,
        report: None,
        blocked_list: handlers::list_blocked().unwrap_or_default(),
        classic_status: classic::status().ok(),
        host_running: crate::host::is_host_running(crate::host::HOST_MUTEX_NAME),
        host_cache_active: SharedCacheClient::open().is_some_and(|c| c.is_valid()),
        rows_items: Vec::new(),
        rows_handlers: Vec::new(),
        inspect_gen: 0,
        inspect_loading: false,
        populating: false,
        modal_open: false,
        pending_inspect: Arc::new(Mutex::new(None)),
        status_override: None,
    });

    if theme.dark {
        let dark_val = 1i32;
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                std::ptr::from_ref(&dark_val).cast(),
                size_of::<i32>() as u32,
            );
        }
    }
    if let Some(icon) = state.gdi.big_icon {
        unsafe { SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG), LPARAM(icon.0 as isize)) };
    }
    if let Some(icon) = state.gdi.small_icon {
        unsafe { SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL), LPARAM(icon.0 as isize)) };
    }

    create_controls(&mut state, HINSTANCE::from(instance))?;
    resize_and_center_window(&state);
    relayout(&mut state);
    populate_settings_list(&mut state);
    update_tab_visibility(&mut state);
    update_header_text(&state);

    let state_ptr = Box::into_raw(state);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        start_background_inspect(&mut *state_ptr);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn create_child(
    parent: HWND,
    instance: HINSTANCE,
    class: &str,
    text: &str,
    style: u32,
    id: usize,
    font: HFONT,
) -> CliResult<HWND> {
    let class_w = wide_null(class);
    let text_w = wide_null(text);
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            0,
            0,
            100,
            24,
            parent,
            windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|e| CliError::new(format!("cannot create control {class}: {e}")))?;
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
    Ok(hwnd)
}

fn setup_listview(lv: HWND, theme: &Theme, no_header: bool) -> HWND {
    let ex_style = LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
    unsafe {
        SendMessageW(
            lv,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            WPARAM(ex_style as usize),
            LPARAM(ex_style as isize),
        );
        SendMessageW(lv, LVM_SETBKCOLOR, WPARAM(0), LPARAM(theme.list_bg.0 as isize));
        SendMessageW(
            lv,
            LVM_SETTEXTBKCOLOR,
            WPARAM(0),
            LPARAM(theme.list_bg.0 as isize),
        );
        SendMessageW(lv, LVM_SETTEXTCOLOR, WPARAM(0), LPARAM(theme.fg.0 as isize));
        let _ = SetWindowTheme(
            lv,
            if theme.dark {
                w!("DarkMode_Explorer")
            } else {
                w!("Explorer")
            },
            PCWSTR::null(),
        );
    }
    let header = HWND(unsafe { SendMessageW(lv, LVM_GETHEADER, WPARAM(0), LPARAM(0)) }.0 as *mut _);
    if !no_header && !header.is_invalid() {
        unsafe {
            let _ = SetWindowTheme(
                header,
                if theme.dark {
                    w!("DarkMode_ItemsView")
                } else {
                    w!("ItemsView")
                },
                PCWSTR::null(),
            );
            let _ = SetWindowSubclass(lv, Some(listview_header_subclass), 1, 0);
        }
    }
    header
}

fn insert_column(lv: HWND, index: i32, text: &str, width: i32) {
    let mut wide = wide_null(text);
    let col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
        fmt: LVCFMT_LEFT,
        cx: width,
        pszText: PWSTR(wide.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            lv,
            LVM_INSERTCOLUMNW,
            WPARAM(index as usize),
            LPARAM(std::ptr::from_ref(&col) as isize),
        );
    }
}

fn set_column_width(lv: HWND, index: i32, width: i32) {
    unsafe {
        SendMessageW(
            lv,
            LVM_SETCOLUMNWIDTH,
            WPARAM(index as usize),
            LPARAM(width as isize),
        );
    }
}

fn set_item_check(lv: HWND, row: i32, checked: bool) {
    let state = if checked { 2u32 << 12 } else { 1u32 << 12 };
    let item = LVITEMW {
        mask: LVIF_STATE,
        state: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(state),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            lv,
            LVM_SETITEMSTATE,
            WPARAM(row as usize),
            LPARAM(std::ptr::from_ref(&item) as isize),
        );
    }
}

fn insert_lv_row(lv: HWND, row: i32, cols: &[&str], checked: bool) {
    let mut c0 = wide_null(cols.first().copied().unwrap_or_default());
    let item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: 0,
        pszText: PWSTR(c0.as_mut_ptr()),
        ..Default::default()
    };
    let inserted = unsafe {
        SendMessageW(
            lv,
            LVM_INSERTITEMW,
            WPARAM(0),
            LPARAM(std::ptr::from_ref(&item) as isize),
        )
    }
    .0 as i32;
    if inserted >= 0 {
        for (idx, text) in cols.iter().enumerate().skip(1) {
            let mut wide = wide_null(text);
            let sub = LVITEMW {
                mask: LVIF_TEXT,
                iItem: inserted,
                iSubItem: idx as i32,
                pszText: PWSTR(wide.as_mut_ptr()),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    lv,
                    LVM_SETITEMW,
                    WPARAM(0),
                    LPARAM(std::ptr::from_ref(&sub) as isize),
                );
            }
        }
        set_item_check(lv, inserted, checked);
    }
}

fn update_lv_subitem(lv: HWND, row: i32, col: i32, text: &str) {
    let mut wide = wide_null(text);
    let sub = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: col,
        pszText: PWSTR(wide.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            lv,
            LVM_SETITEMW,
            WPARAM(0),
            LPARAM(std::ptr::from_ref(&sub) as isize),
        );
    }
}

fn create_controls(state: &mut ManagerState, instance: HINSTANCE) -> CliResult<()> {
    let parent = state.hwnd;
    let ui_font = state.gdi.ui_font;
    let bold_font = state.gdi.bold_font;
    let small_font = state.gdi.small_font;

    let base = (WS_CHILD | WS_VISIBLE).0;
    let btn_style = base | WS_TABSTOP.0 | BS_OWNERDRAW as u32;
    let lbl_style = base | SS_LEFT | SS_CENTERIMAGE | SS_ENDELLIPSIS;
    let hint_style = base | SS_LEFT | SS_CENTERIMAGE;
    let lv_style = base | WS_TABSTOP.0 | WS_BORDER.0 | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS;

    let lbl_target = create_child(parent, instance, "STATIC", "", lbl_style, 0, bold_font)?;
    let btn_file = create_child(
        parent,
        instance,
        "BUTTON",
        t("ファイル選択...", "Pick File..."),
        btn_style,
        ID_BTN_FILE,
        ui_font,
    )?;
    let btn_folder = create_child(
        parent,
        instance,
        "BUTTON",
        t("フォルダ選択...", "Pick Folder..."),
        btn_style,
        ID_BTN_FOLDER,
        ui_font,
    )?;
    let btn_bg = create_child(
        parent,
        instance,
        "BUTTON",
        t("背景モード: OFF", "Background: OFF"),
        btn_style,
        ID_BTN_BG,
        ui_font,
    )?;
    let btn_refresh = create_child(
        parent,
        instance,
        "BUTTON",
        t("再計測", "Refresh"),
        btn_style,
        ID_BTN_REFRESH,
        ui_font,
    )?;

    let tab_items = create_child(
        parent,
        instance,
        "BUTTON",
        t("1. 表示するメニュー項目", "1. Menu Items"),
        btn_style,
        ID_TAB_ITEMS,
        bold_font,
    )?;
    let tab_handlers = create_child(
        parent,
        instance,
        "BUTTON",
        t("2. シェル拡張 (高速化)", "2. Shell Extensions"),
        btn_style,
        ID_TAB_HANDLERS,
        bold_font,
    )?;
    let tab_settings = create_child(
        parent,
        instance,
        "BUTTON",
        t("3. 動作設定", "3. Settings"),
        btn_style,
        ID_TAB_SETTINGS,
        bold_font,
    )?;

    // Tab 0
    let lbl_items_hint = create_child(
        parent,
        instance,
        "STATIC",
        t(
            "チェックを外した項目は右クリックメニューから隠れます (即時反映・いつでも再表示できます)",
            "Unchecked items are hidden from right-click menus immediately and can be restored anytime.",
        ),
        hint_style,
        0,
        small_font,
    )?;
    let edit_search = create_child(
        parent,
        instance,
        "EDIT",
        "",
        base | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        ID_EDIT_SEARCH,
        ui_font,
    )?;
    let cue = wide_null(t("項目名・識別キーで絞り込み...", "Filter by label or key..."));
    unsafe {
        SendMessageW(
            edit_search,
            EM_SETCUEBANNER,
            WPARAM(1),
            LPARAM(cue.as_ptr() as isize),
        );
        let _ = SetWindowTheme(
            edit_search,
            if state.theme.dark {
                w!("DarkMode_CFD")
            } else {
                w!("Explorer")
            },
            PCWSTR::null(),
        );
    }
    let lbl_items_count = create_child(parent, instance, "STATIC", "", hint_style, 0, small_font)?;
    let lv_items = create_child(parent, instance, "SysListView32", "", lv_style, ID_LV_ITEMS, ui_font)?;
    let hdr_items = setup_listview(lv_items, &state.theme, false);
    insert_column(lv_items, 0, t("表示 / メニュー項目", "Show / Menu Item"), 360);
    insert_column(lv_items, 1, t("種類", "Kind"), 140);
    insert_column(lv_items, 2, t("識別キー", "Filter Key"), 320);

    let btn_show_all = create_child(
        parent,
        instance,
        "BUTTON",
        t("すべて表示に戻す", "Show All Items"),
        btn_style,
        ID_BTN_SHOW_ALL,
        ui_font,
    )?;
    let btn_preview = create_child(
        parent,
        instance,
        "BUTTON",
        t("メニューをテスト表示", "Preview Menu"),
        btn_style,
        ID_BTN_PREVIEW,
        ui_font,
    )?;

    // Tab 1
    let lbl_handlers_hint = create_child(
        parent,
        instance,
        "STATIC",
        t(
            "チェックを外した COM シェル拡張は右クリッククエリから除外されます (システムの重要拡張は警告付き)",
            "Unchecked COM shell extensions are blocked from right-click menu queries (system DLLs ask before blocking).",
        ),
        hint_style,
        0,
        small_font,
    )?;
    let lv_handlers = create_child(
        parent,
        instance,
        "SysListView32",
        "",
        lv_style,
        ID_LV_HANDLERS,
        ui_font,
    )?;
    let hdr_handlers = setup_listview(lv_handlers, &state.theme, false);
    insert_column(lv_handlers, 0, t("有効 / シェル拡張名", "Enabled / Extension"), 300);
    insert_column(lv_handlers, 1, t("時間", "Warm Time"), 95);
    insert_column(lv_handlers, 2, t("区分", "Category"), 155);
    insert_column(lv_handlers, 3, "DLL / CLSID", 300);

    let btn_block_slow = create_child(
        parent,
        instance,
        "BUTTON",
        t(
            "遅いサードパーティ拡張をまとめて OFF (25 ms 以上)",
            "Disable Slow 3rd-Party (>= 25 ms)",
        ),
        btn_style,
        ID_BTN_BLOCK_SLOW,
        ui_font,
    )?;
    let btn_unblock_all = create_child(
        parent,
        instance,
        "BUTTON",
        t("RWM のブロックをすべて解除", "Restore RWM-Blocked"),
        btn_style,
        ID_BTN_UNBLOCK_ALL,
        ui_font,
    )?;

    // Tab 2
    let lv_settings = create_child(
        parent,
        instance,
        "SysListView32",
        "",
        lv_style | LVS_NOCOLUMNHEADER,
        ID_LV_SETTINGS,
        ui_font,
    )?;
    setup_listview(lv_settings, &state.theme, true);
    insert_column(lv_settings, 0, t("設定", "Setting"), 320);
    insert_column(lv_settings, 1, t("説明", "Description"), 520);

    let lbl_system_info = create_child(parent, instance, "STATIC", "", base | SS_LEFT, 0, small_font)?;
    let btn_open_config = create_child(
        parent,
        instance,
        "BUTTON",
        t("config.toml を開く", "Open config.toml"),
        btn_style,
        ID_BTN_OPEN_CONFIG,
        ui_font,
    )?;
    let btn_reset_config = create_child(
        parent,
        instance,
        "BUTTON",
        t("設定を初期値に戻す", "Reset to Defaults"),
        btn_style,
        ID_BTN_RESET_CONFIG,
        ui_font,
    )?;

    // Footer
    let lbl_status = create_child(parent, instance, "STATIC", "", lbl_style, 0, small_font)?;

    state.controls = Controls {
        lbl_target,
        btn_file,
        btn_folder,
        btn_bg,
        btn_refresh,
        tab_items,
        tab_handlers,
        tab_settings,
        lbl_items_hint,
        edit_search,
        lbl_items_count,
        lv_items,
        hdr_items,
        btn_show_all,
        btn_preview,
        lbl_handlers_hint,
        lv_handlers,
        hdr_handlers,
        btn_block_slow,
        btn_unblock_all,
        lv_settings,
        lbl_system_info,
        btn_open_config,
        btn_reset_config,
        lbl_status,
    };
    Ok(())
}

fn resize_and_center_window(state: &ManagerState) {
    let w = px(920, state.dpi);
    let h = px(660, state.dpi);
    let monitor = unsafe { MonitorFromWindow(state.hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (x, y) = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let work = info.rcWork;
        let cx = work.left + ((work.right - work.left - w) / 2).max(0);
        let cy = work.top + ((work.bottom - work.top - h) / 2).max(0);
        (cx, cy)
    } else {
        (100, 100)
    };
    unsafe {
        let _ = MoveWindow(state.hwnd, x, y, w, h, true);
    }
}

fn move_ctrl(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w.max(10), h.max(10), true);
    }
}

fn show_ctrl(hwnd: HWND, visible: bool) {
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
}

fn relayout(state: &mut ManagerState) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(state.hwnd, &mut rc);
    }
    let cw = (rc.right - rc.left).max(px(640, state.dpi));
    let ch = (rc.bottom - rc.top).max(px(460, state.dpi));
    let d = state.dpi;
    let pad = px(14, d);
    let gap = px(8, d);
    let btn_h = px(30, d);
    let tab_h = px(34, d);
    let status_h = px(24, d);
    let c = &state.controls;

    // Header row
    let btn_refresh_w = px(84, d);
    let btn_bg_w = px(124, d);
    let btn_folder_w = px(112, d);
    let btn_file_w = px(112, d);
    let buttons_total = btn_file_w + btn_folder_w + btn_bg_w + btn_refresh_w + gap * 3;
    let target_w = (cw - pad * 2 - buttons_total - gap).max(px(120, d));

    move_ctrl(c.lbl_target, pad, pad, target_w, btn_h);
    let mut bx = pad + target_w + gap;
    move_ctrl(c.btn_file, bx, pad, btn_file_w, btn_h);
    bx += btn_file_w + gap;
    move_ctrl(c.btn_folder, bx, pad, btn_folder_w, btn_h);
    bx += btn_folder_w + gap;
    move_ctrl(c.btn_bg, bx, pad, btn_bg_w, btn_h);
    bx += btn_bg_w + gap;
    move_ctrl(c.btn_refresh, bx, pad, btn_refresh_w, btn_h);

    // Tab strip
    let tab_y = pad + btn_h + px(10, d);
    let tab_w = (cw - pad * 2 - gap * 2) / 3;
    move_ctrl(c.tab_items, pad, tab_y, tab_w, tab_h);
    move_ctrl(c.tab_handlers, pad + tab_w + gap, tab_y, tab_w, tab_h);
    move_ctrl(
        c.tab_settings,
        pad + (tab_w + gap) * 2,
        tab_y,
        cw - pad * 2 - (tab_w + gap) * 2,
        tab_h,
    );

    let body_y = tab_y + tab_h + px(10, d);
    let status_y = ch - pad - status_h;
    let footer_btn_y = status_y - gap - btn_h;
    let content_w = cw - pad * 2;
    let hint_h = px(22, d);

    // Tab 0 layout
    let search_y = body_y + hint_h + px(6, d);
    let search_h = px(26, d);
    let count_w = px(260, d);
    let search_w = (content_w - count_w - gap).max(px(160, d));
    move_ctrl(c.lbl_items_hint, pad, body_y, content_w, hint_h);
    move_ctrl(c.edit_search, pad, search_y, search_w, search_h);
    move_ctrl(c.lbl_items_count, pad + search_w + gap, search_y, count_w, search_h);
    let lv0_y = search_y + search_h + gap;
    let lv0_h = (footer_btn_y - gap - lv0_y).max(px(120, d));
    move_ctrl(c.lv_items, pad, lv0_y, content_w, lv0_h);
    let c0_w = (content_w * 46) / 100;
    let c1_w = (content_w * 18) / 100;
    let c2_w = (content_w - c0_w - c1_w - px(24, d)).max(px(140, d));
    set_column_width(c.lv_items, 0, c0_w);
    set_column_width(c.lv_items, 1, c1_w);
    set_column_width(c.lv_items, 2, c2_w);
    let act_w = px(180, d);
    move_ctrl(c.btn_show_all, pad, footer_btn_y, act_w, btn_h);
    move_ctrl(c.btn_preview, pad + act_w + gap, footer_btn_y, act_w, btn_h);

    // Tab 1 layout
    move_ctrl(c.lbl_handlers_hint, pad, body_y, content_w, hint_h);
    let lv1_y = body_y + hint_h + px(6, d);
    let lv1_h = (footer_btn_y - gap - lv1_y).max(px(120, d));
    move_ctrl(c.lv_handlers, pad, lv1_y, content_w, lv1_h);
    let h0_w = (content_w * 36) / 100;
    let h1_w = px(92, d);
    let h2_w = px(160, d);
    let h3_w = (content_w - h0_w - h1_w - h2_w - px(24, d)).max(px(150, d));
    set_column_width(c.lv_handlers, 0, h0_w);
    set_column_width(c.lv_handlers, 1, h1_w);
    set_column_width(c.lv_handlers, 2, h2_w);
    set_column_width(c.lv_handlers, 3, h3_w);
    let slow_btn_w = px(320, d);
    let unblock_btn_w = px(220, d);
    move_ctrl(c.btn_block_slow, pad, footer_btn_y, slow_btn_w, btn_h);
    move_ctrl(
        c.btn_unblock_all,
        pad + slow_btn_w + gap,
        footer_btn_y,
        unblock_btn_w,
        btn_h,
    );

    // Tab 2 layout
    let info_h = px(56, d);
    let lv2_h = (footer_btn_y - gap - info_h - gap - body_y).max(px(160, d));
    move_ctrl(c.lv_settings, pad, body_y, content_w, lv2_h);
    let s0_w = (content_w * 42) / 100;
    let s1_w = (content_w - s0_w - px(24, d)).max(px(200, d));
    set_column_width(c.lv_settings, 0, s0_w);
    set_column_width(c.lv_settings, 1, s1_w);
    move_ctrl(c.lbl_system_info, pad, body_y + lv2_h + gap, content_w, info_h);
    let cfg_btn_w = px(180, d);
    move_ctrl(c.btn_open_config, pad, footer_btn_y, cfg_btn_w, btn_h);
    move_ctrl(
        c.btn_reset_config,
        pad + cfg_btn_w + gap,
        footer_btn_y,
        cfg_btn_w,
        btn_h,
    );

    // Footer status
    move_ctrl(c.lbl_status, pad, status_y, content_w, status_h);
}

fn update_tab_visibility(state: &mut ManagerState) {
    let t0 = state.active_tab == 0;
    let t1 = state.active_tab == 1;
    let t2 = state.active_tab == 2;
    let c = &state.controls;
    for hwnd in [
        c.lbl_items_hint,
        c.edit_search,
        c.lbl_items_count,
        c.lv_items,
        c.btn_show_all,
        c.btn_preview,
    ] {
        show_ctrl(hwnd, t0);
    }
    for hwnd in [
        c.lbl_handlers_hint,
        c.lv_handlers,
        c.btn_block_slow,
        c.btn_unblock_all,
    ] {
        show_ctrl(hwnd, t1);
    }
    for hwnd in [
        c.lv_settings,
        c.lbl_system_info,
        c.btn_open_config,
        c.btn_reset_config,
    ] {
        show_ctrl(hwnd, t2);
    }
    for tab in [c.tab_items, c.tab_handlers, c.tab_settings] {
        unsafe {
            invalidate_hwnd(tab);
        }
    }
}

unsafe fn invalidate_hwnd(hwnd: HWND) {
    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, true);
}

fn set_window_text(hwnd: HWND, text: &str) {
    let wide = wide_null(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
    }
}

fn get_window_text(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    OsString::from_wide(&buf[..copied])
        .to_string_lossy()
        .into_owned()
}

fn update_header_text(state: &ManagerState) {
    let kind = if state.background {
        t("フォルダ背景", "Folder Background")
    } else if state.target_path.is_dir() {
        t("フォルダ", "Folder")
    } else {
        t("ファイル", "File")
    };
    let text = format!(
        "{}: [{}] {}",
        t("対象", "Target"),
        kind,
        state.target_path.display()
    );
    set_window_text(state.controls.lbl_target, &text);
    set_window_text(
        state.controls.btn_bg,
        if state.background {
            t("背景モード: ON", "Background: ON")
        } else {
            t("背景モード: OFF", "Background: OFF")
        },
    );
    unsafe { invalidate_hwnd(state.controls.btn_bg) };
}

fn update_status_bar(state: &ManagerState) {
    let text = if let Some(msg) = &state.status_override {
        msg.clone()
    } else if let Some(err) = &state.config_load_error {
        format!(
            "{} ({err})",
            t(
                "config.toml 読込エラー (修正または初期化まで保存停止):",
                "config.toml parse error (edits paused until fixed or Reset):",
            )
        )
    } else if state.inspect_loading {
        t(
            "メニュー項目とシェル拡張を計測中...",
            "Inspecting context menu items and shell extensions...",
        )
        .to_string()
    } else {
        let query_part = state.report.as_ref().map_or_else(
            || t("計測待ち", "Waiting").to_string(),
            |r| format!("{} {:.1} ms", t("クエリ:", "Query:"), r.warm_query_ms),
        );
        let host_part = if state.host_running && state.host_cache_active {
            t(
                "rwm-host 稼働中 (投機キャッシュ有効)",
                "rwm-host running (speculative cache active)",
            )
        } else if state.host_running {
            t("rwm-host 稼働中", "rwm-host running")
        } else {
            t("rwm-host 停止中", "rwm-host stopped")
        };
        format!("{query_part}  |  {host_part}  |  ABI v{ABI_VERSION}")
    };
    set_window_text(state.controls.lbl_status, &text);
}

fn update_system_info_label(state: &ManagerState) {
    let host_txt = if state.host_running && state.host_cache_active {
        t("稼働中 (投機キャッシュ有効)", "Running (speculative cache active)")
    } else if state.host_running {
        t("稼働中", "Running")
    } else {
        t("停止中", "Stopped")
    };
    let classic_txt = state.classic_status.as_ref().map_or_else(
        || t("不明", "Unknown").to_string(),
        |s| {
            if s.classic_menu_enabled {
                t("有効 (Windows 10 スタイル)", "Enabled (Classic)").to_string()
            } else {
                t("標準 (Windows 11)", "Default (Win11)").to_string()
            }
        },
    );
    let info = format!(
        "{}: {} (ABI v{})  |  {}: {}\n{}: {}",
        t("バックグラウンドホスト", "Host"),
        host_txt,
        ABI_VERSION,
        t("クラシックメニュー", "Classic menu"),
        classic_txt,
        t("設定ファイル", "Config file"),
        config_path().display()
    );
    set_window_text(state.controls.lbl_system_info, &info);
}

fn populate_items_list(state: &mut ManagerState) {
    let search = get_window_text(state.controls.edit_search);
    let root_items = state
        .report
        .as_ref()
        .map(|r| r.root_items.as_slice())
        .unwrap_or(&[]);
    state.rows_items = build_menu_item_rows(root_items, &state.config.hidden_items, &search, is_ja());

    state.populating = true;
    unsafe {
        SendMessageW(state.controls.lv_items, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
    }
    for (idx, row) in state.rows_items.iter().enumerate() {
        insert_lv_row(
            state.controls.lv_items,
            idx as i32,
            &[&row.label, &row.kind_label, &row.rule_key],
            row.visible,
        );
    }
    state.populating = false;

    let hidden_count = state.config.hidden_items.len();
    let total_shown = state.rows_items.iter().filter(|r| r.visible).count();
    let count_text = if is_ja() {
        format!("表示中: {total_shown}  /  非表示ルール: {hidden_count}")
    } else {
        format!("Shown: {total_shown}  /  Hidden rules: {hidden_count}")
    };
    set_window_text(state.controls.lbl_items_count, &count_text);
}

fn handler_category_text(row: &HandlerRow, ja: bool) -> &'static str {
    if row.blocked {
        if row.added_by_rwm {
            if ja {
                "ブロック済み (RWM)"
            } else {
                "Blocked (RWM)"
            }
        } else if ja {
            "ブロック済み (外部)"
        } else {
            "Blocked (External)"
        }
    } else if row.is_system {
        if ja {
            "Windows 標準"
        } else {
            "Windows System"
        }
    } else if row.warm_ms.is_some_and(|ms| ms >= SLOW_THRESHOLD_MS) {
        if ja {
            "サードパーティ (低速)"
        } else {
            "3rd-Party (Slow)"
        }
    } else if ja {
        "サードパーティ"
    } else {
        "3rd-Party"
    }
}

fn handler_time_text(row: &HandlerRow, ja: bool) -> String {
    if row.blocked {
        if ja { "無効" } else { "Off" }.to_string()
    } else if let Some(ms) = row.warm_ms {
        format!("{ms:.1} ms")
    } else {
        "-".to_string()
    }
}

fn handler_detail_text(row: &HandlerRow) -> String {
    match &row.dll_path {
        Some(path) => {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            format!("{file}  ({})", row.clsid)
        }
        None => row.clsid.clone(),
    }
}

fn populate_handlers_list(state: &mut ManagerState) {
    let inspected = state
        .report
        .as_ref()
        .map(|r| r.handlers.as_slice())
        .unwrap_or(&[]);
    state.rows_handlers = merge_and_sort_handlers(inspected, &state.blocked_list);

    state.populating = true;
    unsafe {
        SendMessageW(
            state.controls.lv_handlers,
            LVM_DELETEALLITEMS,
            WPARAM(0),
            LPARAM(0),
        );
    }
    let ja = is_ja();
    for (idx, row) in state.rows_handlers.iter().enumerate() {
        let time_txt = handler_time_text(row, ja);
        let cat_txt = handler_category_text(row, ja);
        let detail_txt = handler_detail_text(row);
        insert_lv_row(
            state.controls.lv_handlers,
            idx as i32,
            &[&row.name, &time_txt, cat_txt, &detail_txt],
            !row.blocked,
        );
    }
    state.populating = false;
}

fn populate_settings_list(state: &mut ManagerState) {
    state.populating = true;
    unsafe {
        SendMessageW(
            state.controls.lv_settings,
            LVM_DELETEALLITEMS,
            WPARAM(0),
            LPARAM(0),
        );
    }
    let ja = is_ja();
    for (idx, spec) in SETTING_SPECS.iter().enumerate() {
        let title = if ja { spec.title_ja } else { spec.title_en };
        let desc = if ja { spec.desc_ja } else { spec.desc_en };
        insert_lv_row(
            state.controls.lv_settings,
            idx as i32,
            &[title, desc],
            (spec.get)(&state.config),
        );
    }
    state.populating = false;
    update_system_info_label(state);
}

fn start_background_inspect(state: &mut ManagerState) {
    state.inspect_gen = state.inspect_gen.wrapping_add(1);
    let gen = state.inspect_gen;
    let hwnd_raw = state.hwnd.0 as isize;
    let path = state.target_path.clone();
    let background = state.background;
    let pending = Arc::clone(&state.pending_inspect);
    state.inspect_loading = true;
    state.status_override = None;
    update_status_bar(state);

    let _ = std::thread::Builder::new()
        .name("rwm-gui-inspect".into())
        .spawn(move || {
            let report = catch_unwind(AssertUnwindSafe(|| run_inspect_probe(&path, background)))
                .unwrap_or_else(|_| Err("inspect worker panicked".to_string()));
            let blocked = handlers::list_blocked().unwrap_or_default();
            let classic = classic::status().ok();
            let host_running = crate::host::is_host_running(crate::host::HOST_MUTEX_NAME);
            let host_cache_active = SharedCacheClient::open().is_some_and(|c| c.is_valid());
            {
                let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(InspectResult {
                    gen,
                    report,
                    blocked,
                    classic,
                    host_running,
                    host_cache_active,
                });
            }
            let _ = unsafe {
                PostMessageW(
                    HWND(hwnd_raw as *mut c_void),
                    WM_APP_INSPECT_DONE,
                    WPARAM(gen as usize),
                    LPARAM(0),
                )
            };
        });
}

fn run_inspect_probe(path: &Path, background: bool) -> Result<InspectReport, String> {
    if probe::resolve_cli_exe().is_err() {
        return inspect_target(path, background, false).map_err(|e| e.to_string());
    }
    let args = InspectArgs {
        target: TargetArgs {
            path: Some(path.to_path_buf()),
            background,
        },
        extended: false,
        json: true,
        timeout_ms: 20_000,
    };
    let value = probe::run(
        &ProbeRequest::Inspect(args),
        Duration::from_millis(20_000),
        "inspect",
    )
    .map_err(|e| e.to_string())?;
    serde_json::from_value::<InspectReport>(value).map_err(|e| e.to_string())
}

fn load_latest_config_for_edit(state: &mut ManagerState) -> Option<RwmConfig> {
    if state.config_load_error.is_some() {
        state.status_override = Some(
            t(
                "config.toml にエラーがあるため上書きを中止しました。修正するか「設定を初期化」してください。",
                "config.toml has an error; refusing to overwrite. Fix it or click Reset Config.",
            )
            .to_string(),
        );
        update_status_bar(state);
        return None;
    }
    match RwmConfig::load_or_default() {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            state.config_load_error = Some(err.to_string());
            state.status_override = Some(format!(
                "{}: {err}",
                t(
                    "config.toml 読込エラー (上書きを中止)",
                    "config.toml error (refusing to overwrite)",
                )
            ));
            update_status_bar(state);
            None
        }
    }
}

fn persist_config_and_reload(state: &mut ManagerState, status_msg: String) {
    if state.config_load_error.is_some() {
        update_status_bar(state);
        return;
    }
    let validated = state.config.clone().validated();
    state.config = validated;
    if let Err(err) = state.config.save() {
        state.status_override = Some(format!("{}: {err}", t("設定保存エラー", "Save failed")));
    } else {
        notify_host_config_reload();
        state.status_override = Some(status_msg);
    }
    update_status_bar(state);
}

struct ModalGuard(*mut ManagerState);
impl Drop for ModalGuard {
    fn drop(&mut self) {
        unsafe {
            (*self.0).modal_open = false;
        }
    }
}

unsafe fn with_modal<R>(ptr: *mut ManagerState, f: impl FnOnce(HWND) -> R) -> R {
    (*ptr).modal_open = true;
    let _guard = ModalGuard(ptr);
    let hwnd = (*ptr).hwnd;
    f(hwnd)
}

fn confirm_box(hwnd: HWND, text: &str, title: &str, warn: bool) -> bool {
    let text_w = wide_null(text);
    let title_w = wide_null(title);
    let flags = MB_YESNO | MB_DEFBUTTON2 | if warn { MB_ICONWARNING } else { MB_ICONQUESTION };
    unsafe { MessageBoxW(hwnd, PCWSTR(text_w.as_ptr()), PCWSTR(title_w.as_ptr()), flags) == IDYES }
}

fn on_item_checkbox_changed(state: &mut ManagerState, row_idx: usize, checked: bool) {
    let Some(row) = state.rows_items.get(row_idx).cloned() else {
        return;
    };
    let Some(mut cfg) = load_latest_config_for_edit(state) else {
        state.populating = true;
        set_item_check(state.controls.lv_items, row_idx as i32, !checked);
        state.populating = false;
        return;
    };
    if !checked {
        cfg.hidden_items.push(row.rule_key.clone());
    } else if row.from_current_target {
        cfg.hidden_items.retain(|rule| {
            !item_matches_hidden(
                std::slice::from_ref(rule),
                row.canonical_verb.as_deref(),
                &row.clean_label,
            )
        });
    } else {
        cfg.hidden_items
            .retain(|rule| !eq_ignore_case(rule, &row.rule_key));
    }
    state.config = cfg.validated();
    if let Some(entry) = state.rows_items.get_mut(row_idx) {
        entry.visible = checked;
    }
    let msg = if checked {
        format!("{} {}", t("表示に戻しました:", "Shown:"), row.label)
    } else {
        format!("{} {}", t("非表示にしました:", "Hidden:"), row.label)
    };
    persist_config_and_reload(state, msg);
    let hidden_count = state.config.hidden_items.len();
    let total_shown = state.rows_items.iter().filter(|r| r.visible).count();
    let count_text = if is_ja() {
        format!("表示中: {total_shown}  /  非表示ルール: {hidden_count}")
    } else {
        format!("Shown: {total_shown}  /  Hidden rules: {hidden_count}")
    };
    set_window_text(state.controls.lbl_items_count, &count_text);
}

unsafe fn on_handler_checkbox_changed_ptr(ptr: *mut ManagerState, row_idx: usize, checked: bool) {
    let Some(row) = (&*ptr).rows_handlers.get(row_idx).cloned() else {
        return;
    };
    if !checked {
        if row.is_system {
            let ok = with_modal(ptr, |hwnd| {
                confirm_box(
                    hwnd,
                    t(
                        "これは Windows 標準のシステムシェル拡張です。\n無効にすると右クリックの一部機能が使えなくなる場合があります。\n本当にブロックしますか？",
                        "This is a Windows system shell extension. Blocking it may disable standard menu features. Continue?",
                    ),
                    t("システム拡張の確認", "Confirm System Extension"),
                    true,
                )
            });
            if !ok {
                let state = &mut *ptr;
                if let Some(idx) = state
                    .rows_handlers
                    .iter()
                    .position(|r| r.clsid.eq_ignore_ascii_case(&row.clsid))
                {
                    state.populating = true;
                    set_item_check(state.controls.lv_handlers, idx as i32, true);
                    state.populating = false;
                }
                return;
            }
        }
        let state = &mut *ptr;
        let Some(current_idx) = state
            .rows_handlers
            .iter()
            .position(|r| r.clsid.eq_ignore_ascii_case(&row.clsid))
        else {
            return;
        };
        if let Err(err) = handlers::block_clsid(&row.clsid, Some(&row.name)) {
            state.status_override = Some(format!("{}: {err}", t("ブロック失敗", "Block failed")));
            state.populating = true;
            set_item_check(state.controls.lv_handlers, current_idx as i32, true);
            state.populating = false;
            update_status_bar(state);
            return;
        }
        if let Some(r) = state.rows_handlers.get_mut(current_idx) {
            r.blocked = true;
            r.added_by_rwm = true;
            update_lv_subitem(
                state.controls.lv_handlers,
                current_idx as i32,
                1,
                &handler_time_text(r, is_ja()),
            );
            update_lv_subitem(
                state.controls.lv_handlers,
                current_idx as i32,
                2,
                handler_category_text(r, is_ja()),
            );
        }
        state.blocked_list = handlers::list_blocked().unwrap_or_default();
        notify_host_config_reload();
        state.status_override = Some(format!(
            "{} {} ({})",
            t("無効化しました:", "Blocked:"),
            row.name,
            t("右クリックへ即反映", "cache refreshed")
        ));
        update_status_bar(state);
        invalidate_hwnd(state.controls.lv_handlers);
    } else {
        let res = handlers::unblock_clsid(&row.clsid, false);
        let unblocked = match res {
            Ok(()) => Ok(()),
            Err(_) => {
                let ok = with_modal(ptr, |hwnd| {
                    confirm_box(
                        hwnd,
                        t(
                            "このシェル拡張は RustWindowsMenu 以外でブロックされています。\nブロックを強制解除して有効化しますか？",
                            "This extension was blocked outside RustWindowsMenu. Force unblock it?",
                        ),
                        t("外部ブロックの解除", "Confirm Force Unblock"),
                        false,
                    )
                });
                if !ok {
                    Err(None)
                } else if let Err(err) = handlers::unblock_clsid(&row.clsid, true) {
                    Err(Some(format!("{}: {err}", t("解除失敗", "Unblock failed"))))
                } else {
                    Ok(())
                }
            }
        };
        let state = &mut *ptr;
        let Some(current_idx) = state
            .rows_handlers
            .iter()
            .position(|r| r.clsid.eq_ignore_ascii_case(&row.clsid))
        else {
            return;
        };
        if let Err(err_msg) = unblocked {
            if let Some(msg) = err_msg {
                state.status_override = Some(msg);
            }
            state.populating = true;
            set_item_check(state.controls.lv_handlers, current_idx as i32, false);
            state.populating = false;
            update_status_bar(state);
            return;
        }
        if let Some(r) = state.rows_handlers.get_mut(current_idx) {
            r.blocked = false;
            r.added_by_rwm = false;
            update_lv_subitem(
                state.controls.lv_handlers,
                current_idx as i32,
                1,
                &handler_time_text(r, is_ja()),
            );
            update_lv_subitem(
                state.controls.lv_handlers,
                current_idx as i32,
                2,
                handler_category_text(r, is_ja()),
            );
        }
        state.blocked_list = handlers::list_blocked().unwrap_or_default();
        notify_host_config_reload();
        state.status_override = Some(format!(
            "{} {}",
            t("有効化しました:", "Enabled:"),
            row.name
        ));
        update_status_bar(state);
        invalidate_hwnd(state.controls.lv_handlers);
        if row.warm_ms.is_none() {
            start_background_inspect(state);
        }
    }
}

fn on_setting_checkbox_changed(state: &mut ManagerState, row_idx: usize, checked: bool) {
    let Some(spec) = SETTING_SPECS.get(row_idx) else {
        return;
    };
    let Some(mut cfg) = load_latest_config_for_edit(state) else {
        state.populating = true;
        set_item_check(state.controls.lv_settings, row_idx as i32, !checked);
        state.populating = false;
        return;
    };
    (spec.set)(&mut cfg, checked);
    state.config = cfg;
    let title = if is_ja() { spec.title_ja } else { spec.title_en };
    let msg = format!(
        "{}: {} -> {}",
        t("設定保存", "Saved"),
        title,
        if checked { "ON" } else { "OFF" }
    );
    persist_config_and_reload(state, msg);
}

fn pick_path_dialog(hwnd: HWND, pick_folder: bool) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let mut opts = dialog.GetOptions().ok()?;
        opts |= FOS_FORCEFILESYSTEM;
        if pick_folder {
            opts |= FOS_PICKFOLDERS;
        }
        dialog.SetOptions(opts).ok()?;
        if dialog.Show(hwnd).is_err() {
            return None;
        }
        let item = dialog.GetResult().ok()?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        if raw.is_null() {
            return None;
        }
        let wide = raw.as_wide();
        let path = PathBuf::from(OsString::from_wide(wide));
        CoTaskMemFree(Some(raw.0.cast()));
        Some(path)
    }
}

unsafe fn handle_command_ptr(ptr: *mut ManagerState, id: usize, code: u32) {
    match id {
        ID_TAB_ITEMS => {
            let state = &mut *ptr;
            state.active_tab = 0;
            update_tab_visibility(state);
        }
        ID_TAB_HANDLERS => {
            let state = &mut *ptr;
            state.active_tab = 1;
            update_tab_visibility(state);
        }
        ID_TAB_SETTINGS => {
            let state = &mut *ptr;
            state.active_tab = 2;
            update_tab_visibility(state);
        }
        ID_BTN_FILE => {
            if let Some(path) = with_modal(ptr, |hwnd| pick_path_dialog(hwnd, false)) {
                let state = &mut *ptr;
                state.target_path = path;
                state.background = false;
                update_header_text(state);
                start_background_inspect(state);
            }
        }
        ID_BTN_FOLDER => {
            if let Some(path) = with_modal(ptr, |hwnd| pick_path_dialog(hwnd, true)) {
                let state = &mut *ptr;
                state.target_path = path;
                update_header_text(state);
                start_background_inspect(state);
            }
        }
        ID_BTN_BG => {
            let state = &mut *ptr;
            if state.target_path.is_dir() {
                state.background = !state.background;
                update_header_text(state);
                start_background_inspect(state);
            } else if let Some(parent) = state.target_path.parent().map(Path::to_path_buf) {
                state.target_path = parent;
                state.background = true;
                update_header_text(state);
                start_background_inspect(state);
            }
        }
        ID_BTN_REFRESH => {
            start_background_inspect(&mut *ptr);
        }
        ID_EDIT_SEARCH if code == EN_CHANGE => {
            let hwnd = (*ptr).hwnd;
            let _ = KillTimer(hwnd, SEARCH_DEBOUNCE_TIMER);
            let _ = SetTimer(hwnd, SEARCH_DEBOUNCE_TIMER, 120, None);
        }
        ID_BTN_SHOW_ALL => {
            let state = &mut *ptr;
            if state.config.hidden_items.is_empty() {
                state.status_override = Some(
                    t("非表示にしている項目はありません。", "No items are currently hidden.")
                        .to_string(),
                );
                update_status_bar(state);
                return;
            }
            let Some(mut cfg) = load_latest_config_for_edit(state) else {
                return;
            };
            cfg.hidden_items.clear();
            state.config = cfg;
            persist_config_and_reload(
                state,
                t(
                    "すべてのメニュー項目を表示に戻しました。",
                    "Restored all hidden menu items.",
                )
                .to_string(),
            );
            populate_items_list(state);
        }
        ID_BTN_PREVIEW => {
            let state = &*ptr;
            if let Ok(cli) = probe::resolve_cli_exe() {
                let mut cmd = Command::new(&cli);
                cmd.arg("preview");
                if state.background {
                    cmd.arg("--background");
                }
                cmd.arg("--").arg(&state.target_path);
                let _ = cmd
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW.0)
                    .spawn();
            }
        }
        ID_BTN_BLOCK_SLOW => {
            let slow: Vec<_> = (&*ptr)
                .rows_handlers
                .iter()
                .filter(|r| !r.blocked && !r.is_system && r.warm_ms.is_some_and(|m| m >= SLOW_THRESHOLD_MS))
                .cloned()
                .collect();
            if slow.is_empty() {
                let state = &mut *ptr;
                state.status_override = Some(
                    t(
                        "25 ms 以上のサードパーティシェル拡張はありませんでした。",
                        "No third-party shell extensions >= 25 ms found.",
                    )
                    .to_string(),
                );
                update_status_bar(state);
                return;
            }
            let list_text: String = slow
                .iter()
                .map(|r| format!("  - {} ({:.1} ms)", r.name, r.warm_ms.unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = if is_ja() {
                format!("以下の遅いサードパーティ拡張をブロックしますか？\n\n{list_text}")
            } else {
                format!("Block these slow third-party shell extensions?\n\n{list_text}")
            };
            let ok = with_modal(ptr, |hwnd| {
                confirm_box(
                    hwnd,
                    &prompt,
                    t("高速化の確認", "Confirm Slow Extension Block"),
                    false,
                )
            });
            if !ok {
                return;
            }
            let mut count = 0usize;
            for r in &slow {
                if handlers::block_clsid(&r.clsid, Some(&r.name)).is_ok() {
                    count += 1;
                }
            }
            notify_host_config_reload();
            let state = &mut *ptr;
            state.status_override = Some(if is_ja() {
                format!("{count} 件の遅い拡張をブロックしました。")
            } else {
                format!("Blocked {count} slow extension(s).")
            });
            start_background_inspect(state);
        }
        ID_BTN_UNBLOCK_ALL => {
            let ok = with_modal(ptr, |hwnd| {
                confirm_box(
                    hwnd,
                    t(
                        "RustWindowsMenu が追加したシェル拡張ブロックをすべて解除しますか？",
                        "Restore all shell extensions previously blocked by RustWindowsMenu?",
                    ),
                    t("ブロック解除の確認", "Confirm Unblock"),
                    false,
                )
            });
            if !ok {
                return;
            }
            let state = &mut *ptr;
            match handlers::restore_rwm_blocks() {
                Ok(n) => {
                    notify_host_config_reload();
                    state.status_override = Some(if is_ja() {
                        format!("{n} 件のブロックを解除しました。")
                    } else {
                        format!("Restored {n} blocked handler(s).")
                    });
                    start_background_inspect(state);
                }
                Err(err) => {
                    state.status_override =
                        Some(format!("{}: {err}", t("解除エラー", "Restore error")));
                    update_status_bar(state);
                }
            }
        }
        ID_BTN_OPEN_CONFIG => {
            let state = &*ptr;
            let cfg = config_path();
            if !cfg.exists() && state.config_load_error.is_none() {
                let _ = state.config.save();
            }
            let wide_path = wide_null(cfg.as_os_str());
            let _ = ShellExecuteW(
                state.hwnd,
                w!("open"),
                PCWSTR(wide_path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
        ID_BTN_RESET_CONFIG => {
            let ok = with_modal(ptr, |hwnd| {
                confirm_box(
                    hwnd,
                    t(
                        "設定を v0.4 初期値 (非表示ルールもクリア) に戻しますか？",
                        "Reset config.toml and hidden item rules to defaults?",
                    ),
                    t("設定初期化の確認", "Confirm Config Reset"),
                    false,
                )
            });
            if ok {
                let state = &mut *ptr;
                state.config_load_error = None;
                state.config = RwmConfig::default();
                persist_config_and_reload(
                    state,
                    t("設定を初期値へ戻しました。", "Reset config to defaults.").to_string(),
                );
                populate_settings_list(state);
                populate_items_list(state);
            }
        }
        _ => {}
    }
}

fn draw_owner_button(dis: &DRAWITEMSTRUCT, state: &ManagerState) {
    let id = dis.CtlID as usize;
    let is_tab = matches!(id, ID_TAB_ITEMS | ID_TAB_HANDLERS | ID_TAB_SETTINGS);
    let is_active_tab = (id == ID_TAB_ITEMS && state.active_tab == 0)
        || (id == ID_TAB_HANDLERS && state.active_tab == 1)
        || (id == ID_TAB_SETTINGS && state.active_tab == 2);
    let is_bg_active = id == ID_BTN_BG && state.background;
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;

    let (fill_brush, border_brush, text_color) = if pressed {
        (state.gdi.btn_pressed_brush, state.gdi.border_brush, state.theme.fg)
    } else if is_bg_active {
        (state.gdi.accent_brush, state.gdi.accent_brush, state.theme.accent_fg)
    } else if is_tab && is_active_tab {
        (state.gdi.tab_active_brush, state.gdi.border_brush, state.theme.fg)
    } else if is_tab {
        (state.gdi.tab_brush, state.gdi.border_brush, state.theme.muted)
    } else {
        (state.gdi.btn_brush, state.gdi.border_brush, state.theme.fg)
    };

    unsafe {
        let _ = FillRect(dis.hDC, &dis.rcItem, fill_brush);
        let _ = FrameRect(dis.hDC, &dis.rcItem, border_brush);
        if is_tab && is_active_tab {
            let accent_bar = RECT {
                left: dis.rcItem.left + 1,
                top: dis.rcItem.bottom - px(3, state.dpi),
                right: dis.rcItem.right - 1,
                bottom: dis.rcItem.bottom - 1,
            };
            let _ = FillRect(dis.hDC, &accent_bar, state.gdi.accent_brush);
        }
        SetBkMode(dis.hDC, TRANSPARENT);
        SetTextColor(dis.hDC, text_color);
        let mut text = wide_null(get_window_text(dis.hwndItem));
        let text_len = text.len().saturating_sub(1);
        let mut tr = dis.rcItem;
        tr.left += px(6, state.dpi);
        tr.right -= px(6, state.dpi);
        DrawTextW(
            dis.hDC,
            &mut text[..text_len],
            &mut tr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}

unsafe extern "system" fn listview_header_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref_data: usize,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        if msg == WM_NOTIFY {
            let nmhdr = &*(lparam.0 as *const NMHDR);
            if nmhdr.code == NM_CUSTOMDRAW {
                let parent = GetParent(hwnd).unwrap_or_default();
                let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const ManagerState;
                if !ptr.is_null() {
                    let state = &*ptr;
                    if let Some(res) = handle_header_custom_draw(state, lparam) {
                        return Some(res);
                    }
                }
            }
        } else if msg == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(listview_header_subclass), 1);
        }
        None
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| DefSubclassProc(hwnd, msg, wparam, lparam))
}

fn handle_header_custom_draw(state: &ManagerState, lparam: LPARAM) -> Option<LRESULT> {
    let nmhdr = unsafe { &*(lparam.0 as *const NMHDR) };
    if nmhdr.hwndFrom == state.controls.hdr_items || nmhdr.hwndFrom == state.controls.hdr_handlers {
        let nmcd = unsafe { &mut *(lparam.0 as *mut NMCUSTOMDRAW) };
        if nmcd.dwDrawStage == CDDS_PREPAINT {
            unsafe {
                let _ = FillRect(nmcd.hdc, &nmcd.rc, state.gdi.header_brush);
            }
            return Some(LRESULT(CDRF_NOTIFYITEMDRAW as isize));
        }
        if nmcd.dwDrawStage == CDDS_ITEMPREPAINT {
            let label = if nmhdr.hwndFrom == state.controls.hdr_items {
                match nmcd.dwItemSpec {
                    0 => Some(t("表示 / メニュー項目", "Show / Menu Item")),
                    1 => Some(t("種類", "Kind")),
                    2 => Some(t("識別キー", "Filter Key")),
                    _ => None,
                }
            } else {
                match nmcd.dwItemSpec {
                    0 => Some(t("有効 / シェル拡張名", "Enabled / Extension")),
                    1 => Some(t("時間", "Warm Time")),
                    2 => Some(t("区分", "Category")),
                    3 => Some("DLL / CLSID"),
                    _ => None,
                }
            };
            if let Some(label) = label {
                unsafe {
                    let _ = FillRect(nmcd.hdc, &nmcd.rc, state.gdi.header_brush);
                    let right_div = RECT {
                        left: (nmcd.rc.right - 1).max(nmcd.rc.left),
                        top: nmcd.rc.top + 4,
                        right: nmcd.rc.right,
                        bottom: (nmcd.rc.bottom - 4).max(nmcd.rc.top + 4),
                    };
                    let _ = FillRect(nmcd.hdc, &right_div, state.gdi.border_brush);
                    let bottom_div = RECT {
                        left: nmcd.rc.left,
                        top: (nmcd.rc.bottom - 1).max(nmcd.rc.top),
                        right: nmcd.rc.right,
                        bottom: nmcd.rc.bottom,
                    };
                    let _ = FillRect(nmcd.hdc, &bottom_div, state.gdi.border_brush);

                    let old_font = SelectObject(nmcd.hdc, HGDIOBJ(state.gdi.small_font.0));
                    SetBkMode(nmcd.hdc, TRANSPARENT);
                    SetTextColor(nmcd.hdc, state.theme.fg);
                    let mut wide = wide_null(label);
                    let wlen = wide.len().saturating_sub(1);
                    let mut tr = nmcd.rc;
                    tr.left += px(8, state.dpi);
                    tr.right = (tr.right - px(6, state.dpi)).max(tr.left);
                    DrawTextW(
                        nmcd.hdc,
                        &mut wide[..wlen],
                        &mut tr,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                    if !old_font.is_invalid() {
                        let _ = SelectObject(nmcd.hdc, old_font);
                    }
                }
                return Some(LRESULT(CDRF_SKIPDEFAULT as isize));
            }
        }
    }
    None
}

fn handle_custom_draw(state: &ManagerState, lparam: LPARAM) -> Option<LRESULT> {
    if let Some(res) = handle_header_custom_draw(state, lparam) {
        return Some(res);
    }
    let nmhdr = unsafe { &*(lparam.0 as *const NMHDR) };

    if nmhdr.hwndFrom == state.controls.lv_handlers {
        let lvcd = unsafe { &mut *(lparam.0 as *mut NMLVCUSTOMDRAW) };
        if lvcd.nmcd.dwDrawStage == CDDS_PREPAINT {
            return Some(LRESULT(CDRF_NOTIFYITEMDRAW as isize));
        }
        if lvcd.nmcd.dwDrawStage == CDDS_ITEMPREPAINT {
            let idx = lvcd.nmcd.dwItemSpec;
            if let Some(row) = state.rows_handlers.get(idx) {
                if row.blocked {
                    lvcd.clrText = state.theme.muted;
                } else if !row.is_system && row.warm_ms.is_some_and(|m| m >= SLOW_THRESHOLD_MS) {
                    lvcd.clrText = state.theme.warn_fg;
                } else {
                    lvcd.clrText = state.theme.fg;
                }
                lvcd.clrTextBk = state.theme.list_bg;
            }
            return Some(LRESULT(CDRF_DODEFAULT as isize));
        }
    }
    None
}

unsafe extern "system" fn manager_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        if message == WM_GETMINMAXINFO {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            let dpi = GetDpiForWindow(hwnd).max(96);
            mmi.ptMinTrackSize = POINT {
                x: px(720, dpi),
                y: px(520, dpi),
            };
            return Some(LRESULT(0));
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ManagerState;
        if ptr.is_null() {
            return None;
        }
        match message {
            WM_CREATE => Some(LRESULT(0)),
            WM_SIZE => {
                relayout(&mut *ptr);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => {
                let state = &*ptr;
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let _ = FillRect(hdc, &rc, state.gdi.bg_brush);
                Some(LRESULT(1))
            }
            WM_CTLCOLORSTATIC => {
                let state = &*ptr;
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
                let ctrl = HWND(lparam.0 as *mut c_void);
                let is_muted = ctrl == state.controls.lbl_items_hint
                    || ctrl == state.controls.lbl_handlers_hint
                    || ctrl == state.controls.lbl_items_count
                    || ctrl == state.controls.lbl_system_info
                    || ctrl == state.controls.lbl_status;
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(
                    hdc,
                    if is_muted {
                        state.theme.muted
                    } else {
                        state.theme.fg
                    },
                );
                Some(LRESULT(state.gdi.bg_brush.0 as isize))
            }
            WM_CTLCOLOREDIT => {
                let state = &*ptr;
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
                SetBkColor(hdc, state.theme.list_bg);
                SetTextColor(hdc, state.theme.fg);
                Some(LRESULT(state.gdi.card_brush.0 as isize))
            }
            WM_DRAWITEM => {
                let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
                draw_owner_button(dis, &*ptr);
                Some(LRESULT(1))
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xffff;
                let code = ((wparam.0 >> 16) & 0xffff) as u32;
                handle_command_ptr(ptr, id, code);
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == SEARCH_DEBOUNCE_TIMER => {
                let _ = KillTimer(hwnd, SEARCH_DEBOUNCE_TIMER);
                populate_items_list(&mut *ptr);
                Some(LRESULT(0))
            }
            WM_COPYDATA => {
                let state = &mut *ptr;
                let cds = &*(lparam.0 as *const COPYDATASTRUCT);
                if !cds.lpData.is_null() && cds.cbData > 0 {
                    let slice =
                        std::slice::from_raw_parts(cds.lpData.cast::<u8>(), cds.cbData as usize);
                    if let Some((path, bg)) = decode_copydata_payload(cds.dwData, slice) {
                        if path.exists() {
                            state.target_path = path;
                            state.background = bg && state.target_path.is_dir();
                            update_header_text(state);
                            start_background_inspect(state);
                        }
                    }
                }
                Some(LRESULT(1))
            }
            WM_APP_INSPECT_DONE => {
                let msg_gen = wparam.0 as u64;
                let res = {
                    let pending = &(*ptr).pending_inspect;
                    pending.lock().unwrap_or_else(|e| e.into_inner()).take()
                };
                if let Some(res) = res {
                    let state = &mut *ptr;
                    if res.gen == state.inspect_gen && res.gen == msg_gen {
                        state.inspect_loading = false;
                        state.blocked_list = res.blocked;
                        state.classic_status = res.classic;
                        state.host_running = res.host_running;
                        state.host_cache_active = res.host_cache_active;
                        match RwmConfig::load_or_default() {
                            Ok(cfg) => {
                                state.config = cfg;
                                state.config_load_error = None;
                            }
                            Err(err) => {
                                state.config_load_error = Some(err.to_string());
                            }
                        }
                        match res.report {
                            Ok(report) => {
                                state.report = Some(report);
                            }
                            Err(err) => {
                                state.report = None;
                                state.status_override = Some(format!(
                                    "{}: {err}",
                                    t("計測エラー", "Inspect error")
                                ));
                            }
                        }
                        populate_items_list(state);
                        populate_handlers_list(state);
                        populate_settings_list(state);
                        update_status_bar(state);
                    }
                }
                Some(LRESULT(0))
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if hdr.code == NM_CUSTOMDRAW {
                    if let Some(res) = handle_custom_draw(&*ptr, lparam) {
                        return Some(res);
                    }
                }
                if hdr.code == LVN_ITEMCHANGED && !(*ptr).populating {
                    let nmlv = &*(lparam.0 as *const NMLISTVIEW);
                    let old_chk = (nmlv.uOldState & LVIS_STATEIMAGEMASK.0) >> 12;
                    let new_chk = (nmlv.uNewState & LVIS_STATEIMAGEMASK.0) >> 12;
                    if (nmlv.uChanged.0 & LVIF_STATE.0) != 0
                        && old_chk != 0
                        && new_chk != 0
                        && old_chk != new_chk
                        && nmlv.iItem >= 0
                    {
                        let idx = nmlv.iItem as usize;
                        let checked = new_chk == 2;
                        let controls = &(*ptr).controls;
                        if hdr.hwndFrom == controls.lv_items {
                            on_item_checkbox_changed(&mut *ptr, idx, checked);
                        } else if hdr.hwndFrom == controls.lv_handlers {
                            on_handler_checkbox_changed_ptr(ptr, idx, checked);
                        } else if hdr.hwndFrom == controls.lv_settings {
                            on_setting_checkbox_changed(&mut *ptr, idx, checked);
                        }
                    }
                }
                None
            }
            WM_CLOSE => {
                if (*ptr).modal_open {
                    return Some(LRESULT(0));
                }
                let _ = DestroyWindow(hwnd);
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                PostQuitMessage(0);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, message, wparam, lparam) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copydata_payload_roundtrip() {
        let p = Path::new(r"C:\Users\demo\Documents\example.txt");
        let (tag, bytes) = encode_copydata_payload(p, false);
        assert_eq!(
            decode_copydata_payload(tag, &bytes),
            Some((p.to_path_buf(), false))
        );
        let dir = Path::new(r"C:\Users\demo\Documents");
        let (tag_bg, bytes_bg) = encode_copydata_payload(dir, true);
        assert_eq!(
            decode_copydata_payload(tag_bg, &bytes_bg),
            Some((dir.to_path_buf(), true))
        );
        assert_eq!(decode_copydata_payload(0x9999, &bytes_bg), None);
        assert_eq!(decode_copydata_payload(tag, b"   "), None);
    }

    #[test]
    fn build_menu_item_rows_includes_extra_hidden_rules_and_search_filters() {
        let items = vec![
            InspectedMenuItem {
                index: 0,
                is_separator: false,
                label: "開く(&O)".into(),
                clean_label: "開く".into(),
                canonical_verb: Some("open".into()),
                w_id: 1,
                is_submenu: false,
                is_owner_draw: false,
                has_bitmap: false,
                enabled: true,
                checked: false,
                is_default: true,
                sub_init_ms: None,
                children: Vec::new(),
            },
            InspectedMenuItem {
                index: 1,
                is_separator: true,
                label: String::new(),
                clean_label: String::new(),
                canonical_verb: None,
                w_id: 0,
                is_submenu: false,
                is_owner_draw: false,
                has_bitmap: false,
                enabled: false,
                checked: false,
                is_default: false,
                sub_init_ms: None,
                children: Vec::new(),
            },
            InspectedMenuItem {
                index: 2,
                is_separator: false,
                label: "共有(&S)".into(),
                clean_label: "共有".into(),
                canonical_verb: Some("Windows.ModernShare".into()),
                w_id: 2,
                is_submenu: false,
                is_owner_draw: false,
                has_bitmap: false,
                enabled: true,
                checked: false,
                is_default: false,
                sub_init_ms: None,
                children: Vec::new(),
            },
            InspectedMenuItem {
                index: 3,
                is_separator: false,
                label: "7-Zip".into(),
                clean_label: "7-Zip".into(),
                canonical_verb: None,
                w_id: 3,
                is_submenu: true,
                is_owner_draw: false,
                has_bitmap: false,
                enabled: true,
                checked: false,
                is_default: false,
                sub_init_ms: None,
                children: Vec::new(),
            },
        ];
        let hidden = vec![
            "verb:Windows.ModernShare".into(),
            "label:他の拡張項目".into(),
        ];

        let all = build_menu_item_rows(&items, &hidden, "", true);
        assert_eq!(all.len(), 4);
        assert!(all[0].visible);
        assert_eq!(all[0].rule_key, "verb:open");
        assert!(!all[1].visible);
        assert!(all[1].from_current_target);
        assert_eq!(all[2].kind_label, "サブメニュー");
        assert!(!all[3].visible);
        assert!(!all[3].from_current_target);
        assert_eq!(all[3].rule_key, "label:他の拡張項目");

        let filtered = build_menu_item_rows(&items, &hidden, "modern", false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].clean_label, "共有");
    }

    #[test]
    fn merge_and_sort_handlers_puts_slow_third_party_first_and_preserves_blocked() {
        let sys_dll = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join(r"System32\shell32.dll");
        let inspected = vec![
            HandlerTiming {
                clsid: "{11111111-1111-1111-1111-111111111111}".into(),
                name: "Open With".into(),
                dll_path: Some(sys_dll),
                scopes: vec!["*".into()],
                blocked: false,
                cold: None,
                warm: None,
                warm_ms: Some(40.0),
            },
            HandlerTiming {
                clsid: "{22222222-2222-2222-2222-222222222222}".into(),
                name: "Fast Third Party".into(),
                dll_path: Some(PathBuf::from(r"C:\Program Files\Fast\fast.dll")),
                scopes: vec!["*".into()],
                blocked: false,
                cold: None,
                warm: None,
                warm_ms: Some(1.5),
            },
            HandlerTiming {
                clsid: "{1D1B5D7B-0FC9-452E-902C-12BACD4FBC20}".into(),
                name: "DaemonShellExtImageLite Class".into(),
                dll_path: Some(PathBuf::from(r"C:\Program Files\DAEMON Tools Lite\dtshl64.dll")),
                scopes: vec!["*".into()],
                blocked: false,
                cold: None,
                warm: None,
                warm_ms: Some(85.0),
            },
        ];
        let blocked = vec![BlockedHandler {
            clsid: "{33333333-3333-3333-3333-333333333333}".into(),
            name: Some("Previously Blocked".into()),
            added_by_rwm: true,
        }];
        let merged = merge_and_sort_handlers(&inspected, &blocked);
        let names: Vec<_> = merged.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "DaemonShellExtImageLite Class",
                "Previously Blocked",
                "Fast Third Party",
                "Open With",
            ]
        );
    }
}
