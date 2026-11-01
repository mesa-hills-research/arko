//! The full screen's writer, the one owner of the terminal while the view
//! has it. What the full screen sends the terminal (its frames, the title,
//! the bell, a copy, the mouse's mode) goes through it in order, and a
//! thread of its own writes it. A terminal that stops taking output then
//! holds up that thread alone. Were the write on one of the async runtime's
//! threads, it could hold up the runtime's IO and timers, and with them the
//! whole session. The view draws a frame only once the writer has written
//! the last one, so a terminal that takes output again gets the changes
//! since the last frame it took.
//!
//! Giving the terminal back is one step, [`Writer::finish`]. It ends the
//! view's output, so nothing the view writes after it reaches the terminal
//! (a frame, the flush of a backend dropped by a panic), and queues the
//! reset once, as a packet whose end in the output is recorded then and
//! never moves. Only the writer's thread marks it written, once its bytes
//! were. What follows the view (the reply) goes after it, and each
//! [`Ticket`] that [`Writer::wait`] waits on is written only when the reset
//! was and its own bytes were. A failure never counts as written. An end
//! that can't count on the writer takes the reset over
//! ([`Writer::take_over`]) and writes it itself. A writer whose thread
//! didn't start fails to start, and one whose thread ended says so
//! ([`Writer::failure`]) and wakes those waiting.
//!
//! Its state is behind one lock, held for no write, no wait but its own
//! Condvar's, and no call out. Those waiting are woken after it's let go.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::watchdog::{Watchdog, Watched};

/// A handle on the writer, for the view. What is written to it waits in the
/// handle, and a flush hands it to the writer's thread, after what the
/// other handles flushed before, until the view finished. The thread ends
/// when the last handle is dropped.
pub(super) struct Writer {
    shared: Arc<Shared>,
    buffer: Vec<u8>,
}

struct Shared {
    state: Mutex<State>,
    /// Rings for the writer's thread when there is something to write, and
    /// for those waiting for it to be written.
    changed: Condvar,
    /// Rings for the view when everything handed over has been written, or
    /// the writer failed.
    written: Notify,
}

struct State {
    /// What waits to be written.
    pending: Vec<u8>,
    /// How many bytes the write going on writes (none when it's zero).
    writing: usize,
    /// When the write going on started.
    started: Option<Instant>,
    /// How many bytes were queued so far, written or not. It only grows.
    queued: u64,
    /// How many of those were written, or dropped as the view's when it
    /// finished. The rest are being written or wait.
    settled: u64,
    /// What gives the terminal back.
    reset: Vec<u8>,
    phase: Phase,
    /// An end that can't count on the writer writes the reset itself, so
    /// the writer writes nothing more.
    taken_over: bool,
    failed: Option<Failure>,
}

/// Where giving the terminal back is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// The view writes.
    Open,
    /// The view finished, and the reset ends at this many bytes queued.
    Finishing(u64),
    /// The writer wrote the reset.
    Reset,
}

impl State {
    fn queue(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        self.queued += bytes.len() as u64;
    }
}

/// Why the writer writes no more.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Failure {
    /// The terminal refused a write, because it closed.
    Gone,
    /// The writer's thread ended while the view had the terminal.
    Died,
}

/// What [`Writer::finish`] queued, for [`Writer::wait`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Ticket(u64);

/// What became of a [`Ticket`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Finish {
    /// The writer wrote it.
    Written,
    /// It won't be, as the writer failed.
    Failed,
    /// Not yet.
    TimedOut,
}

/// What an end that can't count on the writer does about the terminal
/// ([`Writer::take_over`]).
#[derive(Debug, PartialEq)]
pub(super) enum Reset {
    /// Nothing, as the writer wrote the reset, or an earlier end took it over.
    Done,
    /// Writes these bytes.
    Needed(Vec<u8>),
    /// It couldn't tell, so it writes a reset of its own.
    Unknown,
}

/// The name of the writer's thread.
pub(super) const THREAD: &str = "arko-screen";

/// How long the thread waits for output before it checks whether any handle
/// is left.
const IDLE_CHECK: Duration = Duration::from_secs(1);

impl Writer {
    /// Starts the writer's thread, which writes to `out`, watched for writes
    /// that don't end, with the watchdog's lines in the log at `log`.
    /// `reset` gives the terminal back. Fails when the thread doesn't start.
    pub(super) fn start(
        out: impl Write + Send + 'static,
        log: PathBuf,
        reset: Vec<u8>,
    ) -> io::Result<Self> {
        Self::start_with(out, log, reset, |run| {
            std::thread::Builder::new()
                .name(THREAD.into())
                .spawn(run)
                .map(drop)
        })
    }

