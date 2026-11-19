//! The transcript's place. The page keeps the reader's choice, follow or
//! read, and the hold, a press or a finger on the transcript. A frame
//! follows when the choice is follow, nothing holds the view and the end
//! shows. Following never moves a line down. What comes fills the blank
//! below the end first and then moves the lines up, and an end that gets
//! shorter leaves blank below it. A scroll up that moves a line reads, and
//! a scroll down after which the end shows, End, Esc, the marker and
//! sending follow. Nothing else changes the choice.
//!
//! Three movers change the position. Every position this module writes is
//! the page's own, on whole device pixels. A change the browser makes after
//! its layout (a shorter range, a taller view) is put back before the
//! paint. A change in the scroll event is the reader's and acts by its
//! effect. The page applies the wheel and the keys that scroll the
//! transcript itself, with no animation, and keeps its own anchor for every
//! change above the screen, since the browser's scroll anchoring is off.

use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{Element, HtmlElement, KeyboardEvent, WheelEvent};

/// Wheel events this far apart are two gestures (ms). A gesture's carry
/// goes when it ends, and so does a coast.
const GESTURE_GAP_MS: f64 = 100.0;
/// A pointer that moves this far from its press drags (CSS px).
const DRAG_PX: f64 = 4.0;

/// What the reader chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Follow,
    Read,
}

/// What the transcript shows of its place, written only here.
#[derive(Clone, Copy)]
pub struct Shown {
    /// The view follows its end.
    pub follows: RwSignal<bool>,
    /// The marker and the new lines below the bottom edge it counts (0 is
    /// "end"), while it shows.
    pub marker: RwSignal<Option<u32>>,
}

impl Shown {
    pub fn new() -> Self {
        Self {
            follows: RwSignal::new(true),
            marker: RwSignal::new(None),
        }
    }
}

/// The marker's words for `n` new lines.
pub fn marker_words(n: u32) -> String {
    match n {
        0 => "end".into(),
        1 => "1 new line".into(),
        n => format!("{n} new lines"),
    }
}

// ----- the rules, in numbers -----

/// `px` without the noise of float sums, to a thousandth of a pixel.
fn exact(px: f64) -> f64 {
    (px * 1000.0).round() / 1000.0
}

/// Whether the end, its bottom edge at `end`, shows above the view's
/// bottom edge at `bottom` (CSS px, from one origin) at the pixel ratio
/// `ratio`. It shows when no whole device pixel row of it is below the
/// edge.
fn end_shows(end: f64, bottom: f64, ratio: f64) -> bool {
    let (end, bottom) = (exact(end * ratio), exact(bottom * ratio));
    end < bottom.ceil() + 1.0
}

/// Where a position `top` reads, past either end of the range as that end.
/// The range ends at the start and where the end, `end`, meets the bottom
/// edge of a view `height` tall, or at `floor`, the position the blank
/// below the end holds the view at, whichever is lower.
fn within(top: f64, end: f64, height: f64, floor: f64) -> f64 {
    top.clamp(0.0, (end - height).max(floor).max(0.0))
}

/// The choice after a scroll by the reader that moved the lines `moved`
/// device px down (up when less than 0). `down` says it was a scroll down,
/// and `shows` that the end shows after it. A scroll up that moves a line
/// reads, and a scroll down after which the end shows follows, also when
/// nothing moved.
fn chosen(choice: Choice, moved: f64, down: bool, shows: bool) -> Choice {
    if moved >= 1.0 {
        Choice::Read
    } else if down && shows {
        Choice::Follow
    } else {
        choice
    }
}

/// How far the lines move up (down when less than 0) as the view gets `by`
/// px taller (shorter when less than 0), and what is left to give back
/// after it. `blank` is the blank below the end that shows, `owed` what
/// shorter views moved the lines up, and `follows` whether the view
/// follows. A shorter view takes the blank first and moves the lines up by
/// the rest. A taller one gives back what shorter ones took, and the rest
/// of it is blank. A taller view takes from what is owed while reading or
/// held too, and moves nothing then.
fn resized(by: f64, blank: f64, owed: f64, follows: bool) -> (f64, f64) {
    if by < 0.0 {
        if !follows {
            return (0.0, owed);
        }
        let rest = (-by - blank).max(0.0);
        (rest, owed + rest)
    } else {
        let back = owed.min(by);
        if follows {
            (-back, owed - back)
        } else {
            (0.0, owed - back)
        }
    }
}

/// The marker's count of the new content `below` px under the bottom edge,
/// in lines of `line` px rounded up, so any new output counts a line. None
/// while it doesn't show, which is while the view follows or the end shows.
fn marker(follows: bool, shows: bool, below: f64, line: f64, ratio: f64) -> Option<u32> {
    if follows || shows {
        return None;
    }
    // (Less than a whole device pixel of it is no line.)
    if exact(below * ratio) < 1.0 {
        return Some(0);
    }
    Some((exact(below) / line).ceil().max(1.0) as u32)
}

/// A gesture of the wheel, and its carry, the part of its deltas below a
/// device pixel. The carry adds to the next delta the same way and goes
/// when the way turns or the gesture ends. The gesture's last delta came at
/// `last` (ms), going `down` or up.
#[derive(Default)]
struct Gesture {
    carry: f64,
    last: Option<f64>,
    down: bool,
}

impl Gesture {
    /// The whole device pixels a delta of `delta` device px (down is more)
    /// at `now` (ms) moves.
    fn take(&mut self, delta: f64, now: f64) -> f64 {
        let ended = self.last.is_none_or(|last| now - last > GESTURE_GAP_MS);
        if ended || self.carry * delta < 0.0 {
            self.carry = 0.0;
        }
        self.last = Some(now);
        self.down = delta > 0.0;
        let total = self.carry + delta;
        let whole = exact(total).trunc();
        self.carry = total - whole;
        whole
    }
}

/// The coast of a gesture still running when an input placed the view at
/// `last` (ms), going `down` or up as the gesture went.
#[derive(Clone, Copy, Debug)]
struct Coast {
    last: f64,
    down: bool,
}

impl Coast {
    /// The coast of `gesture` after an input that placed the view at `now`,
    /// where its last delta came within 100 ms.
    fn after(gesture: &Gesture, now: f64) -> Option<Self> {
        let running = gesture
            .last
            .is_some_and(|last| now - last <= GESTURE_GAP_MS);
        running.then_some(Self {
            last: now,
            down: gesture.down,
        })
    }

    /// Whether the wheel event going `down` at `now` is the coast's, which
    /// moves nothing. A gap of more than 100 ms ends the coast, and so does
    /// an event the other way.
    fn takes(&mut self, down: bool, now: f64) -> bool {
        if now - self.last > GESTURE_GAP_MS || down != self.down {
            return false;
        }
        self.last = now;
        true
    }
}

// ----- the page's place -----

/// A row shown, by its own key among the rows shown (its `data-row`, the
/// layout's [`Row::key`](crate::layout::Row::key)) and its item (its
/// `data-id`). Two rows can share an item, such as a turn's count of calls
/// in Messages and the Quiet row of its first call under it, and a place
/// keeps to its own row. In another view, where the row's key is another
/// one, it keeps to its item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub id: u64,
    pub key: String,
}

impl Anchor {
    /// The anchor of the row `row`, from its attributes.
    fn of(row: &Element) -> Option<Self> {
        Some(Self {
            id: row.get_attribute("data-id")?.parse().ok()?,
            key: row.get_attribute("data-row").unwrap_or_default(),
        })
    }
}

