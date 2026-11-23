//! Interactive smoke tests. They open real popup windows for about two seconds, so they are ignored
//! by default: `cargo test -p rwm-ui -- --ignored --test-threads=1`. Set `RWM_UI_SMOKE_CAPTURE` to
//! a directory to also save a BMP screenshot of each step.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use rwm_core::config::RwmConfig;
use rwm_core::model::{
    BgraIcon, CoreVerb, EntryAction, EntrySource, HandlerPolicy, HandlerStatus, HandlerTiming,
    HydrationEvent, MenuEntry, MenuItem, MenuSnapshot, SelectionTarget,
};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS,
    SRCCOPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_DOWN, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, PostMessageW, WM_KEYDOWN};

use super::{run_popup, PopupOptions, PopupResult};
use crate::renderer::FORCE_GDI;
use crate::WM_APP_HYDRATE;

const SESSION: u64 = 7;

fn item(label: &str, action: EntryAction) -> MenuItem {
    MenuItem {
        id: 1,
        label: label.to_owned(),
        mnemonic: None,
        shortcut_hint: None,
        icon: None,
        enabled: true,
        checked: false,
        is_default: false,
        source: EntrySource::Core,
        action,
    }
}

fn core(label: &str, verb: CoreVerb, shortcut: &str) -> MenuItem {
    MenuItem {
        shortcut_hint: Some(shortcut.to_owned()),
        ..item(label, EntryAction::CoreVerb(verb))
    }
}

fn static_command(label: &str, verb: &str) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        source: EntrySource::StaticVerb,
        ..item(
            label,
            EntryAction::StaticCommand {
                verb: verb.to_owned(),
                command: "cmd.exe /c exit".to_owned(),
                working_dir: PathBuf::from(r"C:\Windows"),
            },
        )
    })
}

fn com_command(label: &str, handler: &str) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        source: EntrySource::ComHandler {
            clsid: "{00000000-0000-0000-0000-000000000000}".to_owned(),
            name: handler.to_owned(),
        },
        ..item(
            label,
            EntryAction::ComCommand {
                session_id: SESSION,
                handler_index: 0,
                cmd_offset: 1,
            },
        )
    })
}

/// 16x16 premultiplied Windows-blue tile with transparent corners.
fn tile_icon() -> Arc<BgraIcon> {
    let size = 16u32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let corner = (x == 0 || x == size - 1) && (y == 0 || y == size - 1);
            pixels.extend_from_slice(if corner {
                &[0, 0, 0, 0]
            } else {
                &[0xD4, 0x78, 0x00, 0xFF]
            });
        }
    }
    Arc::new(BgraIcon {
        width: size,
        height: size,
        pixels,
    })
}

fn scripted_snapshot() -> MenuSnapshot {
    let open = MenuItem {
        is_default: true,
        ..core("開く", CoreVerb::Open, "Enter")
    };
    let copy = MenuItem {
        icon: Some(tile_icon()),
        ..core("コピー", CoreVerb::Copy, "Ctrl+C")
    };
    let paste = MenuItem {
        enabled: false,
        ..core("貼り付け", CoreVerb::Paste, "Ctrl+V")
    };
    let send_to = item(
        "送る",
        EntryAction::Submenu(vec![
            static_command("デスクトップ (ショートカットを作成)", "desktop"),
            static_command("ドキュメント", "documents"),
        ]),
    );
    MenuSnapshot {
        session_id: SESSION,
        target: SelectionTarget::SingleFile(PathBuf::from(r"C:\Windows\win.ini")),
        entries: vec![
            MenuEntry::Item(open),
            MenuEntry::Item(copy),
            MenuEntry::Item(paste),
            MenuEntry::Item(send_to),
            MenuEntry::Separator,
            MenuEntry::Item(core("プロパティ", CoreVerb::Properties, "Alt+Enter")),
        ],
        stage0_us: 840,
        pending_handlers: 2,
    }
}

fn group(policy: HandlerPolicy, handler: &str, entries: Vec<MenuEntry>) -> HydrationEvent {
    HydrationEvent::GroupReady {
        session_id: SESSION,
        group_name: handler.to_owned(),
        clsid: "{00000000-0000-0000-0000-000000000000}".to_owned(),
        policy,
        timing: HandlerTiming {
            clsid: "{00000000-0000-0000-0000-000000000000}".to_owned(),
            name: handler.to_owned(),
            dll_path: None,
            duration_us: 1_000,
            item_count: entries.len(),
            status: HandlerStatus::Ok,
        },
        entries,
    }
}

