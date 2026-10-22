//! What a session remembers of its prompt cache between runs of Arko, so a
//! resumed session counts down the time the cache has left (or says it
//! expired) from the clock on the wall, not from when Arko started.
//!
//! The cache belongs to a conversation, so the record is kept for each main
//! agent run (`prompt-cache.json` in the session's folder): which model made
//! the request, when it went out (Unix milliseconds, since Arko was gone
//! meanwhile), how long the provider keeps the cache from then, and what it
//! read and wrote. It is written as each of the main agent's requests ends
//! and when the cache is kept warm, never while a request is still running,
//! so a request that was running when Arko stopped counts from the last one
//! that finished. That can only say the cache will lapse sooner than it
//! does, never later. A session saved before the record existed has none,
//! and an older Arko that resumes a session with one ignores it.
//!
//! What a record may not claim is checked as the session opens (see
//! [`restore`]): the model must be the one that made the request, and the
//! cache lifetime the one it was made with. A request in the conversation
//! the session is in now is also needed, since a handover, a compaction
//! and a cut-back start a conversation the cache doesn't hold.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lock;
use crate::ui::Stats;

/// The record's file, in the session's folder.
const FILE: &str = "prompt-cache.json";

/// The format of the record. A file of another one is left unread.
const VERSION: u32 = 1;

/// The most conversations the record keeps: the ones used last.
const MOST: usize = 64;

/// The prompt cache one main agent run's latest request left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The main agent's run.
    pub run: Uuid,
    /// The model that made the request, as the provider names it.
    pub model: String,
    /// When the request went out, or the refresh that kept the cache warm
    /// (Unix milliseconds). The cache lasts `lifetime` from then.
    pub at: i64,
    /// How long the provider keeps the cache after a use, in seconds.
    pub lifetime: u64,
    /// What the request read from the cache and wrote to it, in tokens.
    #[serde(default)]
    pub read: usize,
    #[serde(default)]
    pub written: usize,
}

/// The record's file as it is saved.
#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    #[serde(default)]
    conversations: Vec<Entry>,
}

/// The session's record, kept in memory and in its folder.
#[derive(Debug)]
pub(crate) struct Memory {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
}

impl Memory {
    /// The record in the session's folder `dir`. One that is missing, damaged
    /// or of another format is an empty one: the session starts without a
    /// cache status, as before the record existed.
    pub(crate) fn open(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Saved>(&text) {
                Ok(saved) if saved.version == VERSION => saved.conversations,
                Ok(saved) => {
                    log::debug!(
                        "left {} unread, its format is {}",
                        path.display(),
                        saved.version
                    );
                    Vec::new()
                }
                Err(error) => {
                    log::warn!("could not read {} ({error})", path.display());
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    /// What the record keeps of the conversation whose runs are `runs`: the
    /// latest of their entries.
    pub(crate) fn find(&self, runs: &[Uuid]) -> Option<Entry> {
        lock(&self.entries)
            .iter()
            .filter(|entry| runs.contains(&entry.run))
            .max_by_key(|entry| entry.at)
            .cloned()
    }

    /// Keeps `entry` in place of its run's, and saves the record.
    pub(crate) fn put(&self, entry: Entry) {
        let mut entries = lock(&self.entries);
        entries.retain(|kept| kept.run != entry.run);
        entries.push(entry);
        if entries.len() > MOST {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.at));
            entries.truncate(MOST);
        }
        self.save(&entries);
    }

    /// Takes `run`'s entry away (its conversation was replaced), and saves
    /// the record.
    pub(crate) fn forget(&self, run: Uuid) {
        let mut entries = lock(&self.entries);
        let before = entries.len();
        entries.retain(|kept| kept.run != run);
        if entries.len() != before {
            self.save(&entries);
        }
    }

    /// Writes the record whole (through a file beside it, renamed into place,
    /// so a reader finds the old record or the new). The write is small and
    /// comes as a request ends, never with each token.
    fn save(&self, entries: &[Entry]) {
        let saved = Saved {
            version: VERSION,
            conversations: entries.to_vec(),
        };
        let written = serde_json::to_vec_pretty(&saved)
            .map_err(std::io::Error::other)
            .and_then(|text| crate::session::write_whole(&self.path, &text));
        if let Err(error) = written {
            log::warn!(
                "could not keep the prompt cache's time in {} ({error})",
                self.path.display()
            );
        }
    }
}

/// What the status shows of a cache that was restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Restored {
    /// When the request went out (Unix milliseconds), never later than now.
    pub at: i64,
    pub lifetime: Duration,
    pub read: usize,
    pub written: usize,
}

