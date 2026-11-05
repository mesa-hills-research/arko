//! The approval tray above the message box, with the requests that wait for the
//! user's approval, a row each, which a tap opens to its details. With
//! several, each row has a box, all checked at first. Approve approves the
//! rows checked and rejects the others, and Reject all rejects them all.
//! Include message starts off for text begun before the newest request.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{ChevronDown, ChevronRight};
use wasm_bindgen::JsCast;

use crate::app::{Posted, Sender, page, send as send_to_arko};
use crate::protocol::{
    Approval, ApprovalAnswers, ApprovalRequest, Look, SkillFile, answer_labels, approvals_heading,
};

use crate::protocol::{MessageSwitch, ShownApprovals};

/// What the tray remembers of its rows, by request.
#[derive(Clone, Copy)]
struct Rows {
    /// Those not checked. All start checked.
    unchecked: RwSignal<HashSet<String>>,
    /// Those whose option is on.
    options: RwSignal<HashSet<String>>,
    /// Those whose details show.
    open: RwSignal<HashSet<String>>,
    /// The file a skill's box shows, by its place among the skill's files.
    files: RwSignal<HashMap<String, usize>>,
    /// An answer is on its way.
    sending: RwSignal<bool>,
    message: RwSignal<MessageSwitch>,
    shown: RwSignal<ShownApprovals>,
    now: RwSignal<u64>,
}

impl Rows {
    fn new() -> Self {
        Self {
            unchecked: RwSignal::new(HashSet::new()),
            options: RwSignal::new(HashSet::new()),
            open: RwSignal::new(HashSet::new()),
            files: RwSignal::new(HashMap::new()),
            sending: RwSignal::new(false),
            message: RwSignal::new(MessageSwitch::default()),
            shown: RwSignal::new(ShownApprovals::default()),
            now: RwSignal::new(0),
        }
    }

    /// The rows of a new tray (each feed has its own), which starts `switch`
    /// over before any of its effects reads it.
    fn starting(message: RwSignal<MessageSwitch>) -> Self {
        message.set(MessageSwitch::default());
        Self {
            message,
            ..Self::new()
        }
    }

    fn requests_changed(self, ids: &HashSet<String>, has_text: bool) {
        self.shown.update(|shown| shown.retain(ids));
        self.message
            .update(|message| message.requests(ids.clone(), has_text));
    }

    fn includes_message(self) -> bool {
        self.message.with(MessageSwitch::includes)
    }

    fn flip_message(self) {
        self.message.update(MessageSwitch::flip);
    }

    fn answers(
        self,
        requests: &[ApprovalRequest],
        approve: bool,
        words: &str,
        now_ms: u64,
    ) -> ApprovalAnswers {
        let requests: Vec<_> = requests
            .iter()
            .filter(|request| {
                self.shown
                    .with_untracked(|shown| shown.can_answer(&request.id, now_ms))
            })
            .cloned()
            .collect();
        self.message
            .update(|message| message.edited(has_message(words)));
        let words = if !requests.is_empty() && self.includes_message() {
            words
        } else {
            ""
        };
        self.unchecked.with_untracked(|unchecked| {
            self.options.with_untracked(|options| {
                ApprovalAnswers::of(
                    &requests,
                    |id| approve && !unchecked.contains(id),
                    |id| options.contains(id),
                    words,
                )
            })
        })
    }
}

fn now_ms() -> u64 {
    window()
        .performance()
        .map(|clock| clock.now() as u64)
        .unwrap_or(0)
}

/// Only rows within the list and viewport have been shown to the reader.
fn rendered_rows(list: &web_sys::Element, rows: Rows) {
    let now = now_ms();
    rows.now.set(now);
    if document().visibility_state() != web_sys::VisibilityState::Visible {
        rows.shown
            .update(|shown| shown.visible(&HashSet::new(), now));
        return;
    }
    let bounds = list.get_bounding_client_rect();
    let viewport = window()
        .inner_height()
        .ok()
        .and_then(|height| height.as_f64())
        .unwrap_or(0.0);
    let children = list.children();
    let mut visible = HashSet::new();
    for at in 0..children.length() {
        if let Some(row) = children.item(at)
            && let Some(id) = row.get_attribute("data-request")
        {
            let rect = row
                .first_element_child()
                .unwrap_or(row)
                .get_bounding_client_rect();
            if rect.top() >= bounds.top().max(0.0) - 1.0
                && rect.bottom() <= bounds.bottom().min(viewport) + 1.0
                && rect.width() > 0.0
                && rect.height() > 0.0
            {
                visible.insert(id);
            }
        }
    }
    rows.shown.update(|shown| shown.visible(&visible, now));
}

