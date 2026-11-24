use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

pub fn unique_name(label: &str) -> String {
    format!(
        "rwm_cli_test_{}_{}_{label}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub struct TempDir(PathBuf);
impl TempDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(unique_name(label));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(std::path::absolute(&path).expect("absolute temp dir"))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct RegistryRoot(String);
impl RegistryRoot {
    pub fn new(label: &str) -> Self {
        Self(format!(r"Software\{}", unique_name(label)))
    }
    pub fn path(&self) -> &str {
        &self.0
    }
}
impl Drop for RegistryRoot {
    fn drop(&mut self) {
        let _ = crate::reg::delete_tree(&self.0);
    }
}
