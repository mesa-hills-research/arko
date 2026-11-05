//! The message box, with the command menu, Stop and the files attached
//! ([`crate::attach`]), and the agent's questions and the approval tray
//! above it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{ArrowUp, Maximize2, Minimize2, Square};
use web_sys::HtmlTextAreaElement;

use crate::app::{
    DRAFT, Posted, Sender, get_json, page, post, send as send_to_arko, session_get, session_set,
};
use crate::protocol::MessageSwitch;
use crate::scroll;
use crate::slash::{self, Menu};

/// How long the words an argument may be stay good (a worker may start
/// meanwhile), in ms.
const CHOICES_FRESH: f64 = 10_000.0;

thread_local! {
    /// The words an argument may be, and when they came, by what was asked
    /// of which feed (another Arko has other workers).
    static CHOICES: std::cell::RefCell<HashMap<String, (Option<Vec<String>>, f64)>> = std::cell::RefCell::new(HashMap::new());
    /// What to know of the words that came, by feed and word (how a model
    /// billed per token is billed).
    static NOTES: std::cell::RefCell<HashMap<(Option<u64>, String), String>> = std::cell::RefCell::new(HashMap::new());
}

/// What the page knows of `word`, from the feed `feed`'s choices.
fn note_of(feed: Option<u64>, word: &str) -> String {
    NOTES.with(|notes| {
        notes
            .borrow()
            .get(&(feed, word.to_string()))
            .cloned()
            .unwrap_or_default()
    })
}

/// A message as it was sent, with its text, and the worker whose view it was
/// sent from (none for the main agent's).
#[derive(Debug, Clone, PartialEq)]
struct Draft {
    text: String,
    to: Option<String>,
}

/// What the box says while a send waits for Arko's answer longer than a
/// moment. Send works again meanwhile.
const STILL_SENDING: &str = "Still sending. Your text is kept.";

/// How long Send keeps its place after a send empties the box, so a second
/// tap can't land on Stop, in ms.
const SEND_HOLDS_MS: i32 = 1000;

/// Arko's question before a message continues a worker whose prompt cache
/// expired, with its words, and the message it was asked for.
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

    /// What Continue sends while the box holds `now`, namely the message as the
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

/// A message on its way, which emptied the box, with the approval tray's
/// switch as it was then, and whether its text came back (Arko was slow to
/// answer, or didn't take it).
#[derive(Debug, Clone)]
struct OnItsWay {
    draft: Draft,
    switch: MessageSwitch,
    back: bool,
    /// What the error line said of it while another view showed.
    words: Option<String>,
}

impl OnItsWay {
    fn new(draft: Draft, switch: MessageSwitch) -> Self {
        Self {
            draft,
            switch,
            back: false,
            words: None,
        }
    }

    /// The text, with anything typed since (`now`) on a line after it.
    fn before(&self, now: &str) -> String {
        if now.is_empty() {
            self.draft.text.clone()
        } else {
            format!("{}\n{now}", self.draft.text)
        }
    }

    /// What the box holds once the text comes back while it holds `now`.
    /// None once the text came back already.
    fn back_into(&mut self, now: &str) -> Option<String> {
        (!std::mem::replace(&mut self.back, true)).then(|| self.before(now))
    }

    /// The draft a reload brings back once Arko answered, while the box holds
    /// `now`. A message Arko took is no draft any more, whatever view shows.
    /// One it didn't take is, until it's back in the box.
    fn draft_after(&self, taken: bool, now: &str) -> String {
        if taken || self.back {
            now.to_string()
        } else {
            self.before(now)
        }
    }

    /// Whether the box empties again once Arko takes the message while the
    /// box holds `now`, as it does when the text came back unchanged.
    fn empties_when_taken(&self, now: &Draft) -> bool {
        self.back && *now == self.draft
    }
}

/// Whether Stop takes Send's place, which it does while a turn runs and the
/// box holds nothing to send, unless a send emptied the box a moment ago or
/// still waits for Arko's answer.
fn shows_stop(working: bool, nothing_to_send: bool, recent_sends: u32, sending: bool) -> bool {
    working && nothing_to_send && recent_sends == 0 && !sending
}

