//! The message box, with the command menu and Stop, and the agent's
//! questions and the approval tray above it.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{ArrowUp, Maximize2, Minimize2, Square};
use web_sys::HtmlTextAreaElement;

use crate::app::{DRAFT, Posted, get_json, page, post, send as send_to_arko, session_get, session_set};
use crate::scroll;
use crate::slash::{self, Menu};

/// How long the words an argument may be stay good (a worker may start
/// meanwhile), in ms.
const CHOICES_FRESH: f64 = 10_000.0;

thread_local! {
    static CHOICES: std::cell::RefCell<HashMap<String, (Option<Vec<String>>, f64)>> = std::cell::RefCell::new(HashMap::new());
    /// What to know of the words that came, by word (how a model billed per
    /// token is billed).
    static NOTES: std::cell::RefCell<HashMap<String, String>> = std::cell::RefCell::new(HashMap::new());
}

/// A message as it was sent: its text, and the worker whose view it was
/// sent from (none for the main agent's).
#[derive(Debug, Clone, PartialEq)]
struct Draft {
    text: String,
    to: Option<String>,
}

/// What the box says while a send waits for Arko's answer longer than a
/// moment. Send works again meanwhile.
const STILL_SENDING: &str = "Still sending. Your text is kept.";

/// Arko's question before a message continues a worker whose prompt cache
/// expired: its words, and the message it was asked for.
#[derive(Debug, Clone)]
struct ContinueAsk {
    words: String,
    draft: Draft,
}

impl ContinueAsk {
    /// The question for Arko's answer `words` to the message sent as
    /// `draft`, while the box holds `now`. None once the text or the view
    /// changed while the message was on its way, since the answer is about a
    /// message that is no longer there.
    fn new(words: String, draft: Draft, now: &Draft) -> Option<Self> {
        (draft == *now).then_some(Self { words, draft })
    }

    /// What Continue sends while the box holds `now`: the message as the
    /// box holds it, marked confirmed. None once the text or the view
    /// changed.
    fn confirmed(&self, now: &Draft) -> Option<serde_json::Value> {
        (self.draft == *now).then(|| {
            let mut body = serde_json::json!({ "text": now.text, "confirm_uncached": true });
            if let Some(worker) = &now.to {
                body["to"] = worker.as_str().into();
            }
            body
        })
    }
}