/// Which of the rows, by their anchors in order, a place at `anchor` goes
/// back to, and whether at the place's offset. That's its own row, else
/// (in another view) the first row of its item, else the nearest before
/// it, else (the history let go of all those) the earliest left, from its
/// top.
fn anchor_row(rows: &[Anchor], anchor: &Anchor) -> Option<(usize, bool)> {
    if let Some(at) = rows.iter().position(|row| row.key == anchor.key) {
        return Some((at, true));
    }
    let nearest = rows
        .iter()
        .map(|row| row.id)
        .filter(|id| *id <= anchor.id)
        .max()
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .map(|at| (at, true));
    nearest.or_else(|| (!rows.is_empty()).then_some((0, false)))
}

/// The offset a row goes back to, now `height` tall. It is `offset`, unless the
/// row got too short to show from there (a disclosure in it closed while
/// it was away), when its top goes to the top.
fn bounded_offset(offset: f64, height: f64) -> f64 {
    if offset + height > 0.0 { offset } else { 0.0 }
}

/// The line that keeps its place, a row and how far its top is below the
/// view's top (above it when less than 0), with its height then.
#[derive(Clone)]
struct Top {
    row: Element,
    anchor: Anchor,
    /// The key of the row after it, which takes its place if it goes.
    next: Option<String>,
    offset: f64,
    height: f64,
}

/// A line that keeps its place as what the reader opened or closed lays
/// out (a click, [`door_toggles`]), by its element and its row, how far
/// its top is below its row's and below the view's top. A content's door
/// after it, by its place among its row's doors and how far it is below the
/// view's top, keeps its own place instead where the line's would put it
/// above the view, as when a long content closes from its end.
struct Clicked {
    line: Element,
    row: Option<String>,
    inner: f64,
    offset: f64,
    door: Option<(u32, f64)>,
}

/// What the marker counts from, the last frame drawn following. The row
/// that was last then (by its anchor), its height then, and how far below
/// its bottom the end was.
#[derive(Clone)]
struct Baseline {
    anchor: Anchor,
    height: f64,
    tail: f64,
}

/// The press or the finger that holds the view.
#[derive(Clone, Copy)]
struct Hold {
    x: f64,
    y: f64,
    /// The frame followed at the press.
    followed: bool,
    dragged: bool,
    touch: bool,
    /// A selection made by touch, which holds the view until it goes.
    selection: bool,
}

/// The main transcript as the reader left it for a worker's view.
#[derive(Clone)]
struct Left {
    choice: Choice,
    place: Option<(Anchor, f64)>,
    baseline: Option<Baseline>,
    owed: f64,
}

/// A place to put the view at the next frame, once the rows it names are
/// drawn.
enum Pending {
    /// The end on the bottom edge, with no blank below it.
    End,
    /// The row of `anchor` at `offset` below the view's top, or the next
    /// row the view shows. Where the end would then stand above the bottom
    /// edge with lines hidden above, the end on the bottom edge.
    At(Anchor, f64),
    /// The main transcript as the reader left it.
    Back(Left),
}

/// Where the last frame drew the view.
#[derive(Clone)]
struct Painted {
    /// The scroll position (CSS px, a fraction).
    top: f64,
    /// The view's height.
    height: f64,
    width: f64,
    top_line: Option<Top>,
    shows: bool,
    follows: bool,
}

impl Default for Painted {
    fn default() -> Self {
        Self {
            top: 0.0,
            height: 0.0,
            width: 0.0,
            top_line: None,
            shows: true,
            follows: true,
        }
    }
}

struct State {
    log: Option<HtmlElement>,
    rows: Option<Element>,
    floor: Option<HtmlElement>,
    shown: Option<Shown>,
    choice: Choice,
    hold: Option<Hold>,
    painted: Painted,
    /// What shorter views moved the lines up, to give back (device px).
    owed: f64,
    /// What shorter views took from the view, less what taller ones gave
    /// back, and the lines of it they hid at the bottom while the reader
    /// read, as opposed to blank below the end (CSS px, so another width or
    /// pixel ratio leaves them as they are). Going to the end owes one of
    /// them, as `chose` and `end_now` say. Another view, a worker's or the
    /// main one again, starts both at none.
    shrunk: f64,
    hidden: f64,
    gesture: Gesture,
    coast: Option<Coast>,
    clicked: Option<Clicked>,
    baseline: Option<Baseline>,
    /// The block the reader left for another view, until they move. It
    /// comes before following, so a follower whose fold took the end off the
    /// screen comes back to it through a view where the end shows.
    kept: Option<(Anchor, f64)>,
    pending: Option<Pending>,
    left: Option<Left>,
    /// The body text's line height (CSS px).
    line: f64,
    /// The position and the view's height as an edit of a field began.
    editing: Option<(f64, f64)>,
    /// The part of the position below a CSS pixel, which moves the rows.
    fraction: f64,
    queued: bool,
    closures: Vec<Box<dyn std::any::Any>>,
    frame: Option<Closure<dyn FnMut(f64)>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            log: None,
            rows: None,
            floor: None,
            shown: None,
            choice: Choice::Follow,
            hold: None,
            painted: Painted::default(),
            owed: 0.0,
            shrunk: 0.0,
            hidden: 0.0,
            gesture: Gesture::default(),
            coast: None,
            clicked: None,
            baseline: None,
            kept: None,
            pending: None,
            left: None,
            line: 24.0,
            editing: None,
            fraction: 0.0,
            queued: false,
            closures: Vec::new(),
            frame: None,
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The view's geometry now, laid out. Positions are in the content's
/// coordinates (CSS px from the rows' top), read from rectangles, as the
/// position the browser gives is a whole number.
struct Geometry {
    rows: Element,
    /// The view's top edge in the content, the scroll position.
    top: f64,
    height: f64,
    width: f64,
    /// The end's bottom edge, the rows' padding below it included.
    end: f64,
    ratio: f64,
}

impl Geometry {
    fn read(log: &HtmlElement, rows: &Element) -> Self {
        let view = log.get_bounding_client_rect();
        let content = rows.get_bounding_client_rect();
        Self {
            rows: rows.clone(),
            top: view.top() - content.top(),
            height: view.height(),
            width: view.width(),
            end: content.height(),
            ratio: window().device_pixel_ratio(),
        }
    }

    fn shows(&self, top: f64) -> bool {
        end_shows(self.end, top + self.height, self.ratio)
    }

    /// Where `element`'s top is, in the content.
    fn at(&self, element: &Element) -> f64 {
        element.get_bounding_client_rect().top() - self.rows.get_bounding_client_rect().top()
    }

    /// Where `element`'s bottom is, in the content.
    fn bottom_of(&self, element: &Element) -> f64 {
        element.get_bounding_client_rect().bottom() - self.rows.get_bounding_client_rect().top()
    }
}

/// The rows of the transcript, as elements.
fn children(rows: &Element) -> Vec<Element> {
    let list = rows.children();
    (0..list.length()).filter_map(|at| list.item(at)).collect()
}

/// The first row whose bottom is below `top` by a device pixel or more,
/// found by halves as the rows are in order. Rows not drawn (of no height)
/// are passed over.
fn top_line(rows: &Element, geometry: &Geometry, top: f64) -> Option<Top> {
    let list = rows.children();
    let count = list.length();
    let origin = rows.get_bounding_client_rect().top();
    // The first drawn row from `at` on, and its rectangle.
    let drawn = |mut at: u32| {
        while at < count {
            let row = list.item(at)?;
            let rect = row.get_bounding_client_rect();
            if rect.height() > 0.0 {
                return Some((at, row, rect));
            }
            at += 1;
        }
        None
    };
    let edge = top + 1.0 / geometry.ratio;
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = (low + high) / 2;
        match drawn(middle) {
            Some((at, _, rect)) if rect.bottom() - origin <= edge => low = at + 1,
            Some(_) | None => high = middle,
        }
    }
    let (at, row, rect) = drawn(low)?;
    let anchor = Anchor::of(&row).unwrap_or_else(|| Anchor {
        id: 0,
        key: row.get_attribute("data-row").unwrap_or_default(),
    });
    let next = drawn(at + 1).and_then(|(_, next, _)| next.get_attribute("data-row"));
    Some(Top {
        offset: rect.top() - origin - top,
        height: rect.height(),
        row,
        anchor,
        next,
    })
}

