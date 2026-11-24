use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn rust_sources(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(base.join(dir))? {
        let entry = entry?;
        let path = dir.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            rust_sources(base, &path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let base = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![
        PathBuf::from("build.rs"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("../../Cargo.toml"),
    ];
    let lockfile = Path::new("../../Cargo.lock");
    if base.join(lockfile).is_file() {
        files.push(lockfile.into());
    }
    let hook_manifest = Path::new("../rwm-hook/Cargo.toml");
    if base.join(hook_manifest).is_file() {
        files.push(hook_manifest.into());
    }
    for dir in [Path::new("src"), Path::new("../rwm-hook/src")] {
        if base.join(dir).is_dir() {
            println!("cargo:rerun-if-changed={}", dir.display());
            rust_sources(&base, dir, &mut files)?;
        }
    }
    files.sort();

    let mut hash = 0xcbf29ce484222325u64;
    let mut hash_bytes = |bytes: &[u8]| {
        for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    hash_bytes(version.as_bytes());
    for key in ["PROFILE", "TARGET"] {
        hash_bytes(env::var(key).unwrap().as_bytes());
    }
    for file in files {
        let name = file.to_string_lossy().replace('\\', "/");
        println!("cargo:rerun-if-changed={name}");
        hash_bytes(name.as_bytes());
        hash_bytes(&fs::read(base.join(&file))?);
    }
    println!("cargo:rustc-env=RWM_BUILD_ID={version}-{hash:016x}");
    Ok(())
}
