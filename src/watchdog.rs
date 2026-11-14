//! A watchdog for a loop that must keep turning, such as the full screen's,
//! which its clock turns twice a second at least, or for the terminal,
//! which must take what the full screen writes. A thread of its own,
//! outside the async runtime, checks every second. When the loop hasn't
//! turned, or a write hasn't ended, for [`STALL`], it logs once what the
//! loop was doing (its stage), what its owner adds (who holds the feed's
//! lock), and what each of the process's threads waits on. It logs again
//! when the loop turns or the write ends. A turn costs an atomic store, and
//! a stage an uncontended lock. It writes to the log file with a handle of
//! its own, so a logger stuck with the rest of the process doesn't keep its
//! lines out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// How long a loop may go without turning before it counts as stalled.
const STALL: Duration = Duration::from_secs(10);
/// How often the watchdog checks.
const EVERY: Duration = Duration::from_secs(1);

/// What a watchdog watches.
#[derive(Clone, Copy)]
pub(crate) enum Watched {
    /// A loop, by the name the log gives it ("The full screen's loop"),
    /// which stalls when it hasn't turned for [`STALL`].
    Loop(&'static str),
    /// The terminal, which stalls when a write to it hasn't ended for
    /// [`STALL`]. It may rest between writes. The watchdog asks how long the
    /// write going on has waited ([`Watchdog::terminal`]).
    Terminal,
}

/// Watches one loop, or the terminal, while it is kept. Dropping it ends
/// the watchdog's thread.
pub(crate) struct Watchdog {
    shared: Arc<Shared>,
}

struct Shared {
    watched: Watched,
    started: Instant,
    /// When the loop last turned, or the latest write started or ended, in
    /// milliseconds since `started`.
    turned: AtomicU64,
    /// The terminal rests, with no write going on.
    resting: AtomicBool,
    stage: Mutex<&'static str>,
    /// What the owner adds to the line about a stall.
    more: Box<dyn Fn() -> String + Send + Sync>,
    /// How long the terminal's write going on has waited, for
    /// [`Watched::Terminal`].
    waiting: Option<Waiting>,
}

/// How long a write going on has waited, if one is.
type Waiting = Box<dyn Fn() -> Option<Duration> + Send + Sync>;

impl Watchdog {
    /// Starts watching `watched`, a loop that turns from now on or the
    /// terminal at rest, with lines added to the log at `log`. `more` says
    /// what else a stall's line should tell.
    pub(crate) fn start(
        watched: Watched,
        log: PathBuf,
        more: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        Self::start_with(watched, log, Box::new(more), None)
    }

    /// Starts watching the terminal, with lines added to the log at `log`.
    /// `waiting` says how long the write going on has waited, if one is.
    pub(crate) fn terminal(
        log: PathBuf,
        waiting: impl Fn() -> Option<Duration> + Send + Sync + 'static,
    ) -> Self {
        let more = Box::new(String::new);
        Self::start_with(Watched::Terminal, log, more, Some(Box::new(waiting)))
    }

    fn start_with(
        watched: Watched,
        log: PathBuf,
        more: Box<dyn Fn() -> String + Send + Sync>,
        waiting: Option<Waiting>,
    ) -> Self {
        let shared = Arc::new(Shared {
            watched,
            started: Instant::now(),
            turned: AtomicU64::new(0),
            resting: AtomicBool::new(matches!(watched, Watched::Terminal)),
            stage: Mutex::new("starting"),
            more,
            waiting,
        });
        let watched = Arc::downgrade(&shared);
        if matches!(shared.watched, Watched::Loop(_)) {
            HERE.with(|here| *here.borrow_mut() = watched.clone());
        }
        // (Without a thread, the loop runs unwatched.)
        let _ = std::thread::Builder::new()
            .name("arko-watchdog".into())
            .spawn(move || watch(&watched, &log));
        Self { shared }
    }

    /// The loop turned once more.
    pub(crate) fn turned(&self) {
        let now = millis(self.shared.started.elapsed());
        self.shared.turned.store(now, Ordering::Relaxed);
    }

