use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rwm_core::build_info::{versioned_dll_filename, ABI_VERSION, BUILD_ID};
use serde::Serialize;
use windows::core::{s, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Diagnostics::Debug::{
    SetThreadErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
};
use windows::Win32::UI::WindowsAndMessaging::{RegisterWindowMessageW, HOOKPROC};

use crate::error::{CliError, CliResult};

pub const LEGACY_DETACH_MESSAGE: &str = "RustWindowsMenu.DetachHook";
pub const LEGACY_V01_DETACH_MESSAGE: &str = "RustWindowsMenu_Detach_v1";

type RawExport = unsafe extern "system" fn() -> isize;
type AbiVersionFn = unsafe extern "C" fn() -> u32;
type BuildIdFn = unsafe extern "C" fn(*mut usize) -> *const u8;
type MessageIdFn = unsafe extern "C" fn() -> u32;
type HookProcFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

pub fn attach_message_name() -> String {
    format!("RustWindowsMenu.AttachHook.{BUILD_ID}")
}

pub fn detach_message_name() -> String {
    format!("RustWindowsMenu.DetachHook.{BUILD_ID}")
}

pub fn register_message(name: &str) -> CliResult<u32> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let id = unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) };
    if is_registered_message(id) {
        Ok(id)
    } else {
        Err(CliError::new(format!(
            "RegisterWindowMessageW failed for {name}: {}",
            windows::core::Error::from_win32().message()
        )))
    }
}

pub fn is_registered_message(id: u32) -> bool {
    (0xC000..=0xFFFF).contains(&id)
}

pub fn candidate_paths(exe: &Path) -> Vec<PathBuf> {
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let versioned = versioned_dll_filename();
    let is_installed_bin = dir.file_name().is_some_and(|name| name.eq_ignore_ascii_case("bin"));
    if is_installed_bin {
        let mut paths = Vec::new();
        if let Some(root) = dir.parent() {
            paths.push(root.join("versions").join(BUILD_ID).join(&versioned));
        }
        paths.push(dir.join(&versioned));
        return paths;
    }
    let mut paths = vec![dir.join(&versioned), dir.join("rwm_hook.dll")];
    if let Some(root) = dir.parent() {
        if dir
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("deps") || name.eq_ignore_ascii_case("examples"))
        {
            paths.push(root.join(&versioned));
            paths.push(root.join("rwm_hook.dll"));
        }
    }
    paths
}

pub fn locate(exe: &Path) -> Option<PathBuf> {
    candidate_paths(exe).into_iter().find(|path| path.is_file())
}

pub fn not_found_error(exe: &Path) -> CliError {
    let searched = candidate_paths(exe)
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    CliError::new(format!(
        "{} was not found (looked for {searched}); build rwm-hook or install a matching release",
        versioned_dll_filename()
    ))
}

struct Module(HMODULE);