    /// [`start`](Self::start), with the thread started by `spawn`.
    fn start_with(
        out: impl Write + Send + 'static,
        log: PathBuf,
        reset: Vec<u8>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: Vec::new(),
                writing: 0,
                started: None,
                queued: 0,
                settled: 0,
                reset,
                phase: Phase::Open,
                taken_over: false,
                failed: None,
            }),
            changed: Condvar::new(),
            written: Notify::new(),
        });
        let watched = Arc::downgrade(&shared);
        let watchdog = Watchdog::start(Watched::Terminal, log, String::new);
        spawn(Box::new(move || {
            let _ended = Ended(watched.clone());
            write_all(&watched, out, &watchdog);
        }))?;
        Ok(Self {
            shared,
            buffer: Vec::new(),
        })
    }

    /// Another handle on the same writer.
    pub(super) fn handle(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            buffer: Vec::new(),
        }
    }

    /// Whether what was handed over waits to be written, or is being
    /// written (the terminal hasn't taken it yet).
    pub(super) fn busy(&self) -> bool {
        let state = crate::lock(&self.shared.state);
        state.failed.is_none() && (state.writing > 0 || !state.pending.is_empty())
    }

    /// Why the writer writes no more, if it doesn't.
    pub(super) fn failure(&self) -> Option<Failure> {
        crate::lock(&self.shared.state).failed
    }

    /// How long the write going on has waited for the terminal, if one does.
    pub(super) fn waiting(&self) -> Option<Duration> {
        let started = crate::lock(&self.shared.state).started;
        started.map(|started| started.elapsed())
    }

    /// Waits until everything handed over is written, or the writer failed,
    /// for at most `timeout`. Whether it was.
    #[cfg(test)]
    pub(super) fn wait_written(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut state = crate::lock(&self.shared.state);
        while state.failed.is_none() && (state.writing > 0 || !state.pending.is_empty()) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.wait_for_change(state, left);
        }
        true
    }

    /// Waits until everything handed over is written. It may return early,
    /// so check [`busy`](Self::busy) again.
    pub(super) async fn written(&self) {
        self.shared.written.notified().await;
    }

    /// Ends the view's output and gives the terminal back. What the view's
    /// handles wrote and no write has started on is dropped, and the reset
    /// is queued, with where it ends recorded for good. Then `tail` (what
    /// follows the view). Its handles write nothing from now on. Once more,
    /// it queues only `tail`. Returns the ticket for this call's bytes, and
    /// whether this call ended the view.
    pub(super) fn finish(&self, tail: &[u8]) -> (Ticket, bool) {
        let (ticket, ended) = {
            let mut state = crate::lock(&self.shared.state);
            let ended = state.phase == Phase::Open;
            if ended {
                // (The view's bytes no write started on count as settled.)
                state.settled += state.pending.len() as u64;
                state.pending.clear();
                let reset = state.reset.clone();
                state.queue(&reset);
                state.phase = Phase::Finishing(state.queued);
            }
            state.queue(tail);
            (Ticket(state.queued), ended)
        };
        self.shared.changed.notify_all();
        (ticket, ended)
    }

    /// Waits at most `timeout` for the writer to write the reset and what
    /// `ticket` was given for.
    pub(super) fn wait(&self, ticket: Ticket, timeout: Duration) -> Finish {
        let until = Instant::now() + timeout;
        let mut state = crate::lock(&self.shared.state);
        loop {
            if state.phase == Phase::Reset && state.settled >= ticket.0 {
                return Finish::Written;
            }
            if state.failed.is_some() || state.taken_over {
                return Finish::Failed;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Finish::TimedOut;
            }
            state = self.wait_for_change(state, left);
        }
    }

    /// For an end that can't count on the writer (a stuck runtime, a failed
    /// writer): what it does about the terminal. Unless the writer wrote the
    /// reset, or an earlier end took it over, it ends the view's output and
    /// the writer's, and returns the reset to write. It takes the lock only
    /// if it gets it within `within`.
    pub(super) fn take_over(&self, within: Duration) -> Reset {
        let Some(mut state) = try_lock_for(&self.shared.state, within) else {
            return Reset::Unknown;
        };
        if state.phase == Phase::Reset || state.taken_over {
            return Reset::Done;
        }
        state.taken_over = true;
        state.pending.clear();
        Reset::Needed(state.reset.clone())
    }

    fn wait_for_change<'a>(
        &self,
        state: MutexGuard<'a, State>,
        left: Duration,
    ) -> MutexGuard<'a, State> {
        self.shared
            .changed
            .wait_timeout(state, left)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let taken = {
            let mut state = crate::lock(&self.shared.state);
            let taken = state.failed.is_none() && state.phase == Phase::Open && !state.taken_over;
            if taken {
                let bytes = std::mem::take(&mut self.buffer);
                state.queue(&bytes);
            }
            taken
        };
        if taken {
            self.shared.changed.notify_all();
        } else {
            // (The view finished, or the writer failed.)
            self.buffer.clear();
        }
        Ok(())
    }
}

