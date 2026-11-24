use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rwm_core::build_info::{versioned_dll_filename, ABI_VERSION, BUILD_ID};
use rwm_core::{classic, handlers};
use serde::{Deserialize, Serialize};

use crate::cli::{InstallArgs, UninstallArgs};
use crate::error::{CliError, CliResult, Context};
use crate::hookdll;
use crate::host;
use crate::output::print_line;
use crate::reg;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "RustWindowsMenu";
pub const ENVIRONMENT_KEY: &str = "Environment";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentMetadata {
    pub build_id: String,
    pub abi_version: u32,
    pub hook_dll: PathBuf,
}

pub struct InstallEnv {
    pub root: PathBuf,
    pub source_exe: PathBuf,
    pub source_host: PathBuf,
    pub source_dll: Option<PathBuf>,
    pub run_key: String,
    pub environment_key: String,
    pub validate_dll: fn(&Path) -> CliResult<()>,
    pub stop_hosts: fn() -> CliResult<()>,
    pub start_host: fn(&Path) -> CliResult<()>,
    pub restore_classic: fn() -> CliResult<bool>,
    pub restore_handlers: fn() -> CliResult<usize>,
    pub broadcast_environment: bool,
    pub ensure_config: fn() -> CliResult<()>,
}

fn validate_live_dll(path: &Path) -> CliResult<()> {
    let probe = hookdll::probe(path);
    if probe.is_usable() {
        Ok(())
    } else {
        Err(CliError::new(format!(
            "source hook DLL validation failed: {}",
            probe.problems.join("; ")
        )))
    }
}
fn stop_live_hosts() -> CliResult<()> {
    host::stop().map(|_| ())
}
fn restore_live_classic() -> CliResult<bool> {
    if classic::status()?.has_backup {
        classic::disable(false)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
fn restore_live_handlers() -> CliResult<usize> {
    Ok(handlers::restore_rwm_blocks()?)
}
fn ensure_live_config() -> CliResult<()> {
    if !rwm_core::config::config_path().exists() {
        rwm_core::config::RwmConfig::default().save()?;
    }
    Ok(())
}

impl InstallEnv {
    pub fn live() -> CliResult<Self> {
        let exe = std::env::current_exe().context(|| "cannot locate rwm.exe".to_string())?;
        let root = std::env::var_os("RWM_INSTALL_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(rwm_core::config::local_data_dir);
        Ok(Self {
            root,
            source_host: exe.with_file_name("rwm-host.exe"),
            source_dll: hookdll::locate(&exe),
            source_exe: exe,
            run_key: RUN_KEY.into(),
            environment_key: ENVIRONMENT_KEY.into(),
            validate_dll: validate_live_dll,
            stop_hosts: stop_live_hosts,
            start_host: host::start_host_exe,
            restore_classic: restore_live_classic,
            restore_handlers: restore_live_handlers,
            broadcast_environment: true,
            ensure_config: ensure_live_config,
        })
    }
    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
}

#[derive(Debug)]
pub struct InstallReport {
    pub root: PathBuf,
    pub started: bool,
    pub startup_registered: bool,
    pub path_updated: bool,
    pub cleanup_pending: Vec<PathBuf>,
}
#[derive(Debug)]
pub struct UninstallReport {
    pub root: PathBuf,
    pub classic_restored: bool,
    pub handlers_restored: usize,
    pub cleanup_pending: Vec<PathBuf>,
}

fn is_reparse_or_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

fn require_file(path: &Path, label: &str) -> CliResult<()> {
    if path.is_file() && fs::metadata(path)?.len() > 0 {
        Ok(())
    } else {
        Err(CliError::new(format!(
            "{label} is missing or empty: {}",
            path.display()
        )))
    }
}

fn unique_pending(root: &Path, path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    root.join("cleanup_pending")
        .join(format!("{name}.{}", std::process::id()))
}

struct Replacement {
    destination: PathBuf,
    backup: Option<PathBuf>,
}

struct StagedGuard(Vec<PathBuf>);
impl StagedGuard {
    fn disarm(&mut self) {
        self.0.clear();
    }
}
impl Drop for StagedGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn strip_motw(path: &Path) {
    let mut zone = path.as_os_str().to_os_string();
    zone.push(":Zone.Identifier");
    let _ = fs::remove_file(PathBuf::from(zone));
}

fn stage(source: &Path, destination: &Path) -> CliResult<PathBuf> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged = destination.with_extension(format!("new.{}", std::process::id()));
    fs::copy(source, &staged)
        .context(|| format!("cannot stage {} to {}", source.display(), staged.display()))?;
    strip_motw(&staged);
    Ok(staged)
}

fn commit(staged: &Path, destination: &Path) -> CliResult<Replacement> {
    let backup = if destination.exists() {
        let backup = destination.with_extension(format!("rollback.{}", std::process::id()));
        fs::rename(destination, &backup)
            .context(|| format!("cannot preserve {}", destination.display()))?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = fs::rename(staged, destination) {
        if let Some(backup) = &backup {
            let _ = fs::rename(backup, destination);
        }
        return Err(CliError::new(format!(
            "cannot install {}: {error}",
            destination.display()
        )));
    }
    Ok(Replacement {
        destination: destination.to_path_buf(),
        backup,
    })
}

fn rollback(replacements: Vec<Replacement>) -> Vec<String> {
    let mut errors = Vec::new();
    for replacement in replacements.into_iter().rev() {
        if let Err(error) = fs::remove_file(&replacement.destination) {
            if error.kind() != io::ErrorKind::NotFound {
                errors.push(format!(
                    "remove {}: {error}",
                    replacement.destination.display()
                ));
            }
        }
        if let Some(backup) = replacement.backup {
            if let Err(error) = fs::rename(&backup, &replacement.destination) {
                errors.push(format!(
                    "restore {} from {}: {error}",
                    replacement.destination.display(),
                    backup.display()
                ));
            }
        }
    }
    errors
}

fn finish(replacements: Vec<Replacement>, root: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut pending = Vec::new();
    let mut errors = Vec::new();
    for replacement in replacements {
        if let Some(backup) = replacement.backup {
            move_or_delete_file(root, &backup, &mut pending, &mut errors);
        }
    }
    (pending, errors)
}

fn move_or_delete_file(
    root: &Path,
    path: &Path,
    pending: &mut Vec<PathBuf>,
    errors: &mut Vec<String>,
) {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            errors.push(format!("{}: {error}", path.display()));
            return;
        }
    };
    if is_reparse_or_link(&meta) || meta.is_dir() {
        return;
    }
    if let Err(remove_error) = fs::remove_file(path) {
        let destination = unique_pending(root, path);
        match fs::create_dir_all(destination.parent().unwrap())
            .and_then(|()| fs::rename(path, &destination))
        {
            Ok(()) => pending.push(destination),
            Err(move_error) => errors.push(format!(
                "{}: delete failed ({remove_error}); move to cleanup_pending failed ({move_error})",
                path.display()
            )),
        }
    }
}

fn remove_empty_dir_non_reparse(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !is_reparse_or_link(&meta) && meta.is_dir() {
        let _ = fs::remove_dir(dir);
    }
}

fn is_rwm_version_file(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("rwm_hook") && (name.ends_with(".dll") || name.contains(".dll."))
}

fn sweep_cleanup_pending(root: &Path) {
    let dir = root.join("cleanup_pending");
    let Ok(meta) = fs::symlink_metadata(&dir) else {
        return;
    };
    if is_reparse_or_link(&meta) || !meta.is_dir() {
        return;
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(item_meta) = fs::symlink_metadata(&path) {
                if !is_reparse_or_link(&item_meta) && item_meta.is_file() {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
    remove_empty_dir_non_reparse(&dir);
}

fn cleanup_old_versions(env: &InstallEnv) -> (Vec<PathBuf>, Vec<String>) {
    let versions = env.root.join("versions");
    let mut pending = Vec::new();
    let mut errors = Vec::new();
    let Ok(versions_meta) = fs::symlink_metadata(&versions) else {
        return (pending, errors);
    };
    if is_reparse_or_link(&versions_meta) || !versions_meta.is_dir() {
        return (pending, errors);
    }
    let Ok(entries) = fs::read_dir(&versions) else {
        return (pending, errors);
    };
    for entry in entries.flatten() {
        if entry.file_name() == BUILD_ID {
            continue;
        }
        let dir = entry.path();
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if is_reparse_or_link(&meta) || !meta.is_dir() {
            continue;
        }
        if let Ok(files) = fs::read_dir(&dir) {
            for file in files.flatten() {
                if is_rwm_version_file(&file.file_name()) {
                    move_or_delete_file(&env.root, &file.path(), &mut pending, &mut errors);
                }
            }
        }
        remove_empty_dir_non_reparse(&dir);
    }
    (pending, errors)
}

fn rollback_registry_updates(
    env: &InstallEnv,
    startup_changed: bool,
    prev_run: Option<&str>,
    path_changed: bool,
) {
    if startup_changed {
        let _ = match prev_run {
            Some(prev) => reg::write_string(&env.run_key, RUN_VALUE, prev),
            None => reg::delete_value(&env.run_key, RUN_VALUE).map(|_| ()),
        };
    }
    if path_changed
        && reg::update_user_path_in_key(&env.environment_key, &env.bin(), true).unwrap_or(false)
        && env.broadcast_environment
    {
        broadcast_environment();
    }
}

pub fn install_with(env: &InstallEnv, args: &InstallArgs) -> CliResult<InstallReport> {
    require_file(&env.source_exe, "rwm.exe")?;
    require_file(&env.source_host, "rwm-host.exe")?;
    let dll = env
        .source_dll
        .as_deref()
        .ok_or_else(|| CliError::new("matching hook DLL was not found"))?;
    require_file(dll, "hook DLL")?;
    (env.validate_dll)(dll)?;

    let bin = env.bin();
    let version_dir = env.root.join("versions").join(BUILD_ID);
    let mut staged_guard = StagedGuard(Vec::new());
    let staged_exe = stage(&env.source_exe, &bin.join("rwm.exe"))?;
    staged_guard.0.push(staged_exe.clone());
    let staged_host = stage(&env.source_host, &bin.join("rwm-host.exe"))?;
    staged_guard.0.push(staged_host.clone());
    let staged_dll = stage(dll, &version_dir.join(versioned_dll_filename()))?;
    staged_guard.0.push(staged_dll.clone());
    let metadata = serde_json::to_vec_pretty(&CurrentMetadata {
        build_id: BUILD_ID.into(),
        abi_version: ABI_VERSION,
        hook_dll: version_dir.join(versioned_dll_filename()),
    })?;
    let current = env.root.join("current.json");
    let staged_current = current.with_extension(format!("new.{}", std::process::id()));
    fs::write(&staged_current, metadata)?;
    staged_guard.0.push(staged_current.clone());

    let had_previous_host = bin.join("rwm-host.exe").is_file();
    (env.stop_hosts)()?;
    let mut replacements = Vec::new();
    for (staged, destination) in [
        (&staged_dll, version_dir.join(versioned_dll_filename())),
        (&staged_exe, bin.join("rwm.exe")),
        (&staged_host, bin.join("rwm-host.exe")),
        (&staged_current, current.clone()),
    ] {
        match commit(staged, &destination) {
            Ok(replacement) => replacements.push(replacement),
            Err(error) => {
                let rollback_errs = rollback(replacements);
                let prev_note = if had_previous_host && !args.no_start {
                    match (env.start_host)(&bin.join("rwm-host.exe")) {
                        Ok(()) => "; previous host restarted",
                        Err(_) => "; previous host could not be restarted",
                    }
                } else {
                    ""
                };
                return Err(CliError::new(if rollback_errs.is_empty() {
                    format!("{error}{prev_note}")
                } else {
                    format!(
                        "{error}{prev_note}; rollback warnings: {}",
                        rollback_errs.join("; ")
                    )
                }));
            }
        }
    }
    staged_guard.disarm();

    let fail_with_rollback = |cause: String,
                              replacements: Vec<Replacement>,
                              startup_changed: bool,
                              prev_run: Option<&str>,
                              path_changed: bool| {
        rollback_registry_updates(env, startup_changed, prev_run, path_changed);
        let rollback_errs = rollback(replacements);
        let restart_note = if had_previous_host && !args.no_start {
            match (env.start_host)(&bin.join("rwm-host.exe")) {
                Ok(()) => " (previous installation and host were restored)",
                Err(_) => " (previous files restored, but previous host could not be restarted)",
            }
        } else {
            " (previous installation was restored)"
        };
        let suffix = if rollback_errs.is_empty() {
            String::new()
        } else {
            format!("; rollback warnings: {}", rollback_errs.join("; "))
        };
        Err(CliError::new(format!("{cause}{restart_note}{suffix}")))
    };

    if args.classic {
        if let Err(error) = classic::enable() {
            return fail_with_rollback(
                format!("failed to enable classic menu override: {error}"),
                replacements,
                false,
                None,
                false,
            );
        }
    }
    if let Err(error) = (env.ensure_config)() {
        return fail_with_rollback(
            format!("failed to initialize configuration: {error}"),
            replacements,
            false,
            None,
            false,
        );
    }

    let prev_run = if args.no_startup {
        None
    } else {
        reg::read_string(&env.run_key, RUN_VALUE).ok().flatten()
    };
    let startup_registered = if args.no_startup {
        false
    } else {
        let value = format!("\"{}\"", bin.join("rwm-host.exe").display());
        if let Err(error) = reg::write_string(&env.run_key, RUN_VALUE, &value) {
            return fail_with_rollback(
                format!("failed to register startup entry: {error}"),
                replacements,
                false,
                prev_run.as_deref(),
                false,
            );
        }
        true
    };
    let path_updated = if args.no_path {
        false
    } else {
        match reg::update_user_path_in_key(&env.environment_key, &bin, false) {
            Ok(changed) => {
                if changed && env.broadcast_environment {
                    broadcast_environment();
                }
                changed
            }
            Err(error) => {
                return fail_with_rollback(
                    format!("failed to update user PATH: {error}"),
                    replacements,
                    startup_registered,
                    prev_run.as_deref(),
                    false,
                );
            }
        }
    };
    if !args.no_start {
        if let Err(error) = (env.start_host)(&bin.join("rwm-host.exe")) {
            return fail_with_rollback(
                format!("installed host failed to start: {error}"),
                replacements,
                startup_registered,
                prev_run.as_deref(),
                path_updated,
            );
        }
    }

    let (mut pending, mut cleanup_errors) = finish(replacements, &env.root);
    let legacy_dll = bin.join("rwm_hook.dll");
    if legacy_dll.exists() {
        move_or_delete_file(&env.root, &legacy_dll, &mut pending, &mut cleanup_errors);
    }
    let (old_pending, old_errors) = cleanup_old_versions(env);
    pending.extend(old_pending);
    cleanup_errors.extend(old_errors);
    sweep_cleanup_pending(&env.root);
    print_line(format!(
        "Installed RustWindowsMenu {BUILD_ID} at {}",
        env.root.display()
    ));
    if !cleanup_errors.is_empty() {
        print_line(format!("Cleanup warning(s): {}", cleanup_errors.join("; ")));
    }
    Ok(InstallReport {
        root: env.root.clone(),
        started: !args.no_start,
        startup_registered,
        path_updated,
        cleanup_pending: pending,
    })
}

fn verify_install_root(root: &Path) -> CliResult<CurrentMetadata> {
    let meta_path = root.join("current.json");
    let meta = fs::symlink_metadata(&meta_path).map_err(|_| {
        CliError::new(format!(
            "{} is not a verified RustWindowsMenu install (missing current.json); refusing to delete files",
            root.display()
        ))
    })?;
    if is_reparse_or_link(&meta) || !meta.is_file() {
        return Err(CliError::new(format!(
            "{} is not a regular install manifest file",
            meta_path.display()
        )));
    }
    let bytes = fs::read(&meta_path)
        .context(|| format!("cannot read install manifest {}", meta_path.display()))?;
    serde_json::from_slice(&bytes)
        .context(|| format!("invalid install manifest {}", meta_path.display()))
}

fn remove_owned_install_files(root: &Path, metadata: &CurrentMetadata) -> (Vec<PathBuf>, Vec<String>) {
    let mut pending = Vec::new();
    let mut errors = Vec::new();
    let bin = root.join("bin");
    for file in [
        bin.join("rwm.exe"),
        bin.join("rwm-host.exe"),
        bin.join("rwm_hook.dll"),
        bin.join(versioned_dll_filename()),
        root.join("current.json"),
        metadata.hook_dll.clone(),
    ] {
        move_or_delete_file(root, &file, &mut pending, &mut errors);
    }
    if let Ok(meta) = fs::symlink_metadata(&bin) {
        if !is_reparse_or_link(&meta) && meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&bin) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("rwm.exe.")
                        || name.starts_with("rwm-host.exe.")
                        || is_rwm_version_file(entry.file_name().as_os_str())
                    {
                        move_or_delete_file(root, &entry.path(), &mut pending, &mut errors);
                    }
                }
            }
            remove_empty_dir_non_reparse(&bin);
        }
    }
    let versions = root.join("versions");
    if let Ok(meta) = fs::symlink_metadata(&versions) {
        if !is_reparse_or_link(&meta) && meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&versions) {
                for entry in entries.flatten() {
                    let dir = entry.path();
                    let Ok(dir_meta) = fs::symlink_metadata(&dir) else {
                        continue;
                    };
                    if is_reparse_or_link(&dir_meta) || !dir_meta.is_dir() {
                        continue;
                    }
                    if let Ok(files) = fs::read_dir(&dir) {
                        for file in files.flatten() {
                            if is_rwm_version_file(&file.file_name()) {
                                move_or_delete_file(root, &file.path(), &mut pending, &mut errors);
                            }
                        }
                    }
                    remove_empty_dir_non_reparse(&dir);
                }
            }
            remove_empty_dir_non_reparse(&versions);
        }
    }
    sweep_cleanup_pending(root);
    remove_empty_dir_non_reparse(root);
    (pending, errors)
}

