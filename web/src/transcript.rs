//! The transcript: its rows, laid out for the view, and Latest.

use leptos::prelude::*;
use lucide_leptos::{ArrowDown, ChevronRight, LoaderCircle};

use crate::app::{Cell, Page, STORE, cell, page, post};
use crate::layout::{self, Count, Row, View};
use crate::markdown::{self, Blocks};
use crate::protocol::{CallStatus, ItemData, Kind, Level, Source, WorkerState};
use crate::store::{MAIN, call_actions};
use crate::{scroll, words};

#[component]
pub fn Transcript() -> impl IntoView {
    let page = page();
    let log: NodeRef<leptos::html::Main> = NodeRef::new();
    log.on_load(move |log| scroll::attach(log.into(), page.pinned));
    // The rows of the view, without "Working…".
    let laid_out = Memo::new(move |_| {
        page.version.track();
        let view = page.view.get();
        let worker = page.worker.get();
        STORE.with(|store| layout::layout(&store.borrow().items, view, worker.as_deref()))
    });
    // What shows the agent is under way.
    let motion = Memo::new(move |_| {
        page.version.track();
        page.workers.track();
        page.now.track();
        let view = page.view.get();
        let worker = page.worker.get();
        let live = page.connection.get() == words::Connection::Live;
        if !live {
            return (None, false);
        }
        page.status.with(|status| {
            STORE.with(|store| {
                layout::motion(
                    &store.borrow(),
                    status,
                    view,
                    worker.as_deref(),
                    js_sys::Date::now(),
                )
            })
        })
    });
    let rows = Memo::new(move |_| {
        let working = motion.get().1;
        STORE.with(|store| layout::with_working(laid_out.get(), working, &store.borrow()))
    });
    let thinking = Memo::new(move |_| motion.get().0);
    // The newest item the reader saw, while at the bottom.
    Effect::new(move |_| {
        rows.track();
        if page.pinned.get() {
            page.seen.set(STORE.with(|store| store.borrow().newest()));
            page.grew.set(false);
        }
    });
    let jump_label = move || {
        let fresh = layout::unseen(&rows.get(), page.seen.get());
        if fresh > 0 {
            format!("{fresh} new")
        } else if page.grew.get() {
            "New text".into()
        } else {
            "Latest".into()
        }
    };
    let empty = move || {
        if !rows.with(Vec::is_empty) || page.connection.get() != words::Connection::Live {
            return None;
        }
        Some(match page.worker.get() {
            None => "Nothing here yet.".to_string(),
            Some(id) => {
                page.workers.track();
                match STORE.with(|store| {
                    store
                        .borrow()
                        .workers
                        .get(&id)
                        .and_then(|w| w.report.clone())
                }) {
                    Some(report) => format!(
                        "Its work came before this page’s history began. It reported “{report}”"
                    ),
                    None => "Nothing from it yet.".into(),
                }
            }
        })
    };
    view! {
        <div class="stage">
            <main id="log" class="log" aria-label="Transcript" node_ref=log on:scroll=move |_| scroll::on_scroll()>
                <div id="rows" class="column rows">
                    <For
                        each=move || rows.get()
                        key=|row| match row {
                            Row::Count { count, .. } => format!("{}:{}:{}:{}", row.key(), count.calls, count.failed, count.running),
                            _ => row.key(),
                        }
                        children=move |row| row_view(row, thinking)
                    />
                </div>
                <p class="empty">{empty}</p>
            </main>
            <button
                type="button"
                class="jump"
                class:shown=move || !page.pinned.get()
                tabindex=move || if page.pinned.get() { "-1" } else { "0" }
                on:click=move |_| scroll::jump_to_latest()
            >
                <ArrowDown size=16 />
                {jump_label}
                <kbd class="hint-key">"End"</kbd>
            </button>
        </div>
    }
}