/// Marks the writer failed, and wakes those waiting for it, when its thread
/// ends while a handle is left (a panic). Once no handle is left, no one
/// waits.
struct Ended(Weak<Shared>);

impl Drop for Ended {
    fn drop(&mut self) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        {
            let mut state = crate::lock(&shared.state);
            state.failed.get_or_insert(Failure::Died);
            state.writing = 0;
            state.started = None;
            state.pending.clear();
        }
        shared.written.notify_one();
        shared.changed.notify_all();
    }
}

/// The writer's thread. It writes what is handed over to `out`, all that
/// waits at once, until no handle is left.
fn write_all(shared: &Weak<Shared>, mut out: impl Write, watchdog: &Watchdog) {
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let bytes = {
            let mut state = crate::lock(&shared.state);
            if state.pending.is_empty() || state.failed.is_some() || state.taken_over {
                state.pending.clear();
                // (Let go of the writer meanwhile, so the last handle dropped
                // ends the thread.)
                let state = shared
                    .changed
                    .wait_timeout(state, IDLE_CHECK)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
                drop(state);
                continue;
            }
            let bytes = std::mem::take(&mut state.pending);
            state.writing = bytes.len();
            state.started = Some(Instant::now());
            bytes
        };
        watchdog.writing();
        let wrote = out.write_all(&bytes).and_then(|()| out.flush());
        watchdog.written();
        let done = {
            let mut state = crate::lock(&shared.state);
            state.writing = 0;
            state.started = None;
            match wrote {
                Ok(()) => {
                    state.settled += bytes.len() as u64;
                    // (Only here does the reset count as written.)
                    if let Phase::Finishing(end) = state.phase
                        && state.settled >= end
                    {
                        state.phase = Phase::Reset;
                    }
                }
                Err(_) => {
                    state.failed.get_or_insert(Failure::Gone);
                    state.pending.clear();
                }
            }
            state.pending.is_empty()
        };
        // (Told with the state let go, as the view's waker may take locks of
        // its runtime's.)
        if done {
            shared.written.notify_one();
        }
        shared.changed.notify_all();
    }
}

