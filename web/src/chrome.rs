//! The page's chrome: the header, the worker view's banner, the connection
//! line and the lock screen.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{Bot, ChevronDown, ChevronLeft, Settings};

use crate::app::{self, STORE, Sheet, page, retry_now};
use crate::layout::View;
use crate::transcript::show_worker;
use crate::words::{self, Connection, Local};

/// A moment (Unix ms) in this device's local time.
pub fn local(at: f64) -> Local {
    let date = js_sys::Date::new(&at.into());
    let midnight = js_sys::Date::new_with_year_month_day(
        date.get_full_year(),
        date.get_month() as i32,
        date.get_date() as i32,
    );
    Local {
        year: date.get_full_year() as i32,
        month: date.get_month(),
        day: date.get_date(),
        weekday: date.get_day(),
        hour: date.get_hours(),
        minute: date.get_minutes(),
        day_number: (midnight.get_time() / 86_400_000.0).round() as i64,
    }
}

#[component]
pub fn Header() -> impl IntoView {
    let page = page();
    let viewing = move || {
        page.workers.track();
        page.worker.get().map(|id| {
            let summary = STORE.with(|store| store.borrow().workers.get(&id).cloned());
            (id, summary)
        })
    };
    let state = move || {
        let viewing = viewing();
        let connection = page.connection.get();
        page.status.with(|status| {
            words::state_of(
                Some(status),
                &connection,
                viewing
                    .as_ref()
                    .map(|(id, summary)| (id.as_str(), summary.as_ref())),
            )
        })
    };
    // How long the turn has gone on, or the worker's run time in its view.
    let time = move || {
        if page.connection.get() != Connection::Live {
            return String::new();
        }
        let now = page.server_now();
        if let Some((_, summary)) = viewing() {
            if page.status.with(|status| status.question.is_some()) {
                return String::new();
            }
            return summary
                .and_then(|summary| words::run_time(&summary, now))
                .map(words::duration)
                .unwrap_or_default();
        }
        if !page.status.with(|status| status.working) {
            return String::new();
        }
        page.version.track();
        STORE
            .with(|store| store.borrow().turn_since())
            .map(|since| words::duration(now - since as f64))
            .unwrap_or_default()
    };
    let figures = move || {
        let summary = viewing().and_then(|(_, summary)| summary);
        page.status
            .with(|status| words::figures(status, summary.as_ref()).join(" · "))
    };
    let figures_title = move || {
        let summary = viewing().and_then(|(_, summary)| summary);
        page.status
            .with(|status| words::context_title(status, summary.as_ref()))
    };
    // The prompt cache's timer, while the agent waits for the user.
    let cache = move || {
        if page.connection.get() != Connection::Live || page.worker.get().is_some() {
            return None;
        }
        let now = page.server_now();
        page.status.with(|status| {
            let running = status.working && status.question.is_none();
            let left = words::cache_timer(&status.cache, running, now)?;
            let lapses = (status.cache.refreshed_at? + status.cache.lifetime?) as f64;
            let until = words::when_words(
                local((lapses / 1000.0).floor() * 1000.0 + (page.now.get() - now)),
                local(page.now.get()),
            );
            Some((
                words::cache_label(left),
                words::cache_warns(left),
                words::cache_title(left, &status.cache, &until),
            ))
        })
    };
    let outside = move || page.status.with(|status| status.settings.outside);
    let workers = move || {
        page.workers.track();
        STORE.with(|store| {
            let store = store.borrow();
            let running = store
                .workers
                .values()
                .filter(|worker| worker.state.is_active())
                .count();
            (store.workers.len(), running)
        })
    };
    let menu = RwSignal::new(false);
    // The tab's title says when the session needs the user.
    Effect::new(move |_| {
        let (key, approvals) = page.status.with(|status| {
            (
                words::state_of(Some(status), &page.connection.get(), None).0,
                !status.approvals.is_empty(),
            )
        });
        document().set_title(match key {
            "asking" if approvals => "Arko · needs approval",
            "asking" => "Arko · asks you",
            "working" => "Arko · working",
            _ => "Arko",
        });
    });
    view! {
        <header class="top">
            <div class="column bar">
                <span class=move || format!("state {}", state().0)>
                    <span class="state-label" class:shimmer=move || state().0 == "working">
                        {move || { let (key, label) = state(); (key != "idle").then_some(label) }}
                    </span>
                    <span class="state-time">{time}</span>
                </span>
                <span class="figures" title=figures_title>{figures}</span>
                {move || cache().map(|(label, warns, title)| view! { <span class="cache" class:warn=warns title=title>{label}</span> })}
                {move || outside().then(|| view! { <span class="outside" title="The agents may work outside the workspace">"outside access"</span> })}
                <span class="spacer"></span>
                {move || {
                    let (count, running) = workers();
                    let busy = running > 0;
                    let title = if busy { format!("{running} running") } else { "None running".to_string() };
                    (count > 0).then(|| view! {
                        <button type="button" class="btn ghost small workers-button" class:running=busy
                            title=title
                            aria-haspopup="dialog"
                            on:click=move |_| page.sheet.set(Some(Sheet::Workers))>
                            <Bot size=16 />
                            {words::plural(count as u32, "worker", "workers")}
                        </button>
                    })
                }}
                <div class="views" role="radiogroup" aria-label="View">
                    {View::ALL.map(|view| view! {
                        <button type="button" role="radio" aria-checked=move || (page.view.get() == view).to_string()
                            on:click=move |_| change_view(view)>
                            {view.name()}
                        </button>
                    })}
                </div>
                <div class="view-pick">
                    <button type="button" class="btn ghost small" aria-haspopup="menu" aria-expanded=move || menu.get().to_string()
                        on:click=move |_| menu.update(|open| *open = !*open)>
                        {move || page.view.get().name()}
                        <ChevronDown size=14 />
                    </button>
                    <Show when=move || menu.get()>
                        <div class="scrim" on:click=move |_| menu.set(false)></div>
                        <div class="menu" role="menu" aria-label="View">
                            {View::ALL.map(|view| view! {
                                <button type="button" role="menuitemradio" aria-checked=move || (page.view.get() == view).to_string()
                                    on:click=move |_| { change_view(view); menu.set(false); }>
                                    <span class="name">{view.name()}</span>
                                    <span class="about">{view.about()}</span>
                                </button>
                            })}
                        </div>
                    </Show>
                </div>
                <button type="button" class="btn ghost icon" aria-label="Settings" title="Settings" aria-haspopup="dialog"
                    on:click=move |_| page.sheet.set(Some(Sheet::Settings))>
                    <Settings size=18 />
                </button>
            </div>
        </header>
    }
}