#[component]
pub fn Composer() -> impl IntoView {
    let page = page();
    let ask = page.ask;
    let input: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let empty = ask.box_empty;
    // The text reached the box's height, which the expand button lifts.
    let tall = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let sending = RwSignal::new(false);
    // The latest send, which owns the box: what becomes of an earlier one
    // leaves it as it is.
    let latest = RwSignal::new(0u64);
    // A send of its own, which says so while its answer is long in coming.
    let begin_send = move || {
        let id = latest.get_untracked() + 1;
        latest.set(id);
        sending.set(true);
        spawn_local(async move {
            gloo_wait(5000).await;
            if latest.try_get_untracked() == Some(id) && sending.get_untracked() {
                sending.set(false);
                page.error.set(Some(STILL_SENDING.into()));
            }
        });
        id
    };
    // Arko asks before a message continues a worker whose prompt cache
    // expired: what it said, and the message to send again on Continue.
    let continue_asked = RwSignal::new(None::<ContinueAsk>);
    // What the box holds now, in the view shown.
    let draft_now = move || {
        input.get_untracked().map(|area| Draft {
            text: area.value(),
            to: page.worker.get_untracked(),
        })
    };
    // Another view drops the question.
    Effect::new(move |_| {
        page.worker.track();
        continue_asked.set(None);
    });
    let stopping = RwSignal::new(false);
    let menu = RwSignal::new(None::<Menu>);
    let selected = RwSignal::new(None::<usize>);
    // The text Esc closed the menu for.
    let closed_for = RwSignal::new(None::<String>);
    // Bumped when choices come, so the menu reads them again.
    let choices_came = RwSignal::new(0u32);
    // Enter sends where there's a keyboard and a mouse. On a touch screen
    // its key makes a new line, and Ctrl or Cmd+Enter or the button sends.
    let enter_sends = window()
        .match_media("(hover: hover) and (pointer: fine)")
        .ok()
        .flatten()
        .is_some_and(|list| list.matches());
    // The questions attached, which the next message answers.
    let replying = move || ask.attached.with(Vec::len);
    let placeholder = move || match (replying(), page.worker.get()) {
        (_, Some(worker)) => format!("Message {worker}"),
        (1, None) => "Answer Arko’s question".into(),
        (2.., None) => "Answer Arko’s questions".into(),
        (_, None) if page.status.with(|status| status.working) => "Add to the task".into(),
        (_, None) if enter_sends => "Message (Shift+Enter for a new line)".into(),
        (_, None) => "Message".into(),
    };
    let working = move || {
        page.status.with(|status| status.working)
            && page.connection.get() == crate::words::Connection::Live
    };
    // The old model writes its report for a switch: a message to the main
    // agent waits in the box. (A command, and a message to a worker, go.)
    let reporting =
        move || page.status.with(|status| status.reporting) && page.worker.get().is_none();

    let refresh_menu = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        let value = area.value();
        let element: &web_sys::Element = area.as_ref();
        let focused = document()
            .active_element()
            .is_some_and(|active| &active == element);
        let mut next = None;
        // (A reply to questions is words, not a command.)
        if value.starts_with('/')
            && replying() == 0
            && focused
            && closed_for.get_untracked().as_deref() != Some(value.as_str())
            && let Some(commands) = page.commands.get_untracked()
        {
            next = slash::menu(
                &commands,
                &value,
                &mut |name, before, typed| choices_of(name, before, typed, choices_came),
                &|word| NOTES.with(|notes| notes.borrow().get(word).cloned().unwrap_or_default()),
            );
        }
        if !value.starts_with('/') {
            CHOICES.with(|choices| choices.borrow_mut().clear());
        }
        selected.set(next.as_ref().and_then(|menu| menu.selected));
        menu.set(next);
    };
    Effect::new(move |_| {
        choices_came.track();
        page.commands.track();
        refresh_menu();
    });

    let resize = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        fit(&area, tall, expanded.get_untracked());
        empty.set(area.value().trim().is_empty());
        session_set(DRAFT, Some(&area.value()));
    };
    Effect::new(move |_| {
        expanded.track();
        resize();
    });
    input.on_load(move |area| {
        if let Some(draft) = session_get(DRAFT) {
            area.set_value(&draft);
        }
        let frame = wasm_bindgen::closure::Closure::once_into_js(resize);
        use wasm_bindgen::JsCast;
        let _ = window().request_animation_frame(frame.unchecked_ref());
    });

    let send = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        let text = area.value();
        if text.trim().is_empty() || sending.get_untracked() {
            return;
        }
        let id = begin_send();
        let sent = Draft {
            text: text.clone(),
            to: page.worker.get_untracked(),
        };
        // In a worker's view, it goes to that worker. With questions
        // attached, it answers them (and while the agent waits on a
        // question, a message answers it).
        let attached = ask.attached.get_untracked();
        let body = match page.worker.get_untracked() {
            Some(worker) => serde_json::json!({ "text": text, "to": worker }),
            None if !attached.is_empty() => {
                serde_json::json!({ "text": text, "answers": attached })
            }
            None => serde_json::json!({ "text": text }),
        };
        spawn_local(async move {
            let posted = send_to_arko("/api/message", body).await;
            // A later send owns the box now.
            if latest.try_get_untracked() != Some(id) {
                return;
            }
            match posted {
                Posted::Taken => {
                    if page.worker.get_untracked().is_none() {
                        ask.attached.set(Vec::new());
                    }
                    area.set_value("");
                    expanded.set(false);
                    resize();
                    // What one sends is what one wants to see next.
                    scroll::jump_to_latest();
                }
                Posted::Confirm(why) => continue_asked
                    .set(draft_now().and_then(|now| ContinueAsk::new(why, sent, &now))),
                Posted::Kept => {}
            }
            sending.set(false);
            refresh_menu();
        });
    };
    // Continue sends it again, confirmed, while the box holds it in the same
    // view. (Cancel, or a change to the text, leaves it in the box.)
    let continue_anyway = move |_| {
        let asked = continue_asked.get_untracked();
        continue_asked.set(None);
        let (Some(asked), Some(area)) = (asked, input.get_untracked()) else {
            return;
        };
        let Some(body) = draft_now().and_then(|now| asked.confirmed(&now)) else {
            return;
        };
        let id = begin_send();
        spawn_local(async move {
            let posted = send_to_arko("/api/message", body).await;
            // (A later send owns the box now, and what was typed meanwhile
            // stays.)
            if latest.try_get_untracked() != Some(id) {
                return;
            }
            if matches!(posted, Posted::Taken) && draft_now() == Some(asked.draft) {
                area.set_value("");
                expanded.set(false);
                resize();
                scroll::jump_to_latest();
            }
            sending.set(false);
            refresh_menu();
        });
    };

    let pick = move |at: usize| {
        let (Some(area), Some(open)) = (input.get_untracked(), menu.get_untracked()) else {
            return;
        };
        let value = format!("{}{}", &area.value()[..open.from], open.entries[at].insert);
        area.set_value(&value);
        let end = value.len() as u32;
        let _ = area.set_selection_range(end, end);
        let _ = area.focus();
        resize();
        refresh_menu();
    };

    let keydown = move |event: web_sys::KeyboardEvent| {
        ask.typed_at.set(js_sys::Date::now());
        // (Safari reports the Enter that ends a composition as keyCode 229.)
        if event.is_composing() || event.key_code() == 229 {
            return;
        }
        let plain =
            !event.ctrl_key() && !event.meta_key() && !event.shift_key() && !event.alt_key();
        if plain && let Some(open) = menu.get_untracked() {
            let typed = input
                .get_untracked()
                .map(|area| area.value()[open.from..].to_string())
                .unwrap_or_default();
            if let Some(action) = slash::key(&open, selected.get_untracked(), &event.key(), &typed)
            {
                event.prevent_default();
                // (Esc closes the menu before it leaves a worker's view.)
                event.stop_propagation();
                match action {
                    slash::Key::Close => {
                        closed_for.set(input.get_untracked().map(|area| area.value()));
                        refresh_menu();
                    }
                    slash::Key::Pick(at) => pick(at),
                    slash::Key::Select(at) => selected.set(Some(at)),
                }
                return;
            }
        }
        if event.key() != "Enter" {
            return;
        }
        if event.ctrl_key()
            || event.meta_key()
            || (enter_sends && !event.shift_key() && !event.alt_key())
        {
            event.prevent_default();
            send();
        }
    };

    let stop = move |_| {
        stopping.set(true);
        spawn_local(async move {
            post("/api/stop", serde_json::json!({})).await;
            // The status hides it when the turn ends, and a second tap waits a moment.
            gloo_wait(1500).await;
            stopping.set(false);
        });
    };

    view! {
        <footer class="bottom">
            <div class="column">
                <crate::questions::AnswerPanel />
                <crate::questions::QuestionsChip />
                {move || page.ask.note.get().map(|note| view! { <p class="note-line" role="status">{note}</p> })}
                {move || page.error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
                <Show when=reporting>
                    <p class="note-line" role="status">{crate::words::REPORTING}</p>
                </Show>
                {move || continue_asked.get().map(|asked| view! {
                    <div class="confirm-line" role="alertdialog" aria-label="Continue the worker">
                        <p class="note-line">{asked.words}</p>
                        <button type="button" class="btn ghost small" on:click=move |_| continue_asked.set(None)>"Cancel"</button>
                        <button type="button" class="btn primary small" on:click=continue_anyway>"Continue"</button>
                    </div>
                })}
                <crate::questions::Replying />
                <crate::approvals::Tray />
                <form
                    class="compose"
                    class:expanded=move || expanded.get()
                    on:submit=move |event| {
                        event.prevent_default();
                        send();
                    }
                >
                    {move || menu.get().map(|open| {
                        let hint = open.command.clone();
                        let from_commands = open.from == 0;
                        view! {
                            // A tap on the menu leaves the focus in the box, so a phone keeps its keyboard.
                            <div class="slash" on:mousedown=|event| event.prevent_default()>
                                {hint.map(|command| view! {
                                    <div class="slash-hint" id="slash-hint">
                                        <span class="slash-name">{command.name}</span>
                                        <span class="slash-args">{format!(" {}", command.args)}</span>
                                        <span class="slash-about">{command.about}</span>
                                    </div>
                                })}
                                {(!open.entries.is_empty()).then(|| view! {
                                    <ul class="slash-list" role="listbox" id="slash-list" aria-label=if from_commands { "Commands" } else { "Choices" }>
                                        {open.entries.iter().enumerate().map(|(at, entry)| {
                                            let entry = entry.clone();
                                            let word = entry.about.is_empty();
                                            view! {
                                                <li id=format!("slash-{at}") role="option" class=if word { "slash-word" } else { "slash-command" }
                                                    aria-selected=move || (selected.get() == Some(at)).to_string()
                                                    on:click=move |_| pick(at)>
                                                    <span class="slash-name">{entry.label.clone()}</span>
                                                    {(!entry.args.is_empty()).then(|| view! { <span class="slash-args">{entry.args.clone()}</span> })}
                                                    {(!entry.note.is_empty()).then(|| view! { <span class="slash-args">{entry.note.clone()}</span> })}
                                                    {(!word).then(|| view! { <span class="slash-about">{entry.about.clone()}</span> })}
                                                </li>
                                            }
                                        }).collect_view()}
                                    </ul>
                                })}
                            </div>
                        }
                    })}
                    <div class="field">
                        <textarea
                            node_ref=input
                            rows="1"
                            aria-label=placeholder
                            placeholder=placeholder
                            role="combobox"
                            aria-autocomplete="list"
                            aria-expanded=move || menu.with(|open| open.as_ref().is_some_and(|open| !open.entries.is_empty())).to_string()
                            aria-activedescendant=move || selected.get().map(|at| format!("slash-{at}"))
                            enterkeyhint=if enter_sends { "send" } else { "enter" }
                            on:input=move |_| { continue_asked.set(None); resize(); refresh_menu(); }
                            on:focus=move |_| refresh_menu()
                            on:blur=move |_| { menu.set(None); CHOICES.with(|choices| choices.borrow_mut().clear()); }
                            on:keydown=keydown
                        ></textarea>
                        <button
                            type="button"
                            class="btn ghost icon expand"
                            class:shown=move || tall.get() || expanded.get()
                            aria-label=move || if expanded.get() { "Collapse" } else { "Expand" }
                            title=move || if expanded.get() { "Collapse" } else { "Expand" }
                            on:click=move |_| {
                                expanded.update(|open| *open = !*open);
                                if let Some(area) = input.get_untracked() {
                                    let _ = area.focus();
                                }
                            }
                        >
                            <Show when=move || expanded.get() fallback=|| view! { <Maximize2 size=16 /> }>
                                <Minimize2 size=16 />
                            </Show>
                        </button>
                    </div>
                    <Show when=working>
                        <button type="button" class="btn stop" title=move || if reporting() { "Skip the report" } else { "Stop the turn" } disabled=move || stopping.get() on:click=stop>
                            <Square size=14 />
                            <span>"Stop"</span>
                        </button>
                    </Show>
                    <button
                        type="submit"
                        class="btn primary icon send"
                        aria-label=move || if replying() != 0 { "Answer" } else { "Send" }
                        title=move || if replying() != 0 { "Answer" } else { "Send" }
                        disabled=move || empty.get() || sending.get()
                    >
                        <ArrowUp size=20 />
                    </button>
                </form>
            </div>
        </footer>
    }
}

