//! Runs code on an [`Interpreter`]: the prelude, the code as the body of an
//! async function, and the loop that hands its tool calls out and their
//! answers in. It runs on the interpreter's thread, in the code runner's
//! process, and in the engine tests. It decides why a run ended from its
//! own flags, never from the engine's text.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::engine::{EngineError, EngineLimits, Interpreter, line_in};
use super::protocol::{Answer, Call, Done, Ended, Limits, Start};
use super::{MAX_CALL_BYTES, MAX_LOG_BYTES, MAX_MESSAGE_CHARS, MAX_RESULT_CHARS, MAX_UNREAD};

/// The prelude's file name in stacks.
const PRELUDE_FILE: &str = "prelude.js";
/// The code's file name in stacks.
const CODE_FILE: &str = "code.js";
const PRELUDE: &str = include_str!("prelude.js");

/// Where the code's calls go and their answers come from.
pub trait Calls {
    /// Sends `call` on. False when nobody takes it.
    fn send(&mut self, call: Call) -> bool;
    /// Waits for the next answer. `None` when none will come.
    fn answer(&mut self) -> Option<Answer>;
}

/// What the prelude tells the host, through `__host`.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum FromPrelude {
    Call {
        id: u64,
        tool: String,
        args: serde_json::Value,
    },
    Log {
        line: String,
    },
    Done {
        value: Option<String>,
        unread: Vec<String>,
    },
    Failed {
        refused: bool,
        tool: Option<String>,
    },
}

/// Why the host itself stopped the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    ComputeLimit,
    CallLimit,
    /// `stop` was set from outside.
    Asked,
}

/// What the prelude has said, and the host's count of the run.
#[derive(Default)]
struct State {
    log: Vec<String>,
    log_bytes: usize,
    log_left_out: u64,
    value_left_out: u64,
    calls: u32,
    /// Calls sent and not answered yet.
    outstanding: u32,
    done: Option<Ended>,
    failed: Option<(bool, Option<String>)>,
    /// A message from the prelude the host couldn't read.
    garbled: Option<String>,
}

/// How long the code has computed, and whether it may go on.
struct Clock {
    limit: Duration,
    spent: Cell<Duration>,
    since: Cell<Option<Instant>>,
    stopped: Cell<Option<Stopped>>,
}

impl Clock {
    fn used(&self) -> Duration {
        self.spent.get()
            + self
                .since
                .get()
                .map_or(Duration::ZERO, |since| since.elapsed())
    }

    /// Times `f`, which runs code.
    fn computing<T>(&self, f: impl FnOnce() -> T) -> T {
        self.since.set(Some(Instant::now()));
        let result = f();
        self.spent.set(self.used());
        self.since.set(None);
        result
    }

    /// Runs `f`, which may wait (for room in Arko's pipe), off the clock.
    fn waiting<T>(&self, f: impl FnOnce() -> T) -> T {
        let computing = self.since.get().is_some();
        if computing {
            self.spent.set(self.used());
            self.since.set(None);
        }
        let result = f();
        if computing {
            self.since.set(Some(Instant::now()));
        }
        result
    }
}

