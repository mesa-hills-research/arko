//! What changes while nobody types shows within a frame. Above all, a turn's
//! end shows with the status line and the prompt as they are once it ended.
//! The view runs as Arko runs it, on a thread of its own, in a process and a
//! pseudo-terminal of their own, and the session's feed changes as the
//! session changes it. The test reads what the view writes through
//! termelon's emulator.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use arko_ai::LLMClient;
use termelon::testing::vt::{Emulator, Profile};
use tokio::sync::mpsc;

use super::{FRAME, Setup};
use crate::remote::{Level, WorkerChange};

/// The variable that makes the test's process the view's, with the folder
/// its session keeps its files in.
const VIEW: &str = "ARKO_TEST_REDRAW_VIEW";
/// The terminal's size in columns and rows.
const SIZE: (u16, u16) = (100, 30);
/// Its size after the resize.
const RESIZED: (u16, u16) = (80, 24);
/// How long past a frame a change may take on a busy machine. It stays well
/// under the half second of the view's clock, whose redraws could hide a
/// frame that was never asked for.
const SLACK: Duration = Duration::from_millis(200);
/// How long the view rests before each step, so the step finds it with
/// nothing left to draw, as a change while nobody types does.
const REST: Duration = Duration::from_millis(150);
/// The task of the worker the turn starts.
const TASK: &str = "Make week_start() return the Monday";

/// With no key pressed, a monitor's alert opens a turn, the turn starts a
/// worker and ends, then a notice comes, the worker finishes and the window
/// changes size. Each shows within a frame.
#[test]
fn what_changes_while_nobody_types_shows_within_a_frame() {
    if let Some(dir) = std::env::var_os(VIEW) {
        be_the_view(Path::new(&dir));
    }
    let temp = tempfile::tempdir().unwrap();
    let mut view = View::start(temp.path());
    view.wait("the view to open", Duration::from_secs(30), |rows| {
        prompt(rows) == Some("Message  / for commands")
    });
    let mut took = Vec::new();
    took.push(view.step("alert", |rows| {
        shows(rows, "Monitor 2 alerted")
            && status(rows).is_some_and(|line| line.contains("working"))
            && prompt(rows) == Some("Add to the task")
    }));
    took.push(view.step("worker", |rows| {
        last_row(rows).contains("r1") && last_row(rows).contains("running")
    }));
    // The turn ends.
    took.push(view.step("end", |rows| {
        shows(rows, "All 7 tests pass.")
            && status(rows).is_some_and(|line| line.starts_with("last turn"))
            && prompt(rows) == Some("Message  / for commands")
    }));
    took.push(view.step("notice", |rows| shows(rows, "The disk is nearly full.")));
    took.push(view.step("finish", |rows| {
        last_row(rows).contains("r1") && last_row(rows).contains("finished")
    }));
    took.push(view.resize(RESIZED, |rows| {
        rows.len() == RESIZED.1 as usize
            && last_row(rows).contains("finished")
            && status(rows).is_some_and(|line| line.starts_with("last turn"))
    }));
    view.send("quit");
    let late: Vec<_> = took
        .iter()
        .filter(|(_, took)| *took > FRAME + SLACK)
        .collect();
    assert!(late.is_empty(), "late: {late:?}, all: {took:?}");
}

/// Whether a row shows `text`.
fn shows(rows: &[String], text: &str) -> bool {
    rows.iter().any(|row| row.contains(text))
}

/// The status line, trimmed. It's the row above the input box's top rule.
fn status(rows: &[String]) -> Option<&str> {
    let prompt = rows.iter().rposition(|row| row.starts_with(" › "))?;
    Some(rows.get(prompt.checked_sub(2)?)?.trim())
}

/// What the input box shows, past its "›".
fn prompt(rows: &[String]) -> Option<&str> {
    rows.iter()
        .rev()
        .find_map(|row| row.strip_prefix(" › "))
        .map(str::trim_end)
}

