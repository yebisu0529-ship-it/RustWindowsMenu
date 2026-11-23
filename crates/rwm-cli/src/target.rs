use std::path::{Path, PathBuf};

use rwm_core::model::SelectionTarget;
use serde::Serialize;

use crate::cli::TargetArgs;
use crate::error::{CliError, CliResult, Context};

/// Why this path was chosen. A defaulted target is called out in the output so the reader
/// knows what was actually measured or shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetOrigin {
    Explicit,
    /// No path was given, so this executable stands in as a sample file.
    CurrentExe,
    /// No path was given with `--background`, so the working directory is used.
    CurrentDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub target: SelectionTarget,
    pub origin: TargetOrigin,
}

/// The process facts target resolution depends on, read once at the command boundary.
#[derive(Debug, Clone)]
pub struct TargetContext {
    pub cwd: PathBuf,
    pub exe: PathBuf,
}

impl TargetContext {
    pub fn from_process() -> CliResult<Self> {
        Ok(Self {
            cwd: std::env::current_dir()
                .context(|| "cannot read the current directory".to_string())?,
            exe: std::env::current_exe()
                .context(|| "cannot locate the running executable".to_string())?,
        })
    }
}

/// JSON description of the target a command worked on.
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfo {
    pub kind: &'static str,
    pub path: String,
    pub origin: TargetOrigin,
}

impl ResolvedTarget {
    pub fn info(&self) -> TargetInfo {
        TargetInfo {
            kind: kind_label(&self.target),
            path: self.target.primary_path().display().to_string(),
            origin: self.origin,
        }
    }

    /// One line naming the target, for example `file C:\tools\rwm.exe (no path given, ...)`.
    pub fn describe(&self) -> String {
        let note = match self.origin {
            TargetOrigin::Explicit => "",
            TargetOrigin::CurrentExe => {
                " (no path given, so this executable is used as a sample file)"
            }
            TargetOrigin::CurrentDir => " (no path given, so the current folder is used)",
        };
        format!(
            "{} {}{note}",
            kind_label(&self.target),
            self.target.primary_path().display()
        )
    }
}

pub fn kind_label(target: &SelectionTarget) -> &'static str {
    match target {
        SelectionTarget::SingleFile(_) => "file",
        SelectionTarget::SingleFolder(_) => "folder",
        SelectionTarget::MultipleItems { .. } => "multiple items",
        SelectionTarget::FolderBackground(_) => "folder background",
        SelectionTarget::DesktopBackground(_) => "desktop background",
        SelectionTarget::Drive(_) => "drive",
    }
}

