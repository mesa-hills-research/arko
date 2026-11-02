//! A call's content, as an open row shows it: a read's lines with their
//! numbers muted, an edit's diff in the diff's colours, a command's output,
//! a message's text, another tool's result muted, each in its first lines
//! and the door to the rest, which Arko gives whole on demand
//! (`/api/result/{id}`). The lines read as the terminal reads them
//! ([`crate::content`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::app::{get_json, page};
use crate::content::{self, Line, Mark};
use crate::markdown;
use crate::protocol::{ContentKind, ItemData, ResultData};

/// The last line of a content shown whole, which shows its first lines
/// again, as the terminal says it.
const SHOW_LESS: &str = "↑ show the first lines only";

thread_local! {
    /// The whole contents Arko gave, by their result's ID.
    static WHOLES: RefCell<HashMap<u64, Rc<String>>> = RefCell::new(HashMap::new());
}

/// Forgets the whole contents, for another feed.
pub fn forget() {
    WHOLES.with(|wholes| wholes.borrow_mut().clear());
}

/// The whole content of the result `id`, once Arko gave it.
fn whole(id: u64) -> Option<Rc<String>> {
    WHOLES.with(|wholes| wholes.borrow().get(&id).cloned())
}

/// The content of the result `data` (a `result` item), with its door.
pub fn content_view(data: RwSignal<ItemData>) -> impl IntoView {
    let page = page();
    let id = data.with_untracked(|item| item.id);
    // A fetch that's under way, and one that found nothing.
    let asking = RwSignal::new(false);
    let gone = RwSignal::new(false);
    let open = move || page.doors_open.with(|open| open.contains(&id)) && whole(id).is_some();
    let lines = move || {
        let whole = open().then(|| whole(id)).flatten();
        data.with(|item| {
            let kind = item.content.unwrap_or(ContentKind::Muted);
            let text = whole.as_deref().map_or(item.text.as_str(), String::as_str);
            lines_view(kind, text)
        })
    };
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
        if asking.get_untracked() {
            return;
        }
        asking.set(true);
        spawn_local(async move {
            let got: Option<ResultData> = get_json(&format!("/api/result/{id}"), None).await;
            asking.set(false);
            match got {
                Some(result) => {
                    markdown::remember(&result.code);
                    WHOLES.with(|wholes| {
                        wholes.borrow_mut().insert(id, Rc::new(result.text));
                    });
                    page.doors_open.update(|open| {
                        open.insert(id);
                    });
                }
                None => gone.set(true),
            }
        });
    };
    let door = move || {
        let more = data.with(|item| item.more.clone())?;
        let label = if gone.get() {
            "No longer kept".to_string()
        } else if open() {
            SHOW_LESS.to_string()
        } else {
            more
        };
        Some(view! {
            <button type="button" class="door" disabled=move || gone.get() on:click=toggle>
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
