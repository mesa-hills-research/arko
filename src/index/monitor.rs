//! Watchers: arko_index's inotify watcher (`watch2`, Linux) or its polling watcher
//! (any system, and the fallback where inotify fails), behind one small trait.
//! A watcher keeps a durable state of the tree (its own directory, beside the
//! index's), so opening one reconciles what changed while no watcher ran; each
//! change is a delta with a sequence number, acknowledged once an index build
//! covers it. Only one watcher may hold a state directory: another process's
//! watcher makes opening fail with [`OpenError::Busy`].

use std::path::Path;
use std::time::{Duration, Instant};

use arko_index::FileRules;
use arko_index::bench_v1::{self, Options};

use super::store::Layout;

/// A source of change notifications for one tree.
pub trait Changes: Send {
    /// The sequence of the next delta, waiting up to `wait` for one.
    fn poll(&mut self, wait: Duration) -> Result<Option<u64>, String>;
    /// Acknowledges every delta up to `sequence`, durably.
    fn ack(&mut self, sequence: u64) -> Result<(), String>;
    /// Whether this is the polling watcher (which rereads the tree on each poll).
    fn polling(&self) -> bool;
    /// How long the last poll took to read the tree (polling watcher), to space
    /// polls of a large tree further apart.
    fn check_cost(&self) -> Duration {
        Duration::ZERO
    }
}

/// Why a watcher didn't open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// Another watcher (another Arko process) holds the state.
    Busy,
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Busy => f.write_str("another process is watching this index"),
            OpenError::Failed(error) => f.write_str(error),
        }
    }
}

fn open_error(error: bench_v1::Error) -> OpenError {
    let text = error.to_string();
    if text.contains("already in use") {
        OpenError::Busy
    } else {
        OpenError::Failed(text)
    }
}

/// The polling watcher. `poll` checks once (it rereads and hashes the tree, so the
/// caller spaces the calls) and waits for nothing: a change is delivered by the
/// first check that finds the tree as the previous one did.
pub struct Polling {
    watcher: bench_v1::Watcher,
    last_check: Duration,
}

impl Changes for Polling {
    fn poll(&mut self, _wait: Duration) -> Result<Option<u64>, String> {
        let began = Instant::now();
        let delta = self
            .watcher
            .next_change(Duration::ZERO)
            .map_err(|error| error.to_string());
        self.last_check = began.elapsed();
        Ok(delta?.map(|delta| delta.sequence))
    }
    fn ack(&mut self, sequence: u64) -> Result<(), String> {
        self.watcher
            .ack(sequence)
            .map_err(|error| error.to_string())
    }
    fn polling(&self) -> bool {
        true
    }
    fn check_cost(&self) -> Duration {
        self.last_check
    }
}

#[cfg(target_os = "linux")]
pub struct Inotify {
    watcher: arko_index::watch2::Watcher2,
}

#[cfg(target_os = "linux")]
impl Changes for Inotify {
    fn poll(&mut self, wait: Duration) -> Result<Option<u64>, String> {
        // The delta's chunks are written to a sink: only its sequence is used.
        self.watcher
            .next_change_to(wait, &mut std::io::sink())
            .map_err(|error| error.to_string())
    }
    fn ack(&mut self, sequence: u64) -> Result<(), String> {
        self.watcher
            .ack(sequence)
            .map_err(|error| error.to_string())
    }
    fn polling(&self) -> bool {
        false
    }
}

fn options(docs: bool) -> Options {
    Options {
        front_matter_description: docs,
        ..Options::default()
    }
}

pub fn open_polling(
    root: &Path,
    layout: &Layout,
    rules: &FileRules,
    docs: bool,
) -> Result<Box<dyn Changes>, OpenError> {
    let watcher = bench_v1::open_watch_with(root, &layout.watch(), &options(docs), rules)
        .map_err(open_error)?;
    Ok(Box::new(Polling {
        watcher,
        last_check: Duration::ZERO,
    }))
}

/// Opens the index's watcher: inotify on Linux (unless `force_polling`), falling
/// back to polling when inotify fails for another reason than a busy state.
pub fn open_changes(
    root: &Path,
    layout: &Layout,
    rules: &FileRules,
    docs: bool,
    force_polling: bool,
) -> Result<Box<dyn Changes>, OpenError> {
    #[cfg(target_os = "linux")]
    if !force_polling {
        match arko_index::watch2::Watcher2::open_with(root, &layout.watch2(), &options(docs), rules)
        {
            Ok(watcher) => return Ok(Box::new(Inotify { watcher })),
            Err(error) => match open_error(error) {
                OpenError::Busy => return Err(OpenError::Busy),
                OpenError::Failed(error) => {
                    log::warn!(
                        "index: inotify watcher for {} failed ({error}), polling instead",
                        root.display()
                    );
                }
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = force_polling;
    open_polling(root, layout, rules, docs)
}

/// Whether another process's watcher holds the index's state, found by trying
/// the state's lock without opening a watcher or creating anything. A watcher
/// of this process that holds it counts too, so ask for an index this process
/// doesn't watch.
pub fn watched_elsewhere(layout: &Layout) -> bool {
    // The lock file arko_index's watchers hold in their state directory.
    const LOCK: &str = ".arko-index-state";
    let states = [
        #[cfg(target_os = "linux")]
        layout.watch2(),
        layout.watch(),
    ];
    states.iter().any(|state| {
        std::fs::File::open(state.join(LOCK)).is_ok_and(|file| {
            matches!(
                file.try_lock_shared(),
                Err(std::fs::TryLockError::WouldBlock)
            )
        })
    })
}

/// Every delta a freshly opened watcher has (what changed since its state was
/// last acknowledged): the last sequence, if any.
pub fn drain(changes: &mut dyn Changes) -> Result<Option<u64>, String> {
    let mut last = None;
    // A polling watcher delivers a change on its second look at the tree.
    let looks = if changes.polling() { 2 } else { 1 };
    for look in 0..looks {
        if look > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        while let Some(sequence) = changes.poll(Duration::ZERO)? {
            last = Some(sequence);
        }
    }
    Ok(last)
}
