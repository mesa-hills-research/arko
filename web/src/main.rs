//! Arko's remote page, in Rust with Leptos: the key exchange, the event
//! stream, the main transcript and the message box.

mod markdown;
mod scroll;

// The protocol, compiled from the server's own file.
#[allow(dead_code)]
#[path = "../../src/remote/protocol.rs"]
mod protocol;

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{ArrowDown, ArrowUp, Maximize2, Minimize2, Settings};
use protocol::{Append, CallStatus, ItemData, Kind, Level, Removed, ResetData, StatusData};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{EventSource, HtmlTextAreaElement, MessageEvent, RequestCredentials, RequestInit};

/// Where the key waits, in this tab, between the link and the cookie. The
/// current page uses the same name, so a tab can switch between them.
const KEY: &str = "arko.remote.key";
const THEME: &str = "arko.next.theme";

fn main() {
    console_panics();
    // Apple's systems keep their own scrollbars (see app.css).
    if scroll::apple()
        && let Some(root) = document().document_element()
    {
        let _ = root.set_attribute("class", "apple");
    }
    leptos::mount::mount_to_body(App);
}

fn console_panics() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&info.to_string().into());
    }));
}

// ----- state -----

/// One row of the transcript. Its text and data are signals of their own,
/// so an append redraws that row alone.
#[derive(Clone, Copy)]
struct Row {
    id: u64,
    kind: Kind,
    data: RwSignal<ItemData>,
    text: RwSignal<String>,
}

#[derive(Clone, PartialEq)]
enum Connection {
    Connecting,
    Live,
    Offline,
    /// The page can't get in, and why.
    Locked(&'static str),
}

#[derive(Clone, Copy, PartialEq)]
enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy)]
struct Page {
    rows: RwSignal<Vec<Row>>,
    status: RwSignal<StatusData>,
    connection: RwSignal<Connection>,
}

/// The open stream, with its handlers (dropped when it closes).
type Stream = (EventSource, Vec<Closure<dyn FnMut(MessageEvent)>>);

thread_local! {
    static ROWS: RefCell<HashMap<u64, Row>> = RefCell::new(HashMap::new());
    static STREAM: RefCell<Option<Stream>> = const { RefCell::new(None) };
}

// ----- the server -----

fn session_storage() -> Option<web_sys::Storage> {
    window().session_storage().ok().flatten()
}

fn local_storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

/// Sends a request, and gives its status, or `None` when the server can't
/// be reached.
async fn request(method: &str, path: &str, body: Option<String>) -> Option<u16> {
    let init = RequestInit::new();
    init.set_method(method);
    init.set_credentials(RequestCredentials::SameOrigin);
    if let Some(body) = body {
        let headers = web_sys::Headers::new().ok()?;
        headers.set("Content-Type", "application/json").ok()?;
        init.set_headers(&headers);
        init.set_body(&body.into());
    }
    let response = JsFuture::from(window().fetch_with_str_and_init(path, &init))
        .await
        .ok()?;
    Some(response.dyn_into::<web_sys::Response>().ok()?.status())
}

/// Takes the key from the link's fragment into this tab's storage, and off
/// the address bar.
fn take_key() {
    let location = window().location();
    let hash = location.hash().unwrap_or_default();
    let Some(key) = hash
        .trim_start_matches('#')
        .split('&')
        .find_map(|pair| pair.strip_prefix("key="))
    else {
        return;
    };
    if let Some(storage) = session_storage() {
        let _ = storage.set_item(KEY, key);
    }
    let path = location.pathname().unwrap_or_default() + &location.search().unwrap_or_default();
    if let Ok(history) = window().history() {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&path));
    }
}

/// Gets a session cookie (with the key, if the cookie is gone), then opens
/// the stream.
async fn start(page: Page) {
    match request("GET", "/api/session", None).await {
        Some(204) => {}
        Some(401) => {
            let key = session_storage()
                .and_then(|storage| storage.get_item(KEY).ok().flatten())
                .unwrap_or_default();
            if key.is_empty() {
                page.connection.set(Connection::Locked(NO_KEY));
                return;
            }
            let body = serde_json::json!({ "key": key }).to_string();
            match request("POST", "/api/session", Some(body)).await {
                Some(204) => {}
                Some(429) => return page.connection.set(Connection::Locked(WAIT)),
                Some(_) => {
                    if let Some(storage) = session_storage() {
                        let _ = storage.remove_item(KEY);
                    }
                    return page.connection.set(Connection::Locked(NO_KEY));
                }
                None => return offline(page),
            }
        }
        Some(429) => return page.connection.set(Connection::Locked(WAIT)),
        _ => return offline(page),
    }
    connect(page);
}

const NO_KEY: &str = "Open the link Arko printed, which holds the key.";
const WAIT: &str = "Too many wrong keys. Wait a minute, then try again.";

