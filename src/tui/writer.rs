//! The full screen's writer. What the full screen sends the terminal (its
//! frames, the title, the bell, a copy, the mouse's mode, leaving the
//! screen) goes through it in order, and a thread of its own writes it. A
//! terminal that stops taking output then holds up that thread alone. Were
//! the write on one of the async runtime's threads, it could hold up the
//! runtime's IO and timers, and with them the whole session. The view draws
//! a frame only once the writer has written the last one, so a terminal
//! that takes output again gets the changes since the last frame it took.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::watchdog::{Watchdog, Watched};

/// A handle on the writer. What is written to it waits in the handle, and
/// a flush hands it to the writer's thread, after what the other handles
/// flushed before. The thread ends when the last handle is dropped.
pub(super) struct Writer {
    shared: Arc<Shared>,
    buffer: Vec<u8>,
}

struct Shared {
    state: Mutex<State>,
    /// Rings for the writer's thread when there is something to write, and
    /// for those waiting for it to be written.
    changed: Condvar,
    /// Rings for the view when everything handed over has been written.
    written: Notify,
}

#[derive(Default)]
struct State {
    /// What waits to be written.
    pending: Vec<u8>,
    /// A write goes on.
    writing: bool,
    /// When the write going on started.
    started: Option<Instant>,
    /// The terminal refused a write (it closed), so the rest is dropped.
    gone: bool,
}

/// How long the thread waits for output before it checks whether any handle
/// is left.
const IDLE_CHECK: Duration = Duration::from_secs(1);

impl Writer {
    /// Starts the writer's thread, which writes to `out`, watched for writes
    /// that don't end, with the watchdog's lines in the log at `log`.
    pub(super) fn start(out: impl Write + Send + 'static, log: PathBuf) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            written: Notify::new(),
        });
        let watched = Arc::downgrade(&shared);
        let watchdog = Watchdog::start(Watched::Terminal, log, String::new);
        // (Without a thread nothing is written, and the view waits.)
        let _ = std::thread::Builder::new()
            .name("arko-screen".into())
            .spawn(move || write_all(&watched, out, &watchdog));
        Self {
            shared,
            buffer: Vec::new(),
        }
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
        !state.gone && (state.writing || !state.pending.is_empty())
    }

    /// How long the write going on has waited for the terminal, if one does.
    pub(super) fn waiting(&self) -> Option<Duration> {
        let started = crate::lock(&self.shared.state).started;
        started.map(|started| started.elapsed())
    }

    /// Waits until everything handed over is written, or the terminal
    /// closed, for at most `timeout`. Whether it was.
    pub(super) fn wait_written(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut state = crate::lock(&self.shared.state);
        while !state.gone && (state.writing || !state.pending.is_empty()) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        true
    }

    /// Waits until everything handed over is written. It may return early,
    /// so check [`busy`](Self::busy) again.
    pub(super) async fn written(&self) {
        self.shared.written.notified().await;
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
        let mut state = crate::lock(&self.shared.state);
        if state.gone {
            self.buffer.clear();
        } else {
            state.pending.append(&mut self.buffer);
            self.shared.changed.notify_all();
        }
        Ok(())
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
            if state.pending.is_empty() {
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
            state.writing = true;
            state.started = Some(Instant::now());
            std::mem::take(&mut state.pending)
        };
        watchdog.writing();
        let wrote = out.write_all(&bytes).and_then(|()| out.flush());
        watchdog.written();
        let done = {
            let mut state = crate::lock(&shared.state);
            state.writing = false;
            state.started = None;
            if wrote.is_err() {
                state.gone = true;
                state.pending.clear();
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

/// Where the writer writes, standard output. On Unix, through a descriptor
/// of its own, so a write that waits for the terminal holds no lock that a
/// print elsewhere would wait for.
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
        let mut writer = Writer::start(write, temp.path().join("arko.log"));
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
        let mut writer = Writer::start(write, temp.path().join("arko.log"));
        writer.write_all(b"frame").unwrap();
        writer.flush().unwrap();
        assert!(writer.wait_written(Duration::from_secs(5)));
        writer.write_all(b"another").unwrap();
        writer.flush().unwrap();
        assert!(!writer.busy());
    }
}
