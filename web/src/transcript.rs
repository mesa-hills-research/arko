//! The transcript, with its rows laid out for the view, and the marker of
//! what came below it.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use lucide_leptos::{ArrowDown, ChevronRight};

use crate::app::{Page, RowCell, STORE, cell, feed, page, post, track_worker};
use crate::copy::CopyButton;
use crate::files::SentFiles;
use crate::images::Thumbnails;
use crate::layout::{self, Count, Row, View};
use crate::markdown::{self, Blocks};
use crate::protocol::{
    ActKind, CallStatus, ContentKind, Delivery, FileLine, ItemData, Kind, Level, Source,
    WorkerState,
};
use crate::shimmer::Shimmer;
use crate::spinner::Spinner;
use crate::store::{Entry, MAIN, call_actions, kept_actions};
use crate::{contents, fit, quiet, scroll, words};

/// The marker of a row or line with nothing to open, running or not, as the
/// terminal draws it. One that opens has a triangle.
const STILL: &str = "·";

#[component]
pub fn Transcript() -> impl IntoView {
    let page = page();
    let log: NodeRef<leptos::html::Main> = NodeRef::new();
    log.on_load(move |log| scroll::attach(log.into(), page.scroll, page.width));
    let laid_out = laid_out(page.version, page.view, page.worker);
    // The rows with the Working row where it stands ([`layout::live_line`]),
    // and the thinking that streams.
    let live = Memo::new(move |_| {
        let (feed, rows) = laid_out.get();
        page.version.track();
        page.workers.track();
        page.now.track();
        let view = page.view.get();
        let worker = page.worker.get();
        let (thinking, shown) = if page.connection.get() == words::Connection::Live {
            page.status.with(|status| {
                STORE.with(|store| {
                    let store = store.borrow();
                    let now = js_sys::Date::now();
                    layout::live_line(&rows, &store, status, view, worker.as_deref(), now)
                })
            })
        } else {
            (None, false)
        };
        let rows = layout::with_working(rows, shown);
        (feed, rows, thinking)
    });
    let rows = Memo::new(move |_| live.with(|(feed, rows, _)| (*feed, rows.clone())));
    let thinking = Memo::new(move |_| live.with(|(_, _, thinking)| *thinking));
    // The space around each row, from the shared table, which the rows read
    // ([`Spaced`]). A call's content that its closed call hides takes none.
    // The lines on an open Quiet row's rail are spaced by it too.
    let gaps = Memo::new(move |_| {
        rows.with(|(_, rows)| {
            STORE.with(|store| {
                let store = store.borrow();
                let mut gaps = layout::gaps(rows, |row| match row {
                    Row::Item(id, Kind::Result) => {
                        let call = store.get(*id).and_then(|item| item.call);
                        let command = call.and_then(|call| store.get(call).map(is_command));
                        match (call, command) {
                            (Some(call), Some(command)) => full_open(page, call, command),
                            _ => true,
                        }
                    }
                    _ => true,
                });
                for row in rows {
                    if let Row::Quiet { row, .. } = row {
                        let members: Vec<Row> = layout::quiet_members(&row.calls)
                            .into_iter()
                            .map(|(member, _)| member)
                            .collect();
                        gaps.extend(layout::rail_gaps(&members));
                    }
                }
                gaps
            })
        })
    });
    GAPS.with(|cell| cell.set(Some(gaps)));
    // Quiet rows that stay follow their words and calls.
    Effect::new(move |_| rows.with(|(_, rows)| sync_quiet(rows)));
    let marker = page.scroll.marker;
    let empty = move || {
        if !rows.with(|(_, rows)| rows.is_empty())
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
            <main id="log" class="log" class:following=move || page.scroll.follows.get() aria-label="Transcript" node_ref=log>
                // (The note of a view with no rows is in the rows, where the
                // page measures what the view holds.)
                <div id="rows" class="column rows">
                    <For
                        each=move || {
                            let (feed, rows) = rows.get();
                            rows.into_iter().map(move |row| (feed, row))
                        }
                        key=|(feed, row)| row_key(*feed, row)
                        children=move |(_, row)| row_view(row, thinking)
                    />
                    {move || empty().map(|text| view! { <p class="empty">{text}</p> })}
                </div>
            </main>
            <button
                type="button"
                class="jump"
                class:shown=move || marker.get().is_some()
                tabindex=move || if marker.get().is_some() { "0" } else { "-1" }
                on:click=move |event| scroll::to_end_at(event.time_stamp())
            >
                <ArrowDown size=16 />
                {move || marker.get().map(scroll::marker_words)}
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

/// A row of the transcript. Each row says its item (`data-id`) and its own
/// key among the rows shown (`data-row`, [`Row::key`]), which the reader's
/// place keeps to ([`scroll::Anchor`]).
fn row_view(row: Row, thinking: Memo<Option<u64>>) -> AnyView {
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
            let spaced = Spaced::of(&key);
            view! {
                <div class="row working" data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
                    <span class="mark"><Spinner /></span>
                    <Shimmer class="what" text=move || label.get() active=|| true />
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
    let spaced = Spaced::of(&key);
    let Count {
        calls,
        failed,
        running,
    } = count;
    let open = move || page.counts_open.with(|open| open.contains(&id));
    view! {
        <div class="row count" data-id=id data-row=key data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
            <button type="button" class="call-line" aria-expanded=move || open().to_string()
                on:click=move |_| {
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

/// Whether a line's words are longer than the 8 characters they keep when
/// what follows them takes the rest of the line, so they give way down to
/// those 8 before anything else does.
fn long_words(words: &str) -> bool {
    words.chars().count() > 8
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

/// Whether what the call `item` opens to, `detail` and its content (once
/// it came), holds its call as written ([`quiet::holds_call`]).
fn holds_call(item: &ItemData, detail: Option<&str>) -> bool {
    let content = crate::app::content_of(item.id)
        .and_then(crate::app::cell)
        .map(|cell| cell.text.get());
    let held: Vec<&str> = detail.into_iter().chain(content.as_deref()).collect();
    quiet::holds_call(&item.text, &held)
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
    let spaced = Spaced::of(&key);
    let shared = QUIET.with(|quiet| {
        quiet
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| ArcRwSignal::new(row.clone()))
            .clone()
    });
    let data = RwSignal::from(&shared);
    let words = move || data.with(|row| row.words.clone());
    // Its words and what follows them fitted to the room its line leaves
    // them, which Move to background and Stop take from as they come.
    let line = NodeRef::<leptos::html::Span>::new();
    let room = fit::watch(line, move || {
        page.movable.track();
        page.running_jobs.track();
        data.track();
    });
    let fitted = Memo::new(move |_| room.get().map(|room| quiet_fit(&words(), room.now)));
    // It opens to what its line as drawn left out, its words and its news or
    // note. (How it runs only takes the room its words leave, so it doesn't
    // count, nor show in what the row opens to, as it changes with every
    // call.)
    let steady_words = move || quiet::Words {
        live: Vec::new(),
        ..words()
    };
    let settled = move || {
        data.with(|row| {
            usize::from(row.words.news.is_some()) + usize::from(row.words.note.is_some())
        })
    };
    let left_out =
        move || fitted.with(|fitted| fitted.as_ref().is_some_and(|fit| fit.parts < settled()));
    let cut = Signal::derive(move || {
        fitted.with(|fitted| fitted.as_ref().is_some_and(|fit| fit.words_cut)) || left_out()
    });
    let opens = move || data.with(|row| row.opens);
    let open = move || (opens() || cut.get()) && page.quiet_open.contains(id);
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
    // Stop while its job or monitor runs), whatever row it's in, each in the
    // room its state keeps for it from the start.
    let actions = move || {
        members()
            .into_iter()
            .filter_map(cell)
            .map(|cell| {
                let offered = cell.data.with(|item| {
                    call_actions(
                        item,
                        |id| page.movable.with(|movable| movable.contains(&id)),
                        |job| {
                            page.running_jobs
                                .with(|running| running.iter().any(|running| running == job))
                        },
                    )
                });
                let kept = cell.data.with(kept_actions);
                let call = cell.data.with_untracked(|item| item.id);
                action_buttons(call, offered, kept)
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
    // A row of one call opens to what its line left out, each once. Where
    // the line cut its words, the call as written, unless its detail or
    // content holds it, and where it left out its news or note, them, after
    // its words whole where it cut those too.
    let whole = move || {
        if data.with(|row| row.merged) {
            return None;
        }
        let (words_cut, parts) = fitted.with(|fitted| {
            fitted
                .as_ref()
                .map(|fit| (fit.words_cut, fit.parts.min(settled())))
        })?;
        let cell = members().first().copied().and_then(cell);
        let words_cut = words_cut
            && !cell.as_ref().is_some_and(|cell| {
                cell.data
                    .with(|item| holds_call(item, item.act.as_ref().map(|act| act.detail.as_str())))
            });
        if left_out() {
            let steady = steady_words();
            let text = steady.left_out(words_cut.then_some(steady.text.as_str()), parts)?;
            return Some(view! { <pre class="held">{text}</pre> }.into_any());
        }
        words_cut.then_some(cell).flatten().map(|cell| {
            // (A code run's words whole, which its call as written only
            // repeats with the tool's name.)
            let code = cell.data.with(|item| {
                item.act
                    .as_ref()
                    .is_some_and(|act| act.kind == ActKind::Code)
            });
            let text = match code {
                true => words().text,
                false => cell.data.with(|item| item.text.clone()),
            };
            view! { <pre class="held">{text}</pre> }.into_any()
        })
    };
    // (The call as written is the first of what its one call holds, so it
    // goes in that call's place on the rail.)
    let inside = move || {
        open().then(|| {
            view! {
                <For
                    each=move || data.with(|row| layout::quiet_members(&row.calls))
                    key=|(row, results)| {
                        (row_key(None, row), results.iter().map(Row::key).collect::<Vec<_>>())
                    }
                    children=move |(row, results)| quiet_member(row, results, data, thinking, whole)
                />
            }
        })
    };
    view! {
        <div class=class data-id=id data-row=key data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
            <details class="call quiet" prop:open=open>
                <summary
                    tabindex=move || if opens() || cut.get() { "0" } else { "-1" }
                    on:click=move |event| {
                        event.prevent_default();
                        let opening = !page.quiet_open.contains_untracked(id);
                        if opening && !opens() && !cut.get_untracked() {
                            return;
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
                    <span class="call-line" node_ref=line>
                        // (Its triangle turns as it opens. A row with nothing
                        // to open has a dot.)
                        <span class="mark">{move || if opens() || cut.get() { "▸" } else { STILL }}</span>
                        // (What follows its words gives way whole, the last
                        // first, by the rule the terminal's line follows.)
                        {move || {
                            let words = words();
                            let fitted = fitted.get();
                            let shown = fitted.as_ref().map_or(usize::MAX, |fitted| fitted.parts);
                            let text = fitted.map_or_else(|| words.text.clone(), |fitted| fitted.words);
                            let mut parts = 0;
                            let mut next = || {
                                parts += 1;
                                parts <= shown
                            };
                            let news = words.news.clone().filter(|_| next());
                            let note = words.note.clone().filter(|_| next());
                            let live: Vec<String> = words.live.iter().filter(|_| next()).cloned().collect();
                            view! {
                                <span class="said" class:has-live=room.get().is_none() && !words.live.is_empty() class:fitted=room.get().is_some()>
                                    <span
                                        class="what"
                                        class:long=room.get().is_none() && long_words(&words.text)
                                        title=steady_words().said()
                                    >
                                        {text}
                                    </span>
                                    {news.map(|news| view! { <span class="news">{format!("({news})")}</span> })}
                                    {note.map(|note| view! { <span class="outcome" title=note.clone()>{note.clone()}</span> })}
                                    {live
                                        .into_iter()
                                        .map(|live| view! { <span class="outcome live" title=live.clone()>{live.clone()}</span> })
                                        .collect_view()}
                                </span>
                            }
                        }}
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

/// The room each part after a Quiet row's words takes on its line with its
/// gap and its dot or parentheses, its news, its note and how it runs, in
/// the order they come ([`quiet::Words`]), each with the columns the words
/// keep before it gives way. (Its news and note say what happened.)
fn quiet_parts(words: &quiet::Words, m: fit::Metrics) -> Vec<(f64, usize)> {
    let mut parts = Vec::new();
    if let Some(news) = &words.news {
        parts.push((fit::GAP + m.text(&format!("({news})")), quiet::WORDS_KEEP));
    }
    if let Some(note) = &words.note {
        parts.push((fit::GAP + m.dot() + m.text(note), quiet::note_keep(words)));
    }
    for live in &words.live {
        parts.push((fit::GAP + m.dot() + m.text(live), quiet::WORDS_KEEP_LIVE));
    }
    parts
}

/// A Quiet row's (or a worker's status row's) words and what follows them
/// fitted to `room` pixels by the rule every row's line follows
/// ([`fit::line`]).
fn quiet_fit(words: &quiet::Words, room: f64) -> fit::Fitted {
    let m = fit::metrics();
    fit::line(&words.text, 0.0, &quiet_parts(words, m), room, m)
}

/// What an open Quiet row shows of one of its calls, on the row's rail.
/// In a row of one call that's the call's detail and content. In a row of
/// several it's the call's Full line, muted, which opens to its detail and
/// content, and to the call as written where the line cut it. A thinking
/// between them shows as it does on its own, in their text column. Anything
/// open has a gap below it (`.open`).
fn quiet_member(
    row: Row,
    results: Vec<Row>,
    quiet: RwSignal<layout::QuietRow>,
    thinking: Memo<Option<u64>>,
    written: impl Fn() -> Option<AnyView> + Copy + Send + Sync + 'static,
) -> AnyView {
    let page = page();
    let Row::Item(id, Kind::Call) = row else {
        return match row {
            Row::Item(id, Kind::Thinking) => match cell(id) {
                Some(cell) => thought_row(id, row.key(), cell.for_row(), thinking, true),
                None => ().into_any(),
            },
            row => row_view(row, thinking),
        };
    };
    let Some(cell) = cell(id) else {
        return ().into_any();
    };
    let spaced = Spaced::of(&row.key());
    let data = cell.for_row().data;
    let merged = move || quiet.with(|row| row.merged);
    let has_content = !results.is_empty();
    let folds = move || has_content || data.with(|item| item.detail.is_some());
    // Its line cut its words or left out its outcome's words.
    let cut = RwSignal::new(LineCut::default());
    let open = move || !merged() || ((folds() || cut.get().cut()) && page.calls_open.contains(id));
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
    // (Where its detail or content holds its call as written, that doesn't
    // show twice.)
    let whole = move || {
        merged()
            .then(|| data.with(|item| shown_whole(item, cut.get())))
            .flatten()
            .map(|whole| view! { <pre class="held">{whole}</pre> })
    };
    let holds = move || {
        open().then(|| {
            view! {
                {move || (!merged()).then(written).flatten()}
                {whole}
                {detail}
                {contents
                    .iter()
                    .map(|data| view! { {contents::content_view(*data)}{run_calls(id, *data, false)} })
                    .collect_view()}
            }
        })
    };
    let line = move || {
        merged().then(|| {
            let mark = move || match (folds() || cut.get().cut(), open()) {
                (false, _) => STILL,
                (true, false) => "▸",
                (true, true) => "▾",
            };
            view! {
                <button type="button" class="member-line"
                    aria-expanded=move || (folds() || cut.get().cut()).then(|| open().to_string())
                    on:click=move |_| {
                        if !page.calls_open.contains_untracked(id)
                            && !folds()
                            && !cut.get_untracked().cut()
                        {
                            return;
                        }
                        page.calls_open.toggle(id);
                    }>
                    {call_line(data, mark, true, Some(cut))}
                </button>
            }
        })
    };
    // (Spaced on the rail by the table, as rows are, [`layout::rail_gaps`].)
    view! {
        <div class="quiet-member" class:own=merged class:open=move || merged() && open()
            data-gap=spaced.gap() data-gap-open=spaced.gap_open()>
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
    let spaced = Spaced::of(&key);
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
        <div class="row thinking" class:open=move || open.get() data-id=id data-row=key data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
            <span class="mark">{move || (thinking.get() == Some(id)).then(|| view! { <Spinner /> })}</span>
            <div class="thought" role="button" tabindex="0"
                aria-expanded=move || open.get().to_string()
                on:click=move |event| toggle(&event)
                on:keydown=move |event| {
                    // (Enter and Space click it, as a button's do, so the
                    // transcript keeps its line in place as for a click.)
                    if matches!(event.key().as_str(), "Enter" | " ") {
                        use wasm_bindgen::JsCast;
                        event.prevent_default();
                        if let Some(thought) = handler_element(&event)
                            .and_then(|thought| thought.dyn_into::<web_sys::HtmlElement>().ok())
                        {
                            thought.click();
                        }
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
    /// The space around each row of the transcript, by its key
    /// ([`layout::gaps`]).
    static GAPS: std::cell::Cell<Option<Memo<HashMap<String, layout::Gaps>>>> =
        const { std::cell::Cell::new(None) };
}

/// The space around a row of the transcript, as its attributes say it to
/// the page's style: the step above it (`data-gap`), the step above it when
/// the row before is open (`data-gap-open`), and on the last row the step
/// below it, closed and open (`data-end`, `data-end-open`). The style picks
/// by what shows open, and a row inside another has none.
#[derive(Clone, Copy)]
struct Spaced(Memo<Option<layout::Gaps>>);

impl Spaced {
    fn of(key: &str) -> Self {
        let gaps = GAPS.with(std::cell::Cell::get);
        let key = key.to_string();
        Spaced(Memo::new(move |_| {
            gaps.and_then(|gaps| gaps.with(|gaps| gaps.get(&key).copied()))
        }))
    }

    fn gap(self) -> impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static {
        move || self.0.get().map(|gaps| gaps.above.name())
    }

    fn gap_open(self) -> impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static {
        move || self.0.get().map(|gaps| gaps.above_open.name())
    }

    fn end(self) -> impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static {
        move || self.0.get()?.below.map(|(closed, _)| closed.name())
    }

    fn end_open(self) -> impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static {
        move || self.0.get()?.below.map(|(_, open)| open.name())
    }
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
    let spaced = Spaced::of(&key);
    let item = cell.data.get_untracked();
    let data = cell.data;
    let text = cell.text;
    let row = NodeRef::<leptos::html::Div>::new();
    // A note whose only tag says it waits ("pending") says so beside its
    // bubble where there's room ([`beside_room`]).
    let beside = RwSignal::new(false);
    let class = move || {
        data.with(|item| {
            let mut class = format!("row {}", kind_class(item.kind));
            if beside.get() {
                class.push_str(" beside");
            }
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
            // Its tags, as they read now, or (`pending`) while it waited.
            let tags_of = move |pending: bool| {
                data.with(|item| {
                    let mut tags = Vec::new();
                    if let Some(tag) = words::delivery_tag(item.delivery, pending) {
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
            // A note the agent read after it waited keeps the room its tags
            // had then, under the ones it has now, so its row keeps its
            // height.
            let tags = move || {
                let now = tags_of(data.with(|item| item.pending));
                match data.with(|item| item.waited_as.is_some()).then(|| tags_of(true)).flatten() {
                    Some(then) => Some(view! {
                        <span class="tag kept">
                            <span class="room" aria-hidden="true">{then}</span>
                            <span>{now}</span>
                        </span>
                    }
                    .into_any()),
                    None => now.map(|tags| view! { <span class="tag">{tags}</span> }.into_any()),
                }
            };
            // (Only "pending", which goes once the agent reads it, on words
            // alone, since images and files take their room as they load.)
            let lone = move || {
                data.with(|item| {
                    item.delivery == Some(Delivery::Note)
                        && (item.pending || item.waited_as.is_some())
                        && item.images.is_empty()
                        && item.files.is_empty()
                }) && tags_of(false).is_none()
            };
            // (Decided as soon as the row is in the page, before the frame
            // that settles the view, which then lays out the end with the
            // tag where it stands, so no blank is left under the end.)
            Effect::new(move |_| {
                page.width.track();
                text.track();
                let lone = lone();
                let decide = move || {
                    let room = row.get_untracked().is_some_and(|row| beside_room(&row));
                    beside.set(lone && room);
                };
                match row.get_untracked().is_some_and(|row| row.is_connected()) {
                    true => decide(),
                    false => crate::frame::next_frame(decide),
                }
            });
            // Its images and files, then its words, and under it its tags
            // and Copy (when it has words).
            let worded = move || !text.with(|text| text.trim().is_empty());
            view! {
                <div class="bubble"><Thumbnails data=data /><SentFiles data=data />{move || text.get()}</div>
                <div class="actions">
                    {tags}
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
            // (The line that opens a turn for the workers' news says only
            // what their rows here don't.)
            let said = move || {
                data.with(|item| item.beside_rows.clone().filter(|_| item.opens_turn))
                    .unwrap_or_else(|| text.get())
            };
            view! { {label}<span class="text">{said}</span> }.into_any()
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
        <div node_ref=row class=class data-id=id data-row=key title=title data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
            {body}
            {more}
        </div>
    }
    .into_any()
}

/// Whether the user's message `row` has room left of its bubble for its
/// tag, with its bubble as wide as its words want, unwrapped, and Copy
/// beside it, so the tag can stand there whatever the bubble's state.
fn beside_room(row: &web_sys::HtmlDivElement) -> bool {
    use wasm_bindgen::JsCast;
    let find = |selector: &str| {
        row.query_selector(selector)
            .ok()
            .flatten()
            .and_then(|found| found.dyn_into::<web_sys::HtmlElement>().ok())
    };
    let (Some(bubble), Some(tag)) = (find(":scope > .bubble"), find(":scope > .actions .tag"))
    else {
        return false;
    };
    // (Neither shrunk beside Copy nor held to its width as drawn.)
    let style = bubble.style();
    let _ = style.set_property("flex", "none");
    let _ = style.set_property("width", "max-content");
    let _ = style.set_property("max-width", "none");
    let wanted = bubble.get_bounding_client_rect().width();
    let _ = style.remove_property("flex");
    let _ = style.remove_property("width");
    let _ = style.remove_property("max-width");
    let copy = find(":scope > .actions .btn")
        .map_or(0.0, |copy| copy.get_bounding_client_rect().width() + 4.0);
    let width = row.get_bounding_client_rect().width();
    wanted + copy + 4.0 + tag.scroll_width() as f64 <= width
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
    let spaced = Spaced::of(&key);
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
    let said = move || row.get().map(|(_, words)| words).unwrap_or_default();
    // Its words and note fitted to the room its line leaves them. Where
    // they cut its words or left out its note, it opens to them whole.
    let line = NodeRef::<leptos::html::Span>::new();
    let room = fit::watch(line, move || row.track());
    // (As its line is drawn, its spinner in the room it keeps while the
    // message's call runs.)
    let fitted = Memo::new(move |_| room.get().map(|room| quiet_fit(&said(), room.now)));
    let cut = move || fitted.with(|fitted| fitted.as_ref().is_some_and(|fitted| fitted.cut));
    let opens = move || {
        cut()
            || if report() {
                cell.text.with(|text| !text.trim().is_empty())
            } else {
                result.get().is_some()
            }
    };
    let words = move || {
        // (What its line left out, each once, its words whole where it cut
        // them and its note where it left it out.)
        let whole = fitted
            .get()
            .filter(|fitted| fitted.cut)
            .and_then(|fitted| {
                let said = said();
                said.left_out(fitted.words_cut.then_some(said.text.as_str()), fitted.parts)
            })
            .map(|text| view! { <pre class="held">{text}</pre> });
        let body = if report() {
            view! { <Md id=id text=cell.text code=cell.code /> }.into_any()
        } else {
            content().map(contents::content_view).into_any()
        };
        view! { {whole}{body} }
    };
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
        <div class=class data-id=id data-row=key data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open()>
            <details class="call">
                <summary
                    tabindex=move || if opens() { "0" } else { "-1" }
                    on:click=move |event| {
                        if !opens() {
                            event.prevent_default();
                        }
                    }
                >
                    <span class="call-line" node_ref=line>
                        <span class="mark">{move || if opens() { "▸" } else { STILL }}</span>
                        <span class="who">{agent}</span>
                        <span class="said" class:fitted=move || room.get().is_some()>
                            <span class="what" class:long=move || room.get().is_none() && long_words(&said().text) title=move || said().said()>
                                {move || fitted.get().map_or_else(|| said().text, |fitted| fitted.words)}
                            </span>
                            {move || {
                                let shows = fitted.get().is_none_or(|fitted| fitted.parts > 0);
                                said().note.filter(|_| shows).map(|note| view! { <span class="outcome">{note}</span> })
                            }}
                        </span>
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
/// start, or a code run's, whose code, calls and result do.
fn is_command(item: &ItemData) -> bool {
    item.act
        .as_ref()
        .is_some_and(|act| matches!(act.kind, ActKind::Command | ActKind::Code))
}

/// A call's line, as the terminal's Full line: its fold's triangle (`mark`),
/// whose it is (a worker's in the main view), what it does, its mark and
/// outcome ([`quiet::mark`]), and the spinner, Move to background or Stop
/// while those apply, except in a Quiet row (`in_row`), whose own line has
/// them. Its actions follow the status only while the call runs and the
/// jobs only while it has one, so the many settled calls of a long session
/// follow neither. Its words and outcome fit its width by the rule every
/// row's line follows ([`fit`]), and `cut`, where given, says whether they
/// cut its words or left out its outcome's words.
fn call_line(
    data: RwSignal<ItemData>,
    mark: impl Fn() -> &'static str + Clone + Send + Sync + 'static,
    in_row: bool,
    cut: Option<RwSignal<LineCut>>,
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
    let line = NodeRef::<leptos::html::Span>::new();
    let room = fit::watch(line, move || {
        data.track();
        actions.track();
    });
    let fitted = Memo::new(move |_| {
        let room = room.get()?;
        Some(data.with(|item| fit_call(item, room.now)))
    });
    if let Some(cut) = cut {
        // (It opens to what its line as drawn left out, its spinner and
        // buttons standing, since they keep their room for their state.)
        Effect::new(move |_| {
            let cuts = fitted.with(|fitted| {
                fitted
                    .as_ref()
                    .map_or(LineCut::default(), |fitted| LineCut {
                        words_cut: fitted.words_cut,
                        left_out: !fitted.outcome,
                    })
            });
            if cut.get_untracked() != cuts {
                cut.set(cuts);
            }
        });
    }
    move || {
        data.with(|item| {
            let who = (item.agent != MAIN && page.worker.get_untracked().is_none())
                .then(|| item.agent.clone());
            let mark = mark.clone();
            let fitted = fitted.get();
            let is_fitted = fitted.is_some();
            let (marked, words) = call_outcome(item);
            let shows_words = fitted.as_ref().is_none_or(|fitted| fitted.outcome);
            let outcome = match item.status {
                Some(CallStatus::Running) if in_row => None,
                Some(CallStatus::Running) => Some(
                    view! { <span class="outcome"><Spinner /></span> }
                        .into_any(),
                ),
                Some(_) if marked.is_some() || (words.is_some() && shows_words) => {
                    let title = item.outcome.clone().unwrap_or_default();
                    let glyph = marked.map(|mark| {
                        let failed = mark == CallStatus::Failed;
                        view! { <span class=if failed { "failed" } else { "ok" }>{if failed { "✗" } else { "✓" }}</span> }
                    });
                    let words = words
                        .filter(|_| shows_words)
                        .map(|words| view! { <span class="words">{words}</span> });
                    Some(
                        view! { <span class="outcome" title=title>{glyph}{words}</span> }
                            .into_any(),
                    )
                }
                _ => None,
            };
            // (The spinner and the buttons stay whole, and what the line
            // says fits the room they leave.)
            let (said, spin) = match item.status {
                Some(CallStatus::Running) => (None, outcome),
                _ => (outcome, None),
            };
            // A file's line keeps the file's name whole ([`FileLine`]).
            let file = FileLine::of(&item.text, item.path);
            let what = match (file, fitted.map(|fitted| fitted.words)) {
                (Some(file), fitted) => {
                    let (folders, name) = match fitted {
                        Some(FitWords::File { folders, name }) => (folders, name),
                        _ => (file.folders.to_string(), file.name.to_string()),
                    };
                    view! {
                        <span class="what file" title=item.text.clone()>
                            <span class="lead">{file.lead.to_string()}</span>
                            {(!folders.is_empty()).then(|| view! {
                                <span class="folders"><span>{folders}</span></span>
                            })}
                            <span class="name">{name}</span>
                        </span>
                    }
                    .into_any()
                }
                (None, Some(FitWords::Plain(words))) => view! {
                    <span class="what" title=item.text.clone()>{words}</span>
                }
                .into_any(),
                (None, _) => view! {
                    <span class="what" class:long=long_words(&item.text) title=item.text.clone()>{item.text.clone()}</span>
                }
                .into_any(),
            };
            view! {
                <span class="call-line" node_ref=line>
                    <span class="mark">{mark}</span>
                    {who.map(|who| view! { <span class="who">{who}</span> })}
                    <span class="said" class:file=file.is_some() class:fitted=is_fitted>
                        {what}
                        {said}
                    </span>
                    {spin}
                    {move || {
                        if in_row {
                            return None;
                        }
                        let kept = data.with(kept_actions);
                        Some(action_buttons(id, actions.get(), kept))
                    }}
                </span>
            }
        })
    }
}

/// What follows a call's words once it ended, its mark ([`quiet::mark`]) and
/// its outcome's words, as its line says them (" 41.0 s" after a mark, and
/// without one the words alone). A call that failed says so whatever else
/// it says. Nothing while it runs.
fn call_outcome(item: &ItemData) -> (Option<CallStatus>, Option<String>) {
    let words = quiet::ended(item.status, item.outcome.as_deref());
    let mark = quiet::mark(item.status, item.act.as_ref());
    let words = words.map(|words| match mark {
        Some(_) => format!(" {words}"),
        None => words.to_string(),
    });
    (mark, words)
}

/// What a call line's fit left out, which the call opens to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LineCut {
    /// It cut its words (or a file's folders or name).
    words_cut: bool,
    /// It left out its outcome's words.
    left_out: bool,
}

impl LineCut {
    /// It cut its words or left out its outcome's words.
    fn cut(self) -> bool {
        self.words_cut || self.left_out
    }
}

/// What an open call shows over its detail of what its line left out
/// (`cut`), each once ([`quiet::call_left_out`]). Where the line cut its
/// words, the call as written, unless its detail or content holds it
/// ([`holds_call`]). Where it left out its outcome's words, them. Never its
/// mark.
fn shown_whole(item: &ItemData, cut: LineCut) -> Option<String> {
    let words =
        (cut.words_cut && !holds_call(item, item.detail.as_deref())).then_some(item.text.as_str());
    let outcome = quiet::ended(item.status, item.outcome.as_deref()).filter(|_| cut.left_out);
    quiet::call_left_out(words, outcome)
}

/// A call line's words as they fit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FitWords {
    Plain(String),
    /// A file tool's line, its folders and its name.
    File {
        folders: String,
        name: String,
    },
}

/// A call line's words and outcome as they fit `room` pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallFit {
    words: FitWords,
    /// Its outcome's words show.
    outcome: bool,
    /// It cut its words (or a file's folders or name).
    words_cut: bool,
}

/// `item`'s line fitted to `room` pixels by the rule every row's line
/// follows ([`fit::line`], [`fit::file`]). Its mark with its gap and dot never
/// gives way, and its outcome's words give way whole.
fn fit_call(item: &ItemData, room: f64) -> CallFit {
    let m = fit::metrics();
    let (mark, words) = call_outcome(item);
    let fixed = match mark {
        Some(_) => fit::GAP + m.dot() + m.mark,
        None => 0.0,
    };
    // (Without a mark, the outcome's gap and dot go with its words.)
    let outcome = words.as_deref().map(|words| match mark {
        Some(_) => m.text(words),
        None => fit::GAP + m.dot() + m.text(words),
    });
    match FileLine::of(&item.text, item.path) {
        Some(file) => {
            let fitted = fit::file(file.lead, file.folders, file.name, fixed, outcome, room, m);
            CallFit {
                words: FitWords::File {
                    folders: fitted.folders,
                    name: fitted.name,
                },
                outcome: fitted.outcome,
                words_cut: fitted.words_cut,
            }
        }
        None => {
            let keep = quiet::outcome_keep(item.act.as_ref());
            let parts: Vec<(f64, usize)> =
                outcome.into_iter().map(|outcome| (outcome, keep)).collect();
            let fitted = fit::line(&item.text.replace('\n', " "), fixed, &parts, room, m);
            CallFit {
                outcome: fitted.parts == parts.len(),
                words: FitWords::Plain(fitted.words),
                words_cut: fitted.words_cut,
            }
        }
    }
}

/// A call's row in Full, its line, which opens to its detail and its
/// content (its result's row, under it) when it has them. A command's is
/// open from the start. The images its tool returned show under it, open or
/// not.
fn call_row(cell: RowCell) -> AnyView {
    let page = page();
    let data = cell.data;
    let (id, command) = data.with_untracked(|item| (item.id, is_command(item)));
    let content = Memo::new(move |_| crate::app::content_of(id).is_some());
    // (Its line cut its words or left out its outcome's words.)
    let cut = RwSignal::new(LineCut::default());
    // (A worker's call in the main view shows without its content. A row
    // is drawn for one view.)
    let folds = move || {
        let own = data.with(|item| item.agent == MAIN) || page.worker.get_untracked().is_some();
        data.with(|item| item.detail.is_some()) || (own && content.get()) || cut.get().cut()
    };
    let open = move || folds() && full_open(page, id, command);
    // (Its triangle turns as it opens.)
    let mark = move || if folds() { "▸" } else { STILL };
    let detail = move || {
        open().then(|| {
            // (Where its detail or content holds its call as written, that
            // doesn't show twice.)
            let whole = data
                .with(|item| shown_whole(item, cut.get()))
                .map(|whole| view! { <pre class="held">{whole}</pre> });
            let detail = data
                .with(|item| item.detail.clone())
                .map(|detail| view! { <pre class="held">{detail}</pre> });
            (whole.is_some() || detail.is_some())
                .then(|| view! { <div class="holds">{whole}{detail}</div> })
        })
    };
    view! {
        <details class="call" prop:open=open>
            <summary
                tabindex=move || if folds() { "0" } else { "-1" }
                on:click=move |event| {
                    event.prevent_default();
                    if open() {
                        page.flipped.toggle(id);
                        return;
                    }
                    if folds() && !full_open(page, id, command) {
                        page.flipped.toggle(id);
                    }
                }
            >
                {call_line(data, mark, false, Some(cut))}
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
    let spaced = Spaced::of(&key);
    let call = data.with_untracked(|item| item.call);
    let command =
        call.and_then(|call| STORE.with(|store| store.borrow().get(call).map(is_command)));
    let shown = move || match (call, command) {
        (Some(call), Some(command)) => full_open(page, call, command),
        _ => true,
    };
    // (A code run's calls come with its content, while it shows, so their
    // lines fit the room they have when they show.)
    let holds = move || {
        shown().then(|| {
            view! {
                {contents::content_view(data)}
                {call.map(|call| run_calls(call, data, true))}
            }
        })
    };
    view! {
        <div class="row result" class:closed=move || !shown() data-id=id data-row=key data-gap=spaced.gap() data-gap-open=spaced.gap_open() data-end=spaced.end() data-end-open=spaced.end_open() style:display=move || if shown() { "" } else { "none" }>
            <div class="holds">{holds}</div>
        </div>
    }
    .into_any()
}

/// A call a code run's code made, as its Full line under the run says it,
/// muted, its words and outcome fitted to its width ([`fit`]). The one that
/// runs ends in "…", and the run's own line has the spinner.
fn run_line(line: crate::protocol::CallLine) -> AnyView {
    let node = NodeRef::<leptos::html::Span>::new();
    let room = fit::watch(node, || ());
    let running = line.status == CallStatus::Running;
    let failed = line.status == CallStatus::Failed;
    let words = (!running && !line.outcome.is_empty()).then(|| format!(" {}", line.outcome));
    // A file's line keeps the file's name whole ([`FileLine`]). One that
    // failed is whole, wrapped under its words, since what failed is what
    // the reader opened the run to see.
    let fitted = Memo::new({
        let (text, path, words) = (line.text.clone(), line.path, words.clone());
        move |_| {
            if failed {
                return None;
            }
            let room = room.get()?.now;
            let m = fit::metrics();
            let fixed = match running {
                true => fit::GAP + m.text("…"),
                false => fit::GAP + m.dot() + m.mark,
            };
            let outcome = words.as_deref().map(|words| m.text(words));
            Some(match FileLine::of(&text, path) {
                Some(file) => {
                    let fitted =
                        fit::file(file.lead, file.folders, file.name, fixed, outcome, room, m);
                    (
                        FitWords::File {
                            folders: fitted.folders,
                            name: fitted.name,
                        },
                        fitted.outcome,
                    )
                }
                None => {
                    let parts: Vec<(f64, usize)> = outcome
                        .into_iter()
                        .map(|outcome| (outcome, quiet::WORDS_KEEP))
                        .collect();
                    let fitted = fit::line(&text, fixed, &parts, room, m);
                    (FitWords::Plain(fitted.words), fitted.parts == parts.len())
                }
            })
        }
    });
    let text = line.text.clone();
    let path = line.path;
    let what = move || {
        let file = FileLine::of(&text, path).filter(|_| !failed);
        match (file, fitted.get().map(|(words, _)| words)) {
            (Some(file), fitted) => {
                let (folders, name) = match fitted {
                    Some(FitWords::File { folders, name }) => (folders, name),
                    _ => (file.folders.to_string(), file.name.to_string()),
                };
                view! {
                    <span class="what file" title=text.clone()>
                        <span class="lead">{file.lead.to_string()}</span>
                        {(!folders.is_empty()).then(|| view! {
                            <span class="folders"><span>{folders}</span></span>
                        })}
                        <span class="name">{name}</span>
                    </span>
                }
                .into_any()
            }
            (None, Some(FitWords::Plain(words))) => {
                view! { <span class="what" title=text.clone()>{words}</span> }.into_any()
            }
            (None, _) => {
                view! { <span class="what" title=text.clone()>{text.clone()}</span> }.into_any()
            }
        }
    };
    let outcome = (!running).then(|| {
        let words = words.clone();
        view! {
            <span class="outcome" title=line.outcome.clone()>
                <span class=if failed { "failed" } else { "ok" }>{if failed { "✗" } else { "✓" }}</span>
                {move || {
                    words
                        .clone()
                        .filter(|_| fitted.get().is_none_or(|(_, shows)| shows))
                        .map(|words| view! { <span class="words">{words}</span> })
                }}
            </span>
        }
    });
    let file = FileLine::of(&line.text, line.path).is_some() && !failed;
    view! {
        <span class="call-line" class:wraps=failed node_ref=node>
            <span class="mark">"·"</span>
            <span class="said" class:file=file class:fitted=move || room.get().is_some()>
                {what}
                {running.then(|| view! { <span class="running">"…"</span> })}
                {outcome}
            </span>
        </span>
    }
    .into_any()
}

/// `text` in one line of the box it's in, cut where a word ends to fit the
/// box's width ([`fit`]), as the terminal cuts it.
fn fitted_text(text: String) -> impl IntoView {
    let node = NodeRef::<leptos::html::Span>::new();
    let room = fit::watch(node, || ());
    let words = text.clone();
    let shown = move || match room.get() {
        Some(room) => fit::line(&words, 0.0, &[], room.now, fit::metrics_of(fit::RUN_FONT)).words,
        None => words.clone(),
    };
    view! { <span class="fitted" node_ref=node>{shown}</span> }
}

/// What the calls of the code run `call` did, after `content` when that is
/// the run's code, as they come. In Quiet (`full` unset) that is one muted
/// line of what those that went well did and a line with ✗ for each that
/// failed or was refused, in the order they came. In Full it is each call's
/// line, muted.
fn run_calls(call: u64, content: RwSignal<ItemData>, full: bool) -> impl IntoView {
    let code = content.with_untracked(|item| item.content == Some(ContentKind::Code));
    let calls = move || {
        code.then(|| cell(call))
            .flatten()
            .and_then(|cell| cell.data.with(|item| item.act.as_ref()?.calls.clone()))
    };
    // (Nothing while there is nothing to show, so no empty block takes a gap
    // in what the call holds.)
    move || {
        let calls = calls()?;
        if full {
            if calls.lines.is_empty() {
                return None;
            }
            let lines = calls.lines.into_iter().map(run_line).collect_view();
            return Some(view! { <div class="run-calls full">{lines}</div> }.into_any());
        }
        // (In the order they came, so each keeps its place as the rest come.
        // The summary stands where the first call that went well ended.)
        let summary = || {
            let title = calls.summary.clone();
            view! { <div class="run-summary" title=title>{fitted_text(calls.summary.clone())}</div> }
            .into_any()
        };
        let mut summed = calls.summary.is_empty();
        let mut lines = Vec::new();
        for line in &calls.lines {
            if !line.failed.is_empty() {
                lines.push(
                    view! {
                        <div class="run-failed">
                            <span class="what">{line.failed.clone()}</span>
                            <span class="failed">"✗"</span>
                        </div>
                    }
                    .into_any(),
                );
            } else if line.status != CallStatus::Running && !summed {
                lines.push(summary());
                summed = true;
            }
        }
        if !summed {
            lines.push(summary());
        }
        (!lines.is_empty()).then(|| view! { <div class="run-calls">{lines}</div> }.into_any())
    }
}

#[component]
fn MoveButton(id: u64) -> impl IntoView {
    let busy = RwSignal::new(false);
    view! {
        <button type="button" class="btn ghost small call-action" disabled=move || busy.get()
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

/// A call's buttons on its line, Move to background and Stop where they
/// apply now (`offered`, [`call_actions`]), and where its state keeps their
/// room but they don't apply yet (`kept`, [`kept_actions`]), the button
/// unseen in its place. (So the line keeps its room and its words their
/// cut when the button comes.)
fn action_buttons(id: u64, offered: (bool, Option<String>), kept: (bool, bool)) -> impl IntoView {
    let (moves, stop) = offered;
    let (keep_move, keep_stop) = kept;
    let move_button = match (moves, keep_move) {
        (true, _) => Some(view! { <MoveButton id=id /> }.into_any()),
        (false, true) => Some(view! { <KeptButton label="Move to background" /> }.into_any()),
        _ => None,
    };
    let stop_button = match (stop, keep_stop) {
        (Some(job), _) => Some(view! { <StopJobButton job=job /> }.into_any()),
        (None, true) => Some(view! { <KeptButton label="Stop" /> }.into_any()),
        _ => None,
    };
    view! { {move_button}{stop_button} }
}

/// The room of a button that doesn't apply yet, unseen and out of reach.
#[component]
fn KeptButton(label: &'static str) -> impl IntoView {
    view! {
        <button type="button" class="btn ghost small call-action kept" tabindex="-1" aria-hidden="true" disabled=true>
            {label}
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
        <button type="button" class="btn ghost small call-action" disabled=move || busy.get()
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

/// A worker's live card, two lines in every state, its title and its meta
/// line on one line ([`fit_meta`]). (Its report has a row of its own, and
/// what it does is in its own view.) In the workers' sheet (`listed`), the
/// workers a worker started stand indented under it. It follows its own
/// worker's summary, makes its words once for the places that show them,
/// and follows the clock only while its worker's time runs.
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
                    let parts = page
                        .models
                        .with(|usual| words::worker_parts(summary, now, viewing.as_deref(), usual));
                    (words::worker_title(summary), parts)
                }
                None => (id.clone(), words::unknown_parts()),
            })
        }
    });
    let whole = Memo::new(move |_| {
        words.with(|(_, parts)| {
            parts
                .iter()
                .map(|part| part.words.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        })
    });
    let line = NodeRef::<leptos::html::Span>::new();
    let room = crate::fit::watch(line, move || words.track());
    let meta = Memo::new(move |_| match room.get() {
        Some(room) => words.with(|(_, parts)| fit_meta(parts, room.now)),
        None => whole.get(),
    });
    // The title on its line, in the room its ID's chip leaves, cut where a
    // word ends with "…" right after it.
    let title_line = NodeRef::<leptos::html::Span>::new();
    let title_room = crate::fit::watch(title_line, move || words.track());
    let title = Memo::new({
        let id = id.clone();
        move |_| {
            words.with(|(title, _)| match title_room.get() {
                Some(room) => {
                    let chip = crate::fit::text_width(ID_FONT, &id) + ID_ROOM;
                    crate::fit::cut_text(TITLE_FONT, title, room.now - chip)
                }
                None => title.clone(),
            })
        }
    });
    let open = id.clone();
    let label = {
        let id = id.clone();
        move || words.with(|(title, _)| format!("{id}, {title}, {}", whole.get()))
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
            <span class="wtitle"><span class="wid">{id.clone()}</span><span node_ref=title_line class="fitted">{move || title.get()}</span></span>
            <span class="wgo"><ChevronRight size=16 /></span>
            <span class="wmeta"><span node_ref=line class="fitted">{move || meta.get()}</span></span>
        </button>
    }
}

/// The font of a worker card's meta line (`.wcard .wmeta` in app.css).
const META_FONT: &str = "12.5px var(--sans)";

/// The fonts of a worker card's title and of its ID's chip, and the room
/// the chip takes besides its ID, its padding and its margin
/// (`.wcard .wtitle` and `.wcard .wid` in app.css).
const TITLE_FONT: &str = "600 14.5px var(--sans)";
const ID_FONT: &str = "600 11px var(--sans)";
const ID_ROOM: f64 = 5.0 + 5.0 + 6.0;

/// A worker card's meta line in `room` pixels, on one line
/// ([`words::fit_parts`]).
fn fit_meta(parts: &[words::MetaPart], room: f64) -> String {
    let units = |px: f64| (px * 100.0).round().max(0.0) as usize;
    words::fit_parts(parts, units(room), |text| {
        units(crate::fit::text_width(META_FONT, text))
    })
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
/// worker's opens at its end, and coming back to the main one returns to
/// where the reader was ([`scroll::worker_view`]).
fn switch_view(page: Page, id: Option<String>) {
    scroll::worker_view(id.is_some());
    page.worker.set(id);
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