impl Module {
    fn load(path: &Path) -> windows::core::Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut previous = THREAD_ERROR_MODE(0);
        let changed =
            unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut previous)) }.is_ok();
        let loaded = unsafe {
            LoadLibraryExW(
                PCWSTR(wide.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if changed {
            unsafe {
                let _ = SetThreadErrorMode(previous, None);
            }
        }
        loaded.map(Self)
    }

    fn export(&self, name: PCSTR) -> Option<RawExport> {
        unsafe { GetProcAddress(self.0, name) }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

struct Exports {
    abi: Option<u32>,
    build_id: Option<String>,
    build_error: Option<String>,
    attach: Option<u32>,
    detach: Option<u32>,
    call: Option<HookProcFn>,
    get_msg: Option<HookProcFn>,
}

impl Exports {
    fn read(module: &Module) -> Self {
        let abi = module
            .export(s!("rwm_abi_version"))
            .map(|raw| unsafe { std::mem::transmute::<RawExport, AbiVersionFn>(raw)() });
        let (build_id, build_error) = match module.export(s!("rwm_build_id")) {
            None => (None, Some("export rwm_build_id is missing".to_string())),
            Some(raw) => {
                let mut len = 0usize;
                let ptr = unsafe { std::mem::transmute::<RawExport, BuildIdFn>(raw)(&mut len) };
                if ptr.is_null() || len == 0 || len > 4096 {
                    (
                        None,
                        Some(format!(
                            "rwm_build_id returned an invalid pointer/length ({len})"
                        )),
                    )
                } else {
                    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                    match std::str::from_utf8(bytes) {
                        Ok(id) => (Some(id.to_string()), None),
                        Err(_) => (None, Some("rwm_build_id is not UTF-8".to_string())),
                    }
                }
            }
        };
        let compatible = abi == Some(ABI_VERSION) && build_id.as_deref() == Some(BUILD_ID);
        let message = |name| {
            module
                .export(name)
                .filter(|_| compatible)
                .map(|raw| unsafe { std::mem::transmute::<RawExport, MessageIdFn>(raw)() })
        };
        let hook = |name| {
            module
                .export(name)
                .map(|raw| unsafe { std::mem::transmute::<RawExport, HookProcFn>(raw) })
        };
        Self {
            abi,
            build_id,
            build_error,
            attach: message(s!("rwm_attach_message_id")),
            detach: message(s!("rwm_detach_message_id")),
            call: hook(s!("rwm_call_wnd_proc")),
            get_msg: hook(s!("rwm_get_msg_proc")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookDllProbe {
    pub path: PathBuf,
    pub loaded: bool,
    pub load_error: Option<String>,
    pub abi_version: Option<u32>,
    pub build_id: Option<String>,
    pub attach_message_id: Option<u32>,
    pub detach_message_id: Option<u32>,
    pub has_call_wnd_proc: bool,
    pub has_get_msg_proc: bool,
    pub problems: Vec<String>,
}

impl HookDllProbe {
    pub fn is_usable(&self) -> bool {
        self.problems.is_empty()
    }

    fn from_exports(path: &Path, exports: &Exports) -> Self {
        let mut problems = Vec::new();
        match exports.abi {
            None => problems.push("export rwm_abi_version is missing".to_string()),
            Some(abi) if abi != ABI_VERSION => problems.push(format!(
                "ABI mismatch: DLL has {abi}, host requires {ABI_VERSION}"
            )),
            _ => {}
        }
        if let Some(error) = &exports.build_error {
            problems.push(error.clone());
        } else if exports.build_id.as_deref() != Some(BUILD_ID) {
            problems.push(format!(
                "BUILD_ID mismatch: DLL has {:?}, host requires {BUILD_ID}",
                exports.build_id
            ));
        }
        for (present, name) in [
            (exports.call.is_some(), "rwm_call_wnd_proc"),
            (exports.get_msg.is_some(), "rwm_get_msg_proc"),
            (exports.attach.is_some(), "rwm_attach_message_id"),
            (exports.detach.is_some(), "rwm_detach_message_id"),
        ] {
            if !present {
                problems.push(format!("export {name} is missing or unusable"));
            }
        }
        for (id, name, expected) in [
            (exports.attach, "attach", attach_message_name()),
            (exports.detach, "detach", detach_message_name()),
        ] {
            if let Some(id) = id {
                if !is_registered_message(id) {
                    problems.push(format!(
                        "rwm_{name}_message_id is not a registered window message"
                    ));
                } else if register_message(&expected).is_ok_and(|want| want != id) {
                    problems.push(format!(
                        "rwm_{name}_message_id does not match {expected}"
                    ));
                }
            }
        }
        Self {
            path: path.to_path_buf(),
            loaded: true,
            load_error: None,
            abi_version: exports.abi,
            build_id: exports.build_id.clone(),
            attach_message_id: exports.attach,
            detach_message_id: exports.detach,
            has_call_wnd_proc: exports.call.is_some(),
            has_get_msg_proc: exports.get_msg.is_some(),
            problems,
        }
    }
}

pub fn probe(path: &Path) -> HookDllProbe {
    match Module::load(path) {
        Ok(module) => HookDllProbe::from_exports(path, &Exports::read(&module)),
        Err(error) => HookDllProbe {
            path: path.to_path_buf(),
            loaded: false,
            load_error: Some(error.message()),
            abi_version: None,
            build_id: None,
            attach_message_id: None,
            detach_message_id: None,
            has_call_wnd_proc: false,
            has_get_msg_proc: false,
            problems: vec![format!("LoadLibraryExW failed: {}", error.message())],
        },
    }
}

pub struct HookLibrary {
    module: Module,
    path: PathBuf,
    call: HookProcFn,
    attach: u32,
    detach: u32,
}

impl HookLibrary {
    pub fn load(path: &Path) -> CliResult<Self> {
        let module = Module::load(path).map_err(|error| {
            CliError::new(format!(
                "cannot load {}: {}",
                path.display(),
                error.message()
            ))
        })?;
        let exports = Exports::read(&module);
        let report = HookDllProbe::from_exports(path, &exports);
        if !report.is_usable() {
            return Err(CliError::new(format!(
                "refusing incompatible hook DLL {}: {}",
                path.display(),
                report.problems.join("; ")
            )));
        }
        Ok(Self {
            module,
            path: path.to_path_buf(),
            call: exports.call.unwrap(),
            attach: exports.attach.unwrap(),
            detach: exports.detach.unwrap(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn module(&self) -> HMODULE {
        self.module.0
    }
    pub fn hook_proc(&self) -> HOOKPROC {
        Some(self.call)
    }
    pub fn attach_message_id(&self) -> u32 {
        self.attach
    }
    pub fn detach_message_id(&self) -> u32 {
        self.detach
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_system_dll_with_missing_contract_exports() {
        let path =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\kernel32.dll");
        let report = probe(&path);
        assert!(report.loaded);
        assert!(!report.is_usable());
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.contains("rwm_abi_version")));
        assert!(HookLibrary::load(&path)
            .err()
            .expect("kernel32 must be rejected")
            .to_string()
            .contains("refusing incompatible"));
    }

    #[test]
    fn abi_and_build_mismatches_are_explicitly_rejected() {
        let exports = Exports {
            abi: Some(99),
            build_id: Some("wrong-build".into()),
            build_error: None,
            attach: None,
            detach: None,
            call: None,
            get_msg: None,
        };
        let report = HookDllProbe::from_exports(Path::new("bad.dll"), &exports);
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.contains("ABI mismatch")));
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.contains("BUILD_ID mismatch")));
        assert!(!report.is_usable());
    }

    #[test]
    fn resolution_prefers_versioned_install_and_skips_legacy_bin_dll() {
        let installed = candidate_paths(Path::new(r"C:\Apps\RWM\bin\rwm.exe"));
        let expected_versioned = PathBuf::from(format!(
            r"C:\Apps\RWM\versions\{BUILD_ID}\{}",
            versioned_dll_filename()
        ));
        assert_eq!(installed.first(), Some(&expected_versioned));
        assert!(!installed.contains(&PathBuf::from(r"C:\Apps\RWM\bin\rwm_hook.dll")));
        let dev = candidate_paths(Path::new(r"C:\src\target\release\rwm.exe"));
        assert!(dev.contains(&PathBuf::from(r"C:\src\target\release\rwm_hook.dll")));
    }
}
