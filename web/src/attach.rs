//! Files attached to the message box. The Attach button, a paste of files
//! and a drop add them, each as a chip above the box with its upload, one
//! file at a time. An upload that breaks off goes on from what Arko holds,
//! after a wait that grows, and at once when the page shows again. While
//! uploads run, the screen stays on where the browser can keep it on.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lucide_leptos::{Paperclip, X};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::app::{feed, of_feed, page, relogin, request, session_get, session_set};
use crate::composer::gloo_wait;
use crate::protocol::{UploadCreated, UploadState, size};

/// Where the page keeps its chips across a reload in this tab.
const ATTACHED: &str = "arko.attached";

/// A request that sends no bytes for this long is given up and resumed.
const STALL_MS: f64 = 20_000.0;

/// How long an upload is tried again before it fails, in ms.
const GIVE_UP_MS: f64 = 5.0 * 60_000.0;

/// A file attached to the message, and how its upload goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub key: u64,
    pub name: String,
    pub bytes: u64,
    /// A `blob:` address for an image's thumbnail.
    pub preview: Option<String>,
    /// The feed the upload is Arko's in (another Arko knows nothing of it).
    pub feed: Option<u64>,
    pub state: RwSignal<Upload>,
    /// The upload's ID, once Arko gave one.
    pub id: RwSignal<Option<String>>,
}

/// How a chip's upload goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upload {
    Waiting,
    /// Its bytes go, this much of them (in percent).
    Sending(u8),
    /// It broke off, and goes on from this much.
    Retrying(u8),
    Failed,
    Done,
}

impl Upload {
    /// What a chip says after its name, its size unless something is
    /// under way or wrong.
    pub fn words(self, bytes: u64) -> String {
        match self {
            Upload::Sending(percent) => format!("{percent}%"),
            Upload::Retrying(_) => "Retrying".into(),
            Upload::Failed => "Failed".into(),
            Upload::Waiting | Upload::Done => size(bytes),
        }
    }
}

thread_local! {
    /// The files the chips upload, by chip.
    static FILES: RefCell<HashMap<u64, web_sys::File>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u64> = const { Cell::new(1) };
    /// Whether the uploads are running.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
    /// Bumped when the page shows again, which ends a wait to try again.
    static WAKES: Cell<u32> = const { Cell::new(0) };
    /// What keeps the screen on, while it's held.
    static AWAKE: RefCell<Option<wasm_bindgen::JsValue>> = const { RefCell::new(None) };
}

/// The chips' uploads, done, in the order attached, or `None` while one
/// isn't done (or there are none to send). Send waits for them.
pub fn ready(chips: &[Chip]) -> Option<Vec<String>> {
    chips
        .iter()
        .map(|chip| match chip.state.get() {
            Upload::Done => chip.id.get(),
            _ => None,
        })
        .collect()
}

/// `name` for a chip, its middle left out when it's long so its start and
/// extension show.
pub fn short_name(name: &str) -> String {
    const MOST: usize = 24;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MOST {
        return name.to_string();
    }
    let tail = 9;
    let head: String = chars[..MOST - tail - 1].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{head}…{end}")
}

/// The wait before the `tries`th try again, in ms.
pub fn backoff_ms(tries: u32) -> i32 {
    match tries {
        0 | 1 => 1000,
        2 => 2000,
        3 => 4000,
        4 => 8000,
        5 => 15_000,
        _ => 30_000,
    }
}

/// Attaches `files`, as chips whose uploads start at once.
pub fn attach(files: Vec<web_sys::File>) {
    if files.is_empty() {
        return;
    }
    let chips = page().attached;
    for file in files {
        let key = NEXT.with(|next| {
            next.set(next.get() + 1);
            next.get()
        });
        let preview = file
            .type_()
            .starts_with("image/")
            .then(|| web_sys::Url::create_object_url_with_blob(&file).ok())
            .flatten();
        let chip = Chip {
            key,
            name: file.name(),
            bytes: file.size() as u64,
            preview,
            feed: feed(),
            state: RwSignal::new(Upload::Waiting),
            id: RwSignal::new(None),
        };
        FILES.with(|files| files.borrow_mut().insert(key, file));
        chips.update(|chips| chips.push(chip));
    }
    run();
}

