//! The code runner as a child process: its OS limits, each way a run ends,
//! and Arko's side of the protocol. The runner is this test program, run as
//! the test [`runner`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use arko_agents::StopSource;
use arko_agents::{NestedOutcome, NestedRecord, NestedState, StopSignal, ToolOutput};
use async_trait::async_trait;
use serde_json::Value;

use super::{Gate, Outcome, Run, Runner};
use crate::code_mode::protocol::{Ended, Limits, Start};
use crate::code_mode::{
    COMPUTE_SECONDS, CPU_SECONDS, HEAP_BYTES, MAX_CALLS, OS_ADDRESS_SPACE_BYTES, OS_MEMORY_BYTES,
    STACK_BYTES,
};

/// Set in the runner's environment, to what the runner does.
const RUNNER_TEST_ENV: &str = "ARKO_TEST_CODE_RUNNER";
/// Set in a runner's environment to tell it from the other tests'.
#[cfg(target_os = "linux")]
const MARK_ENV: &str = "ARKO_TEST_RUNNER_MARK";
const MB: u64 = 1024 * 1024;

/// The command that starts this test program as the runner.
pub(super) fn runner_command() -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    runner_doing("run")
}

fn runner_doing(what: &str) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    (
        [
            "--exact",
            "code_mode::child::tests::runner",
            "--nocapture",
            "--test-threads",
            "1",
        ]
        .map(OsString::from)
        .to_vec(),
        vec![(RUNNER_TEST_ENV.into(), what.into())],
    )
}

