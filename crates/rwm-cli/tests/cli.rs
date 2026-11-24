use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rwm_v02_cli_{}_{}_{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn rwm(scratch: &Scratch) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rwm"));
    command.env("RWM_CONFIG", scratch.0.join("config.toml"));
    command
}
fn run(command: &mut Command) -> Output {
    command.output().expect("run rwm")
}
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", stdout(output)))
}

#[test]
fn help_lists_complete_v02_surface_and_hides_probe_worker() {
    let scratch = Scratch::new("help");
    let help = stdout(&run(rwm(&scratch).arg("--help")));
    for command in [
        "install",
        "uninstall",
        "start",
        "stop",
        "status",
        "doctor",
        "bench",
        "inspect",
        "preview",
        "handlers",
        "classic",
        "config",
        "run",
    ] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "missing {command}:\n{help}"
        );
    }
    assert!(!help.contains("__probe-worker"));
}

#[test]
fn config_roundtrip_show_init_reset_and_strict_errors() {
    let scratch = Scratch::new("config");
    let path = run(rwm(&scratch).args(["config", "path"]));
    assert_eq!(
        stdout(&path).trim(),
        scratch.0.join("config.toml").display().to_string()
    );
    assert!(run(rwm(&scratch).args(["config", "show"])).status.success());
    assert!(run(rwm(&scratch).args(["config", "init"])).status.success());
    let config = fs::read_to_string(scratch.0.join("config.toml")).unwrap();
    toml::from_str::<rwm_core::config::RwmConfig>(&config).unwrap();
    assert!(!run(rwm(&scratch).args(["config", "init"])).status.success());
    fs::write(scratch.0.join("config.toml"), "arm_delay_ms = 99\n").unwrap();
    assert!(run(rwm(&scratch).args(["config", "init", "--force"]))
        .status
        .success());
    fs::write(
        scratch.0.join("config.toml"),
        "[appearance]\ntheme = 'dark'\n",
    )
    .unwrap();
    assert!(!run(rwm(&scratch).args(["config", "show"])).status.success());
    assert!(run(rwm(&scratch).args(["config", "reset"]))
        .status
        .success());
}

#[test]
fn read_only_status_and_handlers_json_are_machine_readable() {
    let scratch = Scratch::new("readonly");
    let status = run(rwm(&scratch).args(["status", "--json"]));
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let report = json(&status);
    assert_eq!(report["abi_version"], 2);
    assert!(report["config"]["valid"].as_bool().unwrap());
    assert!(report["host_running"].is_boolean());
    let handlers = run(rwm(&scratch).args(["handlers", "list", "--json"]));
    assert!(handlers.status.success());
    assert!(json(&handlers).is_array());
}

#[test]
fn invalid_targets_fail_before_any_shell_or_registry_change() {
    let scratch = Scratch::new("target");
    for command in ["inspect", "bench", "preview"] {
        let output = run(rwm(&scratch)
            .arg(command)
            .arg(scratch.0.join("missing.txt")));
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("path does not exist"));
    }
    let file = scratch.0.join("file.txt");
    fs::write(&file, b"x").unwrap();
    let output = run(rwm(&scratch).args(["inspect", "--background"]).arg(&file));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--background needs a folder"));
}

#[test]
fn probe_worker_json_contract() {
    let scratch = Scratch::new("probe");
    let output = run(rwm(&scratch).args(["__probe-worker", "sleep", "--sleep-ms", "1"]));
    assert!(output.status.success());
    assert_eq!(json(&output)["slept_ms"], 1);
}

#[test]
fn doctor_warns_actionably_on_legacy_v01_config() {
    let scratch = Scratch::new("legacy_doctor");
    fs::write(
        scratch.0.join("config.toml"),
        "[appearance]\ntheme = 'dark'\n",
    )
    .unwrap();
    let output = run(rwm(&scratch).args(["doctor", "--json"]));
    let report = json(&output);
    let checks = report["checks"].as_array().unwrap();
    let cfg = checks.iter().find(|c| c["name"] == "Config").unwrap();
    assert!(cfg["ok"].as_bool().unwrap());
    assert!(cfg["warning"].as_bool().unwrap());
    assert!(cfg["detail"]
        .as_str()
        .unwrap()
        .contains("rwm config reset"));
}

#[allow(dead_code)]
fn _path(_: &Path) {}