fn has_message(text: &str) -> bool {
    !text.trim().is_empty()
}

/// The message box, whose text can go with the answer.
fn message_box() -> Option<web_sys::HtmlTextAreaElement> {
    document()
        .query_selector(".compose textarea")
        .ok()
        .flatten()?
        .dyn_into()
        .ok()
}

/// Splits `text` at its backticks, so the even parts are words, the odd ones
/// code.
pub fn code_spans(text: &str) -> Vec<(bool, String)> {
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(at, part)| (at % 2 == 1, part.to_string()))
        .collect()
}

/// A line of words, its `code` as code.
fn words_view(text: &str) -> impl IntoView + use<> {
    code_spans(text)
        .into_iter()
        .map(|(code, part)| {
            if code {
                view! { <code>{part}</code> }.into_any()
            } else {
                part.into_any()
            }
        })
        .collect_view()
}

/// Sends the answer to `requests`. When `approve`, those checked are
/// approved and the others rejected, else all are rejected. The message
/// box's included words go with it. Once Arko has them, the box empties
/// unless the user wrote something else there meanwhile.
fn answer(requests: Vec<ApprovalRequest>, approve: bool, rows: Rows) {
    if let Ok(Some(list)) = document().query_selector(".tray-list") {
        rendered_rows(&list, rows);
    }
    let area = message_box();
    let words = area.as_ref().map(|area| area.value()).unwrap_or_default();
    let answers = rows.answers(&requests, approve, &words, now_ms());
    if answers.choices.is_empty() {
        return;
    }
    rows.sending.set(true);
    // (Its words are the message box's, of the view it's sent from.)
    let sender = Sender::from_view();
    spawn_local(async move {
        let body = serde_json::to_value(&answers).unwrap_or_default();
        let outcome = send_to_arko("/api/approvals", body, &sender).await;
        let taken = sender.take(outcome) == Some(Posted::Taken);
        if let Some(area) = area
            && clears_message(taken, &answers, &words, &area.value())
        {
            area.set_value("");
            // The box fits itself again and forgets its draft.
            if let Ok(event) = web_sys::Event::new("input") {
                let _ = area.dispatch_event(&event);
            }
        }
        // (A new feed's tray took the place of this one meanwhile.)
        rows.sending.try_set(false);
    });
}

/// An included message clears only once taken, if the box still holds it.
fn clears_message(taken: bool, answers: &ApprovalAnswers, sent: &str, now: &str) -> bool {
    taken && answers.words.is_some() && sent == now
}

/// Adds `id` to `set`, or takes it out.
fn flip(set: RwSignal<HashSet<String>>, id: &str) {
    set.update(|set| {
        if !set.remove(id) {
            set.insert(id.to_string());
        }
    });
}

