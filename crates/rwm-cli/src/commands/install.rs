use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use rwm_core::classic::{self, ClassicRegistryScope};
use rwm_core::config::RwmConfig;

use super::classic::{disable as disable_classic, enable as enable_classic, AppliedClassic};
use crate::cli::{InstallArgs, UninstallArgs};
use crate::error::{CliError, CliResult, Context};
use crate::explorer::{self, DetachDelivery, ShellWindow};
use crate::hookdll::{self, HOOK_DLL_FILE};
use crate::host::{self, StopOutcome, HOST_EVENT_NAME};
use crate::output::print;
use crate::reg;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "RustWindowsMenu";

const INSTALLED_EXE: &str = "rwm.exe";
const HOST_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Suffix of an installed file that was in use during an upgrade and renamed out of the way.
const SET_ASIDE_SUFFIX: &str = "old";
/// Suffix of a file that was in use during an uninstall and moved out of the bin folder.
const LEFTOVER_SUFFIX: &str = "removed";

/// Every place an installation touches. Tests point all of it at scratch locations.
pub struct InstallEnv {
    pub classic: ClassicRegistryScope,
    /// Whether classic-menu changes are announced to the shell. Off for scratch scopes.
    pub notify_shell: bool,
    pub bin_dir: PathBuf,
    /// HKCU subkey that holds the autostart value.
    pub run_key: String,
    pub host_event: String,
    /// The running `rwm.exe`, which is the file that gets installed.
    pub exe: PathBuf,
    pub hook_dll: Option<PathBuf>,
    /// Creates the default config when there is none. Returns its path and whether it was
    /// created.
    pub ensure_config: fn() -> CliResult<(PathBuf, bool)>,
    pub explorer_windows: fn() -> Vec<ShellWindow>,
}

fn ensure_default_config() -> CliResult<(PathBuf, bool)> {
    Ok(RwmConfig::write_default_if_missing()?)
}

