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
//! that can't count on the writer writes the reset itself
//! ([`Writer::reset_directly`]), once, from a thread of its own, and those
//! that come meanwhile wait for what became of it. A writer whose thread
//! didn't start fails to start, and one whose thread ended says so
//! ([`Writer::failure`]) and wakes those waiting.
//!
//! Its state is behind one lock, held for no write, no wait but its own
//! Condvar's, and no call out. Those waiting are woken after it's let go.
//! The record of the reset written without the writer is an atomic outside
//! it, so an end that can't count on the writer waits on no lock.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
    /// What gives the terminal back.
    reset: Vec<u8>,
    /// The writer wrote the reset ([`Phase::Reset`]), for those who look
    /// without its lock.
    reset_written: AtomicBool,
    /// Where the reset written without the writer is, [`NOT_STARTED`],
    /// [`IN_FLIGHT`], [`WRITTEN`] or [`FAILED`]. Never under the lock.
    direct: Arc<AtomicU8>,
}

/// No one writes the reset without the writer, or the thread to write it
/// didn't start.
const NOT_STARTED: u8 = 0;
/// A thread of its own writes it.
const IN_FLIGHT: u8 = 1;
/// That thread wrote it.
const WRITTEN: u8 = 2;
/// That thread couldn't write it (the terminal is gone), or it broke.
const FAILED: u8 = 3;

/// How often those waiting for the reset written without the writer look.
const POLL: Duration = Duration::from_millis(5);

/// Marks the reset written without the writer failed, when the thread
/// writing it breaks before it says what became of it.
struct Unwound(Arc<AtomicU8>);

impl Drop for Unwound {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(IN_FLIGHT, FAILED, Ordering::SeqCst, Ordering::SeqCst);
    }
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

/// What became of a reset written without the writer
/// ([`Writer::reset_directly`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Direct {
    /// None was needed, as the writer wrote the reset.
    Unneeded,
    /// It was written.
    Written,
    /// It wasn't, because the terminal is gone, or the thread to write it
    /// didn't start (and a later call tries again).
    Failed,
    /// It's being written, and wasn't by the deadline.
    TimedOut,
    /// There's no writer to keep a record, so nothing is known.
    Unknown,
}

/// Cancel, which ends an escape sequence that a frame cut short left open.
pub(super) const CAN: u8 = 0x18;

