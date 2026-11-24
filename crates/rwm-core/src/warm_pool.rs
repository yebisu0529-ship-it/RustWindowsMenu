use std::collections::HashSet;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
};
use windows::Win32::System::ProcessStatus::K32EnumProcessModules;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows_core::PCWSTR;

pub struct ModuleSnapshot(Option<HashSet<usize>>);

pub struct WarmDllPin {
    pinned: Vec<(HMODULE, PathBuf)>,
    max_pinned: usize,
}

impl WarmDllPin {
    pub fn new(max_pinned: usize) -> Self {
        Self {
            pinned: Vec::new(),
            max_pinned: max_pinned.min(128),
        }
    }

    pub fn snapshot_loaded_modules() -> ModuleSnapshot {
        snapshot_loaded_modules()
    }

    pub fn pin_newly_loaded(&mut self, before: &ModuleSnapshot) -> usize {
        let Some(before) = &before.0 else { return 0 };
        if self.pinned.len() >= self.max_pinned {
            return 0;
        }
        let Some(windows) = system_windows_dir() else {
            return 0;
        };
        let Some(after) = snapshot_loaded_modules().0 else {
            return 0;
        };
        let original_count = self.pinned.len();
        let mut candidates: Vec<_> = after
            .difference(before)
            .filter_map(|&base| {
                let path = module_path(HMODULE(base as *mut _))?;
                (!is_core_system_module(&path, &windows)
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("dll")))
                .then_some((base, path))
            })
            .collect();
        candidates.sort_by(|a, b| a.1.cmp(&b.1));
        for (base, path) in candidates {
            if self.pinned.len() >= self.max_pinned {
                break;
            }
            if self
                .pinned
                .iter()
                .any(|(_, known)| known.as_os_str().eq_ignore_ascii_case(path.as_os_str()))
            {
                continue;
            }
            let mut module = HMODULE::default();
            // Take a reference only if the image is still mapped at this address.
            if unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                    PCWSTR(base as *const u16),
                    &mut module,
                )
            }
            .is_err()
            {
                continue;
            }
            if module.0 as usize == base && module_path(module).as_ref() == Some(&path) {
                self.pinned.push((module, path));
            } else {
                unsafe {
                    let _ = FreeLibrary(module);
                }
            }
        }
        self.pinned.len() - original_count
    }

    pub fn release_all(&mut self) {
        for (module, _) in self.pinned.drain(..).rev() {
            unsafe {
                let _ = FreeLibrary(module);
            }
        }
    }
}

impl Drop for WarmDllPin {
    fn drop(&mut self) {
        self.release_all();
    }
}

pub fn snapshot_loaded_modules() -> ModuleSnapshot {
    let mut handles = vec![HMODULE::default(); 256];
    loop {
        let bytes = std::mem::size_of_val(handles.as_slice()) as u32;
        let mut needed = 0;
        let success = unsafe {
            K32EnumProcessModules(
                GetCurrentProcess(),
                handles.as_mut_ptr(),
                bytes,
                &mut needed,
            )
        };
        if !success.as_bool() {
            return ModuleSnapshot(None);
        }
        let count = needed as usize / size_of::<HMODULE>();
        if needed <= bytes {
            handles.truncate(count);
            return ModuleSnapshot(Some(
                handles
                    .into_iter()
                    .map(|module| module.0 as usize)
                    .collect(),
            ));
        }
        handles.resize(count + 32, HMODULE::default());
    }
}

fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buffer = vec![0; 260];
    loop {
        let len = unsafe { GetModuleFileNameW(module, &mut buffer) } as usize;
        if len == 0 || (len == buffer.len() && buffer.len() >= 32768) {
            return None;
        }
        if len < buffer.len() {
            return Some(PathBuf::from(OsString::from_wide(&buffer[..len])));
        }
        buffer.resize((buffer.len() * 2).min(32768), 0);
    }
}

fn system_windows_dir() -> Option<PathBuf> {
    let mut buffer = [0; 260];
    let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) } as usize;
    (len > 0 && len < buffer.len()).then(|| PathBuf::from(OsString::from_wide(&buffer[..len])))
}

fn is_core_system_module(path: &Path, windows: &Path) -> bool {
    let mut parts = path.iter();
    for part in windows.iter() {
        if !parts.next().is_some_and(|p| p.eq_ignore_ascii_case(part)) {
            return false;
        }
    }
    parts.next().is_some_and(|part| {
        ["System32", "SysWOW64", "WinSxS"]
            .iter()
            .any(|name| part.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_modules_and_releases_empty_pool() {
        let snapshot = snapshot_loaded_modules();
        assert!(!snapshot.0.as_ref().unwrap().is_empty());
        let mut pool = WarmDllPin::new(0);
        assert_eq!(pool.pin_newly_loaded(&snapshot), 0);
        pool.release_all();
        assert!(pool.pinned.is_empty());
    }

    #[test]
    fn excludes_core_paths_case_insensitively() {
        let windows = Path::new(r"C:\Windows");
        assert!(is_core_system_module(
            Path::new(r"c:\WINDOWS\System32\shell32.dll"),
            windows
        ));
        assert!(is_core_system_module(
            Path::new(r"C:\Windows\winsxs\one\test.dll"),
            windows
        ));
        assert!(!is_core_system_module(
            Path::new(r"C:\Windows\explorerframe.dll"),
            windows
        ));
        assert!(!is_core_system_module(
            Path::new(r"C:\WindowsOther\System32\vendor.dll"),
            windows
        ));
    }
}
