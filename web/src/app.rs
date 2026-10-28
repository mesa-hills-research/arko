//! The page's state, the key exchange, the event stream and the actions.

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use serde_json::value::RawValue;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{EventSource, MessageEvent, RequestCredentials, RequestInit};

use crate::layout::View;
use crate::protocol::{
    Append, CodeData, CommandData, ImageData, ItemData, JobSummary, Kind, Removed, StatusData,
    WorkerData,
};
use crate::store::Store;
use crate::words::Connection;

/// Where the key waits, in this tab, between the link and the cookie.
const KEY: &str = "arko.remote.key";
const VIEW: &str = "arko.view";
pub const DRAFT: &str = "arko.draft";

/// What the page can't do without the user.
pub const NO_KEY: &str = "Open the link Arko printed in its console, which holds the key. (A new session, or /remote new-key, makes a new one.)";
pub const ENDED: &str = "This link’s session ended. Open the link Arko printed, or scan its code.";
pub const WAIT: &str = "Too many wrong keys. Wait a minute, then try again.";

/// The sheets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sheet {
    Workers,
    Settings,
}

/// One item's signals: its data (a call's outcome) and its text, which
/// grows as it streams. A row redraws when its own change.
#[derive(Clone, Copy)]
pub struct Cell {
    pub data: RwSignal<ItemData>,
    pub text: RwSignal<String>,
    /// Moves when the spans of its code blocks come.
    pub code: RwSignal<u32>,
}

/// The page's state, shared by its views.
#[derive(Clone, Copy)]
pub struct Page {
    pub status: RwSignal<StatusData>,
    pub connection: RwSignal<Connection>,
    /// Why the page can't get in, when it can't.
    pub locked: RwSignal<Option<&'static str>>,
    /// What went wrong with the connection, in words, and the last
    /// request's status, for the reader.
    pub problem: RwSignal<Option<String>>,
    pub detail: RwSignal<String>,
    /// Bumped when items come or go (the views lay out again).
    pub version: RwSignal<u64>,
    /// Bumped when a worker's summary or a job changes.
    pub workers: RwSignal<u64>,
    pub view: RwSignal<View>,
    /// The worker whose own transcript shows.
    pub worker: RwSignal<Option<String>>,
    /// This device's time (ms), every second.
    pub now: RwSignal<f64>,
    /// Server time minus this device's (ms), as the live items tell it.
    pub skew: RwSignal<Option<f64>>,
    pub error: RwSignal<Option<String>>,
    pub commands: RwSignal<Option<Vec<CommandData>>>,
    pub sheet: RwSignal<Option<Sheet>>,
    /// The image the viewer shows.
    pub image: RwSignal<Option<ImageData>>,
    /// The text item that streams now, whose open table row is held.
    pub streaming: RwSignal<Option<u64>>,
    /// The newest item the reader saw at the bottom.
    pub seen: RwSignal<u64>,
    /// Text came below since the reader left the bottom.
    pub grew: RwSignal<bool>,
    pub pinned: RwSignal<bool>,
    /// The agent's questions as the page shows them.
    pub ask: crate::questions::Asking,
}

thread_local! {
    pub static STORE: RefCell<Store> = RefCell::new(Store::default());
    static CELLS: RefCell<HashMap<u64, Cell>> = RefCell::new(HashMap::new());
    static STREAM: RefCell<Option<(EventSource, Vec<Closure<dyn FnMut(MessageEvent)>>)>> =
        const { RefCell::new(None) };
    static HEARD: RefCell<Vec<Box<dyn Fn(&ItemData)>>> = RefCell::new(Vec::new());
    /// A stream came through once, so a key that's refused now is one that
    /// ended.
    static EVER_LIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The one page, also for callbacks and tasks outside any component.
    static PAGE: std::cell::Cell<Option<Page>> = const { std::cell::Cell::new(None) };
}

pub fn page() -> Page {
    PAGE.with(|page| page.get())
        .expect("the page starts before its views")
}

impl Page {
    pub fn new() -> Page {
        let view = local_get(VIEW)
            .as_deref()
            .and_then(View::from_key)
            .unwrap_or(View::Full);
        let page = Page {
            status: RwSignal::new(StatusData::default()),
            connection: RwSignal::new(Connection::Connecting),
            locked: RwSignal::new(None),
            problem: RwSignal::new(None),
            detail: RwSignal::new(String::new()),
            version: RwSignal::new(0),
            workers: RwSignal::new(0),
            view: RwSignal::new(view),
            worker: RwSignal::new(None),
            now: RwSignal::new(js_sys::Date::now()),
            skew: RwSignal::new(None),
            error: RwSignal::new(None),
            commands: RwSignal::new(None),
            sheet: RwSignal::new(None),
            image: RwSignal::new(None),
            streaming: RwSignal::new(None),
            seen: RwSignal::new(0),
            grew: RwSignal::new(false),
            pinned: RwSignal::new(true),
            ask: crate::questions::Asking::new(),
        };
        PAGE.with(|cell| cell.set(Some(page)));
        page
    }