/// The files of a `FileList`.
pub fn files_of(list: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(list) = list else {
        return Vec::new();
    };
    (0..list.length()).filter_map(|at| list.get(at)).collect()
}

/// Removes the chip `key`, and its upload unless `sent`.
pub fn remove(key: u64, sent: bool) {
    let chips = page().attached;
    let Some(chip) =
        chips.with_untracked(|chips| chips.iter().find(|chip| chip.key == key).cloned())
    else {
        return;
    };
    chips.update(|chips| chips.retain(|chip| chip.key != key));
    forget(&chip);
    if !sent && let Some(id) = chip.id.get_untracked() {
        spawn_local(async move {
            let _ = request("DELETE", &format!("/api/uploads/{id}"), None).await;
        });
    }
    save();
}

/// The chips went with a message, so they leave the box.
pub fn sent() {
    let keys: Vec<u64> = page()
        .attached
        .with_untracked(|chips| chips.iter().map(|chip| chip.key).collect());
    for key in keys {
        remove(key, true);
    }
}

fn forget(chip: &Chip) {
    FILES.with(|files| files.borrow_mut().remove(&chip.key));
    if let Some(preview) = &chip.preview {
        let _ = web_sys::Url::revoke_object_url(preview);
    }
}

/// Runs the uploads waiting, one at a time, unless they run already.
fn run() {
    if RUNNING.with(|running| running.replace(true)) {
        return;
    }
    spawn_local(async move {
        hold_screen(true);
        while let Some(chip) = next_waiting() {
            upload(chip).await;
            save();
        }
        hold_screen(false);
        RUNNING.with(|running| running.set(false));
    });
}

fn next_waiting() -> Option<Chip> {
    page().attached.with_untracked(|chips| {
        chips
            .iter()
            .find(|chip| chip.state.get_untracked() == Upload::Waiting)
            .cloned()
    })
}

fn still_attached(chip: &Chip) -> bool {
    page()
        .attached
        .with_untracked(|chips| chips.iter().any(|kept| kept.key == chip.key))
        && feed() == chip.feed
}

fn percent(done: u64, of: u64) -> u8 {
    (done.saturating_mul(100) / of.max(1)).min(100) as u8
}

/// What became of a request for an upload.
enum Came {
    /// Arko holds this much, the whole file when `done`.
    Holds { received: u64, done: bool },
    /// Arko holds another part, this much.
    Elsewhere(u64),
    /// It broke off, and may go on.
    Broke,
    /// Arko refused it, for these words.
    Refused(String),
}

/// Uploads `chip`'s file, trying again for a while as long as it breaks
/// off. It fails once five minutes pass without Arko holding more of it. A
/// refusal removes the chip and says why.
async fn upload(chip: Chip) {
    let Some(file) = FILES.with(|files| files.borrow().get(&chip.key).cloned()) else {
        chip.state.set(Upload::Failed);
        return;
    };
    let mut failing_since = None::<f64>;
    let mut tries = 0u32;
    let mut offset = 0u64;
    // The most Arko held, and whether to ask how much it holds before
    // sending more.
    let mut most = None::<u64>;
    let mut ask = false;
    loop {
        if !still_attached(&chip) {
            return;
        }
        let shown = match failing_since {
            None => Upload::Sending(percent(offset, chip.bytes)),
            Some(_) => Upload::Retrying(percent(offset, chip.bytes)),
        };
        chip.state.set(shown);
        let asked = ask;
        let came = match chip.id.get_untracked() {
            None => create(&chip).await,
            Some(id) if asked => state(&id).await,
            Some(id) => put(&id, &file, offset, &chip).await,
        };
        let held = match came {
            Came::Holds { done: true, .. } => {
                chip.state.set(Upload::Done);
                return;
            }
            Came::Refused(words) => {
                remove(chip.key, false);
                page().error.set(Some(words));
                return;
            }
            Came::Holds { received, .. } => Some((received, true)),
            Came::Elsewhere(received) => Some((received, false)),
            Came::Broke => None,
        };
        if let Some((received, _)) = held {
            ask = false;
            offset = received;
            if most.is_none_or(|most| received > most) {
                most = Some(received);
                tries = 0;
                failing_since = None;
                continue;
            }
        }
        // Once Arko said how much it holds, the bytes go at once. Otherwise
        // it broke off or got no further, so it waits longer each time and
        // gives up after a while.
        if asked && matches!(held, Some((_, true))) {
            continue;
        }
        let now = js_sys::Date::now();
        let since = *failing_since.get_or_insert(now);
        if now - since > GIVE_UP_MS {
            chip.state.set(Upload::Failed);
            return;
        }
        tries += 1;
        ask = held.is_none();
        chip.state
            .set(Upload::Retrying(percent(offset, chip.bytes)));
        wait_or_wake(backoff_ms(tries)).await;
    }
}

