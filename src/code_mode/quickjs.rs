//! The [`Interpreter`] on QuickJS-ng, through rquickjs. This is the only
//! file that names rquickjs, and only its `std` feature is on, so the
//! engine has no file, network or process access of its own.

use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use rquickjs::context::EvalOptions;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Exception, Function, Runtime, Value};

use super::engine::{EngineError, EngineLimits, Frame, Interpreter};

/// A QuickJS-ng runtime with one context.
pub struct QuickJs {
    runtime: Runtime,
    context: Context,
    /// The `stop` it was started with said stop since the last call in.
    interrupted: Rc<Cell<bool>>,
}

impl Interpreter for QuickJs {
    fn start(
        limits: EngineLimits,
        mut stop: Box<dyn FnMut() -> bool>,
        to_host: Box<dyn FnMut(&str) -> String>,
    ) -> Result<Self, EngineError> {
        let failed = |error: rquickjs::Error| EngineError::Failed(error.to_string());
        let runtime = Runtime::new().map_err(failed)?;
        runtime.set_memory_limit(limits.heap_bytes);
        runtime.set_max_stack_size(limits.stack_bytes);
        let interrupted = Rc::new(Cell::new(false));
        let fired = interrupted.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let stops = std::panic::catch_unwind(AssertUnwindSafe(&mut stop)).unwrap_or(true);
            if stops {
                fired.set(true);
            }
            stops
        })));
        let context = Context::full(&runtime).map_err(failed)?;
        let to_host = RefCell::new(to_host);
        context.with(|ctx| -> Result<(), EngineError> {
            let host = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, text: String| -> rquickjs::Result<String> {
                    // (`to_host` never calls back in, so it is never borrowed twice.)
                    let answer = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        to_host.try_borrow_mut().map(|mut to_host| to_host(&text))
                    }));
                    match answer {
                        Ok(Ok(answer)) => Ok(answer),
                        _ => Err(Exception::throw_internal(&ctx, "The host failed.")),
                    }
                },
            )
            .map_err(failed)?;
            ctx.globals().set("__host", host).map_err(failed)
        })?;
        Ok(Self {
            runtime,
            context,
            interrupted,
        })
    }

    fn eval(&mut self, file: &str, source: &str) -> Result<(), EngineError> {
        self.interrupted.set(false);
        let interrupted = self.interrupted.clone();
        self.context.with(|ctx| {
            let mut options = EvalOptions::default();
            options.global = true;
            options.strict = true;
            options.filename = Some(file.to_string());
            ctx.eval_with_options::<(), _>(source, options)
                .catch(&ctx)
                .map_err(|error| error_of(&ctx, error, &interrupted))
        })
    }

    fn call(&mut self, function: &str, text: &str) -> Result<(), EngineError> {
        self.interrupted.set(false);
        let interrupted = self.interrupted.clone();
        self.context.with(|ctx| {
            let target: Function = ctx
                .globals()
                .get(function)
                .catch(&ctx)
                .map_err(|error| error_of(&ctx, error, &interrupted))?;
            target
                .call::<_, ()>((text,))
                .catch(&ctx)
                .map_err(|error| error_of(&ctx, error, &interrupted))
        })
    }

    fn run_jobs(&mut self) -> Result<(), EngineError> {
        self.interrupted.set(false);
        loop {
            match self.runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                // A job that threw left its error on the context it ran in.
                Err(job) => {
                    let interrupted = self.interrupted.clone();
                    return Err(job.0.with(|ctx| {
                        let caught = CaughtError::from_error(&ctx, rquickjs::Error::Exception);
                        error_of(&ctx, caught, &interrupted)
                    }));
                }
            }
        }
    }

    fn heap_bytes(&self) -> u64 {
        u64::try_from(self.runtime.memory_usage().malloc_size).unwrap_or(0)
    }
}

/// An error as an [`EngineError`]. An interrupt is known by the handler
/// having said stop, and an exhausted heap by the error QuickJS-ng throws
/// for it.
fn error_of<'js>(ctx: &Ctx<'js>, error: CaughtError<'js>, interrupted: &Cell<bool>) -> EngineError {
    if interrupted.get() {
        return EngineError::Interrupted;
    }
    match error {
        CaughtError::Error(rquickjs::Error::Allocation) => EngineError::OutOfMemory,
        CaughtError::Error(error) => EngineError::Failed(error.to_string()),
        CaughtError::Exception(exception) => {
            let name: String = exception
                .get::<_, Option<String>>("name")
                .ok()
                .flatten()
                .unwrap_or_else(|| "Error".into());
            let message = exception.message().unwrap_or_default();
            if name == "InternalError" && message == "out of memory" {
                return EngineError::OutOfMemory;
            }
            EngineError::Thrown {
                message: if message.is_empty() {
                    name
                } else {
                    format!("{name}: {message}")
                },
                frames: frames(&exception.stack().unwrap_or_default()),
            }
        }
        // QuickJS-ng throws null when it has no memory left for an Error
        // (so code that throws null itself reads as out of memory too).
        CaughtError::Value(value) if value.is_null() => EngineError::OutOfMemory,
        CaughtError::Value(value) => EngineError::Thrown {
            message: describe(ctx, value),
            frames: Vec::new(),
        },
    }
}

/// A thrown value that isn't an Error, as text.
fn describe<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return text;
    }
    ctx.json_stringify(value)
        .ok()
        .flatten()
        .and_then(|text| text.to_string().ok())
        .unwrap_or_else(|| "a value that isn't an Error".into())
}

/// The frames of a QuickJS-ng stack, whose lines read
/// `    at name (file:line:column)` or `    at file:line:column`.
fn frames(stack: &str) -> Vec<Frame> {
    stack
        .lines()
        .filter_map(|line| {
            let place = line.trim().strip_prefix("at ")?;
            let place = match place.rfind('(') {
                Some(open) if place.ends_with(')') => &place[open + 1..place.len() - 1],
                _ => place,
            };
            let mut parts = place.rsplitn(3, ':');
            let _column = parts.next()?;
            let line = parts.next()?.parse().ok()?;
            let file = parts.next()?.to_string();
            Some(Frame { file, line })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_gives_each_frame_its_file_and_line() {
        let stack = "    at f (code.js:2:30)\n    at exec (native)\n    at <anonymous> (prelude.js:14:5)\n    at code.js:7:1\n";
        assert_eq!(
            frames(stack),
            [
                Frame {
                    file: "code.js".into(),
                    line: 2
                },
                Frame {
                    file: "prelude.js".into(),
                    line: 14
                },
                Frame {
                    file: "code.js".into(),
                    line: 7
                },
            ]
        );
    }
}