impl InstallEnv {
    pub fn live() -> CliResult<Self> {
        let exe = std::env::current_exe()
            .context(|| "cannot locate the running executable".to_string())?;
        Ok(Self {
            classic: ClassicRegistryScope::live(),
            notify_shell: true,
            bin_dir: RwmConfig::local_data_dir().join("bin"),
            run_key: RUN_KEY.to_string(),
            host_event: HOST_EVENT_NAME.to_string(),
            hook_dll: hookdll::locate(&exe),
            exe,
            ensure_config: ensure_default_config,
            explorer_windows: explorer::enumerate_explorer_windows,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
    Copied,
    /// The source already is the installed file: `rwm install` ran from the bin folder.
    AlreadyInPlace,
    /// The old file was in use, so it was renamed aside and the new one put in its place.
    ReplacedInUse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFile {
    pub path: PathBuf,
    pub outcome: FileOutcome,
}

#[derive(Debug)]
pub struct InstallReport {
    pub classic: AppliedClassic,
    pub config_path: PathBuf,
    pub config_created: bool,
    /// Empty with `--classic-only`.
    pub files: Vec<InstalledFile>,
    pub autostart: Option<String>,
}

/// What `rwm install` was asked to set up. `--classic-only` installs no files, so there is
/// nothing for an autostart entry to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    ClassicOnly,
    Full { autostart: bool },
}

impl From<&InstallArgs> for InstallMode {
    fn from(args: &InstallArgs) -> Self {
        if args.classic_only {
            Self::ClassicOnly
        } else {
            Self::Full {
                autostart: args.autostart,
            }
        }
    }
}

/// The Run-key command line: the quoted path of the installed `rwm.exe`, then `run --hidden`.
pub fn autostart_command(installed_exe: &Path) -> String {
    format!("\"{}\" run --hidden", installed_exe.display())
}

pub fn install_with(env: &InstallEnv, mode: InstallMode) -> CliResult<InstallReport> {
    // Checked first so a missing DLL stops the install before anything has changed.
    let hook_dll = match mode {
        InstallMode::ClassicOnly => None,
        InstallMode::Full { .. } => Some(env.hook_dll.as_deref().ok_or_else(|| {
            CliError::new(format!(
                "{} To switch on only the classic menu, use `rwm install --classic-only`.",
                hookdll::not_found_error(&env.exe)
            ))
        })?),
    };

    let classic = enable_classic(&env.classic, true, false, env.notify_shell)?;
    let (config_path, config_created) = (env.ensure_config)()?;
    let files = match hook_dll {
        Some(dll) => install_files(&env.exe, dll, &env.bin_dir)?,
        None => Vec::new(),
    };
    let autostart = if mode == (InstallMode::Full { autostart: true }) {
        let command = autostart_command(&env.bin_dir.join(INSTALLED_EXE));
        reg::write_string(&env.run_key, RUN_VALUE, &command)
            .context(|| format!(r"cannot write HKCU\{}\{RUN_VALUE}", env.run_key))?;
        Some(command)
    } else {
        None
    };

    Ok(InstallReport {
        classic,
        config_path,
        config_created,
        files,
        autostart,
    })
}

fn install_files(exe: &Path, hook_dll: &Path, bin_dir: &Path) -> CliResult<Vec<InstalledFile>> {
    fs::create_dir_all(bin_dir).context(|| format!("cannot create {}", bin_dir.display()))?;
    remove_files_with_suffix(bin_dir, SET_ASIDE_SUFFIX);
    if let Some(data_dir) = bin_dir.parent() {
        remove_files_with_suffix(data_dir, LEFTOVER_SUFFIX);
    }

    [(exe, INSTALLED_EXE), (hook_dll, HOOK_DLL_FILE)]
        .into_iter()
        .map(|(source, name)| {
            let path = bin_dir.join(name);
            let outcome = install_file(source, &path)
                .context(|| format!("cannot copy {} to {}", source.display(), path.display()))?;
            Ok(InstalledFile { path, outcome })
        })
        .collect()
}

fn install_file(source: &Path, destination: &Path) -> io::Result<FileOutcome> {
    if is_same_file(source, destination) {
        return Ok(FileOutcome::AlreadyInPlace);
    }
    let Err(copy_error) = fs::copy(source, destination) else {
        return Ok(FileOutcome::Copied);
    };
    if !destination.exists() {
        return Err(copy_error);
    }

    // The destination is a running rwm.exe or a hook DLL still mapped into Explorer. Windows
    // refuses to overwrite such a file but allows renaming it, and whoever has it open keeps
    // using the renamed copy.
    let aside = sibling_with_suffix(destination, SET_ASIDE_SUFFIX);
    fs::rename(destination, &aside)?;
    if let Err(err) = fs::copy(source, destination) {
        let _ = fs::rename(&aside, destination);
        return Err(err);
    }
    Ok(FileOutcome::ReplacedInUse)
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `rwm.exe` becomes `rwm.exe.<pid>.<suffix>` in `directory`.
fn renamed_into(directory: &Path, file: &Path, suffix: &str) -> PathBuf {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    directory.join(format!("{name}.{}.{suffix}", std::process::id()))
}

fn sibling_with_suffix(file: &Path, suffix: &str) -> PathBuf {
    renamed_into(file.parent().unwrap_or(Path::new(".")), file, suffix)
}

/// Deletes what earlier runs had to rename out of the way. Files still in use stay.
fn remove_files_with_suffix(directory: &Path, suffix: &str) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_some_and(|ext| ext == suffix) {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    NotInstalled,
    Removed,
    /// The folder is gone, but these files were in use and could only be moved next to it.
    RemovedWithLeftovers(Vec<PathBuf>),
    Failed(String),
}

/// What uninstall did about the classic context menu.
#[derive(Debug)]
pub enum ClassicStep {
    /// `--keep-classic` was given.
    Kept,
    /// rwm holds no backup, so it never switched the classic menu on here. Whatever state
    /// the menu is in was set by something else and is not rwm's to undo.
    NothingToRestore,
    Restored(AppliedClassic),
    Failed(String),
}

#[derive(Debug)]
pub struct UninstallReport {
    /// Whether an autostart value existed and was deleted.
    pub autostart_removed: Result<bool, String>,
    pub host: StopOutcome,
    /// `None` when no Explorer window was open to receive the detach message.
    pub detach: Option<DetachDelivery>,
    pub files: Removal,
    pub classic: ClassicStep,
}

impl UninstallReport {
    pub fn succeeded(&self) -> bool {
        self.autostart_removed.is_ok()
            && self.host != StopOutcome::TimedOut
            && !matches!(self.files, Removal::Failed(_))
            && !matches!(self.classic, ClassicStep::Failed(_))
    }
}

/// Runs every step even when an earlier one fails, so one locked file or registry error
/// does not leave the rest of the installation behind.
pub fn uninstall_with(env: &InstallEnv, args: &UninstallArgs) -> UninstallReport {
    let autostart_removed =
        reg::delete_value(&env.run_key, RUN_VALUE).map_err(|err| err.to_string());

    // A running host keeps rwm.exe and the hook DLL open, and it detaches cleanly on request.
    let host = host::stop_running_host(&env.host_event, HOST_STOP_TIMEOUT);

    // Covers a hook DLL that outlived its host, for example after the host was killed.
    let windows = (env.explorer_windows)();
    let detach = (!windows.is_empty())
        .then(|| explorer::send_detach(&windows, explorer::detach_message_id()));

    let files = remove_bin_dir(&env.bin_dir);
    let classic = if args.keep_classic {
        ClassicStep::Kept
    } else {
        restore_classic(env)
    };

    UninstallReport {
        autostart_removed,
        host,
        detach,
        files,
        classic,
    }
}

/// Puts the classic menu back the way `classic::enable` found it. The backup that `enable`
/// writes is the proof that rwm made the change. Without it, `classic::disable` would delete
/// a classic-menu setting the user made on their own.
fn restore_classic(env: &InstallEnv) -> ClassicStep {
    match classic::status(&env.classic) {
        Err(err) => ClassicStep::Failed(err.to_string()),
        Ok(status) if !status.has_backup => ClassicStep::NothingToRestore,
        Ok(_) => match disable_classic(&env.classic, false, env.notify_shell) {
            Ok(applied) => ClassicStep::Restored(applied),
            Err(err) => ClassicStep::Failed(err.to_string()),
        },
    }
}

fn remove_bin_dir(bin_dir: &Path) -> Removal {
    let data_dir = bin_dir.parent().unwrap_or(Path::new("."));
    remove_files_with_suffix(data_dir, LEFTOVER_SUFFIX);
    if !bin_dir.exists() {
        return Removal::NotInstalled;
    }
    if fs::remove_dir_all(bin_dir).is_ok() {
        return Removal::Removed;
    }

    // What is left is in use: this very rwm.exe when it runs from the bin folder, or a hook
    // DLL that Explorer has not unloaded yet. Such a file cannot be deleted, but it can be
    // moved, which lets the folder go now and the file on a later run.
    let entries = match fs::read_dir(bin_dir) {
        Ok(entries) => entries,
        Err(err) => return Removal::Failed(format!("cannot read {}: {err}", bin_dir.display())),
    };
    let mut leftovers = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if fs::remove_file(&path).is_ok() {
            continue;
        }
        let moved = renamed_into(data_dir, &path, LEFTOVER_SUFFIX);
        if let Err(err) = fs::rename(&path, &moved) {
            return Removal::Failed(format!(
                "{} is in use and cannot be deleted or moved: {err}",
                path.display()
            ));
        }
        leftovers.push(moved);
    }
    match fs::remove_dir(bin_dir) {
        Ok(()) if leftovers.is_empty() => Removal::Removed,
        Ok(()) => Removal::RemovedWithLeftovers(leftovers),
        Err(err) => Removal::Failed(format!("cannot remove {}: {err}", bin_dir.display())),
    }
}

/// `rwm install`.
pub fn install(args: &InstallArgs) -> CliResult<ExitCode> {
    let env = InstallEnv::live()?;
    let report = install_with(&env, InstallMode::from(args))?;
    print(&render_install(&report));
    Ok(ExitCode::SUCCESS)
}

/// `rwm uninstall`.
pub fn uninstall(args: &UninstallArgs) -> CliResult<ExitCode> {
    let env = InstallEnv::live()?;
    let report = uninstall_with(&env, args);
    let config_path = RwmConfig::config_path();
    let kept_config = config_path.exists().then_some(config_path.as_path());
    print(&render_uninstall(&report, &env.bin_dir, kept_config));
    Ok(if report.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn render_install(report: &InstallReport) -> String {
    let mut text = report.classic.render(true);
    let config = report.config_path.display();
    if report.config_created {
        text.push_str(&format!("Config: created the default at {config}\n"));
    } else {
        text.push_str(&format!("Config: kept {config}\n"));
    }
    for file in &report.files {
        let path = file.path.display();
        text.push_str(&match file.outcome {
            FileOutcome::Copied => format!("Installed {path}\n"),
            FileOutcome::AlreadyInPlace => format!("Already installed: {path}\n"),
            FileOutcome::ReplacedInUse => format!(
                "Installed {path} (the old copy is still mapped in Explorer; restart \
                 explorer.exe before starting the updated hook host)\n"
            ),
        });
    }
    if let Some(command) = &report.autostart {
        text.push_str(&format!("Autostart at sign-in: {command}\n"));
    }

    if let Some(installed_exe) = report.files.first() {
        text.push_str(&format!(
            "\nStart the menu accelerator with: \"{}\" run\n",
            installed_exe.path.display()
        ));
    }
    text
}

fn render_uninstall(
    report: &UninstallReport,
    bin_dir: &Path,
    kept_config: Option<&Path>,
) -> String {
    let mut text = String::new();
    text.push_str(&match &report.autostart_removed {
        Ok(true) => "Autostart entry removed.\n".to_string(),
        Ok(false) => "No autostart entry was set.\n".to_string(),
        Err(err) => format!("FAILED to remove the autostart entry: {err}\n"),
    });
    text.push_str(match report.host {
        StopOutcome::NotRunning => "No hook host was running.\n",
        StopOutcome::Stopped => "The running hook host detached and exited.\n",
        StopOutcome::TimedOut => {
            "FAILED: the running hook host did not exit in time. Run `rwm stop` (or press Ctrl+C \
             in its console) and run `rwm uninstall` again.\n"
        }
    });
    text.push_str(&match report.detach {
        Some(delivery) => format!(
            "Detach message acknowledged by {} of {} Explorer windows.\n",
            delivery.delivered, delivery.windows
        ),
        None => "No Explorer window is open, so no detach message was sent.\n".to_string(),
    });

    let bin = bin_dir.display();
    text.push_str(&match &report.files {
        Removal::NotInstalled => format!("Nothing is installed at {bin}.\n"),
        Removal::Removed => format!("Removed {bin}\n"),
        Removal::RemovedWithLeftovers(leftovers) => {
            let mut lines = format!("Removed {bin}\n");
            for leftover in leftovers {
                lines.push_str(&format!(
                    "  A file was in use and has been moved to {}. Delete it after signing \
                     out, or let the next `rwm install` or `rwm uninstall` remove it.\n",
                    leftover.display()
                ));
            }
            lines
        }
        Removal::Failed(err) => format!("FAILED to remove {bin}: {err}\n"),
    });
    text.push_str(&match &report.classic {
        ClassicStep::Kept => {
            "The classic context menu was left as it is (--keep-classic).\n".to_string()
        }
        ClassicStep::NothingToRestore => {
            "The classic context menu was not switched on by rwm, so it was left as it is. \
             `rwm classic disable` turns it off.\n"
                .to_string()
        }
        ClassicStep::Restored(applied) => applied.render(false),
        ClassicStep::Failed(err) => format!("FAILED to restore the classic menu state: {err}\n"),
    });
    if let Some(config) = kept_config {
        text.push_str(&format!("Your config was kept: {}\n", config.display()));
    }
    text
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_AS_IMAGE_RESOURCE};

    use super::*;
    use crate::testutil::{unique_name, RegistryRoot, TempDir};

    /// A scratch installation: its own registry root, folders, and host event.
    struct Sandbox {
        env: InstallEnv,
        _registry: RegistryRoot,
        _dir: TempDir,
    }

    fn config_already_exists() -> CliResult<(PathBuf, bool)> {
        Ok((PathBuf::from(r"C:\scratch\config.toml"), false))
    }

    fn sandbox(label: &str) -> Sandbox {
        let registry = RegistryRoot::new(label);
        let dir = TempDir::new(label);
        let source = dir.path().join("download");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("rwm-download.exe"), b"exe v1").unwrap();
        fs::write(source.join(HOOK_DLL_FILE), b"dll v1").unwrap();

        let env = InstallEnv {
            classic: ClassicRegistryScope::isolated(
                registry.path(),
                dir.path().join("data").join("classic_backup.json"),
            ),
            notify_shell: false,
            bin_dir: dir.path().join("data").join("bin"),
            run_key: format!(r"{}\Run", registry.path()),
            host_event: format!(r"Local\{}", unique_name(label)),
            exe: source.join("rwm-download.exe"),
            hook_dll: Some(source.join(HOOK_DLL_FILE)),
            ensure_config: config_already_exists,
            explorer_windows: Vec::new,
        };
        Sandbox {
            env,
            _registry: registry,
            _dir: dir,
        }
    }

    impl Sandbox {
        fn classic_enabled(&self) -> bool {
            classic::status(&self.env.classic)
                .expect("read scratch classic state")
                .classic_menu_enabled
        }

        fn autostart(&self) -> Option<String> {
            reg::read_string(&self.env.run_key, RUN_VALUE).expect("read scratch Run key")
        }
    }

    const FULL: InstallMode = InstallMode::Full { autostart: false };
    const WITH_AUTOSTART: InstallMode = InstallMode::Full { autostart: true };
    const REMOVE_ALL: UninstallArgs = UninstallArgs {
        keep_classic: false,
    };

    /// Maps a file as an image, the way Explorer maps the hook DLL: it cannot be deleted or
    /// overwritten while mapped, but it can be renamed. No code from the file runs.
    struct MappedImage(HMODULE);

    impl MappedImage {
        fn map(path: &Path) -> Self {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            let module = unsafe {
                LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_AS_IMAGE_RESOURCE)
            }
            .expect("map image");
            Self(module)
        }
    }

    impl Drop for MappedImage {
        fn drop(&mut self) {
            unsafe {
                let _ = FreeLibrary(self.0);
            }
        }
    }

    fn system_image() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"))
            .join("System32")
            .join("version.dll")
    }

    #[test]
    fn install_flags_map_to_a_mode() {
        let mode = |classic_only, autostart| {
            InstallMode::from(&InstallArgs {
                classic_only,
                autostart,
            })
        };
        assert_eq!(mode(false, false), FULL);
        assert_eq!(mode(false, true), WITH_AUTOSTART);
        assert_eq!(mode(true, false), InstallMode::ClassicOnly);
    }

    #[test]
    fn autostart_command_quotes_the_installed_exe() {
        assert_eq!(
            autostart_command(Path::new(
                r"C:\Users\Jo Doe\AppData\Local\RustWindowsMenu\bin\rwm.exe"
            )),
            r#""C:\Users\Jo Doe\AppData\Local\RustWindowsMenu\bin\rwm.exe" run --hidden"#
        );
    }

    #[test]
    fn install_enables_classic_copies_files_and_registers_autostart() {
        let sb = sandbox("install_full");
        assert!(!sb.classic_enabled());

        let report = install_with(&sb.env, WITH_AUTOSTART).expect("install");

        assert!(report.classic.changed);
        assert!(sb.classic_enabled());
        assert_eq!(report.config_path, PathBuf::from(r"C:\scratch\config.toml"));
        assert!(!report.config_created);

        let installed_exe = sb.env.bin_dir.join("rwm.exe");
        let installed_dll = sb.env.bin_dir.join("rwm_hook.dll");
        assert_eq!(
            report.files,
            vec![
                InstalledFile {
                    path: installed_exe.clone(),
                    outcome: FileOutcome::Copied
                },
                InstalledFile {
                    path: installed_dll.clone(),
                    outcome: FileOutcome::Copied
                },
            ]
        );
        assert_eq!(fs::read(&installed_exe).unwrap(), b"exe v1");
        assert_eq!(fs::read(&installed_dll).unwrap(), b"dll v1");

        let expected_command = format!("\"{}\" run --hidden", installed_exe.display());
        assert_eq!(report.autostart.as_deref(), Some(expected_command.as_str()));
        assert_eq!(sb.autostart(), Some(expected_command));
    }

    #[test]
    fn install_is_repeatable_and_upgrades_the_files() {
        let sb = sandbox("install_twice");
        install_with(&sb.env, FULL).expect("first install");
        fs::write(&sb.env.exe, b"exe v2").unwrap();

        let second = install_with(&sb.env, FULL).expect("second install");

        assert!(!second.classic.changed, "classic menu was already on");
        assert!(sb.classic_enabled());
        assert_eq!(fs::read(sb.env.bin_dir.join("rwm.exe")).unwrap(), b"exe v2");
        assert_eq!(second.autostart, None);
        assert_eq!(sb.autostart(), None, "autostart is only written on request");
    }

    #[test]
    fn classic_only_install_copies_nothing_even_when_the_files_are_there() {
        let sb = sandbox("install_classic_only");
        assert!(sb.env.hook_dll.is_some());

        let report = install_with(&sb.env, InstallMode::ClassicOnly).expect("classic-only install");

        assert!(sb.classic_enabled());
        assert_eq!(report.files, Vec::new());
        assert!(!sb.env.bin_dir.exists());
        assert_eq!(sb.autostart(), None);
    }

    #[test]
    fn classic_only_install_needs_no_hook_dll() {
        let mut sb = sandbox("install_classic_only_no_dll");
        sb.env.hook_dll = None;

        install_with(&sb.env, InstallMode::ClassicOnly).expect("classic-only install");

        assert!(sb.classic_enabled());
    }

    #[test]
    fn missing_hook_dll_stops_the_install_before_anything_changes() {
        let mut sb = sandbox("install_no_dll");
        sb.env.hook_dll = None;

        let err = install_with(&sb.env, WITH_AUTOSTART).unwrap_err();

        assert!(err.to_string().contains("rwm_hook.dll was not found"));
        assert!(!sb.classic_enabled());
        assert!(!sb.env.classic.backup_path.exists());
        assert!(!sb.env.bin_dir.exists());
        assert_eq!(sb.autostart(), None);
    }

    #[test]
    fn installing_from_the_bin_folder_leaves_the_files_in_place() {
        let mut sb = sandbox("install_in_place");
        install_with(&sb.env, FULL).expect("first install");
        sb.env.exe = sb.env.bin_dir.join("rwm.exe");
        sb.env.hook_dll = Some(sb.env.bin_dir.join("rwm_hook.dll"));

        let report = install_with(&sb.env, FULL).expect("install from the installed copy");

        assert!(report
            .files
            .iter()
            .all(|file| file.outcome == FileOutcome::AlreadyInPlace));
        assert_eq!(fs::read(sb.env.bin_dir.join("rwm.exe")).unwrap(), b"exe v1");
    }

    #[test]
    fn an_in_use_file_is_renamed_aside_and_replaced() {
        let sb = sandbox("install_in_use");
        fs::create_dir_all(&sb.env.bin_dir).unwrap();
        let installed_dll = sb.env.bin_dir.join("rwm_hook.dll");
        fs::copy(system_image(), &installed_dll).unwrap();
        let mapped = MappedImage::map(&installed_dll);

        let report = install_with(&sb.env, FULL).expect("install over a mapped DLL");

        assert_eq!(report.files[0].outcome, FileOutcome::Copied);
        assert_eq!(report.files[1].outcome, FileOutcome::ReplacedInUse);
        assert_eq!(fs::read(&installed_dll).unwrap(), b"dll v1");
        let aside = sibling_with_suffix(&installed_dll, SET_ASIDE_SUFFIX);
        assert!(
            aside.exists(),
            "the mapped copy stays available to its user"
        );

        drop(mapped);
        install_with(&sb.env, FULL).expect("next install");
        assert!(
            !aside.exists(),
            "the set-aside copy is cleaned up once it is free"
        );
    }

    #[test]
    fn uninstall_reverses_a_full_install() {
        let sb = sandbox("uninstall_full");
        install_with(&sb.env, WITH_AUTOSTART).expect("install");

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        assert!(report.succeeded());
        assert_eq!(report.autostart_removed, Ok(true));
        assert_eq!(report.host, StopOutcome::NotRunning);
        assert_eq!(report.detach, None);
        assert_eq!(report.files, Removal::Removed);
        assert!(matches!(
            &report.classic,
            ClassicStep::Restored(applied) if applied.changed && !applied.enabled
        ));

        assert_eq!(sb.autostart(), None);
        assert!(!sb.env.bin_dir.exists());
        assert!(!sb.classic_enabled());
        assert!(!sb.env.classic.backup_path.exists());
    }

    #[test]
    fn uninstall_is_repeatable_when_nothing_is_installed() {
        let sb = sandbox("uninstall_twice");

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        assert!(report.succeeded());
        assert_eq!(report.autostart_removed, Ok(false));
        assert_eq!(report.files, Removal::NotInstalled);
        assert!(matches!(report.classic, ClassicStep::NothingToRestore));
    }

    /// Switches the classic menu on the way a user's own registry tweak does: the key is
    /// there, but rwm holds no backup for it.
    fn enable_classic_outside_rwm(sb: &Sandbox) {
        classic::enable(&sb.env.classic, false, false, false).expect("enable scratch classic menu");
        fs::remove_file(&sb.env.classic.backup_path).expect("drop the backup");
        assert!(sb.classic_enabled());
    }

    #[test]
    fn uninstall_leaves_a_classic_menu_that_rwm_did_not_switch_on() {
        let sb = sandbox("uninstall_foreign_classic");
        enable_classic_outside_rwm(&sb);

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        assert!(matches!(report.classic, ClassicStep::NothingToRestore));
        assert!(report.succeeded());
        assert!(sb.classic_enabled(), "the user's own setting survives");
    }

    #[test]
    fn uninstall_restores_a_classic_menu_that_was_on_before_install() {
        let sb = sandbox("uninstall_prior_classic");
        enable_classic_outside_rwm(&sb);
        install_with(&sb.env, FULL).expect("install");

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        assert!(matches!(
            &report.classic,
            ClassicStep::Restored(applied) if applied.enabled
        ));
        assert!(
            sb.classic_enabled(),
            "uninstall returns to the state before install"
        );
        assert!(!sb.env.classic.backup_path.exists());
    }

    #[test]
    fn keep_classic_leaves_the_classic_menu_on() {
        let sb = sandbox("uninstall_keep");
        install_with(&sb.env, FULL).expect("install");

        let report = uninstall_with(&sb.env, &UninstallArgs { keep_classic: true });

        assert!(report.succeeded());
        assert!(matches!(report.classic, ClassicStep::Kept));
        assert!(sb.classic_enabled());
        assert!(!sb.env.bin_dir.exists());
    }

    #[test]
    fn uninstall_moves_a_still_mapped_file_out_of_the_bin_folder() {
        let sb = sandbox("uninstall_in_use");
        install_with(&sb.env, FULL).expect("install");
        let installed_dll = sb.env.bin_dir.join("rwm_hook.dll");
        fs::copy(system_image(), &installed_dll).unwrap();
        let mapped = MappedImage::map(&installed_dll);

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        let moved = renamed_into(
            sb.env.bin_dir.parent().unwrap(),
            &installed_dll,
            LEFTOVER_SUFFIX,
        );
        assert_eq!(
            report.files,
            Removal::RemovedWithLeftovers(vec![moved.clone()])
        );
        assert!(report.succeeded());
        assert!(!sb.env.bin_dir.exists());
        assert!(moved.exists());

        drop(mapped);
        assert_eq!(
            uninstall_with(&sb.env, &REMOVE_ALL).files,
            Removal::NotInstalled
        );
        assert!(!moved.exists(), "the leftover is deleted once it is free");
    }

    #[test]
    fn uninstall_asks_a_running_host_to_stop_first() {
        let sb = sandbox("uninstall_host");
        install_with(&sb.env, FULL).expect("install");

        let (claimed, wait_claimed) = std::sync::mpsc::channel();
        let event = sb.env.host_event.clone();
        let host = std::thread::spawn(move || {
            let signal = host::HostSignal::claim(&event).expect("claim scratch host event");
            claimed.send(()).unwrap();
            signal.wait_for_stop(Duration::from_secs(10))
        });
        wait_claimed.recv().unwrap();

        let report = uninstall_with(&sb.env, &REMOVE_ALL);

        assert_eq!(report.host, StopOutcome::Stopped);
        assert!(host.join().unwrap(), "the host saw the stop request");
        assert!(report.succeeded());
    }

    #[test]
    fn reports_read_like_a_summary_of_what_happened() {
        let sb = sandbox("render");
        let installed = install_with(&sb.env, WITH_AUTOSTART).expect("install");
        let text = render_install(&installed);
        assert!(text.starts_with("Classic context menu: on\nMenuShowDelay: 0 ms\n"));
        assert!(text.contains(r"Config: kept C:\scratch\config.toml"));
        assert!(text.contains(&format!(
            "Installed {}",
            sb.env.bin_dir.join("rwm.exe").display()
        )));
        assert!(text.contains("Autostart at sign-in: \""));
        assert!(text.contains("Start the menu accelerator with: \""));

        let removed = uninstall_with(&sb.env, &REMOVE_ALL);
        let kept_config = Path::new(r"C:\scratch\config.toml");
        let text = render_uninstall(&removed, &sb.env.bin_dir, Some(kept_config));
        assert!(text.contains("Autostart entry removed."));
        assert!(text.contains("No hook host was running."));
        assert!(text.contains("No Explorer window is open"));
        assert!(text.contains(&format!("Removed {}", sb.env.bin_dir.display())));
        assert!(text.contains("Classic context menu: off\n"));
        assert!(text.contains(r"Your config was kept: C:\scratch\config.toml"));
        assert!(!text.contains("FAILED"));
        assert!(!render_uninstall(&removed, &sb.env.bin_dir, None).contains("Your config"));
    }
}
