//! The agent's questions, the answer panel above the message box, the chips
//! that stand for questions not in it, and a question card's line and
//! record in the transcript. A question shows in one place at a time, in
//! the panel while it's open, then in its card's record. The user never
//! sees a question's id, only its number in its ask.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{MessageCircleQuestionMark, X};
use wasm_bindgen::JsCast;

use crate::app::{Posted, RowCell, Sender, page, send as send_to_arko};
use crate::protocol::{CHOOSE_ANY, QuestionData};

/// How long after a key the user still counts as typing, so a question
/// that comes then waits in a chip rather than open the panel (ms).
const TYPING_REST: f64 = 2000.0;

/// What the user filled in for one question.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub choices: Vec<String>,
    pub you_decide: bool,
    /// "Other", or the answer of a question without choices.
    pub other: String,
}

/// The questions as the page shows them.
#[derive(Clone, Copy)]
pub struct Asking {
    /// The answer panel shows.
    pub panel: RwSignal<bool>,
    /// How many times it opened, which tells an answer sent from one opening
    /// from what the panel shows since (`Asking::open`).
    pub opening: RwSignal<u64>,
    /// The questions the next message answers ("Replying to 2 questions").
    pub attached: RwSignal<Vec<String>>,
    /// The questions the panel has shown, so the others are new.
    pub seen: RwSignal<Vec<String>>,
    /// What's filled in, by question id, so it stays with its question.
    pub drafts: RwSignal<HashMap<String, Draft>>,
    /// When the message box last had a key (ms), and whether it's empty.
    pub typed_at: RwSignal<f64>,
    pub box_empty: RwSignal<bool>,
    /// The question whose part has the keyboard in the panel.
    pub focused: RwSignal<Option<String>>,
    /// The questions this page answered or dismissed, until they're settled.
    pub answering: RwSignal<Vec<String>>,
    /// A word on the questions in the panel's place, and the panel's height
    /// it keeps, until the reader types, sends or closes it.
    pub note: RwSignal<Option<String>>,
    pub room: RwSignal<Option<f64>>,
    /// The panel's height as it's drawn, which a word in its place keeps
    /// once its questions are gone.
    pub drawn: RwSignal<Option<f64>>,
}

impl Asking {
    pub fn new() -> Self {
        Self {
            panel: RwSignal::new(false),
            opening: RwSignal::new(0),
            attached: RwSignal::new(Vec::new()),
            seen: RwSignal::new(Vec::new()),
            drafts: RwSignal::new(HashMap::new()),
            typed_at: RwSignal::new(0.0),
            box_empty: RwSignal::new(true),
            focused: RwSignal::new(None),
            answering: RwSignal::new(Vec::new()),
            note: RwSignal::new(None),
            room: RwSignal::new(None),
            drawn: RwSignal::new(None),
        }
    }

    /// Shows the answer panel. Opened again, it's another opening, so what
    /// became of an answer sent from the last one leaves it as it is. An
    /// open of its own (a question that came) keeps the room a word held in
    /// its place, and a reader's open releases it first ([`release`](Self::release)).
    pub fn open(&self) {
        // (A panel that opens where a word stood keeps the word's room until
        // the reader goes on.)
        if self.note.get_untracked().is_some() {
            self.note.set(None);
        }
        if !self.panel.get_untracked() {
            self.opening.update(|opening| *opening += 1);
            self.panel.set(true);
        }
    }

    /// The word in the panel's place goes, and the room it held, as the
    /// reader goes on. Every reader action in the panel and the message box
    /// calls it, and so does the panel closing with no word in its place.
    pub fn release(&self) {
        if self.note.get_untracked().is_some() {
            self.note.set(None);
        }
        if self.room.get_untracked().is_some() {
            self.room.set(None);
        }
    }

    /// Forgets the questions of a feed that's gone, whose IDs another
    /// session gives again.
    pub fn clear(&self) {
        self.panel.set(false);
        self.attached.set(Vec::new());
        self.seen.set(Vec::new());
        self.drafts.set(HashMap::new());
        self.focused.set(None);
        self.answering.set(Vec::new());
        self.release();
        self.drawn.set(None);
    }
}

// ----- words -----

/// "Arko asks 2 questions" is the panel's title, and the chip of new ones.
pub fn asks_text(questions: &[QuestionData]) -> String {
    match questions.len() {
        1 => "Arko asks a question".to_string(),
        count => format!("Arko asks {count} questions"),
    }
}

/// "2 open questions" is the quiet chip of the questions not attached.
pub fn open_text(count: usize) -> String {
    crate::protocol::plural(count, "open question", "open questions")
}

/// "Replying to 2 questions" names the questions the next message answers.
pub fn replying_text(count: usize) -> String {
    match count {
        1 => "Replying to a question".to_string(),
        count => format!("Replying to {count} questions"),
    }
}

