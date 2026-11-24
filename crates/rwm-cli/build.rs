use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=rwm.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let manifest = manifest_dir.join("rwm.manifest");
        let arg = format!("/MANIFESTINPUT:{}", manifest.display());
        for bin in ["rwm", "rwm-host"] {
            println!("cargo:rustc-link-arg-bin={bin}=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bin={bin}={arg}");
        }
    }
}