/// The row of `key` among the rows, if it's drawn.
fn row_of(rows: &Element, key: &str) -> Option<Element> {
    if key.is_empty() {
        return None;
    }
    children(rows)
        .into_iter()
        .find(|row| row.get_attribute("data-row").as_deref() == Some(key))
}

/// The last row that isn't "Working…" or a note waiting, and the end's
/// place below its bottom.
fn baseline(rows: &Element, geometry: &Geometry) -> Option<Baseline> {
    let all = children(rows);
    let row = all
        .iter()
        .rev()
        .find(|row| !row.matches(".working, .user.pending").unwrap_or(false))?;
    let anchor = Anchor::of(row)?;
    Some(Baseline {
        anchor,
        height: row.get_bounding_client_rect().height(),
        tail: geometry.end - geometry.bottom_of(row),
    })
}

/// Where what came before `baseline` ends, in the content, which the marker
/// counts from.
fn counted_from(rows: &Element, geometry: &Geometry, baseline: &Baseline) -> f64 {
    match row_of(rows, &baseline.anchor.key) {
        Some(row) => geometry.at(&row) + baseline.height + baseline.tail,
        None => {
            let all = children(rows);
            let anchors: Vec<Anchor> = all
                .iter()
                .map(|row| {
                    Anchor::of(row).unwrap_or(Anchor {
                        id: u64::MAX,
                        key: String::new(),
                    })
                })
                .collect();
            match anchor_row(&anchors, &baseline.anchor) {
                Some((at, true)) => geometry.bottom_of(&all[at]) + baseline.tail,
                _ => geometry.end,
            }
        }
    }
}

/// The body text's line height, from the rows' style.
fn line_height(rows: &Element) -> f64 {
    window()
        .get_computed_style(rows)
        .ok()
        .flatten()
        .and_then(|style| style.get_property_value("line-height").ok())
        .and_then(|value| value.trim_end_matches("px").parse::<f64>().ok())
        .filter(|line| *line > 0.0)
        .unwrap_or(24.0)
}

/// Starts managing `log`, the transcript's scroller, with `shown`, and keeps
/// `width` its width.
pub fn attach(log: Element, shown: Shown, width: RwSignal<i32>) {
    let Ok(log) = log.dyn_into::<HtmlElement>() else {
        return;
    };
    let Ok(Some(rows)) = log.query_selector("#rows") else {
        return;
    };
    let floor = document()
        .create_element("div")
        .ok()
        .and_then(|floor| floor.dyn_into::<HtmlElement>().ok());
    if let Some(floor) = &floor {
        floor.set_class_name("floor");
        let _ = floor.set_attribute("aria-hidden", "true");
        let _ = log.append_child(floor);
    }
    let mut closures: Vec<Box<dyn std::any::Any>> = Vec::new();
    // Sizes the browser laid out are settled before they're painted.
    let measured = log.clone();
    let resized = Closure::<dyn FnMut()>::new(move || {
        let now = measured.client_width();
        if width.get_untracked() != now {
            width.set(now);
        }
        settle();
    });
    if let Ok(observer) = web_sys::ResizeObserver::new(resized.as_ref().unchecked_ref()) {
        observer.observe(&log);
        observer.observe(&rows);
    }
    closures.push(Box::new(resized));
    // Anything drawn in the rows is settled at the next frame.
    let drawn = Closure::<dyn FnMut()>::new(changed);
    if let Ok(observer) = web_sys::MutationObserver::new(drawn.as_ref().unchecked_ref()) {
        let options = web_sys::MutationObserverInit::new();
        options.set_child_list(true);
        options.set_subtree(true);
        options.set_character_data(true);
        options.set_attributes(true);
        let _ = observer.observe_with_options(&rows, &options);
    }
    closures.push(Box::new(drawn));
    let listen = |target: &web_sys::EventTarget,
                  name: &str,
                  passive: bool,
                  capture: bool,
                  handler: Box<dyn FnMut(web_sys::Event)>,
                  closures: &mut Vec<Box<dyn std::any::Any>>| {
        let closure = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
        let options = web_sys::AddEventListenerOptions::new();
        options.set_passive(passive);
        options.set_capture(capture);
        let _ = target.add_event_listener_with_callback_and_add_event_listener_options(
            name,
            closure.as_ref().unchecked_ref(),
            &options,
        );
        closures.push(Box::new(closure));
    };
    let window_target: web_sys::EventTarget = window().into();
    listen(
        &log,
        "scroll",
        true,
        false,
        Box::new(|_| on_scroll()),
        &mut closures,
    );
    listen(
        &log,
        "wheel",
        false,
        false,
        Box::new(|event| on_wheel(event.unchecked_into())),
        &mut closures,
    );
    listen(&log, "click", true, true, Box::new(on_click), &mut closures);
    listen(
        &log,
        "pointerdown",
        true,
        false,
        Box::new(on_press),
        &mut closures,
    );
    listen(
        &window_target,
        "pointermove",
        true,
        false,
        Box::new(on_move),
        &mut closures,
    );
    listen(
        &window_target,
        "pointerup",
        true,
        false,
        Box::new(|event| on_release(Some(event))),
        &mut closures,
    );
    listen(
        &window_target,
        "pointercancel",
        true,
        false,
        Box::new(|event| {
            if !is_touch(&event) {
                on_release(None);
            }
        }),
        &mut closures,
    );
    listen(
        &log,
        "touchstart",
        true,
        false,
        Box::new(on_touch),
        &mut closures,
    );
    listen(
        &window_target,
        "touchmove",
        true,
        false,
        Box::new(|_| {
            STATE.with(|state| {
                if let Some(hold) = state.borrow_mut().hold.as_mut()
                    && hold.touch
                {
                    hold.dragged = true;
                }
            });
        }),
        &mut closures,
    );
    listen(
        &window_target,
        "touchend",
        true,
        false,
        Box::new(|event| on_lift(event, false)),
        &mut closures,
    );
    listen(
        &window_target,
        "touchcancel",
        true,
        false,
        Box::new(|event| on_lift(event, true)),
        &mut closures,
    );
    // (The window's own, since an element's blur doesn't bubble.)
    listen(
        &window_target,
        "blur",
        true,
        false,
        Box::new(|event| {
            let own: web_sys::EventTarget = window().into();
            if event.target() == Some(own) {
                on_release(None);
            }
        }),
        &mut closures,
    );
    let document_target: web_sys::EventTarget = document().into();
    // Editing a field lays it out mid-edit (a last line emptied has no
    // height for a moment), and a taller view for that moment shortens the
    // range under the transcript. The view goes back once the edit is done.
    listen(
        &document_target,
        "beforeinput",
        true,
        true,
        Box::new(|_| {
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                state.editing = state
                    .geometry()
                    .map(|geometry| (geometry.top, geometry.height));
            });
        }),
        &mut closures,
    );
    listen(
        &document_target,
        "input",
        true,
        true,
        Box::new(|_| edited()),
        &mut closures,
    );
    listen(
        &document_target,
        "selectionchange",
        true,
        false,
        Box::new(|_| on_selection()),
        &mut closures,
    );
    let frame = Closure::<dyn FnMut(f64)>::new(|_| {
        STATE.with(|state| state.borrow_mut().queued = false);
        settle();
    });
    let line = line_height(&rows);
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.log = Some(log);
        state.rows = Some(rows);
        state.floor = floor;
        state.shown = Some(shown);
        state.line = line;
        state.closures = closures;
        state.frame = Some(frame);
        state.pending = Some(Pending::End);
    });
    changed();
}