    /// Arko's time now (ms).
    pub fn server_now(&self) -> f64 {
        self.now.get() - self.skew.get().unwrap_or(0.0)
    }

    pub fn set_view(&self, view: View) {
        self.view.set(view);
        local_set(VIEW, Some(view.key()));
    }
}

/// The signals of the item `id`, while it's kept.
pub fn cell(id: u64) -> Option<Cell> {
    CELLS.with(|cells| cells.borrow().get(&id).copied())
}

/// Calls `listen` with each item that comes, until the page closes (the
/// settings hear Arko's answers to their commands).
pub fn on_item(listen: impl Fn(&ItemData) + 'static) {
    HEARD.with(|heard| heard.borrow_mut().push(Box::new(listen)));
}

// ----- storage -----

pub fn local_get(name: &str) -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()?
        .get_item(name)
        .ok()
        .flatten()
}

pub fn local_set(name: &str, value: Option<&str>) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        // Private windows may refuse, and the choice lasts as long as the page.
        let _ = match value {
            Some(value) if !value.is_empty() => storage.set_item(name, value),
            _ => storage.remove_item(name),
        };
    }
}

pub fn session_get(name: &str) -> Option<String> {
    window()
        .session_storage()
        .ok()
        .flatten()?
        .get_item(name)
        .ok()
        .flatten()
}

pub fn session_set(name: &str, value: Option<&str>) {
    if let Some(storage) = window().session_storage().ok().flatten() {
        let _ = match value {
            Some(value) if !value.is_empty() => storage.set_item(name, value),
            _ => storage.remove_item(name),
        };
    }
}

// ----- requests -----

/// Sends a request, and gives its status and body, or none when Arko can't
/// be reached.
pub async fn request(method: &str, path: &str, body: Option<String>) -> Option<(u16, String)> {
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
    let response: web_sys::Response = response.dyn_into().ok()?;
    let status = response.status();
    let text = match response.text() {
        Ok(promise) => JsFuture::from(promise)
            .await
            .ok()
            .and_then(|text| text.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    page().detail.set(format!("{method} {path} {status}"));
    Some((status, text))
}

/// Gets JSON, or none.
pub async fn get_json<T: for<'a> Deserialize<'a>>(
    path: &str,
    body: Option<serde_json::Value>,
) -> Option<T> {
    let method = if body.is_some() { "POST" } else { "GET" };
    let (status, text) = request(method, path, body.map(|body| body.to_string())).await?;
    if !(200..300).contains(&status) {
        return None;
    }
    serde_json::from_str(&text).ok()
}

/// What a key trade came to.
#[derive(PartialEq)]
enum Login {
    Ok,
    Wrong,
    Wait,
    Offline,
}

/// Trades the key (this tab's, unless given) for a session cookie.
async fn login(key: Option<String>) -> Login {
    let Some(key) = key
        .or_else(|| session_get(KEY))
        .filter(|key| !key.is_empty())
    else {
        return Login::Wrong;
    };
    let body = serde_json::json!({ "key": key }).to_string();
    match request("POST", "/api/session", Some(body)).await {
        Some((204, _)) => {
            session_set(KEY, Some(&key));
            Login::Ok
        }
        Some((429, _)) => Login::Wait,
        Some((401, _)) => {
            session_set(KEY, None);
            Login::Wrong
        }
        Some(_) => Login::Wrong,
        None => Login::Offline,
    }
}

/// Posts an action. A session cookie that ended is traded again once.
/// Returns whether Arko took it, and says why not in the error line.
pub async fn post(path: &str, body: serde_json::Value) -> bool {
    let page = page();
    for attempt in 0..2 {
        let Some((status, text)) = request("POST", path, Some(body.to_string())).await else {
            page.error.set(Some(
                "Not sent, because Arko is out of reach. Your text is kept.".into(),
            ));
            return false;
        };
        if status == 401 && attempt == 0 {
            match login(None).await {
                Login::Ok => continue,
                result => {
                    lock(page, result);
                    return false;
                }
            }
        }
        if !(200..300).contains(&status) {
            #[derive(Deserialize)]
            struct Why {
                error: String,
            }
            let why = serde_json::from_str::<Why>(&text).ok().map(|why| why.error);
            page.error.set(Some(match why {
                // Answers too long for the agent: Arko says what to shorten.
                Some(why) if status == 413 => why,
                Some(why) => format!("Not sent ({why})."),
                // A proxy in front of Arko (Tailscale Serve, say) answers for it.
                None if matches!(status, 502..=504) => {
                    "Not sent, because Arko is out of reach. Your text is kept.".into()
                }
                None => format!("Not sent ({status})."),
            }));
            return false;
        }
        page.error.set(None);
        return true;
    }
    false
}

fn lock(page: Page, result: Login) {
    close_stream();
    page.locked.set(Some(match result {
        Login::Wait => WAIT,
        _ if EVER_LIVE.with(|live| live.get()) => ENDED,
        _ => NO_KEY,
    }));
}

// ----- the key and the stream -----

/// Takes the key from the link's fragment into this tab's storage, and off
/// the address bar. Returns it when there was one.
pub fn take_key() -> Option<String> {
    let location = window().location();
    let hash = location.hash().unwrap_or_default();
    let key = hash
        .trim_start_matches('#')
        .split('&')
        .find_map(|pair| pair.strip_prefix("key="))
        .filter(|key| !key.is_empty())?
        .to_string();
    session_set(KEY, Some(&key));
    let path = location.pathname().unwrap_or_default() + &location.search().unwrap_or_default();
    if let Ok(history) = window().history() {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&path));
    }
    Some(key)
}