/// Waits `ms`, or less when the page shows again.
async fn wait_or_wake(ms: i32) {
    let wakes = WAKES.with(Cell::get);
    let mut left = ms;
    while left > 0 && WAKES.with(Cell::get) == wakes {
        gloo_wait(left.min(250)).await;
        left -= 250;
    }
}

/// The page showed again, so an upload waiting to try again tries now,
/// and the screen is held again.
pub fn shown_again() {
    WAKES.with(|wakes| wakes.set(wakes.get() + 1));
    if RUNNING.with(Cell::get) {
        hold_screen(true);
    }
}

#[derive(Deserialize)]
struct Why {
    #[serde(default)]
    error: String,
    #[serde(default)]
    received: Option<u64>,
}

/// What Arko's answer `status`, `text` to a request for an upload says.
fn came_of(status: u16, text: &str) -> Came {
    let why: Option<Why> = serde_json::from_str(text).ok();
    match status {
        200 => match serde_json::from_str::<UploadState>(text) {
            Ok(state) => Came::Holds {
                received: state.received,
                done: state.done,
            },
            Err(_) => Came::Broke,
        },
        409 | 408 => match why.as_ref().and_then(|why| why.received) {
            Some(received) if status == 409 => Came::Elsewhere(received),
            Some(_) => Came::Broke,
            None => Came::Refused(why.map(|why| why.error).unwrap_or_default()),
        },
        0 | 401 | 502 | 503 | 504 => Came::Broke,
        _ => Came::Refused(
            why.map(|why| why.error)
                .filter(|words| !words.is_empty())
                .unwrap_or_else(|| "Couldn’t attach the file.".into()),
        ),
    }
}

/// Asks Arko for an upload of `chip`'s file.
async fn create(chip: &Chip) -> Came {
    let body = of_feed(
        serde_json::json!({ "name": chip.name, "size": chip.bytes }),
        chip.feed,
    );
    let Some((status, text)) = request("POST", "/api/uploads", Some(body.to_string())).await else {
        return Came::Broke;
    };
    if status == 401 {
        relogin().await;
        return Came::Broke;
    }
    if status != 201 {
        return came_of(status, &text);
    }
    match serde_json::from_str::<UploadCreated>(&text) {
        Ok(created) => {
            chip.id.set(Some(created.id));
            save();
            Came::Holds {
                received: 0,
                done: false,
            }
        }
        Err(_) => Came::Broke,
    }
}

/// How far the upload `id` is, once a request still writing it stopped.
async fn state(id: &str) -> Came {
    match request("GET", &format!("/api/uploads/{id}"), None).await {
        Some((401, _)) => {
            relogin().await;
            Came::Broke
        }
        Some((status, text)) => came_of(status, &text),
        None => Came::Broke,
    }
}