pub fn resolve_target(args: &TargetArgs, context: &TargetContext) -> CliResult<ResolvedTarget> {
    let (path, origin) = match (&args.path, args.background) {
        (Some(path), _) => (absolute(path, &context.cwd)?, TargetOrigin::Explicit),
        (None, true) => (context.cwd.clone(), TargetOrigin::CurrentDir),
        (None, false) => (context.exe.clone(), TargetOrigin::CurrentExe),
    };

    if !path.exists() {
        return Err(CliError::new(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    if args.background && !path.is_dir() {
        return Err(CliError::new(format!(
            "--background needs a folder, but {} is not one",
            path.display()
        )));
    }

    Ok(ResolvedTarget {
        target: SelectionTarget::from_path(path, args.background),
        origin,
    })
}

/// Shell APIs need absolute paths without `.`/`..` segments or a trailing separator.
fn absolute(path: &Path, cwd: &Path) -> CliResult<PathBuf> {
    let joined = cwd.join(path);
    let absolute = std::path::absolute(&joined)
        .context(|| format!("cannot resolve path {}", joined.display()))?;
    Ok(absolute.components().collect())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::TempDir;

    fn context(cwd: &Path, exe: &Path) -> TargetContext {
        TargetContext {
            cwd: cwd.to_path_buf(),
            exe: exe.to_path_buf(),
        }
    }

    fn args(path: Option<&str>, background: bool) -> TargetArgs {
        TargetArgs {
            path: path.map(PathBuf::from),
            background,
        }
    }

    struct Fixture {
        dir: TempDir,
        file: PathBuf,
        exe: PathBuf,
    }

    fn fixture(label: &str) -> Fixture {
        let dir = TempDir::new(label);
        let file = dir.path().join("notes.TXT");
        let exe = dir.path().join("rwm.exe");
        fs::write(&file, b"hello").expect("write sample file");
        fs::write(&exe, b"MZ").expect("write sample exe");
        Fixture { dir, file, exe }
    }

    #[test]
    fn explicit_file_and_folder_are_classified() {
        let fx = fixture("explicit");
        let ctx = context(fx.dir.path(), &fx.exe);

        let file = resolve_target(&args(fx.file.to_str(), false), &ctx).expect("file target");
        assert_eq!(file.target, SelectionTarget::SingleFile(fx.file.clone()));
        assert_eq!(file.origin, TargetOrigin::Explicit);
        assert_eq!(file.target.extension_lower().as_deref(), Some(".txt"));

        let folder =
            resolve_target(&args(fx.dir.path().to_str(), false), &ctx).expect("folder target");
        assert_eq!(
            folder.target,
            SelectionTarget::SingleFolder(fx.dir.path().to_path_buf())
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let fx = fixture("relative");
        let sub = fx.dir.path().join("sub");
        fs::create_dir(&sub).expect("create sub dir");
        let ctx = context(&sub, &fx.exe);

        let resolved =
            resolve_target(&args(Some(r"..\notes.TXT"), false), &ctx).expect("relative target");
        assert_eq!(
            resolved.target,
            SelectionTarget::SingleFile(fx.file.clone())
        );

        let trailing = format!(r"{}\", sub.display());
        let resolved =
            resolve_target(&args(Some(&trailing), false), &ctx).expect("trailing separator");
        assert_eq!(resolved.target, SelectionTarget::SingleFolder(sub));
    }

    #[test]
    fn background_targets_a_folder() {
        let fx = fixture("background");
        let ctx = context(fx.dir.path(), &fx.exe);

        let explicit =
            resolve_target(&args(fx.dir.path().to_str(), true), &ctx).expect("background target");
        assert_eq!(
            explicit.target,
            SelectionTarget::FolderBackground(fx.dir.path().to_path_buf())
        );
        assert!(explicit.target.is_background());

        let on_file = resolve_target(&args(fx.file.to_str(), true), &ctx).unwrap_err();
        assert!(on_file.to_string().contains("--background needs a folder"));
    }

    #[test]
    fn omitted_path_defaults_are_reported() {
        let fx = fixture("defaults");
        let ctx = context(fx.dir.path(), &fx.exe);

        let sample = resolve_target(&args(None, false), &ctx).expect("default file target");
        assert_eq!(sample.target, SelectionTarget::SingleFile(fx.exe.clone()));
        assert_eq!(sample.origin, TargetOrigin::CurrentExe);
        assert!(sample.describe().starts_with("file "));
        assert!(sample.describe().contains("no path given"));

        let background = resolve_target(&args(None, true), &ctx).expect("default background");
        assert_eq!(
            background.target,
            SelectionTarget::FolderBackground(fx.dir.path().to_path_buf())
        );
        assert_eq!(background.origin, TargetOrigin::CurrentDir);
        assert_eq!(background.info().kind, "folder background");
    }

    #[test]
    fn drive_roots_are_drives() {
        let fx = fixture("drive");
        let ctx = context(fx.dir.path(), &fx.exe);
        let root: PathBuf = fx.dir.path().components().take(2).collect();

        let resolved = resolve_target(&args(root.to_str(), false), &ctx).expect("drive target");
        assert_eq!(resolved.target, SelectionTarget::Drive(root));
        assert_eq!(resolved.info().kind, "drive");
    }

    #[test]
    fn missing_paths_are_rejected() {
        let fx = fixture("missing");
        let ctx = context(fx.dir.path(), &fx.exe);

        let err = resolve_target(&args(Some("nope.bin"), false), &ctx).unwrap_err();
        assert!(err.to_string().starts_with("path does not exist:"));
        assert!(err.to_string().contains("nope.bin"));
    }
}
