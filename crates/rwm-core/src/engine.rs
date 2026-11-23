use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::Globalization::GetUserDefaultUILanguage;

use crate::com_shell::{
    benchmark_target, clipboard_has_files, execute_core_verb, execute_custom_command,
    execute_static_command_in, ComStaWorker, ShellError,
};
use crate::config::{CustomMenuItem, CustomPosition, RwmConfig};
use crate::icons::IconCache;
use crate::model::{
    parse_mnemonic_label, BenchmarkReport, CoreVerb, EntryAction, EntrySource, HandlerPolicy,
    HydrationEvent, MenuEntry, MenuItem, MenuSnapshot, SelectionTarget,
};
use crate::registry::{DiscoveredComHandler, RegistryVerbCache, StaticVerb};

pub struct FastMenuEngine {
    config: Arc<RwmConfig>,
    registry: Arc<RegistryVerbCache>,
    icons: Arc<IconCache>,
    worker: ComStaWorker,
    next_session: AtomicU64,
    japanese: bool,
}

impl FastMenuEngine {
    pub fn new(config: Arc<RwmConfig>, with_registry_watcher: bool) -> Self {
        Self {
            config,
            registry: Arc::new(RegistryVerbCache::new(with_registry_watcher)),
            icons: Arc::new(IconCache::new()),
            worker: ComStaWorker::new(),
            next_session: AtomicU64::new(1),
            japanese: is_japanese(),
        }
    }

    pub fn config(&self) -> &Arc<RwmConfig> {
        &self.config
    }

    pub fn registry(&self) -> &Arc<RegistryVerbCache> {
        &self.registry
    }

    pub fn icons(&self) -> &Arc<IconCache> {
        &self.icons
    }

    pub fn build_instant_snapshot(
        &self,
        target: SelectionTarget,
        shift_held: bool,
    ) -> MenuSnapshot {
        snapshot(
            self.next_session.fetch_add(1, Ordering::Relaxed),
            target,
            shift_held,
            self.japanese,
            &self.config,
            &self.registry,
            &self.icons,
        )
        .0
    }

    pub fn start_hydration(
        &self,
        session_id: u64,
        target: SelectionTarget,
        notify_hwnd: isize,
        notify_msg: u32,
    ) -> mpsc::Receiver<HydrationEvent> {
        let handlers = if self.config.behavior.enable_async_com_hydration {
            filter_hydration_handlers(self.registry.collect_for_target(&target).1, &self.config)
        } else {
            Vec::new()
        };
        self.worker.start_hydration(
            session_id,
            target,
            handlers,
            Arc::clone(&self.config),
            notify_hwnd,
            notify_msg,
        )
    }

    pub fn dispatch_action(
        &self,
        action: &EntryAction,
        target: &SelectionTarget,
        owner_hwnd: HWND,
    ) -> Result<(), ShellError> {
        match action {
            EntryAction::CoreVerb(verb) => {
                execute_core_verb(*verb, target, owner_hwnd, &self.config)
            }
            EntryAction::StaticCommand {
                verb,
                command,
                working_dir,
            } => execute_static_command_in(verb, command, target, owner_hwnd, working_dir),
            EntryAction::CustomCommand {
                command,
                args,
                working_dir,
            } => execute_custom_command(command, args, working_dir, target),
            EntryAction::ComCommand {
                session_id,
                handler_index,
                cmd_offset,
            } => self.worker.invoke(
                *session_id,
                *handler_index,
                *cmd_offset,
                owner_hwnd.0 as isize,
                Some(target.parent_dir()),
            ),
            EntryAction::Submenu(_) | EntryAction::Noop => Ok(()),
        }
    }

    pub fn close_session(&self, session_id: u64) {
        self.worker.close_session(session_id);
    }

    pub fn shutdown(&self) {
        self.registry.stop_watcher();
        self.worker.shutdown();
    }

    pub fn request_shutdown(&self) {
        self.registry.request_stop_watcher();
        self.worker.request_shutdown();
    }

    pub fn benchmark(&self, target: &SelectionTarget) -> Result<BenchmarkReport, ShellError> {
        benchmark_target(target, &self.config, &self.registry, &self.icons)
    }
}

pub(crate) fn is_japanese() -> bool {
    unsafe { GetUserDefaultUILanguage() == 0x0411 }
}

const COPY_AS_PATH_MENU_CLSID: &str = "{F3D06E7C-1E45-4A26-847E-F9FCDEE59BE0}";