/// What became of a question, as its card's record says it, the answer,
/// or a few words. `None` while it's open.
pub fn outcome(question: &QuestionData) -> Option<String> {
    Some(match question.state.as_str() {
        "open" => return None,
        "answered" => question
            .answer
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string(),
        "answered_in_message" => "Answered in your message".to_string(),
        "closed" => match question
            .why
            .as_deref()
            .map(str::trim)
            .filter(|why| !why.is_empty())
        {
            Some(why) => format!("Arko closed it. {why}"),
            None => "Arko closed it".to_string(),
        },
        "dismissed" => "Dismissed".to_string(),
        // A state a later Arko adds.
        _ => "Closed".to_string(),
    })
}

/// A question's words without its number.
pub fn text(question: &QuestionData) -> String {
    question.question.trim().to_string()
}

/// The body of `POST /api/questions/answer` for what is filled in, and the
/// ids it answers, or none when nothing is. It names every question shown,
/// an unanswered one by its id alone.
pub fn answers_body(
    questions: &[QuestionData],
    drafts: &HashMap<String, Draft>,
) -> Option<(serde_json::Value, Vec<String>)> {
    let mut answers = Vec::new();
    let mut ids = Vec::new();
    for question in questions {
        let draft = drafts.get(&question.id).cloned().unwrap_or_default();
        // In the question's order, whatever order they were picked in.
        let choices: Vec<&String> = question
            .choices
            .iter()
            .filter(|choice| draft.choices.contains(choice))
            .collect();
        let other = draft.other.trim();
        if choices.is_empty() && other.is_empty() && !draft.you_decide {
            answers.push(serde_json::json!({ "id": question.id }));
            continue;
        }
        let mut answer = serde_json::json!({ "id": question.id });
        if !choices.is_empty() {
            answer["choices"] = serde_json::json!(choices);
        }
        if !other.is_empty() {
            answer["text"] = serde_json::json!(other);
        }
        if draft.you_decide {
            answer["you_decide"] = serde_json::json!(true);
        }
        answers.push(answer);
        ids.push(question.id.clone());
    }
    if ids.is_empty() {
        return None;
    }
    Some((serde_json::json!({ "answers": answers }), ids))
}

/// Whether the panel opens by itself for the open questions `open`, when one
/// isn't `seen` yet, and the user neither types nor replies to questions
/// `attached` to the message box. (The panel would show those again, and
/// "Answer in chat" would attach the new one with them.)
pub fn opens_by_itself(
    open: &[String],
    seen: &[String],
    attached: &[String],
    typing: bool,
) -> bool {
    open.iter().any(|id| !seen.contains(id)) && !typing && attached.is_empty()
}

/// The open questions a chip stands for, those not attached.
pub fn waiting(open: &[QuestionData], attached: &[String]) -> Vec<QuestionData> {
    open.iter()
        .filter(|question| !attached.contains(&question.id))
        .cloned()
        .collect()
}

// ----- what the panel does -----

fn open_questions() -> Vec<QuestionData> {
    page()
        .status
        .with_untracked(|status| status.questions.clone())
}

/// Follows the open questions. What's filled in and attached stays only
/// for those still open, the panel closes when none is, and opens by itself
/// for new ones ([`opens_by_itself`]).
pub fn follow() {
    let page = page();
    let ask = page.ask;
    Effect::new(move |_| {
        let open: Vec<String> = page.status.with(|status| {
            status
                .questions
                .iter()
                .map(|question| question.id.clone())
                .collect()
        });
        if ask
            .drafts
            .with_untracked(|drafts| drafts.keys().any(|id| !open.contains(id)))
        {
            ask.drafts
                .update(|drafts| drafts.retain(|id, _| open.contains(id)));
        }
        if ask
            .attached
            .with_untracked(|attached| attached.iter().any(|id| !open.contains(id)))
        {
            ask.attached
                .update(|attached| attached.retain(|id| open.contains(id)));
        }
        // The question being typed in was settled elsewhere (another
        // screen, or the agent), so the panel closes, rather than move the
        // typing to another question.
        let gone = ask.focused.get_untracked().filter(|id| {
            !open.contains(id)
                && !ask
                    .answering
                    .with_untracked(|answering| answering.contains(id))
        });
        if ask
            .answering
            .with_untracked(|answering| answering.iter().any(|id| !open.contains(id)))
        {
            ask.answering
                .update(|answering| answering.retain(|id| open.contains(id)));
        }
        if gone.is_some() && ask.panel.get_untracked() {
            // Its word takes the panel's place and keeps its height, so
            // nothing moves until the reader goes on.
            ask.focused.set(None);
            ask.room.set(ask.drawn.get_untracked());
            ask.note
                .set(Some("That question is no longer open.".into()));
            ask.panel.set(false);
            crate::composer::focus();
            return;
        }
        if open.is_empty() {
            if ask.note.with_untracked(Option::is_none) {
                ask.release();
            }
            ask.panel.set(false);
            return;
        }
        let typing = !ask.box_empty.get_untracked()
            || js_sys::Date::now() - ask.typed_at.get_untracked() < TYPING_REST;
        let opens = ask.seen.with_untracked(|seen| {
            ask.attached
                .with_untracked(|attached| opens_by_itself(&open, seen, attached, typing))
        });
        if opens {
            ask.open();
        }
    });
    // What the panel shows is seen.
    Effect::new(move |_| {
        if !ask.panel.get() {
            return;
        }
        let open: Vec<String> = page.status.with(|status| {
            status
                .questions
                .iter()
                .map(|question| question.id.clone())
                .collect()
        });
        if ask
            .seen
            .with_untracked(|seen| open.iter().any(|id| !seen.contains(id)))
        {
            ask.seen.update(|seen| {
                let fresh: Vec<String> = open.into_iter().filter(|id| !seen.contains(id)).collect();
                seen.extend(fresh);
            });
        }
    });
}

