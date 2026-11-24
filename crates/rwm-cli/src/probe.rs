use std::ffi::OsString;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{WaitForSingleObject, CREATE_NO_WINDOW};

use crate::cli::{BenchArgs, InspectArgs};
use crate::error::{CliError, CliResult, Context};

#[derive(Debug)]
pub enum ProbeRequest {
    Inspect(InspectArgs),
    Bench(BenchArgs),
    Sleep(u64),
}

struct Job(HANDLE);
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn kill_on_close_job() -> CliResult<Job> {
    let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(windows_to_cli)?;
    let job = Job(handle);
    let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation:
            windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
        ..Default::default()
    };
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(windows_to_cli)?;
    Ok(job)
}

fn windows_to_cli(error: windows::core::Error) -> CliError {
    CliError::new(error.message())
}

fn arguments(request: &ProbeRequest) -> Vec<OsString> {
    let mut args = vec![OsString::from("__probe-worker")];
    fn push_target(args: &mut Vec<OsString>, path: &Option<std::path::PathBuf>, background: bool) {
        if background {
            args.push("--background".into());
        }
        if let Some(path) = path {
            args.push("--".into());
            args.push(path.as_os_str().to_owned());
        }
    }
    match request {
        ProbeRequest::Inspect(options) => {
            args.push("inspect".into());
            if options.extended {
                args.push("--extended".into());
            }
            args.push("--json".into());
            push_target(&mut args, &options.target.path, options.target.background);
        }
        ProbeRequest::Bench(options) => {
            args.push("bench".into());
            args.extend([
                OsString::from("--runs"),
                options.runs.to_string().into(),
                "--json".into(),
            ]);
            push_target(&mut args, &options.target.path, options.target.background);
        }
        ProbeRequest::Sleep(ms) => args.extend([
            OsString::from("sleep"),
            "--sleep-ms".into(),
            ms.to_string().into(),
        ]),
    }
    args
}

pub fn run(request: &ProbeRequest, timeout: Duration, phase: &str) -> CliResult<Value> {
    let exe =
        std::env::current_exe().context(|| "cannot locate probe worker executable".to_string())?;
    let target = match request {
        ProbeRequest::Inspect(args) => args.target.path.clone(),
        ProbeRequest::Bench(args) => args.target.path.clone(),
        ProbeRequest::Sleep(_) => None,
    };
    run_from(&exe, request, timeout, phase, target.as_deref())
}

fn run_from(
    exe: &Path,
    request: &ProbeRequest,
    timeout: Duration,
    phase: &str,
    target: Option<&Path>,
) -> CliResult<Value> {
    let job = kill_on_close_job()?;
    let mut child = Command::new(exe)
        .args(arguments(request))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .context(|| format!("cannot start watchdog-isolated {phase} probe"))?;
    if let Err(error) = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) } {
        let _ = child.kill();
        let _ = child.wait();
        return Err(windows_to_cli(error));
    }
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = out_tx.send(bytes);
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        let _ = err_tx.send(bytes);
    });
    let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    let wait = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), millis) };
    if wait == WAIT_TIMEOUT {
        if unsafe { TerminateJobObject(job.0, 1) }.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        let _ = out_rx.recv_timeout(Duration::from_millis(500));
        let _ = err_rx.recv_timeout(Duration::from_millis(500));
        return Err(CliError::new(format!("{phase} probe for {} timed out after {} ms; worker PID {pid} and its Job Object were terminated", target.map_or_else(|| "<default>".to_string(), |path| path.display().to_string()), timeout.as_millis())));
    }
    if wait != WAIT_OBJECT_0 {
        let error = windows::core::Error::from_win32();
        if unsafe { TerminateJobObject(job.0, 1) }.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        return Err(windows_to_cli(error));
    }
    let status = child.wait()?;
    // Terminate any remaining job processes so a grandchild cannot keep output pipes open forever.
    unsafe {
        let _ = TerminateJobObject(job.0, 0);
    }
    let drain_timeout = Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(drain_timeout).map_err(|_| {
        CliError::new(format!(
            "{phase} probe stdout pipe stayed open after worker exit"
        ))
    })?;
    let stderr = err_rx.recv_timeout(drain_timeout).map_err(|_| {
        CliError::new(format!(
            "{phase} probe stderr pipe stayed open after worker exit"
        ))
    })?;
    if !status.success() {
        return Err(CliError::new(format!(
            "{phase} probe failed ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    serde_json::from_slice(&stdout).map_err(|error| {
        CliError::new(format!(
            "{phase} probe returned invalid JSON: {error}; {}",
            String::from_utf8_lossy(&stdout)
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_prefixed_target_uses_option_terminator() {
        use clap::Parser;
        let args = arguments(&ProbeRequest::Inspect(InspectArgs {
            target: crate::cli::TargetArgs {
                path: Some("-leading-dash.txt".into()),
                background: false,
            },
            extended: false,
            json: true,
            timeout_ms: 1000,
        }));
        let cli_args = std::iter::once(OsString::from("rwm")).chain(args);
        let parsed = crate::cli::Cli::try_parse_from(cli_args).unwrap();
        let crate::cli::Command::ProbeWorker(worker) = parsed.command else {
            panic!("expected ProbeWorker");
        };
        let crate::cli::ProbeWorkerAction::Inspect(inspect) = worker.action else {
            panic!("expected Inspect");
        };
        assert_eq!(
            inspect.target.path.as_deref(),
            Some(Path::new("-leading-dash.txt"))
        );
    }

    #[test]
    fn timeout_kills_the_job_and_returns_phase_diagnostic() {
        let started = std::time::Instant::now();
        let test_exe = std::env::current_exe().unwrap();
        let rwm = test_exe
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("rwm.exe");
        let error = run_from(
            &rwm,
            &ProbeRequest::Sleep(30_000),
            Duration::from_millis(100),
            "watchdog test",
            Some(Path::new("fixture.txt")),
        )
        .unwrap_err();
        let detail = error.to_string();
        assert!(
            detail.contains("watchdog test probe for fixture.txt timed out"),
            "{detail}"
        );
        let pid = detail
            .split("worker PID ")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let access = windows::Win32::System::Threading::PROCESS_SYNCHRONIZE;
        match unsafe { windows::Win32::System::Threading::OpenProcess(access, false, pid) } {
            Ok(process) => {
                let state =
                    unsafe { windows::Win32::System::Threading::WaitForSingleObject(process, 0) };
                unsafe {
                    let _ = CloseHandle(process);
                }
                assert_eq!(
                    state, WAIT_OBJECT_0,
                    "timed-out worker PID {pid} is still alive"
                );
            }
            // Once `child.wait()` drops the last handle to the terminated process, Windows reaps it
            // immediately and `OpenProcess` returns ERROR_INVALID_PARAMETER.
            Err(error)
                if error.code() == windows::Win32::Foundation::ERROR_INVALID_PARAMETER.to_hresult() => {}
            Err(error) => panic!("unexpected OpenProcess failure for PID {pid}: {error}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
