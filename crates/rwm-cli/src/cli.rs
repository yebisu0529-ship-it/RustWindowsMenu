use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, PartialEq, Eq, Parser)]
#[command(
    name = "rwm",
    version,
    about = "RustWindowsMenu: an instant classic right-click menu for Windows 11",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Show Windows, classic menu, config, hook DLL, and Explorer state
    Status {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Switch Windows 11 between its new menu and the classic context menu
    Classic {
        #[command(subcommand)]
        action: ClassicAction,
    },
    /// Profile menu latency for a path: Rust Stage 0 against synchronous COM handlers
    Bench(BenchArgs),
    /// Inspect the shell verbs and COM handlers registered for a path
    Handlers {
        #[command(subcommand)]
        action: HandlersAction,
    },
    /// Open the real Direct2D popup menu for a path
    Preview(PreviewArgs),
    /// Host the Explorer hook in the foreground until Ctrl+C
    Run {
        /// Detach from the console window (used by autostart)
        #[arg(long)]
        hidden: bool,
    },
    /// Ask a running hook host (including `rwm run --hidden`) to detach and exit
    Stop,
    /// Enable the classic menu and install rwm into %LOCALAPPDATA%\RustWindowsMenu\bin
    Install(InstallArgs),
    /// Remove the installed files and autostart entry, and restore the Windows 11 menu
    Uninstall(UninstallArgs),
    /// Create, locate, or validate config.toml
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check that this PC can run RustWindowsMenu
    Doctor,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ClassicAction {
    /// Turn the classic context menu on
    Enable {
        /// Leave MenuShowDelay as it is instead of setting it to 0
        #[arg(long)]
        no_delay_opt: bool,
        /// Restart explorer.exe so the change applies immediately
        #[arg(long)]
        restart_explorer: bool,
    },
    /// Restore the state from before `rwm classic enable`
    Disable {
        /// Restart explorer.exe so the change applies immediately
        #[arg(long)]
        restart_explorer: bool,
    },
    /// Show whether the classic context menu is on
    Status {
        /// Print the status as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum HandlersAction {
    /// List static verbs and COM handlers with their config policy
    List(HandlersListArgs),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ConfigAction {
    /// Write the default config.toml unless one already exists
    Init,
    /// Print the config file path
    Path,
    /// Parse and validate the config file
    Validate,
}

/// The right-click target shared by `bench`, `handlers list`, and `preview`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct TargetArgs {
    /// File, folder, or drive to right-click. Without it: this executable as a sample
    /// file, or the current folder with --background
    pub path: Option<PathBuf>,
    /// Right-click the empty background of the folder instead of an item
    #[arg(long)]
    pub background: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Print the report as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct HandlersListArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Print the list as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct PreviewArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Show the Stage 0 timing badge in the menu and print the timings afterwards
    #[arg(long)]
    pub timings: bool,
    /// Open at the screen centre, close after this many milliseconds, and print a summary
    #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u32).range(1..))]
    pub smoke_test_ms: Option<u32>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct InstallArgs {
    /// Only enable the classic menu and create the config; copy no files
    #[arg(long)]
    pub classic_only: bool,
    /// Start `rwm run` at sign-in through the HKCU Run key
    #[arg(long, conflicts_with = "classic_only")]
    pub autostart: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub struct UninstallArgs {
    /// Leave the classic context menu switched on
    #[arg(long)]
    pub keep_classic: bool,
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args.iter().copied())
            .unwrap_or_else(|err| panic!("{args:?} should parse: {err}"))
            .command
    }

    fn parse_error(args: &[&str]) -> ErrorKind {
        Cli::try_parse_from(args.iter().copied())
            .expect_err("arguments should be rejected")
            .kind()
    }

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn status_and_doctor_and_run() {
        assert_eq!(parse(&["rwm", "status"]), Command::Status { json: false });
        assert_eq!(
            parse(&["rwm", "status", "--json"]),
            Command::Status { json: true }
        );
        assert_eq!(parse(&["rwm", "doctor"]), Command::Doctor);
        assert_eq!(
            parse(&["rwm", "run"]),
            Command::Run { hidden: false }
        );
        assert_eq!(
            parse(&["rwm", "run", "--hidden"]),
            Command::Run { hidden: true }
        );
        assert_eq!(parse(&["rwm", "stop"]), Command::Stop);
    }

    #[test]
    fn classic_actions() {
        assert_eq!(
            parse(&["rwm", "classic", "enable"]),
            Command::Classic {
                action: ClassicAction::Enable {
                    no_delay_opt: false,
                    restart_explorer: false
                }
            }
        );
        assert_eq!(
            parse(&[
                "rwm",
                "classic",
                "enable",
                "--no-delay-opt",
                "--restart-explorer"
            ]),
            Command::Classic {
                action: ClassicAction::Enable {
                    no_delay_opt: true,
                    restart_explorer: true
                }
            }
        );
        assert_eq!(
            parse(&["rwm", "classic", "disable", "--restart-explorer"]),
            Command::Classic {
                action: ClassicAction::Disable {
                    restart_explorer: true
                }
            }
        );
        assert_eq!(
            parse(&["rwm", "classic", "status", "--json"]),
            Command::Classic {
                action: ClassicAction::Status { json: true }
            }
        );
        assert_eq!(
            parse_error(&["rwm", "classic", "disable", "--no-delay-opt"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn bench_target_forms() {
        assert_eq!(
            parse(&["rwm", "bench"]),
            Command::Bench(BenchArgs {
                target: TargetArgs::default(),
                json: false
            })
        );
        assert_eq!(
            parse(&["rwm", "bench", r"C:\Windows", "--background", "--json"]),
            Command::Bench(BenchArgs {
                target: TargetArgs {
                    path: Some(PathBuf::from(r"C:\Windows")),
                    background: true
                },
                json: true
            })
        );
        assert_eq!(
            parse_error(&["rwm", "bench", "one", "two"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn handlers_list() {
        assert_eq!(
            parse(&["rwm", "handlers", "list", "notes.txt", "--json"]),
            Command::Handlers {
                action: HandlersAction::List(HandlersListArgs {
                    target: TargetArgs {
                        path: Some(PathBuf::from("notes.txt")),
                        background: false
                    },
                    json: true
                })
            }
        );
        assert_eq!(
            parse_error(&["rwm", "handlers"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn preview_options() {
        assert_eq!(
            parse(&[
                "rwm",
                "preview",
                "--background",
                "--timings",
                "--smoke-test-ms",
                "750"
            ]),
            Command::Preview(PreviewArgs {
                target: TargetArgs {
                    path: None,
                    background: true
                },
                timings: true,
                smoke_test_ms: Some(750)
            })
        );
        assert_eq!(
            parse_error(&["rwm", "preview", "--smoke-test-ms", "soon"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            parse_error(&["rwm", "preview", "--smoke-test-ms", "0"]),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn install_and_uninstall_flags() {
        assert_eq!(
            parse(&["rwm", "install"]),
            Command::Install(InstallArgs {
                classic_only: false,
                autostart: false
            })
        );
        assert_eq!(
            parse(&["rwm", "install", "--autostart"]),
            Command::Install(InstallArgs {
                classic_only: false,
                autostart: true
            })
        );
        assert_eq!(
            parse(&["rwm", "install", "--classic-only"]),
            Command::Install(InstallArgs {
                classic_only: true,
                autostart: false
            })
        );
        assert_eq!(
            parse_error(&["rwm", "install", "--classic-only", "--autostart"]),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            parse(&["rwm", "uninstall", "--keep-classic"]),
            Command::Uninstall(UninstallArgs { keep_classic: true })
        );
    }

    #[test]
    fn config_actions() {
        for (word, action) in [
            ("init", ConfigAction::Init),
            ("path", ConfigAction::Path),
            ("validate", ConfigAction::Validate),
        ] {
            assert_eq!(parse(&["rwm", "config", word]), Command::Config { action });
        }
    }

    #[test]
    fn missing_or_unknown_subcommands_are_rejected() {
        assert_eq!(
            parse_error(&["rwm"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            parse_error(&["rwm", "explode"]),
            ErrorKind::InvalidSubcommand
        );
    }
}
