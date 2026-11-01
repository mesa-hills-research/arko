//! A watchdog for a loop that must keep turning, such as the full screen's,
//! which its clock turns twice a second at least. A thread of its own,
//! outside the async runtime, checks the loop every second. When the loop
//! hasn't turned for [`STALL`], it logs once what the loop was doing (its
//! stage), what its owner adds (who holds the feed's lock), and what each of
//! the process's threads waits on. It logs again when the loop turns. A
//! turn costs an atomic store, and a stage an uncontended lock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// How long a loop may go without turning before it counts as stalled.
const STALL: Duration = Duration::from_secs(10);
/// How often the watchdog checks.
const EVERY: Duration = Duration::from_secs(1);

/// Watches one loop while it is kept. Dropping it ends the watchdog's thread.
pub(crate) struct Watchdog {
    shared: Arc<Shared>,
}

struct Shared {
    /// The loop, as the log names it ("The full screen's loop").
    name: &'static str,
    started: Instant,
    /// When the loop last turned, in milliseconds since `started`.
    turned: AtomicU64,
    stage: Mutex<&'static str>,
    /// What the owner adds to the line about a stall.
    more: Box<dyn Fn() -> String + Send + Sync>,
}

impl Watchdog {
    /// Starts watching the loop `name`, which turns from now on. `more` says
    /// what else a stall's line should tell.
    pub(crate) fn start(
        name: &'static str,
        more: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            name,
            started: Instant::now(),
            turned: AtomicU64::new(0),
            stage: Mutex::new("starting"),
            more: Box::new(more),
        });
        let watched = Arc::downgrade(&shared);
        // (Without a thread, the loop runs unwatched.)
        let _ = std::thread::Builder::new()
            .name("arko-watchdog".into())
            .spawn(move || watch(&watched));
        Self { shared }
    }

    /// The loop turned once more.
    pub(crate) fn turned(&self) {
        let now = self.shared.started.elapsed().as_millis();
        self.shared
            .turned
            .store(u64::try_from(now).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// What the loop does from now on, for the log should it stall.
    pub(crate) fn stage(&self, stage: &'static str) {
        *crate::lock(&self.shared.stage) = stage;
    }
}

fn watch(watched: &Weak<Shared>) {
    let mut stalled = None;
    let mut woke = Instant::now();
    loop {
        std::thread::sleep(EVERY);
        let slept = woke.elapsed();
        woke = Instant::now();
        let Some(shared) = watched.upgrade() else {
            return;
        };
        let turned = shared.turned.load(Ordering::Relaxed);
        let since = shared
            .started
            .elapsed()
            .saturating_sub(Duration::from_millis(turned));
        let name = shared.name;
        match check(&mut stalled, turned, since, slept) {
            Some(Check::Slept) => log::warn!(
                "Arko didn't run for {} s (its process was stopped, or the computer slept)",
                slept.as_secs()
            ),
            Some(Check::Stalled) => {
                let stage = *crate::lock(&shared.stage);
                log::warn!(
                    "{name} hasn't turned for {} s, while {stage}. {} {}",
                    since.as_secs(),
                    (shared.more)(),
                    threads()
                );
            }
            Some(Check::Turned(after)) => {
                log::warn!("{name} turned again, after {} s", after.as_secs());
            }
            None => {}
        }
    }
}

/// What a check finds worth logging.
#[derive(Debug, PartialEq)]
enum Check {
    /// The watchdog itself didn't run, so neither did the process.
    Slept,
    /// The loop hasn't turned for [`STALL`].
    Stalled,
    /// The loop that stalled turned again, after this long.
    Turned(Duration),
}

/// A check of a loop that last turned at `turned` (milliseconds), `since`
/// ago, by a watchdog that slept `slept` since its last check. `stalled` is
/// the turn a stall was logged at, until the loop turns again.
fn check(
    stalled: &mut Option<u64>,
    turned: u64,
    since: Duration,
    slept: Duration,
) -> Option<Check> {
    if slept >= STALL {
        *stalled = None;
        return Some(Check::Slept);
    }
    match *stalled {
        None if since >= STALL => {
            *stalled = Some(turned);
            Some(Check::Stalled)
        }
        Some(at) if at != turned => {
            *stalled = None;
            Some(Check::Turned(Duration::from_millis(
                turned.saturating_sub(at),
            )))
        }
        _ => None,
    }
}

/// The process's threads by what they wait on, counted (`tokio-runtime-w S
/// futex_do_wait ×6`), from `/proc`, which shows a lock (`futex`), a write
/// to the terminal (`n_tty_write`), a wait for input (`ep_poll`).
#[cfg(target_os = "linux")]
fn threads() -> String {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return String::new();
    };
    let read = |path: std::path::PathBuf| {
        std::fs::read_to_string(path)
            .map(|text| text.trim().to_string())
            .unwrap_or_default()
    };
    let mut counted: Vec<(String, usize)> = Vec::new();
    for task in tasks.flatten() {
        let path = task.path();
        let stat = read(path.join("stat"));
        // The state follows the name, which is in parentheses.
        let state = stat
            .rfind(')')
            .and_then(|at| stat[at + 1..].split_whitespace().next())
            .unwrap_or("?")
            .to_string();
        let line = format!(
            "{} {state} {}",
            read(path.join("comm")),
            read(path.join("wchan"))
        );
        match counted.iter_mut().find(|(seen, _)| *seen == line) {
            Some((_, count)) => *count += 1,
            None => counted.push((line, 1)),
        }
    }
    counted.sort();
    let list: Vec<String> = counted
        .into_iter()
        .map(|(line, count)| match count {
            1 => line,
            count => format!("{line} ×{count}"),
        })
        .collect();
    format!("Arko's threads are {}.", list.join(", "))
}

#[cfg(not(target_os = "linux"))]
fn threads() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    /// A loop that stops turning is logged once, and again when it turns,
    /// and a pause of the whole process isn't taken for a stall.
    #[test]
    fn a_stall_is_logged_once_and_its_end_too() {
        let mut stalled = None;
        assert_eq!(check(&mut stalled, 1_000, 9 * SECOND, SECOND), None);
        assert_eq!(
            check(&mut stalled, 1_000, 10 * SECOND, SECOND),
            Some(Check::Stalled)
        );
        assert_eq!(check(&mut stalled, 1_000, 30 * SECOND, SECOND), None);
        assert_eq!(
            check(&mut stalled, 31_000, SECOND, SECOND),
            Some(Check::Turned(30 * SECOND))
        );
        assert_eq!(check(&mut stalled, 31_000, 5 * SECOND, SECOND), None);
        assert_eq!(
            check(&mut stalled, 31_000, 40 * SECOND, 40 * SECOND),
            Some(Check::Slept)
        );
        assert_eq!(stalled, None);
    }

    /// The threads are listed with what they wait on, this one among them.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_threads_are_listed() {
        let listed = threads();
        assert!(listed.starts_with("Arko's threads are "), "{listed}");
        assert!(listed.contains(" R ") || listed.contains(" S "), "{listed}");
    }
}