/// Gets a session (trading the key when the cookie is gone), then opens
/// the stream. With `fresh`, a key that just came trades first.
pub async fn start(page: Page, fresh: Option<String>) {
    if let Some(key) = fresh {
        match login(Some(key)).await {
            Login::Ok => {
                page.locked.set(None);
                return connect(page);
            }
            Login::Offline => return offline(page, None),
            result => return lock(page, result),
        }
    }
    match request("GET", "/api/session", None).await {
        Some((204, _)) => {}
        Some((401, _)) => match login(None).await {
            Login::Ok => {}
            Login::Offline => return offline(page, None),
            result => return lock(page, result),
        },
        Some((429, _)) => return lock(page, Login::Wait),
        Some((status, _)) => return offline(page, Some(format!("Arko answered {status}."))),
        None => return offline(page, None),
    }
    page.locked.set(None);
    connect(page);
}

thread_local! {
    static RETRY: RefCell<(f64, u32, i32)> = const { RefCell::new((1000.0, 0, 0)) };
}

fn offline(page: Page, why: Option<String>) {
    let (delay, failures) = RETRY.with(|retry| {
        let mut retry = retry.borrow_mut();
        retry.1 += 1;
        let delay = retry.0;
        retry.0 = (retry.0 * 2.0).min(15_000.0);
        (delay, retry.1)
    });
    // A dropped stream reconnects quietly at first, so a phone that slept
    // comes back within a second or two.
    page.connection.set(if failures > 1 {
        Connection::Offline
    } else {
        Connection::Connecting
    });
    page.problem.set(Some(why.unwrap_or_else(|| {
        if failures > 3 {
            "Arko isn’t reachable, and it may have quit. The page reconnects when it’s back.".into()
        } else {
            "Not connected. Reconnecting…".into()
        }
    })));
    let again = Closure::once_into_js(move || spawn_local(start(page, None)));
    let timer = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(again.unchecked_ref(), delay as i32)
        .unwrap_or(0);
    RETRY.with(|retry| retry.borrow_mut().2 = timer);
}

/// Reconnects at once (Retry, or a phone back from sleep).
pub fn retry_now(page: Page) {
    RETRY.with(|retry| {
        let mut retry = retry.borrow_mut();
        window().clear_timeout_with_handle(retry.2);
        retry.0 = 1000.0;
    });
    spawn_local(start(page, None));
}

pub fn streaming_now() -> bool {
    STREAM.with(|stream| stream.borrow().is_some())
}