fn row_view(row: Row, thinking: Memo<Option<u64>>) -> AnyView {
    match row {
        Row::Working => view! {
            <div class="row working"><span class="mark">"▸"</span><span class="what shimmer">"Working…"</span></div>
        }
        .into_any(),
        Row::Count { id, count, .. } => count_row(id, count),
        Row::Item(id) => match cell(id) {
            Some(cell) => item_row(id, cell, thinking),
            None => ().into_any(),
        },
    }
}

/// A turn's tool calls as one line (Messages). A click shows them in Full.
fn count_row(id: u64, count: Count) -> AnyView {
    let page = page();
    let Count {
        calls,
        failed,
        running,
    } = count;
    view! {
        <div class="row count" data-id=id>
            <button type="button" title="Show them (Full view)"
                on:click=move |_| {
                    scroll::keep_place(id);
                    page.set_view(View::Full);
                }>
                "▸ "{words::plural(calls, "tool call", "tool calls")}
                {(failed > 0).then(|| view! { " · " <span class="failed">{format!("{failed} failed")}</span> })}
                {(running > 0).then_some(" · running")}
            </button>
        </div>
    }
    .into_any()
}

fn local_time(at: i64) -> String {
    js_sys::Date::new(&(at as f64).into())
        .to_locale_time_string("default")
        .as_string()
        .unwrap_or_default()
}

fn item_row(id: u64, cell: Cell, thinking: Memo<Option<u64>>) -> AnyView {
    let page = page();
    let item = cell.data.get_untracked();
    let data = cell.data;
    let text = cell.text;
    let class = move || {
        data.with(|item| {
            let mut class = format!("row {}", kind_class(item.kind));
            if let Some(level) = item.level {
                class.push_str(match level {
                    Level::Warn => " level-warn",
                    Level::Error => " level-error",
                    Level::Info => " level-info",
                });
            }
            if item.pending {
                class.push_str(" pending");
            }
            if item.kind == Kind::Reply && item.agent != MAIN {
                class.push_str(" report");
            }
            class
        })
    };
    let title = local_time(item.at);
    let body = match item.kind {
        Kind::User => {
            let tags = move || {
                let status_app = page.status.with(|status| status.info.app.clone());
                data.with(|item| {
                    let mut tags = Vec::new();
                    if let Some(tag) = words::delivery_tag(item) {
                        tags.push(tag.to_string());
                    }
                    match item.source {
                        Some(Source::Terminal) => tags.push("typed in the terminal".into()),
                        Some(Source::App) => tags.push(format!(
                            "typed in {}",
                            if status_app.is_empty() { "an app".to_string() } else { status_app }
                        )),
                        _ => {}
                    }
                    (!tags.is_empty()).then(|| tags.join(" · "))
                })
            };
            view! {
                <div class="bubble">{move || text.get()}</div>
                {move || tags().map(|tags| view! { <div class="tag">{tags}</div> })}
            }
            .into_any()
        }
        Kind::Text => view! { <Md id=id text=text code=cell.code /> }.into_any(),
        Kind::Thinking => {
            let first = move || text.with(|text| layout::first_line(text));
            view! {
                <details class="thinking" class:streaming=move || thinking.get() == Some(id)>
                    <summary>
                        <span class="mark">"▸"</span>
                        <span class="what">"Thinking"</span>
                        <span class="first">{first}</span>
                    </summary>
                    <Md id=id text=text code=cell.code class="thinking-text" />
                </details>
            }
            .into_any()
        }
        Kind::Message => {
            let from = item.from.as_deref().map(|from| if from == MAIN { "the lead" } else { from });
            let label = match from {
                Some(from) => format!("Message from {from}"),
                None => "Message".into(),
            };
            view! { <span class="label">{label}</span><Md id=id text=text code=cell.code /> }.into_any()
        }
        Kind::Reply => {
            let report = (item.agent != MAIN).then(|| view! { <span class="label">"Report"</span> });
            view! { {report}<Md id=id text=text code=cell.code /> }.into_any()
        }
        Kind::Question => crate::questions::card_view(id, cell),
        Kind::Call => call_row(cell),
        Kind::Result => view! {
            <pre>{move || data.with(|item| item.detail.clone().unwrap_or_else(|| text.get()))}</pre>
        }
        .into_any(),
        Kind::Output => view! { <pre>{move || text.get()}</pre> }.into_any(),
        Kind::Turn => view! {
            <div class="turn-line"><span>{move || text.get()}</span></div>
            {move || data.with(|item| item.detail.clone()).map(|plan| view! { <div class="turn-plan">{plan}</div> })}
        }
        .into_any(),
        Kind::Conversation => view! {
            <div class="turn-line"><span>{move || text.get()}</span></div>
        }
        .into_any(),
        Kind::WorkerLine => {
            let worker = item.worker.clone().unwrap_or_default();
            view! { <WorkerCard id=worker listed=false /> }.into_any()
        }
        Kind::Notice => {
            let label = (item.agent != MAIN).then(|| {
                let agent = item.agent.clone();
                move || {
                    page.workers.track();
                    if page.worker.get().is_some() {
                        return None;
                    }
                    let summary = STORE.with(|store| store.borrow().workers.get(&agent).cloned());
                    Some(view! { <span class="label">{words::notice_label(&agent, summary.as_ref())}</span> })
                }
            });
            view! { {label}<span class="text">{move || text.get()}</span> }.into_any()
        }
    };
    let more = (item.detail.is_some() && !matches!(item.kind, Kind::Call | Kind::Result | Kind::Turn)).then(|| {
        view! {
            <details class="more">
                <summary>"More"</summary>
                <pre class="block">{move || data.with(|item| item.detail.clone().unwrap_or_default())}</pre>
            </details>
        }
    });
    view! {
        <div class=class data-id=id title=title>
            {body}
            {more}
        </div>
    }
    .into_any()
}