/// How long an end that can't count on the writer tries for a lock.
pub(super) const LOCK_WAIT: Duration = Duration::from_millis(50);

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
                phase: Phase::Open,
                taken_over: false,
                failed: None,
            }),
            changed: Condvar::new(),
            written: Notify::new(),
            reset,
            reset_written: AtomicBool::new(false),
            direct: Arc::new(AtomicU8::new(NOT_STARTED)),
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
                state.queue(&self.shared.reset);
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
    /// writer): writes the reset without it, unless the writer wrote it, and
    /// waits at most `wait` for what became of it. Its record is an atomic of
    /// its own, never under the writer's lock, which this takes for no wait.
    /// The first call (or the first after a thread that didn't start) claims
    /// it, stops the writer if it gets the writer's lock within
    /// [`LOCK_WAIT`], and starts a thread with `spawn` that writes [`CAN`] and
    /// the reset to what `out` opens. All calls then look at the record every
    /// few milliseconds until their deadline.
    pub(super) fn reset_directly(
        &self,
        wait: Duration,
        out: impl FnOnce() -> Box<dyn Write + Send> + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> Direct {
        let until = Instant::now() + wait;
        let mut start = Some((out, spawn));
        loop {
            if self.shared.reset_written.load(Ordering::SeqCst) {
                return Direct::Unneeded;
            }
            match self.shared.direct.load(Ordering::SeqCst) {
                WRITTEN => return Direct::Written,
                FAILED => return Direct::Failed,
                NOT_STARTED => {
                    // (This call's own thread didn't start.)
                    let Some((out, spawn)) = start.take() else {
                        return Direct::Failed;
                    };
                    let claimed = self.shared.direct.compare_exchange(
                        NOT_STARTED,
                        IN_FLIGHT,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    if claimed.is_err() {
                        start = Some((out, spawn));
                        continue;
                    }
                    // (The writer writes no more, if its lock can be had.)
                    let left = until.saturating_duration_since(Instant::now());
                    if let Some(mut state) = try_lock_for(&self.shared.state, left.min(LOCK_WAIT)) {
                        state.taken_over = true;
                        state.pending.clear();
                    }
                    let mut bytes = vec![CAN];
                    bytes.extend_from_slice(&self.shared.reset);
                    let direct = self.shared.direct.clone();
                    let started = spawn(Box::new(move || {
                        let _unwound = Unwound(direct.clone());
                        let mut out = out();
                        let wrote = out.write_all(&bytes).and_then(|()| out.flush());
                        let outcome = if wrote.is_ok() { WRITTEN } else { FAILED };
                        direct.store(outcome, Ordering::SeqCst);
                    }));
                    if started.is_err() {
                        self.shared.direct.store(NOT_STARTED, Ordering::SeqCst);
                        return Direct::Failed;
                    }
                }
                _ => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Direct::TimedOut;
                    }
                    std::thread::sleep(left.min(POLL));
                }
            }
        }
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
                        shared.reset_written.store(true, Ordering::SeqCst);
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
        waiting: Arc<Mutex<usize>>,
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

        /// How many writes wait for it to take them.
        pub(in crate::tui) fn waiting(&self) -> usize {
            crate::lock(&self.waiting).to_owned()
        }
    }

    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            *crate::lock(&self.waiting) += 1;
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
            *crate::lock(&self.waiting) -= 1;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Starts a thread for a reset written without the writer, as Arko does.
    pub(in crate::tui) fn spawn(run: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        std::thread::Builder::new().spawn(run).map(drop)
    }

    /// Writes a reset without the writer to `gate`, waiting `wait` at most.
    fn reset_to(writer: &Writer, gate: &Gate, wait: Duration) -> Direct {
        let gate = gate.clone();
        writer.reset_directly(wait, move || Box::new(gate), spawn)
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
            // (The full screen's watchdog tells how long it has waited,
            // from when the writer's thread took the first frame.)
            while kept.waiting().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
        assert_eq!(reset_to(&writer, &gate, LONG), Direct::Unneeded);
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
        let direct = Gate::default();
        assert_eq!(reset_to(&writer, &direct, LONG), Direct::Written);
        assert_eq!(direct.taken(), b"\x18[reset]");
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
        let direct = Gate::default();
        assert_eq!(reset_to(&writer, &direct, LONG), Direct::Written);
        assert_eq!(direct.taken(), b"\x18[reset]");

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
        let direct = Gate::default();
        assert_eq!(reset_to(&writer, &direct, LONG), Direct::Written);
        assert_eq!(direct.taken(), b"\x18[reset]");
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
        let direct = Gate::default();
        assert_eq!(reset_to(&writer, &direct, LONG), Direct::Unneeded);
        assert!(direct.taken().is_empty());
        gate.hold(false);
    }

    /// The writer's reset and the one written without it go through each of
    /// their states. The writer's goes Open, Finishing, then Reset when the
    /// writer wrote it, or Failed when the writer dies, which never turns
    /// into written. The direct one goes NotStarted, InFlight, then Written,
    /// or Failed when the terminal refuses it, or back to NotStarted when its
    /// thread doesn't start, so a later call tries again. While it's in
    /// flight, others wait for it, and then all get what it came to.
    #[test]
    fn the_resets_go_through_each_of_their_states() {
        const SHORT: Duration = Duration::from_millis(100);
        // Open, Finishing, Reset.
        let gate = Gate::default();
        let writer = Writer::start(gate.clone(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let mut view = writer.handle();
        write!(view, "[open]").unwrap();
        view.flush().unwrap();
        assert!(writer.wait_written(LONG));
        gate.hold(true);
        let (ticket, ended) = writer.finish(b"");
        assert!(ended);
        write!(view, "[sealed]").unwrap();
        view.flush().unwrap();
        assert_eq!(writer.wait(ticket, SHORT), Finish::TimedOut);
        gate.hold(false);
        assert_eq!(writer.wait(ticket, LONG), Finish::Written);
        assert_eq!(reset_to(&writer, &Gate::default(), LONG), Direct::Unneeded);
        assert_eq!(String::from_utf8(gate.taken()).unwrap(), "[open][reset]");

        // Finishing, Failed, also when given back again.
        let dies = Writer::start(Breaks, PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let (ticket, _) = dies.finish(b"");
        assert_eq!(dies.wait(ticket, LONG), Finish::Failed);
        let (again, _) = dies.finish(b"[reply]");
        assert_eq!(dies.wait(again, SHORT), Finish::Failed);

        // NotStarted, InFlight, NotStarted again when its thread doesn't start.
        let direct = Gate::default();
        let refused = {
            let direct = direct.clone();
            dies.reset_directly(
                LONG,
                move || Box::new(direct),
                |_| Err(io::Error::other("no thread")),
            )
        };
        assert_eq!(refused, Direct::Failed);
        assert!(direct.taken().is_empty());

        // NotStarted, InFlight, Written, with two at once and one more that
        // gives up waiting.
        direct.hold(true);
        let closers: Vec<_> = (0..2)
            .map(|_| {
                let (writer, direct) = (dies.handle(), direct.clone());
                std::thread::spawn(move || reset_to(&writer, &direct, LONG))
            })
            .collect();
        let started = Instant::now();
        while direct.waiting() == 0 {
            assert!(started.elapsed() < LONG);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(reset_to(&dies, &direct, SHORT), Direct::TimedOut);
        direct.hold(false);
        for closer in closers {
            assert_eq!(closer.join().unwrap(), Direct::Written);
        }
        assert_eq!(direct.taken(), b"\x18[reset]");
        // Written stays so, with nothing more written.
        assert_eq!(reset_to(&dies, &direct, LONG), Direct::Written);
        assert_eq!(direct.taken(), b"\x18[reset]");

        // InFlight, Failed, when the terminal refuses it, and no other try.
        struct Refuses;
        impl Write for Refuses {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("the terminal is gone"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let refusing = Writer::start(Breaks, PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let (ticket, _) = refusing.finish(b"");
        assert_eq!(refusing.wait(ticket, LONG), Finish::Failed);
        assert_eq!(
            refusing.reset_directly(LONG, || Box::new(Refuses), spawn),
            Direct::Failed
        );
        let unused = Gate::default();
        assert_eq!(reset_to(&refusing, &unused, LONG), Direct::Failed);
        assert!(unused.taken().is_empty());
    }

    /// A reset whose thread didn't start while something else held the
    /// writer's lock is left for the next end, which writes it.
    #[test]
    fn a_reset_whose_thread_didnt_start_while_the_lock_was_held_is_tried_again() {
        let writer = Writer::start(io::sink(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let shared = writer.shared.clone();
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let mut holder = None;
        let first = writer.reset_directly(
            Duration::from_millis(10),
            || Box::new(io::sink()),
            |_| {
                holder = Some(std::thread::spawn(move || {
                    let state = crate::lock(&shared.state);
                    held.send(()).unwrap();
                    let _ = released.recv();
                    drop(state);
                }));
                holding.recv().unwrap();
                Err(io::Error::other("no thread"))
            },
        );
        release.send(()).unwrap();
        holder.unwrap().join().unwrap();
        assert_ne!(first, Direct::Written);
        let direct = Gate::default();
        assert_eq!(reset_to(&writer, &direct, LONG), Direct::Written);
        assert_eq!(direct.taken(), b"\x18[reset]");
    }

    /// A thread writing the reset that breaks leaves it failed, which those
    /// waiting learn at once, not at their deadline.
    #[test]
    fn a_reset_whose_thread_breaks_counts_as_failed() {
        let writer = Writer::start(io::sink(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let started = Instant::now();
        let first = writer.reset_directly(
            Duration::from_secs(1),
            || -> Box<dyn Write + Send> { panic!("the reset's thread breaks") },
            spawn,
        );
        assert_eq!(first, Direct::Failed);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(reset_to(&writer, &Gate::default(), LONG), Direct::Failed);
    }

    /// One waiting for a reset in flight returns by its deadline, also while
    /// something else holds the writer's lock.
    #[test]
    fn waiting_for_a_reset_ends_at_its_deadline_while_the_lock_is_held() {
        let writer = Writer::start(io::sink(), PathBuf::new(), b"[reset]".to_vec()).unwrap();
        let held = Gate::default();
        held.hold(true);
        let first = {
            let (writer, held) = (writer.handle(), held.clone());
            std::thread::spawn(move || reset_to(&writer, &held, LONG))
        };
        let started = Instant::now();
        while held.waiting() == 0 {
            assert!(started.elapsed() < LONG);
            std::thread::sleep(Duration::from_millis(1));
        }
        let waiter = {
            let writer = writer.handle();
            std::thread::spawn(move || {
                let started = Instant::now();
                let direct = writer.reset_directly(
                    Duration::from_millis(200),
                    || Box::new(io::sink()),
                    spawn,
                );
                (direct, started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let state = crate::lock(&writer.shared.state);
        std::thread::sleep(Duration::from_millis(600));
        drop(state);
        let (direct, waited) = waiter.join().unwrap();
        held.hold(false);
        assert_eq!(first.join().unwrap(), Direct::Written);
        assert_eq!(direct, Direct::TimedOut);
        assert!(waited < Duration::from_millis(400), "it waited {waited:?}");
    }
}