fn filter_hydration_handlers(
    handlers: Vec<DiscoveredComHandler>,
    config: &RwmConfig,
) -> Vec<DiscoveredComHandler> {
    handlers
        .into_iter()
        .filter(|handler| {
            // Core always exposes CopyPath, so skip Explorer's built-in CopyAsPathMenu
            // unless the user explicitly configured an override for it.
            let is_copy_as_path = handler
                .clsid
                .trim()
                .eq_ignore_ascii_case(COPY_AS_PATH_MENU_CLSID);
            if !is_copy_as_path {
                return true;
            }
            config.handlers.overrides.keys().any(|k| {
                let kt = k.trim();
                kt.eq_ignore_ascii_case(&handler.clsid)
                    || (!handler.name.is_empty() && kt.eq_ignore_ascii_case(&handler.name))
            })
        })
        .collect()
}

pub(crate) fn snapshot(
    session_id: u64,
    target: SelectionTarget,
    shift_held: bool,
    japanese: bool,
    config: &RwmConfig,
    registry: &RegistryVerbCache,
    icons: &IconCache,
) -> (MenuSnapshot, Vec<DiscoveredComHandler>) {
    let start = Instant::now();
    let (verbs, handlers) = registry.collect_for_target(&target);
    let pending_handlers = if config.behavior.enable_async_com_hydration {
        filter_hydration_handlers(handlers.clone(), config)
            .iter()
            .filter(|h| config.handlers.policy_for(&h.clsid, &h.name) != HandlerPolicy::Disabled)
            .count()
    } else {
        0
    };
    let working_dir = target.parent_dir();
    let mut core = core_entries(&target, japanese, config.appearance.show_shortcuts);
    let mut properties_group = Vec::new();
    if let Some(pos) = core.iter().position(|entry| {
        matches!(
            entry,
            MenuEntry::Item(item) if matches!(item.action, EntryAction::CoreVerb(CoreVerb::Properties))
        )
    }) {
        properties_group.push(core.remove(pos));
    }

    let mut top = Vec::new();
    let mut bottom = Vec::new();
    let mut next_id = core.len() as u32 + 2;
    let mut static_entries = Vec::new();
    for verb in verbs {
        let position = verb
            .position
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if let Some(entry) =
            static_entry(verb, shift_held, &working_dir, config, icons, &mut next_id)
        {
            match position.as_str() {
                "top" => top.push(entry),
                "bottom" => bottom.push(entry),
                _ => static_entries.push(entry),
            }
        }
    }

    let extension = target.extension_lower();
    for item in &config.custom_items {
        if !custom_matches(item, &target, extension.as_deref()) {
            continue;
        }
        let (label, mnemonic) = parse_mnemonic_label(&item.label);
        let entry = MenuEntry::Item(MenuItem {
            id: next_id,
            label,
            mnemonic,
            shortcut_hint: None,
            icon: entry_icon(config, icons, item.icon.as_deref()),
            enabled: true,
            checked: false,
            is_default: false,
            source: EntrySource::Custom,
            action: EntryAction::CustomCommand {
                command: item.command.clone(),
                args: item.args.clone(),
                working_dir: working_dir.clone(),
            },
        });
        next_id += 1;
        match item.position {
            CustomPosition::Top => top.push(entry),
            CustomPosition::Bottom => bottom.push(entry),
        }
    }

    let mut entries = Vec::new();
    for group in [
        &mut top,
        &mut core,
        &mut static_entries,
        &mut bottom,
        &mut properties_group,
    ] {
        if !group.is_empty() {
            if !entries.is_empty() {
                entries.push(MenuEntry::Separator);
            }
            entries.append(group);
        }
    }

    (
        MenuSnapshot {
            session_id,
            target,
            entries,
            stage0_us: start.elapsed().as_micros() as u64,
            pending_handlers,
        },
        handlers,
    )
}

fn entry_icon(
    config: &RwmConfig,
    icons: &IconCache,
    spec: Option<&str>,
) -> Option<Arc<crate::model::BgraIcon>> {
    spec.filter(|_| config.appearance.show_icons)
        .and_then(|spec| icons.load_icon_spec(spec, 16))
}

fn static_entry(
    verb: StaticVerb,
    shift_held: bool,
    working_dir: &Path,
    config: &RwmConfig,
    icons: &IconCache,
    next_id: &mut u32,
) -> Option<MenuEntry> {
    let canonical = verb.canonical_verb.as_deref().unwrap_or(&verb.key_name);
    if (verb.extended_only && !shift_held)
        || [canonical, &verb.key_name].iter().any(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "open"
                    | "cut"
                    | "copy"
                    | "paste"
                    | "delete"
                    | "rename"
                    | "properties"
                    | "printto"
                    | "openas"
                    | "runasuser"
            )
        })
    {
        return None;
    }

    let is_submenu = !verb.subcommands.is_empty();
    let children: Vec<_> = verb
        .subcommands
        .into_iter()
        .filter_map(|child| static_entry(child, shift_held, working_dir, config, icons, next_id))
        .collect();
    let action = if !children.is_empty() {
        EntryAction::Submenu(children)
    } else if is_submenu && verb.command.is_none() {
        return None;
    } else {
        EntryAction::StaticCommand {
            verb: canonical.to_owned(),
            command: verb.command.unwrap_or_default(),
            working_dir: working_dir.to_path_buf(),
        }
    };
    let id = *next_id;
    *next_id += 1;
    Some(MenuEntry::Item(MenuItem {
        id,
        label: verb.label,
        mnemonic: verb.mnemonic,
        shortcut_hint: None,
        icon: entry_icon(config, icons, verb.icon_spec.as_deref()),
        enabled: true,
        checked: false,
        is_default: false,
        source: EntrySource::StaticVerb,
        action,
    }))
}