/// Apple's systems, which keep their own scrollbars (iPadOS says it is a
/// Mac).
pub fn apple() -> bool {
    let agent = window().navigator().user_agent().unwrap_or_default();
    ["Macintosh", "iPhone", "iPad", "iPod"]
        .iter()
        .any(|name| agent.contains(name))
}

fn is_touch(event: &web_sys::Event) -> bool {
    event
        .dyn_ref::<web_sys::PointerEvent>()
        .is_some_and(|event| event.pointer_type() == "touch")
}

/// What the rows show changed, or may have, so the next frame settles the
/// view before it's painted.
pub fn changed() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.queued {
            return;
        }
        state.queued = true;
        if let Some(frame) = &state.frame {
            let _ = window().request_animation_frame(frame.as_ref().unchecked_ref());
        }
    });
}

/// What scroll.rs writes into the page, done once the state isn't
/// borrowed.
struct Out {
    shown: Option<Shown>,
    follows: bool,
    marker: Option<u32>,
}

impl Out {
    fn apply(self) {
        if let Some(shown) = self.shown {
            if shown.follows.get_untracked() != self.follows {
                shown.follows.set(self.follows);
            }
            if shown.marker.get_untracked() != self.marker {
                shown.marker.set(self.marker);
            }
        }
    }
}

impl State {
    /// Writes the view's position, `top`, on a whole device pixel, with the
    /// floor under the content at the view's bottom edge, and reads the
    /// frame it makes.
    fn write(&mut self, geometry: &Geometry, top: f64) -> Out {
        let (Some(log), Some(rows)) = (self.log.clone(), self.rows.clone()) else {
            return self.out(geometry, true);
        };
        // (A line kept in its place keeps its device pixel row only where the
        // view keeps the fraction of a device pixel it stands at, so the
        // position isn't rounded. The reader's own moves are whole device
        // pixels from where the view stands.)
        let top = self.place(&log, &rows, geometry, top.max(0.0));
        let shows = geometry.shows(top);
        self.painted = Painted {
            top,
            height: geometry.height,
            width: geometry.width,
            top_line: top_line(&rows, geometry, top),
            shows,
            follows: self.choice == Choice::Follow && self.hold.is_none() && shows,
        };
        if self.painted.follows {
            self.baseline = baseline(&rows, geometry);
        }
        let _ = log.set_attribute(
            "data-choice",
            match self.choice {
                Choice::Follow => "follow",
                Choice::Read => "read",
            },
        );
        let _ = log.set_attribute("data-held", &self.hold.is_some().to_string());
        self.out(geometry, shows)
    }

    /// Puts the view's top at `wanted`. The browser
    /// keeps a scroller's position on whole CSS pixels, so the part below
    /// one moves the rows up as a transform. The floor sits under the
    /// content at the view's bottom edge. Returns the position as the
    /// browser laid it out.
    fn place(
        &mut self,
        log: &HtmlElement,
        rows: &Element,
        geometry: &Geometry,
        wanted: f64,
    ) -> f64 {
        let whole = wanted.floor();
        let fraction = exact(wanted - whole);
        if let Some(floor) = &self.floor {
            let _ = floor.style().set_property(
                "top",
                &format!("{}px", (wanted + geometry.height).ceil() - 1.0),
            );
        }
        if fraction != self.fraction {
            self.fraction = fraction;
            if let Some(rows) = rows.dyn_ref::<HtmlElement>() {
                let transform = if fraction > 0.0 {
                    format!("translateY({}px)", -fraction)
                } else {
                    String::new()
                };
                let _ = rows.style().set_property("transform", &transform);
            }
        }
        if f64::from(log.scroll_top()) != whole {
            let options = web_sys::ScrollToOptions::new();
            options.set_top(whole);
            options.set_behavior(web_sys::ScrollBehavior::Instant);
            log.scroll_to_with_scroll_to_options(&options);
        }
        let _ = geometry;
        Geometry::read(log, rows).top
    }

    /// What the page shows of the frame, with the end showing or not.
    fn out(&self, geometry: &Geometry, shows: bool) -> Out {
        let below = match (&self.rows, &self.baseline) {
            (Some(rows), Some(baseline)) => {
                let from =
                    counted_from(rows, geometry, baseline).max(self.painted.top + geometry.height);
                geometry.end - from
            }
            _ => 0.0,
        };
        Out {
            shown: self.shown,
            follows: self.painted.follows,
            marker: marker(
                self.painted.follows,
                shows,
                below,
                self.line,
                geometry.ratio,
            ),
        }
    }

    fn geometry(&self) -> Option<Geometry> {
        Some(Geometry::read(self.log.as_ref()?, self.rows.as_ref()?))
    }

    /// Where the row `top` kept its place puts the view now.
    fn kept_by(&self, geometry: &Geometry, top: &Top) -> Option<f64> {
        let rows = self.rows.as_ref()?;
        let row = if top.row.is_connected() {
            top.row.clone()
        } else if let Some(row) = row_of(rows, &top.anchor.key) {
            row
        } else {
            // (It went, and the row after it takes its place.)
            let next = row_of(rows, top.next.as_deref()?)?;
            return Some(geometry.at(&next) - top.offset.max(0.0));
        };
        // At another width a row wraps again, and the place inside it keeps
        // its share of the row.
        let offset = if exact(geometry.width) != exact(self.painted.width)
            && top.offset < 0.0
            && top.height > 0.0
        {
            top.offset / top.height * row.get_bounding_client_rect().height()
        } else {
            top.offset
        };
        Some(geometry.at(&row) - offset)
    }

    /// Where a place puts the view, the row of `anchor` at `offset`, or the
    /// nearest row the view shows. Another view (`another`) brings the end
    /// to the bottom edge where it would stand above it with lines hidden
    /// above.
    fn placed(
        &self,
        geometry: &Geometry,
        anchor: &Anchor,
        offset: f64,
        another: bool,
    ) -> Option<f64> {
        let rows = self.rows.as_ref()?;
        let all = children(rows);
        let anchors: Vec<Anchor> = all
            .iter()
            .map(|row| {
                Anchor::of(row).unwrap_or(Anchor {
                    id: u64::MAX,
                    key: String::new(),
                })
            })
            .collect();
        let (at, at_offset) = anchor_row(&anchors, anchor)?;
        let offset = if at_offset {
            bounded_offset(offset, all[at].get_bounding_client_rect().height())
        } else {
            0.0
        };
        let mut top = geometry.at(&all[at]) - offset;
        if another && top > 0.0 && geometry.end < top + geometry.height {
            top = (geometry.end - geometry.height).max(0.0);
        }
        Some(top)
    }

