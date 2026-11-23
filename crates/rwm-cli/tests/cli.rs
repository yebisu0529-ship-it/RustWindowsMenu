//! Drives the built `rwm.exe` the way a user does.
//!
//! Only read-only commands run here. Nothing in this file may invoke `run`, `install`,
//! `uninstall`, or `classic enable|disable` with the hook DLL present, because those change
//! the live registry or attach to the real Explorer.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// A scratch folder that is deleted when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rwm_cli_e2e_{}_{}_{label}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `rwm.exe` with its config pointed into the scratch folder, away from the user's own.
fn rwm(scratch: &Scratch) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rwm"));
    command.env("RWM_CONFIG", scratch.path().join("config.toml"));
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("rwm.exe starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("stdout is not JSON ({err}):\n{}", stdout(output)))
}

#[test]
fn version_and_help_list_the_command_suite() {
    let scratch = Scratch::new("help");

    let version = run(rwm(&scratch).arg("--version"));
    assert!(version.status.success());
    assert_eq!(
        stdout(&version).trim(),
        format!("rwm {}", env!("CARGO_PKG_VERSION"))
    );

    let help = stdout(&run(rwm(&scratch).arg("--help")));
    for subcommand in [
        "status",
        "classic",
        "bench",
        "handlers",
        "preview",
        "run",
        "stop",
        "install",
        "uninstall",
        "config",
        "doctor",
    ] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(subcommand)),
            "help does not list `{subcommand}`:\n{help}"
        );
    }
}

#[test]
fn config_commands_create_and_validate_the_file() {
    let scratch = Scratch::new("config");
    let config = scratch.path().join("config.toml");

    let path = run(rwm(&scratch).args(["config", "path"]));
    assert_eq!(stdout(&path).trim(), config.display().to_string());

    let missing = run(rwm(&scratch).args(["config", "validate"]));
    assert!(missing.status.success(), "a missing config means defaults");
    assert!(stdout(&missing).contains("built-in defaults apply"));

    let created = run(rwm(&scratch).args(["config", "init"]));
    assert!(created.status.success());
    assert!(stdout(&created).starts_with("Created the default config"));
    let written = fs::read_to_string(&config).expect("config init wrote the file");

    let again = run(rwm(&scratch).args(["config", "init"]));
    assert!(stdout(&again).contains("left unchanged"));
    assert_eq!(fs::read_to_string(&config).unwrap(), written);

    let valid = run(rwm(&scratch).args(["config", "validate"]));
    assert!(valid.status.success());
    assert!(stdout(&valid).contains("(valid)"));

    fs::write(&config, "[handlers]\ndefault_policy = \"sideways\"\n").unwrap();
    let invalid = run(rwm(&scratch).args(["config", "validate"]));
    assert_eq!(invalid.status.code(), Some(1));
    assert!(stdout(&invalid).contains("INVALID"));
}

