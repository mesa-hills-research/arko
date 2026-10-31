//! The transcript: its rows, laid out for the view, and Latest.

use std::collections::HashSet;

use leptos::prelude::*;
use lucide_leptos::{ArrowDown, ChevronRight};

use crate::app::{Page, RowCell, STORE, cell, page, post, store_is_current, track_worker};
use crate::copy::CopyButton;
use crate::images::Thumbnails;
use crate::layout::{self, Count, Row, View};
use crate::markdown::{self, Blocks};
use crate::protocol::{CallStatus, ItemData, Kind, Level, Source, WorkerState};
use crate::spinner::Spinner;
use crate::store::{Entry, MAIN, call_actions};
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
            page.grown.update(HashSet::clear);
        }
    });
    let jump_label = move || {
        rows.with(|rows| {
            page.grown.with(|grown| {
                STORE
                    .with(|store| layout::jump_label(rows, &store.borrow(), page.seen.get(), grown))
            })
        })
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
            <main id="log" class="log" class:pinned=move || page.pinned.get() aria-label="Transcript" node_ref=log on:scroll=move |_| scroll::on_scroll()>
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
            <div class="row working">
                <span class="what shimmer">"Working…"<span class="shine" data-text="Working…" aria-hidden="true"></span></span>
            </div>
        }
        .into_any(),
        Row::Count { id, count, .. } => count_row(id, count),
        Row::Item(id) => match cell(id) {
            Some(cell) => item_row(id, cell.for_row(), thinking),
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

fn item_row(id: u64, cell: RowCell, thinking: Memo<Option<u64>>) -> AnyView {
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
                data.with(|item| {
                    let mut tags = Vec::new();
                    if let Some(tag) = words::delivery_tag(item) {
                        tags.push(tag.to_string());
                    }
                    match item.source {
                        Some(Source::Terminal) => tags.push("typed in the terminal".into()),
                        Some(Source::App) => tags.push(page.app.with(|app| {
                            format!("typed in {}", if app.is_empty() { "an app" } else { app })
                        })),
                        _ => {}
                    }
                    (!tags.is_empty()).then(|| tags.join(" · "))
                })
            };
            // Its images, then its words, and under it its tags and Copy.
            view! {
                <div class="bubble"><Thumbnails data=data />{move || text.get()}</div>
                <div class="actions">
                    {move || tags().map(|tags| view! { <span class="tag">{tags}</span> })}
                    <CopyButton label="Copy message" text=text />
                </div>
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
                        <span class="what">"Thinking"<span class="shine" data-text="Thinking" aria-hidden="true"></span></span>
                        <span class="first">{first}</span>
                    </summary>
                    <Md id=id text=text code=cell.code class="thinking-text" />
                </details>
            }
            .into_any()
        }
        Kind::Message => {
            let from = item.from.clone().map(|from| if from == MAIN { "the lead".into() } else { from });
            let agent = item.agent.clone();
            // A worker's message to the user names the worker outside its own view.
            let label = move || match &from {
                Some(from) => format!("Message from {from}"),
                None if agent != MAIN && page.worker.get().is_none() => format!("{agent} to you"),
                None => "Message".into(),
            };
            view! {
                <span class="label">{label}</span>
                <Md id=id text=text code=cell.code />
                <div class="actions"><CopyButton label="Copy message" text=text /></div>
            }
            .into_any()
        }
        Kind::Reply => {
            let report = (item.agent != MAIN).then(|| view! { <span class="label">"Report"</span> });
            view! {
                {report}
                <Md id=id text=text code=cell.code />
                <div class="actions"><CopyButton label="Copy message" text=text /></div>
            }
            .into_any()
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
                    if page.worker.get().is_some() {
                        return None;
                    }
                    track_worker(&agent);
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
/// those apply. A long one opens to show itself whole. The line follows
/// its own item, and its actions follow the status only while the call
/// runs and the jobs only while it has one, so the many settled calls of a
/// long session follow neither.
fn call_row(cell: RowCell) -> AnyView {
    let page = page();
    let data = cell.data;
    let (id, long) = data.with_untracked(|item| {
        (
            item.id,
            item.detail.is_some() || item.text.chars().count() > 44,
        )
    });
    let actions = Memo::new(move |_| {
        data.with(|item| {
            call_actions(
                item,
                |id| page.movable.with(|movable| movable.contains(&id)),
                |job| {
                    page.running_jobs
                        .with(|running| running.iter().any(|running| running == job))
                },
            )
        })
    });
    let line = move || {
        data.with(|item| {
            let who = (item.agent != MAIN && page.worker.get().is_none()).then(|| item.agent.clone());
            // As on the earlier page: the status and the outcome after the call.
            let outcome = match item.status {
                Some(CallStatus::Running) => Some(
                    view! { <span class="outcome"><span class="running"><Spinner /></span></span> }
                        .into_any(),
                ),
                Some(status) => {
                    let failed = status == CallStatus::Failed;
                    let title = item.outcome.clone().unwrap_or_default();
                    let words = item.outcome.as_ref().map(|outcome| format!(" {outcome}"));
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
                    {move || {
                        let (moves, stop) = actions.get();
                        view! {
                            {moves.then(|| view! { <MoveButton id=id /> })}
                            {stop.map(|job| view! { <StopJobButton job=job /> })}
                        }
                    }}
                </span>
            }
        })
    };
    // The images its tool returned show under it, open or not.
    if long {
        view! {
            <details class="call">
                <summary>{line}</summary>
                <pre>{move || data.with(|item| item.detail.clone().unwrap_or_else(|| item.text.clone()))}</pre>
            </details>
            <Thumbnails data=data />
        }
        .into_any()
    } else {
        view! { {line}<Thumbnails data=data /> }.into_any()
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
                    let taken = post("/api/background", serde_json::json!({ "item": id })).await;
                    answered(busy, taken);
                });
            }>
            "Move to background"
        </button>
    }
}