    fn settle(&mut self) -> Out {
        let Some(geometry) = self.geometry() else {
            return Out {
                shown: None,
                follows: true,
                marker: None,
            };
        };
        let painted = self.painted.clone();
        let mut top = geometry.top;
        match self.pending.take() {
            Some(Pending::End) => {
                self.choice = Choice::Follow;
                top = (geometry.end - geometry.height).max(0.0);
            }
            Some(Pending::At(anchor, offset)) => {
                if let Some(at) = self.placed(&geometry, &anchor, offset, true) {
                    top = at;
                }
            }
            Some(Pending::Back(left)) => {
                self.choice = left.choice;
                self.owed = left.owed;
                self.baseline = left.baseline;
                match left.place {
                    Some((anchor, offset)) => {
                        if let Some(at) = self.placed(&geometry, &anchor, offset, false) {
                            top = at;
                        }
                    }
                    None => top = (geometry.end - geometry.height).max(0.0),
                }
                if self.choice == Choice::Follow && geometry.shows(top) {
                    top = top.max(geometry.end - geometry.height);
                }
            }
            None => {
                // The line that stays, the line the reader clicked, else the
                // top line.
                let clicked = self.clicked.take();
                let held = match &clicked {
                    Some(clicked) => self.clicked_at(&geometry, clicked),
                    None => painted
                        .top_line
                        .as_ref()
                        .and_then(|line| self.kept_by(&geometry, line)),
                };
                if let Some(at) = held {
                    top = at;
                }
                // The view's height changed, which a follower takes first
                // from the blank, and which gives back what shorter views
                // took.
                let by = geometry.height - painted.height;
                let follows = self.choice == Choice::Follow && self.hold.is_none() && painted.shows;
                if painted.height > 0.0 && exact(by) != 0.0 {
                    let blank = (painted.top + painted.height - geometry.end).max(0.0);
                    let (up, owed) = resized(
                        by * geometry.ratio,
                        blank * geometry.ratio,
                        self.owed,
                        follows,
                    );
                    self.owed = owed;
                    // (A shorter view while reading hides what was below its
                    // new bottom edge, lines or blank.)
                    if by < 0.0 {
                        self.shrunk -= by;
                        if self.choice == Choice::Read {
                            self.hidden += (-by - blank).max(0.0);
                        }
                    } else {
                        self.shrunk = (self.shrunk - by).max(0.0);
                        self.hidden = (self.hidden - by).max(0.0);
                    }
                    if up < 0.0 {
                        top += up / geometry.ratio;
                    }
                }
                // Following, the end that would go below the bottom edge comes
                // to it, and blank that would fill the whole view brings the
                // end to the bottom edge.
                if follows && clicked.is_none() {
                    if geometry.end > top + geometry.height {
                        top = geometry.end - geometry.height;
                    } else if geometry.end <= top {
                        top = (geometry.end - geometry.height).max(0.0);
                    }
                }
            }
        }
        self.write(&geometry, top)
    }

    /// Where the line the reader clicked puts the view, or its door, where
    /// the line would put the door above the view's top.
    fn clicked_at(&self, geometry: &Geometry, clicked: &Clicked) -> Option<f64> {
        let row = clicked
            .row
            .as_deref()
            .and_then(|key| row_of(self.rows.as_ref()?, key));
        let line = if clicked.line.is_connected() {
            geometry.at(&clicked.line)
        } else {
            geometry.at(row.as_ref()?) + clicked.inner
        };
        let top = line - clicked.offset;
        let door = row.zip(clicked.door).and_then(|(row, (index, offset))| {
            let door = row.get_elements_by_class_name("door").item(index)?;
            Some((geometry.at(&door), offset))
        });
        Some(match door {
            Some((at, offset)) if at < top => at - offset,
            _ => top,
        })
    }

    /// The reader's own scroll, `delta` CSS px (down is more), which the
    /// page applies.
    fn scroll_by(&mut self, geometry: &Geometry, delta: f64) -> Out {
        let from = geometry.top;
        let mut top = from;
        let shows = geometry.shows(from);
        if delta > 0.0 {
            if !shows {
                top = (from + delta).min(geometry.end - geometry.height).max(from);
            }
        } else {
            top = (from + delta).max(0.0);
        }
        let moved = (from - top) * geometry.ratio;
        let choice = chosen(self.choice, moved, delta > 0.0, geometry.shows(top));
        self.chose(choice);
        self.write(geometry, top)
    }

    /// Sets the choice, after a move of the reader's, which lets go of the
    /// block they left for another view. When following starts, the rows to
    /// give back are the lines shorter views hid while the reader read, so a
    /// view that grows again brings them back with its bottom edge, and the
    /// blank it hid comes back as blank.
    fn chose(&mut self, choice: Choice) {
        if choice == Choice::Follow && self.choice == Choice::Read {
            self.owed = self.hidden * window().device_pixel_ratio();
        }
        if choice == Choice::Follow {
            self.hidden = 0.0;
        }
        self.choice = choice;
        self.kept = None;
    }

    /// `line`, and the door after it, keep their places at the next frame,
    /// where the line shows. It's an action of the reader's, which lets go
    /// of the block they left for another view.
    fn keep(&mut self, line: Element, door: Option<&Element>) {
        self.kept = None;
        let Some(geometry) = self.geometry() else {
            return;
        };
        let at = geometry.at(&line);
        let shows = line.is_connected()
            && geometry.bottom_of(&line) > geometry.top
            && at < geometry.top + geometry.height;
        if !shows {
            return;
        }
        let row = line.closest("[data-row]").ok().flatten();
        let door = row.as_ref().zip(door).and_then(|(row, door)| {
            let doors = row.get_elements_by_class_name("door");
            let index = (0..doors.length()).find(|at| doors.item(*at).as_ref() == Some(door))?;
            Some((index, geometry.at(door) - geometry.top))
        });
        self.clicked = Some(Clicked {
            inner: row.as_ref().map_or(0.0, |row| at - geometry.at(row)),
            row: row.and_then(|row| row.get_attribute("data-row")),
            offset: at - geometry.top,
            line,
            door,
        });
    }

    /// The view goes to the end, with no blank below it, and follows. All
    /// that shorter views took is owed then, the blank they took and the
    /// lines they hid or moved up alike, whether the reader read or
    /// followed, so a view that grows again (a phone's keyboard going after
    /// a message is sent) keeps the end on its bottom edge instead of
    /// leaving blank below it.
    fn end_now(&mut self, now: f64) -> Out {
        self.coast = Coast::after(&self.gesture, now);
        self.chose(Choice::Follow);
        self.owed = self.owed.max(self.shrunk * window().device_pixel_ratio());
        let Some(geometry) = self.geometry() else {
            self.pending = Some(Pending::End);
            return self.out_without();
        };
        self.write(&geometry, (geometry.end - geometry.height).max(0.0))
    }

    fn out_without(&self) -> Out {
        Out {
            shown: self.shown,
            follows: self.choice == Choice::Follow,
            marker: None,
        }
    }
}

fn settle() {
    STATE.with(|state| state.borrow_mut().settle()).apply();
}

/// An edit of a field is done. A position the browser moved while it laid
/// the field out mid-edit goes back, where the view is as tall as before.
fn edited() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let (Some((top, height)), Some(geometry)) = (state.editing.take(), state.geometry()) else {
            return;
        };
        if exact(geometry.height) == exact(height)
            && exact(geometry.top) != exact(top)
            && let (Some(log), Some(rows)) = (state.log.clone(), state.rows.clone())
        {
            state.place(&log, &rows, &geometry, top);
        }
    });
}