/// Opens the panel, on the first open question of the card `card` or on
/// the first question, and gives it the keyboard (Alt+Q, a chip, a card's
/// line).
pub fn open_panel(card: Option<u64>) {
    open_on(card, None);
}

/// Opens the panel on the open question `id` (its card `card` was clicked
/// on it), else on the card's first open question.
fn open_question(card: u64, id: &str) {
    open_on(Some(card), Some(id));
}

fn open_on(card: Option<u64>, id: Option<&str>) {
    let page = page();
    let open = open_questions();
    if open.is_empty() {
        return;
    }
    page.sheet.set(None);
    page.ask.release();
    page.ask.open();
    let target = id
        .and_then(|id| open.iter().find(|question| question.id == id))
        .or_else(|| card.and_then(|card| open.iter().find(|question| question.item == card)))
        .or(open.first())
        .map(|question| question.id.clone());
    let frame = wasm_bindgen::closure::Closure::once_into_js(move || {
        let field = target.and_then(|id| document().get_element_by_id(&format!("answer-{id}")));
        if let Some(field) = field {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            field.scroll_into_view_with_scroll_into_view_options(&options);
            let _ = field.unchecked_into::<web_sys::HtmlElement>().focus();
        }
    });
    let _ = window().request_animation_frame(frame.unchecked_ref());
}

/// The question an element of the panel belongs to, by its id.
fn question_of(target: Option<web_sys::EventTarget>) -> Option<String> {
    target?
        .dyn_into::<web_sys::Element>()
        .ok()?
        .closest("[data-question]")
        .ok()??
        .get_attribute("data-question")
}

/// Closes the panel, the questions left as they are (×). The keyboard goes
/// back to the message box if the panel had it.
fn close_panel() {
    // (Once its questions are settled the panel may be gone already, with
    // the keyboard nowhere.)
    let body = document().body().map(web_sys::Element::from);
    let had = document().active_element().is_none_or(|active| {
        Some(&active) == body.as_ref() || active.closest(".answers").ok().flatten().is_some()
    });
    page().ask.release();
    page().ask.panel.set(false);
    if had {
        crate::composer::focus();
    }
}

/// Closes the panel and attaches the open questions to the message box,
/// which the next message answers ("Answer in chat", Esc). The keyboard
/// goes to the message box.
fn answer_in_chat() {
    let ask = page().ask;
    ask.attached.set(
        open_questions()
            .into_iter()
            .map(|question| question.id)
            .collect(),
    );
    ask.release();
    ask.panel.set(false);
    crate::composer::focus();
}

/// Sends what is filled in, and closes the panel.
fn send() {
    let ask = page().ask;
    ask.release();
    let questions = open_questions();
    let Some((body, ids)) = ask
        .drafts
        .with_untracked(|drafts| answers_body(&questions, drafts))
    else {
        return;
    };
    ask.answering
        .update(|answering| answering.extend(ids.iter().cloned()));
    let sender = Sender::from_page();
    let opening = Some(ask.opening.get_untracked());
    spawn_local(async move {
        let outcome = send_to_arko("/api/questions/answer", body, &sender).await;
        if let Some(posted) = sender.take(outcome)
            && answered(ask, &ids, posted, opening)
        {
            close_panel();
        }
    });
}

/// "Not now" dismisses the questions the chip stands for, and the
/// agent goes on with its defaults.
fn not_now(ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let ask = page().ask;
    ask.release();
    ask.answering
        .update(|answering| answering.extend(ids.iter().cloned()));
    let sender = Sender::from_page();
    spawn_local(async move {
        let body = serde_json::json!({ "ids": ids.clone() });
        let outcome = send_to_arko("/api/questions/dismiss", body, &sender).await;
        if let Some(posted) = sender.take(outcome) {
            answered(ask, &ids, posted, None);
        }
    });
}

/// What became of answering or dismissing the questions `ids`, as Arko
/// said (`posted`), from the panel's opening `opening` when they were
/// answered in it. Taken, their drafts go, and it's true, so the panel
/// closes. Not taken, they can be answered again. Once the panel opened
/// again (for other questions, perhaps of the same IDs), it leaves the
/// questions as they are.
fn answered(ask: Asking, ids: &[String], posted: Posted, opening: Option<u64>) -> bool {
    if opening.is_some_and(|opening| ask.opening.get_untracked() != opening) {
        return false;
    }
    match posted {
        Posted::Taken => {
            ask.drafts
                .update(|drafts| drafts.retain(|id, _| !ids.contains(id)));
            true
        }
        Posted::Kept | Posted::Confirm(_) => {
            ask.answering
                .update(|answering| answering.retain(|id| !ids.contains(id)));
            false
        }
        Posted::Stale => false,
    }
}

