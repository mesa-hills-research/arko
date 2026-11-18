//! Arko's side of a run. It starts the code runner, sends it the run, runs
//! the code's calls through the gate ([`NestedCalls`]) and answers them. It
//! ends the child at the run's time limit, on a stop, at the refusal cap,
//! at a call the task stops at, and on anything it can't read. The child
//! is killed when the run's future is dropped. A child that dies is the
//! run's error, never Arko's.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arko_agents::{NestedCalls, NestedOutcome, NestedRecord, NestedState, StopSignal};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::protocol::{
    Answer, Call, Done, FromChild, MAGIC, MAX_FRAME_FROM_CHILD_BYTES, Start, ToChild,
};
use super::{MAX_ANSWER_BYTES, MAX_CALLS, MAX_REFUSALS};

/// How often Arko looks at the runner's resident memory.
const MEMORY_CHECK: Duration = Duration::from_millis(100);

/// Where the code's calls go. In a session it is the gate,
/// [`NestedCalls`], which each call goes through.
#[async_trait]
pub trait Gate: Send + Sync {
    async fn call(&self, tool: &str, params: HashMap<String, Value>) -> NestedOutcome;
    /// The calls whose tools started, as far as each got.
    fn records(&self) -> Vec<NestedRecord>;
}

#[async_trait]
impl Gate for NestedCalls {
    async fn call(&self, tool: &str, params: HashMap<String, Value>) -> NestedOutcome {
        NestedCalls::call(self, tool, params).await
    }

    fn records(&self) -> Vec<NestedRecord> {
        NestedCalls::records(self)
    }
}

/// What the runner's error output is kept to, for a child that died.
const STDERR_BYTES: usize = 4 * 1024;

/// How the code runner is started.
#[derive(Debug, Clone)]
pub struct Runner {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl Runner {
    /// This program as the code runner. On Linux it is `/proc/self/exe`,
    /// the program that runs, also once a new build has replaced its file.
    /// (In tests it is the test program, which runs the runner in a test of
    /// its own.)
    pub fn this_program() -> std::io::Result<Self> {
        let program = if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe()?
        };
        #[cfg(not(test))]
        let (args, env) = (vec![OsString::from(super::RUNNER_ARGUMENT)], Vec::new());
        #[cfg(test)]
        let (args, env) = tests::runner_command();
        Ok(Self { program, args, env })
    }
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome {
    /// The runner said how.
    Done(Done),
    /// The whole run took longer than it may.
    TimedOut,
    /// Too many of the code's calls were refused, the last for `last`.
    Refusals { last: String },
    /// The middleware stopped or paused the task at a call of `tool`, for
    /// `message`.
    Halted { tool: String, message: String },
    /// The task was stopped.
    Stopped,
    /// The runner ended without saying how, as `status` says, having
    /// written `stderr`.
    Died { status: String, stderr: String },
    /// The runner held more memory than it may, and Arko ended it.
    OverMemory,
    /// The runner couldn't start, or said something Arko couldn't read.
    Broken(String),
}

/// What a run's calls changed, counted from the calls of tools that don't
/// only read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    pub files_written: u32,
    pub files_edited: u32,
    pub commands: u32,
}

impl Changes {
    /// What `records`' calls may have changed. A call that still ran when
    /// the run ended counts, since it may have.
    fn of(records: &[NestedRecord]) -> Self {
        use arko_agents::builtins::{EDIT_FILE_TOOL_NAME, WRITE_FILE_TOOL_NAME};
        let mut changes = Self::default();
        for record in records {
            let ran = !matches!(record.state, NestedState::Refused);
            let wrote = matches!(
                record.state,
                NestedState::Running | NestedState::Ran { failed: false } | NestedState::Halted
            );
            match record.tool_name.as_str() {
                WRITE_FILE_TOOL_NAME if wrote => changes.files_written += 1,
                EDIT_FILE_TOOL_NAME if wrote => changes.files_edited += 1,
                // A command that failed may have changed things too.
                crate::shell::SHELL_TOOL_NAME if ran => changes.commands += 1,
                _ => {}
            }
        }
        changes
    }
}

/// A run's end and its record.
#[derive(Debug)]
pub struct Run {
    pub outcome: Outcome,
    pub changes: Changes,
    /// The calls the runner sent.
    pub calls: u32,
    /// The most bytes of the code's calls Arko held at once, read and not
    /// yet answered.
    pub most_held: usize,
    pub took: Duration,
}

/// The bytes of the runner's frames Arko holds.
#[derive(Debug, Default)]
struct Held {
    now: AtomicUsize,
    most: AtomicUsize,
}