/// The tray, while requests wait. `switch` says whether an answer takes the
/// box's words (`draft`), which the box keeps across a send that comes back.
#[component]
pub fn Tray(draft: RwSignal<String>, switch: RwSignal<MessageSwitch>) -> impl IntoView {
    let page = page();
    let requests = Memo::new(move |_| page.status.with(|status| status.approvals.clone()));
    let rows = Rows::starting(switch);
    let list: NodeRef<leptos::html::Ul> = NodeRef::new();
    let observer = StoredValue::new_local(
        None::<(
            web_sys::ResizeObserver,
            wasm_bindgen::closure::Closure<dyn FnMut()>,
        )>,
    );
    Effect::new(move |_| {
        let list = list.get();
        observer.update_value(|previous| {
            if let Some((resize, _)) = previous.take() {
                resize.disconnect();
            }
        });
        let Some(list) = list else {
            return;
        };
        let element: web_sys::Element = list.into();
        let resized = element.clone();
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            rendered_rows(&resized, rows)
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
    let visibility = leptos::leptos_dom::helpers::window_event_listener(
        leptos::ev::visibilitychange,
        move |_| {
            if let Some(list) = list.get_untracked() {
                rendered_rows(&list, rows);
            }
        },
    );
    on_cleanup(move || visibility.remove());
    // What was set for requests no longer waiting goes.
    Effect::new(move |_| {
        let ids: HashSet<String> =
            requests.with(|requests| requests.iter().map(|request| request.id.clone()).collect());
        rows.requests_changed(&ids, draft.with_untracked(|text| has_message(text)));
        for set in [rows.unchecked, rows.options, rows.open] {
            if set.with_untracked(|set| set.iter().any(|id| !ids.contains(id))) {
                set.update(|set| set.retain(|id| ids.contains(id)));
            }
        }
        if rows
            .files
            .with_untracked(|files| files.keys().any(|id| !ids.contains(id)))
        {
            rows.files
                .update(|files| files.retain(|id, _| ids.contains(id)));
        }
    });
    Effect::new(move |_| {
        let has_text = draft.with(|text| has_message(text));
        rows.message.update(|message| message.edited(has_text));
    });
    let timeout = StoredValue::new(None::<leptos::leptos_dom::helpers::TimeoutHandle>);
    Effect::new(move |_| {
        let now = rows.now.get();
        let ready_at = rows.shown.with(|shown| shown.ready_at_ms(now));
        if let Some(previous) = timeout.get_value() {
            previous.clear();
        }
        timeout.set_value(ready_at.and_then(|ready| {
            leptos::leptos_dom::helpers::set_timeout_with_handle(
                move || {
                    rows.now.try_set(now_ms());
                },
                std::time::Duration::from_millis(ready.saturating_sub(now_ms()).max(1)),
            )
            .ok()
        }));
    });
    on_cleanup(move || {
        if let Some(timeout) = timeout.get_value() {
            timeout.clear();
        }
    });
    let count = move || requests.with(Vec::len);
    let waiting = move || count() != 0;
    let several = Signal::derive(move || count() > 1);
    let ready = Memo::new(move |_| {
        let now = rows.now.get();
        requests.with(|requests| {
            rows.shown.with(|shown| {
                requests
                    .iter()
                    .filter(|request| shown.can_answer(&request.id, now))
                    .cloned()
                    .collect::<Vec<_>>()
            })
        })
    });
    let labels = Memo::new(move |_| {
        ready.with(|ready| {
            rows.unchecked.with(|unchecked| {
                let checked = ready
                    .iter()
                    .filter(|request| !unchecked.contains(&request.id))
                    .count();
                answer_labels(checked, ready.len(), count())
            })
        })
    });
    let disabled = move || rows.sending.get() || ready.with(Vec::is_empty);
    view! {
        <Show when=waiting>
            <section class="tray" aria-label=move || approvals_heading(count())>
                <h2 class="tray-title">{move || approvals_heading(count())}</h2>
                <ul class="tray-list" node_ref=list on:scroll=move |_| {
                    if let Some(list) = list.get_untracked() {
                        rendered_rows(&list, rows);
                    }
                }>
                    <For each=move || requests.get() key=|request| request.id.clone() let:request>
                        <Row request=request rows=rows several=several />
                    </For>
                </ul>
                <div class="tray-foot">
                    <Show when=move || draft.with(|text| has_message(text))>
                        <label class="tray-message">
                            "Include message"
                            <button type="button" class="switch" role="switch"
                                aria-checked=move || rows.includes_message().to_string()
                                disabled=move || rows.sending.get()
                                on:click=move |_| rows.flip_message()>
                                <span></span>
                            </button>
                        </label>
                    </Show>
                    <button type="button" class="btn outline small" disabled=disabled
                        on:click=move |_| answer(requests.get_untracked(), false, rows)>
                        {move || labels.with(|labels| labels.1.clone())}
                    </button>
                    <button type="button" class="btn primary small" disabled=disabled
                        on:click=move |_| answer(requests.get_untracked(), true, rows)>
                        {move || labels.with(|labels| labels.0.clone())}
                    </button>
                </div>
            </section>
        </Show>
    }
}

/// A request's row, with its box (with several), and its kind, name, scope and
/// summary, which open its details.
#[component]
fn Row(request: ApprovalRequest, rows: Rows, several: Signal<bool>) -> impl IntoView {
    let row: NodeRef<leptos::html::Li> = NodeRef::new();
    row.on_load(move |row| {
        let drawn = wasm_bindgen::closure::Closure::once_into_js(move || {
            if let Some(list) = row.parent_element() {
                rendered_rows(&list, rows);
            }
        });
        let _ = window().request_animation_frame(drawn.unchecked_ref());
    });
    let id = request.id.clone();
    let approval = request.approval.clone();
    let open = {
        let id = id.clone();
        move || rows.open.with(|open| open.contains(&id))
    };
    let checked = {
        let id = id.clone();
        move || rows.unchecked.with(|unchecked| !unchecked.contains(&id))
    };
    let check = {
        let id = id.clone();
        let name = approval.name().to_string();
        move || {
            let id = id.clone();
            let checked = checked.clone();
            view! {
                <input type="checkbox" class="tray-check" aria-label=format!("Approve {name}")
                    prop:checked=checked on:change=move |_| flip(rows.unchecked, &id) />
            }
        }
    };
    let toggle = {
        let id = id.clone();
        move |_| flip(rows.open, &id)
    };
    let expanded = open.clone();
    let chevron = open.clone();
    let opened = open.clone();
    view! {
        <li class="tray-row" class:open=opened node_ref=row data-request=id>
            <div class="tray-line">
                <Show when=move || several.get()>{check.clone()}</Show>
                <button type="button" class="tray-summary" aria-expanded=move || expanded().to_string() on:click=toggle>
                    <span class="tray-kind">{approval.kind_word()}</span>
                    <span class="tray-name">{approval.name().to_string()}</span>
                    <span class="tray-scope" class:every=approval.every_project()>{approval.scope().to_string()}</span>
                    <span class="tray-text">{approval.summary()}</span>
                    <span class="tray-chevron">
                        {move || if chevron() {
                            view! { <ChevronDown size=16 /> }.into_any()
                        } else {
                            view! { <ChevronRight size=16 /> }.into_any()
                        }}
                    </span>
                </button>
            </div>
            <Show when=open>
                {
                    let request = request.clone();
                    move || view! { <Details request=request.clone() rows=rows /> }
                }
            </Show>
        </li>
    }
}

/// A request's details, as its row opens to them, namely what it asks, its lines,
/// a skill's files with the one chosen in a box, and its option.
#[component]
fn Details(request: ApprovalRequest, rows: Rows) -> impl IntoView {
    let id = request.id.clone();
    let approval = request.approval;
    let files: Option<Vec<SkillFile>> = match &approval {
        Approval::Skill(skill) => Some(skill.files.clone()),
        _ => None,
    };
    // The box of a skill's SKILL.md, which the files below show. (An
    // update's changes have a box of their own.)
    let main_text: Option<String> = files
        .as_ref()
        .and_then(|files| files.first())
        .and_then(|file| file.text.as_deref())
        .map(|text| text.trim_end().to_string());
    let lines = approval
        .details()
        .into_iter()
        .filter_map(|detail| match detail.look {
            Look::Text => Some(view! { <p>{words_view(&detail.text)}</p> }.into_any()),
            Look::Heading => Some(view! { <h3 class="tray-heading">{detail.text}</h3> }.into_any()),
            Look::Warning => Some(view! { <p class="tray-warning">{detail.text}</p> }.into_any()),
            // A skill's files show below, with the one chosen in the box.
            Look::Box if main_text.as_deref() == Some(detail.text.as_str()) => None,
            Look::File if files.is_some() => None,
            Look::Box => Some(view! { <div class="tray-box">{detail.text}</div> }.into_any()),
            Look::File => Some(view! { <p>{detail.text}</p> }.into_any()),
        })
        .collect_view();
    let files = files.filter(|files| !files.is_empty()).map(|files| {
        let shown = {
            let id = id.clone();
            move || rows.files.with(|files| files.get(&id).copied().unwrap_or(0))
        };
        let buttons = files
            .iter()
            .enumerate()
            .map(|(at, file)| {
                let id = id.clone();
                let shown = shown.clone();
                view! {
                    <button type="button" class="chip tray-file" aria-pressed=move || (shown() == at).to_string()
                        on:click=move |_| rows.files.update(|files| { files.insert(id.clone(), at); })>
                        {file.path.clone()}
                    </button>
                }
            })
            .collect_view();
        let text = move || {
            files
                .get(shown())
                .map(|file| match &file.text {
                    Some(text) => text.trim_end().to_string(),
                    None => format!("{} has no text to show.", file.path),
                })
                .unwrap_or_default()
        };
        view! {
            <div class="tray-files">{buttons}</div>
            <div class="tray-box">{text}</div>
        }
    });
    let option = approval.option().map(|option| {
        let on = {
            let id = id.clone();
            move || rows.options.with(|options| options.contains(&id))
        };
        let id = id.clone();
        view! {
            <label class="tray-option">
                <input type="checkbox" prop:checked=on on:change=move |_| flip(rows.options, &id) />
                {option}
            </label>
        }
    });
    view! {
        <div class="tray-details">
            <p class="tray-question">{approval.title()}</p>
            {lines}
            {files}
            {option}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests(ids: &[&str]) -> Vec<ApprovalRequest> {
        ids.iter()
            .map(|id| ApprovalRequest {
                id: (*id).into(),
                batch: 1,
                approval: Approval::WorkerBilling(crate::protocol::BillingApproval {
                    model: "Opus".into(),
                    task: "Review".into(),
                    ..Default::default()
                }),
            })
            .collect()
    }

    fn follow(rows: Rows, requests: &[ApprovalRequest], has_text: bool) {
        rows.requests_changed(
            &requests.iter().map(|request| request.id.clone()).collect(),
            has_text,
        );
        rows.shown.update(|shown| {
            shown.visible(
                &requests.iter().map(|request| request.id.clone()).collect(),
                1000,
            )
        });
    }

    /// A new feed's tray starts the box's switch over before it follows the
    /// requests, so a message typed after them goes with the answer.
    #[test]
    fn a_new_tray_starts_the_switch_over() {
        Owner::new().with(|| {
            let switch = RwSignal::new(MessageSwitch::default());
            switch.update(|switch| {
                switch.requests(HashSet::from(["a1".into()]), true);
                switch.flip();
            });
            assert!(switch.with_untracked(MessageSwitch::includes));
            let rows = Rows::starting(switch);
            assert_eq!(switch.get_untracked(), MessageSwitch::default());
            follow(rows, &requests(&["b1"]), false);
            rows.message.update(|message| message.edited(true));
            assert!(rows.includes_message());
        });
    }

    #[test]
    fn both_answers_wait_for_rendering_and_leave_newer_requests_waiting() {
        Owner::new().with(|| {
            for approve in [false, true] {
                let rows = Rows::new();
                let pending = requests(&["a1"]);
                rows.requests_changed(&HashSet::from(["a1".into()]), false);
                let answers = rows.answers(&pending, approve, "Reason", 1000);
                assert!(answers.choices.is_empty());
                assert_eq!(answers.words, None);
                rows.shown
                    .update(|shown| shown.visible(&HashSet::from(["a1".into()]), 1000));
                assert!(
                    rows.answers(&pending, approve, "Reason", 1499)
                        .choices
                        .is_empty()
                );
                let pending = requests(&["a1", "a2"]);
                rows.requests_changed(&HashSet::from(["a1".into(), "a2".into()]), true);
                rows.shown.update(|shown| {
                    shown.visible(&HashSet::from(["a1".into(), "a2".into()]), 1500)
                });
                let answers = rows.answers(&pending, approve, "Reason", 1500);
                assert_eq!(answers.choices.len(), 1);
                assert_eq!(answers.choices[0].id, "a1");
                assert_eq!(answers.choices[0].approve, approve);
                assert_eq!(answers.words, None);
                rows.requests_changed(&HashSet::from(["a2".into()]), true);
                let answers = rows.answers(&requests(&["a2"]), approve, "Reason", 2000);
                assert_eq!(answers.choices.len(), 1);
                assert_eq!(answers.choices[0].id, "a2");
            }
        });
    }

    #[test]
    fn pre_existing_words_stay_for_both_answers() {
        Owner::new().with(|| {
            for approve in [false, true] {
                let rows = Rows::new();
                let requests = requests(&["a1"]);
                let text = "Half-written message";
                follow(rows, &requests, true);
                assert!(!rows.includes_message());
                let answers = rows.answers(&requests, approve, text, 1500);
                assert_eq!(answers.words, None);
                assert_eq!(answers.choices[0].approve, approve);
                assert!(
                    serde_json::to_value(&answers)
                        .unwrap()
                        .get("words")
                        .is_none()
                );
                assert!(!clears_message(true, &answers, text, text));
                rows.flip_message();
                let answers = rows.answers(&requests, approve, text, 1500);
                assert_eq!(answers.words.as_deref(), Some(text));
                assert!(clears_message(true, &answers, text, text));
            }
        });
    }

    #[test]
    fn later_typing_includes_words_and_the_switch_flips_both_answers() {
        Owner::new().with(|| {
            for approve in [false, true] {
                for flips in 0..=2 {
                    let rows = Rows::new();
                    let requests = requests(&["a1"]);
                    follow(rows, &requests, false);
                    rows.message.update(|message| message.edited(true));
                    assert!(rows.includes_message());
                    for _ in 0..flips {
                        rows.flip_message();
                    }
                    let include = flips != 1;
                    let answers = rows.answers(&requests, approve, " Reason ", 1500);
                    assert_eq!(answers.words.as_deref(), include.then_some("Reason"));
                    assert_eq!(answers.choices[0].approve, approve);
                    assert_eq!(
                        clears_message(true, &answers, " Reason ", " Reason "),
                        include
                    );
                    assert!(!clears_message(false, &answers, " Reason ", " Reason "));
                    assert!(!clears_message(true, &answers, " Reason ", "Another draft"));
                }
            }
        });
    }

    #[test]
    fn an_empty_box_has_no_switch_or_words() {
        Owner::new().with(|| {
            let rows = Rows::new();
            let requests = requests(&["a1"]);
            follow(rows, &requests, false);
            for text in ["", " ", "\n"] {
                assert!(!has_message(text));
                let answers = rows.answers(&requests, true, text, 1500);
                assert!(answers.words.is_none());
                assert!(!clears_message(true, &answers, text, text));
            }
            assert!(has_message("Reason"));
        });
    }

    #[test]
    fn backticks_mark_code() {
        assert_eq!(
            code_spans("Runs `npx -y server`."),
            [
                (false, "Runs ".to_string()),
                (true, "npx -y server".to_string()),
                (false, ".".to_string())
            ]
        );
        assert_eq!(code_spans("Plain."), [(false, "Plain.".to_string())]);
    }

    /// The page reads the requests in the status as the server writes
    /// them, and its answer as the server reads it.
    #[test]
    fn the_status_brings_the_requests() {
        let status: crate::protocol::StatusData = serde_json::from_value(serde_json::json!({
            "working": true,
            "approvals": [{
                "id": "a1", "batch": 1,
                "approval": {"kind": "mcp_server", "name": "github", "scope": "project",
                             "command": "npx -y @modelcontextprotocol/server-github",
                             "env": [{"name": "GITHUB_TOKEN", "set": false}],
                             "why": "Read the issues."}
            }, {
                "id": "a2", "batch": 1,
                "approval": {"kind": "worker_billing", "model": "Opus 5.5", "task": "Review"}
            }]
        }))
        .unwrap();
        assert_eq!(status.approvals.len(), 2);
        assert_eq!(
            status.approvals[0].approval.title(),
            "Add the MCP server github for this project?"
        );
        let answers = ApprovalAnswers::of(&status.approvals, |id| id == "a2", |_| true, " ok ");
        assert_eq!(
            serde_json::to_value(&answers).unwrap(),
            serde_json::json!({
                "choices": [
                    {"id": "a1", "approve": false},
                    {"id": "a2", "approve": true, "option": true}
                ],
                "words": "ok"
            })
        );
    }
}