fn connect(page: Page) {
    close_stream();
    let Ok(source) = EventSource::new("/api/events") else {
        return offline(page, None);
    };
    let mut handlers = Vec::new();
    let mut on = |name: &str, handle: fn(Page, &str) -> Result<(), String>| {
        let event = name.to_string();
        let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
            let Some(data) = message.data().as_string() else {
                return;
            };
            if let Err(why) = handle(page, &data) {
                // Never quietly stuck: say what couldn't be read.
                web_sys::console::error_1(&format!("{event}: {why}").into());
                page.problem.set(Some(format!(
                    "Couldn’t read Arko’s update ({event}: {why})."
                )));
            }
        });
        let _ = source.add_event_listener_with_callback(name, handler.as_ref().unchecked_ref());
        handlers.push(handler);
    };
    on("reset", on_reset);
    on("item", on_item_event);
    on("append", on_append);
    on("change", on_change);
    on("remove", on_remove);
    on("worker", on_worker);
    on("job", on_job);
    on("status", on_status);
    on("code", on_code);
    let error = Closure::<dyn FnMut(MessageEvent)>::new(move |_| {
        close_stream();
        // The stream may have ended with the session: ask before retrying.
        offline(page, None);
    });
    source.set_onerror(Some(error.as_ref().unchecked_ref()));
    handlers.push(error);
    STREAM.with(|stream| *stream.borrow_mut() = Some((source, handlers)));
    load_commands(page);
}

pub fn close_stream() {
    STREAM.with(|stream| {
        if let Some((source, _)) = stream.borrow_mut().take() {
            source.close();
        }
    });
}

fn load_commands(page: Page) {
    if page.commands.get_untracked().is_some() {
        return;
    }
    spawn_local(async move {
        if let Some(list) = get_json::<Vec<CommandData>>("/api/commands", None).await {
            page.commands.set(Some(list));
        }
    });
}

// ----- the stream's events -----

fn parse<T: for<'a> Deserialize<'a>>(data: &str) -> Result<T, String> {
    serde_json::from_str(data).map_err(|error| error.to_string())
}

fn new_cell(item: &ItemData) -> Cell {
    Cell {
        text: RwSignal::new(item.text.clone()),
        data: RwSignal::new(item.clone()),
        code: RwSignal::new(0),
    }
}

fn on_reset(page: Page, data: &str) -> Result<(), String> {
    mark("arko-reset");
    #[derive(Deserialize)]
    struct Raw<'a> {
        #[serde(borrow)]
        items: Vec<&'a RawValue>,
        #[serde(default, borrow)]
        workers: Vec<&'a RawValue>,
        #[serde(default, borrow)]
        jobs: Vec<&'a RawValue>,
        #[serde(default)]
        status: Option<&'a RawValue>,
    }
    let raw: Raw = serde_json::from_str(data).map_err(|error| error.to_string())?;
    // An item that can't be read is left out, and said, rather than losing
    // the transcript.
    let mut bad = 0;
    let mut items: Vec<ItemData> = Vec::with_capacity(raw.items.len());
    for value in raw.items {
        match serde_json::from_str::<ItemData>(value.get()) {
            Ok(mut item) => {
                crate::markdown::remember(&std::mem::take(&mut item.code));
                items.push(item);
            }
            Err(_) => bad += 1,
        }
    }
    let workers: Vec<WorkerData> = raw
        .workers
        .iter()
        .filter_map(|value| serde_json::from_str(value.get()).ok())
        .collect();
    let jobs: Vec<JobSummary> = raw
        .jobs
        .iter()
        .filter_map(|value| serde_json::from_str(value.get()).ok())
        .collect();
    let status: StatusData = match raw.status {
        Some(value) => parse(value.get())?,
        None => StatusData::default(),
    };
    mark("arko-parsed");
    let now = js_sys::Date::now();
    let cells: HashMap<u64, Cell> = items.iter().map(|item| (item.id, new_cell(item))).collect();
    CELLS.with(|all| *all.borrow_mut() = cells);
    STORE.with(|store| store.borrow_mut().reset(items, workers, jobs, now));
    RETRY.with(|retry| *retry.borrow_mut() = (1000.0, 0, 0));
    page.skew.set(None);
    page.status.set(status);
    page.connection.set(Connection::Live);
    EVER_LIVE.with(|live| live.set(true));
    page.problem.set(
        (bad > 0).then(|| format!("{bad} of Arko’s items couldn’t be read, so they’re left out.")),
    );
    page.streaming.set(None);
    page.version.update(|version| *version += 1);
    page.workers.update(|version| *version += 1);
    let drawn = Closure::once_into_js(move || {
        if page.pinned.get_untracked() {
            crate::scroll::to_bottom_now();
            page.seen.set(STORE.with(|store| store.borrow().newest()));
        }
        mark("arko-transcript");
    });
    let _ = window().request_animation_frame(drawn.unchecked_ref());
    Ok(())
}

