#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    rwm_cli::init_logging();
    match rwm_cli::host::run() {
        Ok(code) => code,
        Err(error) => {
            rwm_cli::output::print_error_line(format!("rwm-host: {error}"));
            std::process::ExitCode::FAILURE
        }
    }
}
