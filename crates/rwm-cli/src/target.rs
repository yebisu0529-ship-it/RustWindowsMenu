use std::path::{Path, PathBuf};

use crate::cli::TargetArgs;
use crate::error::{CliError, CliResult, Context};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub path: PathBuf,
    pub background: bool,
    pub defaulted: bool,
}

impl ResolvedTarget {
    pub fn describe(&self) -> String {
        let kind = if self.background {
            "folder background"
        } else if self.path.is_dir() {
            "folder"
        } else {
            "file"
        };
        let suffix = if self.defaulted {
            " (default target)"
        } else {
            ""
        };
        format!("{kind} {}{suffix}", self.path.display())
    }
}

pub fn resolve(args: &TargetArgs) -> CliResult<ResolvedTarget> {
    let (path, defaulted) = match (&args.path, args.background) {
        (Some(path), _) => (absolute(path)?, false),
        (None, true) => (
            std::env::current_dir().context(|| "cannot read current directory".to_string())?,
            true,
        ),
        (None, false) => (
            std::env::current_exe().context(|| "cannot locate current executable".to_string())?,
            true,
        ),
    };
    if !path.exists() {
        return Err(CliError::new(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    if args.background && !path.is_dir() {
        return Err(CliError::new(format!(
            "--background needs a folder: {}",
            path.display()
        )));
    }
    Ok(ResolvedTarget {
        path,
        background: args.background,
        defaulted,
    })
}

fn absolute(path: &Path) -> CliResult<PathBuf> {
    let path =
        std::path::absolute(path).context(|| format!("cannot resolve {}", path.display()))?;
    Ok(path.components().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn validates_background_and_missing_paths() {
        let dir = TempDir::new("target");
        let file = dir.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(resolve(&TargetArgs {
            path: Some(file.clone()),
            background: true
        })
        .is_err());
        assert!(resolve(&TargetArgs {
            path: Some(dir.path().join("missing")),
            background: false
        })
        .is_err());
        assert_eq!(
            resolve(&TargetArgs {
                path: Some(dir.path().to_path_buf()),
                background: true
            })
            .unwrap()
            .path,
            dir.path()
        );
    }
}