/// The words an argument may be after `before`, asked of Arko and kept a
/// while, or none while they're on their way.
fn choices_of(
    name: &str,
    before: &[String],
    typed: &str,
    came: RwSignal<u32>,
) -> Option<Vec<String>> {
    // An OpenRouter model is completed from its list, for the start typed.
    let openrouter = name == "/model" && typed.to_lowercase().starts_with("openrouter:");
    let key = if openrouter {
        serde_json::json!([name, before, typed]).to_string()
    } else {
        serde_json::json!([name, before]).to_string()
    };
    let now = js_sys::Date::now();
    let hit = CHOICES.with(|choices| choices.borrow().get(&key).cloned());
    if let Some((words, at)) = &hit
        && now - at < CHOICES_FRESH
    {
        return words.clone();
    }
    let known = hit.and_then(|(words, _)| words);
    // (Words that are known stay while they're asked for again.)
    CHOICES.with(|choices| {
        choices
            .borrow_mut()
            .insert(key.clone(), (known.clone(), now))
    });
    let body = if openrouter {
        serde_json::json!({ "command": name, "before": before, "typed": typed })
    } else {
        serde_json::json!({ "command": name, "before": before })
    };
    spawn_local(async move {
        #[derive(serde::Deserialize)]
        struct Answer {
            #[serde(default)]
            choices: Vec<String>,
            #[serde(default)]
            notes: HashMap<String, String>,
        }
        let answer = get_json::<Answer>("/api/choices", Some(body)).await;
        let words = answer
            .map(|answer| {
                NOTES.with(|notes| notes.borrow_mut().extend(answer.notes));
                answer.choices
            })
            .unwrap_or_default();
        // (The list may still be on its way, so no words stay good for 1.5 s only.)
        let fresh = if openrouter && words.is_empty() {
            1500.0
        } else {
            CHOICES_FRESH
        };
        CHOICES.with(|choices| {
            choices.borrow_mut().insert(
                key,
                (Some(words), js_sys::Date::now() - (CHOICES_FRESH - fresh)),
            )
        });
        came.update(|count| *count += 1);
    });
    known
}