fn custom_matches(
    item: &CustomMenuItem,
    target: &SelectionTarget,
    extension: Option<&str>,
) -> bool {
    let scope_matches = item.scopes.is_empty()
        || item
            .scopes
            .iter()
            .any(|scope| match scope.trim().to_ascii_lowercase().as_str() {
                "all" | "*" => true,
                "file" => {
                    matches!(target, SelectionTarget::SingleFile(_))
                        || matches!(target, SelectionTarget::MultipleItems { items, .. }
                        if !items.is_empty() && items.iter().all(|path| !path.is_dir()))
                }
                "folder" => {
                    matches!(
                        target,
                        SelectionTarget::SingleFolder(_) | SelectionTarget::Drive(_)
                    ) || matches!(target, SelectionTarget::MultipleItems { items, .. }
                        if !items.is_empty() && items.iter().all(|path| path.is_dir()))
                }
                "background" => target.is_background(),
                "desktop" => matches!(target, SelectionTarget::DesktopBackground(_)),
                "drive" => matches!(target, SelectionTarget::Drive(_)),
                "multiple" => matches!(target, SelectionTarget::MultipleItems { .. }),
                _ => false,
            });
    scope_matches
        && (item.extensions.is_empty()
            || extension.is_some_and(|extension| {
                item.extensions.iter().any(|ext| {
                    ext.trim()
                        .trim_start_matches(['*', '.'])
                        .eq_ignore_ascii_case(&extension[1..])
                })
            }))
}

