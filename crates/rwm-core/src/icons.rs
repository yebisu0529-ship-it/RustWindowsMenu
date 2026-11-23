use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, S_FALSE, S_OK};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Shell::{
    ExtractIconExW, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON,
    SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::model::BgraIcon;

pub struct IconCache {
    cache: RwLock<HashMap<String, Option<Arc<BgraIcon>>>>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

struct DcGuard {
    hwnd: HWND,
    hdc: HDC,
    is_compat: bool,
}

impl Drop for DcGuard {
    fn drop(&mut self) {
        if !self.hdc.is_invalid() {
            unsafe {
                if self.is_compat {
                    let _ = DeleteDC(self.hdc);
                } else {
                    ReleaseDC(self.hwnd, self.hdc);
                }
            }
        }
    }
}

struct BitmapGuard(HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0.0));
            }
        }
    }
}

impl IconCache {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::with_capacity(128)),
        }
    }

    pub fn clear(&self) {
        self.cache.write().clear();
    }

    pub fn load_icon_spec(&self, spec: &str, _size: u32) -> Option<Arc<BgraIcon>> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return None;
        }
        let cache_key = format!("spec:{trimmed}");
        if let Some(hit) = self.cache.read().get(&cache_key) {
            return hit.clone();
        }

        let icon = load_icon_from_spec_uncached(trimmed).map(Arc::new);
        self.cache.write().insert(cache_key, icon.clone());
        icon
    }

    pub fn load_file_icon(&self, path: &Path, is_dir: bool) -> Option<Arc<BgraIcon>> {
        let cache_key = if is_dir {
            "shell:dir".to_string()
        } else {
            match path.extension().and_then(|e| e.to_str()) {
                Some(ext) => {
                    let lower = ext.to_ascii_lowercase();
                    if matches!(lower.as_str(), "exe" | "ico" | "lnk" | "scr" | "cur") {
                        format!("file:{}", path.to_string_lossy().to_ascii_lowercase())
                    } else {
                        format!("ext:.{lower}")
                    }
                }
                None => "ext:__none__".to_string(),
            }
        };

        if let Some(hit) = self.cache.read().get(&cache_key) {
            return hit.clone();
        }

        let icon = load_shell_small_icon(path, is_dir).map(Arc::new);
        self.cache.write().insert(cache_key, icon.clone());
        icon
    }

    pub fn from_hicon(hicon: HICON) -> Option<BgraIcon> {
        if hicon.is_invalid() {
            return None;
        }
        unsafe {
            let mut info = ICONINFO::default();
            if GetIconInfo(hicon, &mut info).is_err() {
                return None;
            }
            let _color_guard = BitmapGuard(info.hbmColor);
            let _mask_guard = BitmapGuard(info.hbmMask);

            let screen_dc = DcGuard {
                hwnd: HWND::default(),
                hdc: GetDC(HWND::default()),
                is_compat: false,
            };
            if screen_dc.hdc.is_invalid() {
                return None;
            }
            let mem_dc = DcGuard {
                hwnd: HWND::default(),
                hdc: CreateCompatibleDC(screen_dc.hdc),
                is_compat: true,
            };
            if mem_dc.hdc.is_invalid() {
                return None;
            }

            if !info.hbmColor.is_invalid() {
                let mut bmp = BITMAP::default();
                if GetObjectW(
                    HGDIOBJ(info.hbmColor.0),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some(&mut bmp as *mut _ as *mut _),
                ) == 0
                {
                    return None;
                }
                let w = bmp.bmWidth;
                let h = bmp.bmHeight;
                if w <= 0 || h <= 0 || w > 256 || h > 256 {
                    return None;
                }

                let mut bmi = top_down_bgra_bmi(w, h);
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                let rows = GetDIBits(
                    mem_dc.hdc,
                    info.hbmColor,
                    0,
                    h as u32,
                    Some(pixels.as_mut_ptr() as *mut _),
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                if rows == 0 {
                    return None;
                }

                let has_alpha = pixels.chunks_exact(4).any(|px| px[3] != 0);
                if !has_alpha {
                    if !info.hbmMask.is_invalid() {
                        let mut mask_pixels = vec![0u8; (w * h * 4) as usize];
                        let mut mask_bmi = top_down_bgra_bmi(w, h);
                        let mask_rows = GetDIBits(
                            mem_dc.hdc,
                            info.hbmMask,
                            0,
                            h as u32,
                            Some(mask_pixels.as_mut_ptr() as *mut _),
                            &mut mask_bmi,
                            DIB_RGB_COLORS,
                        );
                        if mask_rows > 0 {
                            for (px, mpx) in
                                pixels.chunks_exact_mut(4).zip(mask_pixels.chunks_exact(4))
                            {
                                // In AND mask: 0 = opaque pixel, non-zero = transparent pixel.
                                px[3] = if mpx[0] == 0 && mpx[1] == 0 && mpx[2] == 0 {
                                    255
                                } else {
                                    0
                                };
                            }
                        } else {
                            for px in pixels.chunks_exact_mut(4) {
                                px[3] = 255;
                            }
                        }
                    } else {
                        for px in pixels.chunks_exact_mut(4) {
                            px[3] = 255;
                        }
                    }
                }

                premultiply_bgra(&mut pixels);
                return Some(BgraIcon {
                    width: w as u32,
                    height: h as u32,
                    pixels,
                });
            }

            None
        }
    }

    pub fn from_hbitmap(hbitmap: HBITMAP) -> Option<BgraIcon> {
        let raw = hbitmap.0 as isize;
        // Skip null or HBMMENU_* sentinel pseudo-handles (-1 ..= 11).
        if raw == 0 || (-1..=11).contains(&raw) {
            return None;
        }
        unsafe {
            let mut bmp = BITMAP::default();
            if GetObjectW(
                HGDIOBJ(hbitmap.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut _ as *mut _),
            ) == 0
            {
                return None;
            }
            let w = bmp.bmWidth;
            let h = bmp.bmHeight;
            if w <= 0 || h <= 0 || w > 128 || h > 128 {
                return None;
            }

            let screen_dc = DcGuard {
                hwnd: HWND::default(),
                hdc: GetDC(HWND::default()),
                is_compat: false,
            };
            if screen_dc.hdc.is_invalid() {
                return None;
            }
            let mem_dc = DcGuard {
                hwnd: HWND::default(),
                hdc: CreateCompatibleDC(screen_dc.hdc),
                is_compat: true,
            };
            if mem_dc.hdc.is_invalid() {
                return None;
            }

            let mut bmi = top_down_bgra_bmi(w, h);
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let rows = GetDIBits(
                mem_dc.hdc,
                hbitmap,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if rows == 0 {
                return None;
            }

            let has_alpha = pixels.chunks_exact(4).any(|px| px[3] != 0);
            if !has_alpha {
                // Legacy GDI menu bitmaps use 0x00 alpha for opaque pixels; force full alpha so they render.
                for px in pixels.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            } else {
                premultiply_bgra(&mut pixels);
            }

            Some(BgraIcon {
                width: w as u32,
                height: h as u32,
                pixels,
            })
        }
    }
}

fn top_down_bgra_bmi(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: Default::default(),
    }
}

