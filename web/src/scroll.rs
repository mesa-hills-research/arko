//! The transcript's scrolling. New output is followed while the reader is at
//! the bottom, and "Latest" jumps there at once. On systems without momentum
//! of their own (Windows, Linux, Android) the mouse wheel scrolls on a
//! spring. Apple's systems keep their native scrolling, and with reduced
//! motion the wheel is native everywhere.

use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{Element, ScrollBehavior, ScrollToOptions, WheelEvent};

/// The wheel's spring, critically damped so it never overshoots, follows
/// the wheel within about 0.2 s.
const WHEEL_STIFFNESS: f64 = 520.0;
/// Within this many pixels of the bottom, new output is followed.
const NEAR_BOTTOM: f64 = 48.0;

#[derive(Default)]
struct State {
    log: Option<Element>,
    pinned: Option<RwSignal<bool>>,
    position: f64,
    velocity: f64,
    target: f64,
    running: bool,
    stiffness: f64,
    last_frame: Option<f64>,
    /// Where the spring last put the scroll position.
    written: f64,
    frame: Option<Closure<dyn FnMut(f64)>>,
    follow_queued: bool,
    follow_frame: Option<Closure<dyn FnMut(f64)>>,
    wheel: Option<Closure<dyn FnMut(WheelEvent)>>,
    resized: Option<Closure<dyn FnMut()>>,
    opened: Option<Closure<dyn FnMut(web_sys::Event)>>,
    /// The scroll position at the last scroll event.
    last_top: i32,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Starts managing `log`, the transcript's scroller, and keeps `pinned`
/// true while it is at the bottom.
pub fn attach(log: Element, pinned: RwSignal<bool>) {
    let frame = Closure::<dyn FnMut(f64)>::new(tick);
    let follow_frame = Closure::<dyn FnMut(f64)>::new(|_| follow_now());
    let wheel = Closure::<dyn FnMut(WheelEvent)>::new(on_wheel);
    // The scroller or its rows changed size (another view, rows added or
    // gone, the message box grew, the keyboard opened), so where the reader
    // is against the end is decided again.
    let resized = Closure::<dyn FnMut()>::new(settle);
    if let Ok(observer) = web_sys::ResizeObserver::new(resized.as_ref().unchecked_ref()) {
        observer.observe(&log);
        if let Ok(Some(rows)) = log.query_selector("#rows") {
            observer.observe(&rows);
        }
    }
    let opened = Closure::<dyn FnMut(web_sys::Event)>::new(on_open);
    let _ = log.add_event_listener_with_callback_and_bool(
        "click",
        opened.as_ref().unchecked_ref(),
        true,
    );
    let options = web_sys::AddEventListenerOptions::new();
    options.set_passive(false);
    let _ = log.add_event_listener_with_callback_and_add_event_listener_options(
        "wheel",
        wheel.as_ref().unchecked_ref(),
        &options,
    );
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.log = Some(log);
        state.pinned = Some(pinned);
        state.frame = Some(frame);
        state.follow_frame = Some(follow_frame);
        state.wheel = Some(wheel);
        state.resized = Some(resized);
        state.opened = Some(opened);
    });
}

fn media(query: &str) -> bool {
    window()
        .match_media(query)
        .ok()
        .flatten()
        .is_some_and(|list| list.matches())
}

pub fn reduced_motion() -> bool {
    media("(prefers-reduced-motion: reduce)")
}

/// Apple's systems, whose scrolling has momentum of its own (iPadOS says
/// it is a Mac).
pub fn apple() -> bool {
    let agent = window().navigator().user_agent().unwrap_or_default();
    ["Macintosh", "iPhone", "iPad", "iPod"]
        .iter()
        .any(|name| agent.contains(name))
}

fn bottom(log: &Element) -> f64 {
    (log.scroll_height() - log.client_height()).max(0) as f64
}

fn scroll_to(log: &Element, top: f64) {
    let options = ScrollToOptions::new();
    options.set_top(top);
    options.set_behavior(ScrollBehavior::Instant);
    log.scroll_to_with_scroll_to_options(&options);
}

/// The scroller moved (by the reader, the spring or new output). Reaching
/// the bottom pins it there, and only scrolling up unpins it: a scroller
/// that gets shorter (the message box grew) stays pinned.
pub fn on_scroll() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(log) = state.log.clone() else { return };
        let top = log.scroll_top();
        let near = bottom(&log) - top as f64 <= NEAR_BOTTOM;
        let up = top < state.last_top - 1;
        state.last_top = top;
        if let Some(pinned) = state.pinned {
            if near && !pinned.get_untracked() {
                pinned.set(true);
            } else if !near && up && pinned.get_untracked() {
                pinned.set(false);
            }
        }
    });
}