/// The runner, when this test program is started as one. With
/// `call-then-exit`, it sends one call and ends, as a runner that crashes
/// would.
#[test]
fn runner() {
    match std::env::var(RUNNER_TEST_ENV).as_deref() {
        Ok("run") => super::super::runner::run(),
        Ok("forge") => {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(crate::code_mode::protocol::MAGIC).unwrap();
            for id in 1..=3 {
                let call =
                    crate::code_mode::protocol::FromChild::Call(crate::code_mode::protocol::Call {
                        id,
                        tool: "terminal_start".into(),
                        args: serde_json::json!({"command": "sleep 600"}),
                    });
                crate::code_mode::protocol::write_frame(&mut out, &call).unwrap();
            }
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
        Ok("forged-done") => {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(crate::code_mode::protocol::MAGIC).unwrap();
            let done =
                crate::code_mode::protocol::FromChild::Done(crate::code_mode::protocol::Done {
                    ended: Ended::Returned {
                        value: Some("x".repeat(40_000)),
                        unread: Vec::new(),
                    },
                    log: vec!["a line".into()],
                    log_left_out: u64::MAX,
                    value_left_out: u64::MAX,
                    calls: u32::MAX,
                    heap_bytes: u64::MAX,
                    compute_ms: u64::MAX,
                });
            crate::code_mode::protocol::write_frame(&mut out, &done).unwrap();
            std::process::exit(0);
        }
        Ok("garbage-then-deaf") => {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(crate::code_mode::protocol::MAGIC).unwrap();
            let call =
                crate::code_mode::protocol::FromChild::Call(crate::code_mode::protocol::Call {
                    id: 1,
                    tool: "large".into(),
                    args: serde_json::json!({}),
                });
            crate::code_mode::protocol::write_frame(&mut out, &call).unwrap();
            // Arko's answer fills the pipe this never reads, and then the
            // frame Arko can't read comes.
            std::thread::sleep(Duration::from_secs(1));
            out.write_all(&5u32.to_le_bytes()).unwrap();
            out.write_all(b"notjs").unwrap();
            out.flush().unwrap();
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
        Ok(what @ ("call-then-exit" | "call-then-garbage")) => {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(crate::code_mode::protocol::MAGIC).unwrap();
            let call =
                crate::code_mode::protocol::FromChild::Call(crate::code_mode::protocol::Call {
                    id: 1,
                    tool: "shell".into(),
                    args: serde_json::json!({"command": "sleep 600"}),
                });
            crate::code_mode::protocol::write_frame(&mut out, &call).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            if what == "call-then-exit" {
                std::process::exit(3);
            }
            out.write_all(&5u32.to_le_bytes()).unwrap();
            out.write_all(b"notjs").unwrap();
            out.flush().unwrap();
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
        _ => {}
    }
}

/// A gate that runs nothing. `echo` answers with its arguments, `block`
/// waits until dropped, `refuse` is refused, `halt` halts, `big` returns
/// 9 MB, and `shell` and `write_file` record themselves as run. Each call
/// that starts is recorded, as the real gate records it.
#[derive(Default)]
struct FakeGate {
    records: Mutex<Vec<NestedRecord>>,
    calls: AtomicUsize,
    blocked: AtomicBool,
    dropped: Arc<AtomicBool>,
    release: tokio::sync::Notify,
}

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Gate for FakeGate {
    async fn call(&self, tool: &str, params: HashMap<String, Value>) -> NestedOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let id = uuid::Uuid::new_v4();
        let start = |state| {
            self.records.lock().unwrap().push(NestedRecord {
                tool_call_id: id,
                tool_name: tool.into(),
                state,
            })
        };
        let text = serde_json::to_string(&params).unwrap();
        match tool {
            "refuse" => NestedOutcome::Refused("Refused: not from here.".into()),
            "halt" => NestedOutcome::Halted("Budget exhausted.".into()),
            "big" => NestedOutcome::Finished(ToolOutput::text("x".repeat(9 * MB as usize))),
            "large" => NestedOutcome::Finished(ToolOutput::text("x".repeat(7 * MB as usize))),
            // An edit that may have changed its file, then failed.
            "edit_file" => {
                start(NestedState::Ran { failed: true });
                NestedOutcome::Failed("no space left on device".into())
            }
            "block" | "shell" => {
                start(NestedState::Running);
                let guard = Dropped(self.dropped.clone());
                self.blocked.store(true, Ordering::SeqCst);
                self.release.notified().await;
                std::mem::forget(guard);
                NestedOutcome::Finished(ToolOutput::text("released"))
            }
            _ => {
                start(NestedState::Ran { failed: false });
                NestedOutcome::Finished(ToolOutput::text(text))
            }
        }
    }

    fn records(&self) -> Vec<NestedRecord> {
        self.records.lock().unwrap().clone()
    }
}

/// Waits until a call of `gate` blocks, for at most 20 seconds.
async fn until_blocked(gate: &FakeGate) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !gate.blocked.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a call blocked in time");
}

fn limits() -> Limits {
    Limits {
        compute_ms: COMPUTE_SECONDS * 1000,
        heap_bytes: HEAP_BYTES,
        stack_bytes: STACK_BYTES,
        calls: MAX_CALLS,
        os_memory_bytes: OS_MEMORY_BYTES,
        os_address_space_bytes: OS_ADDRESS_SPACE_BYTES,
        cpu_seconds: CPU_SECONDS,
    }
}

fn start(code: &str, limits: Limits) -> Start {
    Start {
        code: code.into(),
        tools: [
            "echo",
            "block",
            "refuse",
            "halt",
            "big",
            "shell",
            "write_file",
            "edit_file",
        ]
        .map(String::from)
        .to_vec(),
        limits,
    }
}

async fn run_with(code: &str, limits: Limits, gate: &FakeGate, timeout: Duration) -> Run {
    let runner = Runner::this_program().unwrap();
    super::run(
        &runner,
        start(code, limits),
        gate,
        timeout,
        &StopSignal::never(),
    )
    .await
}

async fn run(code: &str) -> Run {
    run_with(
        code,
        limits(),
        &FakeGate::default(),
        Duration::from_secs(60),
    )
    .await
}

fn returned(run: &Run) -> String {
    match &run.outcome {
        Outcome::Done(done) => match &done.ended {
            Ended::Returned { value, .. } => value.clone().unwrap_or_default(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn ended(run: &Run) -> &Ended {
    match &run.outcome {
        Outcome::Done(done) => &done.ended,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_run_in_a_child_returns_its_value_and_its_calls_answers() {
    assert_eq!(returned(&run("return 6 * 7;").await), "42");
    let ran = run("const a = await tools.echo({ n: 1 }); return JSON.parse(a).n + 1;").await;
    assert_eq!(returned(&ran), "2");
    assert_eq!(ran.calls, 1);
}

/// Its own environment, none of Arko's (so none of its keys), and a
/// working directory of no project's.
#[tokio::test]
async fn the_runner_gets_none_of_arkos_environment() {
    // SAFETY: tests set only variables no other test reads.
    unsafe { std::env::set_var("ARKO_TEST_SECRET_KEY", "sk-not-for-the-child") };
    let ran = run("return 1;").await;
    assert_eq!(returned(&ran), "1");
    let runner = Runner::this_program().unwrap();
    assert!(
        runner
            .env
            .iter()
            .all(|(name, _)| name != "ARKO_TEST_SECRET_KEY")
    );
}

#[tokio::test]
async fn refusals_end_the_run_at_the_third() {
    let gate = FakeGate::default();
    let ran = run_with(
        "for (let i = 0; i < 5; i++) { try { await tools.refuse({}); } catch {} } return 'went on';",
        limits(),
        &gate,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        matches!(&ran.outcome, Outcome::Refusals { last } if last == "Refused: not from here."),
        "{:?}",
        ran.outcome
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_halt_at_a_call_ends_the_run() {
    let ran = run("await tools.echo({}); await tools.halt({}); return 1;").await;
    assert!(
        matches!(&ran.outcome, Outcome::Halted { tool, message } if tool == "halt" && message == "Budget exhausted."),
        "{:?}",
        ran.outcome
    );
}

#[tokio::test]
async fn a_result_too_large_for_the_code_fails_its_call() {
    let ran = run("try { await tools.big({}); } catch (e) { return e.message; }").await;
    assert_eq!(
        returned(&ran),
        "The result of big is 9 MB, over the 8 MB a result into code may be."
    );
}

/// The code has only the tools it was given, and Arko refuses a call of
/// any other, as from a runner that sends one it wasn't given.
#[tokio::test]
async fn a_tool_the_run_wasnt_given_is_refused() {
    let mut given = start(
        "return [typeof tools.block, await tools.echo({})].join(' ');",
        limits(),
    );
    given.tools = vec!["echo".into()];
    let gate = FakeGate::default();
    let runner = Runner::this_program().unwrap();
    let ran = super::run(
        &runner,
        given,
        &gate,
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert_eq!(returned(&ran), "undefined {}");
    let (args, env) = runner_doing("forge");
    let forged = Runner {
        args,
        env,
        ..Runner::this_program().unwrap()
    };
    let gate = FakeGate::default();
    let ran = super::run(
        &forged,
        start("", limits()),
        &gate,
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert!(
        matches!(&ran.outcome, Outcome::Refusals { last } if last == "terminal_start can't be called from code."),
        "{:?}",
        ran.outcome
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_whole_run_has_a_time_limit() {
    let began = Instant::now();
    let gate = FakeGate::default();
    let ran = run_with(
        "await tools.block({});",
        limits(),
        &gate,
        Duration::from_millis(500),
    )
    .await;
    assert!(
        matches!(ran.outcome, Outcome::TimedOut),
        "{:?}",
        ran.outcome
    );
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(
        gate.dropped.load(Ordering::SeqCst),
        "the call in flight is dropped"
    );
}

/// A command that may have run counts among the changes, also when the
/// run ends while it runs.
#[tokio::test]
async fn what_changed_counts_a_call_that_was_still_running() {
    let gate = FakeGate::default();
    let ran = run_with(
        "await tools.write_file({ path: 'a', content: 'b' }); await tools.shell({ command: 'sleep 600' });",
        limits(),
        &gate,
        Duration::from_millis(800),
    )
    .await;
    assert!(
        matches!(ran.outcome, Outcome::TimedOut),
        "{:?}",
        ran.outcome
    );
    assert_eq!(
        ran.changes,
        super::Changes {
            files_written: 1,
            files_edited: 0,
            commands: 1,
            files_maybe_changed: 0,
        }
    );
}

/// An edit that failed after it started may have changed its file, and a
/// failed run says so.
#[tokio::test]
async fn a_failed_edit_counts_as_a_change_it_may_have_made() {
    let ran = run(
        "try { await tools.edit_file({ path: 'a', old_string: 'b', new_string: 'c' }); } catch {}\n\
         throw new Error('after the edit');",
    )
    .await;
    assert_eq!(ran.changes.files_maybe_changed, 1);
    let shown = crate::code_mode::tool::words::result(&ran, 300).unwrap_err();
    assert_eq!(
        shown,
        "The code threw on line 2: Error: after the edit\nA write or edit that failed or was stopped may have changed its file."
    );
}

/// The resident memory a process has, on Linux.
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .split_whitespace()
            .nth(2)
            .is_some_and(|state| state == "Z")
    })
}

/// The runner, marked with `mark`.
#[cfg(target_os = "linux")]
fn marked(mark: &str) -> Runner {
    let mut runner = Runner::this_program().unwrap();
    runner.env.push((MARK_ENV.into(), mark.into()));
    runner
}

/// The runners this test program started with `mark`, by their process
/// IDs.
#[cfg(target_os = "linux")]
fn runners(mark: &str) -> Vec<u32> {
    let me = std::process::id();
    let wanted = format!("{MARK_ENV}={mark}");
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let parent = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let after = stat.rsplit_once(')')?.1.to_string();
                    after.split_whitespace().nth(1)?.parse::<u32>().ok()
                });
            parent == Some(me)
                && std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                    environ
                        .split(|byte| *byte == 0)
                        .any(|variable| variable == wanted.as_bytes())
                })
        })
        .filter(|pid| alive(*pid))
        .collect()
}

/// A stop ends the child within a second, while the code loops and while
/// a call is in flight, and drops the call.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stop_kills_the_child_while_it_loops_and_while_a_call_runs() {
    for code in ["while (true) {}", "await tools.block({});"] {
        let gate = Arc::new(FakeGate::default());
        let source = StopSource::new();
        let signal = source.signal();
        let mark = uuid::Uuid::new_v4().to_string();
        let task = tokio::spawn({
            let (gate, mark) = (gate.clone(), mark.clone());
            async move {
                let runner = marked(&mark);
                super::run(
                    &runner,
                    start(code, limits()),
                    gate.as_ref(),
                    Duration::from_secs(60),
                    &signal,
                )
                .await
            }
        });
        let began = Instant::now();
        let child = loop {
            let found = runners(&mark);
            if let Some(pid) = found.first() {
                break *pid;
            }
            assert!(began.elapsed() < Duration::from_secs(10), "no runner");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        if code.contains("block") {
            until_blocked(&gate).await;
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let stopped = Instant::now();
        source.stop("Stopped.");
        let ran = task.await.unwrap();
        assert!(matches!(ran.outcome, Outcome::Stopped), "{:?}", ran.outcome);
        while alive(child) {
            assert!(
                stopped.elapsed() < Duration::from_secs(1),
                "{code}: the runner lives on"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if code.contains("block") {
            assert!(gate.dropped.load(Ordering::SeqCst));
        }
    }
    // And dropping the run's future does the same.
    let gate = Arc::new(FakeGate::default());
    let mark = uuid::Uuid::new_v4().to_string();
    let task = tokio::spawn({
        let (gate, mark) = (gate.clone(), mark.clone());
        async move {
            super::run(
                &marked(&mark),
                start("while (true) {}", limits()),
                gate.as_ref(),
                Duration::from_secs(60),
                &StopSignal::never(),
            )
            .await
        }
    });
    let began = Instant::now();
    let child = loop {
        if let Some(pid) = runners(&mark).first() {
            break *pid;
        }
        assert!(began.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    task.abort();
    let _ = task.await;
    let dropped = Instant::now();
    while alive(child) {
        assert!(
            dropped.elapsed() < Duration::from_secs(1),
            "the runner lives on"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The runner's OS limits are set before its code runs, and its
/// oom_score_adj is raised so it goes first.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_runner_runs_under_its_os_limits() {
    let gate = Arc::new(FakeGate::default());
    let mark = uuid::Uuid::new_v4().to_string();
    let task = tokio::spawn({
        let (gate, mark) = (gate.clone(), mark.clone());
        async move {
            super::run(
                &marked(&mark),
                start("await tools.block({});", limits()),
                gate.as_ref(),
                Duration::from_secs(60),
                &StopSignal::never(),
            )
            .await
        }
    });
    until_blocked(&gate).await;
    let child = runners(&mark)[0];
    let adj = std::fs::read_to_string(format!("/proc/{child}/oom_score_adj")).unwrap();
    assert_eq!(adj.trim(), "1000");
    let limits = std::fs::read_to_string(format!("/proc/{child}/limits")).unwrap();
    let soft = |name: &str| -> String {
        let line = limits.lines().find(|line| line.starts_with(name)).unwrap();
        line[name.len()..]
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(soft("Max data size"), OS_MEMORY_BYTES.to_string());
    assert_eq!(soft("Max cpu time"), CPU_SECONDS.to_string());
    assert_eq!(soft("Max core file size"), "0");
    assert_eq!(soft("Max file size"), "0");
    let address_space: u64 = soft("Max address space").parse().unwrap();
    assert!(address_space > OS_ADDRESS_SPACE_BYTES && address_space < 4 * OS_ADDRESS_SPACE_BYTES);
    gate.release.notify_one();
    assert_eq!(returned(&task.await.unwrap()), "");
}

/// Memory past the heap's count, here with the heap's cap set past the
/// operating system's, is stopped by the operating system's limit, and the
/// run ends with an error, not Arko.
#[tokio::test]
async fn a_runaway_allocation_past_the_heap_cap_is_stopped_by_the_os() {
    let mut loose = limits();
    loose.heap_bytes = 8 * 1024 * MB;
    loose.os_memory_bytes = 256 * MB;
    loose.os_address_space_bytes = 512 * MB;
    let began = Instant::now();
    let ran = run_with(
        "const all = []; while (true) { all.push(new Array(1000000).fill(7)); }",
        loose,
        &FakeGate::default(),
        Duration::from_secs(60),
    )
    .await;
    println!("a runaway allocation: {:?}", ran.outcome);
    match &ran.outcome {
        Outcome::Done(done) => assert!(
            matches!(done.ended, Ended::OutOfMemory { .. }),
            "{:?}",
            done.ended
        ),
        Outcome::Died { .. } | Outcome::OverMemory => {}
        other => panic!("{other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(30));
}

/// The address-space limit alone (the watchdog and `RLIMIT_DATA` loose,
/// the heap's cap at 2 GB) stops a runaway allocation well before the
/// heap's cap would. Each array holds about 16 MB, so the heap's cap
/// would take some 120 of them, and 512 MB of address space about 30.
#[cfg(unix)]
#[tokio::test]
async fn the_address_space_limit_stops_a_runaway_allocation() {
    let mut loose = limits();
    loose.heap_bytes = 2 * 1024 * MB;
    loose.os_memory_bytes = 16 * 1024 * MB;
    loose.os_address_space_bytes = 512 * MB;
    let ran = run_with(
        "const all = [];\n\
         try { while (true) all.push(new Array(1000000).fill(7)); } catch (e) { return `${all.length} ${e.message}`; }",
        loose,
        &FakeGate::default(),
        Duration::from_secs(60),
    )
    .await;
    println!("the address space alone: {:?}", ran.outcome);
    match &ran.outcome {
        Outcome::Done(_) => {
            let shown = returned(&ran);
            let (arrays, why) = shown.split_once(' ').unwrap();
            let arrays: usize = arrays.parse().unwrap();
            assert!(arrays < 60, "{shown}");
            assert_eq!(why, "out of memory");
        }
        // An allocation of the runner's own failed, which aborts it.
        Outcome::Died { status, .. } => assert!(status.contains("signal"), "{status}"),
        other => panic!("{other:?}"),
    }
}

/// The processor-time limit (`RLIMIT_CPU`, or the Job Object's) ends a
/// runner whose computing time limit is looser, as a fault in the engine's
/// own count would.
#[tokio::test]
async fn the_processor_time_limit_ends_a_runaway_runner() {
    let mut loose = limits();
    loose.compute_ms = 600_000;
    loose.cpu_seconds = 1;
    let began = Instant::now();
    let ran = run_with(
        "while (true) {}",
        loose,
        &FakeGate::default(),
        Duration::from_secs(60),
    )
    .await;
    println!(
        "the processor time: {:?} after {:?}",
        ran.outcome,
        began.elapsed()
    );
    match &ran.outcome {
        Outcome::Died { status, .. } => {
            if cfg!(unix) {
                assert!(status.contains("signal"), "{status}");
            }
        }
        other => panic!("{other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(30));
}

/// A runner that ends while one of its calls runs ends the run at once,
/// and the call (here a command) is dropped.
#[tokio::test]
async fn a_runner_that_dies_during_a_call_drops_the_call() {
    let gate = FakeGate::default();
    let (args, env) = runner_doing("call-then-exit");
    let runner = Runner {
        args,
        env,
        ..Runner::this_program().unwrap()
    };
    let began = Instant::now();
    let ran = super::run(
        &runner,
        start("", limits()),
        &gate,
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    match &ran.outcome {
        Outcome::Died { status, .. } => assert!(status.contains("code 3"), "{status}"),
        other => panic!("{other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(10));
    assert!(gate.dropped.load(Ordering::SeqCst));
    assert_eq!(ran.changes.commands, 1);
}

/// A runner that breaks the protocol while one of its calls runs ends the
/// run at once, and the call is dropped.
#[tokio::test]
async fn a_broken_frame_during_a_call_ends_the_run() {
    let gate = FakeGate::default();
    let (args, env) = runner_doing("call-then-garbage");
    let runner = Runner {
        args,
        env,
        ..Runner::this_program().unwrap()
    };
    let began = Instant::now();
    let ran = super::run(
        &runner,
        start("", limits()),
        &gate,
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert!(
        matches!(&ran.outcome, Outcome::Broken(why) if why.starts_with("The code runner sent a frame Arko can't read")),
        "{:?}",
        ran.outcome
    );
    assert!(began.elapsed() < Duration::from_secs(10));
    assert!(gate.dropped.load(Ordering::SeqCst));
}

/// A runner's counts are the child's word, so the largest it can send
/// don't upset Arko's arithmetic.
#[tokio::test]
async fn a_forged_done_with_the_largest_counts_reads_cleanly() {
    let (args, env) = runner_doing("forged-done");
    let runner = Runner {
        args,
        env,
        ..Runner::this_program().unwrap()
    };
    let ran = super::run(
        &runner,
        start("", limits()),
        &FakeGate::default(),
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    let shown = crate::code_mode::tool::words::result(&ran, 300).unwrap();
    assert!(shown.starts_with("a line\n["), "{}", &shown[..40]);
    assert!(
        shown.ends_with(" more characters left out]"),
        "{}",
        &shown[shown.len() - 60..]
    );
}

/// A runner that breaks the protocol while Arko waits to write it an answer
/// it doesn't read ends the run at once.
#[tokio::test]
async fn a_broken_frame_while_an_answer_waits_to_be_written_ends_the_run() {
    let (args, env) = runner_doing("garbage-then-deaf");
    let runner = Runner {
        args,
        env,
        ..Runner::this_program().unwrap()
    };
    let mut tools = start("", limits());
    tools.tools.push("large".into());
    let began = Instant::now();
    let ran = super::run(
        &runner,
        tools,
        &FakeGate::default(),
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert!(
        matches!(&ran.outcome, Outcome::Broken(why) if why.starts_with("The code runner sent a frame Arko can't read")),
        "{:?}",
        ran.outcome
    );
    assert!(began.elapsed() < Duration::from_secs(10));
}

/// A runner whose code throws while a call it didn't await runs sends its
/// end and exits, and the run ends with the code's error.
#[tokio::test]
async fn a_runner_that_ends_with_a_done_during_a_call_reports_it() {
    let gate = FakeGate::default();
    let ran = run_with(
        "tools.block({}); await null; throw new Error('boom');",
        limits(),
        &gate,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        matches!(ended(&ran), Ended::Threw { message, .. } if message == "Error: boom"),
        "{:?}",
        ran.outcome
    );
    assert!(gate.dropped.load(Ordering::SeqCst));
}

/// A runner that isn't one: its output ends before the magic line, or it
/// sends a frame Arko can't read.
#[tokio::test]
async fn a_program_that_isnt_a_runner_is_a_clean_error() {
    let ran = super::run(
        &Runner {
            program: std::env::current_exe().unwrap(),
            args: ["--exact", "no::such::test"].map(OsString::from).to_vec(),
            env: Vec::new(),
        },
        start("return 1;", limits()),
        &FakeGate::default(),
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert!(
        matches!(ran.outcome, Outcome::Died { .. }),
        "{:?}",
        ran.outcome
    );
    let ran = super::run(
        &Runner {
            program: "/no/such/program".into(),
            args: Vec::new(),
            env: Vec::new(),
        },
        start("return 1;", limits()),
        &FakeGate::default(),
        Duration::from_secs(60),
        &StopSignal::never(),
    )
    .await;
    assert!(
        matches!(ran.outcome, Outcome::Broken(_)),
        "{:?}",
        ran.outcome
    );
}

/// Calls fanned out with large arguments while the first waits: Arko holds
/// at most two of them, and the time the code waits on the full pipe isn't
/// computing time.
#[tokio::test]
async fn a_fan_out_behind_a_blocked_call_is_held_back_and_isnt_computing() {
    let gate = Arc::new(FakeGate::default());
    let mut tight = limits();
    tight.compute_ms = 2_000;
    let task = tokio::spawn({
        let gate = gate.clone();
        async move {
            run_with(
                "const big = 'x'.repeat(4 * 1024 * 1024);\n\
                 const first = tools.block({});\n\
                 const rest = [];\n\
                 for (let i = 0; i < 12; i++) rest.push(tools.echo({ i, big }));\n\
                 await first;\n\
                 return (await Promise.all(rest)).length;",
                tight,
                gate.as_ref(),
                Duration::from_secs(60),
            )
            .await
        }
    });
    until_blocked(&gate).await;
    // Longer than the computing limit, while the code's writes wait.
    tokio::time::sleep(Duration::from_secs(3)).await;
    gate.release.notify_one();
    let ran = task.await.unwrap();
    assert_eq!(returned(&ran), "12");
    assert!(
        ran.most_held <= 2 * (4 * MB as usize + 64 * 1024),
        "{} bytes held",
        ran.most_held
    );
}

/// A value larger than a frame from the runner may be is cut in the
/// runner, which says how much it left out.
#[tokio::test]
async fn a_long_value_is_cut_in_the_runner() {
    let ran = run("return 'é'.repeat(10 * 1024 * 1024);").await;
    let Outcome::Done(done) = &ran.outcome else {
        panic!("{:?}", ran.outcome);
    };
    assert_eq!(
        returned(&ran).chars().count(),
        crate::code_mode::MAX_RESULT_CHARS
    );
    assert_eq!(
        done.value_left_out,
        (10 * 1024 * 1024 - crate::code_mode::MAX_RESULT_CHARS) as u64
    );
    let shown = crate::code_mode::tool::words::result(&ran, 300).unwrap();
    assert!(
        shown.ends_with("\n[10,455,760 more characters left out]"),
        "{}",
        &shown[shown.len() - 60..]
    );
}

/// Arguments over the most a call may send fail the call in the runner.
#[tokio::test]
async fn arguments_too_large_fail_the_call() {
    let ran = run(
        "try { await tools.echo({ big: 'x'.repeat(9 * 1024 * 1024) }); } catch (e) { return e.message; }",
    )
    .await;
    assert_eq!(
        returned(&ran),
        "The arguments of tools.echo are 9 MB, over the 8 MB a call may send."
    );
}

/// Several runs at once on a multi-thread runtime, many times over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_at_once_on_a_multi_thread_runtime() {
    let gate = Arc::new(FakeGate::default());
    let rounds = tokio::time::timeout(Duration::from_secs(120), async {
        for round in 0..5 {
            let runs: Vec<_> = (0..4)
                .map(|n| {
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        run_with(
                            &format!("const a = await tools.echo({{ n: {n} }}); return JSON.parse(a).n * {round};"),
                            limits(),
                            gate.as_ref(),
                            Duration::from_secs(60),
                        )
                        .await
                    })
                })
                .collect();
            for (n, task) in runs.into_iter().enumerate() {
                assert_eq!(returned(&task.await.unwrap()), (n * round).to_string());
            }
        }
    });
    rounds.await.expect("the runs made progress");
}

/// How long a run takes to start: the process and the engine, for a run
/// that returns at once. Prints the median of 20.
#[tokio::test]
#[ignore = "a measurement, run with --ignored --nocapture"]
async fn start_time() {
    let mut times = Vec::new();
    for _ in 0..20 {
        let ran = run("return 1;").await;
        assert_eq!(returned(&ran), "1");
        times.push(ran.took);
    }
    times.sort();
    println!(
        "a run of `return 1` in a child, 20 runs: median {:?}, fastest {:?}, slowest {:?}",
        times[10], times[0], times[19]
    );
}