/// The scroller moved. A change the page made is its own, and any other is
/// the reader's, acting by its effect.
fn on_scroll() {
    let out = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let geometry = state.geometry()?;
        // A view of another size moved by the browser, which shortens its
        // range under a taller view, is put back as the page's own.
        if exact(geometry.height) != exact(state.painted.height)
            || exact(geometry.width) != exact(state.painted.width)
        {
            return Some(state.settle());
        }
        // (A position past either end, a bounce, reads as that end.)
        let top = within(
            geometry.top,
            geometry.end,
            geometry.height,
            state.painted.top,
        );
        let moved = (state.painted.top - top) * geometry.ratio;
        if moved.abs() < 0.5 {
            return None;
        }
        // (A scroll up drops the blank that left the view.)
        let down = moved < 0.0;
        let choice = chosen(state.choice, moved, down, geometry.shows(top));
        state.chose(choice);
        Some(state.write(&geometry, geometry.top))
    });
    if let Some(out) = out {
        out.apply();
    }
}

/// A box in the transcript that can scroll the wheel's way, which takes
/// the wheel.
fn scrolls_itself(event: &WheelEvent, log: &Element) -> bool {
    let mut at = event
        .target()
        .and_then(|target| target.dyn_into::<Element>().ok());
    let down = event.delta_y() > 0.0;
    while let Some(element) = at {
        if element == *log {
            return false;
        }
        if element.scroll_height() > element.client_height() + 1 {
            let overflow = window()
                .get_computed_style(&element)
                .ok()
                .flatten()
                .and_then(|style| style.get_property_value("overflow-y").ok())
                .unwrap_or_default();
            if overflow == "auto" || overflow == "scroll" {
                let top = element.scroll_top();
                let room = element.scroll_height() - element.client_height() - top;
                if (down && room > 0) || (!down && top > 0) {
                    return true;
                }
            }
        }
        at = element.parent_element();
    }
    false
}

/// The wheel, which the page applies itself, but for the browser's zoom, a
/// sideways wheel and a box that scrolls the wheel's way.
fn on_wheel(event: WheelEvent) {
    if event.ctrl_key() || event.delta_x().abs() > event.delta_y().abs() || event.delta_y() == 0.0 {
        return;
    }
    let out = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let log = state.log.clone()?;
        if scrolls_itself(&event, &log) {
            return None;
        }
        event.prevent_default();
        let now = event.time_stamp();
        let down = event.delta_y() > 0.0;
        if let Some(coast) = state.coast.as_mut() {
            if coast.takes(down, now) {
                return None;
            }
            state.coast = None;
        }
        let geometry = state.geometry()?;
        let unit = match event.delta_mode() {
            WheelEvent::DOM_DELTA_LINE => state.line,
            WheelEvent::DOM_DELTA_PAGE => geometry.height - 2.0 * state.line,
            _ => 1.0,
        };
        let device = event.delta_y() * unit * geometry.ratio;
        let whole = state.gesture.take(device, now);
        if whole == 0.0 {
            // (A scroll down of less than a device pixel moves nothing, and
            // follows where the end shows, as a whole one does.)
            if !down || !geometry.shows(geometry.top) {
                return None;
            }
            let reading = state.choice == Choice::Read;
            state.chose(Choice::Follow);
            return reading.then(|| state.write(&geometry, geometry.top));
        }
        Some(state.scroll_by(&geometry, whole / geometry.ratio))
    });
    if let Some(out) = out {
        out.apply();
    }
}

/// A click in the transcript. The line it's on keeps its place as what it
/// opens or closes lays out again.
fn on_click(event: web_sys::Event) {
    let Some(target) = event
        .target()
        .and_then(|target| target.dyn_into::<Element>().ok())
    else {
        return;
    };
    let line = target
        .closest("summary, button, [aria-expanded]")
        .ok()
        .flatten()
        .unwrap_or(target);
    STATE.with(|state| state.borrow_mut().keep(line, None));
    changed();
}

/// A content's door opens or closes now, and the content's first lines,
/// `lines`, keep their place at the next frame, also when its whole came a
/// while after the click. Closing from its end keeps the door in its place
/// instead, where none of the first lines would show. Lines off the screen
/// keep nothing, and the top line keeps its place as for any change.
pub fn door_toggles(lines: &Element, door: &Element) {
    STATE.with(|state| state.borrow_mut().keep(lines.clone(), Some(door)));
    changed();
}

/// A press of the mouse's button on the transcript holds the view, but on
/// its scrollbar, which scrolls.
fn on_press(event: web_sys::Event) {
    let Some(event) = event.dyn_ref::<web_sys::PointerEvent>() else {
        return;
    };
    if event.pointer_type() == "touch" || event.button() != 0 {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(log) = state.log.clone() else { return };
        let rect = log.get_bounding_client_rect();
        if f64::from(event.client_x()) - rect.left() >= f64::from(log.client_width()) {
            return;
        }
        let followed = state.painted.follows;
        state.hold = Some(Hold {
            x: f64::from(event.client_x()),
            y: f64::from(event.client_y()),
            followed,
            dragged: false,
            touch: false,
            selection: false,
        });
    });
    changed();
}

fn on_move(event: web_sys::Event) {
    let Some(event) = event.dyn_ref::<web_sys::PointerEvent>() else {
        return;
    };
    STATE.with(|state| {
        if let Some(hold) = state.borrow_mut().hold.as_mut()
            && !hold.touch
        {
            let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
            if (x - hold.x).hypot(y - hold.y) > DRAG_PX {
                hold.dragged = true;
            }
        }
    });
}

/// A finger down on the transcript holds the view.
fn on_touch(_: web_sys::Event) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let followed = state.painted.follows;
        state.hold = Some(Hold {
            x: 0.0,
            y: 0.0,
            followed,
            dragged: false,
            touch: true,
            selection: false,
        });
    });
    changed();
}

/// The finger lifted, or the browser cancelled it. A selection it leaves
/// holds the view until it goes.
fn on_lift(event: web_sys::Event, cancelled: bool) {
    let touching = event
        .dyn_ref::<web_sys::TouchEvent>()
        .is_some_and(|event| event.touches().length() > 0);
    if touching {
        return;
    }
    let selected = !cancelled && selection_in_log();
    let keep = STATE.with(|state| {
        let mut state = state.borrow_mut();
        match state.hold.as_mut() {
            Some(hold) if hold.touch && selected => {
                hold.selection = true;
                true
            }
            _ => false,
        }
    });
    if !keep {
        on_release(None);
    }
}

/// Whether a selection stands in the transcript.
fn selection_in_log() -> bool {
    let log = STATE.with(|state| state.borrow().log.clone());
    let (Some(log), Ok(Some(selection))) = (log, window().get_selection()) else {
        return false;
    };
    !selection.is_collapsed()
        && selection
            .anchor_node()
            .is_some_and(|node| log.contains(Some(&node)))
}

fn on_selection() {
    let held = STATE.with(|state| state.borrow().hold.is_some_and(|hold| hold.selection));
    if held && !selection_in_log() {
        on_release(None);
    }
}

/// The hold ends. A plain click by a follower, during which output took the
/// end off the screen, goes to the end.
fn on_release(event: Option<web_sys::Event>) {
    if event.as_ref().is_some_and(is_touch) {
        return;
    }
    let clicks = event
        .as_ref()
        .and_then(|event| event.dyn_ref::<web_sys::MouseEvent>())
        .map_or(1, |event| event.detail());
    let now = event.as_ref().map_or_else(now, |event| event.time_stamp());
    let out = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let hold = state.hold.take()?;
        let geometry = state.geometry()?;
        if hold.followed
            && !hold.dragged
            && clicks <= 1
            && state.choice == Choice::Follow
            && !geometry.shows(geometry.top)
        {
            return Some(state.end_now(now));
        }
        Some(state.write(&geometry, geometry.top))
    });
    if let Some(out) = out {
        out.apply();
    }
    changed();
}

