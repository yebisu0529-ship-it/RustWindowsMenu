use std::process::ExitCode;

use rwm_core::config::HandlersConfig;
use rwm_core::model::HandlerPolicy;
use rwm_core::registry::{DiscoveredComHandler, RegistryVerbCache, StaticVerb};
use serde::Serialize;

use super::config::load_config;
use super::policy_label;
use crate::cli::HandlersListArgs;
use crate::error::CliResult;
use crate::output::{print, print_json, truncate_to_width, Align, Table};
use crate::target::{resolve_target, ResolvedTarget, TargetContext, TargetInfo};

const LABEL_COLUMN_WIDTH: usize = 40;

#[derive(Debug, Serialize)]
struct VerbRow {
    verb: String,
    label: String,
    /// Only shown when Shift is held during the right-click.
    shift_only: bool,
    subitems: usize,
    command: Option<String>,
}

#[derive(Debug, Serialize)]
struct HandlerRow {
    name: String,
    clsid: String,
    policy: HandlerPolicy,
    dll_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct HandlersReport {
    target: TargetInfo,
    default_policy: HandlerPolicy,
    static_verbs: Vec<VerbRow>,
    com_handlers: Vec<HandlerRow>,
}

impl HandlersReport {
    fn new(
        target: &ResolvedTarget,
        config: &HandlersConfig,
        verbs: &[StaticVerb],
        handlers: &[DiscoveredComHandler],
    ) -> Self {
        Self {
            target: target.info(),
            default_policy: config.default_policy,
            static_verbs: verbs
                .iter()
                .map(|verb| VerbRow {
                    verb: verb.key_name.clone(),
                    label: verb.label.clone(),
                    shift_only: verb.extended_only,
                    subitems: verb.subcommands.len(),
                    command: verb.command.clone(),
                })
                .collect(),
            com_handlers: handlers
                .iter()
                .map(|handler| HandlerRow {
                    name: handler.name.clone(),
                    clsid: handler.clsid.clone(),
                    policy: config.policy_for(&handler.clsid, &handler.name),
                    dll_path: handler.dll_path.clone(),
                })
                .collect(),
        }
    }
}

pub fn list(args: &HandlersListArgs) -> CliResult<ExitCode> {
    let resolved = resolve_target(&args.target, &TargetContext::from_process()?)?;
    let config = load_config()?;
    let (verbs, handlers) = RegistryVerbCache::new(false).collect_for_target(&resolved.target);
    let report = HandlersReport::new(&resolved, &config.handlers, &verbs, &handlers);

    if args.json {
        print_json(&report)?;
    } else {
        print(&render(&report, &resolved));
    }
    Ok(ExitCode::SUCCESS)
}

fn render(report: &HandlersReport, target: &ResolvedTarget) -> String {
    let mut text = format!("Target: {}\n\n", target.describe());

    text.push_str("Static verbs (read from the registry, always shown inline):\n");
    if report.static_verbs.is_empty() {
        text.push_str("  none\n");
    } else {
        let mut table = Table::new(&[
            ("verb", Align::Left),
            ("label", Align::Left),
            ("shift_only", Align::Left),
            ("subitems", Align::Right),
            ("command", Align::Left),
        ]);
        for row in &report.static_verbs {
            table.row([
                truncate_to_width(&row.verb, LABEL_COLUMN_WIDTH),
                truncate_to_width(&row.label, LABEL_COLUMN_WIDTH),
                if row.shift_only { "yes" } else { "no" }.to_string(),
                row.subitems.to_string(),
                row.command.clone().unwrap_or_else(|| "-".to_string()),
            ]);
        }
        text.push_str(&table.render());
    }

    text.push_str(&format!(
        "\nCOM handlers (config default policy: {}):\n",
        policy_label(report.default_policy)
    ));
    if report.com_handlers.is_empty() {
        text.push_str("  none\n");
    } else {
        let mut table = Table::new(&[
            ("name", Align::Left),
            ("clsid", Align::Left),
            ("policy", Align::Left),
            ("dll_path", Align::Left),
        ]);
        for row in &report.com_handlers {
            table.row([
                truncate_to_width(&row.name, LABEL_COLUMN_WIDTH),
                row.clsid.clone(),
                policy_label(row.policy).to_string(),
                row.dll_path.clone().unwrap_or_else(|| "-".to_string()),
            ]);
        }
        text.push_str(&table.render());
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rwm_core::model::SelectionTarget;

    use super::*;
    use crate::target::TargetOrigin;

    fn verb(key_name: &str, label: &str, extended_only: bool, command: Option<&str>) -> StaticVerb {
        StaticVerb {
            key_name: key_name.to_string(),
            label: label.to_string(),
            mnemonic: None,
            icon_spec: None,
            command: command.map(str::to_string),
            extended_only,
            subcommands: Vec::new(),
            canonical_verb: Some(key_name.to_string()),
            position: None,
        }
    }

    fn handler(name: &str, clsid: &str, dll_path: Option<&str>) -> DiscoveredComHandler {
        DiscoveredComHandler {
            name: name.to_string(),
            clsid: clsid.to_string(),
            dll_path: dll_path.map(str::to_string),
        }
    }

    fn target() -> ResolvedTarget {
        ResolvedTarget {
            target: SelectionTarget::SingleFolder(PathBuf::from(r"C:\work")),
            origin: TargetOrigin::Explicit,
        }
    }

    fn sample_report() -> HandlersReport {
        let mut config = HandlersConfig::default();
        config
            .overrides
            .insert("filesyncex".to_string(), HandlerPolicy::Submenu);
        config.overrides.insert(
            "{11111111-1111-1111-1111-111111111111}".to_string(),
            HandlerPolicy::Disabled,
        );

        let mut cascade = verb("git_shell", "Git Bash Here", false, None);
        cascade.subcommands = vec![verb("child", "Child", false, Some("child.exe"))];
        HandlersReport::new(
            &target(),
            &config,
            &[
                verb(
                    "cmd",
                    "Open command window here",
                    true,
                    Some("cmd.exe /s /k pushd \"%V\""),
                ),
                cascade,
            ],
            &[
                handler(
                    "FileSyncEx",
                    "{CB3D0F55-BC2C-4C1A-85ED-23ED75B5106B}",
                    Some(r"C:\OneDrive\FileSyncShell64.dll"),
                ),
                handler("Legacy", "{11111111-1111-1111-1111-111111111111}", None),
                handler(
                    "7-Zip",
                    "{23170F69-40C1-278A-1000-000100020000}",
                    Some(r"C:\Program Files\7-Zip\7-zip.dll"),
                ),
            ],
        )
    }

    #[test]
    fn policies_follow_config_overrides_by_name_and_clsid() {
        let report = sample_report();
        let policies: Vec<(&str, HandlerPolicy)> = report
            .com_handlers
            .iter()
            .map(|row| (row.name.as_str(), row.policy))
            .collect();
        assert_eq!(
            policies,
            vec![
                ("FileSyncEx", HandlerPolicy::Submenu),
                ("Legacy", HandlerPolicy::Disabled),
                ("7-Zip", HandlerPolicy::Inline),
            ]
        );
    }

    #[test]
    fn text_listing_shows_verbs_and_handlers() {
        let text = render(&sample_report(), &target());

        assert!(text.starts_with("Target: folder C:\\work\n"));
        assert!(text.contains(
            "cmd        Open command window here  yes                0  cmd.exe /s /k pushd \"%V\""
        ));
        assert!(text.contains("git_shell  Git Bash Here             no                 1  -"));
        assert!(text.contains("COM handlers (config default policy: inline):"));
        assert!(text.contains(
            "FileSyncEx  {CB3D0F55-BC2C-4C1A-85ED-23ED75B5106B}  submenu   C:\\OneDrive\\FileSyncShell64.dll"
        ));
        assert!(text.contains("Legacy      {11111111-1111-1111-1111-111111111111}  disabled  -"));
    }

    #[test]
    fn empty_listing_says_none() {
        let report = HandlersReport::new(&target(), &HandlersConfig::default(), &[], &[]);
        let text = render(&report, &target());
        assert_eq!(text.matches("  none\n").count(), 2);
    }

    #[test]
    fn json_listing_uses_lowercase_policy_names() {
        let json = serde_json::to_value(sample_report()).unwrap();
        assert_eq!(json["target"]["kind"], "folder");
        assert_eq!(json["default_policy"], "inline");
        assert_eq!(json["static_verbs"][0]["verb"], "cmd");
        assert_eq!(json["static_verbs"][0]["shift_only"], true);
        assert_eq!(json["static_verbs"][1]["subitems"], 1);
        assert_eq!(json["com_handlers"][0]["policy"], "submenu");
        assert_eq!(json["com_handlers"][1]["policy"], "disabled");
        assert!(json["com_handlers"][1]["dll_path"].is_null());
    }
}