fn kind_class(kind: Kind) -> &'static str {
    match kind {
        Kind::User => "user",
        Kind::Text => "text",
        Kind::Thinking => "thinking",
        Kind::Message => "message",
        Kind::Call => "call",
        Kind::Result => "result",
        Kind::Notice => "notice",
        Kind::Output => "output",
        Kind::Question => "question",
        Kind::Reply => "reply",
        Kind::Turn => "turn",
        Kind::WorkerLine => "wline",
        Kind::Conversation => "conversation",
    }
}

/// A call's line: its status, whose it is (a worker's in the main view),
/// what it does and its outcome, and Move to background or Stop while
/// those apply. A long one opens to show itself whole.
fn call_row(cell: Cell) -> AnyView {
    let page = page();
    let data = cell.data;
    let item = data.get_untracked();
    let id = item.id;
    let long = item.detail.is_some() || item.text.chars().count() > 44;
    let line = move || {
        let item = data.get();
        let who = (item.agent != MAIN && page.worker.get().is_none()).then(|| item.agent.clone());
        let movable = page.status.with(|status| status.movable.clone());
        page.workers.track();
        let (moves, stop) = STORE.with(|store| call_actions(&item, &store.borrow(), &movable));
        // As on the earlier page: the status and the outcome after the call.
        let outcome = match item.status {
            Some(CallStatus::Running) => Some(
                view! { <span class="outcome turning"><span class="running"><LoaderCircle size=12 /></span></span> }
                    .into_any(),
            ),
            Some(status) => {
                let failed = status == CallStatus::Failed;
                let title = item.outcome.clone().unwrap_or_default();
                let words = item.outcome.clone().map(|outcome| format!(" {outcome}"));
                Some(
                    view! {
                        <span class="outcome" title=title>
                            <span class=if failed { "failed" } else { "ok" }>{if failed { "✗" } else { "✓" }}</span>
                            {words}
                        </span>
                    }
                    .into_any(),
                )
            }
            None => None,
        };
        view! {
            <span class="call-line">
                <span class="mark">"▸"</span>
                {who.map(|who| view! { <span class="who">{who}</span> })}
                <span class="what">{item.text.clone()}</span>
                {outcome}
                {moves.then(|| view! { <MoveButton id=id /> })}
                {stop.map(|job| view! { <StopJobButton job=job /> })}
            </span>
        }
    };
    if long {
        view! {
            <details class="call">
                <summary>{line}</summary>
                <pre>{move || data.with(|item| item.detail.clone().unwrap_or_else(|| item.text.clone()))}</pre>
            </details>
        }
        .into_any()
    } else {
        view! { {line} }.into_any()
    }
}