/// Sends `file` from `offset` to the upload `id`, telling `chip` how far
/// it is. A request that sends nothing for [`STALL_MS`] is given up.
async fn put(id: &str, file: &web_sys::File, offset: u64, chip: &Chip) -> Came {
    let Ok(request) = web_sys::XmlHttpRequest::new() else {
        return Came::Broke;
    };
    let Ok(body) = file.slice_with_f64_and_f64(offset as f64, chip.bytes as f64) else {
        return Came::Broke;
    };
    if request.open("PUT", &format!("/api/uploads/{id}")).is_err()
        || request
            .set_request_header("Upload-Offset", &offset.to_string())
            .is_err()
    {
        return Came::Broke;
    }
    // When bytes last moved. (A browser may tell of progress without any.)
    let last = std::rc::Rc::new(Cell::new(js_sys::Date::now()));
    let progress = {
        let (last, state, bytes) = (last.clone(), chip.state, chip.bytes);
        let mut sent = 0.0;
        Closure::<dyn FnMut(web_sys::ProgressEvent)>::new(move |event: web_sys::ProgressEvent| {
            if event.loaded() > sent {
                sent = event.loaded();
                last.set(js_sys::Date::now());
            }
            let done = offset + sent as u64;
            state.set(Upload::Sending(percent(done, bytes)));
        })
    };
    if let Ok(upload) = request.upload() {
        upload.set_onprogress(Some(progress.as_ref().unchecked_ref()));
    }
    let ended = js_sys::Promise::new(&mut |resolve, _| {
        let _ = request.add_event_listener_with_callback("loadend", &resolve);
    });
    if request.send_with_opt_blob(Some(&body)).is_err() {
        return Came::Broke;
    }
    // (Stops a request whose connection stays open with nothing moving.)
    let finished = std::rc::Rc::new(Cell::new(false));
    {
        let (request, finished, last) = (request.clone(), finished.clone(), last.clone());
        spawn_local(async move {
            while !finished.get() {
                gloo_wait(1000).await;
                if !finished.get() && js_sys::Date::now() - last.get() > STALL_MS {
                    let _ = request.abort();
                }
            }
        });
    }
    let _ = wasm_bindgen_futures::JsFuture::from(ended).await;
    finished.set(true);
    drop(progress);
    let status = request.status().unwrap_or(0);
    let text = request.response_text().ok().flatten().unwrap_or_default();
    if status == 401 {
        relogin().await;
    }
    came_of(status, &text)
}

/// Keeps the screen on while `on`, where the browser can, and lets it go
/// otherwise.
fn hold_screen(on: bool) {
    use wasm_bindgen::JsValue;
    let release = |sentinel: JsValue| {
        if let Ok(release) = js_sys::Reflect::get(&sentinel, &"release".into())
            && let Ok(release) = release.dyn_into::<js_sys::Function>()
        {
            let _ = release.call0(&sentinel);
        }
    };
    if !on {
        if let Some(sentinel) = AWAKE.with(|awake| awake.borrow_mut().take()) {
            release(sentinel);
        }
        return;
    }
    let Ok(lock) = js_sys::Reflect::get(&window().navigator(), &"wakeLock".into()) else {
        return;
    };
    let Ok(request) = js_sys::Reflect::get(&lock, &"request".into()) else {
        return;
    };
    let Ok(request) = request.dyn_into::<js_sys::Function>() else {
        return;
    };
    let Ok(asked) = request.call1(&lock, &"screen".into()) else {
        return;
    };
    let Ok(asked) = asked.dyn_into::<js_sys::Promise>() else {
        return;
    };
    spawn_local(async move {
        let Ok(sentinel) = wasm_bindgen_futures::JsFuture::from(asked).await else {
            return;
        };
        // (Uploads that ended meanwhile need it no more.)
        if RUNNING.with(Cell::get) {
            if let Some(old) = AWAKE.with(|awake| awake.borrow_mut().replace(sentinel)) {
                release(old);
            }
        } else {
            release(sentinel);
        }
    });
}

/// A chip as the tab keeps it across a reload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub id: String,
    pub name: String,
    pub bytes: u64,
    pub feed: Option<u64>,
    pub done: bool,
}

/// Keeps the chips whose uploads Arko has for this tab, so a reload keeps
/// those done.
fn save() {
    let kept: Vec<Kept> = page().attached.with_untracked(|chips| {
        chips
            .iter()
            .filter_map(|chip| {
                Some(Kept {
                    id: chip.id.get_untracked()?,
                    name: chip.name.clone(),
                    bytes: chip.bytes,
                    feed: chip.feed,
                    done: chip.state.get_untracked() == Upload::Done,
                })
            })
            .collect()
    });
    let text = (!kept.is_empty())
        .then(|| serde_json::to_string(&kept).ok())
        .flatten();
    session_set(ATTACHED, text.as_deref());
}