/// What the session is now, which a record has to match to be restored.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Basis<'a> {
    /// The main agent's model.
    pub model: &'a str,
    /// How long its provider keeps the cache, when it says. Claude Code's
    /// shows in its cache writes (`learns`), which the record kept.
    pub lifetime: Option<Duration>,
    pub learns: bool,
    /// The clock now (Unix milliseconds).
    pub now: i64,
}

/// The cache `entry` left, as the session can show it now, or `None` where
/// it can't be trusted: another model made the request, or the lifetime it
/// was made with isn't the one the session counts. A cache whose time is
/// out restores too, which then says it expired. A request dated in the
/// future (the clock went back) counts from now.
pub(crate) fn restore(entry: &Entry, basis: Basis) -> Option<Restored> {
    if entry.model != basis.model || entry.lifetime == 0 {
        return None;
    }
    let lifetime = Duration::from_secs(entry.lifetime);
    if !basis.learns && basis.lifetime != Some(lifetime) {
        return None;
    }
    Some(Restored {
        at: entry.at.min(basis.now),
        lifetime,
        read: entry.read,
        written: entry.written,
    })
}

impl crate::Harness {
    /// Shows the prompt cache of the conversation now current as the
    /// session's record has it (none for a new conversation). `measured` is
    /// whether the conversation has a request in its current context, which
    /// a handover, a compaction or a cut-back leaves it without until the
    /// next one. Called as a session opens and as `/resume` changes the
    /// conversation.
    pub(crate) fn restore_cache(&self, measured: bool) {
        let runs: Vec<Uuid> = self
            .conversations
            .current
            .and_then(|current| self.conversations.list.get(current))
            .map(|record| record.runs.clone())
            .unwrap_or_default();
        let mut stats = lock(&self.stats);
        let entry = stats
            .cache_memory
            .as_ref()
            .filter(|_| measured)
            .and_then(|memory| memory.find(&runs));
        let restored = entry
            .zip(stats.main_model.as_deref())
            .and_then(|(entry, model)| {
                restore(
                    &entry,
                    Basis {
                        model,
                        lifetime: stats.cache_lifetime,
                        learns: stats.learns_cache_lifetime,
                        now: chrono::Utc::now().timestamp_millis(),
                    },
                )
            });
        stats.set_cache(restored);
    }
}

impl Stats {
    /// The record's entry for the cache as the main agent's request in `run`
    /// left it, once it ended. `None` where nothing is cached or its
    /// lifetime isn't known.
    pub(crate) fn cache_entry(&self, run: Uuid) -> Option<Entry> {
        let (read, written) = self.cache_use.unwrap_or_default();
        Some(Entry {
            run,
            model: self.main_model.clone()?,
            at: self.cache_refreshed_at?,
            lifetime: self.cache_lifetime?.as_secs(),
            read,
            written,
        })
    }

