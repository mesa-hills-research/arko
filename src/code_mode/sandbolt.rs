//! The [`Interpreter`] on Sandbolt, a JavaScript engine in safe Rust. This
//! is the only file that names Sandbolt. Its global object has the
//! language's own names and `__host`, and no module loader, so the engine
//! has no file, network or process access of its own.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use sandbolt::api::{EvalOptions, JsEngine};

use super::engine::{EngineError, EngineLimits, Frame, Interpreter};

/// A Sandbolt engine with its one realm.
pub struct Sandbolt {
    engine: JsEngine,
    /// The engine panicked, so it isn't run again.
    broken: bool,
}

impl Interpreter for Sandbolt {
    fn start(
        limits: EngineLimits,
        stop: Box<dyn FnMut() -> bool>,
        to_host: Box<dyn FnMut(&str) -> String>,
    ) -> Result<Self, EngineError> {
        let limits = sandbolt::EngineLimits {
            heap_bytes: limits.heap_bytes,
            stack_bytes: limits.stack_bytes,
        };
        let started = catch_unwind(AssertUnwindSafe(|| {
            let mut engine = JsEngine::new(limits)?;
            // `Atomics.wait` throws, so the code never blocks its thread.
            engine.set_can_block(false);
            engine.set_interrupt_handler(Some(stop));
            engine.install_host_bridge(to_host)?;
            Ok(engine)
        }));
        match started {
            Ok(Ok(engine)) => Ok(Self {
                engine,
                broken: false,
            }),
            Ok(Err(error)) => Err(error_of(error)),
            Err(panic) => Err(panicked(&*panic)),
        }
    }

    fn eval(&mut self, file: &str, source: &str) -> Result<(), EngineError> {
        let options = EvalOptions {
            filename: file.to_string(),
            strict: true,
            module: false,
            ..EvalOptions::default()
        };
        self.run(|engine| engine.eval(source, options))
    }

    fn call(&mut self, function: &str, text: &str) -> Result<(), EngineError> {
        self.run(|engine| engine.call_global(function, text))
    }

    fn run_jobs(&mut self) -> Result<(), EngineError> {
        self.run(JsEngine::run_jobs)
    }

    fn heap_bytes(&self) -> u64 {
        self.engine.heap_bytes()
    }
}

impl Sandbolt {
    /// Runs `f` on the engine. A panic in the engine, or in the `stop` or
    /// `to_host` it calls, is the engine's failure, and the engine isn't
    /// run again.
    fn run(
        &mut self,
        f: impl FnOnce(&mut JsEngine) -> Result<(), sandbolt::EngineError>,
    ) -> Result<(), EngineError> {
        if self.broken {
            return Err(EngineError::Failed(
                "The engine failed earlier in the run.".into(),
            ));
        }
        let engine = &mut self.engine;
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(ran) => ran.map_err(error_of),
            Err(panic) => {
                self.broken = true;
                Err(panicked(&*panic))
            }
        }
    }
}

/// A panic in the engine as its failure.
fn panicked(panic: &(dyn Any + Send)) -> EngineError {
    let what = if let Some(text) = panic.downcast_ref::<&str>() {
        text
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text
    } else {
        "a panic"
    };
    EngineError::Failed(format!("The engine failed ({what})."))
}

/// Sandbolt's error as the interface's. Sandbolt tells an interrupt and an
/// exhausted heap apart from what code throws, so no message is read.
fn error_of(error: sandbolt::EngineError) -> EngineError {
    match error {
        sandbolt::EngineError::Thrown { message, frames } => EngineError::Thrown {
            message,
            frames: frames
                .into_iter()
                .map(|frame| Frame {
                    file: frame.file,
                    line: frame.line,
                })
                .collect(),
        },
        sandbolt::EngineError::OutOfMemory => EngineError::OutOfMemory,
        sandbolt::EngineError::Interrupted => EngineError::Interrupted,
        sandbolt::EngineError::Failed(message) => EngineError::Failed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::engine::line_in;

    fn limits() -> EngineLimits {
        EngineLimits {
            heap_bytes: 16 * 1024 * 1024,
            stack_bytes: 256 * 1024,
        }
    }

    #[test]
    fn a_panic_in_the_host_fails_the_run_and_the_engine_isnt_run_again() {
        let mut engine = Sandbolt::start(
            limits(),
            Box::new(|| false),
            Box::new(|_: &str| -> String { panic!("the host broke") }),
        )
        .unwrap();
        engine
            .eval("prelude.js", "globalThis.f = () => __host('x');")
            .unwrap();
        assert_eq!(
            engine.call("f", ""),
            Err(EngineError::Failed(
                "The engine failed (the host broke).".into()
            ))
        );
        assert_eq!(
            engine.eval("code.js", "1;"),
            Err(EngineError::Failed(
                "The engine failed earlier in the run.".into()
            ))
        );
    }

    #[test]
    fn eval_is_strict_and_names_its_frames_by_file() {
        let mut engine = Sandbolt::start(
            limits(),
            Box::new(|| false),
            Box::new(|_: &str| String::new()),
        )
        .unwrap();
        match engine.eval("code.js", "\nundeclared = 1;") {
            Err(EngineError::Thrown { message, frames }) => {
                assert!(message.starts_with("ReferenceError"), "{message}");
                assert_eq!(line_in(&frames, "code.js"), Some(2));
            }
            other => panic!("{other:?}"),
        }
    }
}