fn offline(page: Page) {
    page.connection.set(Connection::Offline);
    let retry = Closure::once_into_js(move || spawn_local(start(page)));
    let _ =
        window().set_timeout_with_callback_and_timeout_and_arguments_0(retry.unchecked_ref(), 2000);
}

fn connect(page: Page) {
    close_stream();
    let Ok(source) = EventSource::new("/api/events") else {
        return offline(page);
    };
    let mut handlers = Vec::new();
    let mut on = |name: &str, handle: fn(Page, &str)| {
        let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(data) = event.data().as_string() {
                handle(page, &data);
            }
        });
        let _ = source.add_event_listener_with_callback(name, handler.as_ref().unchecked_ref());
        handlers.push(handler);
    };
    on("reset", on_reset);
    on("item", on_item);
    on("append", on_append);
    on("change", on_change);
    on("remove", on_remove);
    on("status", on_status);
    let error = Closure::<dyn FnMut(MessageEvent)>::new(move |_| {
        close_stream();
        offline(page);
    });
    source.set_onerror(Some(error.as_ref().unchecked_ref()));
    handlers.push(error);
    STREAM.with(|stream| *stream.borrow_mut() = Some((source, handlers)));
}

fn close_stream() {
    STREAM.with(|stream| {
        if let Some((source, _)) = stream.borrow_mut().take() {
            source.close();
        }
    });
}

// ----- the stream's events -----

/// The page shows the main transcript.
fn shown(item: &ItemData) -> bool {
    item.agent == "main" && item.kind != Kind::Thinking
}

fn new_row(item: ItemData) -> Row {
    Row {
        id: item.id,
        kind: item.kind,
        text: RwSignal::new(item.text.clone()),
        data: RwSignal::new(item),
    }
}

/// A mark on the page's timeline, for timing it.
fn mark(name: &str) {
    if let Some(performance) = window().performance() {
        let _ = performance.mark(name);
    }
}

fn on_reset(page: Page, data: &str) {
    mark("arko-reset");
    let Ok(reset) = serde_json::from_str::<ResetData>(data) else {
        return;
    };
    mark("arko-parsed");
    let rows: Vec<Row> = reset.items.into_iter().filter(shown).map(new_row).collect();
    ROWS.with(|index| {
        let mut index = index.borrow_mut();
        index.clear();
        index.extend(rows.iter().map(|row| (row.id, *row)));
    });
    page.rows.set(rows);
    page.status.set(reset.status);
    page.connection.set(Connection::Live);
    // Once drawn: to the latest, and a mark for timing the load.
    let drawn = Closure::once_into_js(move || {
        scroll::to_bottom_now();
        mark("arko-transcript");
    });
    let _ = window().request_animation_frame(drawn.unchecked_ref());
}

fn on_item(page: Page, data: &str) {
    let Ok(item) = serde_json::from_str::<ItemData>(data) else {
        return;
    };
    if !shown(&item) {
        return;
    }
    let row = new_row(item);
    ROWS.with(|index| index.borrow_mut().insert(row.id, row));
    page.rows.update(|rows| rows.push(row));
    scroll::follow();
}

fn on_append(_: Page, data: &str) {
    let Ok(append) = serde_json::from_str::<Append>(data) else {
        return;
    };
    if let Some(row) = ROWS.with(|index| index.borrow().get(&append.id).copied()) {
        row.text.update(|text| text.push_str(&append.text));
        scroll::follow();
    }
}

fn on_change(_: Page, data: &str) {
    let Ok(item) = serde_json::from_str::<ItemData>(data) else {
        return;
    };
    if let Some(row) = ROWS.with(|index| index.borrow().get(&item.id).copied()) {
        if row.text.get_untracked() != item.text {
            row.text.set(item.text.clone());
        }
        row.data.set(item);
        scroll::follow();
    }
}

fn on_remove(page: Page, data: &str) {
    let Ok(removed) = serde_json::from_str::<Removed>(data) else {
        return;
    };
    if ROWS
        .with(|index| index.borrow_mut().remove(&removed.id))
        .is_some()
    {
        page.rows
            .update(|rows| rows.retain(|row| row.id != removed.id));
    }
}

fn on_status(page: Page, data: &str) {
    if let Ok(status) = serde_json::from_str::<StatusData>(data) {
        page.status.set(status);
    }
}

// ----- the view -----

