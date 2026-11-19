//! Work for the next frame, held by the reactive owner that asks for it.
//!
//! A row asks for a frame to measure itself once the browser has laid it
//! out. The row may go before that frame (another view, a row drawn again
//! under another key), and the frame would then read what went with it.
//! Here the owner holds the frame, and letting go of it cancels the frame.

use leptos::prelude::*;

/// Runs `then` at the next frame, unless the current owner lets go first,
/// which cancels it. An effect's owner lets go before the effect runs again
/// and when the row it belongs to goes, so each run waits for one frame at
/// most and `then` never runs after the row.
pub fn next_frame(then: impl FnOnce() + 'static) {
    debug_assert!(
        Owner::current().is_some(),
        "a frame needs an owner to hold it"
    );
    StoredValue::new_local(Frame::request(then));
}

#[cfg(not(test))]
use browser::Frame;
#[cfg(test)]
use queue::Frame;

#[cfg(not(test))]
mod browser {
    use leptos::prelude::window;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;

    /// A frame the browser was asked for, with what it calls. Dropped, it
    /// cancels the frame and frees the callback.
    pub struct Frame {
        id: i32,
        _callback: Closure<dyn FnMut()>,
    }

    impl Frame {
        pub fn request(then: impl FnOnce() + 'static) -> Self {
            let mut then = Some(then);
            let callback = Closure::<dyn FnMut()>::new(move || {
                if let Some(then) = then.take() {
                    then();
                }
            });
            let id = window()
                .request_animation_frame(callback.as_ref().unchecked_ref())
                .unwrap_or(0);
            Frame {
                id,
                _callback: callback,
            }
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            let _ = window().cancel_animation_frame(self.id);
        }
    }
}

/// The browser's frames for tests, which wait until [`run`].
#[cfg(test)]
mod queue {
    use std::cell::{Cell, RefCell};

    thread_local! {
        pub static WAITING: RefCell<Vec<(u64, Box<dyn FnOnce()>)>> = RefCell::new(Vec::new());
        pub static ASKED: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Frame {
        id: u64,
    }

    impl Frame {
        pub fn request(then: impl FnOnce() + 'static) -> Self {
            let id = ASKED.with(|asked| {
                asked.set(asked.get() + 1);
                asked.get()
            });
            WAITING.with(|waiting| waiting.borrow_mut().push((id, Box::new(then))));
            Frame { id }
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            // (An owner the thread drops as it ends finds the queue gone.)
            let _ = WAITING.try_with(|waiting| {
                waiting.borrow_mut().retain(|(id, _)| *id != self.id);
            });
        }
    }
}

/// How many frames wait.
#[cfg(test)]
pub fn waiting() -> usize {
    queue::WAITING.with(|waiting| waiting.borrow().len())
}

/// Runs the frames that wait, as the browser's next frame does. One that an
/// earlier one cancels doesn't run, and one asked for meanwhile waits for
/// the frame after.
#[cfg(test)]
pub fn run() {
    let last = queue::ASKED.with(std::cell::Cell::get);
    loop {
        let due = queue::WAITING.with(|waiting| {
            let mut waiting = waiting.borrow_mut();
            let at = waiting.iter().position(|(id, _)| *id <= last)?;
            Some(waiting.remove(at).1)
        });
        match due {
            Some(then) => then(),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    /// A frame runs once while its owner stays, and not at all once its owner
    /// has let go, as when a row that asked for it goes before the frame.
    #[test]
    fn a_frame_runs_only_while_its_owner_stays() {
        let ran = Rc::new(Cell::new(0));
        let ask = |owner: &Owner| {
            let ran = ran.clone();
            owner.with(|| next_frame(move || ran.set(ran.get() + 1)));
        };
        let stays = Owner::new();
        ask(&stays);
        run();
        run();
        assert_eq!(ran.get(), 1);
        let goes = Owner::new();
        ask(&goes);
        assert_eq!(waiting(), 1);
        drop(goes);
        assert_eq!(waiting(), 0);
        run();
        assert_eq!(ran.get(), 1);
    }
}
