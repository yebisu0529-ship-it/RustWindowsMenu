//! A per-thread view of `config.toml` that touches the file system at most once a second.

use std::fs;
use std::time::{Duration, Instant, SystemTime};

use rwm_core::config::{config_path, is_kill_switch_active, RwmConfig};

const RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Identifies one version of the file: its modification time, length, and kill-switch state.
type Stamp = (Option<(SystemTime, u64)>, bool);

#[derive(Default)]
pub(crate) struct ConfigCache {
    config: RwmConfig,
    file_config: RwmConfig,
    stamp: Option<Stamp>,
    checked_at: Option<Instant>,
}

impl ConfigCache {
    pub(crate) fn current(&mut self, now: Instant) -> &RwmConfig {
        let stamp = || {
            let file_stamp = fs::metadata(config_path())
                .ok()
                .and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
            (file_stamp, is_kill_switch_active())
        };
        self.refresh(now, stamp, RwmConfig::load_or_default)
    }

    /// Reloads when the stamp or kill-switch state changes. A file that fails to load keeps
    /// the previous config until the stamp changes again; a missing file means the defaults.
    fn refresh<E>(
        &mut self,
        now: Instant,
        stamp: impl FnOnce() -> Stamp,
        load: impl FnOnce() -> Result<RwmConfig, E>,
    ) -> &RwmConfig {
        let due = |at: Instant| now.saturating_duration_since(at) >= RECHECK_INTERVAL;
        if self.checked_at.is_none_or(due) {
            self.checked_at = Some(now);
            let stamp = stamp();
            if Some(&stamp) != self.stamp.as_ref() {
                let prev_file = self.stamp.as_ref().and_then(|s| s.0);
                self.stamp = Some(stamp);
                if stamp.0 != prev_file {
                    if let Ok(config) = load() {
                        self.file_config = config;
                    }
                } else if stamp.0.is_none() {
                    self.file_config = RwmConfig::default();
                }
                self.config = self.file_config.clone();
                if stamp.1 {
                    self.config.enabled = false;
                }
            }
        }
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(seconds: u64) -> Stamp {
        (
            Some((SystemTime::UNIX_EPOCH + Duration::from_secs(seconds), 64)),
            false,
        )
    }

    fn killed(seconds: u64) -> Stamp {
        (
            Some((SystemTime::UNIX_EPOCH + Duration::from_secs(seconds), 64)),
            true,
        )
    }

    fn enabled(enabled: bool) -> Result<RwmConfig, ()> {
        Ok(RwmConfig {
            enabled,
            ..RwmConfig::default()
        })
    }

    fn untouched<T>() -> T {
        panic!("the file is not read")
    }

    #[test]
    fn checks_the_file_at_most_once_a_second_and_reloads_only_edits() {
        let (mut cache, start) = (ConfigCache::default(), Instant::now());
        assert!(
            cache
                .refresh(start, || (None, false), || Ok::<_, ()>(RwmConfig::default()))
                .enabled
        );
        assert!(
            !cache
                .refresh(start + RECHECK_INTERVAL, || stamp(1), || enabled(false))
                .enabled
        );
        let soon = start + RECHECK_INTERVAL + Duration::from_millis(999);
        assert!(!cache.refresh(soon, || unreachable!(), || enabled(true)).enabled);
        let unchanged = start + RECHECK_INTERVAL * 2;
        assert!(
            !cache
                .refresh(unchanged, || stamp(1), untouched::<Result<RwmConfig, ()>>)
                .enabled
        );
        assert!(
            cache
                .refresh(start + RECHECK_INTERVAL * 3, || stamp(2), || enabled(true))
                .enabled
        );
    }

    #[test]
    fn a_broken_edit_keeps_the_last_good_config_and_a_deleted_file_restores_defaults() {
        let (mut cache, start) = (ConfigCache::default(), Instant::now());
        cache.refresh(start, || stamp(1), || enabled(false));
        assert!(
            !cache
                .refresh(start + RECHECK_INTERVAL, || stamp(2), || Err(()))
                .enabled
        );
        let retry = start + RECHECK_INTERVAL * 2;
        assert!(
            !cache
                .refresh(retry, || stamp(2), untouched::<Result<RwmConfig, ()>>)
                .enabled
        );
        let defaults = || Ok::<_, ()>(RwmConfig::default());
        let deleted = cache.refresh(start + RECHECK_INTERVAL * 3, || (None, false), defaults);
        assert_eq!(*deleted, RwmConfig::default());
    }

    #[test]
    fn kill_switch_disables_and_removal_restores_config() {
        let (mut cache, start) = (ConfigCache::default(), Instant::now());
        assert!(cache.refresh(start, || stamp(1), || enabled(true)).enabled);
        assert!(!cache
            .refresh(
                start + RECHECK_INTERVAL,
                || killed(1),
                untouched::<Result<RwmConfig, ()>>,
            )
            .enabled);
        assert!(cache
            .refresh(
                start + RECHECK_INTERVAL * 2,
                || stamp(1),
                untouched::<Result<RwmConfig, ()>>,
            )
            .enabled);
    }
}