#[component]
fn App() -> impl IntoView {
    let page = Page {
        rows: RwSignal::new(Vec::new()),
        status: RwSignal::new(StatusData::default()),
        connection: RwSignal::new(Connection::Connecting),
    };
    take_key();
    spawn_local(start(page));

    let theme = RwSignal::new(
        match local_storage()
            .and_then(|storage| storage.get_item(THEME).ok().flatten())
            .as_deref()
        {
            Some("light") => Theme::Light,
            Some("dark") => Theme::Dark,
            _ => Theme::System,
        },
    );
    Effect::new(move |_| {
        let name = match theme.get() {
            Theme::System => None,
            Theme::Light => Some("light"),
            Theme::Dark => Some("dark"),
        };
        if let Some(root) = document().document_element() {
            let _ = match name {
                Some(name) => root.set_attribute("data-theme", name),
                None => root.remove_attribute("data-theme"),
            };
        }
        if let Some(storage) = local_storage() {
            let _ = storage.set_item(THEME, name.unwrap_or("system"));
        }
    });

    let locked = move || match page.connection.get() {
        Connection::Locked(why) => Some(why),
        _ => None,
    };
    view! {
        <Show
            when=move || locked().is_none()
            fallback=move || view! { <main class="lock"><p>{locked}</p></main> }
        >
            <Header page=page theme=theme />
            <Transcript page=page />
            <Composer />
        </Show>
    }
}

#[component]
fn Header(page: Page, theme: RwSignal<Theme>) -> impl IntoView {
    let menu = RwSignal::new(false);
    let state = move || match page.connection.get() {
        Connection::Connecting => "Connecting",
        Connection::Offline => "Offline",
        _ if page.status.with(|status| status.question.is_some()) => "Asking",
        _ if page.status.with(|status| status.working) => "Working",
        _ => "",
    };
    let cost = move || {
        let cost = page.status.with(|status| status.cost);
        (cost > 0.0).then(|| format!("${cost:.2}"))
    };
    let choice = move |value: Theme, label: &'static str| {
        view! {
            <button
                type="button"
                role="menuitemradio"
                aria-checked=move || (theme.get() == value).to_string()
                on:click=move |_| {
                    theme.set(value);
                    menu.set(false);
                }
            >
                {label}
            </button>
        }
    };
    view! {
        <header class="top">
            <div class="column bar">
                <span class="brand">"Arko"</span>
                <span class="state" class:busy=move || !state().is_empty()>{state}</span>
                <span class="spacer"></span>
                <span class="cost">{cost}</span>
                <button
                    type="button"
                    class="icon"
                    aria-label="Settings"
                    aria-haspopup="menu"
                    aria-expanded=move || menu.get().to_string()
                    on:click=move |_| menu.update(|open| *open = !*open)
                >
                    <Settings size=18 />
                </button>
                <Show when=move || menu.get()>
                    <div class="scrim" on:click=move |_| menu.set(false)></div>
                    <div class="menu" role="menu" aria-label="Theme">
                        <p class="menu-title">"Theme"</p>
                        {choice(Theme::System, "System")}
                        {choice(Theme::Light, "Light")}
                        {choice(Theme::Dark, "Dark")}
                    </div>
                </Show>
            </div>
        </header>
    }
}

#[component]
fn Transcript(page: Page) -> impl IntoView {
    let log: NodeRef<leptos::html::Main> = NodeRef::new();
    let pinned = RwSignal::new(true);
    log.on_load(move |log| scroll::attach(log.into(), pinned));
    view! {
        <div class="stage">
            <main id="log" class="log" node_ref=log on:scroll=move |_| scroll::on_scroll()>
                <div id="rows" class="column rows">
                    <For each=move || page.rows.get() key=|row| row.id children=row_view />
                </div>
            </main>
            <button
                type="button"
                class="jump"
                class:shown=move || !pinned.get()
                tabindex=move || if pinned.get() { "-1" } else { "0" }
                on:click=move |_| scroll::jump_to_latest()
            >
                <ArrowDown size=16 />
                "Latest"
            </button>
        </div>
    }
}

fn row_view(row: Row) -> AnyView {
    let text = row.text;
    let data = row.data;
    match row.kind {
        Kind::User => view! {
            <div class="row user" class:pending=move || data.with(|item| item.pending)>
                <div class="bubble">{move || text.get()}</div>
            </div>
        }
        .into_any(),
        Kind::Text | Kind::Reply | Kind::Message | Kind::Question => view! {
            <div
                class="row md"
                class:question=row.kind == Kind::Question
                inner_html=move || markdown::to_html(&text.get())
            ></div>
        }
        .into_any(),
        Kind::Call => {
            let status = move || data.with(|item| item.status);
            view! {
                <div
                    class="row call"
                    class:running=move || status() == Some(CallStatus::Running)
                    class:failed=move || status() == Some(CallStatus::Failed)
                >
                    <span class="mark" aria-hidden="true"></span>
                    <span class="what">{move || text.get()}</span>
                    <span class="outcome">
                        {move || data.with(|item| item.outcome.clone().unwrap_or_default())}
                    </span>
                </div>
            }
            .into_any()
        }
        Kind::Result | Kind::Output => {
            view! { <pre class="row block">{move || text.get()}</pre> }.into_any()
        }
        Kind::Notice => {
            let level = move || data.with(|item| item.level);
            view! {
                <div
                    class="row notice"
                    class:warn=move || level() == Some(Level::Warn)
                    class:error=move || level() == Some(Level::Error)
                >
                    {move || text.get()}
                </div>
            }
            .into_any()
        }
        Kind::Turn | Kind::Conversation => view! {
            <div class="row turn"><span>{move || text.get()}</span></div>
        }
        .into_any(),
        Kind::WorkerLine => view! {
            <div class="row worker">{move || text.get()}</div>
        }
        .into_any(),
        Kind::Thinking => ().into_any(),
    }
}