/// The screen's last row. While there are workers, it's the agent list's
/// last.
fn last_row(rows: &[String]) -> &str {
    rows.last().map_or("", String::as_str)
}

/// The view's process in a pseudo-terminal, with what the terminal shows.
struct View {
    child: std::process::Child,
    /// Where the steps go. The view's process reads them in order.
    steps: File,
    master: File,
    screen: Emulator,
}

impl View {
    /// Starts the test's own process as the view's, whose session keeps its
    /// files in `dir`, on a terminal of [`SIZE`].
    fn start(dir: &Path) -> Self {
        use std::os::unix::process::CommandExt;
        let (master, slave) = pty(SIZE);
        let (steps_read, steps) = pipe();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tui::redraw_tests::what_changes_while_nobody_types_shows_within_a_frame",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            // (What the terminal supports comes from its answers alone.)
            .env_clear()
            .env("TERM", "xterm-256color")
            .env("HOME", dir)
            .env(VIEW, dir)
            .current_dir(dir)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        let steps_fd = steps_read.as_raw_fd();
        // SAFETY: only calls that are safe between fork and exec, on
        // descriptors this process owns.
        let child = unsafe {
            command.pre_exec(move || {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 0);
                // The steps come on descriptor 3, kept open across exec.
                if steps_fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0);
                } else {
                    libc::dup2(steps_fd, 3);
                }
                Ok(())
            })
        }
        .spawn()
        .unwrap();
        let mut screen = Emulator::with_profile(Profile::xterm(), SIZE.0, SIZE.1);
        screen.set_strict(false);
        screen.set_record_events(false);
        Self {
            child,
            steps,
            master,
            screen,
        }
    }

    /// Sends the view's process `step`, and returns how long until the
    /// screen `shows` it.
    fn step(&mut self, step: &str, shows: impl Fn(&[String]) -> bool) -> (String, Duration) {
        self.rest();
        let sent = Instant::now();
        self.send(step);
        self.wait(step, Duration::from_secs(5), shows);
        (step.to_string(), sent.elapsed())
    }

    /// Makes the terminal `size`, as a window that changes size does, and
    /// returns how long until the screen `shows` it.
    fn resize(
        &mut self,
        size: (u16, u16),
        shows: impl Fn(&[String]) -> bool,
    ) -> (String, Duration) {
        self.rest();
        let sent = Instant::now();
        self.screen.resize(size.0, size.1);
        let size = winsize(size);
        // SAFETY: the size of the terminal this test opened.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
        self.wait("resize", Duration::from_secs(5), shows);
        ("resize".to_string(), sent.elapsed())
    }

    fn send(&mut self, step: &str) {
        writeln!(self.steps, "{step}").unwrap();
    }

    /// Reads what the view writes for a few frames' time, so the next step
    /// comes to a view that has drawn all it had.
    fn rest(&mut self) {
        let until = Instant::now() + REST;
        while Instant::now() < until {
            self.read(until - Instant::now());
        }
    }

    /// Reads what the view wrote within `within`, if it wrote, and answers
    /// the terminal's questions as the emulator does.
    fn read(&mut self, within: Duration) {
        let mut poll = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = within.as_millis().clamp(1, 5) as i32;
        // SAFETY: one descriptor to wait on.
        if unsafe { libc::poll(&mut poll, 1, wait) } <= 0 {
            return;
        }
        let mut chunk = [0; 65536];
        if let Ok(count @ 1..) = self.master.read(&mut chunk) {
            let _ = self.screen.feed(&chunk[..count]);
            let answers = self.screen.take_output();
            // (Once the view ended, no one reads them.)
            let _ = self.master.write_all(&answers);
        }
    }

    /// Reads what the view writes until a whole frame on the screen `shows`
    /// what `what` waits for. Fails after `within` and prints the screen.
    fn wait(&mut self, what: &str, within: Duration, shows: impl Fn(&[String]) -> bool) {
        let started = Instant::now();
        loop {
            // (A frame half written, or inside its synchronized update,
            // isn't on the screen yet.)
            if !self.screen.in_sequence() && !self.screen.mode(2026) && shows(&self.rows()) {
                return;
            }
            assert!(
                started.elapsed() < within,
                "waited for {what}, the screen:\n{}\nthe view's process: {:?}",
                self.rows().join("\n"),
                self.child.try_wait()
            );
            self.read(within);
        }
    }

    fn rows(&self) -> Vec<String> {
        let (_, rows) = self.screen.size();
        (0..rows)
            .map(|y| self.screen.row_text(y).unwrap_or_default())
            .collect()
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // (Asked to quit, it gives the terminal back and ends. One that
        // doesn't is ended.)
        let until = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().is_ok_and(|ended| ended.is_none()) && Instant::now() < until {
            self.read(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn winsize((cols, rows): (u16, u16)) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Opens a pseudo-terminal of `size`. Returns its master's end and its
/// slave's.
fn pty(size: (u16, u16)) -> (File, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let size = winsize(size);
    // SAFETY: two descriptors and a size for a new terminal.
    let made = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(made, 0);
    // SAFETY: the descriptors `openpty` just opened, owned here. They close
    // in every process this one starts, and the view's process gets the
    // slave's as its standard streams.
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        (
            File::from(OwnedFd::from_raw_fd(master)),
            OwnedFd::from_raw_fd(slave),
        )
    }
}

/// Opens a pipe whose ends close in every process this one starts. Returns
/// the end to read and the end to write.
fn pipe() -> (OwnedFd, File) {
    let mut fds = [0; 2];
    // SAFETY: two descriptors for a new pipe.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: the descriptors `pipe2` just opened, owned here.
    unsafe {
        (
            OwnedFd::from_raw_fd(fds[0]),
            File::from(OwnedFd::from_raw_fd(fds[1])),
        )
    }
}

/// The view's process. It opens the view on its terminal for a session of
/// its own in `dir`, then changes the session's feed as the session does at
/// each step that comes on descriptor 3, until "quit".
fn be_the_view(dir: &Path) -> ! {
    use std::io::BufRead;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _session = runtime.enter();
    let (harness, _inputs, input) = crate::harness::tests::harness(dir, LLMClient::new(), true);
    let (requests, _requested) = mpsc::unbounded_channel();
    let (_asks, quit_asks) = mpsc::unbounded_channel();
    let feed = harness.feed.clone();
    let setup = Setup {
        feed: feed.clone(),
        details: harness.details.clone(),
        meter: harness.gauges.clone(),
        input,
        requests,
        root: harness.root.clone(),
        session_dir: harness.session.dir.clone(),
        workspace: harness.settings.workspace.clone(),
        handover_at: 250_000,
        show_thinking: false,
        banner: harness.header(None),
        keys: harness.keys.clone(),
        quit_asks,
    };
    let screen = super::start(setup, &harness.output).expect("the view opens");
    // SAFETY: the steps' descriptor, which the test left open for this process.
    let steps = std::io::BufReader::new(unsafe { File::from_raw_fd(3) });
    for step in steps.lines() {
        match step.unwrap().as_str() {
            // As the session opens a turn for a monitor's alert.
            "alert" => {
                feed.turn_opening("Monitor 2 alerted", None);
                feed.turn_started();
            }
            "worker" => feed.worker("r1", TASK, WorkerChange::Started),
            "end" => {
                feed.reply("All 7 tests pass.");
                feed.turn_ended("6s", None);
            }
            "notice" => feed.notice(Level::Warn, "The disk is nearly full."),
            "finish" => feed.worker(
                "r1",
                TASK,
                WorkerChange::Finished {
                    report: "It returns the Monday.",
                },
            ),
            _ => break,
        }
    }
    runtime.block_on(screen.close());
    std::process::exit(0);
}