/// The chips the tab kept before it reloaded, those done. The page can't
/// send the rest again, so their uploads are removed and the error line
/// names them.
pub fn restore() {
    let kept: Vec<Kept> = session_get(ATTACHED)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let (done, cut): (Vec<Kept>, Vec<Kept>) = kept.into_iter().partition(|kept| kept.done);
    let chips: Vec<Chip> = done
        .into_iter()
        .map(|kept| Chip {
            key: NEXT.with(|next| {
                next.set(next.get() + 1);
                next.get()
            }),
            name: kept.name,
            bytes: kept.bytes,
            preview: None,
            feed: kept.feed,
            state: RwSignal::new(Upload::Done),
            id: RwSignal::new(Some(kept.id)),
        })
        .collect();
    page().attached.set(chips);
    if let Some(first) = cut.first() {
        page().error.set(Some(match cut.len() {
            1 => format!("{} didn’t finish uploading. Attach it again.", first.name),
            n => format!("{n} files didn’t finish uploading. Attach them again."),
        }));
    }
    for kept in cut {
        spawn_local(async move {
            let _ = request("DELETE", &format!("/api/uploads/{}", kept.id), None).await;
        });
    }
    save();
}

/// Another Arko knows nothing of the chips' uploads, so they leave, and the
/// error line says so.
pub fn feed_changed() {
    // (Before the first feed comes, the chips' feed is still to be seen.)
    let Some(now) = feed() else {
        return;
    };
    let now = Some(now);
    let gone: Vec<u64> = page().attached.with_untracked(|chips| {
        chips
            .iter()
            .filter(|chip| chip.feed.is_some() && chip.feed != now)
            .map(|chip| chip.key)
            .collect()
    });
    if gone.is_empty() {
        return;
    }
    for key in &gone {
        remove(*key, true);
    }
    page()
        .error
        .set(Some("Arko restarted, so attach your files again.".into()));
}

/// The Attach button, which opens the device's file picker. The picker's
/// input is the button's own, so a phone opens it without a script.
#[component]
pub fn AttachButton() -> impl IntoView {
    let picked = move |event: web_sys::Event| {
        let Some(input) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
        else {
            return;
        };
        attach(files_of(input.files()));
        // (The same file can be picked again.)
        input.set_value("");
    };
    view! {
        <label class="btn ghost icon attach" title="Attach">
            <input type="file" multiple class="picker" aria-label="Attach" on:change=picked />
            <Paperclip size=18 />
        </label>
    }
}

/// The chips of the files attached, above the box.
#[component]
pub fn Chips() -> impl IntoView {
    let chips = page().attached;
    move || {
        let list = chips.get();
        (!list.is_empty()).then(|| {
            view! {
                <ul class="attached" aria-label="Attachments">
                    {list.into_iter().map(|chip| view! { <ChipView chip=chip /> }).collect_view()}
                </ul>
            }
        })
    }
}

#[component]
fn ChipView(chip: Chip) -> impl IntoView {
    let Chip {
        key,
        name,
        bytes,
        preview,
        state,
        ..
    } = chip;
    let failed = move || state.get() == Upload::Failed;
    let sending = move || matches!(state.get(), Upload::Sending(_) | Upload::Retrying(_));
    let retry = move |_| {
        state.set(Upload::Waiting);
        run();
    };
    let remove_label = format!("Remove {name}");
    let busy = move || match state.get() {
        Upload::Sending(percent) => Some(format!("Uploading, {percent}%")),
        _ => None,
    };
    view! {
        <li class="chip-file" class:failed=failed class:sending=sending aria-busy=move || sending().to_string()>
            {preview.map(|url| view! { <img class="chip-thumb" src=url alt="" /> })}
            <span class="chip-name" title=name.clone()>{short_name(&name)}</span>
            <span class="chip-meta" aria-label=busy>{move || state.get().words(bytes)}</span>
            {move || failed().then(|| view! {
                <button type="button" class="btn ghost small" on:click=retry>"Retry"</button>
            })}
            <button type="button" class="btn ghost icon chip-remove" aria-label=remove_label title="Remove"
                on:click=move |_| remove(key, false)>
                <X size=14 />
            </button>
        </li>
    }
}