#[test]
fn status_json_reports_every_section() {
    let scratch = Scratch::new("status");

    let output = run(rwm(&scratch).args(["status", "--json"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let report = json(&output);

    assert_eq!(report["rwm_version"], env!("CARGO_PKG_VERSION"));
    assert!(report["classic"]["windows_build"].is_u64());
    assert!(report["classic"]["classic_menu_enabled"].is_boolean());
    assert_eq!(report["config"]["exists"], false);
    assert_eq!(report["config"]["valid"], true);
    assert!(report["kill_switch"]["active"].is_boolean());
    assert!(report["hook_dll"]["found"].is_boolean());
    assert!(report["explorer"]["ui_threads"].is_u64());
    assert!(report["hook_host_running"].is_boolean());

    let text = stdout(&run(rwm(&scratch).arg("status")));
    for label in [
        "Windows",
        "Classic menu",
        "MenuShowDelay",
        "Backup",
        "Config",
        "Kill switch",
        "Hook DLL",
        "Explorer",
    ] {
        assert!(
            text.contains(label),
            "status does not show `{label}`:\n{text}"
        );
    }
}

#[test]
fn handlers_list_describes_the_target_and_its_handlers() {
    let scratch = Scratch::new("handlers");
    let file = scratch.path().join("notes.txt");
    fs::write(&file, "hello").unwrap();

    let output = run(rwm(&scratch)
        .args(["handlers", "list", "--json"])
        .arg(&file));
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = json(&output);
    assert_eq!(listing["target"]["kind"], "file");
    assert_eq!(listing["target"]["origin"], "explicit");
    assert_eq!(listing["default_policy"], "inline");
    assert!(listing["static_verbs"].is_array());
    let handlers = listing["com_handlers"].as_array().expect("com_handlers");
    for handler in handlers {
        assert!(handler["clsid"].as_str().unwrap().starts_with('{'));
        assert!(matches!(
            handler["policy"].as_str(),
            Some("inline" | "submenu" | "disabled")
        ));
    }

    let background = run(rwm(&scratch)
        .args(["handlers", "list", "--background", "--json"])
        .current_dir(scratch.path()));
    assert_eq!(json(&background)["target"]["kind"], "folder background");
    assert_eq!(json(&background)["target"]["origin"], "current_dir");
}

#[test]
fn config_policy_overrides_show_up_in_the_listing() {
    let scratch = Scratch::new("policy");
    fs::write(
        scratch.path().join("config.toml"),
        "[handlers]\ndefault_policy = \"submenu\"\n",
    )
    .unwrap();

    let output = run(rwm(&scratch)
        .args(["handlers", "list", "--background", "--json"])
        .current_dir(scratch.path()));
    let listing = json(&output);
    assert_eq!(listing["default_policy"], "submenu");
    for handler in listing["com_handlers"].as_array().expect("com_handlers") {
        assert_eq!(handler["policy"], "submenu");
    }
}

#[test]
fn bad_targets_fail_with_a_clear_message() {
    let scratch = Scratch::new("bad_target");
    let file = scratch.path().join("notes.txt");
    fs::write(&file, "hello").unwrap();

    let missing = run(rwm(&scratch)
        .arg("bench")
        .arg(scratch.path().join("nope.bin")));
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).starts_with("error: path does not exist:"));
    assert!(stdout(&missing).is_empty());

    let file_as_background = run(rwm(&scratch).args(["preview", "--background"]).arg(&file));
    assert_eq!(file_as_background.status.code(), Some(1));
    assert!(stderr(&file_as_background).contains("--background needs a folder"));

    let unknown = run(rwm(&scratch).arg("explode"));
    assert_eq!(
        unknown.status.code(),
        Some(2),
        "clap rejects unknown commands"
    );
}

#[test]
fn doctor_exit_code_follows_its_report() {
    let scratch = Scratch::new("doctor");

    let output = run(rwm(&scratch).arg("doctor"));
    let report = stdout(&output);
    for check in [
        "Windows 11",
        "Direct2D + DirectWrite",
        "Hook DLL present",
        "Config",
        "Kill switch",
        "HKCU registry write",
    ] {
        assert!(
            report.contains(check),
            "doctor does not run `{check}`:\n{report}"
        );
    }
    assert_eq!(
        output.status.success(),
        !report.contains("[FAIL]"),
        "a failed check must make doctor exit non-zero:\n{report}"
    );

    fs::write(scratch.path().join("config.toml"), "[appearance\n").unwrap();
    let broken = run(rwm(&scratch).arg("doctor"));
    assert_eq!(broken.status.code(), Some(1));
    assert!(stdout(&broken).contains("[FAIL] Config"));
}

/// With no hook DLL beside it, `rwm run` has nothing to attach and must stop right there.
#[test]
fn run_without_the_hook_dll_says_what_is_missing() {
    let scratch = Scratch::new("no_dll");
    let lonely_exe = scratch.path().join("rwm.exe");
    fs::copy(env!("CARGO_BIN_EXE_rwm"), &lonely_exe).expect("copy rwm.exe");

    let output = run(Command::new(&lonely_exe).arg("run"));

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("error: rwm_hook.dll was not found"));
    assert!(stdout(&output).is_empty(), "nothing was started");
}

#[test]
#[ignore = "loads every COM shell extension registered on this PC; run with --ignored"]
fn bench_profiles_a_real_file() {
    let scratch = Scratch::new("bench");
    let file = scratch.path().join("notes.txt");
    fs::write(&file, "hello").unwrap();

    let output = run(rwm(&scratch).args(["bench", "--json"]).arg(&file));
    assert!(output.status.success(), "{}", stderr(&output));
    let report = json(&output);

    assert_eq!(report["target"]["kind"], "file");
    assert!(report["stage0_us"].as_u64().unwrap() > 0);
    assert!(report["stage0_item_count"].as_u64().unwrap() > 0);
    assert!(report["speedup_factor"].as_f64().unwrap() >= 1.0);
    let durations: Vec<u64> = report["com_handlers"]
        .as_array()
        .expect("com_handlers")
        .iter()
        .map(|handler| handler["duration_us"].as_u64().unwrap())
        .collect();
    assert!(
        durations.windows(2).all(|pair| pair[0] >= pair[1]),
        "handlers are listed slowest first: {durations:?}"
    );
}

#[test]
#[ignore = "opens the real popup menu on screen for a moment; run with --ignored"]
fn preview_smoke_test_renders_and_exits() {
    let scratch = Scratch::new("preview");
    let file = scratch.path().join("notes.txt");
    fs::write(&file, "hello").unwrap();

    let output = run(rwm(&scratch)
        .args(["preview", "--smoke-test-ms", "400"])
        .arg(&file));
    assert!(output.status.success(), "{}", stderr(&output));

    let text = stdout(&output);
    let summary = text
        .lines()
        .find(|line| line.starts_with("preview smoke-test: "))
        .unwrap_or_else(|| panic!("no summary line in:\n{text}"));
    let keys: Vec<&str> = summary["preview smoke-test: ".len()..]
        .split(' ')
        .filter_map(|pair| pair.split_once('=').map(|(key, _)| key))
        .collect();
    assert_eq!(
        keys,
        [
            "rendered_with_d2d",
            "stage0_us",
            "hydrated_groups",
            "final_item_count"
        ]
    );
}