fn pause(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

/// Plays hydration and keyboard input against a live popup the way the COM worker and a user do:
/// events go through the channel plus `WM_APP_HYDRATE`, keys arrive as `WM_KEYDOWN`.
fn run_scripted_session(anchor: (i32, i32), capture_prefix: &'static str) -> PopupResult {
    let (events, receiver) = mpsc::channel();
    let (root_sender, root_receiver) = mpsc::channel::<isize>();
    let script = thread::spawn(move || {
        let root = HWND(root_receiver.recv().expect("popup root window") as *mut c_void);
        let post = |message: u32, wparam: usize| unsafe {
            let _ = PostMessageW(root, message, WPARAM(wparam), LPARAM(0));
        };
        let key = |key: VIRTUAL_KEY| {
            post(WM_KEYDOWN, usize::from(key.0));
            pause(80);
        };

        pause(300);
        capture(root, capture_prefix, "1-loading");
        let _ = events.send(group(
            HandlerPolicy::Inline,
            "7-Zip",
            vec![com_command("7-Zip で圧縮して送る", "7-Zip")],
        ));
        post(WM_APP_HYDRATE, SESSION as usize);
        pause(150);
        let _ = events.send(group(
            HandlerPolicy::Submenu,
            "Defender",
            vec![com_command("Microsoft Defender でスキャンする", "Defender")],
        ));
        let _ = events.send(HydrationEvent::Completed {
            session_id: SESSION,
            total_us: 380_000,
        });
        post(WM_APP_HYDRATE, SESSION as usize);
        pause(300);
        capture(root, capture_prefix, "2-hydrated");

        for step in [VK_DOWN, VK_DOWN, VK_DOWN, VK_DOWN, VK_RIGHT] {
            key(step);
        }
        pause(300);
        capture(root, capture_prefix, "3-cascade");
        key(VK_DOWN);
        key(VK_RETURN);
    });

    let options = PopupOptions {
        anchor,
        owner_hwnd: HWND::default(),
        show_timings: true,
        submenu_hover_delay_ms: 140,
        smoke_test_ms: Some(8_000),
    };
    let result = run_popup(
        &RwmConfig::default(),
        scripted_snapshot(),
        options,
        move |root| {
            root_sender
                .send(root.0 as isize)
                .expect("script thread is waiting");
            Some(receiver)
        },
    );
    script.join().expect("script thread panicked");
    result
}

fn assert_scripted_outcome(result: &PopupResult) {
    match &result.chosen_action {
        Some(EntryAction::StaticCommand { verb, .. }) => {
            assert_eq!(
                verb, "documents",
                "Enter must invoke the selected submenu item"
            );
        }
        other => panic!("expected the ドキュメント command from the 送る submenu, got {other:?}"),
    }
    assert_eq!(result.hydrated_groups, 2);
    assert_eq!(
        result.final_item_count, 7,
        "5 snapshot items + 1 inline handler item + 1 extension submenu entry"
    );
}

#[test]
#[ignore = "opens a real popup window on the desktop"]
fn scripted_session_renders_with_direct2d() {
    let result = run_scripted_session((240, 200), "d2d");
    assert_scripted_outcome(&result);
    assert!(
        result.rendered_with_d2d,
        "Direct2D must be available on Windows 11"
    );
}

#[test]
#[ignore = "opens a real popup window on the desktop"]
fn scripted_session_renders_with_the_gdi_fallback() {
    FORCE_GDI.with(|force| force.set(true));
    let result = run_scripted_session((900, 200), "gdi");
    FORCE_GDI.with(|force| force.set(false));
    assert_scripted_outcome(&result);
    assert!(!result.rendered_with_d2d);
}

fn capture(root: HWND, prefix: &str, step: &str) {
    let Some(dir) = std::env::var_os("RWM_UI_SMOKE_CAPTURE") else {
        return;
    };
    let mut window = RECT::default();
    if unsafe { GetWindowRect(root, &mut window) }.is_err() {
        return;
    }
    // Leave room on the right for a cascading submenu.
    let region = RECT {
        left: window.left - 16,
        top: window.top - 16,
        right: window.right + (window.right - window.left) + 48,
        bottom: window.bottom + 16,
    };
    let path = Path::new(&dir).join(format!("{prefix}-{step}.bmp"));
    if let Err(error) = save_screen_region(region, &path) {
        eprintln!("capturing {} failed: {error}", path.display());
    }
}

fn save_screen_region(region: RECT, path: &Path) -> io::Result<()> {
    let width = region.right - region.left;
    let height = region.bottom - region.top;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let (copied, lines) = unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            screen,
            region.left,
            region.top,
            SRCCOPY | CAPTUREBLT,
        );
        SelectObject(memory, previous);
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        (copied, lines)
    };
    copied.map_err(io::Error::other)?;
    if lines == 0 {
        return Err(io::Error::other("GetDIBits copied no scan lines"));
    }
    write_bmp(path, width, height, &pixels)
}

fn write_bmp(path: &Path, width: i32, height: i32, bgra: &[u8]) -> io::Result<()> {
    const HEADERS: u32 = 14 + 40;
    let image_size = bgra.len() as u32;
    let mut file = Vec::with_capacity((HEADERS + image_size) as usize);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(HEADERS + image_size).to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&HEADERS.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&width.to_le_bytes());
    file.extend_from_slice(&(-height).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&image_size.to_le_bytes());
    file.extend_from_slice(&[0u8; 16]);
    file.extend_from_slice(bgra);
    std::fs::write(path, file)
}