/// Runs `start`'s code on a fresh `E` and says how it ended. Its calls go
/// to `calls`, at most `start.limits.calls` of them, and `stop` stops it
/// from outside.
pub fn run<E: Interpreter>(
    start: &Start,
    calls: impl Calls + 'static,
    stop: Arc<AtomicBool>,
) -> Done {
    let limits = start.limits;
    let state = Rc::new(RefCell::new(State::default()));
    let clock = Rc::new(Clock {
        limit: Duration::from_millis(limits.compute_ms),
        spent: Cell::new(Duration::ZERO),
        since: Cell::new(None),
        stopped: Cell::new(None),
    });
    let calls = Rc::new(RefCell::new(calls));
    let polled = {
        let clock = clock.clone();
        move || {
            if clock.stopped.get().is_some() {
                return true;
            }
            let why = if stop.load(Ordering::Relaxed) {
                Some(Stopped::Asked)
            } else if clock.used() > clock.limit {
                Some(Stopped::ComputeLimit)
            } else {
                None
            };
            clock.stopped.set(why);
            why.is_some()
        }
    };
    let to_host = {
        let (state, clock, calls) = (state.clone(), clock.clone(), calls.clone());
        move |text: &str| hear(text, &state, &clock, &calls, limits)
    };
    let engine_limits = EngineLimits {
        heap_bytes: usize::try_from(limits.heap_bytes).unwrap_or(usize::MAX),
        stack_bytes: usize::try_from(limits.stack_bytes).unwrap_or(usize::MAX),
    };
    let ended = match E::start(engine_limits, Box::new(polled), Box::new(to_host)) {
        Ok(mut engine) => {
            let ended = drive(&mut engine, start, &state, &clock, &calls);
            let heap = engine.heap_bytes();
            (ended, heap)
        }
        Err(error) => (failed(error), 0),
    };
    let (ended, heap_bytes) = ended;
    let state = state.take();
    Done {
        ended,
        log: state.log,
        log_left_out: state.log_left_out,
        value_left_out: state.value_left_out,
        calls: state.calls,
        heap_bytes,
        compute_ms: u64::try_from(clock.used().as_millis()).unwrap_or(u64::MAX),
    }
}

/// The prelude, the code, and then answers in until the code ends.
fn drive<E: Interpreter>(
    engine: &mut E,
    start: &Start,
    state: &RefCell<State>,
    clock: &Clock,
    calls: &RefCell<impl Calls>,
) -> Ended {
    let setup = engine.eval(PRELUDE_FILE, PRELUDE).and_then(|()| {
        let names = serde_json::to_string(&start.tools).unwrap_or_else(|_| "[]".into());
        engine.call("__setup", &names)
    });
    if let Err(error) = setup {
        return Ended::Failed {
            message: format!("The prelude failed: {error:?}"),
        };
    }
    // On the wrapper's first line, so the code's lines keep their numbers.
    let wrapped = format!("__run(async () => {{{}\n}});", start.code);
    if let Err(error) = clock.computing(|| engine.eval(CODE_FILE, &wrapped)) {
        return ended_by(error, true, clock);
    }
    loop {
        if let Err(error) = clock.computing(|| engine.run_jobs()) {
            return ended_by(error, false, clock);
        }
        if let Some(stopped) = clock.stopped.get() {
            return limit_ended(stopped);
        }
        let mut current = state.borrow_mut();
        if let Some(message) = current.garbled.take() {
            return Ended::Failed { message };
        }
        if let Some(done) = current.done.take() {
            return done;
        }
        if let Some((refused, tool)) = current.failed.take() {
            drop(current);
            // The prelude kept what the code threw, for the engine to tell.
            return match clock.computing(|| engine.call("__rethrow", "")) {
                Err(EngineError::Thrown {
                    message: text,
                    frames,
                }) => Ended::Threw {
                    line: line_in(&frames, CODE_FILE),
                    message: message(text),
                    compiling: false,
                    refused,
                    tool,
                },
                Err(error) => ended_by(error, false, clock),
                Ok(()) => Ended::Failed {
                    message: "The code failed with nothing thrown.".into(),
                },
            };
        }
        if current.outstanding == 0 {
            return Ended::Unsettled;
        }
        drop(current);
        let Some(answer) = calls.borrow_mut().answer() else {
            return Ended::Failed {
                message: "Arko stopped answering.".into(),
            };
        };
        state.borrow_mut().outstanding -= 1;
        let text = serde_json::json!({
            "id": answer.id,
            "ok": answer.ok,
            "text": answer.text,
            "refused": answer.refused,
        })
        .to_string();
        if let Err(error) = clock.computing(|| engine.call("__settle", &text)) {
            return ended_by(error, false, clock);
        }
    }
}