pub fn uninstall_with(env: &InstallEnv, args: &UninstallArgs) -> CliResult<UninstallReport> {
    let manifest = verify_install_root(&env.root)?;
    (env.stop_hosts)()?;
    reg::delete_value(&env.run_key, RUN_VALUE)?;
    if reg::update_user_path_in_key(&env.environment_key, &env.bin(), true)?
        && env.broadcast_environment
    {
        broadcast_environment();
    }
    let mut warnings = Vec::new();
    let classic_restored = match (env.restore_classic)() {
        Ok(restored) => restored,
        Err(error) => {
            warnings.push(format!("classic menu restore failed: {error}"));
            false
        }
    };
    let handlers_restored = match (env.restore_handlers)() {
        Ok(count) => count,
        Err(error) => {
            warnings.push(format!("handler block restore failed: {error}"));
            0
        }
    };
    let (pending, mut cleanup_errors) = remove_owned_install_files(&env.root, &manifest);
    warnings.append(&mut cleanup_errors);
    if args.purge {
        let config = rwm_core::config::config_dir();
        if let Ok(meta) = fs::symlink_metadata(&config) {
            if !is_reparse_or_link(&meta) && meta.is_dir() {
                if let Err(error) = fs::remove_dir_all(&config) {
                    warnings.push(format!("cannot purge {}: {error}", config.display()));
                }
            }
        }
    }
    print_line(format!(
        "Uninstalled {}. Restored classic state: {classic_restored}; handlers: {handlers_restored}.",
        env.root.display()
    ));
    if !pending.is_empty() {
        print_line(format!(
            "{} mapped file(s) remain in cleanup_pending; delete them after sign-out.",
            pending.len()
        ));
    }
    if !warnings.is_empty() {
        return Err(CliError::new(warnings.join("; ")));
    }
    Ok(UninstallReport {
        root: env.root.clone(),
        classic_restored,
        handlers_restored,
        cleanup_pending: pending,
    })
}

