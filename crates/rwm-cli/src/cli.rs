use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rwm",
    version,
    about = "RustWindowsMenu 0.2: fast native Windows menus",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Install(InstallArgs),
    Uninstall(UninstallArgs),
    Start,
    Stop,
    Status {
        #[arg(long)]
        json: bool,
    },
    Doctor(DoctorArgs),
    Bench(BenchArgs),
    Inspect(InspectArgs),
    Preview(PreviewArgs),
    Handlers {
        #[command(subcommand)]
        action: HandlersAction,
    },
    Classic {
        #[command(subcommand)]
        action: ClassicAction,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    Run,
    #[command(name = "__probe-worker", hide = true)]
    ProbeWorker(ProbeWorkerArgs),
}

#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    #[arg(long)]
    pub classic: bool,
    #[arg(long)]
    pub no_startup: bool,
    #[arg(long)]
    pub no_path: bool,
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    #[arg(long)]
    pub purge: bool,
}

#[derive(Debug, Clone, Default, Args)]
pub struct TargetArgs {
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub background: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_t = 30_000, hide = true)]
    pub timeout_ms: u64,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, default_value_t = 5)]
    pub runs: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_t = 30_000, hide = true)]
    pub timeout_ms: u64,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub extended: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_t = 30_000, hide = true)]
    pub timeout_ms: u64,
}

#[derive(Debug, Args)]
pub struct PreviewArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub extended: bool,
    #[arg(long)]
    pub no_animation: bool,
}

#[derive(Debug, Subcommand)]
pub enum HandlersAction {
    List {
        #[arg(long)]
        json: bool,
    },
    Block {
        clsid: String,
        #[arg(long)]
        name: Option<String>,
    },
    Unblock {
        clsid: String,
        #[arg(long)]
        force: bool,
    },
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum ClassicAction {
    Status {
        #[arg(long)]
        json: bool,
    },
    Enable {
        #[arg(long)]
        restart: bool,
    },
    Disable {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        restart: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    Path,
    Show,
    Init {
        #[arg(long)]
        force: bool,
    },
    Reset,
}

#[derive(Debug, Args)]
pub struct ProbeWorkerArgs {
    #[command(subcommand)]
    pub action: ProbeWorkerAction,
}

#[derive(Debug, Subcommand)]
pub enum ProbeWorkerAction {
    Inspect(InspectArgs),
    Bench(BenchArgs),
    #[command(hide = true)]
    Sleep {
        #[arg(long)]
        sleep_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn complete_surface_parses() {
        for args in [
            vec![
                "rwm",
                "install",
                "--classic",
                "--no-startup",
                "--no-path",
                "--no-start",
            ],
            vec!["rwm", "uninstall", "--purge"],
            vec!["rwm", "start"],
            vec!["rwm", "stop"],
            vec!["rwm", "status", "--json"],
            vec!["rwm", "doctor", "--background", "--json"],
            vec!["rwm", "bench", "--runs", "3", "--json"],
            vec!["rwm", "inspect", "--extended", "--json"],
            vec!["rwm", "preview", "--extended", "--no-animation"],
            vec!["rwm", "handlers", "list", "--json"],
            vec![
                "rwm",
                "handlers",
                "block",
                "{01234567-89AB-CDEF-0123-456789ABCDEF}",
            ],
            vec![
                "rwm",
                "handlers",
                "unblock",
                "{01234567-89AB-CDEF-0123-456789ABCDEF}",
                "--force",
            ],
            vec!["rwm", "handlers", "reset"],
            vec!["rwm", "classic", "status", "--json"],
            vec!["rwm", "classic", "enable", "--restart"],
            vec!["rwm", "classic", "disable", "--force", "--restart"],
            vec!["rwm", "config", "path"],
            vec!["rwm", "config", "show"],
            vec!["rwm", "config", "init", "--force"],
            vec!["rwm", "config", "reset"],
            vec!["rwm", "run"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }
}
