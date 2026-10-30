//! The agent's questions: the answer panel above the message box, the chips
//! that stand for questions not in it, and a question card's line and
//! record in the transcript. A question shows in one place at a time: in
//! the panel while it's open, then in its card's record. The user never
//! sees a question's id, only its number in its ask.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{MessageCircleQuestionMark, X};
use wasm_bindgen::JsCast;

use crate::app::{Cell, page, post};
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
    /// A word on the questions, above the message box for a moment.
    pub note: RwSignal<Option<String>>,
}

impl Asking {
    pub fn new() -> Self {
        Self {
            panel: RwSignal::new(false),
            attached: RwSignal::new(Vec::new()),
            seen: RwSignal::new(Vec::new()),
            drafts: RwSignal::new(HashMap::new()),
            typed_at: RwSignal::new(0.0),
            box_empty: RwSignal::new(true),
            focused: RwSignal::new(None),
            answering: RwSignal::new(Vec::new()),
            note: RwSignal::new(None),
        }
    }
}

// ----- words -----

/// "Arko asks 2 questions": the panel's title, and the chip of new ones.
pub fn asks_text(questions: &[QuestionData]) -> String {
    match questions.len() {
        1 => "Arko asks a question".to_string(),
        count => format!("Arko asks {count} questions"),
    }
}

/// "2 open questions": the quiet chip of the questions not attached.
pub fn open_text(count: usize) -> String {
    match count {
        1 => "1 open question".to_string(),
        count => format!("{count} open questions"),
    }
}

/// "Replying to 2 questions": the questions the next message answers.
pub fn replying_text(count: usize) -> String {
    match count {
        1 => "Replying to a question".to_string(),
        count => format!("Replying to {count} questions"),
    }
}

/// What became of a question, as its card's record says it: the answer,
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

// ----- what the panel does -----

fn open_questions() -> Vec<QuestionData> {
    page()
        .status
        .with_untracked(|status| status.questions.clone())
}