/// Over the box while files are dragged over the page.
#[component]
pub fn DropOverlay(dropping: RwSignal<bool>) -> impl IntoView {
    view! {
        <Show when=move || dropping.get()>
            <div class="drop" aria-hidden="true">"Drop to attach"</div>
        </Show>
    }
}

/// Whether a drag carries files.
pub fn drags_files(event: &web_sys::DragEvent) -> bool {
    event.data_transfer().is_some_and(|transfer| {
        transfer
            .types()
            .iter()
            .any(|kind| kind.as_string().as_deref() == Some("Files"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_keeps_its_start_and_extension() {
        assert_eq!(short_name("IMG_0412.jpeg"), "IMG_0412.jpeg");
        assert_eq!(
            short_name("RPReplay_Final1730460000.MP4"),
            "RPReplay_Final…60000.MP4"
        );
        assert_eq!(short_name(&"é".repeat(30)).chars().count(), 24);
    }

    #[test]
    fn a_chip_says_its_size_unless_something_is_under_way_or_wrong() {
        assert_eq!(Upload::Waiting.words(48_200_000), "46.0 MB");
        assert_eq!(Upload::Sending(34).words(48_200_000), "34%");
        assert_eq!(Upload::Retrying(34).words(48_200_000), "Retrying");
        assert_eq!(Upload::Failed.words(48_200_000), "Failed");
        assert_eq!(Upload::Done.words(3_000), "3 KB");
    }

    #[test]
    fn tries_again_wait_longer_up_to_half_a_minute() {
        let waits: Vec<i32> = (1..=8).map(backoff_ms).collect();
        assert_eq!(
            waits,
            [1000, 2000, 4000, 8000, 15_000, 30_000, 30_000, 30_000]
        );
    }

    #[test]
    fn arkos_answers_say_where_an_upload_goes_on_from() {
        let state = r#"{"received": 10, "size": 20, "done": false}"#;
        assert!(matches!(
            came_of(200, state),
            Came::Holds {
                received: 10,
                done: false
            }
        ));
        let done = r#"{"received": 20, "size": 20, "done": true, "kind": "file"}"#;
        assert!(matches!(came_of(200, done), Came::Holds { done: true, .. }));
        let elsewhere = r#"{"error": "Arko holds another part of the file.", "received": 7}"#;
        assert!(matches!(came_of(409, elsewhere), Came::Elsewhere(7)));
        let stalled = r#"{"error": "The upload stopped.", "received": 7}"#;
        assert!(matches!(came_of(408, stalled), Came::Broke));
        assert!(matches!(came_of(0, ""), Came::Broke));
        assert!(matches!(came_of(503, ""), Came::Broke));
        let refused = r#"{"error": "There isn’t room for a.mov. The computer has 1.2 GB free.", "received": null}"#;
        let Came::Refused(words) = came_of(507, refused) else {
            panic!("refused");
        };
        assert_eq!(
            words,
            "There isn’t room for a.mov. The computer has 1.2 GB free."
        );
        let unwritable = r#"{"error": "Couldn't make /w/.arko/attachments (permission denied).", "received": null}"#;
        assert!(matches!(came_of(409, unwritable), Came::Refused(_)));
        assert!(
            matches!(came_of(500, "oops"), Came::Refused(words) if words == "Couldn’t attach the file.")
        );
    }

    #[test]
    fn send_waits_for_every_upload() {
        let _page = crate::testing::page();
        let chip = |state, id: Option<&str>| Chip {
            key: 1,
            name: "a".into(),
            bytes: 1,
            preview: None,
            feed: None,
            state: RwSignal::new(state),
            id: RwSignal::new(id.map(str::to_string)),
        };
        assert_eq!(ready(&[]), Some(Vec::new()));
        assert_eq!(
            ready(&[
                chip(Upload::Done, Some("u1")),
                chip(Upload::Done, Some("u2"))
            ]),
            Some(vec!["u1".to_string(), "u2".to_string()])
        );
        assert_eq!(
            ready(&[
                chip(Upload::Done, Some("u1")),
                chip(Upload::Sending(5), Some("u2"))
            ]),
            None
        );
        assert_eq!(ready(&[chip(Upload::Failed, Some("u1"))]), None);
    }
}