#[component]
fn MoveButton(id: u64) -> impl IntoView {
    let busy = RwSignal::new(false);
    view! {
        <button type="button" class="btn outline small call-action" disabled=move || busy.get()
            on:click=move |event| {
                event.stop_propagation();
                event.prevent_default();
                busy.set(true);
                leptos::task::spawn_local(async move {
                    post("/api/background", serde_json::json!({ "item": id })).await;
                });
            }>
            "Move to background"
        </button>
    }
}

#[component]
fn StopJobButton(job: String) -> impl IntoView {
    let busy = RwSignal::new(false);
    view! {
        <button type="button" class="btn small call-action" disabled=move || busy.get()
            on:click=move |event| {
                event.stop_propagation();
                event.prevent_default();
                busy.set(true);
                let job = job.clone();
                leptos::task::spawn_local(async move {
                    post("/api/jobs/stop", serde_json::json!({ "job": job })).await;
                });
            }>
            "Stop"
        </button>
    }
}

/// A worker's live card. In the workers' sheet (`listed`), the workers a
/// worker started stand indented under it.
#[component]
pub fn WorkerCard(id: String, listed: bool) -> impl IntoView {
    let page = page();
    let summary = {
        let id = id.clone();
        move || {
            page.workers.track();
            STORE.with(|store| store.borrow().workers.get(&id).cloned())
        }
    };
    let state = {
        let summary = summary.clone();
        move || match summary().map(|summary| summary.state) {
            Some(WorkerState::Queued) => "queued",
            Some(WorkerState::Finished) => "finished",
            Some(WorkerState::Failed) => "failed",
            Some(WorkerState::Stopped) => "stopped",
            _ => "running",
        }
    };
    let depth = {
        let summary = summary.clone();
        move || {
            if listed {
                summary().map_or(0, |s| s.depth.saturating_sub(1).min(3))
            } else {
                0
            }
        }
    };
    let words = {
        let id = id.clone();
        move || {
            let now = page.server_now();
            let viewing = if listed { None } else { page.worker.get() };
            match summary() {
                Some(summary) => words::worker_words(&summary, now, viewing.as_deref()),
                None => (id.clone(), "0 calls · running".into(), String::new()),
            }
        }
    };
    let open = id.clone();
    let label = {
        let words = words.clone();
        let id = id.clone();
        move || {
            let (title, meta, _) = words();
            format!("{id}, {title}, {meta}")
        }
    };
    view! {
        <button
            type="button"
            class=move || format!("wcard {} depth-{}", state(), depth())
            aria-label=label
            on:click=move |_| {
                page.sheet.set(None);
                show_worker(page, Some(open.clone()));
            }
        >
            <span class="wtitle"><span class="wid">{id.clone()}</span>{{ let words = words.clone(); move || words().0 }}</span>
            <span class="wgo"><ChevronRight size=16 /></span>
            <span class="wmeta">{{ let words = words.clone(); move || words().1 }}</span>
            {move || { let last = words().2; (!last.is_empty()).then(|| view! { <span class="wlast">{last}</span> }) }}
        </button>
    }
}

