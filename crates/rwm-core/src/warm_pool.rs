use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
};
use windows::Win32::System::ProcessStatus::K32EnumProcessModules;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows_core::PCWSTR;

use crate::fasthash::FastSet;

pub struct ModuleSnapshot(Option<FastSet<usize>>);

pub struct WarmDllPin {
    pinned: Vec<(HMODULE, PathBuf)>,
    max_pinned: usize,
    handles: Vec<HMODULE>,
    after_scratch: FastSet<usize>,
    last_needed_bytes: u32,
}

impl WarmDllPin {
    pub fn new(max_pinned: usize) -> Self {
        Self {
            pinned: Vec::new(),
            max_pinned: max_pinned.min(128),
            handles: vec![HMODULE::default(); 256],
            after_scratch: FastSet::default(),
            last_needed_bytes: 0,
        }
    }

    #[inline]
    pub fn should_snapshot(&self) -> bool {
        self.pinned.len() < self.max_pinned
    }

    #[inline]
    pub fn max_pinned(&self) -> usize {
        self.max_pinned
    }

    #[inline]
    pub fn last_needed_bytes(&self) -> u32 {
        self.last_needed_bytes
    }

    /// 1-syscall check returning the byte size required to enumerate all currently loaded modules.
    pub fn probe_module_bytes(&self) -> Option<u32> {
        let mut probe = HMODULE::default();
        let mut needed = 0u32;
        let ok = unsafe {
            K32EnumProcessModules(
                GetCurrentProcess(),
                &mut probe,
                size_of::<HMODULE>() as u32,
                &mut needed,
            )
        };
        ok.as_bool().then_some(needed)
    }

    pub fn snapshot_loaded_modules() -> ModuleSnapshot {
        let mut handles = vec![HMODULE::default(); 256];
        let mut set = FastSet::default();
        if fill_module_set(&mut handles, &mut set).is_some() {
            ModuleSnapshot(Some(set))
        } else {
            ModuleSnapshot(None)
        }
    }

    pub fn pin_newly_loaded(&mut self, before: &ModuleSnapshot) -> usize {
        let Some(before) = &before.0 else { return 0 };
        if !self.should_snapshot() {
            return 0;
        }
        let Some(windows) = system_windows_dir() else {
            return 0;
        };
        let Some(needed) = fill_module_set(&mut self.handles, &mut self.after_scratch) else {
            return 0;
        };
        self.last_needed_bytes = needed;
        let original_count = self.pinned.len();
        let mut candidates: Vec<_> = self
            .after_scratch
            .difference(before)
            .filter_map(|&base| {
                let path = module_path(HMODULE(base as *mut _))?;
                (!is_core_system_module(&path, windows)
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("dll")))
                .then_some((base, path))
            })
            .collect();
        candidates.sort_by(|a, b| a.1.cmp(&b.1));
        for (base, path) in candidates {
            if !self.should_snapshot() {
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

fn fill_module_set(handles: &mut Vec<HMODULE>, out: &mut FastSet<usize>) -> Option<u32> {
    if handles.is_empty() {
        handles.resize(256, HMODULE::default());
    }
    loop {
        let bytes = std::mem::size_of_val(handles.as_slice()) as u32;
        let mut needed = 0;
        let success = unsafe {
            K32EnumProcessModules(GetCurrentProcess(), handles.as_mut_ptr(), bytes, &mut needed)
        };
        if !success.as_bool() {
            return None;
        }
        let count = needed as usize / size_of::<HMODULE>();
        if needed <= bytes {
            out.clear();
            out.reserve(count);
            for &module in &handles[..count] {
                out.insert(module.0 as usize);
            }
            return Some(needed);
        }
        handles.resize(count + 32, HMODULE::default());
    }
}

pub fn snapshot_loaded_modules() -> ModuleSnapshot {
    WarmDllPin::snapshot_loaded_modules()
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

fn system_windows_dir() -> Option<&'static Path> {
    static WINDOWS_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    WINDOWS_DIR
        .get_or_init(|| {
            let mut buffer = [0; 260];
            let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) } as usize;
            (len > 0 && len < buffer.len())
                .then(|| PathBuf::from(OsString::from_wide(&buffer[..len])))
        })
        .as_deref()
}

/// Returns `true` when `path` resides under `<Windows>\System32`, `SysWOW64`, or `WinSxS`.
pub fn is_system_module(path: &Path) -> bool {
    system_windows_dir().is_some_and(|windows| is_core_system_module(path, windows))
}

/// Returns `true` for OS core system DLLs as well as built-in Windows Defender shell extensions.
pub fn is_builtin_windows_handler(path: &Path) -> bool {
    if is_system_module(path) {
        return true;
    }
    let candidate = path.to_string_lossy();
    let lower = candidate.to_ascii_lowercase();
    for env_key in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(root) = std::env::var_os(env_key) {
            let prefix = format!(
                "{}\\windows defender\\",
                root.to_string_lossy().trim_end_matches('\\').to_ascii_lowercase()
            );
            if lower.starts_with(&prefix) {
                return true;
            }
        }
    }
    if let Some(root) = std::env::var_os("ProgramData") {
        let prefix = format!(
            "{}\\microsoft\\windows defender\\",
            root.to_string_lossy().trim_end_matches('\\').to_ascii_lowercase()
        );
        if lower.starts_with(&prefix) {
            return true;
        }
    }
    false
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
        assert!(!pool.should_snapshot());
        assert_eq!(pool.pin_newly_loaded(&snapshot), 0);
        pool.release_all();
        assert!(pool.pinned.is_empty());

        let warm = WarmDllPin::new(4);
        assert!(warm.should_snapshot());
        assert!(warm.probe_module_bytes().unwrap_or(0) > 0);
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
        assert!(is_builtin_windows_handler(Path::new(
            r"C:\Program Files\Windows Defender\shellext.dll"
        )));
        assert!(!is_builtin_windows_handler(Path::new(
            r"C:\Program Files\Vendor\ext.dll"
        )));
    }
}