fn now() -> f64 {
    window()
        .performance()
        .map_or(0.0, |performance| performance.now())
}

/// End, the marker and sending go to the end, closing the blank below it,
/// and follow.
pub fn to_end() {
    to_end_at(now());
}

/// End or the marker at the time of its event, `now`, from which a coast
/// still running moves nothing.
pub fn to_end_at(now: f64) {
    STATE.with(|state| state.borrow_mut().end_now(now)).apply();
}

/// Esc follows, from where the reader is where the end shows, and from the
/// end where it doesn't.
pub fn escape(now: f64) {
    let out = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let geometry = state.geometry()?;
        if !geometry.shows(geometry.top) {
            return Some(state.end_now(now));
        }
        state.coast = Coast::after(&state.gesture, now);
        state.chose(Choice::Follow);
        Some(state.write(&geometry, geometry.top))
    });
    if let Some(out) = out {
        out.apply();
    }
}

/// Home puts the start on the top row, and reads where a line moved.
fn to_start(now: f64) -> Option<Out> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let geometry = state.geometry()?;
        state.coast = Coast::after(&state.gesture, now);
        let choice = chosen(state.choice, geometry.top * geometry.ratio, false, false);
        state.chose(choice);
        Some(state.write(&geometry, 0.0))
    })
}

/// The keys that scroll the transcript, which the page takes so the
/// browser doesn't glide. `typing` says the message box (or another field)
/// has the keyboard, and `empty` that it holds nothing. Returns whether the
/// key was the transcript's.
pub fn key(event: &KeyboardEvent, typing: bool, empty: bool) -> bool {
    let shift = event.shift_key();
    let ctrl = event.ctrl_key() || event.meta_key();
    let focus_fold = event
        .target()
        .and_then(|target| target.dyn_into::<Element>().ok())
        .is_some_and(|target| {
            target
                .matches("summary, button, a, [tabindex]:not(#log)")
                .unwrap_or(false)
        });
    let (line, height) = STATE.with(|state| {
        let state = state.borrow();
        let height = state
            .log
            .as_ref()
            .map_or(600.0, |log| f64::from(log.client_height()));
        (state.line, height)
    });
    let step = 3.0 * line;
    let page = (height - 2.0 * line).max(line);
    let now = event.time_stamp();
    let by = match event.key().as_str() {
        "PageUp" => Some(-page),
        "PageDown" => Some(page),
        "ArrowUp" if !typing || shift => Some(-step),
        "ArrowDown" if !typing || shift => Some(step),
        " " if !typing && !focus_fold => Some(if shift { -page } else { page }),
        "Home" if ctrl || !typing || empty => {
            if let Some(out) = to_start(now) {
                out.apply();
            }
            return true;
        }
        "End" if ctrl || !typing || empty => {
            to_end_at(now);
            return true;
        }
        _ => None,
    };
    let Some(by) = by else {
        return false;
    };
    let out = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let geometry = state.geometry()?;
        Some(state.scroll_by(&geometry, by))
    });
    if let Some(out) = out {
        out.apply();
    }
    true
}

/// Another view (Full, Quiet, Messages) is about to show. A follower
/// follows at its end, with no blank. A reader keeps the block they left
/// on its row, or the next one the view shows, until they move, and so does
/// a follower whose fold took the end off the screen, also through a view
/// where the end shows.
pub fn view_changing() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.clicked = None;
        state.coast = Coast::after(&state.gesture, now());
        if state.kept.is_none() && state.painted.follows {
            state.pending = Some(Pending::End);
            return;
        }
        let kept = state.kept.clone().or_else(|| {
            let top = state.painted.top_line.as_ref()?;
            Some((top.anchor.clone(), top.offset))
        });
        state.kept = kept.clone();
        state.pending = kept.map(|(anchor, offset)| Pending::At(anchor, offset));
    });
    changed();
}

/// A worker's view is about to show (`worker`), or the main one again. A
/// worker's opens at its end, following, and the main one comes back as
/// the reader left it, with its choice, its top line and its marker.
pub fn worker_view(worker: bool) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.clicked = None;
        // (Another view starts what the window took afresh.)
        state.shrunk = 0.0;
        state.hidden = 0.0;
        state.coast = Coast::after(&state.gesture, now());
        if worker {
            if state.left.is_none() {
                state.left = Some(Left {
                    choice: state.choice,
                    place: state
                        .painted
                        .top_line
                        .as_ref()
                        .map(|top| (top.anchor.clone(), top.offset)),
                    baseline: state.baseline.clone(),
                    owed: state.owed,
                });
            }
            state.choice = Choice::Follow;
            state.owed = 0.0;
            state.pending = Some(Pending::End);
        } else {
            state.pending = Some(match state.left.take() {
                Some(left) => Pending::Back(left),
                None => Pending::End,
            });
        }
    });
    changed();
}

/// Another feed (Arko started again), or the first, opens at its end and
/// follows.
pub fn new_feed() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.choice = Choice::Follow;
        state.owed = 0.0;
        state.shrunk = 0.0;
        state.hidden = 0.0;
        state.left = None;
        state.kept = None;
        state.baseline = None;
        state.pending = Some(Pending::End);
    });
    changed();
}