/// A call's action button once Arko answered. One Arko took stays busy
/// until Arko's update takes the action away. One it didn't take can be
/// pressed again (the error line says why), while its row still shows.
fn answered(busy: RwSignal<bool>, taken: bool) {
    if !taken {
        busy.try_set(false);
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
                    let taken = post("/api/jobs/stop", serde_json::json!({ "job": job })).await;
                    answered(busy, taken);
                });
            }>
            "Stop"
        </button>
    }
}

/// A worker's live card. In the workers' sheet (`listed`), the workers a
/// worker started stand indented under it. It follows its own worker's
/// summary, makes its words once for the places that show them, and
/// follows the clock only while its worker's time runs.
#[component]
pub fn WorkerCard(id: String, listed: bool) -> impl IntoView {
    let page = page();
    let summary = Memo::new({
        let id = id.clone();
        move |_| {
            track_worker(&id);
            STORE.with(|store| store.borrow().workers.get(&id).cloned())
        }
    });
    let state = move || {
        summary.with(
            |summary| match summary.as_ref().map(|summary| summary.state) {
                Some(WorkerState::Queued) => "queued",
                Some(WorkerState::Finished) => "finished",
                Some(WorkerState::Failed) => "failed",
                Some(WorkerState::Stopped) => "stopped",
                _ => "running",
            },
        )
    };
    let depth = move || {
        if listed {
            summary.with(|summary| {
                summary
                    .as_ref()
                    .map_or(0, |summary| summary.depth.saturating_sub(1).min(3))
            })
        } else {
            0
        }
    };
    let words = Memo::new({
        let id = id.clone();
        move |_| {
            let viewing = if listed { None } else { page.worker.get() };
            summary.with(|summary| match summary {
                Some(summary) => {
                    // (A worker that isn't running has a run time that stays.)
                    let now = if summary.running_since.is_some() {
                        page.server_now()
                    } else {
                        0.0
                    };
                    page.models
                        .with(|usual| words::worker_words(summary, now, viewing.as_deref(), usual))
                }
                None => (id.clone(), "0 calls · running".into(), String::new()),
            })
        }
    });
    let open = id.clone();
    let label = {
        let id = id.clone();
        move || words.with(|(title, meta, _)| format!("{id}, {title}, {meta}"))
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
            <span class="wtitle"><span class="wid">{id.clone()}</span>{move || words.with(|words| words.0.clone())}</span>
            <span class="wgo"><ChevronRight size=16 /></span>
            <span class="wmeta">{move || words.with(|words| words.1.clone())}</span>
            {move || words.with(|words| (!words.2.is_empty()).then(|| words.2.clone())).map(|last| view! { <span class="wlast">{last}</span> })}
        </button>
    }
}