fn core_entries(target: &SelectionTarget, japanese: bool, show_shortcuts: bool) -> Vec<MenuEntry> {
    use CoreVerb::*;

    let background = target.is_background();
    let drive = matches!(target, SelectionTarget::Drive(_));
    let folder = matches!(target, SelectionTarget::SingleFolder(_));
    let has_selection = !background;
    let can_edit = has_selection && !drive;
    let can_paste = background || drive || folder;
    let paste_enabled = can_paste && clipboard_has_files();
    let verbs = [
        (Open, "Open", "開く", Some("Enter"), has_selection, true),
        (
            OpenInTerminal,
            "Open in Terminal",
            "ターミナルで開く",
            None,
            true,
            true,
        ),
        (Cut, "Cut", "切り取り", Some("Ctrl+X"), can_edit, true),
        (Copy, "Copy", "コピー", Some("Ctrl+C"), can_edit, true),
        (
            CopyPath,
            "Copy as path",
            "パスのコピー",
            Some("Ctrl+Shift+C"),
            true,
            true,
        ),
        (
            Paste,
            "Paste",
            "貼り付け",
            Some("Ctrl+V"),
            can_paste,
            paste_enabled,
        ),
        (
            PasteShortcut,
            "Paste shortcut",
            "ショートカットの貼り付け",
            None,
            can_paste,
            paste_enabled,
        ),
        (
            CreateShortcut,
            "Create shortcut",
            "ショートカットの作成",
            None,
            can_edit,
            true,
        ),
        (Delete, "Delete", "削除", Some("Del"), can_edit, true),
        (Rename, "Rename", "名前の変更", Some("F2"), can_edit, true),
        (
            Refresh,
            "Refresh",
            "最新の情報に更新",
            Some("F5"),
            background,
            true,
        ),
        (
            Properties,
            "Properties",
            "プロパティ",
            Some("Alt+Enter"),
            true,
            true,
        ),
    ];
    verbs
        .into_iter()
        .filter(|(_, _, _, _, visible, _)| *visible)
        .enumerate()
        .map(|(index, (verb, en, ja, shortcut, _, enabled))| {
            MenuEntry::Item(MenuItem {
                id: index as u32 + 1,
                label: if japanese { ja } else { en }.to_owned(),
                mnemonic: None,
                shortcut_hint: shortcut.filter(|_| show_shortcuts).map(str::to_owned),
                icon: None,
                enabled,
                checked: false,
                is_default: verb == Open,
                source: EntrySource::Core,
                action: EntryAction::CoreVerb(verb),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_target(extension: &str) -> SelectionTarget {
        SelectionTarget::SingleFile(std::env::temp_dir().join(format!("rwm-test.{extension}")))
    }

    #[test]
    fn warm_stage_zero_builds_file_and_background_in_under_15_ms() {
        let engine = FastMenuEngine::new(Arc::new(RwmConfig::default()), false);
        let windows = std::env::var_os("SystemRoot").expect("SystemRoot");
        let windows = std::path::PathBuf::from(windows);
        for target in [
            SelectionTarget::SingleFile(windows.join("notepad.exe")),
            SelectionTarget::FolderBackground(windows),
        ] {
            let _ = engine.build_instant_snapshot(target.clone(), false);
            let mut timings = Vec::new();
            for _ in 0..32 {
                let result = engine.build_instant_snapshot(target.clone(), false);
                assert!(!result.entries.is_empty());
                assert_eq!(result.target, target);
                timings.push(result.stage0_us);
            }
            timings.sort_unstable();
            let max = timings[31];
            eprintln!(
                "{}: Stage 0 min={} median={} max={max} µs (32 warm snapshots)",
                target.primary_path().display(),
                timings[0],
                timings[16],
            );
            assert!(max < 15_000, "warm Stage 0 took {max} µs");
        }
    }

    #[test]
    fn localized_core_labels_and_dedup_use_canonical_verbs() {
        let target = file_target("txt");
        let config = RwmConfig::default();
        let icons = IconCache::new();
        for (japanese, expected) in [(false, "Open"), (true, "開く")] {
            let entries = core_entries(&target, japanese, true);
            assert!(matches!(
                &entries[0],
                MenuEntry::Item(item)
                    if item.label == expected && matches!(item.action, EntryAction::CoreVerb(CoreVerb::Open))
            ));
        }

        let mut next_id = 1;
        for canonical in [
            "OPEN",
            "Cut",
            "copy",
            "paste",
            "Delete",
            "Rename",
            "Properties",
            "printto",
            "openas",
            "runasuser",
        ] {
            let duplicate = StaticVerb {
                key_name: "Alias".into(),
                label: "Alternate label".into(),
                mnemonic: None,
                icon_spec: None,
                command: Some("test.exe".into()),
                extended_only: false,
                subcommands: Vec::new(),
                canonical_verb: Some(canonical.into()),
                position: None,
            };
            assert!(static_entry(
                duplicate,
                false,
                &target.parent_dir(),
                &config,
                &icons,
                &mut next_id
            )
            .is_none());
        }

        let engine = FastMenuEngine::new(Arc::new(config), false);
        let menu = engine.build_instant_snapshot(target, false);
        assert!(matches!(
            &menu.entries[0],
            MenuEntry::Item(item)
                if item.label == if is_japanese() { "開く" } else { "Open" }
        ));
    }

    #[test]
    fn matches_custom_items_and_preserves_positions_and_shift_filter() {
        let item = CustomMenuItem {
            label: "Top text action".into(),
            command: "test.exe".into(),
            scopes: vec!["file".into()],
            extensions: vec!["*.TXT".into()],
            ..Default::default()
        };
        let config = Arc::new(RwmConfig {
            custom_items: vec![
                item.clone(),
                CustomMenuItem {
                    label: "Bottom text action".into(),
                    position: CustomPosition::Bottom,
                    ..item
                },
            ],
            ..Default::default()
        });
        let engine = FastMenuEngine::new(Arc::clone(&config), false);
        let target = file_target("txt");
        let menu = engine.build_instant_snapshot(target.clone(), true);
        assert!(
            matches!(&menu.entries[0], MenuEntry::Item(item) if item.label == "Top text action")
        );
        let bottom_idx = menu
            .entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Item(item) if item.label == "Bottom text action"))
            .expect("bottom custom item present");
        assert!(
            matches!(
                menu.entries.last(),
                Some(MenuEntry::Item(item))
                    if matches!(item.action, EntryAction::CoreVerb(CoreVerb::Properties))
            ),
            "Properties must stay anchored as the final entry"
        );
        assert!(bottom_idx < menu.entries.len() - 1);
        let other = engine.build_instant_snapshot(file_target("bin"), true);
        assert!(!other.entries.iter().any(|entry| {
            matches!(entry, MenuEntry::Item(item) if item.source == EntrySource::Custom)
        }));

        let extended = StaticVerb {
            key_name: "ExtendedDemo".into(),
            label: "Extended demo".into(),
            mnemonic: None,
            icon_spec: None,
            command: Some("test.exe".into()),
            extended_only: true,
            subcommands: Vec::new(),
            canonical_verb: None,
            position: None,
        };
        let mut id = 1;
        assert!(static_entry(
            extended.clone(),
            false,
            &target.parent_dir(),
            &config,
            engine.icons(),
            &mut id
        )
        .is_none());
        assert!(static_entry(
            extended,
            true,
            &target.parent_dir(),
            &config,
            engine.icons(),
            &mut id
        )
        .is_some());
    }
}