/// The reader's choice.
#[cfg(test)]
pub fn choice() -> Choice {
    STATE.with(|state| state.borrow().choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_end_shows_until_a_whole_device_pixel_row_of_it_is_below_the_edge() {
        // At a ratio of 1, the end on the edge shows, and a pixel lower it
        // doesn't.
        assert!(end_shows(800.0, 800.0, 1.0));
        assert!(end_shows(780.0, 800.0, 1.0));
        assert!(!end_shows(801.0, 800.0, 1.0));
        // Less than a whole row below the edge still shows.
        assert!(end_shows(800.6, 800.0, 1.0));
        // At 1.25, an edge at 800.2 device px leaves row 801 the first
        // whole one below it.
        assert!(end_shows(641.5, 640.16, 1.25));
        assert!(!end_shows(642.0, 640.0, 1.25));
        // Float noise from sums of heights doesn't count.
        assert!(end_shows(800.000_000_1, 800.0, 1.0));
        assert!(!end_shows(800.999_999_9, 800.0, 1.0));
    }

    #[test]
    fn a_position_past_either_end_reads_as_that_end() {
        // The end 900 px down a view 600 tall, so the range ends at 300.
        assert_eq!(within(330.0, 900.0, 600.0, 300.0), 300.0);
        // A bounce's way back from there moves nothing.
        assert_eq!(
            within(310.0, 900.0, 600.0, 300.0),
            within(300.0, 900.0, 600.0, 300.0)
        );
        assert_eq!(within(-20.0, 900.0, 600.0, 0.0), 0.0);
        // With blank below the end, the range ends where the view stands.
        assert_eq!(within(420.0, 900.0, 600.0, 400.0), 400.0);
        assert_eq!(within(250.0, 900.0, 600.0, 400.0), 250.0);
    }

    #[test]
    fn a_scroll_up_reads_only_where_a_line_moves() {
        use Choice::*;
        // A device pixel or more down reads.
        assert_eq!(chosen(Follow, 1.0, false, true), Read);
        assert_eq!(chosen(Follow, 30.0, false, false), Read);
        // Less than one moves nothing and changes nothing.
        assert_eq!(chosen(Follow, 0.0, false, true), Follow);
        assert_eq!(chosen(Read, 0.0, false, false), Read);
    }

    #[test]
    fn a_scroll_down_after_which_the_end_shows_follows_also_when_nothing_moved() {
        use Choice::*;
        assert_eq!(chosen(Read, -12.0, true, true), Follow);
        assert_eq!(chosen(Read, 0.0, true, true), Follow);
        assert_eq!(chosen(Read, -12.0, true, false), Read);
        assert_eq!(chosen(Follow, 0.0, true, true), Follow);
    }

    #[test]
    fn a_shorter_view_takes_the_blank_first_and_a_taller_one_gives_back() {
        // 30 px of blank and 50 px shorter move the lines up 20, owed 20.
        assert_eq!(resized(-50.0, 30.0, 0.0, true), (20.0, 20.0));
        // Taller by 50 again gives the 20 back, and the rest is blank.
        assert_eq!(resized(50.0, 0.0, 20.0, true), (-20.0, 0.0));
        // Shorter by less than the blank moves nothing.
        assert_eq!(resized(-10.0, 30.0, 0.0, true), (0.0, 0.0));
        // A taller window with nothing owed moves nothing.
        assert_eq!(resized(80.0, 0.0, 0.0, true), (0.0, 0.0));
        // Reading, a shorter view moves nothing and owes nothing.
        assert_eq!(resized(-50.0, 0.0, 10.0, false), (0.0, 10.0));
        // Reading or held, a taller view takes from what is owed and moves
        // nothing.
        assert_eq!(resized(15.0, 0.0, 20.0, false), (0.0, 5.0));
    }

    #[test]
    fn a_panel_opened_and_closed_puts_every_line_back() {
        // With 0, 1 and 5 lines of blank (24 px each) and a box 3 lines tall.
        for blank in [0.0, 24.0, 120.0] {
            let (up, owed) = resized(-72.0, blank, 0.0, true);
            let blank_after = (blank - 72.0).max(0.0);
            let (down, owed) = resized(72.0, blank_after, owed, true);
            assert_eq!(up + down, 0.0, "{blank}");
            assert_eq!(owed, 0.0);
        }
    }

    #[test]
    fn the_marker_counts_new_lines_below_the_edge_rounded_up() {
        // Following, or with the end in sight, it doesn't show.
        assert_eq!(marker(true, false, 100.0, 24.0, 1.0), None);
        assert_eq!(marker(false, true, 100.0, 24.0, 1.0), None);
        // Nothing new below the edge says end.
        assert_eq!(marker(false, false, 0.0, 24.0, 1.0), Some(0));
        assert_eq!(marker(false, false, 0.4, 24.0, 2.0), Some(0));
        // Any new output below the edge counts a line.
        assert_eq!(marker(false, false, 3.0, 24.0, 1.0), Some(1));
        assert_eq!(marker(false, false, 24.0, 24.0, 1.0), Some(1));
        assert_eq!(marker(false, false, 24.5, 24.0, 1.0), Some(2));
        assert_eq!(marker(false, false, 168.0, 24.0, 1.0), Some(7));
        assert_eq!(marker_words(0), "end");
        assert_eq!(marker_words(1), "1 new line");
        assert_eq!(marker_words(7), "7 new lines");
    }

    #[test]
    fn a_gesture_carries_what_is_under_a_device_pixel() {
        let mut gesture = Gesture::default();
        // Three deltas of 0.4 move nothing twice, then a pixel.
        assert_eq!(gesture.take(-0.4, 0.0), 0.0);
        assert_eq!(gesture.take(-0.4, 10.0), 0.0);
        assert_eq!(gesture.take(-0.4, 20.0), -1.0);
        // Up 0.4 and down 0.4, repeated, move nothing.
        let mut gesture = Gesture::default();
        for at in 0..10 {
            let delta = if at % 2 == 0 { 0.4 } else { -0.4 };
            assert_eq!(gesture.take(delta, f64::from(at) * 10.0), 0.0);
        }
        // The carry goes with the gesture.
        let mut gesture = Gesture::default();
        assert_eq!(gesture.take(0.9, 0.0), 0.0);
        assert_eq!(gesture.take(0.2, 500.0), 0.0);
        assert_eq!(gesture.take(0.9, 510.0), 1.0);
    }

    #[test]
    fn a_coast_after_the_marker_moves_nothing_until_it_ends() {
        // A flick up, its last delta at 112 ms, and the marker at 140.
        let mut flick = Gesture::default();
        for at in 0..8 {
            flick.take(-30.0, f64::from(at) * 16.0);
        }
        let mut coast = Coast::after(&flick, 140.0).unwrap();
        // Thirty more coast events up move nothing.
        for at in 1..=30 {
            assert!(coast.takes(false, 140.0 + f64::from(at) * 16.0));
        }
        // A wheel event up 150 ms after the marker is the reader's.
        let mut coast = Coast::after(&flick, 140.0).unwrap();
        assert!(!coast.takes(false, 290.0));
        // A flick down, End during its coast, and an event up 20 ms later.
        let mut flick = Gesture::default();
        flick.take(30.0, 0.0);
        let mut coast = Coast::after(&flick, 40.0).unwrap();
        assert!(coast.takes(true, 48.0));
        assert!(!coast.takes(false, 68.0));
        // With no gesture going, there's no coast.
        assert!(Coast::after(&flick, 400.0).is_none());
        assert!(Coast::after(&Gesture::default(), 0.0).is_none());
    }

    /// Rows of `heights`, each an item's row.
    fn items(ids: &[u64]) -> Vec<Anchor> {
        ids.iter()
            .map(|id| Anchor {
                id: *id,
                key: format!("i{id}"),
            })
            .collect()
    }

    #[test]
    fn a_place_goes_back_to_its_row_or_the_nearest_left() {
        let at = |id: u64| Anchor {
            id,
            key: format!("i{id}"),
        };
        // Its own row, at the place's offset, wherever pending rows put it.
        assert_eq!(
            anchor_row(&items(&[3, 4, 5, 9, 7]), &at(5)),
            Some((2, true))
        );
        // Gone, the nearest row before it.
        assert_eq!(
            anchor_row(&items(&[3, 4, 6, 9, 7]), &at(5)),
            Some((1, true))
        );
        // The history let go of it and of every row before it, so the earliest
        // row left, from its top.
        assert_eq!(anchor_row(&items(&[8, 9, 10]), &at(5)), Some((0, false)));
        assert_eq!(anchor_row(&[], &at(5)), None);
    }

    /// A turn's count of calls and the Quiet row of its first call share an
    /// item, and a place keeps to its own row of the two. In another view,
    /// where neither is, it goes to its item's first row.
    #[test]
    fn a_place_keeps_to_its_own_row_of_rows_that_share_an_item() {
        let row = |id: u64, key: &str| Anchor {
            id,
            key: key.to_string(),
        };
        let open = [row(1, "i1"), row(2, "c1:2"), row(2, "q2"), row(6, "i6")];
        assert_eq!(anchor_row(&open, &row(2, "c1:2")), Some((1, true)));
        assert_eq!(anchor_row(&open, &row(2, "q2")), Some((2, true)));
        assert_eq!(anchor_row(&open, &row(2, "i2")), Some((1, true)));
    }

    #[test]
    fn a_row_that_got_shorter_while_away_shows_from_its_top() {
        // A row as tall as it was keeps the pixel.
        assert_eq!(bounded_offset(-189.5, 260.0), -189.5);
        assert_eq!(bounded_offset(12.0, 30.0), 12.0);
        // Its open disclosure closed, it's now shorter than the part that was
        // above the top, so it would scroll out of sight.
        assert_eq!(bounded_offset(-189.5, 40.0), 0.0);
        assert_eq!(bounded_offset(-189.5, 189.5), 0.0);
    }
}
