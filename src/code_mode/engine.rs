//! The interface between the host loop and a JavaScript engine. One file
//! implements it ([`super::sandbolt`], the only one that names the
//! engine), and another engine replaces that file and passes the engine
//! tests. Strings are the only values that cross it.

/// What an interpreter is started with, set before any code runs.
#[derive(Debug, Clone, Copy)]
pub struct EngineLimits {
    /// The most the engine's heap may hold.
    pub heap_bytes: usize,
    /// The most stack the engine's calls may use, on a thread with more.
    pub stack_bytes: usize,
}

/// A frame of a thrown error's stack, innermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The file the frame's code came from, as [`Interpreter::eval`] named it.
    pub file: String,
    pub line: u32,
}

/// Why a script, a call into it or its jobs ended early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// It threw. `message` is the error as the engine writes it (its name
    /// and message, or the thrown value), and `frames` where it was thrown.
    Thrown { message: String, frames: Vec<Frame> },
    /// The heap reached its cap.
    OutOfMemory,
    /// The `stop` the interpreter was started with said stop. The engine
    /// unwound past every `catch`. Why it stopped is the host's to know.
    Interrupted,
    /// The engine couldn't do what it was asked, such as start.
    Failed(String),
}

/// The line of the innermost of `frames` in `file`, if one is.
pub fn line_in(frames: &[Frame], file: &str) -> Option<u32> {
    frames
        .iter()
        .find(|frame| frame.file == file)
        .map(|frame| frame.line)
}

/// A fresh JavaScript interpreter on the calling thread, which never leaves
/// it. Each function runs to its end or to an error, and none panics.
pub trait Interpreter: Sized {
    /// An interpreter with `limits`, that polls `stop` as code runs and
    /// unwinds past every `catch` once it says true. Its global object has
    /// one native function, `__host`, which hands its one argument, a
    /// string, to `to_host` and returns `to_host`'s answer, a string. The
    /// prelude takes it and removes it from the global object.
    fn start(
        limits: EngineLimits,
        stop: Box<dyn FnMut() -> bool>,
        to_host: Box<dyn FnMut(&str) -> String>,
    ) -> Result<Self, EngineError>;

    /// Runs `source` as a script in strict mode, its frames named `file`.
    fn eval(&mut self, file: &str, source: &str) -> Result<(), EngineError>;

    /// Calls the global function `function` with one string, `text`.
    fn call(&mut self, function: &str, text: &str) -> Result<(), EngineError>;

    /// Runs promise jobs until none is queued.
    fn run_jobs(&mut self) -> Result<(), EngineError>;

    /// The bytes the engine's heap holds.
    fn heap_bytes(&self) -> u64;
}