impl Held {
    fn take(&self, bytes: usize) {
        let now = self.now.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.most.fetch_max(now, Ordering::SeqCst);
    }

    fn give_back(&self, bytes: usize) {
        self.now.fetch_sub(bytes, Ordering::SeqCst);
    }
}

/// Runs `start` in a fresh child, its calls through `gate`, for at most
/// `timeout`, until `stop` fires, and while the child's resident memory
/// stays under `start.limits.os_memory_bytes`.
pub async fn run(
    runner: &Runner,
    start: Start,
    gate: &dyn Gate,
    timeout: Duration,
    stop: &StopSignal,
) -> Run {
    let began = Instant::now();
    let deadline = tokio::time::Instant::now() + timeout;
    let held = Arc::new(Held::default());
    let mut session = Session {
        gate,
        allowed: start.tools.clone(),
        calls: 0,
        refusals: 0,
        held: held.clone(),
    };
    let outcome = tokio::select! {
        outcome = session.drive(runner, start) => outcome,
        () = tokio::time::sleep_until(deadline) => Outcome::TimedOut,
        () = stop.stopping() => Outcome::Stopped,
    };
    // (The child, if it still runs, was killed as `drive` was dropped.)
    Run {
        outcome,
        changes: Changes::of(&gate.records()),
        calls: session.calls,
        most_held: held.most.load(Ordering::SeqCst),
        took: began.elapsed(),
    }
}

/// A run's count of its calls.
struct Session<'a> {
    gate: &'a dyn Gate,
    allowed: Vec<String>,
    calls: u32,
    refusals: u32,
    held: Arc<Held>,
}

/// What a call of the code's comes to.
enum Answered {
    Answer(Answer),
    /// The run ends here.
    End(Outcome),
}

/// The running child and Arko's ends of its pipes.
struct Pipes {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    /// The child's frames, one at a time.
    frames: mpsc::Receiver<(FromChild, usize)>,
    /// Why the reader stopped reading, when the child broke the protocol.
    fatal: tokio::sync::watch::Receiver<Option<String>>,
    /// The first part of the child's error output.
    stderr: tokio::task::JoinHandle<String>,
}

impl Pipes {
    /// Resolves with why the reader stopped, once the child breaks the
    /// protocol.
    async fn broken(fatal: &mut tokio::sync::watch::Receiver<Option<String>>) -> String {
        let fault = fatal
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|why| why.clone());
        match fault {
            Some(why) => why,
            // The reader ended without a fault.
            None => std::future::pending().await,
        }
    }

    /// How the run ended with the child's output closed: its fault, or its
    /// death.
    async fn ended(&mut self, why: String) -> Outcome {
        if let Some(fault) = self.fatal.borrow().clone() {
            return Outcome::Broken(fault);
        }
        died(&mut self.child, &mut self.stderr, why).await
    }
}