/// Changes the view, keeping the reader's place.
fn change_view(view: View) {
    let page = page();
    if page.view.get_untracked() == view {
        return;
    }
    if !page.pinned.get_untracked()
        && let Some(at) = crate::scroll::first_visible()
    {
        crate::scroll::keep_place(at);
    }
    page.set_view(view);
}

/// The worker view's banner: back to the main transcript, and the worker.
#[component]
pub fn Banner() -> impl IntoView {
    let page = page();
    let words = move || {
        page.workers.track();
        let id = page.worker.get()?;
        let summary = STORE.with(|store| store.borrow().workers.get(&id).cloned());
        let (title, meta, _) = match &summary {
            Some(summary) => {
                let usual = page.status.with(|status| status.models.clone());
                words::worker_words(summary, page.server_now(), None, &usual)
            }
            None => (id.clone(), String::new(), String::new()),
        };
        Some((format!("{id} · {title}"), meta))
    };
    view! {
        <Show when=move || page.worker.get().is_some()>
            <div class="banner">
                <div class="column banner-row">
                    <button type="button" class="btn ghost small back" on:click=move |_| show_worker(page, None)>
                        <ChevronLeft size=16 />"Main"
                    </button>
                    <div class="banner-text">
                        <span class="banner-title">{move || words().map(|w| w.0)}</span>
                        <span class="banner-meta">{move || words().map(|w| w.1)}</span>
                    </div>
                    <kbd class="hint-key">"Esc"</kbd>
                </div>
            </div>
        </Show>
    }
}

/// What's wrong with the connection, while something is.
#[component]
pub fn ConnectionLine() -> impl IntoView {
    let page = page();
    let shown = move || {
        page.problem.get().filter(|_| {
            page.connection.get() != Connection::Connecting || page.problem.get().is_some()
        })
    };
    view! {
        {move || shown().map(|problem| view! {
            <div class="conn" role="status">
                <div class="column conn-row">
                    <span>{problem}</span>
                    {(page.connection.get() != Connection::Live).then(|| view! {
                        <button type="button" class="btn ghost small" on:click=move |_| retry_now(page)>"Retry now"</button>
                    })}
                    <span class="conn-detail">{move || page.detail.get()}</span>
                </div>
            </div>
        })}
    }
}

/// The lock screen: why the page can't get in, and a field for the key.
#[component]
pub fn Lock() -> impl IntoView {
    let page = page();
    let key: NodeRef<leptos::html::Input> = NodeRef::new();
    let open = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let Some(input) = key.get_untracked() else {
            return;
        };
        let value = input.value().trim().to_string();
        input.set_value("");
        if value.is_empty() {
            return;
        }
        spawn_local(app::start(page, Some(value)));
    };
    view! {
        <section class="lock">
            <img src="/icon.svg" alt="" class="lock-icon" width="72" height="72" />
            <h1>"Arko"</h1>
            <p>{move || page.locked.get()}</p>
            <form autocomplete="off" on:submit=open>
                <label for="key">"Or paste the key"</label>
                <input id="key" node_ref=key type="password" autocapitalize="off" spellcheck="false" />
                <button class="btn primary" type="submit">"Open"</button>
            </form>
            <p class="conn-detail">{move || page.detail.get()}</p>
        </section>
    }
}