    /// Shows the cache `restored` (the conversation now current's), or none.
    pub(crate) fn set_cache(&mut self, restored: Option<Restored>) {
        match restored {
            Some(restored) => {
                self.cache_refreshed_at = Some(restored.at);
                self.cache_use = Some((restored.read, restored.written));
                if self.learns_cache_lifetime {
                    self.cache_lifetime = Some(restored.lifetime);
                }
            }
            None => {
                self.cache_refreshed_at = None;
                self.cache_use = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn entry(at: i64) -> Entry {
        Entry {
            run: Uuid::from_u128(1),
            model: "claude-opus-5-5".into(),
            at,
            lifetime: 3600,
            read: 170_000,
            written: 10_000,
        }
    }

    fn basis(now: i64) -> Basis<'static> {
        Basis {
            model: "claude-opus-5-5",
            lifetime: Some(HOUR),
            learns: false,
            now,
        }
    }

    /// What was saved comes back, in a new `Memory` of the same folder,
    /// and a run's entry replaces its earlier one.
    #[test]
    fn the_record_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let memory = Memory::open(temp.path());
        assert_eq!(memory.find(&[Uuid::from_u128(1)]), None);
        memory.put(entry(1_000));
        memory.put(entry(2_000));
        let other = Entry {
            run: Uuid::from_u128(2),
            ..entry(1_500)
        };
        memory.put(other.clone());
        let reopened = Memory::open(temp.path());
        assert_eq!(reopened.find(&[Uuid::from_u128(1)]), Some(entry(2_000)));
        assert_eq!(reopened.find(&[Uuid::from_u128(2)]), Some(other));
        // The conversation's latest run's entry, among its runs.
        let both = [Uuid::from_u128(1), Uuid::from_u128(2)];
        assert_eq!(reopened.find(&both), Some(entry(2_000)));
        reopened.forget(Uuid::from_u128(1));
        assert_eq!(Memory::open(temp.path()).find(&[Uuid::from_u128(1)]), None);
        // (No temporary file is left behind.)
        let names: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|file| file.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [FILE]);
    }

    /// A session saved before the record existed has none, one that is
    /// damaged or empty counts as none, and a format that isn't this one is
    /// left unread. A later save replaces any of them.
    #[test]
    fn a_missing_damaged_or_older_record_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let run = [Uuid::from_u128(1)];
        assert_eq!(Memory::open(temp.path()).find(&run), None);
        let path = temp.path().join(FILE);
        for text in [
            "",
            "{",
            "[]",
            "null",
            r#"{"version":1,"conversations":[{"run":"nope"}]}"#,
            r#"{"conversations":[]}"#,
            r#"{"version":0,"conversations":[]}"#,
            // A newer format, whose entries this one can't judge.
            &format!(
                r#"{{"version":2,"conversations":[{}]}}"#,
                serde_json::to_string(&entry(5)).unwrap()
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(Memory::open(temp.path()).find(&run), None, "{text:?}");
        }
        let memory = Memory::open(temp.path());
        memory.put(entry(7));
        assert_eq!(Memory::open(temp.path()).find(&run), Some(entry(7)));
    }