impl Session<'_> {
    async fn drive(&mut self, runner: &Runner, start: Start) -> Outcome {
        let mut child = match spawn(runner) {
            Ok(child) => child,
            Err(error) => {
                return Outcome::Broken(format!("Arko couldn't start the code runner ({error})."));
            }
        };
        let (Some(input), Some(output), Some(errors)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Outcome::Broken("The code runner has no pipes.".into());
        };
        let memory_limit = start.limits.os_memory_bytes;
        // One frame at a time, so Arko holds at most the frame waiting and
        // the one being read, and the runner waits for room.
        let (frames_in, frames) = mpsc::channel(1);
        let (fault, fatal) = tokio::sync::watch::channel(None);
        let reader = tokio::spawn(read_frames(output, frames_in, fault, self.held.clone()));
        // The error output, read on its own so the runner never waits on it.
        let stderr = tokio::spawn(read_start(errors, STDERR_BYTES));
        let _tasks = Tasks([reader.abort_handle(), stderr.abort_handle()]);
        let mut pipes = Pipes {
            child,
            input,
            frames,
            fatal,
            stderr,
        };
        if let Err(error) = send(&mut pipes.input, &ToChild::Start(start)).await {
            return pipes
                .ended(format!("Arko couldn't send the code ({error})"))
                .await;
        }
        let pid = pipes.child.id();
        let over_memory = async {
            let mut checks = tokio::time::interval(MEMORY_CHECK);
            loop {
                checks.tick().await;
                if pid
                    .and_then(resident_bytes)
                    .is_some_and(|bytes| bytes > memory_limit)
                {
                    return;
                }
            }
        };
        tokio::select! {
            outcome = self.answer_calls(&mut pipes) => outcome,
            () = over_memory => Outcome::OverMemory,
        }
    }

    /// Answers the runner's calls until it says how the run ended.
    async fn answer_calls(&mut self, pipes: &mut Pipes) -> Outcome {
        loop {
            let Some((frame, bytes)) = pipes.frames.recv().await else {
                return pipes.ended(String::new()).await;
            };
            let call = match frame {
                FromChild::Done(done) => return Outcome::Done(done),
                FromChild::Call(call) => call,
            };
            self.calls += 1;
            if self.calls > MAX_CALLS {
                return Outcome::Broken(format!(
                    "The code runner sent more than {MAX_CALLS} calls."
                ));
            }
            let answered = tokio::select! {
                answered = self.answer(call) => answered,
                // The runner ended, or broke the protocol, while the call
                // ran, so the call is dropped (a command it started is
                // killed).
                _ = pipes.child.wait() => return ended_during_call(pipes).await,
                fault = Pipes::broken(&mut pipes.fatal) => return Outcome::Broken(fault),
            };
            self.held.give_back(bytes);
            match answered {
                Answered::Answer(answer) => {
                    if let Err(error) = send(&mut pipes.input, &ToChild::Answer(answer)).await {
                        return pipes
                            .ended(format!("Arko couldn't send an answer ({error})"))
                            .await;
                    }
                }
                Answered::End(outcome) => return outcome,
            }
        }
    }

    /// Runs `call` through the gate, and what its answer is.
    async fn answer(&mut self, call: Call) -> Answered {
        let refused = |id, text: String| Answer {
            id,
            ok: false,
            text,
            refused: true,
        };
        if !self.allowed.contains(&call.tool) {
            return self.refused(refused(
                call.id,
                format!("{} can't be called from code.", call.tool),
            ));
        }
        let params: HashMap<String, Value> = match call.args {
            Value::Object(args) => args.into_iter().collect(),
            _ => {
                return Answered::Answer(Answer {
                    id: call.id,
                    ok: false,
                    text: format!("tools.{} takes one object of arguments.", call.tool),
                    refused: false,
                });
            }
        };
        match self.gate.call(&call.tool, params).await {
            NestedOutcome::Finished(output) => {
                Answered::Answer(if output.text.len() > MAX_ANSWER_BYTES {
                    Answer {
                        id: call.id,
                        ok: false,
                        text: format!(
                            "The result of {} is {} MB, over the {} MB a result into code may be.",
                            call.tool,
                            output.text.len().div_ceil(1024 * 1024),
                            MAX_ANSWER_BYTES / (1024 * 1024)
                        ),
                        refused: false,
                    }
                } else {
                    Answer {
                        id: call.id,
                        ok: true,
                        text: output.text,
                        refused: false,
                    }
                })
            }
            NestedOutcome::Failed(text) => Answered::Answer(Answer {
                id: call.id,
                ok: false,
                text,
                refused: false,
            }),
            NestedOutcome::Refused(text) => self.refused(refused(call.id, text)),
            NestedOutcome::Halted(message) => Answered::End(Outcome::Halted {
                tool: call.tool,
                message,
            }),
        }
    }

    /// A refused call's answer, or the run's end at the refusal cap.
    fn refused(&mut self, answer: Answer) -> Answered {
        self.refusals += 1;
        if self.refusals >= MAX_REFUSALS {
            return Answered::End(Outcome::Refusals { last: answer.text });
        }
        Answered::Answer(answer)
    }
}

/// The outcome of a run whose runner ended while one of its calls ran. A
/// runner whose code failed sends its [`Done`] without waiting for its
/// calls, and then ends, so that is read first.
async fn ended_during_call(pipes: &mut Pipes) -> Outcome {
    // The output ends soon after the runner, once its last frames are read.
    let rest = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some((frame, _)) = pipes.frames.recv().await {
            if let FromChild::Done(done) = frame {
                return Some(done);
            }
        }
        None
    });
    match rest.await {
        Ok(Some(done)) => Outcome::Done(done),
        _ => pipes.ended(String::new()).await,
    }
}

/// The outcome of a child whose output ended without a [`Done`], `why`
/// when Arko knows.
async fn died(
    child: &mut tokio::process::Child,
    stderr: &mut tokio::task::JoinHandle<String>,
    why: String,
) -> Outcome {
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => describe_status(status),
        Ok(Err(error)) => format!("its status couldn't be read ({error})"),
        Err(_) => {
            let _ = child.start_kill();
            "it stopped answering and was killed".into()
        }
    };
    let status = if why.is_empty() {
        status
    } else {
        format!("{why}, and {status}")
    };
    let stderr = tokio::time::timeout(Duration::from_secs(1), stderr)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Outcome::Died { status, stderr }
}