/// Follows the open questions: what's filled in and attached stays only
/// for those still open, the panel closes when none is, and opens by itself
/// for new ones while the user isn't typing.
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
        // screen, or the agent): the panel closes, rather than move the
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
            ask.focused.set(None);
            ask.panel.set(false);
            say("That question is no longer open.");
            crate::composer::focus();
            return;
        }
        if open.is_empty() {
            ask.panel.set(false);
            return;
        }
        let unseen = ask
            .seen
            .with_untracked(|seen| open.iter().any(|id| !seen.contains(id)));
        let typing = !ask.box_empty.get_untracked()
            || js_sys::Date::now() - ask.typed_at.get_untracked() < TYPING_REST;
        if unseen && !typing && !ask.panel.get_untracked() {
            ask.panel.set(true);
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
    let page = page();
    let open = open_questions();
    if open.is_empty() {
        return;
    }
    page.sheet.set(None);
    page.ask.panel.set(true);
    let target = card
        .and_then(|card| open.iter().find(|question| question.item == card))
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

/// Says `words` above the message box for a few seconds.
fn say(words: &str) {
    let note = page().ask.note;
    note.set(Some(words.to_string()));
    let words = words.to_string();
    spawn_local(async move {
        crate::composer::gloo_wait(5000).await;
        if note.get_untracked().as_deref() == Some(words.as_str()) {
            note.set(None);
        }
    });
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
    ask.panel.set(false);
    crate::composer::focus();
}

/// Sends what is filled in, and closes the panel.
fn send() {
    let ask = page().ask;
    let questions = open_questions();
    let Some((body, ids)) = ask
        .drafts
        .with_untracked(|drafts| answers_body(&questions, drafts))
    else {
        return;
    };
    ask.answering
        .update(|answering| answering.extend(ids.iter().cloned()));
    spawn_local(async move {
        // What's typed stays until Arko has it, and the panel with it.
        if post("/api/questions/answer", body).await {
            ask.drafts
                .update(|drafts| drafts.retain(|id, _| !ids.contains(id)));
            close_panel();
        } else {
            ask.answering
                .update(|answering| answering.retain(|id| !ids.contains(id)));
        }
    });
}

/// "Not now": the questions the chip stands for are dismissed, and the
/// agent goes on with its defaults.
fn not_now(ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let ask = page().ask;
    ask.answering
        .update(|answering| answering.extend(ids.iter().cloned()));
    spawn_local(async move {
        if !post(
            "/api/questions/dismiss",
            serde_json::json!({ "ids": ids.clone() }),
        )
        .await
        {
            ask.answering
                .update(|answering| answering.retain(|id| !ids.contains(id)));
        }
    });
}

fn update_draft(id: &str, change: impl FnOnce(&mut Draft)) {
    page()
        .ask
        .drafts
        .update(|drafts| change(drafts.entry(id.to_string()).or_default()));
}

// ----- views -----

/// The answer panel above the message box: each open question with its
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
    view! {
        <Show when=move || ask.panel.get() && !open.with(Vec::is_empty)>
            <section class="answers" aria-label=move || open.with(|open| asks_text(open)) on:keydown=keys
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
                {question.number.map(|number| view! { <span class="answers-number">{format!("{number}.")}</span> })}
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
/// the panel: "Arko asks 2 questions" when one is new, else a quiet "2 open
/// questions". It opens the panel, and "Not now" dismisses them.
#[component]
pub fn QuestionsChip() -> impl IntoView {
    let page = page();
    let ask = page.ask;
    let waiting = Memo::new(move |_| {
        if ask.panel.get() {
            return Vec::new();
        }
        let attached = ask.attached.get();
        page.status.with(|status| {
            status
                .questions
                .iter()
                .filter(|question| !attached.contains(&question.id))
                .cloned()
                .collect::<Vec<_>>()
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

/// "Replying to 2 questions ×": the questions the next message answers,
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

/// A question card in the transcript. While all its questions are open, one
/// quiet line where the agent asked ("Arko asked 2 questions") opens the
/// panel on the card. Once any is settled, the card is their record, boxed
/// as the panel is, with each settled question and what became of it (the
/// open ones are in the panel). A card without its questions (asked before
/// they had ids) shows its text.
pub fn card_view(id: u64, cell: Cell) -> AnyView {
    let data = cell.data;
    // (A card comes first with its line, and its questions follow.)
    let legacy = Memo::new(move |_| data.with(|item| item.questions.is_none()));
    let recorded = Memo::new(move |_| {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .any(|question| question.state != "open")
        })
    });
    let open = move || {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .any(|question| question.state == "open")
        })
    };
    let settled = move || {
        data.with(|item| {
            item.questions
                .iter()
                .flatten()
                .filter(|question| question.state != "open")
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    (move || {
        if legacy.get() {
            return view! {
                <span class="label">"Arko asked"</span>
                <div class="md">{move || data.with(|item| item.text.clone())}</div>
            }
            .into_any();
        }
        if !recorded.get() {
            return view! {
                <button type="button" class="asked" disabled=move || !open() title=move || if open() { "Answer (Alt+Q)" } else { "" }
                    on:click=move |_| open_panel(Some(id))>
                    <MessageCircleQuestionMark size=16 />
                    <span>{move || data.with(|item| item.text.clone())}</span>
                </button>
            }
            .into_any();
        }
        view! {
            <div class="record-box">
                <h3 class="record-title">{move || data.with(|item| item.text.clone())}</h3>
                <ol class="record">
                    {move || settled().into_iter().map(|question| {
                        let outcome = outcome(&question).unwrap_or_default();
                        let answered = question.state == "answered";
                        view! {
                            <li>
                                <span class="record-number">{question.number.map(|number| format!("{number}."))}</span>
                                <div>
                                    <div>{text(&question)}</div>
                                    <div class="record-outcome" class:answer=answered>{outcome}</div>
                                </div>
                            </li>
                        }
                    }).collect_view()}
                </ol>
            </div>
        }
        .into_any()
    })
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(json: serde_json::Value) -> QuestionData {
        serde_json::from_value(json).unwrap()
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