/// What a change of size does to the reader's place.
#[derive(Debug, PartialEq)]
enum Settle {
    /// Back to the bottom, as the reader was there.
    Follow,
    /// The bottom came to the reader, so new output is followed again.
    Pin,
    Stay,
}

/// A reader at the bottom stays there, one the bottom came to is there
/// again, and anyone else keeps their place (and Latest). The wheel's spring
/// decides once it stops.
fn settle_action(pinned: bool, running: bool, near: bool) -> Settle {
    match (running, pinned, near) {
        (true, _, _) => Settle::Stay,
        (false, true, _) => Settle::Follow,
        (false, false, true) => Settle::Pin,
        (false, false, false) => Settle::Stay,
    }
}

fn settle() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let (Some(log), Some(pinned)) = (state.log.clone(), state.pinned) else {
            return;
        };
        let near = bottom(&log) - log.scroll_top() as f64 <= NEAR_BOTTOM;
        match settle_action(pinned.get_untracked(), state.running, near) {
            Settle::Follow => scroll_to(&log, bottom(&log)),
            Settle::Pin => pinned.set(true),
            Settle::Stay => {}
        }
        state.last_top = log.scroll_top();
    });
}

/// A row the reader opens grows downward instead of pushing itself up off
/// the screen, as in Studio, so following stops until the reader is back at
/// the bottom.
fn on_open(event: web_sys::Event) {
    let summary = event
        .target()
        .and_then(|target| target.dyn_into::<Element>().ok())
        .and_then(|target| target.closest("summary").ok().flatten());
    if summary.is_none() {
        return;
    }
    STATE.with(|state| {
        if let Some(pinned) = state.borrow().pinned
            && pinned.get_untracked()
        {
            pinned.set(false);
        }
    });
}

/// Follows new output at the next frame, once it's drawn, if the reader is
/// at the bottom.
pub fn follow() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.follow_queued {
            return;
        }
        state.follow_queued = true;
        if let Some(frame) = &state.follow_frame {
            let _ = window().request_animation_frame(frame.as_ref().unchecked_ref());
        }
    });
}

fn follow_now() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.follow_queued = false;
        let pinned = state.pinned.is_some_and(|pinned| pinned.get_untracked());
        if pinned
            && !state.running
            && let Some(log) = &state.log
        {
            scroll_to(log, bottom(log));
        }
    });
}

/// Runs `change` (which may move the scroller's layout, as sizing the
/// message box does) and keeps a pinned scroller at the bottom.
pub fn keeping_bottom(change: impl FnOnce()) {
    let pinned = STATE.with(|state| {
        let state = state.borrow();
        state.pinned.is_some_and(|pinned| pinned.get_untracked()) && !state.running
    });
    change();
    if pinned {
        STATE.with(|state| {
            if let Some(log) = &state.borrow().log {
                scroll_to(log, bottom(log));
            }
        });
    }
}

/// Puts the reader at the bottom at once, as after the transcript loads.
pub fn to_bottom_now() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.running = false;
        if let Some(log) = &state.log {
            scroll_to(log, bottom(log));
        }
        if let Some(pinned) = state.pinned {
            pinned.set(true);
        }
    });
}

/// "Latest": to the bottom at once.
pub fn jump_to_latest() {
    to_bottom_now();
}

fn on_wheel(event: WheelEvent) {
    if event.ctrl_key() || apple() || reduced_motion() {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(log) = state.log.clone() else { return };
        let unit = match event.delta_mode() {
            WheelEvent::DOM_DELTA_LINE => 40.0,
            WheelEvent::DOM_DELTA_PAGE => log.client_height() as f64 * 0.9,
            _ => 1.0,
        };
        let delta = event.delta_y() * unit;
        // Sideways scrolling (a wide code block) stays native.
        if delta == 0.0 || event.delta_x().abs() > event.delta_y().abs() {
            return;
        }
        event.prevent_default();
        if !state.running {
            state.position = log.scroll_top() as f64;
            state.target = state.position;
            state.velocity = 0.0;
        }
        state.target = (state.target + delta).clamp(0.0, bottom(&log));
        state.stiffness = WHEEL_STIFFNESS;
        start(&mut state);
    });
}