/// Puts the focus in the message box.
pub fn focus() {
    use wasm_bindgen::JsCast;
    if let Some(area) = document()
        .query_selector(".compose textarea")
        .ok()
        .flatten()
    {
        let _ = area.unchecked_into::<web_sys::HtmlElement>().focus();
    }
}

/// Waits `ms`.
pub async fn gloo_wait(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Sizes the box to its text, up to its cap (CSS's `max-height`), and
/// shows a scrollbar only past the cap. An empty box never scrolls.
///
/// Where the browser has `field-sizing`, it sizes the box at every layout,
/// so text drawn without an input event (iOS's inline predictions) or a
/// narrower box (Stop showing, a rotation) still fits, and this only notes
/// whether the text is past the cap.
fn fit(area: &HtmlTextAreaElement, tall: RwSignal<bool>, expanded: bool) {
    // (Sizing shrinks the box for a moment, which would scroll the
    // transcript up.)
    scroll::keeping_bottom(|| {
        let sizes_itself = window()
            .get_computed_style(area)
            .ok()
            .flatten()
            .and_then(|style| style.get_property_value("field-sizing").ok())
            .is_some_and(|value| value == "content");
        let style = web_sys::HtmlElement::style(area);
        if !sizes_itself {
            let _ = style.set_property("height", "auto");
            let needed = area.scroll_height();
            let _ = style.set_property("height", &format!("{needed}px"));
        }
        // `height` is capped by `max-height`, so a box that holds less than
        // it needs is past the cap.
        let over = area.scroll_height() > area.client_height() + 1;
        if !sizes_itself {
            let _ = style.set_property("overflow-y", if over { "auto" } else { "hidden" });
        }
        tall.set(over || expanded);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str, to: Option<&str>) -> Draft {
        Draft {
            text: text.into(),
            to: to.map(str::to_string),
        }
    }

    /// Arko's question stands for the message it was asked about: once the
    /// text or the view changed while the message was on its way, or before
    /// Continue, nothing is sent, and the new draft stays. Continue sends
    /// the message as the box holds it, confirmed, from the main view too.
    #[test]
    fn the_question_holds_only_for_the_draft_it_was_asked_about() {
        let sent = draft("Go on.", Some("r3"));
        let ask = |now: &Draft| ContinueAsk::new("Words.".into(), sent.clone(), now);
        // Edited before Arko's answer came, or another view: no question.
        assert!(ask(&draft("Go on. Now.", Some("r3"))).is_none());
        assert!(ask(&draft("Go on.", None)).is_none());
        assert!(ask(&draft("Go on.", Some("r4"))).is_none());
        // Unchanged: Continue sends it, confirmed, while it stays so.
        let asked = ask(&draft("Go on.", Some("r3"))).unwrap();
        assert_eq!(asked.words, "Words.");
        assert_eq!(
            asked.confirmed(&draft("Go on.", Some("r3"))),
            Some(serde_json::json!({"text": "Go on.", "to": "r3", "confirm_uncached": true}))
        );
        assert_eq!(asked.confirmed(&draft("Something else.", Some("r3"))), None);
        assert_eq!(asked.confirmed(&draft("Go on.", None)), None);
        let typed = draft("/message r3 Go on.", None);
        let asked = ContinueAsk::new("Words.".into(), typed.clone(), &typed).unwrap();
        assert_eq!(
            asked.confirmed(&typed),
            Some(serde_json::json!({"text": "/message r3 Go on.", "confirm_uncached": true}))
        );
    }
}