fn broadcast_environment() {
    use windows::core::w;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };
    let mut result = 0usize;
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").0 as isize),
            SMTO_ABORTIFHUNG,
            5000,
            Some(&mut result),
        );
    }
}

pub fn install(args: &InstallArgs) -> CliResult<ExitCode> {
    host::ensure_matching_elevation("install")?;
    let report = install_with(&InstallEnv::live()?, args)?;
    print_line(format!(
        "Root: {}; started={}; startup={}; Path updated={}; pending cleanup={}",
        report.root.display(),
        report.started,
        report.startup_registered,
        report.path_updated,
        report.cleanup_pending.len()
    ));
    Ok(ExitCode::SUCCESS)
}
pub fn uninstall(args: &UninstallArgs) -> CliResult<ExitCode> {
    let report = uninstall_with(&InstallEnv::live()?, args)?;
    print_line(format!(
        "Root: {}; classic restored={}; handlers restored={}; pending cleanup={}",
        report.root.display(),
        report.classic_restored,
        report.handlers_restored,
        report.cleanup_pending.len()
    ));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{RegistryRoot, TempDir};

    fn accept(_: &Path) -> CliResult<()> {
        Ok(())
    }
    fn noop() -> CliResult<()> {
        Ok(())
    }
    fn no_config() -> CliResult<()> {
        Ok(())
    }
    fn start_ok(_: &Path) -> CliResult<()> {
        Ok(())
    }
    fn start_fail(_: &Path) -> CliResult<()> {
        Err(CliError::new("injected start failure"))
    }
    fn restored_classic() -> CliResult<bool> {
        Ok(true)
    }
    fn restored_handlers() -> CliResult<usize> {
        Ok(2)
    }

    struct Sandbox {
        env: InstallEnv,
        _reg: RegistryRoot,
        _dir: TempDir,
    }
    fn sandbox(label: &str, start: fn(&Path) -> CliResult<()>) -> Sandbox {
        let dir = TempDir::new(label);
        let registry = RegistryRoot::new(label);
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("rwm.exe"), b"new rwm").unwrap();
        fs::write(source.join("rwm-host.exe"), b"new host").unwrap();
        fs::write(source.join("rwm_hook.dll"), b"new dll").unwrap();
        let env = InstallEnv {
            root: dir.path().join("install"),
            source_exe: source.join("rwm.exe"),
            source_host: source.join("rwm-host.exe"),
            source_dll: Some(source.join("rwm_hook.dll")),
            run_key: format!(r"{}\Run", registry.path()),
            environment_key: format!(r"{}\Environment", registry.path()),
            validate_dll: accept,
            stop_hosts: noop,
            start_host: start,
            restore_classic: restored_classic,
            restore_handlers: restored_handlers,
            broadcast_environment: false,
            ensure_config: no_config,
        };
        Sandbox {
            env,
            _reg: registry,
            _dir: dir,
        }
    }
    fn flags() -> InstallArgs {
        InstallArgs {
            classic: false,
            no_startup: false,
            no_path: false,
            no_start: false,
        }
    }

    #[test]
    fn install_uninstall_roundtrip_is_idempotent_in_scratch_locations() {
        let sb = sandbox("roundtrip", start_ok);
        install_with(&sb.env, &flags()).unwrap();
        install_with(&sb.env, &flags()).unwrap();
        assert!(sb.env.root.join("current.json").is_file());
        assert!(reg::read_string(&sb.env.run_key, RUN_VALUE)
            .unwrap()
            .is_some());
        assert!(reg::read_string(&sb.env.environment_key, "Path")
            .unwrap()
            .unwrap()
            .contains("bin"));
        let report = uninstall_with(&sb.env, &UninstallArgs { purge: false }).unwrap();
        assert!(report.classic_restored);
        assert_eq!(report.handlers_restored, 2);
        assert!(!sb.env.bin().exists());
        assert!(reg::read_string(&sb.env.run_key, RUN_VALUE)
            .unwrap()
            .is_none());
        assert!(!reg::read_string(&sb.env.environment_key, "Path")
            .unwrap()
            .unwrap()
            .contains("bin"));
    }

    #[test]
    fn failed_host_start_rolls_back_existing_binaries_and_metadata() {
        let mut sb = sandbox("rollback", start_ok);
        install_with(&sb.env, &flags()).unwrap();
        fs::write(sb.env.root.join("current.json"), b"sentinel").unwrap();
        fs::write(sb.env.bin().join("rwm.exe"), b"old rwm").unwrap();
        fs::write(&sb.env.source_exe, b"newer rwm").unwrap();
        sb.env.start_host = start_fail;
        let error = install_with(&sb.env, &flags()).unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert_eq!(fs::read(sb.env.bin().join("rwm.exe")).unwrap(), b"old rwm");
        assert_eq!(
            fs::read(sb.env.root.join("current.json")).unwrap(),
            b"sentinel"
        );
    }

    #[test]
    fn uninstall_moves_mapped_dlls_to_cleanup_pending() {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::FreeLibrary;
        use windows::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        };

        let sb = sandbox("mapped", start_ok);
        install_with(&sb.env, &flags()).unwrap();
        let dll = sb
            .env
            .root
            .join("versions")
            .join(BUILD_ID)
            .join(versioned_dll_filename());
        let system =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\version.dll");
        fs::copy(system, &dll).unwrap();
        let wide = dll.as_os_str().encode_wide().chain([0]).collect::<Vec<_>>();
        let mapped =
            unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_AS_IMAGE_RESOURCE) }
                .unwrap();
        let report = uninstall_with(&sb.env, &UninstallArgs { purge: false }).unwrap();
        assert_eq!(report.cleanup_pending.len(), 1);
        assert!(report.cleanup_pending[0].is_file());
        assert!(report.cleanup_pending[0].starts_with(sb.env.root.join("cleanup_pending")));
        unsafe {
            FreeLibrary(mapped).unwrap();
        }
    }

    #[test]
    fn missing_or_rejected_source_changes_nothing() {
        let sb = sandbox("reject", start_fail);
        fs::remove_file(&sb.env.source_host).unwrap();
        assert!(install_with(&sb.env, &flags()).is_err());
        assert!(!sb.env.root.exists());
        assert!(reg::read_string(&sb.env.run_key, RUN_VALUE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn uninstall_requires_manifest_and_preserves_unrelated_files() {
        let sb = sandbox("unverified", start_ok);
        fs::create_dir_all(&sb.env.root).unwrap();
        let unrelated = sb.env.root.join("precious.txt");
        fs::write(&unrelated, b"keep me").unwrap();
        let err = uninstall_with(&sb.env, &UninstallArgs { purge: false }).unwrap_err();
        assert!(err.to_string().contains("current.json"), "{err}");
        assert!(unrelated.is_file());
    }

    #[test]
    fn uninstall_does_not_descend_into_junctions_or_delete_unrelated_user_files() {
        use std::os::windows::process::CommandExt;
        let sb = sandbox("junction", start_ok);
        install_with(&sb.env, &flags()).unwrap();
        let external = sb._dir.path().join("external_precious");
        fs::create_dir_all(&external).unwrap();
        let precious = external.join("precious.txt");
        fs::write(&precious, b"do not delete").unwrap();
        let user_note = sb.env.root.join("user-note.txt");
        fs::write(&user_note, b"keep me too").unwrap();
        let link = sb.env.root.join("versions").join("linked_dir");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &external.to_string_lossy(),
            ])
            .creation_flags(0x0800_0000)
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");

        uninstall_with(&sb.env, &UninstallArgs { purge: false }).unwrap();
        assert!(precious.is_file(), "junction target file was modified");
        assert!(user_note.is_file(), "unrelated root file was deleted");
        let _ = fs::remove_dir(&link);
    }

    #[test]
    fn upgrade_removes_legacy_v01_bin_dll_and_uses_versioned_layout() {
        let sb = sandbox("v01upgrade", start_ok);
        fs::create_dir_all(sb.env.bin()).unwrap();
        let legacy_dll = sb.env.bin().join("rwm_hook.dll");
        fs::write(&legacy_dll, b"old v0.1 hook dll").unwrap();
        install_with(&sb.env, &flags()).unwrap();
        assert!(!legacy_dll.exists(), "stale v0.1 bin\\rwm_hook.dll must be cleaned up");
        assert!(sb
            .env
            .root
            .join("versions")
            .join(BUILD_ID)
            .join(versioned_dll_filename())
            .is_file());
    }
}