fn update_draft(id: &str, change: impl FnOnce(&mut Draft)) {
    page()
        .ask
        .drafts
        .update(|drafts| change(drafts.entry(id.to_string()).or_default()));
}

// ----- views -----

/// The answer panel above the message box, with each open question with its
/// choices, "You decide" and an Other field, "Answer in chat" and Send.
#[component]
pub fn AnswerPanel() -> impl IntoView {
    let page = page();
    let ask = page.ask;
    let open = Memo::new(move |_| page.status.with(|status| status.questions.clone()));
    let can_send = move || {
        let questions = open.get();
        ask.drafts
            .with(|drafts| answers_body(&questions, drafts).is_some())
    };
    let keys = move |event: web_sys::KeyboardEvent| {
        // Esc puts the questions in the message box, to answer in a message.
        if event.key() == "Escape" {
            event.prevent_default();
            event.stop_propagation();
            answer_in_chat();
        }
    };
    // The panel's height as it's drawn, followed as it changes.
    let panel = NodeRef::<leptos::html::Section>::new();
    let observer = StoredValue::new_local(
        None::<(
            web_sys::ResizeObserver,
            wasm_bindgen::closure::Closure<dyn FnMut()>,
        )>,
    );
    Effect::new(move |_| {
        let section = panel.get();
        observer.update_value(|previous| {
            if let Some((resize, _)) = previous.take() {
                resize.disconnect();
            }
        });
        let Some(section) = section else {
            return;
        };
        let element: web_sys::Element = section.into();
        let drawn = element.clone();
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            if drawn.is_connected() {
                ask.drawn
                    .set(Some(drawn.get_bounding_client_rect().height()));
            }
        });
        if let Ok(resize) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
            resize.observe(&element);
            observer.set_value(Some((resize, callback)));
        }
    });
    on_cleanup(move || {
        observer.with_value(|observer| {
            if let Some((resize, _)) = observer {
                resize.disconnect();
            }
        })
    });
    // In the panel's place, the word on a question the reader was answering
    // that was settled elsewhere, as tall as the panel was.
    let note = move || {
        ask.note.get().map(|note| {
            view! {
                <section class="answers ended" role="status" style:height=move || ask.room.get().map(|room| format!("min({room}px, 60dvh)"))>
                    <div class="answers-head">
                        <p class="answers-title">{note}</p>
                        <button type="button" class="btn ghost icon small" aria-label="Close" title="Close" on:click=move |_| ask.release()>
                            <X size=16 />
                        </button>
                    </div>
                </section>
            }
        })
    };
    view! {
        <Show when=move || ask.panel.get() && !open.with(Vec::is_empty) fallback=note>
            <section node_ref=panel class="answers" style:min-height=move || ask.room.get().map(|room| format!("min({room}px, 60dvh)")) aria-label=move || open.with(|open| asks_text(open)) on:keydown=keys
                on:focusin=move |event| ask.focused.set(question_of(event.target()))
                on:focusout=move |event: web_sys::FocusEvent| {
                    if question_of(event.related_target()).is_none() {
                        ask.focused.set(None);
                    }
                }>
                <div class="answers-head">
                    <h2 class="answers-title">{move || open.with(|open| asks_text(open))}</h2>
                    <button type="button" class="btn ghost icon small" aria-label="Close" title="Close" on:click=move |_| close_panel()>
                        <X size=16 />
                    </button>
                </div>
                <div class="answers-list">
                    <For each=move || open.get() key=|question| question.id.clone() let:question>
                        <QuestionView question=question open=open />
                    </For>
                </div>
                <div class="answers-foot">
                    <button type="button" class="btn ghost small" title="Answer in chat (Esc)" on:click=move |_| answer_in_chat()>
                        "Answer in chat"
                    </button>
                    <button type="button" class="btn primary small" disabled=move || !can_send() on:click=move |_| send()>
                        "Send"
                    </button>
                </div>
            </section>
        </Show>
    }
}