/// Tasks that end with the run.
struct Tasks([tokio::task::AbortHandle; 2]);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// The child, with nothing of Arko's environment (its keys stay with
/// Arko), in a directory of no project's, in a process group of its own so
/// the terminal's signals don't reach it, and without a window on Windows.
fn spawn(runner: &Runner) -> std::io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(&runner.program);
    command
        .args(&runner.args)
        .env_clear()
        .envs(runner.env.iter().map(|(name, value)| (name, value)))
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Some of the system's own libraries look for it.
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    command.process_group(0);
    command.spawn()
}

async fn send(input: &mut tokio::process::ChildStdin, message: &ToChild) -> std::io::Result<()> {
    let body = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    let length = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    input.write_all(&length.to_le_bytes()).await?;
    input.write_all(&body).await?;
    input.flush().await
}

/// Reads the child's output. What comes before [`MAGIC`] is skipped, and
/// then each frame goes to `frames` with its size, which `held` counts from
/// the moment it is read. A frame that is too large or can't be read is
/// the child's fault, which goes to `fault`, and the reading stops. The
/// channel closes when the output ends.
async fn read_frames(
    mut output: tokio::process::ChildStdout,
    frames: mpsc::Sender<(FromChild, usize)>,
    fault: tokio::sync::watch::Sender<Option<String>>,
    held: Arc<Held>,
) {
    match skip_to_magic(&mut output).await {
        Magic::Found => {}
        Magic::Ended => return,
        Magic::Missing => {
            fault.send_replace(Some(
                "The program started as the code runner isn't one.".into(),
            ));
            return;
        }
    }
    loop {
        let mut length = [0; 4];
        if output.read_exact(&mut length).await.is_err() {
            return;
        }
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_FRAME_FROM_CHILD_BYTES {
            fault.send_replace(Some(format!(
                "The code runner sent a frame of {length} bytes."
            )));
            return;
        }
        held.take(length);
        let mut body = vec![0; length];
        if output.read_exact(&mut body).await.is_err() {
            return;
        }
        let frame = match serde_json::from_slice(&body) {
            Ok(frame) => frame,
            Err(error) => {
                fault.send_replace(Some(format!(
                    "The code runner sent a frame Arko can't read ({error})."
                )));
                return;
            }
        };
        drop(body);
        if frames.send((frame, length)).await.is_err() {
            return;
        }
    }
}

/// How the output began.
enum Magic {
    Found,
    /// It ended first.
    Ended,
    /// 64 KB came without it.
    Missing,
}

/// Reads up to the end of [`MAGIC`], at most 64 KB in.
async fn skip_to_magic(output: &mut tokio::process::ChildStdout) -> Magic {
    let mut seen: Vec<u8> = Vec::new();
    let mut byte = [0; 1];
    while seen.len() < 64 * 1024 {
        if output.read_exact(&mut byte).await.is_err() {
            return Magic::Ended;
        }
        seen.push(byte[0]);
        if seen.ends_with(MAGIC) {
            return Magic::Found;
        }
    }
    Magic::Missing
}

/// The first `most` bytes of `input`, read to its end.
async fn read_start(mut input: impl AsyncRead + Unpin, most: usize) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0; 4096];
    while let Ok(count) = input.read(&mut chunk).await {
        if count == 0 {
            break;
        }
        let room = most.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..count.min(room)]);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// The resident memory of process `pid`, where the system says.
#[cfg(target_os = "linux")]
fn resident_bytes(pid: u32) -> Option<u64> {
    // The second field of statm, in pages.
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` reads a constant of the system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * u64::try_from(page).ok()?)
}

#[cfg(target_os = "macos")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` is the structure the flavor fills in, with its size.
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (read == size).then_some(info.pti_resident_size)
}

/// (Windows limits the runner's memory with its Job Object.)
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resident_bytes(_: u32) -> Option<u64> {
    None
}

fn describe_status(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                libc::SIGKILL => " (SIGKILL)",
                libc::SIGXCPU => " (SIGXCPU, its processor time ran out)",
                libc::SIGABRT => " (SIGABRT)",
                libc::SIGSEGV => " (SIGSEGV)",
                _ => "",
            };
            return format!("it was ended by signal {signal}{name}");
        }
    }
    match status.code() {
        Some(code) => format!("it exited with code {code}"),
        None => "it ended".into(),
    }
}

#[cfg(test)]
#[path = "child_tests.rs"]
pub(super) mod tests;