/// The lock of `mutex`, if it can be had within `within`. std's Mutex has
/// no timed lock, so it tries again every millisecond.
pub(super) fn try_lock_for<T>(mutex: &Mutex<T>, within: Duration) -> Option<MutexGuard<'_, T>> {
    let until = Instant::now() + within;
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}
/// Where the writer writes, standard output. On Unix, through a descriptor
/// of its own, so a write that waits for the terminal holds no lock that a
/// print elsewhere would wait for.
#[allow(clippy::disallowed_methods, reason = "the writer's own output")]
pub(super) fn standard_output() -> Box<dyn Write + Send> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        if let Ok(fd) = io::stdout().as_fd().try_clone_to_owned() {
            return Box::new(std::fs::File::from(fd));
        }
    }
    Box::new(io::stdout())
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsFd, OwnedFd};

    /// A pipe whose reading end nobody reads, already full, so a write to it
    /// waits until the test reads it. Returns its two ends and how many
    /// bytes fill it.
    pub(in crate::tui) fn full_pipe() -> (std::fs::File, std::fs::File, usize) {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe() makes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe() made them, and nothing else owns them.
        let (read, write) = unsafe {
            use std::os::fd::FromRawFd;
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        };
        let blocking = |fd: &OwnedFd, on: bool| {
            use std::os::fd::AsRawFd;
            let raw = fd.as_fd().as_raw_fd();
            // SAFETY: reads and sets the flags of a descriptor this owns.
            unsafe {
                let flags = libc::fcntl(raw, libc::F_GETFL);
                let flags = if on {
                    flags & !libc::O_NONBLOCK
                } else {
                    flags | libc::O_NONBLOCK
                };
                libc::fcntl(raw, libc::F_SETFL, flags);
            }
        };
        blocking(&write, false);
        let mut write = std::fs::File::from(write);
        let mut filled = 0;
        while let Ok(count) = write.write(&[b'.'; 4096]) {
            filled += count;
        }
        let write = OwnedFd::from(write);
        blocking(&write, true);
        (
            std::fs::File::from(read),
            std::fs::File::from(write),
            filled,
        )
    }

    /// A terminal for tests, which keeps what it takes and takes nothing
    /// while it is held. A write waits meanwhile, as one to a terminal that
    /// doesn't read does.
    #[derive(Clone, Default)]
    pub(in crate::tui) struct Gate {
        taken: Arc<Mutex<Vec<u8>>>,
        held: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Gate {
        /// Stops taking writes, or takes them again.
        pub(in crate::tui) fn hold(&self, held: bool) {
            *crate::lock(&self.held.0) = held;
            self.held.1.notify_all();
        }

        /// What it took so far.
        pub(in crate::tui) fn taken(&self) -> Vec<u8> {
            crate::lock(&self.taken).clone()
        }
    }

    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut held = crate::lock(&self.held.0);
            while *held {
                held = self
                    .held
                    .1
                    .wait(held)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            drop(held);
            crate::lock(&self.taken).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A terminal whose writer breaks on the first write.
    pub(in crate::tui) struct Breaks;

    impl Write for Breaks {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("the terminal's writer breaks")
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Reads `count` bytes from `read`.
    pub(in crate::tui) fn take(read: &mut std::fs::File, count: usize) -> Vec<u8> {
        let mut bytes = vec![0; count];
        read.read_exact(&mut bytes).unwrap();
        bytes
    }

    /// A write to a terminal that doesn't read returns at once, and the
    /// writer is busy until the terminal takes it. Meanwhile the async
    /// runtime's timers fire and its sockets answer, since no runtime thread
    /// waits for the terminal.
    #[test]
    fn a_terminal_that_doesnt_read_holds_up_no_runtime_thread() {
        let temp = tempfile::tempdir().unwrap();
        let (mut read, write, filled) = full_pipe();
        let mut writer = Writer::start(write, temp.path().join("arko.log"), Vec::new()).unwrap();
        // (The writer's thread ends with its last handle.)
        let kept = writer.handle();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                socket.write_all(&byte).await.unwrap();
            });
            // The frames the view writes, on a runtime thread, as it does.
            tokio::spawn(async move {
                let started = Instant::now();
                for frame in 0..50 {
                    write!(writer, "frame {frame} ").unwrap();
                    writer.flush().unwrap();
                }
                assert!(started.elapsed() < Duration::from_secs(1));
                assert!(writer.busy());
                writer
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            // (The full screen's watchdog tells how long it has waited.)
            assert!(kept.waiting() >= Some(Duration::from_millis(100)));
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(b"x").await.unwrap();
            let mut byte = [0];
            let answered =
                tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut byte)).await;
            assert!(answered.is_ok(), "the socket answers");
        });
        // The terminal takes it all, in order, once it reads.
        assert_eq!(take(&mut read, filled).len(), filled);
        let expected: String = (0..50).map(|frame| format!("frame {frame} ")).collect();
        assert_eq!(take(&mut read, expected.len()), expected.as_bytes());
        drop(kept);
    }

    /// Once the terminal closed, what is written is dropped, and the writer
    /// isn't busy.
    #[test]
    fn a_closed_terminal_drops_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let (read, write, _) = full_pipe();
        drop(read);
        let mut writer = Writer::start(write, temp.path().join("arko.log"), Vec::new()).unwrap();
        writer.write_all(b"frame").unwrap();
        writer.flush().unwrap();
        assert!(writer.wait_written(Duration::from_secs(5)));
        writer.write_all(b"another").unwrap();
        writer.flush().unwrap();
        assert!(!writer.busy());
    }

    const LONG: Duration = Duration::from_secs(5);

    /// What the view wrote after it finished never reaches the terminal, nor
    /// what waited unwritten as it finished. Only the reset and what follows
    /// the view do, after the write that went on.
    #[test]
    fn the_view_writes_nothing_after_it_finished() {
        let gate = Gate::default();
        let writer = Writer::start(gate.clone(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let mut view = writer.handle();
        gate.hold(true);
        write!(view, "[taken]").unwrap();
        view.flush().unwrap();
        while writer.waiting().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        write!(view, "[waiting]").unwrap();
        view.flush().unwrap();
        // A signal's end gives the terminal back meanwhile.
        let (ticket, ended) = writer.finish(b"[reply]");
        assert!(ended);
        write!(view, "[a frame after]").unwrap();
        view.flush().unwrap();
        gate.hold(false);
        assert_eq!(writer.wait(ticket, LONG), Finish::Written);
        write!(view, "[another]").unwrap();
        view.flush().unwrap();
        assert!(writer.wait_written(LONG));
        assert_eq!(
            String::from_utf8(gate.taken()).unwrap(),
            "[taken][reset][reply]"
        );
    }

    /// Ends that give the terminal back at once share one reset, which one
    /// of them queued. Until the writer wrote it the terminal isn't given
    /// back, and an end that can't wait writes the reset itself. Once it
    /// was written, that end has nothing to do.
    #[test]
    fn the_terminal_is_given_back_once_the_reset_is_written() {
        let gate = Gate::default();
        let writer = Writer::start(gate.clone(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let mut view = writer.handle();
        gate.hold(true);
        write!(view, "[frame]").unwrap();
        view.flush().unwrap();
        // (The writer took it, and waits for the terminal.)
        while writer.waiting().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let closers: Vec<_> = (0..4)
            .map(|_| {
                let writer = writer.handle();
                std::thread::spawn(move || {
                    let (ticket, ended) = writer.finish(b"");
                    (ended, writer.wait(ticket, Duration::from_millis(200)))
                })
            })
            .collect();
        let results: Vec<_> = closers
            .into_iter()
            .map(|closer| closer.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|(ended, _)| *ended).count(), 1);
        assert!(results.iter().all(|(_, wait)| *wait == Finish::TimedOut));
        gate.hold(false);
        let (ticket, ended) = writer.finish(b"");
        assert!(!ended);
        assert_eq!(writer.wait(ticket, LONG), Finish::Written);
        assert_eq!(writer.take_over(LONG), Reset::Done);
        assert_eq!(String::from_utf8(gate.taken()).unwrap(), "[frame][reset]");

        // One that the terminal doesn't take is the forced end's to write.
        let stuck = Gate::default();
        let writer = Writer::start(stuck.clone(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        stuck.hold(true);
        let (ticket, _) = writer.finish(b"");
        assert_eq!(
            writer.wait(ticket, Duration::from_millis(100)),
            Finish::TimedOut
        );
        assert_eq!(writer.take_over(LONG), Reset::Needed(b"[reset]".to_vec()));
        assert_eq!(writer.wait(ticket, LONG), Finish::Failed);
        stuck.hold(false);
    }

    /// A writer whose thread breaks says so, isn't busy, and wakes those
    /// waiting, and the reset is left for an end that writes it itself. One
    /// whose thread doesn't start fails to start.
    #[test]
    fn a_writer_that_breaks_or_never_starts_says_so() {
        let mut writer = Writer::start(Breaks, PathBuf::new(), b"[reset]".to_vec()).unwrap();
        writer.write_all(b"[frame]").unwrap();
        writer.flush().unwrap();
        assert!(writer.wait_written(LONG));
        assert_eq!(writer.failure(), Some(Failure::Died));
        assert!(!writer.busy());
        let (ticket, _) = writer.finish(b"");
        assert_eq!(writer.wait(ticket, LONG), Finish::Failed);
        assert_eq!(writer.take_over(LONG), Reset::Needed(b"[reset]".to_vec()));

        let refused = Writer::start_with(Vec::new(), PathBuf::new(), Vec::new(), |_| {
            Err(io::Error::other("no thread"))
        });
        assert!(refused.is_err());
    }

    /// A writer that dies on the reset never counts it as written, also
    /// when the terminal is given back again with nothing after it, and the
    /// reset is left for an end that writes it itself.
    #[test]
    fn a_second_finish_after_the_writer_died_on_the_reset_isnt_written() {
        let writer = Writer::start(Breaks, PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let (first, ended) = writer.finish(b"");
        assert!(ended);
        assert_eq!(writer.wait(first, LONG), Finish::Failed);
        let (second, ended) = writer.finish(b"");
        assert!(!ended);
        assert_eq!(
            writer.wait(second, Duration::from_millis(200)),
            Finish::Failed
        );
        assert_eq!(writer.take_over(LONG), Reset::Needed(b"[reset]".to_vec()));
    }

    /// A reset the writer wrote stays written while what follows the view
    /// waits for the terminal, so an end that can't wait doesn't write
    /// another.
    #[test]
    fn a_written_reset_stays_so_while_a_reply_waits() {
        let gate = Gate::default();
        let writer = Writer::start(gate.clone(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let (reset, _) = writer.finish(b"");
        assert_eq!(writer.wait(reset, LONG), Finish::Written);
        gate.hold(true);
        let (reply, ended) = writer.finish(b"[reply]");
        assert!(!ended);
        while writer.waiting().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            writer.wait(reply, Duration::from_millis(100)),
            Finish::TimedOut
        );
        assert_eq!(writer.take_over(LONG), Reset::Done);
        gate.hold(false);
    }
}