/// One question in the panel. Its parts follow the question by its id, so
/// the keyboard stays on it when another one is settled elsewhere.
#[component]
fn QuestionView(question: QuestionData, open: Memo<Vec<QuestionData>>) -> impl IntoView {
    let ask = page().ask;
    let id = question.id.clone();
    let item = question.item;
    // Another ask's questions start below a line.
    let batch = {
        let id = id.clone();
        move || {
            open.with(|open| {
                let at = open
                    .iter()
                    .position(|question| question.id == id)
                    .unwrap_or(0);
                at > 0 && open[at - 1].item != item
            })
        }
    };
    let draft = {
        let id = id.clone();
        move || {
            ask.drafts
                .with(|drafts| drafts.get(&id).cloned().unwrap_or_default())
        }
    };
    // The numbers keep two digits' room, or the widest number's.
    let digits = move || {
        open.with(|open| {
            open.iter()
                .filter_map(|question| question.number)
                .max()
                .map_or(2, |last| last.to_string().len().max(2))
        })
    };
    let words = text(&question);
    let placeholder = if question.choices.is_empty() {
        "Answer"
    } else {
        "Other"
    };
    let chips = question
        .choices
        .iter()
        .map(|choice| {
            let (id, choice) = (id.clone(), choice.clone());
            let multiple = question.multiple;
            let pressed = {
                let draft = draft.clone();
                let choice = choice.clone();
                move || draft().choices.contains(&choice)
            };
            view! {
                <button type="button" class="chip" aria-pressed=move || pressed().to_string()
                    on:click=move |_| update_draft(&id, |draft| {
                        if let Some(at) = draft.choices.iter().position(|chosen| *chosen == choice) {
                            draft.choices.remove(at);
                        } else {
                            if !multiple {
                                draft.choices.clear();
                            }
                            draft.choices.push(choice.clone());
                            draft.you_decide = false;
                        }
                    })>
                    {choice.clone()}
                </button>
            }
        })
        .collect_view();
    let you_decide = {
        let id = id.clone();
        let default = question.default.clone();
        let spoken = match &default {
            Some(default) => format!("You decide, Arko goes with {default}"),
            None => "You decide".to_string(),
        };
        let pressed = {
            let draft = draft.clone();
            move || draft().you_decide
        };
        view! {
            <button type="button" class="chip" aria-label=spoken aria-pressed=move || pressed().to_string()
                on:click=move |_| update_draft(&id, |draft| {
                    draft.you_decide = !draft.you_decide;
                    if draft.you_decide {
                        draft.choices.clear();
                    }
                })>
                "You decide"
                {default.map(|default| view! { <span class="chip-default">{format!("· {default}")}</span> })}
            </button>
        }
    };
    let other_id = id.clone();
    let initial = draft().other;
    let label = format!("{}, other answer", words.lines().next().unwrap_or_default());
    view! {
        <div class="answers-question" class:batch=batch id=format!("question-{id}") data-question=id.clone() role="group" aria-label=words.clone()>
            <div class="answers-ask">
                {question.number.map(|number| view! {
                    <span class="answers-number" style:min-width=move || format!("{}ch", digits() + 1)>{format!("{number}.")}</span>
                })}
                <div class="answers-words">
                    // "Choose any" follows the question on its line.
                    <span>
                        {words.clone()}
                        {question.multiple.then(|| view! { " " <span class="answers-any">{CHOOSE_ANY}</span> })}
                    </span>
                </div>
            </div>
            <div class="answers-chips">{chips}{you_decide}</div>
            <input
                class="answers-other"
                id=format!("answer-{id}")
                placeholder=placeholder
                aria-label=label
                autocomplete="off"
                value=initial
                on:input=move |event| {
                    let value = event_target_value(&event);
                    update_draft(&other_id, |draft| draft.other = value);
                }
                on:keydown=move |event: web_sys::KeyboardEvent| {
                    if event.key() == "Enter" && !event.is_composing() {
                        event.prevent_default();
                        send();
                    }
                }
            />
        </div>
    }
}

/// Above the message box, for the open questions neither attached nor in
/// the panel, with "Arko asks 2 questions" when one is new, else a quiet "2 open
/// questions". It opens the panel, and "Not now" dismisses them.
#[component]
pub fn QuestionsChip() -> impl IntoView {
    let page = page();
    let ask = page.ask;
    let waiting = Memo::new(move |_| {
        if ask.panel.get() || ask.note.with(Option::is_some) {
            return Vec::new();
        }
        ask.attached.with(|attached| {
            page.status
                .with(|status| waiting(&status.questions, attached))
        })
    });
    let new = move || {
        ask.seen.with(|seen| {
            waiting.with(|waiting| waiting.iter().any(|question| !seen.contains(&question.id)))
        })
    };
    view! {
        <Show when=move || !waiting.with(Vec::is_empty)>
            <div class="qchip">
                <button type="button" class="btn ghost small qchip-open" class:new=new title="Answer (Alt+Q)" on:click=move |_| open_panel(None)>
                    <MessageCircleQuestionMark size=16 />
                    {move || waiting.with(|waiting| if new() { asks_text(waiting) } else { open_text(waiting.len()) })}
                </button>
                <button type="button" class="btn ghost small" title="Arko goes on without your answers"
                    on:click=move |_| not_now(waiting.get_untracked().into_iter().map(|question| question.id).collect())>
                    "Not now"
                </button>
            </div>
        </Show>
    }
}

/// "Replying to 2 questions ×" shows the questions the next message answers,
/// and the way to send it without answering them.
#[component]
pub fn Replying() -> impl IntoView {
    let ask = page().ask;
    view! {
        <Show when=move || !ask.attached.with(Vec::is_empty)>
            <div class="replying">
                <MessageCircleQuestionMark size=14 />
                <span>{move || ask.attached.with(|attached| replying_text(attached.len()))}</span>
                <button type="button" class="btn ghost icon small" aria-label="Don’t answer them" title="Don’t answer them"
                    on:click=move |_| ask.attached.set(Vec::new())>
                    <X size=14 />
                </button>
            </div>
        </Show>
    }
}

/// What a question card holds open for an answer, on the line its answer
/// takes once given.
pub const HELD: &str = "Needs your answer";

/// The font of the line under a question in its card (`.record-outcome` in
/// app.css), which it's cut to fit in.
const HELD_FONT: &str = "14px var(--sans)";