thread_local! {
    /// Where the reader was in the main transcript, to come back to.
    static MAIN_PLACE: std::cell::Cell<Option<(u64, f64)>> = const { std::cell::Cell::new(None) };
}

/// Shows the worker `id`'s own transcript (none for the main one) in a new
/// entry of the browser's history, so Back and Forward step through the
/// views the reader saw.
pub fn show_worker(page: Page, id: Option<String>) {
    if page.worker.get_untracked() == id {
        return;
    }
    let entry = Entry {
        session: id.is_some().then(|| session_of(page)),
        worker: id.clone(),
    };
    if let Ok(history) = window().history() {
        let _ = history.push_state(&history_state(&entry), "");
    }
    switch_view(page, id);
}

/// Shows the view of the history entry that Back or Forward reached, whose
/// state is `state`, and again for the entry shown once the stream starts
/// (over). Until then, a worker's entry shows that worker. Once it has, a
/// worker of another session, or one gone, shows the main transcript, and
/// the entry says so from then on.
pub fn show_history_entry(page: Page, state: &wasm_bindgen::JsValue) {
    let text = |name: &str| {
        js_sys::Reflect::get(state, &name.into())
            .ok()
            .and_then(|value| value.as_string())
    };
    let entry = Entry {
        worker: text("worker"),
        session: text("session"),
    };
    let shown = STORE.with(|store| {
        store
            .borrow()
            .entry_view(&entry, &session_of(page), store_is_current())
    });
    if shown != entry.worker
        && let Ok(history) = window().history()
    {
        let _ = history.replace_state(&history_state(&Entry::default()), "");
    }
    if page.worker.get_untracked() != shown {
        switch_view(page, shown);
    }
}

/// The state of the history entry `entry`, null for the main transcript.
pub fn history_state(entry: &Entry) -> wasm_bindgen::JsValue {
    let Some(worker) = &entry.worker else {
        return wasm_bindgen::JsValue::NULL;
    };
    let state = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&state, &"worker".into(), &worker.into());
    if let Some(session) = &entry.session {
        let _ = js_sys::Reflect::set(&state, &"session".into(), &session.into());
    }
    state.into()
}

/// The session the page follows, as its status names it.
fn session_of(page: Page) -> String {
    page.status
        .with_untracked(|status| status.info.session.clone())
}

/// Shows the worker `id`'s own transcript (none for the main one). A
/// worker's opens at its latest, and coming back to the main one returns
/// to where the reader was.
fn switch_view(page: Page, id: Option<String>) {
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
    page.worker.set(id);
    match back_to {
        Some(place) => scroll::restore_place(place),
        None => {
            page.pinned.set(true);
            let frame = wasm_bindgen::closure::Closure::once_into_js(scroll::to_bottom_now);
            use wasm_bindgen::JsCast;
            let _ = window().request_animation_frame(frame.unchecked_ref());
        }
    }
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
    // Whether it streams, which this text alone hears of.
    let streaming = Memo::new(move |_| crate::app::streaming_is(id));
    on_cleanup(move || crate::app::streaming_forget(id));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A request Arko didn't take frees its button, one it took keeps it
    /// busy, and a button whose row went meanwhile is left alone.
    #[test]
    fn an_action_arko_didnt_take_can_be_pressed_again() {
        let owner = Owner::new();
        owner.with(|| {
            let busy = RwSignal::new(true);
            answered(busy, true);
            assert!(busy.get_untracked());
            answered(busy, false);
            assert!(!busy.get_untracked());

            let gone = RwSignal::new(true);
            gone.dispose();
            answered(gone, false);
        });
    }
}