    /// What the loop does from now on, for the log should it stall.
    pub(crate) fn stage(&self, stage: &'static str) {
        *crate::lock(&self.shared.stage) = stage;
    }
}

thread_local! {
    /// The watchdog of the loop this thread runs, if any.
    static HERE: std::cell::RefCell<Weak<Shared>> = const { std::cell::RefCell::new(Weak::new()) };
}

/// What the loop this thread runs does from now on, as
/// [`Watchdog::stage`] says it, for code the loop calls (a read of the
/// status). Nothing on a thread that runs no watched loop.
pub(crate) fn stage(stage: &'static str) {
    HERE.with(|here| {
        if let Some(shared) = here.borrow().upgrade() {
            *crate::lock(&shared.stage) = stage;
        }
    });
}

fn watch(watched: &Weak<Shared>, log: &Path) {
    let mut stalled = None;
    let mut resumed = 0;
    let mut woke = Instant::now();
    let mut asleep = computer_asleep();
    // How long the terminal's write had waited at the last check.
    let mut writing: Option<Duration> = None;
    loop {
        std::thread::sleep(EVERY);
        let slept = woke.elapsed();
        woke = Instant::now();
        let before = std::mem::replace(&mut asleep, computer_asleep());
        let Some(shared) = watched.upgrade() else {
            return;
        };
        // (The loop's watchdog says what the process went through, once.)
        let says = matches!(shared.watched, Watched::Loop(_));
        if let Some(waiting) = &shared.waiting {
            note_write(&shared, &mut writing, waiting());
        }
        let computer_slept = asleep.saturating_sub(before);
        if says && computer_slept >= STALL {
            write(
                log,
                &format!("The computer slept for {} s", computer_slept.as_secs()),
            );
        }
        let now = millis(shared.started.elapsed());
        let turned = shared.turned.load(Ordering::Relaxed);
        let resting = shared.resting.load(Ordering::Relaxed);
        let found = check(&mut stalled, &mut resumed, now, turned, slept, resting);
        let since = Duration::from_millis(now.saturating_sub(turned.max(resumed)));
        let line = match (found, shared.watched) {
            (Some(Check::Stopped), Watched::Loop(_)) => format!(
                "Arko didn't run for {} s (its process was stopped)",
                slept.as_secs()
            ),
            (Some(Check::Stalled), watched) => {
                let more = (shared.more)();
                let what = match watched {
                    Watched::Loop(name) => {
                        let stage = *crate::lock(&shared.stage);
                        format!(
                            "{name} hasn't turned for {} s, while {stage}.",
                            since.as_secs()
                        )
                    }
                    Watched::Terminal => format!(
                        "The terminal hasn't taken Arko's output for {} s.",
                        since.as_secs()
                    ),
                };
                [what, more, threads()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            (Some(Check::Turned(after)), Watched::Loop(name)) => {
                format!("{name} turned again, after {} s", after.as_secs())
            }
            (Some(Check::Turned(after)), Watched::Terminal) => format!(
                "The terminal took Arko's output again, after {} s",
                after.as_secs()
            ),
            _ => continue,
        };
        write(log, &line);
    }
}

/// Takes what the terminal's write going on has waited, `now`, as the
/// watchdog of a loop takes its turns. A write that started since the last
/// check (it waited less than the one then) turns it, and so does the end
/// of one. `last` is what the last check found.
fn note_write(shared: &Shared, last: &mut Option<Duration>, now: Option<Duration>) {
    let started = |waited: Duration| millis(shared.started.elapsed().saturating_sub(waited));
    match (*last, now) {
        (Some(before), Some(waited)) if waited >= before => {}
        (_, Some(waited)) => {
            shared.turned.store(started(waited), Ordering::Relaxed);
            shared.resting.store(false, Ordering::Relaxed);
        }
        (Some(_), None) => {
            shared
                .turned
                .store(started(Duration::ZERO), Ordering::Relaxed);
            shared.resting.store(true, Ordering::Relaxed);
        }
        (None, None) => {}
    }
    *last = now;
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// How long the computer has slept since it started. Linux's boot clock
/// counts the time asleep, and its monotonic clock (`Instant`'s) doesn't.
#[cfg(target_os = "linux")]
fn computer_asleep() -> Duration {
    let read = |clock| {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: fills in `time`, which lives through the call.
        unsafe { libc::clock_gettime(clock, &mut time) };
        Duration::new(
            u64::try_from(time.tv_sec).unwrap_or(0),
            u32::try_from(time.tv_nsec).unwrap_or(0),
        )
    };
    read(libc::CLOCK_BOOTTIME).saturating_sub(read(libc::CLOCK_MONOTONIC))
}

/// Elsewhere it isn't measured.
#[cfg(not(target_os = "linux"))]
fn computer_asleep() -> Duration {
    Duration::ZERO
}

/// Adds `text` to the log at `path` as a warning, in the logger's format.
pub(crate) fn write(path: &Path, text: &str) {
    use std::io::Write;
    let line = format!(
        "[{} WARN  arko::watchdog] {text}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(line.as_bytes());
    }
}

/// What a check finds worth logging.
#[derive(Debug, PartialEq)]
enum Check {
    /// The watchdog itself didn't run, so neither did the process.
    Stopped,
    /// The loop hasn't turned for [`STALL`].
    Stalled,
    /// The loop that stalled turned again, after this long.
    Turned(Duration),
}

/// A check at `now` of a loop that last turned at `turned` (milliseconds
/// since the watchdog started), by a watchdog that slept `slept` since its
/// last check. `stalled` is the turn a stall was logged at, until the loop
/// turns again. `resumed` is when the process ran again after it was
/// stopped, which a stall counts from, as the loop couldn't turn before. A
/// loop that is `resting` (the terminal between writes) doesn't stall.
fn check(
    stalled: &mut Option<u64>,
    resumed: &mut u64,
    now: u64,
    turned: u64,
    slept: Duration,
    resting: bool,
) -> Option<Check> {
    if slept >= STALL {
        *stalled = None;
        *resumed = now;
        return Some(Check::Stopped);
    }
    let since = Duration::from_millis(now.saturating_sub(turned.max(*resumed)));
    match *stalled {
        None if since >= STALL && !resting => {
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
        let (mut stalled, mut resumed) = (None, 0);
        let mut check =
            |now, turned, slept| check(&mut stalled, &mut resumed, now, turned, slept, false);
        assert_eq!(check(10_000, 1_000, SECOND), None);
        assert_eq!(check(11_000, 1_000, SECOND), Some(Check::Stalled));
        assert_eq!(check(31_000, 1_000, SECOND), None);
        assert_eq!(
            check(32_000, 31_000, SECOND),
            Some(Check::Turned(30 * SECOND))
        );
        assert_eq!(check(36_000, 31_000, SECOND), None);
        assert_eq!(check(76_000, 31_000, 40 * SECOND), Some(Check::Stopped));
        assert_eq!(stalled, None);
    }

    /// After the process was stopped, a loop that hasn't turned yet stalls
    /// only once it hasn't turned for the stall's time since the process
    /// ran again.
    #[test]
    fn a_stop_isnt_taken_for_a_stall() {
        let (mut stalled, mut resumed) = (None, 0);
        let mut check =
            |now, turned, slept| check(&mut stalled, &mut resumed, now, turned, slept, false);
        assert_eq!(check(60_000, 39_500, 20 * SECOND), Some(Check::Stopped));
        assert_eq!(check(61_000, 39_500, SECOND), None);
        assert_eq!(check(61_500, 61_200, SECOND / 2), None);
        // One that stays stuck after it is a stall.
        assert_eq!(check(71_000, 61_200, SECOND), None);
        assert_eq!(check(72_000, 61_200, SECOND), Some(Check::Stalled));
    }

    /// The terminal may rest between writes for as long as it likes, and a
    /// write that hasn't ended for the stall's time is logged, and its end.
    #[test]
    fn a_write_that_doesnt_end_is_a_stall_and_rest_isnt() {
        let (mut stalled, mut resumed) = (None, 0);
        let mut check =
            |now, turned, resting| check(&mut stalled, &mut resumed, now, turned, SECOND, resting);
        assert_eq!(check(60_000, 0, true), None);
        assert_eq!(check(70_000, 60_000, false), Some(Check::Stalled));
        assert_eq!(
            check(76_000, 75_000, true),
            Some(Check::Turned(15 * SECOND))
        );
        assert_eq!(check(105_000, 75_000, true), None);
    }

    /// The terminal's watchdog takes a write's start from how long it has
    /// waited, so one write that goes on keeps its start, a new one counts
    /// from its own, and the end of one is a turn.
    #[test]
    fn a_write_counts_from_its_start() {
        let shared = Shared {
            watched: Watched::Terminal,
            started: Instant::now() - 100 * SECOND,
            turned: AtomicU64::new(0),
            resting: AtomicBool::new(true),
            stage: Mutex::new("starting"),
            more: Box::new(String::new),
            waiting: None,
        };
        let turned = |shared: &Shared| shared.turned.load(Ordering::Relaxed);
        let resting = |shared: &Shared| shared.resting.load(Ordering::Relaxed);
        let mut last = None;
        note_write(&shared, &mut last, Some(30 * SECOND));
        let first = turned(&shared);
        assert!((69_000..=71_000).contains(&first), "{first}");
        assert!(!resting(&shared));
        note_write(&shared, &mut last, Some(31 * SECOND));
        assert_eq!(turned(&shared), first, "the same write");
        note_write(&shared, &mut last, Some(SECOND));
        let second = turned(&shared);
        assert!(second > first + 20_000, "a new write, {second}");
        note_write(&shared, &mut last, None);
        assert!(resting(&shared));
        assert!(turned(&shared) >= second);
    }

    /// The computer's sleep is measured where the clocks tell it, and is
    /// never negative.
    #[test]
    fn the_computers_sleep_is_measured() {
        let first = computer_asleep();
        assert!(computer_asleep() >= first);
    }

    /// A line goes to the end of the log, as the logger writes its own.
    #[test]
    fn a_line_is_added_to_the_log() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("arko.log");
        std::fs::write(&path, "[earlier line]\n").unwrap();
        write(&path, "The loop turned again, after 12 s");
        let text = std::fs::read_to_string(&path).unwrap();
        let last = text.lines().last().unwrap();
        assert!(text.starts_with("[earlier line]\n"), "{text}");
        assert!(
            last.starts_with('[')
                && last.ends_with(" WARN  arko::watchdog] The loop turned again, after 12 s"),
            "{last}"
        );
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