/// A question card in the transcript, its line ("Arko asked 2 questions")
/// and its questions boxed as the panel is. Each question has a line under
/// it that holds its answer, [`HELD`] while it's open, then the answer or
/// what else became of it ([`outcome`]), cut to fit, so a question settling
/// never changes the card's height. While a question is open, the card
/// opens the panel on the question clicked, or on its first open one, and
/// its line is the keyboard's way there. A settled question's cut line is
/// a button that shows it whole, or cut again. A card without its
/// questions (asked before they had ids) shows its text.
pub fn card_view(id: u64, cell: RowCell) -> AnyView {
    let data = cell.data;
    let legacy = Memo::new(move |_| data.with(|item| item.questions.is_none()));
    let ids = Memo::new(move |_| {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .map(|question| question.id.clone())
                .collect::<Vec<_>>()
        })
    });
    let asking = Memo::new(move |_| {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .any(|question| question.state == "open")
        })
    });
    // The settled questions whose line shows whole, by id.
    let whole = RwSignal::new(Vec::<String>::new());
    // A click on an open question opens the panel on it, and one on a
    // settled question's cut line shows it whole or cut again. Anywhere
    // else, the panel opens on the card while a question is open.
    let clicked = move |event: web_sys::MouseEvent| {
        let at = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("li[data-question]").ok().flatten());
        let question = at.as_ref().and_then(|at| at.get_attribute("data-question"));
        let state = question.as_ref().and_then(|question| {
            data.with_untracked(|item| {
                item.questions
                    .iter()
                    .flatten()
                    .find(|known| known.id == *question)
                    .map(|known| known.state.clone())
            })
        });
        let cut = at
            .as_ref()
            .and_then(|at| {
                at.query_selector(".record-outcome[aria-expanded]")
                    .ok()
                    .flatten()
            })
            .is_some();
        match (question, state.as_deref()) {
            (Some(question), Some("open")) => open_question(id, &question),
            (Some(question), Some(_)) if cut => flip(whole, &question),
            _ if asking.get_untracked() => open_panel(Some(id)),
            _ => {}
        }
    };
    (move || {
        if legacy.get() {
            return view! {
                <span class="label">"Arko asked"</span>
                <div class="md">{move || data.with(|item| item.text.clone())}</div>
            }
            .into_any();
        }
        view! {
            <button type="button" class="asked" disabled=move || !asking.get() title=move || if asking.get() { "Answer (Alt+Q)" } else { "" }
                on:click=move |_| open_panel(Some(id))>
                <MessageCircleQuestionMark size=16 />
                <span>{move || data.with(|item| item.text.clone())}</span>
            </button>
            <div class="record-box" on:click=clicked>
                <ol class="record">
                    <For each=move || ids.get() key=|id| id.clone() let:question>
                        <RecordQuestion data=data id=question whole=whole />
                    </For>
                </ol>
            </div>
        }
        .into_any()
    })
    .into_any()
}

/// `text` in `room` pixels of `font` ([`cut_to`]).
fn cut_line(font: &'static str, text: &str, room: f64) -> String {
    let units = |px: f64| (px * 100.0).round().max(0.0) as usize;
    cut_to(text, units(room), |text| {
        units(crate::fit::text_width(font, text))
    })
}

/// `text` in `room`, as `width` measures text, cut where a word ends
/// ([`quiet::cut_at_word`](crate::quiet::cut_at_word)) with an ellipsis
/// after it, or whole where it fits.
fn cut_to(text: &str, room: usize, width: impl Fn(&str) -> usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    if width(text) <= room {
        return text.to_string();
    }
    // (A line holds a few hundred clusters at most, so those after them
    // never show.)
    let clusters: Vec<(&str, usize)> = text
        .graphemes(true)
        .take(1000)
        .map(|cluster| (cluster, width(cluster)))
        .collect();
    // (A word longer than a row's line keeps is cut inside.)
    let long = crate::quiet::WORDS_KEEP * width("0");
    let mut left = room.saturating_sub(width("…"));
    loop {
        let kept = crate::quiet::cut_at_word(&clusters, left, long);
        let shown: String = clusters[..kept].iter().map(|(c, _)| *c).collect::<String>() + "…";
        let over = width(&shown).saturating_sub(room);
        if over == 0 || kept == 0 {
            return shown;
        }
        left = left.saturating_sub(over.max(1));
    }
}

/// Shows the line of the question `id` whole, or cut again.
fn flip(whole: RwSignal<Vec<String>>, id: &str) {
    whole.update(|whole| match whole.iter().position(|known| known == id) {
        Some(at) => {
            whole.remove(at);
        }
        None => whole.push(id.to_string()),
    });
}

