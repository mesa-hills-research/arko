//! A call's content, as an open row shows it: a read's lines with their
//! numbers muted, an edit's diff in the diff's colours, a command's output,
//! a message's text, another tool's result muted, each in its first lines
//! and the door to the rest, which Arko gives whole on demand
//! (`/api/result/{id}`). The lines read as the terminal reads them
//! ([`crate::content`]).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::app::{page, request};
use crate::content::{self, Line, Mark};
use crate::markdown;
use crate::protocol::{ContentKind, ItemData, ResultData};

/// The last line of a content shown whole, which shows its first lines
/// again, as the terminal says it.
const SHOW_LESS: &str = "↑ show the first lines only";

/// The door of a content whose whole Arko no longer keeps.
const GONE: &str = "No longer kept";

/// The door of a content whose whole didn't come, which tries again.
const FAILED: &str = "Couldn’t load the rest. Try again";

/// The bytes of whole contents the page keeps, the latest asked for.
const WHOLES_KEPT: usize = 8 * 1024 * 1024;

/// The whole contents Arko gave, by their result's ID, in the order they
/// came, within [`WHOLES_KEPT`].
#[derive(Default)]
struct Wholes {
    texts: HashMap<u64, Rc<String>>,
    order: VecDeque<u64>,
    bytes: usize,
}

impl Wholes {
    fn get(&self, id: u64) -> Option<Rc<String>> {
        self.texts.get(&id).cloned()
    }

    /// Keeps the whole `text` of the result `id`, and lets go of the oldest
    /// past the budget.
    fn keep(&mut self, id: u64, text: String) {
        self.let_go(id);
        self.bytes += text.len();
        self.texts.insert(id, Rc::new(text));
        self.order.push_back(id);
        while self.bytes > WHOLES_KEPT && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(text) = self.texts.remove(&oldest) {
                self.bytes -= text.len();
            }
        }
    }

    fn let_go(&mut self, id: u64) {
        if let Some(text) = self.texts.remove(&id) {
            self.bytes -= text.len();
            self.order.retain(|kept| *kept != id);
        }
    }
}

thread_local! {
    static WHOLES: RefCell<Wholes> = RefCell::new(Wholes::default());
}

/// Forgets the whole contents, for another feed.
pub fn forget() {
    WHOLES.with(|wholes| *wholes.borrow_mut() = Wholes::default());
}

/// Lets go of the whole content of the item `id`, which the page no longer
/// keeps.
pub fn let_go(id: u64) {
    WHOLES.with(|wholes| wholes.borrow_mut().let_go(id));
}

/// The whole content of the result `id`, once Arko gave it.
fn whole(id: u64) -> Option<Rc<String>> {
    WHOLES.with(|wholes| wholes.borrow().get(id))
}

/// What a request for a whole content came to.
#[derive(Debug, PartialEq)]
enum Fetched {
    Whole(ResultData),
    /// Arko no longer keeps it.
    Gone,
    /// It didn't come (the connection, or Arko failed), which another try
    /// may change.
    Failed,
}

/// What Arko's `answer` to a request for a whole content (its status and
/// body, or none when the request failed) says.
fn fetched(answer: Option<(u16, String)>) -> Fetched {
    match answer {
        Some((200..=299, body)) => {
            serde_json::from_str(&body).map_or(Fetched::Failed, Fetched::Whole)
        }
        Some((404, _)) => Fetched::Gone,
        _ => Fetched::Failed,
    }
}

