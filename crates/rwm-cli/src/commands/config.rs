use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rwm_core::config::{ConfigError, RwmConfig};
use serde::Serialize;

use crate::cli::ConfigAction;
use crate::error::{CliError, CliResult};
use crate::output::print_line;

/// Loads the config for a command that runs the engine. A missing file means the defaults.
pub fn load_config() -> CliResult<RwmConfig> {
    RwmConfig::load_or_default().map_err(|err| match err {
        // Every other variant already names the file.
        ConfigError::Validation(_) => CliError::new(format!(
            "{}: {err}. Check it with `rwm config validate`.",
            RwmConfig::config_path().display()
        )),
        other => other.into(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigHealth {
    /// No file yet. The built-in defaults apply.
    Missing,
    Valid,
    Invalid(String),
}

/// The config file as `status`, `doctor`, and `config validate` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigState {
    pub path: PathBuf,
    pub health: ConfigHealth,
}

/// The JSON shape of a `ConfigState`: plain booleans are easier on scripts than an enum.
#[derive(Debug, Serialize)]
struct ConfigJson<'a> {
    path: &'a Path,
    exists: bool,
    valid: bool,
    error: Option<&'a str>,
}

impl Serialize for ConfigState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ConfigJson {
            path: &self.path,
            exists: self.health != ConfigHealth::Missing,
            valid: self.is_usable(),
            error: match &self.health {
                ConfigHealth::Invalid(error) => Some(error),
                _ => None,
            },
        }
        .serialize(serializer)
    }
}

impl ConfigState {
    pub fn inspect(path: &Path) -> Self {
        let health = if !path.exists() {
            ConfigHealth::Missing
        } else {
            match RwmConfig::load_from(path) {
                Ok(_) => ConfigHealth::Valid,
                Err(err) => ConfigHealth::Invalid(err.to_string()),
            }
        };
        Self {
            path: path.to_path_buf(),
            health,
        }
    }

    pub fn current() -> Self {
        Self::inspect(&RwmConfig::config_path())
    }

    /// A missing file counts as usable: the hook and the CLI fall back to the defaults.
    pub fn is_usable(&self) -> bool {
        !matches!(self.health, ConfigHealth::Invalid(_))
    }

    pub fn describe(&self) -> String {
        let path = self.path.display();
        match &self.health {
            ConfigHealth::Missing => {
                format!("{path} (not created yet, built-in defaults apply)")
            }
            ConfigHealth::Valid => format!("{path} (valid)"),
            ConfigHealth::Invalid(error) => format!("{path} (INVALID: {error})"),
        }
    }
}

pub fn run(action: ConfigAction) -> CliResult<ExitCode> {
    match action {
        ConfigAction::Path => {
            print_line(RwmConfig::config_path().display().to_string());
            Ok(ExitCode::SUCCESS)
        }
        ConfigAction::Init => {
            let (path, created) = RwmConfig::write_default_if_missing()?;
            if created {
                print_line(format!("Created the default config at {}", path.display()));
            } else {
                print_line(format!(
                    "A config already exists at {}. It was left unchanged.",
                    path.display()
                ));
            }
            Ok(ExitCode::SUCCESS)
        }
        ConfigAction::Validate => {
            let state = ConfigState::current();
            print_line(state.describe());
            Ok(if state.is_usable() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn missing_config_is_usable_and_reported_as_defaults() {
        let dir = TempDir::new("config_missing");
        let state = ConfigState::inspect(&dir.path().join("config.toml"));

        assert_eq!(state.health, ConfigHealth::Missing);
        assert!(state.is_usable());
        assert!(state.describe().contains("built-in defaults apply"));
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["exists"], false);
        assert_eq!(json["valid"], true);
        assert!(json["error"].is_null());
    }

    #[test]
    fn valid_config_is_accepted() {
        let dir = TempDir::new("config_valid");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[handlers]\ndefault_policy = \"submenu\"\n").unwrap();

        let state = ConfigState::inspect(&path);
        assert_eq!(state.health, ConfigHealth::Valid);
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["path"], path.to_str().unwrap());
        assert_eq!(json["exists"], true);
        assert_eq!(json["valid"], true);
    }

    #[test]
    fn broken_toml_and_out_of_range_values_are_invalid() {
        let dir = TempDir::new("config_invalid");

        let broken = dir.path().join("broken.toml");
        fs::write(&broken, "[appearance\ntheme = ").unwrap();
        let state = ConfigState::inspect(&broken);
        assert!(matches!(state.health, ConfigHealth::Invalid(_)));
        assert!(!state.is_usable());
        assert!(state.describe().contains("INVALID"));

        let out_of_range = dir.path().join("range.toml");
        fs::write(&out_of_range, "[appearance]\ncorner_radius = 99\n").unwrap();
        let json = serde_json::to_value(ConfigState::inspect(&out_of_range)).unwrap();
        assert_eq!(json["exists"], true);
        assert_eq!(json["valid"], false);
        assert!(json["error"].as_str().unwrap().contains("corner_radius"));
    }
}