/// One question of a card ([`card_view`]), its number when its card has
/// several, its words, and the line that holds its answer, cut to fit
/// unless it's among `whole`.
#[component]
fn RecordQuestion(
    data: RwSignal<crate::protocol::ItemData>,
    id: String,
    whole: RwSignal<Vec<String>>,
) -> impl IntoView {
    let question = Memo::new({
        let id = id.clone();
        move |_| {
            data.with(|item| {
                item.questions
                    .iter()
                    .flatten()
                    .find(|question| question.id == id)
                    .cloned()
            })
        }
    });
    let number = move || question.with(|question| question.as_ref().and_then(|q| q.number));
    // The numbers keep two digits' room, or the widest number's, so the
    // questions' words share a column past them.
    let digits = Memo::new(move |_| {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .filter_map(|question| question.number)
                .max()
                .map_or(2, |last| last.to_string().len().max(2))
        })
    });
    let words = move || question.with(|question| question.as_ref().map(text).unwrap_or_default());
    // What its line says, and whether that's the user's answer.
    let said = Memo::new(move |_| {
        question.with(|question| match question {
            None => (String::new(), false),
            Some(question) => match outcome(question) {
                None => (HELD.to_string(), false),
                Some(said) => (said, question.state == "answered"),
            },
        })
    });
    let open = Memo::new(move |_| {
        question.with(|question| question.as_ref().is_some_and(|q| q.state == "open"))
    });
    let shows_whole = Memo::new({
        let id = id.clone();
        move |_| !open.get() && whole.with(|whole| whole.contains(&id))
    });
    let line = NodeRef::<leptos::html::Span>::new();
    let room = crate::fit::watch(line, move || said.track());
    // On one line, cut where a word ends, as a row's line is. Its own line
    // breaks are spaces there.
    let one = Memo::new(move |_| {
        said.with(|(said, _)| said.split_whitespace().collect::<Vec<_>>().join(" "))
    });
    let shown = Memo::new(move |_| match room.get() {
        Some(room) => one.with(|one| cut_line(HELD_FONT, one, room.now)),
        None => one.get(),
    });
    let cut = Memo::new(move |_| {
        shown.with(|shown| one.with(|one| shown != one)) || said.with(|said| said.0.contains('\n'))
    });
    // (Only a settled line that's cut, or shows whole, is a button.)
    let button = Memo::new(move |_| !open.get() && (cut.get() || shows_whole.get()));
    // Enter and Space click it, as a button's do, so the transcript keeps
    // its line in place as it shows whole or cut, as for a click.
    let keys = move |event: web_sys::KeyboardEvent| {
        if button.get_untracked() && matches!(event.key().as_str(), "Enter" | " ") {
            event.prevent_default();
            if let Some(line) = line.get_untracked() {
                line.click();
            }
        }
    };
    view! {
        <li data-question=id.clone() class:open=move || open.get()>
            {move || number().map(|number| view! {
                <span class="record-number" style:min-width=move || format!("{}ch", digits.get() + 1)>{format!("{number}.")}</span>
            })}
            <div class="record-text">
                <div>{words}</div>
                <span node_ref=line class="record-outcome fitted" class:answer=move || said.with(|said| said.1) class:whole=move || shows_whole.get()
                    role=move || button.get().then_some("button")
                    tabindex=move || button.get().then_some("0")
                    aria-expanded=move || button.get().then(|| shows_whole.get().to_string())
                    title=move || (button.get() && !shows_whole.get()).then(|| said.with(|said| said.0.clone()))
                    on:keydown=keys>
                    {move || if shows_whole.get() { said.with(|said| said.0.clone()) } else { shown.get() }}
                </span>
            </div>
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(json: serde_json::Value) -> QuestionData {
        serde_json::from_value(json).unwrap()
    }

    /// An answer sent from one opening of the panel that comes once the
    /// panel opened again, for other questions perhaps of the same IDs,
    /// leaves the panel and its drafts as they are. In its own opening, taken,
    /// its drafts go and the panel closes, and not taken, the questions can
    /// be answered again.
    #[test]
    fn an_answer_settles_only_the_panels_opening_it_was_sent_from() {
        let owner = Owner::new();
        owner.with(|| {
            let ask = Asking::new();
            let q1 = vec!["q1".to_string()];
            let typed = Draft {
                other: "Blue".into(),
                ..Draft::default()
            };
            let fill = || {
                ask.drafts
                    .set(HashMap::from([("q1".into(), typed.clone())]));
                ask.answering.set(q1.clone());
            };
            fill();
            ask.open();
            let sent_from = Some(ask.opening.get_untracked());
            ask.panel.set(false);
            ask.open();
            assert!(!answered(ask, &q1, Posted::Taken, sent_from));
            assert!(!answered(ask, &q1, Posted::Kept, sent_from));
            assert_eq!(ask.drafts.get_untracked()["q1"], typed);
            assert_eq!(ask.answering.get_untracked(), q1);
            assert!(ask.panel.get_untracked());

            let now = Some(ask.opening.get_untracked());
            assert!(!answered(ask, &q1, Posted::Kept, now));
            assert!(ask.answering.get_untracked().is_empty());
            fill();
            assert!(answered(ask, &q1, Posted::Taken, now));
            assert!(ask.drafts.with_untracked(HashMap::is_empty));
            // A dismissal from a chip belongs to no opening.
            fill();
            assert!(!answered(ask, &q1, Posted::Kept, None));
            assert!(ask.answering.get_untracked().is_empty());
        });
    }

    /// A question's line is cut where a word ends, with an ellipsis, to fit
    /// its room, and shows whole where it fits.
    #[test]
    fn a_line_is_cut_where_a_word_ends() {
        let width = |text: &str| text.chars().count();
        assert_eq!(cut_to("Spaces", 10, width), "Spaces");
        let answer = "Spaces, four wide, and tabs in the Makefile";
        let cut = cut_to(answer, 24, width);
        assert_eq!(cut, "Spaces, four wide, and…");
        assert!(width(&cut) <= 24);
    }

    #[test]
    fn the_words_follow_the_count_and_what_is_asked() {
        let one = question(serde_json::json!({ "id": "q7", "question": "Tabs?", "state": "open" }));
        assert_eq!(
            asks_text(std::slice::from_ref(&one)),
            "Arko asks a question"
        );
        assert_eq!(
            asks_text(&[one.clone(), one.clone()]),
            "Arko asks 2 questions"
        );
        assert_eq!(text(&one), "Tabs?");
        assert_eq!(
            (open_text(1), replying_text(2)),
            ("1 open question".into(), "Replying to 2 questions".into())
        );
        assert_eq!(open_text(0), "0 open questions");
        assert_eq!(open_text(2), "2 open questions");
    }

    #[test]
    fn the_record_says_what_became_of_each_question() {
        let state = |json: serde_json::Value| outcome(&question(json));
        assert_eq!(
            state(serde_json::json!({ "id": "q", "question": "?", "state": "open" })),
            None
        );
        assert_eq!(state(serde_json::json!({ "id": "q", "question": "?", "state": "answered", "answer": "Spaces" })).as_deref(), Some("Spaces"));
        assert_eq!(
            state(
                serde_json::json!({ "id": "q", "question": "?", "state": "answered_in_message" })
            )
            .as_deref(),
            Some("Answered in your message")
        );
        assert_eq!(state(serde_json::json!({ "id": "q", "question": "?", "state": "closed", "why": "In the README" })).as_deref(), Some("Arko closed it. In the README"));
        assert_eq!(
            state(serde_json::json!({ "id": "q", "question": "?", "state": "dismissed" }))
                .as_deref(),
            Some("Dismissed")
        );
    }

    /// "Answer in chat" attaches the questions open then. One asked after
    /// it waits in the chip, even while the box is empty, rather than open
    /// the panel, which would bring the attached ones back beside it.
    #[test]
    fn a_question_asked_after_answer_in_chat_waits_in_the_chip() {
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let first = question(
            serde_json::json!({ "id": "q1", "question": "Tabs?", "state": "open", "item": 7 }),
        );
        let second = question(
            serde_json::json!({ "id": "q2", "question": "Push?", "state": "open", "item": 8 }),
        );
        let (open, seen, attached) = (ids(&["q1", "q2"]), ids(&["q1"]), ids(&["q1"]));
        assert!(!opens_by_itself(&open, &seen, &attached, false));
        let chip = waiting(&[first, second.clone()], &attached);
        assert_eq!(chip, [second]);
        assert_eq!(asks_text(&chip), "Arko asks a question");
        // Without questions attached, a new one opens the panel unless the
        // user types.
        assert!(opens_by_itself(&open, &seen, &[], false));
        assert!(!opens_by_itself(&open, &seen, &[], true));
        assert!(!opens_by_itself(&ids(&["q1"]), &seen, &[], false));
    }

    #[test]
    fn the_answers_sent_are_what_is_filled_in_by_question() {
        let questions = vec![
            question(
                serde_json::json!({ "id": "q3", "question": "Which?", "choices": ["A", "B", "C"], "multiple": true, "state": "open" }),
            ),
            question(serde_json::json!({ "id": "q4", "question": "Push?", "state": "open" })),
            question(serde_json::json!({ "id": "q5", "question": "Branch?", "state": "open" })),
            question(
                serde_json::json!({ "id": "q6", "question": "Tabs?", "choices": ["Tabs", "Spaces"], "state": "open" }),
            ),
        ];
        let mut drafts = HashMap::new();
        assert!(answers_body(&questions, &drafts).is_none());
        drafts.insert(
            "q3".to_string(),
            Draft {
                choices: vec!["C".into(), "A".into()],
                other: " 2 wide ".into(),
                ..Draft::default()
            },
        );
        // The words in Other go with "You decide", and with a question's
        // one choice.
        drafts.insert(
            "q4".to_string(),
            Draft {
                you_decide: true,
                other: "ask Bob first".into(),
                ..Draft::default()
            },
        );
        drafts.insert("q5".to_string(), Draft::default());
        drafts.insert(
            "q6".to_string(),
            Draft {
                choices: vec!["Spaces".into()],
                other: "4 wide".into(),
                ..Draft::default()
            },
        );
        let (body, ids) = answers_body(&questions, &drafts).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                // Every question shown is named, an unanswered one by its id.
                "answers": [
                    { "id": "q3", "choices": ["A", "C"], "text": "2 wide" },
                    { "id": "q4", "text": "ask Bob first", "you_decide": true },
                    { "id": "q5" },
                    { "id": "q6", "choices": ["Spaces"], "text": "4 wide" },
                ],
            })
        );
        assert_eq!(ids, ["q3", "q4", "q6"]);
    }
}