/// What the prelude said, and the answer `__host` gives it: empty, or for
/// a call the host refuses, why.
fn hear(
    text: &str,
    state: &RefCell<State>,
    clock: &Clock,
    calls: &RefCell<impl Calls>,
    limits: Limits,
) -> String {
    let too_long = text.len() > MAX_CALL_BYTES;
    let message = match serde_json::from_str::<FromPrelude>(text) {
        Ok(message) => message,
        Err(error) => {
            state.borrow_mut().garbled =
                Some(format!("The prelude said something unreadable ({error})."));
            return String::new();
        }
    };
    let mut state = state.borrow_mut();
    match message {
        FromPrelude::Call { id, tool, args } => {
            if state.calls >= limits.calls {
                clock.stopped.set(Some(Stopped::CallLimit));
                return format!(
                    "The code made {} tool calls, the most a run may make.",
                    limits.calls
                );
            }
            if too_long {
                return format!(
                    "The arguments of tools.{tool} are {} MB, over the {} MB a call may send.",
                    (text.len() + 512 * 1024) / (1024 * 1024),
                    MAX_CALL_BYTES / (1024 * 1024)
                );
            }
            state.calls += 1;
            // Arko may take a while to read it, which isn't computing.
            if !clock.waiting(|| calls.borrow_mut().send(Call { id, tool, args })) {
                return "Arko stopped taking calls.".into();
            }
            state.outstanding += 1;
        }
        FromPrelude::Log { line } => {
            if state.log_bytes + line.len() <= MAX_LOG_BYTES {
                state.log_bytes += line.len();
                state.log.push(line);
            } else {
                state.log_left_out += 1;
            }
        }
        FromPrelude::Done { value, unread } => {
            let value = value.map(|value| {
                let (kept, left_out) = cut(&value, MAX_RESULT_CHARS);
                state.value_left_out = left_out;
                kept
            });
            let unread = unread
                .iter()
                .take(MAX_UNREAD)
                .map(|line| cut(line, MAX_MESSAGE_CHARS).0)
                .collect();
            state.done = Some(Ended::Returned { value, unread });
        }
        FromPrelude::Failed { refused, tool } => state.failed = Some((refused, tool)),
    }
    String::new()
}

/// How the run ended at `error`, from the host's own flags first.
/// `compiling` says the error came as the code was read.
fn ended_by(error: EngineError, compiling: bool, clock: &Clock) -> Ended {
    if let Some(stopped) = clock.stopped.get() {
        return limit_ended(stopped);
    }
    match error {
        EngineError::Thrown {
            message: text,
            frames,
        } => Ended::Threw {
            line: line_in(&frames, CODE_FILE),
            message: message(text),
            compiling,
            refused: false,
            tool: None,
        },
        EngineError::OutOfMemory => Ended::OutOfMemory { line: None },
        EngineError::Interrupted => Ended::Failed {
            message: "The engine stopped the code without a reason.".into(),
        },
        EngineError::Failed(message) => Ended::Failed { message },
    }
}

/// `text`'s first `most` characters, and how many it left out.
fn cut(text: &str, most: usize) -> (String, u64) {
    match text.char_indices().nth(most) {
        Some((end, _)) => (text[..end].to_string(), text[end..].chars().count() as u64),
        None => (text.to_string(), 0),
    }
}

/// A thrown error's message, cut to what the runner keeps.
fn message(text: String) -> String {
    match cut(&text, MAX_MESSAGE_CHARS) {
        (kept, 0) => kept,
        (kept, left_out) => format!("{kept} [{left_out} more characters left out]"),
    }
}

fn limit_ended(stopped: Stopped) -> Ended {
    match stopped {
        Stopped::ComputeLimit => Ended::ComputeLimit,
        Stopped::CallLimit => Ended::CallLimit,
        Stopped::Asked => Ended::Failed {
            message: "The run was stopped.".into(),
        },
    }
}

fn failed(error: EngineError) -> Ended {
    Ended::Failed {
        message: format!("The engine didn't start: {error:?}"),
    }
}
