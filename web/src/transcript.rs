//! The transcript, with its rows laid out for the view, and Latest.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use lucide_leptos::{ArrowDown, ChevronRight};

use crate::app::{Page, RowCell, STORE, cell, feed, page, post, track_worker};
use crate::copy::CopyButton;
use crate::files::SentFiles;
use crate::images::Thumbnails;
use crate::layout::{self, Count, Row, View};
use crate::markdown::{self, Blocks};
use crate::protocol::{ActKind, CallStatus, ItemData, Kind, Level, Source, WorkerState};
use crate::shimmer::Shimmer;
use crate::spinner::Spinner;
use crate::store::{Entry, MAIN, call_actions};
use crate::{contents, scroll, words};

#[component]
pub fn Transcript() -> impl IntoView {
    let page = page();
    let log: NodeRef<leptos::html::Main> = NodeRef::new();
    log.on_load(move |log| scroll::attach(log.into(), page.pinned, page.width));
    let laid_out = laid_out(page.version, page.view, page.worker);
    // What shows the agent is under way.
    let motion = Memo::new(move |_| {
        page.version.track();
        page.workers.track();
        page.now.track();
        let view = page.view.get();
        let worker = page.worker.get();
        let live = page.connection.get() == words::Connection::Live;
        if !live {
            return (None, false, false);
        }
        page.status.with(|status| {
            STORE.with(|store| {
                let store = store.borrow();
                let now = js_sys::Date::now();
                let (thinking, working) =
                    layout::motion(&store, status, view, worker.as_deref(), now);
                let room = layout::room(&store, status, view, worker.as_deref(), now);
                (thinking, working, room)
            })
        })
    });
    // The rows, and whether the Working row is only its room, unseen.
    let rows = Memo::new(move |_| {
        let (thinking, working, room) = motion.get();
        let (feed, rows) = laid_out.get();
        let worker = page.worker.get();
        STORE.with(|store| {
            let store = store.borrow();
            // The thinking that streams is the live line where its row
            // shows, and a call that runs with a row of its own shows that
            // there.
            let live = thinking.is_some_and(|id| rows.contains(&Row::Item(id, Kind::Thinking)));
            let elsewhere =
                working && (live || layout::running_shown(&rows, &store, worker.as_deref()));
            let working = working && !elsewhere;
            let room = !working && (room || elsewhere);
            let rows = layout::with_working(rows, working || room, &store);
            (feed, rows, room)
        })
    });
    let room = Memo::new(move |_| rows.with(|(_, _, room)| *room));
    let thinking = Memo::new(move |_| motion.get().0);
    // Quiet rows that stay follow their words and calls.
    Effect::new(move |_| rows.with(|(_, rows, _)| sync_quiet(rows)));
    // The newest item the reader saw, while at the bottom.
    Effect::new(move |_| {
        rows.track();
        if page.pinned.get() {
            page.seen.set(STORE.with(|store| store.borrow().newest()));
            page.grown.update(HashSet::clear);
        }
    });
    let jump_label = move || {
        rows.with(|(_, rows, _)| {
            page.grown.with(|grown| {
                STORE
                    .with(|store| layout::jump_label(rows, &store.borrow(), page.seen.get(), grown))
            })
        })
    };
    let empty = move || {
        if !rows.with(|(_, rows, _)| rows.is_empty())
            || page.connection.get() != words::Connection::Live
        {
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
                        each=move || {
                            let (feed, rows, _) = rows.get();
                            rows.into_iter().map(move |row| (feed, row))
                        }
                        key=|(feed, row)| row_key(*feed, row)
                        children=move |(_, row)| row_view(row, thinking, room)
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

/// The rows of the view, without "Working…", and the feed they're of, laid
/// out again when `version` moves.
fn laid_out(
    version: RwSignal<u64>,
    view: RwSignal<View>,
    worker: RwSignal<Option<String>>,
) -> Memo<(Option<u64>, Vec<Row>)> {
    let page = page();
    // The workers that workers started, by their leads, which change far
    // less often than the workers' summaries do.
    let leads = Memo::new(move |_| {
        page.workers.track();
        STORE.with(|store| {
            store
                .borrow()
                .workers
                .values()
                .filter_map(|worker| Some((worker.id.clone(), worker.parent.clone()?)))
                .collect::<HashMap<String, String>>()
        })
    });
    Memo::new(move |_| {
        version.track();
        let view = view.get();
        let worker = worker.get();
        let counts_open = page.counts_open.get();
        leads.with(|leads| {
            // (A row sealed as it opens holds one call already, so the seal
            // changes only where later calls go, and their layout reads it.)
            page.quiet_sealed.with_untracked(|sealed| {
                STORE.with(|store| {
                    let store = store.borrow();
                    let rows = layout::layout_sealed(
                        &store.items,
                        view,
                        worker.as_deref(),
                        sealed,
                        leads,
                        &counts_open,
                    );
                    (store.feed, rows)
                })
            })
        })
    })
}

/// A row's key in the transcript, with the feed it's of, as another feed's
/// item 7 is another item, its identity, an item's kind, which decides how
/// its row is drawn (a text becomes the reply), and a count's numbers,
/// which its line shows.
fn row_key(feed: Option<u64>, row: &Row) -> (Option<u64>, String) {
    let key = match row {
        Row::Item(_, kind) => format!("{}:{kind:?}", row.key()),
        Row::Count { count, .. } => format!(
            "{}:{}:{}:{}",
            row.key(),
            count.calls,
            count.failed,
            count.running
        ),
        _ => row.key(),
    };
    (feed, key)
}

/// A row of the transcript. `room` says the Working row is only its room,
/// unseen ([`layout::room`]). Each row says its item (`data-id`) and its own
/// key among the rows shown (`data-row`, [`Row::key`]), which the reader's
/// place keeps to ([`scroll::Anchor`]).
fn row_view(row: Row, thinking: Memo<Option<u64>>, room: Memo<bool>) -> AnyView {
    let key = row.key();
    match row {
        // The live line while the agent waits on its model, its spinner in
        // the markers' column and its words where a thinking's start, so
        // the thinking that takes its place starts where it stood.
        Row::Working => {
            let page = page();
            // What the agent's call does while it runs, else "Working…".
            let label = Memo::new(move |_| {
                page.version.track();
                let worker = page.worker.get();
                STORE.with(|store| layout::working_label(&store.borrow(), worker.as_deref()))
            });
            view! {
                <div class="row working" class:room=move || room.get()
                    aria-hidden=move || room.get().then_some("true")>
                    <span class="mark">{move || (!room.get()).then(|| view! { <Spinner /> })}</span>
                    <Shimmer class="what" text=move || label.get() active=move || !room.get() />
                </div>
            }
            .into_any()
        }
        Row::Count { id, count, .. } => count_row(id, key, count),
        Row::Quiet { id, row } => quiet_row(id, key, row, thinking),
        Row::Status(id) => match cell(id) {
            Some(cell) => status_row(id, key, cell.for_row()),
            None => ().into_any(),
        },
        Row::Item(id, _) => match cell(id) {
            Some(cell) => item_row(id, key, cell.for_row(), thinking),
            None => ().into_any(),
        },
    }
}

/// A turn's tool calls as one line (Messages), its triangle in the column
/// of every fold's. A click opens it to the turn's Quiet rows, each where
/// it came, and closes it again, its line staying where it is.
fn count_row(id: u64, key: String, count: Count) -> AnyView {
    let page = page();
    let Count {
        calls,
        failed,
        running,
    } = count;
    let open = move || page.counts_open.with(|open| open.contains(&id));
    // (It keeps its place as it opens, not the Quiet row of its first call.)
    let anchor = scroll::Anchor {
        id,
        key: key.clone(),
    };
    view! {
        <div class="row count" data-id=id data-row=key>
            <button type="button" class="call-line" aria-expanded=move || open().to_string()
                on:click=move |_| {
                    scroll::keep_place(anchor.clone());
                    page.counts_open.update(|open| {
                        if !open.remove(&id) {
                            open.insert(id);
                        }
                    });
                }>
                <span class="mark">"▸"</span>
                <span>{words::plural(calls, "tool call", "tool calls")}</span>
                {(failed > 0).then(|| view! {
                    <span class="outcome"><span class="failed">{format!("{failed} failed")}</span></span>
                })}
                {(running > 0).then(|| view! { <span class="outcome">"running"</span> })}
            </button>
        </div>
    }
    .into_any()
}

/// Whether the element under `root` that `selector` finds shows less than
/// its text, its end cut with an ellipsis.
fn overflows(root: &web_sys::Element, selector: &str) -> bool {
    root.query_selector(selector)
        .ok()
        .flatten()
        .is_some_and(|found| found.scroll_width() > found.client_width())
}

/// The element an event's handler is on.
fn handler_element(event: &web_sys::Event) -> Option<web_sys::Element> {
    use wasm_bindgen::JsCast;
    event.current_target()?.dyn_into::<web_sys::Element>().ok()
}

/// Whether the words (`.what`) of the line `summary` holds are cut, as
/// drawn, measured once the line shows and again whenever `words` say
/// something else or the transcript's width changes. A line that cuts its
/// words opens to them whole.
fn cut_words(
    summary: NodeRef<leptos::html::Summary>,
    words: impl Fn() -> String + Send + Sync + 'static,
) -> RwSignal<bool> {
    let cut = RwSignal::new(false);
    let width = page().width;
    Effect::new(move |_| {
        words();
        width.track();
        let measure = move || {
            if let Some(summary) = summary.get_untracked() {
                cut.try_set(overflows(&summary, ".what"));
            }
        };
        // (Once the browser has laid the line out.)
        let frame = wasm_bindgen::closure::Closure::once_into_js(measure);
        use wasm_bindgen::JsCast;
        let _ = window().request_animation_frame(frame.unchecked_ref());
    });
    cut
}

/// Calls in a few words (Quiet), by the first one's ID. The row keeps its
/// place in the page while calls join it and its words change, so focus and
/// what's open inside it stay ([`QUIET`]). Its line is one line, its words
/// cut to fit and its outcome whole. A click on its line (or Enter on it)
/// opens it to what its calls hold, on a rail ([`quiet_member`]), which is
/// made only then, and to the call as written where the line cut its words.
/// A row with nothing more to show has no fold. Each of its calls keeps
/// what its own row offers (Move to background, Stop) and the images its
/// tool returned under the line, as Full does, whatever row it's in.
fn quiet_row(id: u64, key: String, row: layout::QuietRow, thinking: Memo<Option<u64>>) -> AnyView {
    let page = page();
    let shared = QUIET.with(|quiet| {
        quiet
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| ArcRwSignal::new(row.clone()))
            .clone()
    });
    let data = RwSignal::from(&shared);
    let summary = NodeRef::<leptos::html::Summary>::new();
    let cut = cut_words(summary, move || data.with(|row| row.words.said()));
    let opens = move || data.with(|row| row.opens);
    let open = move || (opens() || cut.get()) && page.quiet_open.contains(id);
    let words = move || data.with(|row| row.words.clone());
    // The row's calls, by ID.
    let members = move || {
        data.with(|row| {
            row.calls
                .iter()
                .filter_map(|row| match row {
                    Row::Item(id, Kind::Call) => Some(*id),
                    _ => None,
                })
                .collect::<Vec<u64>>()
        })
    };
    let class = move || {
        let words = words();
        let mut class = "row call quiet".to_string();
        if words.failed {
            class.push_str(" has-failure");
        }
        if words.muted {
            class.push_str(" muted");
        }
        if !opens() && !cut.get() {
            class.push_str(" still");
        }
        class
    };
    // What each call's own row offers (Move to background while it runs,
    // Stop while its job or monitor runs), whatever row it's in.
    let actions = move || {
        members()
            .into_iter()
            .filter_map(cell)
            .map(|cell| {
                let (moves, stop) = cell.data.with(|item| {
                    call_actions(
                        item,
                        |id| page.movable.with(|movable| movable.contains(&id)),
                        |job| {
                            page.running_jobs
                                .with(|running| running.iter().any(|running| running == job))
                        },
                    )
                });
                let call = cell.data.with_untracked(|item| item.id);
                view! {
                    {moves.then(|| view! { <MoveButton id=call /> })}
                    {stop.map(|job| view! { <StopJobButton job=job /> })}
                }
            })
            .collect_view()
    };
    // The images the calls' tools returned, also those that come later,
    // under the line as Full shows them. (Open, each call shows its own.)
    let images = move || {
        (!open()).then(|| {
            members()
                .into_iter()
                .filter_map(cell)
                .map(|cell| view! { <Thumbnails data=cell.for_row().data /> })
                .collect_view()
        })
    };
    // Where its line cut its words, a row of one call opens to the call as
    // written.
    let whole = move || {
        (cut.get() && !data.with(|row| row.merged))
            .then(|| members().first().copied().and_then(cell))
            .flatten()
            .map(|cell| {
                let text = cell.data.with(|item| item.text.clone());
                view! { <div class="holds"><pre class="held">{text}</pre></div> }
            })
    };
    let inside = move || {
        open().then(|| {
            view! {
                {whole}
                <For
                    each=move || data.with(|row| layout::quiet_members(&row.calls))
                    key=|(row, results)| {
                        (row_key(None, row), results.iter().map(Row::key).collect::<Vec<_>>())
                    }
                    children=move |(row, results)| quiet_member(row, results, data, thinking)
                />
            }
        })
    };
    view! {
        <div class=class data-id=id data-row=key>
            <details class="call quiet" prop:open=open>
                <summary
                    node_ref=summary
                    tabindex=move || if opens() || cut.get() { "0" } else { "-1" }
                    on:click=move |event| {
                        event.prevent_default();
                        let opening = !page.quiet_open.contains_untracked(id);
                        if opening {
                            let cuts = handler_element(&event)
                                .is_some_and(|summary| overflows(&summary, ".what"));
                            cut.set(cuts);
                            if !opens() && !cuts {
                                return;
                            }
                        }
                        page.quiet_open.toggle(id);
                        // A row of one call the reader opened keeps its form.
                        if opening && !data.with_untracked(|row| row.merged) {
                            page.quiet_sealed.update(|sealed| {
                                sealed.insert(id);
                            });
                        }
                    }
                >
                    <span class="call-line">
                        // (Its triangle turns as it opens.)
                        <span class="mark">{move || if opens() || cut.get() { "▸" } else { "" }}</span>
                        <span class="what" title=move || words().said()>{move || words().text}</span>
                        {move || words().news.map(|news| view! { <span class="news">{format!("({news})")}</span> })}
                        {move || words().note.map(|note| view! { <span class="outcome" title=note.clone()>{note.clone()}</span> })}
                        {move || words().running.then(|| view! {
                            <span class="outcome"><Spinner /></span>
                        })}
                        {actions}
                        {move || words().failed.then(|| view! {
                            <span class="outcome right"><span class="failed">"✗"</span></span>
                        })}
                    </span>
                </summary>
                <div class="quiet-calls">{inside}</div>
            </details>
            {images}
        </div>
    }
    .into_any()
}

/// What an open Quiet row shows of one of its calls, on the row's rail.
/// In a row of one call that's the call's detail and content. In a row of
/// several it's the call's Full line, muted, which opens to its detail and
/// content, and to the call as written where the line cut it. A thinking
/// between them shows as it does on its own, in their text column. Anything
/// open stands apart from its neighbours (`.open`).
fn quiet_member(
    row: Row,
    results: Vec<Row>,
    quiet: RwSignal<layout::QuietRow>,
    thinking: Memo<Option<u64>>,
) -> AnyView {
    let page = page();
    let Row::Item(id, Kind::Call) = row else {
        return match row {
            Row::Item(id, Kind::Thinking) => match cell(id) {
                Some(cell) => thought_row(id, row.key(), cell.for_row(), thinking, true),
                None => ().into_any(),
            },
            row => row_view(row, thinking, Memo::new(|_| false)),
        };
    };
    let Some(cell) = cell(id) else {
        return ().into_any();
    };
    let data = cell.for_row().data;
    let merged = move || quiet.with(|row| row.merged);
    let has_content = !results.is_empty();
    let folds = move || has_content || data.with(|item| item.detail.is_some());
    // Its line cut its words as the reader opened it.
    let cut = RwSignal::new(false);
    let open = move || !merged() || ((folds() || cut.get()) && page.calls_open.contains(id));
    // What it holds, its detail and its content.
    let contents: Vec<RwSignal<ItemData>> = results
        .iter()
        .filter_map(|row| match row {
            Row::Item(result, Kind::Result) => {
                crate::app::cell(*result).map(|cell| cell.for_row().data)
            }
            _ => None,
        })
        .collect();
    let detail = move || {
        data.with(|item| {
            if merged() {
                item.detail.clone()
            } else {
                item.act
                    .as_ref()
                    .map(|act| act.detail.clone())
                    .filter(|detail| !detail.is_empty())
            }
        })
        .map(|detail| view! { <pre class="held">{detail}</pre> })
    };
    let whole = move || {
        (merged() && cut.get())
            .then(|| view! { <pre class="held">{data.with(|item| item.text.clone())}</pre> })
    };
    let holds = move || {
        open().then(|| {
            view! {
                {whole}
                {detail}
                {contents.iter().map(|data| contents::content_view(*data)).collect_view()}
            }
        })
    };
    let line = move || {
        merged().then(|| {
            let mark = move || match (folds() || cut.get(), open()) {
                (false, _) => "",
                (true, false) => "▸",
                (true, true) => "▾",
            };
            view! {
                <button type="button" class="member-line"
                    on:click=move |event| {
                        if !page.calls_open.contains_untracked(id) {
                            let cuts = handler_element(&event)
                                .is_some_and(|line| overflows(&line, ".what"));
                            cut.set(cuts);
                            if !folds() && !cuts {
                                return;
                            }
                        }
                        page.calls_open.toggle(id);
                    }>
                    {call_line(data, mark, true)}
                </button>
            }
        })
    };
    // (Open, its line and what it holds stand apart from its neighbours.)
    view! {
        <div class="quiet-member" class:own=merged class:open=move || merged() && open()>
            {line}
            <div class="holds">
                {holds}
                <Thumbnails data=data />
            </div>
        </div>
    }
    .into_any()
}

/// An agent's thinking, in the words' column with no label or marker, its
/// muted italic words its first line, which a click, a tap or Enter opens
/// whole and closes again. One that fits on its line has nothing to open.
/// While it streams, the spinner turns in the markers' column. `inside` is
/// set for one on an open Quiet row's rail. One the reader opened on its own
/// line in Quiet stays there when the next call comes
/// ([`crate::quiet::Step::Thinking`]).
fn thought_row(
    id: u64,
    key: String,
    cell: RowCell,
    thinking: Memo<Option<u64>>,
    inside: bool,
) -> AnyView {
    let page = page();
    let text = cell.text;
    // (Full keeps its thoughts open apart from the other views.)
    let opened = move || {
        if !inside && page.view.get() == View::Full {
            page.thoughts_open
        } else {
            page.thoughts_open_quiet
        }
    };
    let open = Memo::new(move |_| opened().contains(id));
    let first = move || {
        text.with(|text| {
            let (first, more) = layout::first_line(text);
            if more { format!("{first} …") } else { first }
        })
    };
    let toggle = move |event: &web_sys::Event| {
        let opening = !open.get_untracked();
        if opening {
            let more = text.with_untracked(|text| layout::first_line(text).1);
            let cut = handler_element(event).is_some_and(|words| overflows(&words, ".first"));
            if !more && !cut {
                return;
            }
            if !inside && page.view.get_untracked() != View::Full {
                page.quiet_sealed.update(|sealed| {
                    sealed.insert(id);
                });
            }
        }
        let opened = if !inside && page.view.get_untracked() == View::Full {
            page.thoughts_open
        } else {
            page.thoughts_open_quiet
        };
        opened.set(id, opening);
    };
    let words = move || {
        if open.get() {
            view! { <Md id=id text=text code=cell.code class="thinking-text" /> }.into_any()
        } else {
            view! { <span class="first">{first}</span> }.into_any()
        }
    };
    view! {
        <div class="row thinking" class:open=move || open.get() data-id=id data-row=key>
            <span class="mark">{move || (thinking.get() == Some(id)).then(|| view! { <Spinner /> })}</span>
            <div class="thought" role="button" tabindex="0"
                aria-expanded=move || open.get().to_string()
                on:click=move |event| toggle(&event)
                on:keydown=move |event| {
                    if matches!(event.key().as_str(), "Enter" | " ") {
                        event.prevent_default();
                        toggle(&event);
                    }
                }>
                {words}
            </div>
        </div>
    }
    .into_any()
}

thread_local! {
    /// What each Quiet row shows, by its first call, which the row reads
    /// and a new layout updates ([`sync_quiet`]).
    static QUIET: std::cell::RefCell<HashMap<u64, ArcRwSignal<layout::QuietRow>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Gives each Quiet row of `rows` what it shows as laid out now, and lets
/// go of those of rows no longer shown.
fn sync_quiet(rows: &[Row]) {
    QUIET.with(|quiet| {
        let mut quiet = quiet.borrow_mut();
        let mut shown = HashSet::new();
        for row in rows {
            if let Row::Quiet { id, row } = row {
                shown.insert(*id);
                if let Some(signal) = quiet.get(id)
                    && signal.with_untracked(|old| old != row)
                {
                    signal.set(row.clone());
                }
            }
        }
        quiet.retain(|id, _| shown.contains(id));
    });
}

fn local_time(at: i64) -> String {
    js_sys::Date::new(&(at as f64).into())
        .to_locale_time_string("default")
        .as_string()
        .unwrap_or_default()
}

fn item_row(id: u64, key: String, cell: RowCell, thinking: Memo<Option<u64>>) -> AnyView {
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
            // Its images and files, then its words, and under it its tags
            // and Copy (when it has words).
            let worded = move || !text.with(|text| text.trim().is_empty());
            view! {
                <div class="bubble"><Thumbnails data=data /><SentFiles data=data />{move || text.get()}</div>
                <div class="actions">
                    {move || tags().map(|tags| view! { <span class="tag">{tags}</span> })}
                    <Show when=worded>
                        <CopyButton label="Copy message" text=text />
                    </Show>
                </div>
            }
            .into_any()
        }
        Kind::Text => view! { <Md id=id text=text code=cell.code /> }.into_any(),
        Kind::Thinking => return thought_row(id, key, cell, thinking, false),
        Kind::Message => {
            let from = item.from.clone();
            let agent = item.agent.clone();
            // Who it's from and for, as the user reads it ("main to r12"). A
            // worker's message to the user names the worker outside its own
            // view.
            let label = move || match &from {
                Some(from) => crate::quiet::mail_label(from, &agent),
                None if agent != MAIN && page.worker.get().is_none() => {
                    crate::quiet::mail_label(&agent, "you")
                }
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
        Kind::Result => return result_row(id, key, data),
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
        <div class=class data-id=id data-row=key title=title>
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

/// A worker's report or message to its lead in its lead's view, as the
/// terminal's row says it ("r1 reported", "r1 messaged the lead"), with how
/// a message's call stands (the spinner while it runs, ✗ when it failed,
/// the stop that cut it short). It opens to its words, the report's
/// Markdown and the message as its call's content has it, once there are
/// any.
fn status_row(id: u64, key: String, cell: RowCell) -> AnyView {
    let data = cell.data;
    let agent = data.with_untracked(|item| item.agent.clone());
    // The worker's lead, whose view it shows in.
    let lead = {
        let agent = agent.clone();
        move || {
            track_worker(&agent);
            STORE.with(|store| {
                store
                    .borrow()
                    .workers
                    .get(&agent)
                    .and_then(|worker| worker.parent.clone())
                    .unwrap_or_else(|| MAIN.to_string())
            })
        }
    };
    let row = Memo::new(move |_| {
        let lead = lead();
        data.with(|item| {
            crate::quiet::WorkerRow::of(
                item.kind,
                item.act.as_ref(),
                item.status,
                item.outcome.as_deref(),
                &lead,
            )
            .map(|row| (row, row.words(item.outcome.as_deref())))
        })
    });
    // The message's content, once its result came.
    let result = Memo::new(move |_| crate::app::content_of(id));
    let content = move || Some(crate::app::cell(result.get()?)?.for_row().data);
    let report = move || matches!(row.get(), Some((crate::quiet::WorkerRow::Reported, _)));
    let opens = move || {
        if report() {
            cell.text.with(|text| !text.trim().is_empty())
        } else {
            result.get().is_some()
        }
    };
    let words = move || {
        if report() {
            view! { <Md id=id text=cell.text code=cell.code /> }.into_any()
        } else {
            content().map(contents::content_view).into_any()
        }
    };
    let said = move || row.get().map(|(_, words)| words).unwrap_or_default();
    let class = move || {
        let said = said();
        let mut class = "row call status".to_string();
        if said.failed {
            class.push_str(" has-failure");
        }
        if said.muted {
            class.push_str(" muted");
        }
        if !opens() {
            class.push_str(" still");
        }
        class
    };
    view! {
        <div class=class data-id=id data-row=key>
            <details class="call">
                <summary
                    tabindex=move || if opens() { "0" } else { "-1" }
                    on:click=move |event| {
                        if !opens() {
                            event.prevent_default();
                        }
                    }
                >
                    <span class="call-line">
                        <span class="mark">{move || if opens() { "▸" } else { "" }}</span>
                        <span class="who">{agent}</span>
                        <span class="what">{move || said().text}</span>
                        {move || said().note.map(|note| view! { <span class="outcome">{note}</span> })}
                        {move || said().running.then(|| view! {
                            <span class="outcome"><Spinner /></span>
                        })}
                        {move || said().failed.then(|| view! {
                            <span class="outcome right"><span class="failed">"✗"</span></span>
                        })}
                    </span>
                </summary>
                <div class="holds">{words}</div>
            </details>
        </div>
    }
    .into_any()
}

/// Whether a call shows its content in Full, which a command does from the
/// start, as the reader left it.
fn full_open(page: Page, id: u64, command: bool) -> bool {
    command != page.flipped.contains(id)
}

/// Whether `item` is a command's call, whose content shows in Full from the
/// start.
fn is_command(item: &ItemData) -> bool {
    item.act
        .as_ref()
        .is_some_and(|act| act.kind == ActKind::Command)
}

/// A call's line, as the terminal's Full line: its fold's triangle (`mark`),
/// whose it is (a worker's in the main view), what it does, ✓ or ✗ and its
/// outcome, and the spinner, Move to background or Stop while those apply,
/// except in a Quiet row (`in_row`), whose own line has them. Its actions
/// follow the status only while the call runs and the jobs only while it
/// has one, so the many settled calls of a long session follow neither.
fn call_line(
    data: RwSignal<ItemData>,
    mark: impl Fn() -> &'static str + Clone + Send + Sync + 'static,
    in_row: bool,
) -> impl IntoView {
    let page = page();
    let id = data.with_untracked(|item| item.id);
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
    move || {
        data.with(|item| {
            let who = (item.agent != MAIN && page.worker.get_untracked().is_none())
                .then(|| item.agent.clone());
            let mark = mark.clone();
            let outcome = match item.status {
                Some(CallStatus::Running) if in_row => None,
                Some(CallStatus::Running) => Some(
                    view! { <span class="outcome"><Spinner /></span> }
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
                    <span class="mark">{mark}</span>
                    {who.map(|who| view! { <span class="who">{who}</span> })}
                    <span class="what" title=item.text.clone()>{item.text.clone()}</span>
                    {outcome}
                    {move || {
                        let (moves, stop) = if in_row { (false, None) } else { actions.get() };
                        view! {
                            {moves.then(|| view! { <MoveButton id=id /> })}
                            {stop.map(|job| view! { <StopJobButton job=job /> })}
                        }
                    }}
                </span>
            }
        })
    }
}

/// A call's row in Full, its line, which opens to its detail and its
/// content (its result's row, under it) when it has them. A command's is
/// open from the start. The images its tool returned show under it, open
/// or not.
fn call_row(cell: RowCell) -> AnyView {
    let page = page();
    let data = cell.data;
    let (id, command) = data.with_untracked(|item| (item.id, is_command(item)));
    let content = Memo::new(move |_| crate::app::content_of(id).is_some());
    let summary = NodeRef::<leptos::html::Summary>::new();
    let cut = cut_words(summary, move || data.with(|item| item.text.clone()));
    // (A worker's call in the main view shows without its content. A row
    // is drawn for one view.)
    let folds = move || {
        let own = data.with(|item| item.agent == MAIN) || page.worker.get_untracked().is_some();
        data.with(|item| item.detail.is_some()) || (own && content.get()) || cut.get()
    };
    let open = move || folds() && full_open(page, id, command);
    // (Its triangle turns as it opens.)
    let mark = move || if folds() { "▸" } else { "" };
    let detail = move || {
        open().then(|| {
            let whole = cut
                .get()
                .then(|| view! { <pre class="held">{data.with(|item| item.text.clone())}</pre> });
            let detail = data
                .with(|item| item.detail.clone())
                .map(|detail| view! { <pre class="held">{detail}</pre> });
            (cut.get() || detail.is_some())
                .then(|| view! { <div class="holds">{whole}{detail}</div> })
        })
    };
    view! {
        <details class="call" prop:open=open>
            <summary
                node_ref=summary
                tabindex=move || if folds() { "0" } else { "-1" }
                on:click=move |event| {
                    event.prevent_default();
                    if open() {
                        page.flipped.toggle(id);
                        return;
                    }
                    let cuts = handler_element(&event)
                        .is_some_and(|summary| overflows(&summary, ".what"));
                    cut.set(cuts);
                    if folds() && !full_open(page, id, command) {
                        page.flipped.toggle(id);
                    }
                }
            >
                {call_line(data, mark, false)}
            </summary>
            {detail}
        </details>
        <Thumbnails data=data />
    }
    .into_any()
}

/// A call's content in Full, under the call, while the call is open. One
/// whose call is gone shows as it is.
fn result_row(id: u64, key: String, data: RwSignal<ItemData>) -> AnyView {
    let page = page();
    let call = data.with_untracked(|item| item.call);
    let command =
        call.and_then(|call| STORE.with(|store| store.borrow().get(call).map(is_command)));
    let shown = move || match (call, command) {
        (Some(call), Some(command)) => full_open(page, call, command),
        _ => true,
    };
    view! {
        <div class="row result" data-id=id data-row=key style:display=move || if shown() { "" } else { "none" }>
            <div class="holds">{move || shown().then(|| contents::content_view(data))}</div>
        </div>
    }
    .into_any()
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
    /// Where the reader was in the main transcript, to come back to, and the
    /// feed it's a place among the rows of.
    static MAIN_PLACE: std::cell::Cell<Option<(Option<u64>, (scroll::Anchor, f64))>> =
        const { std::cell::Cell::new(None) };
}

/// Where the reader was in the main transcript of `feed`, taken to go back
/// to. A place of another feed is among rows that are gone.
fn take_main_place(feed: Option<u64>) -> Option<(scroll::Anchor, f64)> {
    MAIN_PLACE
        .with(|place| place.take())
        .filter(|(of, _)| *of == feed)
        .map(|(_, place)| place)
}

/// Shows the worker `id`'s own transcript (none for the main one) in a new
/// entry of the browser's history, so Back and Forward step through the
/// views the reader saw.
pub fn show_worker(page: Page, id: Option<String>) {
    if page.worker.get_untracked() == id {
        return;
    }
    let entry = Entry {
        feed: id.as_ref().and_then(|_| feed()),
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
/// worker of another feed, or one gone, shows the main transcript, and the
/// entry says so from then on.
pub fn show_history_entry(page: Page, state: &wasm_bindgen::JsValue) {
    let text = |name: &str| {
        js_sys::Reflect::get(state, &name.into())
            .ok()
            .and_then(|value| value.as_string())
    };
    let entry = Entry {
        worker: text("worker"),
        feed: text("feed").as_deref().and_then(crate::store::parse_feed),
    };
    let shown = STORE.with(|store| store.borrow().entry_view(&entry));
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
    if let Some(feed) = entry.feed {
        let _ = js_sys::Reflect::set(&state, &"feed".into(), &format!("{feed:016x}").into());
    }
    state.into()
}

/// Shows the worker `id`'s own transcript (none for the main one). A
/// worker's opens at its latest, and coming back to the main one returns
/// to where the reader was.
fn switch_view(page: Page, id: Option<String>) {
    if page.worker.get_untracked().is_none() {
        let place = (!page.pinned.get_untracked())
            .then(scroll::first_visible)
            .flatten();
        MAIN_PLACE.with(|saved| saved.set(place.map(|place| (feed(), place))));
    }
    let back_to = if id.is_none() {
        take_main_place(feed())
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

/// Markdown that may still stream. Its finished blocks stay as they are,
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
    let streaming = crate::app::streams(id);
    let mut blocks = Blocks::default();
    let (done, open) = text.with_untracked(|text| blocks.update(text, streaming.get_untracked()));
    // Each finished block, its place, how often it was painted, its HTML.
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

    /// A thought is its first line in the words' column, with no label or
    /// marker, and "…" where more follows. While it streams, the spinner
    /// turns in the markers' column.
    #[test]
    fn a_thought_is_its_first_line_with_no_label() {
        use crate::store::tests::item;
        let owner = Owner::new();
        owner.with(|| {
            let _page = crate::testing::page();
            let cell = || RowCell {
                data: RwSignal::new(item(1, Kind::Thinking, MAIN, "")),
                text: RwSignal::new("**Planning**\nRead the tests first.".into()),
                code: RwSignal::new(0),
            };
            let html = thought_row(1, "i1".into(), cell(), Memo::new(|_| None), false).to_html();
            assert!(html.contains(">Planning …<"), "{html}");
            for label in ["Thought", "Thinking", "▸", "spinner"] {
                assert!(!html.contains(label), "{label}: {html}");
            }
            let live = thought_row(1, "i1".into(), cell(), Memo::new(|_| Some(1)), false).to_html();
            assert!(
                live.contains("<span class=\"mark\"><span class=\"spinner\">"),
                "{live}"
            );
        });
    }

    /// Another feed's rows are other rows, whatever their items' IDs.
    #[test]
    fn a_rows_key_names_its_feed() {
        let row = Row::Item(4, Kind::Text);
        assert_eq!(row_key(Some(1), &row), row_key(Some(1), &row));
        assert_ne!(row_key(Some(1), &row), row_key(Some(2), &row));
    }

    /// The reader comes back to their place in the main transcript of the
    /// feed it was in, and a new feed's opens at its latest.
    #[test]
    fn a_place_of_another_feed_isnt_gone_back_to() {
        let at = || scroll::Anchor {
            id: 7,
            key: "i7".into(),
        };
        MAIN_PLACE.with(|place| place.set(Some((Some(1), (at(), -3.5)))));
        assert_eq!(take_main_place(Some(1)), Some((at(), -3.5)));
        assert_eq!(take_main_place(Some(1)), None);
        MAIN_PLACE.with(|place| place.set(Some((Some(1), (at(), -3.5)))));
        assert_eq!(take_main_place(Some(2)), None);
    }

    /// A text that becomes the reply (Arko's `change`, with no new item) is
    /// laid out again in the Full view, and its row's key changes, so it's
    /// drawn again as a reply. More text alone is drawn in place.
    #[test]
    fn a_text_that_becomes_a_reply_is_laid_out_again_in_full() {
        use crate::app::{take_change, take_reset, tests::reset_of};
        use crate::store::tests::item;
        let owner = Owner::new();
        owner.with(|| {
            let page = crate::testing::page();
            let rows = laid_out(page.version, page.view, page.worker);
            let keys = move || {
                rows.with(|(feed, rows)| {
                    rows.iter()
                        .map(|row| row_key(*feed, row))
                        .collect::<Vec<_>>()
                })
            };
            let text = item(4, Kind::Text, MAIN, "Done.");
            let items = vec![item(3, Kind::User, MAIN, "go"), text.clone()];
            take_reset(page, reset_of(1, items, Vec::new()), 0.0);
            let before = keys();

            let mut longer = text;
            longer.text.push_str(" Mostly.");
            let version = page.version.get_untracked();
            take_change(page, longer.clone(), false, 1.0);
            assert_eq!(page.version.get_untracked(), version);
            assert_eq!(keys(), before);
            let reply = ItemData {
                kind: Kind::Reply,
                ..longer
            };
            take_change(page, reply, false, 2.0);
            let after = keys();
            assert_eq!(after[0], before[0]);
            assert_ne!(after[1], before[1]);
        });
    }

    /// A worker's report shows in its lead's view, so it moves to the view of
    /// the worker that turns out to have started it. A summary that names no
    /// other lead leaves the rows as they were.
    #[test]
    fn a_report_follows_its_workers_lead() {
        use crate::app::{take_reset, tests::reset_of};
        use crate::store::tests::{item, worker};
        let owner = Owner::new();
        owner.with(|| {
            let page = crate::testing::page();
            let rows = laid_out(page.version, page.view, page.worker);
            let keys = move || rows.with(|(_, rows)| rows.iter().map(Row::key).collect::<Vec<_>>());
            let items = vec![
                item(1, Kind::User, MAIN, "go"),
                item(2, Kind::Reply, "r2", "Counted."),
            ];
            take_reset(
                page,
                reset_of(1, items, vec![worker("r1"), worker("r2")]),
                0.0,
            );
            assert_eq!(keys(), ["i1", "s2"]);
            let changed = |parent: Option<&str>| {
                let summary = crate::protocol::WorkerData {
                    parent: parent.map(str::to_string),
                    tool_calls: 3,
                    ..worker("r2")
                };
                STORE.with(|store| store.borrow_mut().worker(summary));
                page.workers.update(|version| *version += 1);
            };
            changed(None);
            assert_eq!(keys(), ["i1", "s2"]);
            changed(Some("r1"));
            assert_eq!(keys(), ["i1"]);
        });
    }

    /// Two workers' counts of as many calls in the same turn are other
    /// rows, so switching between them draws the count of the worker shown,
    /// with its first call to keep the reader's place by.
    #[test]
    fn equal_counts_of_two_workers_are_other_rows() {
        use crate::app::{take_reset, tests::reset_of};
        use crate::store::tests::item;
        let owner = Owner::new();
        owner.with(|| {
            let page = crate::testing::page();
            let rows = laid_out(page.version, page.view, page.worker);
            let counts = move || {
                rows.with(|(feed, rows)| {
                    rows.iter()
                        .filter(|row| matches!(row, Row::Count { .. }))
                        .map(|row| row_key(*feed, row))
                        .collect::<Vec<_>>()
                })
            };
            let call = |id, agent| {
                let mut call = item(id, Kind::Call, agent, "cat");
                call.status = Some(CallStatus::Ok);
                call.turn = Some(1);
                call
            };
            let items = vec![
                item(1, Kind::User, MAIN, "go"),
                call(2, "r1"),
                call(3, "r2"),
            ];
            take_reset(page, reset_of(1, items, Vec::new()), 0.0);
            page.view.set(View::Messages);
            page.worker.set(Some("r1".into()));
            let first = counts();
            page.worker.set(Some("r2".into()));
            let second = counts();
            assert_eq!((first.len(), second.len()), (1, 1));
            assert_ne!(first, second);
        });
    }

    /// A Quiet row keeps its key while calls join it and its words change,
    /// so the page keeps its element, its focus and what's open inside it.
    #[test]
    fn a_quiet_rows_key_is_its_first_call() {
        let row = |text: &str, calls: Vec<Row>| Row::Quiet {
            id: 2,
            row: layout::QuietRow {
                words: crate::quiet::Words {
                    text: text.into(),
                    ..Default::default()
                },
                calls,
                merged: false,
                opens: true,
            },
        };
        assert_eq!(
            row_key(
                Some(1),
                &row("Made 1 edit to app.rs", vec![Row::Item(2, Kind::Call)])
            ),
            row_key(
                Some(1),
                &row(
                    "Made 2 edits to app.rs",
                    vec![Row::Item(2, Kind::Call), Row::Item(3, Kind::Call)]
                )
            )
        );
    }

    /// Switching the view while a worker is shown lays its transcript out
    /// again in that view at once.
    #[test]
    fn switching_the_view_changes_a_workers_transcript_at_once() {
        use crate::app::{take_reset, tests::reset_of};
        use crate::store::tests::item;
        let owner = Owner::new();
        owner.with(|| {
            let page = crate::testing::page();
            let rows = laid_out(page.version, page.view, page.worker);
            let kinds = move || {
                rows.with(|(_, rows)| {
                    rows.iter()
                        .map(|row| match row {
                            Row::Item(_, kind) => format!("{kind:?}"),
                            Row::Count { count, .. } => format!("{} calls", count.calls),
                            Row::Quiet { row, .. } => row.words.text.clone(),
                            Row::Status(id) => format!("status {id}"),
                            Row::Working => "working".into(),
                        })
                        .collect::<Vec<_>>()
                })
            };
            let mut call = item(3, Kind::Call, "r1", "cat");
            call.status = Some(CallStatus::Ok);
            let items = vec![
                item(1, Kind::User, MAIN, "go"),
                item(2, Kind::Thinking, "r1", "hmm"),
                call,
                item(4, Kind::Text, "r1", "Done."),
            ];
            take_reset(page, reset_of(1, items, Vec::new()), 0.0);
            page.worker.set(Some("r1".into()));
            assert_eq!(kinds(), ["Thinking", "Call", "Text"]);
            page.view.set(View::Messages);
            assert_eq!(kinds(), ["1 calls", "Text"]);
            page.view.set(View::Quiet);
            assert_eq!(kinds(), ["Thinking", "Call", "Text"]);
        });
    }

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