/// The content of the result `data` (a `result` item), with its door.
pub fn content_view(data: RwSignal<ItemData>) -> impl IntoView {
    let page = page();
    let id = data.with_untracked(|item| item.id);
    // A fetch under way, and how the last one went.
    let asking = RwSignal::new(false);
    let state = RwSignal::new(None::<Fetched>);
    let open = move || page.doors_open.with(|open| open.contains(&id)) && whole(id).is_some();
    let lines = move || {
        let whole = open().then(|| whole(id)).flatten();
        data.with(|item| {
            let kind = item.content.unwrap_or(ContentKind::Muted);
            let text = whole.as_deref().map_or(item.text.as_str(), String::as_str);
            lines_view(kind, text)
        })
    };
    let gone = move || state.with(|state| matches!(state, Some(Fetched::Gone)));
    let toggle = move |_| {
        if open() {
            page.doors_open.update(|open| {
                open.remove(&id);
            });
            return;
        }
        if whole(id).is_some() {
            page.doors_open.update(|open| {
                open.insert(id);
            });
            return;
        }
        if asking.get_untracked() || gone() {
            return;
        }
        asking.set(true);
        spawn_local(async move {
            let asked = crate::app::feed();
            let answer = request("GET", &format!("/api/result/{id}"), None).await;
            asking.set(false);
            // (Another feed's item of that ID is another.)
            if crate::app::feed() != asked {
                return;
            }
            match fetched(answer) {
                Fetched::Whole(result) => {
                    markdown::remember(&result.code);
                    WHOLES.with(|wholes| wholes.borrow_mut().keep(id, result.text));
                    state.set(None);
                    page.doors_open.update(|open| {
                        open.insert(id);
                    });
                }
                other => state.set(Some(other)),
            }
        });
    };
    let door = move || {
        let more = data.with(|item| item.more.clone())?;
        let label = if gone() {
            GONE.to_string()
        } else if open() {
            SHOW_LESS.to_string()
        } else if state.with(|state| matches!(state, Some(Fetched::Failed))) {
            FAILED.to_string()
        } else {
            more
        };
        Some(view! {
            <button type="button" class="door" disabled=gone on:click=toggle>
                {label}
            </button>
        })
    };
    view! { {lines}{door} }
}

/// The lines of a content `text` of `kind`.
fn lines_view(kind: ContentKind, text: &str) -> AnyView {
    match kind {
        ContentKind::Lines | ContentKind::Diff => {
            let width = content::number_width(kind, text);
            // The code's lines highlighted, in the order its numbered lines
            // come (a diff's notes between them have none).
            let code = content::code(kind, text);
            let mut painted = code
                .as_deref()
                .and_then(markdown::code_lines_html)
                .unwrap_or_default()
                .into_iter();
            let diff = kind == ContentKind::Diff;
            let lines = text
                .split('\n')
                .map(|line| match content::line(kind, line) {
                    Line::Numbered { number, mark, text } => {
                        let class = match mark {
                            Mark::Added => "line add",
                            Mark::Removed => "line del",
                            Mark::Kept | Mark::None => "line",
                        };
                        let html = painted.next().unwrap_or_else(|| {
                            let mut html = String::new();
                            markdown::escape(&mut html, text);
                            html
                        });
                        let sign = diff.then(|| view! { <span class="sign">{mark.symbol().to_string()}</span> });
                        view! {
                            <div class=class>
                                <span class="ln" style=format!("min-width:{width}ch")>{number.to_string()}</span>
                                {sign}
                                <span class="text" inner_html=html></span>
                            </div>
                        }
                        .into_any()
                    }
                    Line::Plain(text) => view! { <div class="line note">{text.to_string()}</div> }.into_any(),
                })
                .collect_view();
            view! { <div class="held code" class:diff=diff>{lines}</div> }.into_any()
        }
        _ => {
            let plain = kind == ContentKind::Plain;
            view! { <pre class="held" class:plain=plain>{text.to_string()}</pre> }.into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arko's answer says whether the whole came, is no longer kept (404),
    /// or didn't come and may on another try.
    #[test]
    fn a_fetchs_answer_tells_gone_from_failed() {
        let body = serde_json::json!({"text": "a\nb"}).to_string();
        assert_eq!(
            fetched(Some((200, body))),
            Fetched::Whole(ResultData {
                text: "a\nb".into(),
                code: Vec::new()
            })
        );
        assert_eq!(fetched(Some((404, String::new()))), Fetched::Gone);
        for failed in [None, Some((500, String::new())), Some((200, "{".into()))] {
            assert_eq!(fetched(failed), Fetched::Failed);
        }
    }

    /// The page keeps the latest wholes within its budget, and lets go of
    /// one whose item it no longer keeps.
    #[test]
    fn wholes_are_kept_within_a_budget_and_go_with_their_items() {
        let mut wholes = Wholes::default();
        let big = "x".repeat(WHOLES_KEPT / 3);
        for id in 1..=4 {
            wholes.keep(id, big.clone());
        }
        assert!(wholes.get(1).is_none());
        assert!(wholes.get(4).is_some());
        assert!(wholes.bytes <= WHOLES_KEPT);
        wholes.let_go(4);
        assert!(wholes.get(4).is_none());
        assert_eq!(wholes.bytes, big.len() * 2);
        assert_eq!(wholes.order.len(), 2);
    }
}