fn on_item_event(page: Page, data: &str) -> Result<(), String> {
    let mut item: ItemData = parse(data)?;
    let code = std::mem::take(&mut item.code);
    crate::markdown::remember(&code);
    // This device's clock against Arko's: the smallest gap a new item
    // shows (the rest is the network's delay).
    let sample = js_sys::Date::now() - item.at as f64;
    page.skew
        .update(|skew| *skew = Some(skew.map_or(sample, |skew| skew.min(sample))));
    let now = js_sys::Date::now();
    let known = cell(item.id);
    match known {
        Some(cell) => {
            cell.text.set(item.text.clone());
            cell.data.set(item.clone());
            if !code.is_empty() {
                cell.code.update(|code| *code += 1);
            }
            STORE.with(|store| store.borrow_mut().change(item.clone(), now));
        }
        None => {
            let trimmed = STORE.with(|store| store.borrow_mut().add(item.clone(), now));
            CELLS.with(|cells| {
                let mut cells = cells.borrow_mut();
                cells.insert(item.id, new_cell(&item));
                for id in trimmed {
                    cells.remove(&id);
                }
            });
        }
    }
    if matches!(item.kind, Kind::Text | Kind::Thinking) {
        page.streaming.set(Some(item.id));
    } else if page.streaming.get_untracked().is_some() && item.agent == crate::store::MAIN {
        page.streaming.set(None);
    }
    page.version.update(|version| *version += 1);
    HEARD.with(|heard| {
        for listen in heard.borrow().iter() {
            listen(&item);
        }
    });
    crate::scroll::follow();
    Ok(())
}

fn on_append(page: Page, data: &str) -> Result<(), String> {
    let append: Append = parse(data)?;
    let now = js_sys::Date::now();
    if !STORE.with(|store| store.borrow_mut().append(append.id, &append.text, now)) {
        return Ok(());
    }
    if let Some(cell) = cell(append.id) {
        cell.text.update(|text| text.push_str(&append.text));
    }
    if !page.pinned.get_untracked() {
        page.grew.set(true);
    }
    crate::scroll::follow();
    Ok(())
}

fn on_change(page: Page, data: &str) -> Result<(), String> {
    let mut item: ItemData = parse(data)?;
    let code = std::mem::take(&mut item.code);
    crate::markdown::remember(&code);
    let now = js_sys::Date::now();
    if !STORE.with(|store| store.borrow_mut().change(item.clone(), now)) {
        return Ok(());
    }
    if let Some(cell) = cell(item.id) {
        if cell.text.get_untracked() != item.text {
            cell.text.set(item.text.clone());
        }
        cell.data.set(item);
        if !code.is_empty() {
            cell.code.update(|code| *code += 1);
        }
    }
    // A call's status moves a Messages count.
    if page.view.get_untracked() == View::Messages {
        page.version.update(|version| *version += 1);
    }
    crate::scroll::follow();
    Ok(())
}

/// The code blocks a text finished as it streams, highlighted.
fn on_code(_page: Page, data: &str) -> Result<(), String> {
    let code: CodeData = parse(data)?;
    crate::markdown::remember(&code.code);
    if let Some(cell) = cell(code.id) {
        cell.code.update(|code| *code += 1);
    }
    Ok(())
}

fn on_remove(page: Page, data: &str) -> Result<(), String> {
    let removed: Removed = parse(data)?;
    if STORE.with(|store| store.borrow_mut().remove(removed.id)) {
        CELLS.with(|cells| cells.borrow_mut().remove(&removed.id));
        page.version.update(|version| *version += 1);
    }
    Ok(())
}

fn on_worker(page: Page, data: &str) -> Result<(), String> {
    let summary: WorkerData = parse(data)?;
    STORE.with(|store| store.borrow_mut().worker(summary));
    page.workers.update(|version| *version += 1);
    Ok(())
}

fn on_job(page: Page, data: &str) -> Result<(), String> {
    let summary: JobSummary = parse(data)?;
    STORE.with(|store| store.borrow_mut().job(summary));
    page.workers.update(|version| *version += 1);
    Ok(())
}

fn on_status(page: Page, data: &str) -> Result<(), String> {
    let status: StatusData = parse(data)?;
    if !status.working {
        page.streaming.set(None);
    }
    page.status.set(status);
    Ok(())
}

/// A mark on the page's timeline, for timing it.
pub fn mark(name: &str) {
    if let Some(performance) = window().performance() {
        let _ = performance.mark(name);
    }
}
