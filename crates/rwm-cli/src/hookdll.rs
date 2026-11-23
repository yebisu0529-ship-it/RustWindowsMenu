use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use windows::core::{s, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Diagnostics::Debug::{
    SetThreadErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::HOOKPROC;

use crate::error::{CliError, CliResult};

pub const HOOK_DLL_FILE: &str = "rwm_hook.dll";

/// The `rwm_abi_version()` this host knows how to drive.
pub const SUPPORTED_ABI: u32 = 1;

/// What `GetProcAddress` hands back before the export gets its real signature.
type RawExport = unsafe extern "system" fn() -> isize;
type AbiVersionFn = unsafe extern "C" fn() -> u32;
type DetachMessageIdFn = unsafe extern "C" fn() -> u32;
type CallWndProcFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

/// Where the hook DLL may sit for a given `rwm.exe`: beside it, or one level up when Cargo
/// runs a test or example binary out of `target/<profile>/deps`.
pub fn candidate_paths(exe: &Path) -> Vec<PathBuf> {
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let mut candidates = vec![dir.join(HOOK_DLL_FILE)];
    let in_cargo_subdir = dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "deps" || name == "examples");
    if in_cargo_subdir {
        if let Some(profile_dir) = dir.parent() {
            candidates.push(profile_dir.join(HOOK_DLL_FILE));
        }
    }
    candidates
}

pub fn locate(exe: &Path) -> Option<PathBuf> {
    candidate_paths(exe).into_iter().find(|path| path.is_file())
}

/// The error for commands that cannot work without the hook DLL.
pub fn not_found_error(exe: &Path) -> CliError {
    let searched: Vec<String> = candidate_paths(exe)
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    CliError::new(format!(
        "{HOOK_DLL_FILE} was not found (looked for {}). It ships next to rwm.exe; \
         from a source checkout, build it with `cargo build -p rwm-hook`.",
        searched.join(", ")
    ))
}

pub fn is_registered_message(id: u32) -> bool {
    (0xC000..=0xFFFF).contains(&id)
}

struct Module(HMODULE);

impl Module {
    fn load(path: &Path) -> windows::core::Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // Without this, Windows answers a corrupt DLL with a modal "Bad Image" dialog,
        // which would hang `rwm doctor` in a script.
        let mut previous_mode = THREAD_ERROR_MODE(0);
        let mode_changed =
            unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut previous_mode)) }.is_ok();
        let loaded = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) };
        if mode_changed {
            unsafe {
                let _ = SetThreadErrorMode(previous_mode, None);
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

/// What a loaded module exposes of the hook contract.
struct Exports {
    abi_version: Option<u32>,
    call_wnd_proc: Option<CallWndProcFn>,
    has_detach_message_id: bool,
    detach_message_id: Option<u32>,
    attach_message_id: Option<u32>,
}

impl Exports {
    fn read(module: &Module) -> Self {
        // SAFETY: the transmutes give each export the signature the frozen hook contract
        // declares for it. `rwm_abi_version` takes no arguments and has no side effects.
        let abi_version = module
            .export(s!("rwm_abi_version"))
            .map(|raw| unsafe { std::mem::transmute::<RawExport, AbiVersionFn>(raw)() });
        let call_wnd_proc = module
            .export(s!("rwm_call_wnd_proc"))
            .map(|raw| unsafe { std::mem::transmute::<RawExport, CallWndProcFn>(raw) });
        let detach_export = module.export(s!("rwm_detach_message_id"));
        // Nothing beyond the version query is called in a DLL that speaks another ABI.
        let detach_message_id = detach_export
            .filter(|_| abi_version == Some(SUPPORTED_ABI))
            .map(|raw| unsafe { std::mem::transmute::<RawExport, DetachMessageIdFn>(raw)() });
        let attach_message_id = module
            .export(s!("rwm_attach_message_id"))
            .filter(|_| abi_version == Some(SUPPORTED_ABI))
            .map(|raw| unsafe { std::mem::transmute::<RawExport, DetachMessageIdFn>(raw)() })
            .filter(|id| is_registered_message(*id));

        Self {
            abi_version,
            call_wnd_proc,
            has_detach_message_id: detach_export.is_some(),
            detach_message_id,
            attach_message_id,
        }
    }
}

/// Everything `rwm status` and `rwm doctor` report about a hook DLL file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookDllProbe {
    pub path: PathBuf,
    pub loaded: bool,
    pub load_error: Option<String>,
    pub abi_version: Option<u32>,
    pub has_call_wnd_proc: bool,
    pub has_detach_message_id: bool,
    pub detach_message_id: Option<u32>,
}

impl HookDllProbe {
    fn of_loaded(path: &Path, exports: &Exports) -> Self {
        Self {
            path: path.to_path_buf(),
            loaded: true,
            load_error: None,
            abi_version: exports.abi_version,
            has_call_wnd_proc: exports.call_wnd_proc.is_some(),
            has_detach_message_id: exports.has_detach_message_id,
            detach_message_id: exports.detach_message_id,
        }
    }

    /// Reasons this DLL cannot be hosted. Empty means `rwm run` will accept it.
    pub fn problems(&self) -> Vec<String> {
        if let Some(error) = &self.load_error {
            return vec![format!("LoadLibraryW failed: {error}")];
        }
        let mut problems = Vec::new();
        match self.abi_version {
            None => problems.push("export rwm_abi_version is missing".to_string()),
            Some(version) if version != SUPPORTED_ABI => problems.push(format!(
                "rwm_abi_version() is {version}, but this rwm.exe needs {SUPPORTED_ABI}"
            )),
            Some(_) => {}
        }
        if !self.has_call_wnd_proc {
            problems.push("export rwm_call_wnd_proc is missing".to_string());
        }
        if !self.has_detach_message_id {
            problems.push("export rwm_detach_message_id is missing".to_string());
        } else if let Some(id) = self
            .detach_message_id
            .filter(|id| !is_registered_message(*id))
        {
            problems.push(format!(
                "rwm_detach_message_id() returned {id:#x}, which is not a registered window message"
            ));
        }
        problems
    }

    pub fn is_usable(&self) -> bool {
        self.problems().is_empty()
    }
}

/// Loads the DLL into this process, reads its exports, and unloads it again.
pub fn probe(path: &Path) -> HookDllProbe {
    match Module::load(path) {
        Err(err) => HookDllProbe {
            path: path.to_path_buf(),
            loaded: false,
            load_error: Some(err.message()),
            abi_version: None,
            has_call_wnd_proc: false,
            has_detach_message_id: false,
            detach_message_id: None,
        },
        Ok(module) => HookDllProbe::of_loaded(path, &Exports::read(&module)),
    }
}

/// A hook DLL that passed every contract check and stays loaded until dropped.
pub struct HookLibrary {
    module: Module,
    path: PathBuf,
    call_wnd_proc: CallWndProcFn,
    detach_message_id: u32,
    attach_message_id: Option<u32>,
}

impl HookLibrary {
    pub fn load(path: &Path) -> CliResult<Self> {
        let module = Module::load(path).map_err(|err| {
            CliError::new(format!("cannot load {}: {}", path.display(), err.message()))
        })?;
        let exports = Exports::read(&module);
        match (
            exports.abi_version,
            exports.call_wnd_proc,
            exports.detach_message_id,
        ) {
            (Some(SUPPORTED_ABI), Some(call_wnd_proc), Some(detach_message_id))
                if is_registered_message(detach_message_id) =>
            {
                Ok(Self {
                    module,
                    path: path.to_path_buf(),
                    call_wnd_proc,
                    detach_message_id,
                    attach_message_id: exports.attach_message_id,
                })
            }
            _ => Err(CliError::new(format!(
                "{} is not a usable hook DLL: {}",
                path.display(),
                HookDllProbe::of_loaded(path, &exports)
                    .problems()
                    .join("; ")
            ))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn module(&self) -> HMODULE {
        self.module.0
    }

    pub fn hook_proc(&self) -> HOOKPROC {
        Some(self.call_wnd_proc)
    }

    pub fn detach_message_id(&self) -> u32 {
        self.detach_message_id
    }

    pub fn attach_message_id(&self) -> Option<u32> {
        self.attach_message_id
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::TempDir;

    fn system_dll(name: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows");
        PathBuf::from(root).join("System32").join(name)
    }

    fn probe_of(abi_version: Option<u32>) -> HookDllProbe {
        HookDllProbe {
            path: PathBuf::from(r"C:\rwm\rwm_hook.dll"),
            loaded: true,
            load_error: None,
            abi_version,
            has_call_wnd_proc: true,
            has_detach_message_id: true,
            detach_message_id: Some(0xC123),
        }
    }

    #[test]
    fn dll_is_searched_beside_the_exe_and_above_cargo_subdirs() {
        assert_eq!(
            candidate_paths(Path::new(r"C:\Apps\rwm\rwm.exe")),
            vec![PathBuf::from(r"C:\Apps\rwm\rwm_hook.dll")]
        );
        assert_eq!(
            candidate_paths(Path::new(r"C:\src\target\debug\deps\rwm-0123abcd.exe")),
            vec![
                PathBuf::from(r"C:\src\target\debug\deps\rwm_hook.dll"),
                PathBuf::from(r"C:\src\target\debug\rwm_hook.dll"),
            ]
        );
    }

    #[test]
    fn locate_prefers_the_first_existing_candidate() {
        let dir = TempDir::new("locate");
        let deps = dir.path().join("deps");
        fs::create_dir(&deps).expect("create deps dir");
        let exe = deps.join("rwm-test.exe");

        assert_eq!(locate(&exe), None);
        assert!(not_found_error(&exe)
            .to_string()
            .contains("rwm_hook.dll was not found"));

        let above = dir.path().join(HOOK_DLL_FILE);
        fs::write(&above, b"").expect("write dll above deps");
        assert_eq!(locate(&exe), Some(above));

        let beside = deps.join(HOOK_DLL_FILE);
        fs::write(&beside, b"").expect("write dll beside exe");
        assert_eq!(locate(&exe), Some(beside));
    }

    #[test]
    fn usable_probe_has_no_problems() {
        assert!(probe_of(Some(SUPPORTED_ABI)).is_usable());
    }

    #[test]
    fn probe_problems_name_each_broken_part_of_the_contract() {
        let wrong_abi = probe_of(Some(2));
        assert_eq!(
            wrong_abi.problems(),
            vec!["rwm_abi_version() is 2, but this rwm.exe needs 1".to_string()]
        );

        let mut stripped = probe_of(None);
        stripped.has_call_wnd_proc = false;
        stripped.has_detach_message_id = false;
        stripped.detach_message_id = None;
        assert_eq!(
            stripped.problems(),
            vec![
                "export rwm_abi_version is missing".to_string(),
                "export rwm_call_wnd_proc is missing".to_string(),
                "export rwm_detach_message_id is missing".to_string(),
            ]
        );

        let mut bad_message = probe_of(Some(SUPPORTED_ABI));
        bad_message.detach_message_id = Some(0x0400);
        assert!(bad_message.problems()[0].contains("not a registered window message"));
    }

    #[test]
    fn a_dll_without_the_hook_exports_is_reported_as_unusable() {
        let kernel32 = system_dll("kernel32.dll");
        let report = probe(&kernel32);
        assert!(report.loaded);
        assert_eq!(report.abi_version, None);
        assert!(!report.has_call_wnd_proc);
        assert!(!report.is_usable());

        let err = HookLibrary::load(&kernel32)
            .err()
            .expect("kernel32 is not a hook DLL");
        assert!(err.to_string().contains("is not a usable hook DLL"));
        assert!(err
            .to_string()
            .contains("export rwm_abi_version is missing"));
    }

    #[test]
    fn a_file_that_is_not_a_dll_fails_to_load() {
        let dir = TempDir::new("not_a_dll");
        let fake = dir.path().join(HOOK_DLL_FILE);
        fs::write(&fake, b"this is not a PE image").expect("write fake dll");

        let report = probe(&fake);
        assert!(!report.loaded);
        assert!(report.problems()[0].starts_with("LoadLibraryW failed:"));
        assert!(HookLibrary::load(&fake)
            .err()
            .expect("a text file cannot be loaded")
            .to_string()
            .starts_with("cannot load "));
    }

    #[test]
    fn built_hook_dll_satisfies_the_contract() {
        let exe = std::env::current_exe().expect("test executable path");
        let Some(dll) = locate(&exe) else {
            eprintln!("skipped: {HOOK_DLL_FILE} is not built (cargo build -p rwm-hook)");
            return;
        };

        let report = probe(&dll);
        assert_eq!(report.problems(), Vec::<String>::new());
        assert_eq!(report.abi_version, Some(SUPPORTED_ABI));
        assert_eq!(
            report.detach_message_id,
            Some(crate::explorer::detach_message_id()),
            "host and DLL must agree on the detach message name"
        );

        let library = HookLibrary::load(&dll).expect("hook DLL loads");
        assert_eq!(library.path(), dll);
        assert!(library.hook_proc().is_some());
        assert_eq!(
            library.detach_message_id(),
            crate::explorer::detach_message_id()
        );
    }
}
