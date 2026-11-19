//! How often and how recently each application was launched.
//!
//! The table is small (at most `MAX_ENTRIES` rows), lives in memory while the
//! compositor runs, and is persisted to `$XDG_STATE_HOME/gavno/launcher.json`.
//! Reading happens on the worker that loads the desktop entries and every
//! write runs on a short-lived thread, so the compositor thread never touches
//! the disk.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const MAX_ENTRIES: usize = 200;
/// A launch counts half as much after this many days.
const HALF_LIFE_DAYS: f64 = 14.0;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub count: u32,
    /// Seconds since the Unix epoch of the latest launch.
    pub last: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frecency {
    #[serde(default)]
    apps: HashMap<String, Entry>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `$XDG_STATE_HOME/gavno/launcher.json`, falling back to `~/.local/state`.
pub fn state_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .map(|p| p.join("gavno/launcher.json"))
}

impl Frecency {
    /// A missing or damaged file is an empty table.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map(|mut f| {
                f.prune();
                f
            })
            .unwrap_or_default()
    }

    /// Writes through a temporary file so a crash cannot leave half a table.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Writes a snapshot on its own thread.
    pub fn save_in_background(&self, path: PathBuf) {
        let snapshot = self.clone();
        let spawned = std::thread::Builder::new().name("launcher-state".into()).spawn(move || {
            if let Err(error) = snapshot.save(&path) {
                tracing::warn!(%error, "cannot save the launcher state");
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start the launcher state writer");
        }
    }

    pub fn record(&mut self, id: &str, now: u64) {
        let entry = self.apps.entry(id.to_owned()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last = now;
        if self.apps.len() > MAX_ENTRIES {
            self.prune_at(now);
        }
    }

    /// Launch count weighted by recency; 0 for an unknown app.
    pub fn score(&self, id: &str, now: u64) -> f64 {
        self.apps.get(id).map_or(0.0, |e| {
            let age_days = now.saturating_sub(e.last) as f64 / 86_400.0;
            e.count as f64 * 0.5f64.powf(age_days / HALF_LIFE_DAYS)
        })
    }

    /// The `n` best ids, best first.
    pub fn top(&self, n: usize, now: u64) -> Vec<&str> {
        let mut ids: Vec<(&str, f64)> = self.apps.keys().map(|id| (id.as_str(), self.score(id, now))).collect();
        ids.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ids.into_iter().take(n).map(|(id, _)| id).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.apps.len()
    }

    fn prune(&mut self) {
        self.prune_at(now_secs());
    }

    fn prune_at(&mut self, now: u64) {
        if self.apps.len() <= MAX_ENTRIES {
            return;
        }
        let keep: std::collections::HashSet<String> =
            self.top(MAX_ENTRIES, now).into_iter().map(str::to_owned).collect();
        self.apps.retain(|id, _| keep.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn recent_launches_beat_old_ones() {
        let mut f = Frecency::default();
        for _ in 0..4 {
            f.record("old", 1000);
        }
        f.record("new", 1000 + 60 * DAY);
        let now = 1000 + 60 * DAY;
        assert!(f.score("new", now) > f.score("old", now));
        assert_eq!(f.top(2, now), ["new", "old"]);
        assert_eq!(f.score("missing", now), 0.0);
    }

    #[test]
    fn frequent_launches_beat_single_ones() {
        let mut f = Frecency::default();
        f.record("once", 100);
        f.record("often", 100);
        f.record("often", 100);
        assert_eq!(f.top(5, 100), ["often", "once"]);
    }

    #[test]
    fn the_table_stays_bounded() {
        let mut f = Frecency::default();
        for i in 0..MAX_ENTRIES + 50 {
            f.record(&format!("app{i}"), 10 + i as u64);
        }
        f.record("favourite", 1000);
        f.record("favourite", 1001);
        assert!(f.len() <= MAX_ENTRIES + 1);
        assert_eq!(f.top(1, 1001), ["favourite"]);
    }

    #[test]
    fn persistence_round_trips_and_survives_damage() {
        let dir = std::env::temp_dir().join(format!("gavno-frecency-{}", std::process::id()));
        let path = dir.join("gavno/launcher.json");
        let mut f = Frecency::default();
        f.record("org.example.App", 5);
        f.record("org.example.App", 6);
        f.save(&path).unwrap();
        assert_eq!(Frecency::load(&path), f);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Frecency::load(&path), Frecency::default());
        assert_eq!(Frecency::load(&dir.join("absent.json")), Frecency::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_file_is_pruned_on_load() {
        let dir = std::env::temp_dir().join(format!("gavno-frecency-big-{}", std::process::id()));
        let path = dir.join("launcher.json");
        let mut f = Frecency::default();
        for i in 0..MAX_ENTRIES + 30 {
            f.apps.insert(format!("a{i}"), Entry { count: 1, last: now_secs() });
        }
        f.save(&path).unwrap();
        assert_eq!(Frecency::load(&path).len(), MAX_ENTRIES);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