fn start(state: &mut State) {
    if state.running {
        return;
    }
    state.running = true;
    state.last_frame = None;
    if let Some(log) = &state.log {
        state.written = log.scroll_top() as f64;
    }
    if let Some(frame) = &state.frame {
        let _ = window().request_animation_frame(frame.as_ref().unchecked_ref());
    }
}

fn tick(now: f64) {
    let again = STATE.with(|state| {
        let mut state = state.borrow_mut();
        if !state.running {
            return false;
        }
        let Some(log) = state.log.clone() else {
            return false;
        };
        // The reader took over (dragged the scrollbar, touched the screen).
        if (log.scroll_top() as f64 - state.written).abs() > 2.0 {
            state.running = false;
            return false;
        }
        let max = bottom(&log);
        let dt = state.last_frame.map_or(1.0 / 60.0, |last| {
            ((now - last) / 1000.0).clamp(0.001, 0.05)
        });
        state.last_frame = Some(now);
        let damping = 2.0 * state.stiffness.sqrt();
        let force = state.stiffness * (state.target - state.position) - damping * state.velocity;
        state.velocity += force * dt;
        state.position = (state.position + state.velocity * dt).clamp(0.0, max);
        let settled = (state.target - state.position).abs() < 0.5 && state.velocity.abs() < 20.0;
        if settled {
            state.position = state.target;
        }
        scroll_to(&log, state.position);
        state.written = log.scroll_top() as f64;
        if settled {
            state.running = false;
        }
        !settled
    });
    if again {
        STATE.with(|state| {
            if let Some(frame) = &state.borrow().frame {
                let _ = window().request_animation_frame(frame.as_ref().unchecked_ref());
            }
        });
    } else {
        on_scroll();
    }
}

/// The item of the first row whose bottom shows, as the reader's place.
pub fn first_visible() -> Option<u64> {
    let log = STATE.with(|state| state.borrow().log.clone())?;
    let top = log.get_bounding_client_rect().top();
    let rows = log.query_selector("#rows").ok().flatten()?.children();
    (0..rows.length())
        .filter_map(|at| rows.item(at))
        .find_map(|row| {
            let id = row.get_attribute("data-id")?.parse().ok()?;
            (row.get_bounding_client_rect().bottom() > top + 1.0).then_some(id)
        })
}

/// Keeps the reader's place at the item `id` (or the nearest row before it)
/// across the layout that's about to change: it stays where it shows now,
/// else a third of the way down.
pub fn keep_place(id: u64) {
    let Some(log) = STATE.with(|state| state.borrow().log.clone()) else {
        return;
    };
    let row_of = move |log: &Element, id: u64| -> Option<Element> {
        let rows = log.query_selector("#rows").ok().flatten()?.children();
        let mut best: Option<(u64, Element)> = None;
        for at in 0..rows.length() {
            let Some(row) = rows.item(at) else { continue };
            let Some(row_id) = row
                .get_attribute("data-id")
                .and_then(|id| id.parse::<u64>().ok())
            else {
                continue;
            };
            if row_id == id {
                return Some(row);
            }
            if row_id < id && best.as_ref().is_none_or(|(best, _)| row_id > *best) {
                best = Some((row_id, row));
            }
        }
        best.map(|(_, row)| row)
    };
    let top = log.get_bounding_client_rect().top();
    let offset = row_of(&log, id)
        .map(|row| row.get_bounding_client_rect().top() - top)
        .filter(|offset| *offset >= 0.0 && *offset < log.client_height() as f64)
        .unwrap_or(log.client_height() as f64 / 3.0);
    STATE.with(|state| {
        let state = state.borrow();
        if let Some(pinned) = state.pinned {
            pinned.set(false);
        }
    });
    let restore = Closure::once_into_js(move || {
        if let Some(row) = row_of(&log, id) {
            let now = row.get_bounding_client_rect().top() - log.get_bounding_client_rect().top();
            scroll_to(&log, log.scroll_top() as f64 + now - offset);
        }
        on_scroll();
    });
    let _ = window().request_animation_frame(restore.unchecked_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_size_keeps_the_reader_at_the_end_or_shows_latest() {
        // Another view made the rows taller under a reader at the end.
        assert_eq!(settle_action(true, false, false), Settle::Follow);
        assert_eq!(settle_action(true, false, true), Settle::Follow);
        // Rows closed or a view got shorter, and the end came to the reader.
        assert_eq!(settle_action(false, false, true), Settle::Pin);
        // A reader away from the end keeps their place, and Latest shows.
        assert_eq!(settle_action(false, false, false), Settle::Stay);
        // The wheel's spring is moving.
        assert_eq!(settle_action(true, true, false), Settle::Stay);
    }
}
