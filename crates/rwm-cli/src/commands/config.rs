use std::process::ExitCode;

use rwm_core::config::{config_path, RwmConfig};
use serde::Serialize;

use crate::cli::ConfigAction;
use crate::error::{CliError, CliResult};
use crate::output::print_line;

#[derive(Debug, Serialize)]
pub struct ConfigState {
    pub path: std::path::PathBuf,
    pub exists: bool,
    pub valid: bool,
    pub error: Option<String>,
    pub legacy_v01_keys: bool,
}

pub fn state() -> ConfigState {
    let path = config_path();
    let raw = std::fs::read_to_string(&path);
    let exists = raw.is_ok();
    let (valid, error) = match RwmConfig::load_strict() {
        Ok(_) => (true, None),
        Err(rwm_core::config::ConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            (true, None)
        }
        Err(error) => (false, Some(error.to_string())),
    };
    ConfigState {
        path,
        exists,
        valid,
        error,
        legacy_v01_keys: raw
            .ok()
            .is_some_and(|raw| RwmConfig::detect_legacy_v01_keys(&raw)),
    }
}

pub fn run(action: ConfigAction) -> CliResult<ExitCode> {
    match action {
        ConfigAction::Path => print_line(config_path().display().to_string()),
        ConfigAction::Show => {
            let config = RwmConfig::load_or_default()?;
            print_line(toml::to_string_pretty(&config)?);
        }
        ConfigAction::Init { force } => {
            let path = config_path();
            if path.exists() && !force {
                return Err(CliError::new(format!(
                    "config already exists: {}; use --force to replace it",
                    path.display()
                )));
            }
            RwmConfig::default().save()?;
            print_line(format!("Wrote default config to {}", path.display()));
        }
        ConfigAction::Reset => {
            RwmConfig::default().save()?;
            print_line(format!(
                "Reset config to v0.3 defaults: {}",
                config_path().display()
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn example_config_parses_strictly_as_v03() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config.example.toml"
        ));
        let parsed = toml::from_str::<rwm_core::config::RwmConfig>(raw)
            .unwrap()
            .validated();
        assert_eq!(parsed, rwm_core::config::RwmConfig::default());
    }
}
