//! The code runner: this program started by Arko with [`RUNNER_ARGUMENT`],
//! for one run of code. It writes [`MAGIC`], reads its [`Start`], sets its
//! limits with the operating system, and runs the code on a thread of its
//! own, sending the code's calls out and reading their answers in. It ends
//! with a [`Done`], and at once when its input closes, since Arko is gone
//! or has stopped the run.

use std::io::Write;
use std::sync::mpsc;

use super::host::{self, Calls};
use super::protocol::{self, Answer, Call, Done, Ended, FromChild, MAGIC, Start, ToChild};
use super::quickjs::QuickJs;
use super::{RUNNER_ARGUMENT, THREAD_STACK_BYTES, os_limits};

/// This program runs as the code runner when started as one, so a run may
/// start it.
static ANSWERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this program starts as the code runner when asked. A program
/// whose `main` doesn't call [`run_if_asked`] would start as itself.
pub(super) fn answers() -> bool {
    cfg!(test) || ANSWERS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Runs this process as the code runner and exits, when its command line
/// is the runner's. A program built on this library calls it first thing
/// in `main` if its sessions run code (`--tools both`), since the runner
/// is the same program. Until it is called, `run_code` fails instead of
/// starting the program.
pub fn run_if_asked() {
    ANSWERS.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == RUNNER_ARGUMENT) && args.next().is_none() {
        run();
    }
}

/// Runs one run of code from standard input and exits.
pub(super) fn run() -> ! {
    let mut input = std::io::stdin();
    let mut out = std::io::stdout();
    if out.write_all(MAGIC).and_then(|()| out.flush()).is_err() {
        std::process::exit(1);
    }
    let start = match protocol::read_frame::<ToChild>(&mut input) {
        Ok(Some(ToChild::Start(start))) => start,
        _ => std::process::exit(1),
    };
    if let Err(message) = os_limits::apply(&start.limits) {
        finish(failed(format!(
            "The code runner couldn't set its limits: {message}."
        )));
    }
    let (answer, answers) = mpsc::channel();
    // Reads the answers, and ends the runner when its input ends.
    let reader = std::thread::Builder::new()
        .name("arko-code-input".into())
        .spawn(move || {
            loop {
                match protocol::read_frame::<ToChild>(&mut input) {
                    Ok(Some(ToChild::Answer(reply))) => {
                        if answer.send(reply).is_err() {
                            break;
                        }
                    }
                    Ok(Some(ToChild::Start(_))) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            std::process::exit(0);
        });
    if reader.is_err() {
        finish(failed("The code runner couldn't start its reader.".into()));
    }
    let engine = std::thread::Builder::new()
        .name("arko-code".into())
        .stack_size(THREAD_STACK_BYTES)
        .spawn(move || run_code(&start, answers));
    let done = match engine.map(|engine| engine.join()) {
        Ok(Ok(done)) => done,
        _ => failed("The code runner failed.".into()),
    };
    finish(done)
}

fn run_code(start: &Start, answers: mpsc::Receiver<Answer>) -> Done {
    let calls = Frames {
        out: std::io::stdout(),
        answers,
    };
    host::run::<QuickJs>(start, calls, Default::default())
}

/// The code's calls as frames on standard output, and their answers from
/// the reader.
struct Frames {
    out: std::io::Stdout,
    answers: mpsc::Receiver<Answer>,
}

impl Calls for Frames {
    fn send(&mut self, call: Call) -> bool {
        protocol::write_frame(&mut self.out.lock(), &FromChild::Call(call)).is_ok()
    }

    fn answer(&mut self) -> Option<Answer> {
        self.answers.recv().ok()
    }
}

fn failed(message: String) -> Done {
    Done {
        ended: Ended::Failed { message },
        log: Vec::new(),
        log_left_out: 0,
        value_left_out: 0,
        calls: 0,
        heap_bytes: 0,
        compute_ms: 0,
    }
}

/// Sends `done` and exits.
fn finish(done: Done) -> ! {
    let sent = protocol::write_frame(&mut std::io::stdout().lock(), &FromChild::Done(done));
    std::process::exit(if sent.is_ok() { 0 } else { 1 })
}