    /// Fields a later Arko adds are ignored, and the counts an earlier
    /// record lacked are zero.
    #[test]
    fn unknown_fields_are_ignored_and_the_counts_default() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!(
            r#"{{"version":1,"later":true,"conversations":[{{"run":"{}","model":"m","at":9,"lifetime":300,"more":1}}]}}"#,
            Uuid::from_u128(1)
        );
        std::fs::write(temp.path().join(FILE), text).unwrap();
        let found = Memory::open(temp.path())
            .find(&[Uuid::from_u128(1)])
            .unwrap();
        assert_eq!(
            (found.at, found.lifetime, found.read, found.written),
            (9, 300, 0, 0)
        );
    }

    /// The record keeps the conversations used last.
    #[test]
    fn the_record_stays_small() {
        let temp = tempfile::tempdir().unwrap();
        let memory = Memory::open(temp.path());
        for n in 0..(MOST as u128 + 10) {
            memory.put(Entry {
                run: Uuid::from_u128(n + 1),
                ..entry(n as i64)
            });
        }
        let reopened = Memory::open(temp.path());
        assert_eq!(reopened.entries.lock().unwrap().len(), MOST);
        assert!(reopened.find(&[Uuid::from_u128(1)]).is_none());
        assert!(
            reopened
                .find(&[Uuid::from_u128(MOST as u128 + 10)])
                .is_some()
        );
    }

    /// A cache still valid keeps the time it was made, and one that lapsed
    /// while Arko was closed restores too, to say so.
    #[test]
    fn a_cache_restores_valid_or_expired() {
        let at = 1_000_000;
        let valid = restore(&entry(at), basis(at + 20 * 60_000)).unwrap();
        assert_eq!(valid.at, at);
        assert_eq!(valid.lifetime, HOUR);
        assert_eq!((valid.read, valid.written), (170_000, 10_000));
        let expired = restore(&entry(at), basis(at + 90 * 60_000)).unwrap();
        assert_eq!(expired.at, at);
    }

    /// A clock that went back dates the request in the future: it counts
    /// from now, so the time left is never more than the lifetime.
    #[test]
    fn a_request_from_the_future_counts_from_now() {
        let restored = restore(&entry(5_000_000), basis(1_000_000)).unwrap();
        assert_eq!(restored.at, 1_000_000);
    }

    /// Another model's cache is not this one's, and neither is one with
    /// another lifetime (5 minutes against the 1 hour now counted, say).
    #[test]
    fn a_cache_of_another_model_or_lifetime_is_not_restored() {
        let at = 1_000;
        let other = Basis {
            model: "claude-sonnet-5-5",
            ..basis(at)
        };
        assert_eq!(restore(&entry(at), other), None);
        let five = Basis {
            lifetime: Some(Duration::from_secs(300)),
            ..basis(at)
        };
        assert_eq!(restore(&entry(at), five), None);
        // The provider's lifetime is unknown now (a model without one).
        let unknown = Basis {
            lifetime: None,
            ..basis(at)
        };
        assert_eq!(restore(&entry(at), unknown), None);
        let none = Entry {
            lifetime: 0,
            ..entry(at)
        };
        assert_eq!(restore(&none, basis(at)), None);
    }

    /// Claude Code's lifetime is learned from its cache writes, which the
    /// record kept: it restores where the session hasn't learned it yet.
    #[test]
    fn a_learned_lifetime_restores() {
        let learns = Basis {
            lifetime: None,
            learns: true,
            ..basis(10)
        };
        let restored = restore(&entry(5), learns).unwrap();
        assert_eq!(restored.lifetime, HOUR);
        let mut stats = Stats {
            learns_cache_lifetime: true,
            ..Stats::default()
        };
        stats.set_cache(Some(restored));
        assert_eq!(stats.cache_lifetime, Some(HOUR));
        assert_eq!(stats.cache_refreshed_at, Some(5));
        assert_eq!(stats.cache_use, Some((170_000, 10_000)));
        stats.set_cache(None);
        assert_eq!((stats.cache_refreshed_at, stats.cache_use), (None, None));
        // (The lifetime stays: the next request writes a cache of that
        // length.)
        assert_eq!(stats.cache_lifetime, Some(HOUR));
    }

    /// What the monitors plan from, the cache's expiry, is the restored time
    /// plus the lifetime: in the past for a cache that lapsed (the monitors
    /// take a time that passed as now), and never more than the lifetime
    /// from now.
    #[test]
    fn a_restored_cache_gives_the_monitors_its_expiry() {
        let at = 5_000_000;
        let expiry = |entry_at: i64, now: i64| {
            let restored = restore(&entry(entry_at), basis(now)).unwrap();
            let mut stats = Stats {
                cache_lifetime: Some(HOUR),
                ..Stats::default()
            };
            stats.set_cache(Some(restored));
            crate::harness::cache_expiry(&stats).unwrap()
        };
        let millis = |time: std::time::SystemTime| {
            time.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        };
        assert_eq!(millis(expiry(at, at + 1_000)), at + 3_600_000);
        assert_eq!(millis(expiry(at, at + 7_200_000)), at + 3_600_000);
        assert_eq!(millis(expiry(at + 9_000_000, at)), at + 3_600_000);
    }

    /// The entry kept for a request holds what the status knows, and none
    /// where the lifetime or the time isn't known.
    #[test]
    fn the_entry_holds_what_the_status_knows() {
        let run = Uuid::from_u128(9);
        let mut stats = Stats {
            main_model: Some("m".into()),
            ..Stats::default()
        };
        assert_eq!(stats.cache_entry(run), None);
        stats.cache_refreshed_at = Some(42);
        assert_eq!(stats.cache_entry(run), None, "no lifetime");
        stats.cache_lifetime = Some(Duration::from_secs(300));
        stats.cache_use = Some((7, 8));
        let kept = stats.cache_entry(run).unwrap();
        assert_eq!(
            kept,
            Entry {
                run,
                model: "m".into(),
                at: 42,
                lifetime: 300,
                read: 7,
                written: 8
            }
        );
        stats.main_model = None;
        assert_eq!(stats.cache_entry(run), None, "no model");
    }
}