thread_local! {
    /// Where the reader was in the main transcript, to come back to.
    static MAIN_PLACE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Shows the worker `id`'s own transcript (none for the main one). A
/// worker's opens at its latest, and going back returns to where the
/// reader was.
pub fn show_worker(page: Page, id: Option<String>) {
    if page.worker.get_untracked() == id {
        return;
    }
    if page.worker.get_untracked().is_none() {
        MAIN_PLACE.with(|place| {
            place.set(if page.pinned.get_untracked() {
                None
            } else {
                scroll::first_visible()
            })
        });
    }
    let back_to = if id.is_none() {
        MAIN_PLACE.with(|place| place.take())
    } else {
        None
    };
    if let Ok(history) = window().history() {
        match &id {
            Some(worker) => {
                let state = serde_wasm_state(worker);
                let _ = history.push_state(&state, "");
            }
            None => {
                if history
                    .state()
                    .ok()
                    .is_some_and(|state| !state.is_null() && !state.is_undefined())
                {
                    let _ = history.back();
                }
            }
        }
    }
    page.worker.set(id);
    match back_to {
        Some(at) => scroll::keep_place(at),
        None => {
            page.pinned.set(true);
            let frame = wasm_bindgen::closure::Closure::once_into_js(scroll::to_bottom_now);
            use wasm_bindgen::JsCast;
            let _ = window().request_animation_frame(frame.unchecked_ref());
        }
    }
}

fn serde_wasm_state(worker: &str) -> wasm_bindgen::JsValue {
    let state = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&state, &"worker".into(), &worker.into());
    state.into()
}

/// Markdown that may still stream: its finished blocks stay as they are,
/// and only the open block at its end is read again as text comes. When the
/// spans of its code blocks come (`code` moves), the blocks with code are
/// painted again.
#[component]
fn Md(
    id: u64,
    text: RwSignal<String>,
    code: RwSignal<u32>,
    #[prop(optional)] class: &'static str,
) -> impl IntoView {
    let page = page();
    let streaming = Memo::new(move |_| page.streaming.get() == Some(id));
    let mut blocks = Blocks::default();
    let (done, open) = text.with_untracked(|text| blocks.update(text, streaming.get_untracked()));
    // Each finished block: its place, how often it was painted, its HTML.
    let frozen = RwSignal::new(
        done.iter()
            .enumerate()
            .map(|(at, (_, html))| (at, 0u32, html.clone()))
            .collect::<Vec<_>>(),
    );
    let sources = StoredValue::new(
        done.into_iter()
            .map(|(source, _)| source)
            .collect::<Vec<_>>(),
    );
    let tail = RwSignal::new(open);
    let blocks = StoredValue::new(blocks);
    Effect::new(move |first: Option<()>| {
        let streaming = streaming.get();
        text.track();
        if first.is_none() {
            return;
        }
        let (done, open) = text.with_untracked(|text| blocks.write_value().update(text, streaming));
        if blocks.with_value(|blocks| blocks.started_over) {
            frozen.set(Vec::new());
            sources.set_value(Vec::new());
        }
        if !done.is_empty() {
            frozen.update(|frozen| {
                for (source, html) in done {
                    frozen.push((frozen.len(), 0, html));
                    sources.update_value(|sources| sources.push(source));
                }
            });
        }
        tail.set(open);
    });
    Effect::new(move |first: Option<()>| {
        code.track();
        if first.is_none() {
            return;
        }
        let fenced = |source: &str| source.contains("```") || source.contains("~~~");
        sources.with_value(|sources| {
            frozen.update(|frozen| {
                for (at, painted, html) in frozen.iter_mut() {
                    if let Some(source) = sources.get(*at).filter(|source| fenced(source)) {
                        *painted += 1;
                        *html = markdown::to_html(source);
                    }
                }
            })
        });
        let open = text.with_untracked(|text| {
            blocks
                .write_value()
                .update(text, streaming.get_untracked())
                .1
        });
        tail.set(open);
    });
    view! {
        <div class=format!("md {class}")>
            <For each=move || frozen.get() key=|(at, painted, _)| (*at, *painted) let:block>
                <div class="mdb" inner_html=block.2></div>
            </For>
            <div class="mdb" inner_html=move || tail.get()></div>
        </div>
    }
}

/// The item's data, for views that need it whole.
#[allow(dead_code)]
pub fn item(id: u64) -> Option<ItemData> {
    STORE.with(|store| store.borrow().get(id).cloned())
}