#[component]
pub fn Composer() -> impl IntoView {
    let page = page();
    let ask = page.ask;
    let input: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let empty = ask.box_empty;
    // What the box holds, which starts as the saved draft. (The box starts
    // with it, so no first fit can save an empty box over it, whichever way
    // the effects run.)
    let draft = RwSignal::new(session_get(DRAFT).unwrap_or_default());
    // The text reached the box's height, which the expand button lifts.
    let tall = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let sending = RwSignal::new(false);
    // The latest send, which owns the box. What becomes of an earlier one
    // leaves it as it is.
    let latest = RwSignal::new(0u64);
    // A send of its own, which says so while its answer is long in coming,
    // and then calls `slow`.
    let begin_send = move |slow: Box<dyn FnOnce()>| {
        let sender = Sender::from_view();
        latest.set(sender.id());
        sending.set(true);
        let waiting = sender.clone();
        spawn_local(async move {
            gloo_wait(5000).await;
            if latest.try_get_untracked() == Some(waiting.id()) && sending.get_untracked() {
                sending.set(false);
                slow();
                waiting.say(Some(STILL_SENDING.into()));
            }
        });
        sender
    };
    // Arko asks before a message continues a worker whose prompt cache
    // expired, with what it said, and the message to send again on Continue.
    let continue_asked = RwSignal::new(None::<ContinueAsk>);
    // What the box holds now, in the view shown.
    let draft_now = move || {
        input.get_untracked().map(|area| Draft {
            text: area.value(),
            to: page.worker.get_untracked(),
        })
    };
    // Another view, or another feed (Arko started again), drops the question.
    Effect::new(move |_| {
        page.worker.track();
        page.feeds.track();
        continue_asked.set(None);
    });
    // The files attached, kept across a reload of the tab. Another feed's
    // uploads are no use, and a page shown again goes on with its uploads.
    crate::attach::own_chips();
    crate::attach::restore();
    let attached = page.attached;
    // Whether an approval answer takes the box's words, kept here so a text
    // that comes back keeps what the tray made of it. Each feed's tray
    // starts it over.
    let switch = RwSignal::new(MessageSwitch::default());
    Effect::new(move |_| {
        page.feeds.track();
        crate::attach::feed_changed();
    });
    let _shown = window_event_listener(leptos::ev::visibilitychange, |_| {
        if document().visibility_state() == web_sys::VisibilityState::Visible {
            crate::attach::shown_again();
        }
    });
    // Files dragged over the page drop onto the box, wherever they're let go.
    let dropping = RwSignal::new(false);
    let _drag = window_event_listener(leptos::ev::dragover, move |event| {
        if crate::attach::drags_files(&event) && page.worker.get_untracked().is_none() {
            event.prevent_default();
            dropping.set(true);
        }
    });
    let _left = window_event_listener(leptos::ev::dragleave, move |event| {
        // (Leaving the window, not one element for another.)
        if event.related_target().is_none() {
            dropping.set(false);
        }
    });
    let _dropped = window_event_listener(leptos::ev::drop, move |event| {
        dropping.set(false);
        if crate::attach::drags_files(&event) {
            event.prevent_default();
            if page.worker.get_untracked().is_none() {
                let files = event.data_transfer().and_then(|transfer| transfer.files());
                crate::attach::attach(crate::attach::files_of(files));
            }
        }
    });
    // Send waits for every upload, and takes files without text. A message
    // to a worker takes none.
    let files_ready = move || match page.worker.get() {
        Some(_) => Some(Vec::new()),
        None => attached.with(|chips| crate::attach::ready(chips)),
    };
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
    // The sends that emptied the box in the last SEND_HOLDS_MS.
    let recent_sends = RwSignal::new(0u32);
    // Stop takes Send's place while a turn runs and the box holds nothing to
    // send.
    let stop_shown = move || {
        shows_stop(
            working(),
            empty.get() && (page.worker.get().is_some() || attached.with(Vec::is_empty)),
            recent_sends.get(),
            sending.get(),
        )
    };
    // The old model writes its report for a switch, so a message to the main
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
                &|word| note_of(crate::app::feed(), word),
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
        let text = area.value();
        empty.set(text.trim().is_empty());
        if draft.with_untracked(|draft| *draft != text) {
            draft.set(text.clone());
        }
        session_set(DRAFT, Some(&text));
    };
    Effect::new(move |_| {
        expanded.track();
        resize();
    });
    input.on_load(move |_| {
        // Fits the box again once laid out.
        let frame = wasm_bindgen::closure::Closure::once_into_js(resize);
        use wasm_bindgen::JsCast;
        let _ = window().request_animation_frame(frame.unchecked_ref());
    });
    // Send keeps its place a moment, so a second tap can't land on Stop.
    let hold_send = move || {
        recent_sends.update(|count| *count += 1);
        spawn_local(async move {
            gloo_wait(SEND_HOLDS_MS).await;
            recent_sends.try_update(|count| *count -= 1);
        });
    };
    // Empties the box for a message that went.
    let clear_sent = move |area: &HtmlTextAreaElement| {
        area.set_value("");
        expanded.set(false);
        resize();
        hold_send();
    };
    // A text sent from another view, which comes back once that view shows.
    let parked = StoredValue::new_local(None::<Rc<RefCell<OnItsWay>>>);
    // Puts the text of the message `way` back in the box, with the tray's
    // switch as it was at the send, in the view it was sent from.
    let put_back = move |way: &Rc<RefCell<OnItsWay>>| {
        if page.worker.get_untracked() != way.borrow().draft.to {
            parked.set_value(Some(way.clone()));
            return;
        }
        let Some(area) = input.get_untracked() else {
            return;
        };
        let mut way = way.borrow_mut();
        if let Some(text) = way.back_into(&area.value()) {
            area.set_value(&text);
            resize();
            switch.update(|switch| switch.restored(&way.switch, true));
        }
    };
    Effect::new(move |_| {
        let shown = page.worker.get();
        let Some(way) = parked.get_value() else {
            return;
        };
        if way.borrow().draft.to == shown {
            parked.set_value(None);
            put_back(&way);
            if let Some(words) = way.borrow().words.clone() {
                page.error.set(Some(words));
            }
        }
    });

    let send = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        let text = area.value();
        let Some(files) = files_ready() else {
            return;
        };
        if (text.trim().is_empty() && files.is_empty()) || sending.get_untracked() {
            return;
        }
        let sent = Draft {
            text: text.clone(),
            to: page.worker.get_untracked(),
        };
        // The box empties at once, as the message is on its way. Its text
        // comes back while Arko is slow to answer, and when Arko doesn't
        // take it. Until Arko takes it, it's the draft a reload gets back.
        let way = Rc::new(RefCell::new(OnItsWay::new(
            sent.clone(),
            switch.get_untracked(),
        )));
        clear_sent(&area);
        session_set(DRAFT, Some(&text));
        menu.set(None);
        let sender = begin_send(Box::new({
            let way = way.clone();
            move || {
                way.borrow_mut().words = Some(STILL_SENDING.into());
                put_back(&way);
            }
        }));
        // In a worker's view, it goes to that worker. With questions
        // attached, it answers them (and while the agent waits on a
        // question, a message answers it).
        let answers = match page.worker.get_untracked() {
            Some(_) => Vec::new(),
            None => ask.attached.get_untracked(),
        };
        let mut body = match page.worker.get_untracked() {
            Some(worker) => serde_json::json!({ "text": text, "to": worker }),
            None if !answers.is_empty() => {
                serde_json::json!({ "text": text, "answers": answers })
            }
            None => serde_json::json!({ "text": text }),
        };
        // (What it sends, so it removes those chips alone.)
        let (sent_keys, ids): (Vec<u64>, Vec<String>) = files.into_iter().unzip();
        if !ids.is_empty() {
            body["files"] = ids.into();
        }
        spawn_local(async move {
            let outcome = send_to_arko("/api/message", body, &sender).await;
            let taken = outcome.posted == Posted::Taken;
            if !taken {
                way.borrow_mut().words = outcome.words.clone();
                put_back(&way);
            } else if parked.with_value(|parked| {
                parked
                    .as_ref()
                    .is_some_and(|parked| Rc::ptr_eq(parked, &way))
            }) {
                parked.set_value(None);
            }
            // A later send owns the box now.
            if latest.try_get_untracked() != Some(sender.id()) {
                return;
            }
            match sender.take(outcome) {
                Some(Posted::Taken) => {
                    // The questions it answered, its files, and the text
                    // that came back meanwhile, unless the box holds another.
                    ask.attached
                        .update(|attached| attached.retain(|id| !answers.contains(id)));
                    crate::attach::sent(&sent_keys);
                    let now = draft_now();
                    if now
                        .as_ref()
                        .is_some_and(|now| way.borrow().empties_when_taken(now))
                    {
                        clear_sent(&area);
                    } else if !way.borrow().back {
                        // (The box emptied at the tap, a while ago.)
                        hold_send();
                    }
                    // What one sends is what one wants to see next.
                    scroll::jump_to_latest();
                }
                Some(Posted::Confirm(why)) => continue_asked
                    .set(draft_now().and_then(|now| ContinueAsk::new(why, sent, &now))),
                // Kept, or no longer the page's, so the box has it back.
                _ => {}
            }
            session_set(DRAFT, Some(&way.borrow().draft_after(taken, &area.value())));
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
        let sender = begin_send(Box::new(|| {}));
        spawn_local(async move {
            let outcome = send_to_arko("/api/message", body, &sender).await;
            // (A later send owns the box now, and what was typed meanwhile
            // stays.)
            if latest.try_get_untracked() != Some(sender.id()) {
                return;
            }
            if sender.take(outcome) == Some(Posted::Taken) && draft_now() == Some(asked.draft) {
                clear_sent(&area);
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
            stopping.try_set(false);
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
                // Each feed's requests in a tray of their own, whose IDs
                // the last one gave too.
                {move || {
                    page.feeds.track();
                    view! { <crate::approvals::Tray draft=draft switch=switch /> }
                }}
                <Show when=move || page.worker.get().is_none()>
                    <crate::attach::Chips />
                </Show>
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
                    <crate::attach::DropOverlay dropping=dropping />
                    <Show when=move || page.worker.get().is_none()>
                        <crate::attach::AttachButton />
                    </Show>
                    <div class="field">
                        <textarea
                            node_ref=input
                            prop:value=draft.get_untracked()
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
                            on:paste=move |event: web_sys::ClipboardEvent| {
                                // Files pasted without text are attached.
                                let Some(data) = event.clipboard_data() else { return };
                                let files = crate::attach::files_of(data.files());
                                let text = data.get_data("text/plain").unwrap_or_default();
                                if !files.is_empty() && text.is_empty() && page.worker.get_untracked().is_none() {
                                    event.prevent_default();
                                    crate::attach::attach(files);
                                }
                            }
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
                    <SendSlot
                        stop_shown=stop_shown
                        stop_title=move || if reporting() { "Skip the report" } else { "Stop the turn" }
                        stopping=stopping
                        on_stop=stop
                        send_label=move || if replying() != 0 { "Answer" } else { "Send" }
                        send_title=move || match (files_ready().is_none(), replying() != 0) {
                            (true, _) => "Waiting for uploads",
                            (false, true) => "Answer",
                            (false, false) => "Send",
                        }
                        send_disabled=move || match files_ready() {
                            None => true,
                            Some(files) => (empty.get() && files.is_empty()) || sending.get(),
                        }
                    />
                </form>
            </div>
        </footer>
    }
}

/// Send, and Stop in its place while `stop_shown`. Both stay in the one slot
/// and only one shows, so a turn that starts or ends moves nothing.
#[component]
fn SendSlot(
    #[prop(into)] stop_shown: Signal<bool>,
    #[prop(into)] stop_title: Signal<&'static str>,
    #[prop(into)] stopping: Signal<bool>,
    on_stop: impl FnMut(leptos::ev::MouseEvent) + 'static,
    #[prop(into)] send_label: Signal<&'static str>,
    #[prop(into)] send_title: Signal<&'static str>,
    #[prop(into)] send_disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="send-slot">
            <button
                type="submit"
                class="btn primary icon send"
                class:shown=move || !stop_shown.get()
                aria-label=send_label
                title=send_title
                disabled=send_disabled
            >
                <ArrowUp size=20 />
            </button>
            <button
                type="button"
                class="btn icon stop"
                class:shown=stop_shown
                aria-label="Stop"
                title=stop_title
                disabled=stopping
                on:click=on_stop
            >
                <Square size=14 />
            </button>
        </div>
    }
}

/// What the words an argument of `name` may be after `before` (and `typed`,
/// for a list completed from the start typed) are kept by, for the feed
/// they're asked of.
fn choices_key(feed: Option<u64>, name: &str, before: &[String], typed: Option<&str>) -> String {
    serde_json::json!([feed, name, before, typed]).to_string()
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
    let feed = crate::app::feed();
    let key = choices_key(feed, name, before, openrouter.then_some(typed));
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
                NOTES.with(|notes| {
                    let notes = &mut *notes.borrow_mut();
                    notes.extend(
                        answer
                            .notes
                            .into_iter()
                            .map(|(word, note)| ((feed, word), note)),
                    );
                });
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
        came.try_update(|count| *count += 1);
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
/// narrower box (a rotation) still fits, and this only notes
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

    /// The words an argument may be, and what's known of them, are another
    /// feed's for an Arko started again, whose r1 is another worker.
    #[test]
    fn choices_and_their_notes_are_each_feeds_own() {
        let before = ["r1".to_string()];
        let key = |feed| choices_key(feed, "/message", &before, None);
        assert_eq!(key(Some(1)), key(Some(1)));
        assert_ne!(key(Some(1)), key(Some(2)));
        assert_ne!(
            choices_key(Some(1), "/model", &[], Some("openrouter:a")),
            choices_key(Some(1), "/model", &[], Some("openrouter:b"))
        );
        NOTES.with(|notes| {
            let notes = &mut *notes.borrow_mut();
            notes.insert((Some(1), "gpt".into()), "billed per token".into());
        });
        assert_eq!(note_of(Some(1), "gpt"), "billed per token");
        assert_eq!(note_of(Some(2), "gpt"), "");
    }

    /// Send and Stop are both always in the one slot, whichever shows, so a
    /// turn that starts or ends changes nothing around them. Stop keeps its
    /// name, and Send stays the form's submit button.
    #[test]
    fn stop_takes_sends_place_in_one_slot() {
        let owner = Owner::new();
        owner.with(|| {
            let stop_shown = RwSignal::new(false);
            let render = || {
                view! {
                    <SendSlot
                        stop_shown=stop_shown
                        stop_title=|| "Stop the turn"
                        stopping=|| false
                        on_stop=|_| {}
                        send_label=|| "Send"
                        send_title=|| "Send"
                        send_disabled=|| false
                    />
                }
                .to_html()
            };
            for stop in [false, true] {
                stop_shown.set(stop);
                let html = render();
                assert!(
                    html.starts_with("<div class=\"send-slot\"><button type=\"submit\""),
                    "{html}"
                );
                assert_eq!(html.matches("<button").count(), 2, "{html}");
                assert!(
                    html.contains("aria-label=\"Stop\" title=\"Stop the turn\""),
                    "{html}"
                );
                assert_eq!(
                    html.contains("class=\"btn primary icon send shown\""),
                    !stop,
                    "{html}"
                );
                assert_eq!(
                    html.contains("class=\"btn icon stop shown\""),
                    stop,
                    "{html}"
                );
            }
        });
        // The slot holds both in one 40 px cell, and only the one shown is seen.
        let css = include_str!("../style/app.css");
        assert!(css.contains(".send-slot { display: grid; flex: none; }"));
        assert!(css.contains("  grid-area: 1 / 1;\n  width: 40px;\n  height: 40px;"));
        assert!(css.contains(".send-slot > .btn:not(.shown) { opacity: 0; visibility: hidden;"));
    }

    fn draft(text: &str, to: Option<&str>) -> Draft {
        Draft {
            text: text.into(),
            to: to.map(str::to_string),
        }
    }

    /// A send's text comes back once, as it was into an empty box, and before
    /// what was typed since, on a line of its own. A text that came back
    /// goes again once Arko takes it, unless it was changed meanwhile.
    #[test]
    fn a_sent_text_comes_back_once_and_goes_once_taken() {
        let sent = draft("Fix it", Some("r5"));
        let way = || OnItsWay::new(sent.clone(), MessageSwitch::default());
        // Refused, into an empty box.
        let mut refused = way();
        assert_eq!(refused.back_into("").as_deref(), Some("Fix it"));
        // Refused after the reader typed more.
        assert_eq!(
            way().back_into("and also").as_deref(),
            Some("Fix it\nand also")
        );
        // Slow, then refused, so it comes back once.
        let mut slow = way();
        assert_eq!(slow.back_into("").as_deref(), Some("Fix it"));
        assert_eq!(slow.back_into("Fix it"), None);
        // Slow, then taken, so the box empties if it's as it came back.
        assert!(slow.empties_when_taken(&sent));
        assert!(!slow.empties_when_taken(&draft("Fix it now", Some("r5"))));
        // Taken without coming back, so what's typed since stays.
        assert!(!way().empties_when_taken(&sent));
        // Arko asks first, and the text comes back before Continue compares.
        let mut asked = way();
        let now = asked.back_into("").unwrap();
        assert!(
            ContinueAsk::new("Words.".into(), sent.clone(), &draft(&now, Some("r5"))).is_some()
        );
    }

    /// A message Arko took is no draft for a reload, also when another view
    /// showed as it went. One it didn't take is, until it's back in the box.
    #[test]
    fn a_reload_brings_back_only_a_text_that_didnt_go() {
        let mut way = OnItsWay::new(draft("Fix it", Some("r5")), MessageSwitch::default());
        // Taken while the main view showed, its box empty or holding more.
        assert_eq!(way.draft_after(true, ""), "");
        assert_eq!(way.draft_after(true, "Hello"), "Hello");
        // Refused while another view showed, so it waits for its own.
        assert_eq!(way.draft_after(false, ""), "Fix it");
        assert_eq!(way.draft_after(false, "Hello"), "Fix it\nHello");
        // Back in the box, the box is the draft.
        assert_eq!(way.back_into("").as_deref(), Some("Fix it"));
        assert_eq!(way.draft_after(false, "Fix it"), "Fix it");
    }

    /// Stop stays off Send's place while a send waits for Arko's answer, and
    /// a moment after one emptied the box.
    #[test]
    fn stop_waits_for_a_sends_answer() {
        assert!(shows_stop(true, true, 0, false));
        assert!(!shows_stop(true, true, 0, true));
        assert!(!shows_stop(true, true, 1, false));
        assert!(!shows_stop(true, false, 0, false));
        assert!(!shows_stop(false, true, 0, false));
    }

    /// Arko's question stands for the message it was asked about. Once the
    /// text or the view changed while the message was on its way, or before
    /// Continue, nothing is sent, and the new draft stays. Continue sends
    /// the message as the box holds it, confirmed, from the main view too.
    #[test]
    fn the_question_holds_only_for_the_draft_it_was_asked_about() {
        let sent = draft("Go on.", Some("r3"));
        let ask = |now: &Draft| ContinueAsk::new("Words.".into(), sent.clone(), now);
        // Edited before Arko's answer came, or another view, so no question.
        assert!(ask(&draft("Go on. Now.", Some("r3"))).is_none());
        assert!(ask(&draft("Go on.", None)).is_none());
        assert!(ask(&draft("Go on.", Some("r4"))).is_none());
        // Unchanged, so Continue sends it, confirmed, while it stays so.
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