fn premultiply_bgra(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        let a = px[3] as u16;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            px[0] = ((px[0] as u16 * a + 127) / 255) as u8;
            px[1] = ((px[1] as u16 * a + 127) / 255) as u8;
            px[2] = ((px[2] as u16 * a + 127) / 255) as u8;
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn expand_env_vars(raw: &str) -> String {
    let wide = to_wide_null(raw);
    let mut buf = vec![0u16; 1024];
    let len = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    if len == 0 || (len as usize) > buf.len() {
        return raw.to_string();
    }
    let slice = &buf[..(len as usize).saturating_sub(1)];
    String::from_utf16_lossy(slice)
}

fn parse_icon_location(raw: &str) -> (String, i32) {
    let expanded = expand_env_vars(raw);
    let cleaned = expanded.trim().trim_matches('"').trim();
    if let Some((path_part, idx_part)) = cleaned.rsplit_once(',') {
        if let Ok(idx) = idx_part.trim().parse::<i32>() {
            let path = path_part.trim().trim_matches('"').to_string();
            return (path, idx);
        }
    }
    (cleaned.to_string(), 0)
}

fn load_icon_from_spec_uncached(spec: &str) -> Option<BgraIcon> {
    let (path, idx) = parse_icon_location(spec);
    if path.is_empty() {
        return None;
    }

    let wide = to_wide_null(&path);
    unsafe {
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR(wide.as_ptr()),
            idx,
            None,
            Some(&mut small_icon),
            1,
        );
        if extracted > 0 && !small_icon.is_invalid() {
            let icon = IconCache::from_hicon(small_icon);
            let _ = DestroyIcon(small_icon);
            if icon.is_some() {
                return icon;
            }
        }
    }

    load_shell_small_icon(Path::new(&path), false)
}

struct ComStaGuard(bool);

impl ComStaGuard {
    fn enter() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self(hr == S_OK || hr == S_FALSE)
    }
}

impl Drop for ComStaGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn load_shell_small_icon(path: &Path, is_dir: bool) -> Option<BgraIcon> {
    let _com = ComStaGuard::enter();
    let path_str = if is_dir || path.exists() {
        path.to_string_lossy().into_owned()
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file.txt".to_string())
    };
    let wide = to_wide_null(&path_str);
    let attrs = if is_dir {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    unsafe {
        let mut sfi = SHFILEINFOW::default();
        let flags = SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES;
        let res = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if res == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let icon = IconCache::from_hicon(sfi.hIcon);
        let _ = DestroyIcon(sfi.hIcon);
        icon
    }
}

pub fn _unused_pwstr(_p: PWSTR) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_icon_spec_locations() {
        assert_eq!(
            parse_icon_location(r#""C:\Windows\System32\shell32.dll",-167"#),
            (r"C:\Windows\System32\shell32.dll".to_string(), -167)
        );
        assert_eq!(
            parse_icon_location(r"C:\app\demo.exe, 0"),
            (r"C:\app\demo.exe".to_string(), 0)
        );
    }

    #[test]
    fn loads_real_windows_shell_icon_into_premultiplied_bgra() {
        let cache = IconCache::new();
        let icon = cache.load_icon_spec(r"shell32.dll,-4", 16);
        assert!(icon.is_some(), "expected shell32 folder icon to load");
        let icon = icon.unwrap();
        assert!(icon.width > 0 && icon.height > 0);
        assert_eq!(icon.pixels.len(), (icon.width * icon.height * 4) as usize);
    }
}
