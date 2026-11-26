//! On-disk cache for complete price days.
//!
//! A day is only stored once its series covers the whole day. Entries carry no expiry
//! of their own: the service keeps a day that has ended as it is and rechecks one
//! that has not after a while, falling back to the entry if the check fails, so
//! cached days keep working offline. Incomplete days and live grid data are never
//! cached. `--refresh` bypasses and overwrites an entry.

use std::path::PathBuf;

use jiff::{Timestamp, civil::Date};
use serde::{Deserialize, Serialize};

use crate::model::Market;

const VERSION: u32 = 1;

/// A complete day of prices as stored on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    version: u32,
    pub url: String,
    pub fetched_at: Timestamp,
    pub published_at: Option<Timestamp>,
    /// `(start, end, eur_per_mwh)`
    pub slots: Vec<(Timestamp, Timestamp, f64)>,
}

impl Entry {
    pub fn new(
        url: String,
        fetched_at: Timestamp,
        published_at: Option<Timestamp>,
        slots: Vec<(Timestamp, Timestamp, f64)>,
    ) -> Self {
        Self {
            version: VERSION,
            url,
            fetched_at,
            published_at,
            slots,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `VATIO_CACHE_DIR` when set; otherwise `%LOCALAPPDATA%\\vatio\\cache` on Windows and
    /// `$XDG_CACHE_HOME/vatio` or `~/.cache/vatio` everywhere else (macOS included, as
    /// is usual for command-line tools). `None` when no home directory is known.
    pub fn default_location() -> Option<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        if let Some(dir) = var("VATIO_CACHE_DIR") {
            return Some(Self::at(dir));
        }
        if cfg!(windows) {
            return var("LOCALAPPDATA").map(|dir| Self::at(dir.join("vatio").join("cache")));
        }
        var("XDG_CACHE_HOME")
            .or_else(|| var("HOME").map(|home| home.join(".cache")))
            .map(|dir| Self::at(dir.join("vatio")))
    }

    fn path(&self, market: Market, day: Date) -> PathBuf {
        self.dir
            .join(format!("prices-{}-{day}.json", market.slug()))
    }

    /// A stored day, or `None` if absent, unreadable or written by another version.
    pub fn get(&self, market: Market, day: Date) -> Option<Entry> {
        let bytes = std::fs::read(self.path(market, day)).ok()?;
        let entry: Entry = serde_json::from_slice(&bytes).ok()?;
        (entry.version == VERSION && !entry.slots.is_empty()).then_some(entry)
    }

    /// Store a day. Best effort: a read-only or missing cache directory is not an error.
    pub fn put(&self, market: Market, day: Date, entry: &Entry) {
        let path = self.path(market, day);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let written = std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(entry).unwrap_or_default()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        let t = |s: &str| s.parse::<Timestamp>().unwrap();
        Entry::new(
            "http://x/y".into(),
            t("2026-11-25T10:00:00Z"),
            Some(t("2026-11-24T19:46:06Z")),
            vec![(t("2026-11-24T23:00:00Z"), t("2026-11-25T00:00:00Z"), 166.11)],
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vatio-cache-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_and_keys_by_market_and_day() {
        let dir = temp_dir("roundtrip");
        let cache = Cache::at(&dir);
        let day: Date = "2026-11-25".parse().unwrap();
        assert_eq!(cache.get(Market::Pvpc, day), None);
        cache.put(Market::Pvpc, day, &entry());
        assert_eq!(cache.get(Market::Pvpc, day), Some(entry()));
        assert_eq!(cache.get(Market::Spot, day), None);
        assert_eq!(cache.get(Market::Pvpc, day.tomorrow().unwrap()), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_or_foreign_files_are_ignored() {
        let dir = temp_dir("corrupt");
        let cache = Cache::at(&dir);
        let day: Date = "2026-11-25".parse().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(cache.path(Market::Pvpc, day), b"{ not json").unwrap();
        assert_eq!(cache.get(Market::Pvpc, day), None);
        let mut other = entry();
        other.version = 99;
        std::fs::write(
            cache.path(Market::Pvpc, day),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();
        assert_eq!(cache.get(Market::Pvpc, day), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unwritable_location_is_silently_skipped() {
        // A directory cannot be created underneath a regular file on any platform.
        let file = temp_dir("unwritable");
        std::fs::write(&file, b"not a directory").unwrap();
        let cache = Cache::at(file.join("cache"));
        let day: Date = "2026-11-25".parse().unwrap();
        cache.put(Market::Pvpc, day, &entry());
        assert_eq!(cache.get(Market::Pvpc, day), None);
        std::fs::remove_file(file).unwrap();
    }
}