#[component]
fn Composer() -> impl IntoView {
    let input: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let empty = RwSignal::new(true);
    // The text reached the box's height, which the expand button lifts.
    let tall = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let error = RwSignal::new(None::<&'static str>);
    let sending = RwSignal::new(false);
    // Enter sends where there's a keyboard and a mouse. On a touch screen
    // its key makes a new line, and the button sends.
    let enter_sends = window()
        .match_media("(hover: hover) and (pointer: fine)")
        .ok()
        .flatten()
        .is_some_and(|list| list.matches());
    let placeholder = if enter_sends {
        "Message (Shift+Enter for a new line)"
    } else {
        "Message"
    };

    let resize = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        fit(&area, tall, expanded.get_untracked());
        empty.set(area.value().trim().is_empty());
    };
    Effect::new(move |_| {
        expanded.track();
        resize();
    });

    let send = move || {
        let Some(area) = input.get_untracked() else {
            return;
        };
        let text = area.value();
        if text.trim().is_empty() || sending.get_untracked() {
            return;
        }
        sending.set(true);
        spawn_local(async move {
            let body = serde_json::json!({ "text": text }).to_string();
            match request("POST", "/api/message", Some(body)).await {
                Some(202) => {
                    error.set(None);
                    area.set_value("");
                    expanded.set(false);
                    fit(&area, tall, false);
                    empty.set(true);
                    scroll::jump_to_latest();
                }
                Some(401) => error.set(Some(
                    "Couldn’t send. The session ended, so open the link again.",
                )),
                Some(_) => error.set(Some("Couldn’t send the message.")),
                None => error.set(Some("Couldn’t send. Arko isn’t reachable.")),
            }
            sending.set(false);
        });
    };

    view! {
        <footer class="bottom">
            <div class="column">
                <p class="error" role="alert" class:shown=move || error.get().is_some()>
                    {move || error.get()}
                </p>
                <form
                    class="compose"
                    class:expanded=move || expanded.get()
                    on:submit=move |event| {
                        event.prevent_default();
                        send();
                    }
                >
                    <textarea
                        node_ref=input
                        rows="1"
                        aria-label="Message"
                        placeholder=placeholder
                        enterkeyhint=if enter_sends { "send" } else { "enter" }
                        on:input=move |_| resize()
                        on:keydown=move |event| {
                            if event.key() == "Enter" && !event.shift_key()
                                && !event.is_composing() && enter_sends
                            {
                                event.prevent_default();
                                send();
                            }
                        }
                    ></textarea>
                    <button
                        type="button"
                        class="icon expand"
                        class:shown=move || tall.get() || expanded.get()
                        aria-label=move || if expanded.get() { "Collapse" } else { "Expand" }
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
                    <button
                        type="submit"
                        class="icon send"
                        aria-label="Send"
                        disabled=move || empty.get() || sending.get()
                    >
                        <ArrowUp size=18 />
                    </button>
                </form>
            </div>
        </footer>
    }
}

/// Sizes the box to its text, up to its cap (CSS's `max-height`), and
/// shows a scrollbar only past the cap. An empty box never scrolls.
fn fit(area: &HtmlTextAreaElement, tall: RwSignal<bool>, expanded: bool) {
    // (Sizing shrinks the box for a moment, which would scroll the
    // transcript up.)
    scroll::keeping_bottom(|| fit_now(area, tall, expanded));
}

fn fit_now(area: &HtmlTextAreaElement, tall: RwSignal<bool>, expanded: bool) {
    let style = web_sys::HtmlElement::style(area);
    let _ = style.set_property("height", "auto");
    let needed = area.scroll_height();
    let _ = style.set_property("height", &format!("{needed}px"));
    // `height` is capped by `max-height`, so a box that holds less than it
    // needs is past the cap.
    let over = needed > area.client_height() + 1;
    let _ = style.set_property("overflow-y", if over { "auto" } else { "hidden" });
    tall.set(over || expanded);
}
