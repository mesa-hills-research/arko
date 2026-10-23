//! The remote's HTTP server: the page, the key exchange, the event stream
//! (Server-Sent Events) and the actions (POST).
//!
//! The page is open to all; everything under `/api/` but the key exchange
//! needs a session cookie from that exchange, or the key itself as a bearer
//! token. The key never travels in a URL: the link's fragment, which
//! browsers don't send, carries it to the page.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use super::feed::{Activity, Feed, Update};
use super::{Action, Info, Meter, Reading};

const INDEX: &str = include_str!("page/index.html");
const SCRIPT: &str = include_str!("page/app.js");
const STYLE: &str = include_str!("page/app.css");
/// The mark, for the lock screen.
const ICON: &str = include_str!("page/icon.svg");
/// The mark drawn for 16 to 32 pixels, for the tab's icon.
const ICON_SMALL: &str = include_str!("page/icon-small.svg");
/// The mark on a tile, at 180 pixels, for a phone's home screen.
const APPLE_ICON: &[u8] = include_bytes!("page/apple-touch-icon.png");
const CORE: &str = include_str!("page/core.js");
/// The page's monospace font, the same on every device (SIL Open Font
/// License 1.1; see NOTICE).
const MONO_FONT: &[u8] = include_bytes!("page/fonts/JetBrainsMono-Regular.ttf");

/// The largest request body: a long message, pasted.
const MAX_BODY: usize = 512 * 1024;
/// Wrong keys an address may send before it must wait...
const FREE_FAILURES: u32 = 3;
/// ...a second, then twice as long after each other one, up to this.
const MAX_WAIT: Duration = Duration::from_secs(300);
/// Session cookies kept; the oldest ones end first.
const MAX_SESSIONS: usize = 64;
/// How often a stream looks for changes in the status (cost, workers).
const STATUS_EVERY: Duration = Duration::from_secs(2);

/// A running server, which stops when it's dropped.
pub struct Server {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    /// Serves on `addr`, with `key`. The feed goes out, the actions come in
    /// on `actions`, and `meter` tells the status.
    pub fn start(
        addr: SocketAddr,
        key: String,
        info: Info,
        feed: Feed,
        actions: mpsc::UnboundedSender<Action>,
        meter: Arc<dyn Meter>,
    ) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let addr = listener.local_addr()?;
        let (stop, closed) = watch::channel(false);
        let app = Arc::new(App {
            addr,
            key,
            // Cookies don't tell ports apart: each server has its own.
            cookie: format!("arko_remote_{}", addr.port()),
            sessions: Mutex::new(VecDeque::new()),
            failures: Mutex::new(HashMap::new()),
            feed,
            actions,
            meter,
            info,
            closed: closed.clone(),
        });
        let router = router(app);
        let mut shutdown = closed;
        let task = tokio::spawn(async move {
            let service = router.into_make_service_with_connect_info::<SocketAddr>();
            let result = axum::serve(listener, service)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.wait_for(|closed| *closed).await;
                })
                .await;
            if let Err(error) = result {
                log::warn!("the remote's server stopped: {error}");
            }
        });
        Ok(Self {
            addr,
            stop,
            task: Some(task),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops serving, and waits (a few seconds at most) until the port is
    /// free. The streams end, and the session cookies end with the server.
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(3), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    /// The streams end, the connections close, and the session cookies
    /// end with the server.
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(mut task) = self.task.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if tokio::time::timeout(Duration::from_secs(3), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            });
        }
    }
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { file(INDEX, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { file(SCRIPT, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/core.js",
            get(|| async { file(CORE, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/fonts/JetBrainsMono-Regular.ttf",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "font/ttf"),
                        // It never changes under its name.
                        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                    ],
                    MONO_FONT,
                )
                    .into_response()
            }),
        )
        .route(
            "/app.css",
            get(|| async { file(STYLE, "text/css; charset=utf-8") }),
        )
        .route("/icon.svg", get(|| async { file(ICON, "image/svg+xml") }))
        .route(
            "/icon-small.svg",
            get(|| async { file(ICON_SMALL, "image/svg+xml") }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], APPLE_ICON).into_response() }),
        )
        .route("/api/session", get(session_check).post(session_open))
        .route("/api/events", get(events))
        .route("/api/message", axum::routing::post(message))
        .route("/api/answer", axum::routing::post(answer))
        .route("/api/stop", axum::routing::post(stop))
        .route("/api/background", axum::routing::post(background))
        .route("/api/jobs/stop", axum::routing::post(stop_job))
        .route("/api/links", get(links))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::map_response(harden))
        .with_state(app)
}

fn file(text: &'static str, kind: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], text).into_response()
}

/// Headers every response gets: no caching, sniffing, framing or
/// referrers, and a page that loads only its own files.
async fn harden(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

struct App {
    /// Where the server listens.
    addr: SocketAddr,
    key: String,
    /// The session cookie's name.
    cookie: String,
    /// The session cookies given out.
    sessions: Mutex<VecDeque<String>>,
    /// Wrong keys, by address.
    failures: Mutex<HashMap<IpAddr, Failures>>,
    feed: Feed,
    actions: mpsc::UnboundedSender<Action>,
    meter: Arc<dyn Meter>,
    info: Info,
    /// Set when the server stops: the streams end.
    closed: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    /// No key is checked from the address until then.
    until: Instant,
    last: Instant,
}

/// Why a request is refused.
enum Refusal {
    /// No session cookie and no right key.
    Unauthorized,
    /// Too many wrong keys from the address: it waits this long.
    Wait(Duration),
    /// Sent from another site's page.
    CrossSite,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            Refusal::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                error_body("open the link Arko printed, which holds the key"),
            )
                .into_response(),
            Refusal::Wait(wait) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, wait.as_secs().max(1).to_string())],
                error_body("too many wrong keys. Wait, then try again"),
            )
                .into_response(),
            Refusal::CrossSite => (
                StatusCode::FORBIDDEN,
                error_body("requests from other sites are refused"),
            )
                .into_response(),
        }
    }
}

fn error_body(message: &str) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "error": message }))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether the two texts are equal, in a time that doesn't depend on
/// where they differ.
fn same(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

impl App {
    fn wait(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        lock(&self.failures)
            .get(&ip)
            .filter(|failures| failures.until > now)
            .map(|failures| failures.until - now)
    }

    /// Checks a key from `ip`, which may have to wait first.
    fn check_key(&self, ip: IpAddr, key: &str) -> Result<(), Refusal> {
        if let Some(wait) = self.wait(ip) {
            return Err(Refusal::Wait(wait));
        }
        let mut failures = lock(&self.failures);
        if same(key, &self.key) {
            failures.remove(&ip);
            return Ok(());
        }
        let now = Instant::now();
        if failures.len() > 1000 {
            failures.retain(|_, failures| now.duration_since(failures.last) < MAX_WAIT * 2);
        }
        let entry = failures.entry(ip).or_insert(Failures {
            count: 0,
            until: now,
            last: now,
        });
        entry.count += 1;
        entry.last = now;
        if entry.count >= FREE_FAILURES {
            let doublings = (entry.count - FREE_FAILURES).min(16);
            entry.until = now + Duration::from_secs(1 << doublings).min(MAX_WAIT);
        }
        log::warn!("the remote refused a wrong key from {ip}");
        Err(Refusal::Unauthorized)
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        let Some(token) = cookie(headers, &self.cookie) else {
            return false;
        };
        lock(&self.sessions)
            .iter()
            .any(|session| same(session, token))
    }

    /// Lets in a request with a session cookie, or the key as a bearer
    /// token.
    fn authorize(&self, headers: &HeaderMap, ip: IpAddr) -> Result<(), Refusal> {
        if cross_site(headers) {
            return Err(Refusal::CrossSite);
        }
        if self.has_session(headers) {
            return Ok(());
        }
        match bearer(headers) {
            Some(key) => self.check_key(ip, key),
            None => Err(Refusal::Unauthorized),
        }
    }

    fn new_session(&self) -> String {
        let token = super::random_token(32);
        let mut sessions = lock(&self.sessions);
        if sessions.len() >= MAX_SESSIONS {
            sessions.pop_front();
        }
        sessions.push_back(token.clone());
        token
    }

    /// The status the page shows.
    fn status(&self, activity: &Activity) -> String {
        #[derive(Serialize)]
        struct Status<'a> {
            #[serde(flatten)]
            activity: &'a Activity,
            #[serde(flatten)]
            reading: Reading,
            info: Info,
        }
        let reading = self.meter.read();
        // The main agent's model as the session runs it now: `/model`
        // changes it before the conversation starts, also after the page's
        // server started.
        let mut info = self.info.clone();
        if !reading.models.banner_model.is_empty() {
            info.model = reading.models.banner_model.clone();
        }
        serde_json::to_string(&Status {
            activity,
            reading,
            info,
        })
        .unwrap_or_default()
    }

    fn act(&self, action: Action) -> Response {
        match self.actions.send(action) {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                error_body("the session is closing"),
            )
                .into_response(),
        }
    }
}

/// The value of the cookie `name`.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// Whether a browser sent the request from another site's page (another
/// port of this host counts: cookies don't tell ports apart). Clients that
/// aren't browsers send neither header.
fn cross_site(headers: &HeaderMap) -> bool {
    let fetched_from = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    if matches!(fetched_from, Some("cross-site" | "same-site")) {
        return true;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return false;
    };
    let Some(origin) = origin.to_str().ok().and_then(|origin| {
        origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
    }) else {
        return true;
    };
    // Behind a proxy (Tailscale Serve, say), the host the browser used.
    ![header::HOST.as_str(), "x-forwarded-host"]
        .iter()
        .filter_map(|name| headers.get(*name).and_then(|value| value.to_str().ok()))
        .any(|host| host.eq_ignore_ascii_case(origin))
}

type Peer = ConnectInfo<SocketAddr>;

/// `GET /api/session`: 204 with a session (or the key), else 401.
async fn session_check(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
) -> Response {
    match app.authorize(&headers, peer.ip()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

/// `POST /api/session` with `{"key": ...}`: the key for a session cookie.
async fn session_open(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if cross_site(&headers) {
        return Refusal::CrossSite.into_response();
    }
    let Ok(body) = serde_json::from_slice::<KeyBody>(&body) else {
        return (StatusCode::BAD_REQUEST, error_body("send {\"key\": ...}")).into_response();
    };
    if let Err(refusal) = app.check_key(peer.ip(), body.key.trim()) {
        return refusal.into_response();
    }
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        app.cookie,
        app.new_session()
    );
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

/// `GET /api/events`: the stream. It starts with `reset` (the history, the
/// workers, the background jobs and the status), then sends `item` (a new
/// item), `append` (more text for an item), `change` (an item, changed: a
/// call's outcome), `worker` (a worker's summary), `job` (a background
/// job's summary) and `status` events. A stream that falls
/// behind starts over with a `reset`.
async fn events(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    let mut stream = Stream {
        closed: app.closed.clone(),
        updates: app.feed.subscribe().1,
        app,
        pending: VecDeque::new(),
        status: String::new(),
        tick: tokio::time::interval(STATUS_EVERY),
        ending: false,
    };
    stream.reset();
    let events = futures_util::stream::unfold(stream, Stream::next);
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

struct Stream {
    app: Arc<App>,
    updates: broadcast::Receiver<Update>,
    closed: watch::Receiver<bool>,
    pending: VecDeque<Event>,
    /// The status last sent.
    status: String,
    tick: tokio::time::Interval,
    /// The server stopped: the stream ends once `pending` is sent.
    ending: bool,
}

impl Stream {
    /// Starts over: the history and the status.
    fn reset(&mut self) {
        let (snapshot, updates) = self.app.feed.subscribe();
        self.updates = updates;
        self.pending.clear();
        self.status = self.app.status(&snapshot.activity);
        let status: serde_json::Value = serde_json::from_str(&self.status).unwrap_or_default();
        self.pending.push_back(
            Event::default().event("reset").data(
                serde_json::json!({
                    "items": snapshot.items,
                    "workers": snapshot.workers,
                    "jobs": snapshot.jobs,
                    "status": status,
                })
                .to_string(),
            ),
        );
    }

    fn send_status(&mut self) {
        let status = self.app.status(&self.app.feed.activity());
        if status != self.status {
            self.pending
                .push_back(Event::default().event("status").data(status.clone()));
            self.status = status;
        }
    }

    fn take(&mut self, update: Update) {
        match update {
            Update::Item(item) => self.pending.push_back(
                Event::default()
                    .event("item")
                    .data(serde_json::to_string(&item).unwrap_or_default()),
            ),
            Update::Append { id, text } => self.pending.push_back(
                Event::default()
                    .event("append")
                    .data(serde_json::json!({ "id": id, "text": text }).to_string()),
            ),
            Update::Change(item) => self.pending.push_back(
                Event::default()
                    .event("change")
                    .data(serde_json::to_string(&item).unwrap_or_default()),
            ),
            Update::Worker(summary) => self.pending.push_back(
                Event::default()
                    .event("worker")
                    .data(serde_json::to_string(&summary).unwrap_or_default()),
            ),
            // (The page reads a worker's end in its summary.)
            Update::WorkerEnded { .. } => {}
            Update::Activity => self.send_status(),
            Update::Remove { id } => self.pending.push_back(
                Event::default()
                    .event("remove")
                    .data(serde_json::json!({ "id": id }).to_string()),
            ),
            Update::Job(summary) => self.pending.push_back(
                Event::default()
                    .event("job")
                    .data(serde_json::to_string(&summary).unwrap_or_default()),
            ),
            // The page starts over from the history as it is now.
            Update::Truncate { .. } => self.reset(),
        }
    }

    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some((Ok(event), self));
            }
            if self.ending {
                return None;
            }
            if *self.closed.borrow() {
                // What came before the server stopped (why it stops, say)
                // goes out, then the stream ends.
                while let Ok(update) = self.updates.try_recv() {
                    self.take(update);
                }
                self.ending = true;
                continue;
            }
            tokio::select! {
                changed = self.closed.changed() => {
                    if changed.is_err() {
                        self.ending = true;
                    }
                }
                update = self.updates.recv() => match update {
                    Ok(update) => self.take(update),
                    Err(broadcast::error::RecvError::Lagged(_)) => self.reset(),
                    Err(broadcast::error::RecvError::Closed) => self.ending = true,
                },
                _ = self.tick.tick() => self.send_status(),
            }
        }
    }
}

#[derive(Deserialize)]
struct MessageBody {
    text: String,
    /// The worker whose view it was sent from: what isn't a command goes
    /// to that worker, as the full-screen view sends it.
    #[serde(default)]
    to: Option<String>,
}

/// The text as typed: line ends as `\n`, and not blank.
fn typed(text: &str) -> Option<String> {
    let text = text.replace("\r\n", "\n");
    (!text.trim().is_empty()).then_some(text)
}

/// `POST /api/message` with `{"text": ...}`: as if typed at the prompt.
async fn message(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    match serde_json::from_slice::<MessageBody>(&body)
        .ok()
        .and_then(|body| Some((typed(&body.text)?, body.to)))
    {
        // Text like "/usr/bin is missing" is a message, not a command.
        Some((text, Some(worker)))
            if crate::commands::find(&text).is_none()
                && !worker.is_empty()
                && !worker.contains(char::is_whitespace) =>
        {
            app.act(Action::Message(format!(
                "/message {worker} {}",
                text.trim()
            )))
        }
        Some((text, _)) => app.act(Action::Message(text)),
        None => (StatusCode::BAD_REQUEST, error_body("send {\"text\": ...}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AnswerBody {
    question: u64,
    text: String,
}

/// `POST /api/answer` with `{"question": ID, "text": ...}`: answers the
/// agent's question, if it is still the one asked.
async fn answer(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    let Some((question, text)) = serde_json::from_slice::<AnswerBody>(&body)
        .ok()
        .and_then(|body| Some((body.question, typed(&body.text)?)))
    else {
        return (
            StatusCode::BAD_REQUEST,
            error_body("send {\"question\": ID, \"text\": ...}"),
        )
            .into_response();
    };
    if app
        .feed
        .activity()
        .question
        .is_none_or(|asked| asked.id != question)
    {
        return (
            StatusCode::CONFLICT,
            error_body("that question is no longer asked"),
        )
            .into_response();
    }
    app.act(Action::Answer { question, text })
}

/// A link to the page for `GET /api/links`.
#[derive(Serialize)]
struct LinkOut {
    url: String,
    interface: Option<String>,
    kind: super::LinkKind,
    /// The link as a QR code, in SVG.
    qr_svg: Option<String>,
}

/// `GET /api/links`: the links to the page, best first, with their QR
/// codes, for opening it on another device. They hold the key, as the
/// links Arko prints do.
async fn links(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    let links: Vec<LinkOut> = super::links(app.addr, &app.key)
        .0
        .into_iter()
        .map(|link| LinkOut {
            qr_svg: super::qr_svg(&link.url),
            url: link.url,
            interface: link.interface,
            kind: link.kind,
        })
        .collect();
    axum::Json(links).into_response()
}

/// `POST /api/stop`: stops the turn, as Ctrl-C does.
async fn stop(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    app.act(Action::Stop)
}

#[derive(Deserialize)]
struct BackgroundBody {
    #[serde(default)]
    item: Option<u64>,
}

/// `POST /api/background` with `{"item": ID}`: moves that running call to
/// the background, or with `{}` the main agent's calls that can move.
async fn background(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    let body = if body.is_empty() {
        Ok(BackgroundBody { item: None })
    } else {
        serde_json::from_slice::<BackgroundBody>(&body)
    };
    let Ok(body) = body else {
        return (StatusCode::BAD_REQUEST, error_body("send {\"item\": ID}")).into_response();
    };
    app.act(Action::Background { item: body.item })
}

#[derive(Deserialize)]
struct StopJobBody {
    job: uuid::Uuid,
}

/// `POST /api/jobs/stop` with `{"job": ID}`: stops that background job.
async fn stop_job(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refusal) = app.authorize(&headers, peer.ip()) {
        return refusal.into_response();
    }
    let Ok(body) = serde_json::from_slice::<StopJobBody>(&body) else {
        return (StatusCode::BAD_REQUEST, error_body("send {\"job\": ID}")).into_response();
    };
    app.act(Action::StopJob { job: body.job })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words of the workers' settings on the page are the library's, the
    /// ones the terminal and an app show: the page's script and markup hold
    /// them as the library writes them.
    #[test]
    fn the_pages_worker_settings_use_the_librarys_words() {
        use crate::workers::{self, ModelChoice};
        // The script writes each choice as `name: "..."` and `about: "..."`.
        for choice in ModelChoice::ALL {
            assert!(
                CORE.contains(&format!("name: \"{}\"", choice.label())),
                "{}",
                choice.label()
            );
            assert!(
                CORE.contains(&format!("about: \"{}\"", choice.about())),
                "{}",
                choice.about()
            );
        }
        assert!(CORE.contains(&format!("\"{}\"", workers::depth_about())));
        // The page writes the cap's sentence for a number from a template.
        assert!(CORE.contains(&format!("\"{}\"", workers::retry_cap_about(None))));
        for on in [true, false] {
            assert!(
                CORE.contains(&format!("\"{}\"", workers::copies_about(on))),
                "{on}"
            );
        }
        // The markup names the settings and says what the first does.
        for name in [
            workers::MODELS_NAME,
            workers::DEPTH_NAME,
            workers::RETRY_CAP_NAME,
            workers::COPIES_NAME,
        ] {
            assert!(INDEX.contains(&format!(">{name}<")), "{name}");
            assert!(INDEX.contains(&format!("aria-label=\"{name}\"")), "{name}");
        }
        assert!(INDEX.contains(&format!(
            "{} {}",
            workers::MODELS_ABOUT,
            workers::MODELS_APPLIES
        )));
        // No curly apostrophe next to the straight ones.
        assert!(!INDEX.contains("Session\u{2019}s"));
    }

    /// The words of the built-in skills setting on the page are the
    /// library's, and its markup has the switch.
    #[test]
    fn the_pages_built_in_skills_setting_uses_the_librarys_words() {
        use crate::builtin_skills::{AFTER_START, SETTING_ABOUT, SETTING_NAME};
        assert!(CORE.contains(SETTING_ABOUT));
        assert!(CORE.contains(AFTER_START));
        assert!(INDEX.contains(&format!(">{SETTING_NAME}<")));
        assert!(INDEX.contains(&format!("aria-label=\"{SETTING_NAME}\"")));
    }

    /// A worker's meta line holds its model, profile and cost, which a phone
    /// would cut at the end if the line couldn't wrap.
    #[test]
    fn a_workers_meta_line_wraps_on_a_narrow_screen() {
        let rule = STYLE
            .lines()
            .find(|line| line.starts_with(".wcard .wmeta {"))
            .expect("the meta line's rule");
        assert!(
            !rule.contains("nowrap") && !rule.contains("ellipsis"),
            "{rule}"
        );
    }

    #[test]
    fn cookies_and_bearer_tokens_are_read() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=b; arko_remote_1=tok; c=d"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer  k3y "),
        );
        assert_eq!(cookie(&headers, "arko_remote_1"), Some("tok"));
        assert_eq!(cookie(&headers, "arko_remote_2"), None);
        assert_eq!(bearer(&headers), Some("k3y"));
    }

    #[test]
    fn other_sites_are_told_apart() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, HeaderValue::from_static(value));
            }
            headers
        };
        assert!(!cross_site(&headers(&[("host", "127.0.0.1:8765")])));
        assert!(!cross_site(&headers(&[
            ("host", "127.0.0.1:8765"),
            ("origin", "http://127.0.0.1:8765"),
            ("sec-fetch-site", "same-origin"),
        ])));
        assert!(cross_site(&headers(&[
            ("host", "127.0.0.1:8765"),
            ("origin", "http://127.0.0.1:3000"),
        ])));
        assert!(cross_site(&headers(&[
            ("host", "127.0.0.1:8765"),
            ("sec-fetch-site", "same-site"),
        ])));
        assert!(cross_site(&headers(&[
            ("host", "127.0.0.1:8765"),
            ("origin", "null"),
        ])));
        assert!(!cross_site(&headers(&[
            ("host", "127.0.0.1:8765"),
            ("x-forwarded-host", "box.tailnet.ts.net"),
            ("origin", "https://box.tailnet.ts.net"),
        ])));
    }

    // ----- the server, in process -----

    const KEY: &str = "Zm9yIHRlc3RzIG9ubHkgMDEyMzQ1";

    struct Still;

    impl Meter for Still {
        fn read(&self) -> Reading {
            Reading {
                cost: 1.25,
                ..Reading::default()
            }
        }
    }

    struct Served {
        server: Server,
        actions: mpsc::UnboundedReceiver<Action>,
        base: String,
        http: reqwest::Client,
    }

    /// A meter whose reading a test changes.
    struct Moving(Mutex<Reading>);

    impl Meter for Moving {
        fn read(&self) -> Reading {
            lock(&self.0).clone()
        }
    }

    fn serve_on(feed: &Feed, addr: &str) -> Served {
        serve_with(feed, addr, Info::default(), Arc::new(Still))
    }

    fn serve_with(feed: &Feed, addr: &str, info: Info, meter: Arc<dyn Meter>) -> Served {
        let (actions, received) = mpsc::unbounded_channel();
        let server = Server::start(
            addr.parse().unwrap(),
            KEY.into(),
            info,
            feed.clone(),
            actions,
            meter,
        )
        .unwrap();
        Served {
            base: format!("http://{}", server.addr()),
            server,
            actions: received,
            http: arko_ai::http_client(),
        }
    }

    fn serve(feed: &Feed) -> Served {
        serve_on(feed, "127.0.0.1:0")
    }

    impl Served {
        async fn post(&self, path: &str, cookie: Option<&str>, body: &str) -> reqwest::Response {
            let mut request = self
                .http
                .post(format!("{}{path}", self.base))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            request.send().await.unwrap()
        }

        async fn get(&self, path: &str, cookie: Option<&str>) -> reqwest::Response {
            let mut request = self.http.get(format!("{}{path}", self.base));
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            request.send().await.unwrap()
        }

        /// Trades the key for a session cookie, as `name=value`.
        async fn login(&self) -> String {
            let response = self
                .post("/api/session", None, &format!(r#"{{"key": "{KEY}"}}"#))
                .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
            cookie.split(';').next().unwrap().to_string()
        }
    }

    /// Reads a stream's events until `done` holds for one; returns them,
    /// as (event, data).
    async fn read_until(
        stream: &mut reqwest::Response,
        buffer: &mut String,
        done: impl Fn(&str, &serde_json::Value) -> bool,
    ) -> Vec<(String, serde_json::Value)> {
        let mut events = Vec::new();
        loop {
            while let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let mut name = "message".to_string();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        name = value.trim().to_string();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push_str(value.trim_start());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                let data: serde_json::Value = serde_json::from_str(&data).unwrap();
                let stop = done(&name, &data);
                events.push((name, data));
                if stop {
                    return events;
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
                .await
                .expect("an event in time")
                .unwrap()
                .expect("the stream goes on");
            buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// `GET /api/links` answers 401 without a session or the key, and
    /// with one the page's links, best first, each with its kind and QR
    /// code; from another site's page, 403.
    #[tokio::test]
    async fn the_links_with_their_qr_codes_need_a_session() {
        let feed = Feed::new("main");
        let served = serve_on(&feed, "0.0.0.0:0");
        assert_eq!(
            served.get("/api/links", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let cookie = served.login().await;
        let response = served.get("/api/links", Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let links: Vec<serde_json::Value> = response.json().await.unwrap();
        let last = links.last().unwrap();
        assert_eq!(last["kind"], "loopback");
        assert!(
            last["url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/#key={KEY}"))
        );
        for link in &links {
            assert!(
                link["qr_svg"].as_str().unwrap().starts_with("<svg"),
                "{link}"
            );
            assert!(
                ["lan", "tailscale", "other", "vpn", "loopback"]
                    .contains(&link["kind"].as_str().unwrap())
            );
        }
        let bearer = served
            .http
            .get(format!("{}/api/links", served.base))
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .send()
            .await
            .unwrap();
        assert_eq!(bearer.status(), StatusCode::OK);
        let cross = served
            .http
            .get(format!("{}/api/links", served.base))
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .unwrap();
        assert_eq!(cross.status(), StatusCode::FORBIDDEN);
    }

    /// The tab's icon, the lock screen's mark and the phone's home screen
    /// icon load before the key is entered, as what the page links to. The
    /// home screen icon is the 180 pixel PNG iOS asks for.
    #[tokio::test]
    async fn the_pages_icons_load_without_the_key() {
        let feed = Feed::new("main");
        let served = serve(&feed);
        for (path, kind) in [
            ("/icon.svg", "image/svg+xml"),
            ("/icon-small.svg", "image/svg+xml"),
            ("/apple-touch-icon.png", "image/png"),
        ] {
            let response = served.get(path, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], kind, "{path}");
            let body = response.bytes().await.unwrap();
            assert!(!body.is_empty() && body.len() < 8 * 1024, "{path}");
            if kind == "image/png" {
                // The header's width and height, after the 8 byte signature
                // and the chunk's length and name.
                assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
                assert_eq!(&body[16..24], [0, 0, 0, 180, 0, 0, 0, 180]);
            } else {
                assert!(body.starts_with(b"<svg"), "{path}");
            }
        }
        assert!(INDEX.contains(r#"<link rel="icon" href="/icon-small.svg""#));
        assert!(INDEX.contains(r#"<link rel="apple-touch-icon" href="/apple-touch-icon.png">"#));
        assert!(INDEX.contains(r#"<img src="/icon.svg""#));
    }

    /// The page is open to all; the API answers 401 without the key, and
    /// the key buys an HttpOnly, SameSite=Strict cookie that opens it.
    #[tokio::test]
    async fn the_api_needs_the_key_which_buys_a_session_cookie() {
        let feed = Feed::new("main");
        let mut served = serve(&feed);
        let page = served.get("/", None).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert!(
            page.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("script-src 'self'")
        );
        assert!(page.text().await.unwrap().contains("/app.js"));

        for path in ["/api/session", "/api/events"] {
            let response = served.get(path, None).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        let response = served
            .post("/api/message", None, r#"{"text": "rm -rf"}"#)
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = served
            .post(
                "/api/message",
                Some("arko_remote_1=guess"),
                r#"{"text": "hi"}"#,
            )
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(served.actions.try_recv().is_err());

        let response = served
            .post("/api/session", None, r#"{"key": "not-the-key"}"#)
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());

        let response = served
            .post("/api/session", None, &format!(r#"{{"key": "{KEY}"}}"#))
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let set = response.headers()[header::SET_COOKIE].to_str().unwrap();
        let port = served.server.addr().port();
        assert!(set.starts_with(&format!("arko_remote_{port}=")), "{set}");
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
        assert!(!set.contains(KEY), "the cookie isn't the key");
        let cookie = set.split(';').next().unwrap();
        let response = served.get("/api/session", Some(cookie)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // The key as a bearer token, for clients that aren't browsers.
        let response = served
            .http
            .get(format!("{}/api/session", served.base))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Another site's page can't use the cookie.
        let response = served
            .http
            .post(format!("{}/api/message", served.base))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .body(r#"{"text": "hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(served.actions.try_recv().is_err());
    }

    /// After three wrong keys, an address waits, longer after each other
    /// one, even with the right key.
    #[tokio::test]
    async fn wrong_keys_make_the_address_wait() {
        let feed = Feed::new("main");
        let served = serve(&feed);
        for _ in 0..FREE_FAILURES {
            let response = served
                .post("/api/session", None, r#"{"key": "wrong"}"#)
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = served
            .post("/api/session", None, &format!(r#"{{"key": "{KEY}"}}"#))
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().get(header::RETRY_AFTER).is_some());
        // A wrong bearer token counts too.
        let response = served
            .http
            .get(format!("{}/api/events", served.base))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        served.login().await;
    }

    /// The stream starts with the history and the status, then sends what
    /// follows: new items, more text, the status as it changes.
    #[tokio::test]
    async fn the_stream_sends_the_history_then_what_follows() {
        use arko_agents::AgentEvent;
        use arko_workflows::WorkflowEvent;
        let feed = Feed::new("main");
        feed.notice(super::super::Level::Info, "Resumed.");
        let served = serve(&feed);
        let cookie = served.login().await;
        let mut stream = served.get("/api/events", Some(&cookie)).await;
        assert_eq!(stream.status(), StatusCode::OK);
        assert!(
            stream.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let mut buffer = String::new();
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        let (_, reset) = events.last().unwrap();
        assert_eq!(reset["items"][0]["text"], "Resumed.");
        assert_eq!(reset["status"]["working"], false);
        assert_eq!(reset["status"]["cost"], 1.25);
        assert_eq!(reset["status"]["settings"]["outside"], false);
        assert_eq!(reset["workers"], serde_json::json!([]));

        feed.turn_started();
        let chunk = |text: &str| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: uuid::Uuid::nil(),
            agent_name: "main".into(),
            agent_run_id: uuid::Uuid::nil(),
            event: AgentEvent::AssistantResponseChunk(text.into()),
        };
        feed.event(&chunk("Look"), None);
        feed.event(&chunk("ing."), None);
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "append").await;
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["status", "item", "append"]);
        assert_eq!(events[0].1["working"], true);
        assert_eq!(events[1].1["kind"], "text");
        assert_eq!(events[1].1["text"], "Look");
        assert_eq!(events[2].1["text"], "ing.");
        assert_eq!(events[2].1["id"], events[1].1["id"]);

        // A call's outcome comes as a change of it; a worker's summary as
        // `worker`.
        let call = uuid::Uuid::new_v4();
        let event = |event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: uuid::Uuid::nil(),
            agent_name: "main".into(),
            agent_run_id: uuid::Uuid::nil(),
            event,
        };
        feed.event(
            &event(AgentEvent::ToolCallFormatted {
                tool_call_id: call,
                tool_name: "read_file".into(),
                parameters: [("path".to_string(), "a.rs".into())].into(),
            }),
            None,
        );
        feed.event(
            &event(AgentEvent::ToolResult {
                tool_call_id: call,
                tool_name: "read_file".into(),
                output: "1 | a\n2 | b".into(),
            }),
            None,
        );
        feed.worker(
            "r1",
            "Count the files.",
            super::super::WorkerChange::Started,
        );
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "worker").await;
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["item", "change", "item", "worker"]);
        assert_eq!(events[0].1["status"], "running");
        assert_eq!(events[1].1["id"], events[0].1["id"]);
        assert_eq!(events[1].1["status"], "ok");
        assert_eq!(events[1].1["outcome"], "2 lines");
        assert_eq!(events[2].1["kind"], "worker_line");
        assert_eq!(events[2].1["worker"], "r1");
        assert_eq!(events[3].1["title"], "Count the files.");
        assert_eq!(events[3].1["state"], "running");

        // A note waits, pending, and moves once the agent reads it. The
        // page drops it and gets it again, with no new history.
        feed.note("Also the docs.", super::super::Source::Web, 1, Vec::new());
        feed.event(
            &event(AgentEvent::ContextInjected {
                ids: vec!["note:1".into()],
                bytes: 20,
            }),
            None,
        );
        let events = read_until(&mut stream, &mut buffer, |name, data| {
            name == "item" && data["kind"] == "user" && data.get("pending").is_none()
        })
        .await;
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["item", "remove", "item"]);
        assert_eq!(events[0].1["pending"], true);
        assert_eq!(events[1].1["id"], events[0].1["id"]);
        assert_eq!(events[2].1["text"], "Also the docs.");

        // A background job's summary comes as `job`, and a page that
        // connects later gets the jobs with the history.
        let job = uuid::Uuid::new_v4();
        feed.job_started(super::super::JobStart {
            id: job,
            agent: "main".into(),
            tool_call: None,
            kind: super::super::JobKind::Call,
            label: "index_create".into(),
            started_at: 0,
            monitor: None,
        });
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "job").await;
        let (_, summary) = events.last().unwrap();
        assert_eq!(summary["id"], job.to_string());
        assert_eq!(summary["kind"], "call");
        assert_eq!(summary["state"], "running");
        assert_eq!(summary["label"], "index_create");
        let mut later = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut later, &mut buffer, |name, _| name == "reset").await;
        assert_eq!(events.last().unwrap().1["jobs"][0]["id"], job.to_string());
    }

    /// The status carries each worker's context and the size it hands over
    /// at, which the page's header shows in a worker's own view. A session
    /// without workers sends none.
    #[tokio::test]
    async fn the_status_carries_each_workers_context() {
        let feed = Feed::new("main");
        let meter = Arc::new(Moving(Mutex::new(Reading::default())));
        let served = serve_with(&feed, "127.0.0.1:0", Info::default(), meter.clone());
        let cookie = served.login().await;
        let mut stream = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        let (_, reset) = events.last().unwrap();
        assert!(reset["status"].get("agents").is_none(), "{reset}");

        lock(&meter.0).agents = vec![super::super::AgentStatus {
            id: "r1".into(),
            context_tokens: Some(88_000),
            context_window: Some(1_000_000),
            handover_at: Some(190_000),
            compacts: false,
        }];
        let events = read_until(&mut stream, &mut buffer, |name, data| {
            name == "status" && data.get("agents").is_some()
        })
        .await;
        let (_, status) = events.last().unwrap();
        assert_eq!(
            status["agents"],
            serde_json::json!([{
                "id": "r1",
                "context_tokens": 88_000,
                "context_window": 1_000_000,
                "handover_at": 190_000,
                "compacts": false,
            }])
        );
    }

    /// The status carries the main agent's prompt cache once its lifetime
    /// and last use are known (the lifetime in milliseconds), which the
    /// page counts down in its header. Until then it carries none.
    #[tokio::test]
    async fn the_status_carries_the_prompt_cache_once_it_is_known() {
        let feed = Feed::new("main");
        let meter = Arc::new(Moving(Mutex::new(Reading::default())));
        let served = serve_with(&feed, "127.0.0.1:0", Info::default(), meter.clone());
        let cookie = served.login().await;
        let mut stream = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        assert!(events.last().unwrap().1["status"].get("cache").is_none());

        lock(&meter.0).cache = super::super::CacheStatus {
            lifetime: Some(Duration::from_secs(300)),
            tokens: Some(180_000),
            miss_cost: Some(0.9),
            ..Default::default()
        };
        // (The last use isn't known: no timer to count.)
        lock(&meter.0).context_tokens = Some(1);
        let events = read_until(&mut stream, &mut buffer, |name, data| {
            name == "status" && data["context_tokens"] == 1
        })
        .await;
        assert!(events.last().unwrap().1.get("cache").is_none());

        lock(&meter.0).cache.refreshed_at = Some(1_800_000_000_000);
        let events = read_until(&mut stream, &mut buffer, |name, data| {
            name == "status" && data.get("cache").is_some()
        })
        .await;
        assert_eq!(
            events.last().unwrap().1["cache"],
            serde_json::json!({
                "lifetime": 300_000,
                "refreshed_at": 1_800_000_000_000_i64,
                "tokens": 180_000,
                "miss_cost": 0.9,
            })
        );
    }

    /// The page's Model row names the model the session runs now, not the
    /// one it ran as the server started: `/model` before the conversation
    /// starts changes it then. A page open gets it with the next status,
    /// and a page that connects later gets it at once.
    #[tokio::test]
    async fn the_status_names_the_model_the_session_runs_now() {
        let feed = Feed::new("main");
        let models = |model: &str| super::super::Models {
            banner_model: model.into(),
            ..Default::default()
        };
        let meter = Arc::new(Moving(Mutex::new(Reading {
            models: models("gpt-6-luna (high effort)"),
            ..Reading::default()
        })));
        let info = Info {
            model: "gpt-6-luna (high effort)".into(),
            ..Info::default()
        };
        let served = serve_with(&feed, "127.0.0.1:0", info, meter.clone());
        let cookie = served.login().await;
        let mut stream = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        let (_, reset) = events.last().unwrap();
        assert_eq!(reset["status"]["info"]["model"], "gpt-6-luna (high effort)");

        lock(&meter.0).models = models("gpt-6.1-sol (high effort)");
        read_until(&mut stream, &mut buffer, |name, data| {
            name == "status" && data["info"]["model"] == "gpt-6.1-sol (high effort)"
        })
        .await;
        let mut later = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut later, &mut buffer, |name, _| name == "reset").await;
        let (_, reset) = events.last().unwrap();
        assert_eq!(
            reset["status"]["info"]["model"],
            "gpt-6.1-sol (high effort)"
        );
        assert_eq!(
            reset["status"]["models"]["banner_model"],
            "gpt-6.1-sol (high effort)"
        );

        // Before the session tells its models, the server's own copy.
        lock(&meter.0).models = models("");
        let fresh = serve_with(
            &feed,
            "127.0.0.1:0",
            Info {
                model: "claude-opus-5-5".into(),
                ..Info::default()
            },
            meter.clone(),
        );
        let cookie = fresh.login().await;
        let mut stream = fresh.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        let events = read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        let (_, reset) = events.last().unwrap();
        assert_eq!(reset["status"]["info"]["model"], "claude-opus-5-5");
    }

    /// Messages, answers and stops come out as actions; an answer to a
    /// question that isn't asked is refused.
    #[tokio::test]
    async fn actions_come_out_of_the_channel() {
        let feed = Feed::new("main");
        let mut served = serve(&feed);
        let cookie = served.login().await;
        let response = served
            .post(
                "/api/message",
                Some(&cookie),
                r#"{"text": "Use tabs.\r\nThanks."}"#,
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            served.actions.recv().await.unwrap(),
            Action::Message("Use tabs.\nThanks.".into())
        );
        let response = served
            .post("/api/message", Some(&cookie), r#"{"text": "  "}"#)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // From a worker's view: text goes to the worker, a command doesn't.
        for (text, sent) in [
            ("/usr/bin is missing", "/message r1 /usr/bin is missing"),
            ("Use tabs.", "/message r1 Use tabs."),
            ("/outside on", "/outside on"),
        ] {
            let body = serde_json::json!({ "text": text, "to": "r1" }).to_string();
            let response = served.post("/api/message", Some(&cookie), &body).await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(
                served.actions.recv().await.unwrap(),
                Action::Message(sent.into())
            );
        }

        let response = served
            .post(
                "/api/answer",
                Some(&cookie),
                r#"{"question": 1, "text": "Yes"}"#,
            )
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let id = feed.question("Tabs or spaces?");
        let response = served
            .post(
                "/api/answer",
                Some(&cookie),
                &format!(r#"{{"question": {id}, "text": "Tabs"}}"#),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            served.actions.recv().await.unwrap(),
            Action::Answer {
                question: id,
                text: "Tabs".into()
            }
        );

        let response = served.post("/api/stop", Some(&cookie), "").await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(served.actions.recv().await.unwrap(), Action::Stop);

        // Move to background, for one call or the main agent's, and a job's
        // Stop.
        for (body, item) in [(r#"{"item": 12}"#, Some(12)), ("", None), ("{}", None)] {
            let response = served.post("/api/background", Some(&cookie), body).await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(
                served.actions.recv().await.unwrap(),
                Action::Background { item }
            );
        }
        let job = uuid::Uuid::new_v4();
        let response = served
            .post(
                "/api/jobs/stop",
                Some(&cookie),
                &format!(r#"{{"job": "{job}"}}"#),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            served.actions.recv().await.unwrap(),
            Action::StopJob { job }
        );
        for (path, body) in [
            ("/api/jobs/stop", r#"{"job": "nine"}"#),
            ("/api/background", r#"{"item": "first"}"#),
        ] {
            let response = served.post(path, Some(&cookie), body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        }
        // Without the session's cookie, nothing moves or stops.
        let response = served.post("/api/background", None, "{}").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A stopped server ends its streams, and its cookies don't open the
    /// next one, even on the same port with the same key.
    #[tokio::test]
    async fn stopping_ends_the_streams_and_the_sessions() {
        let feed = Feed::new("main");
        let served = serve(&feed);
        let addr = served.server.addr();
        let cookie = served.login().await;
        let mut stream = served.get("/api/events", Some(&cookie)).await;
        let mut buffer = String::new();
        read_until(&mut stream, &mut buffer, |name, _| name == "reset").await;
        let Served { server, http, .. } = served;
        // What is said as it stops still reaches the page.
        feed.notice(super::super::Level::Warn, "Turned off.");
        tokio::time::timeout(Duration::from_secs(5), server.stop())
            .await
            .expect("the server stops in time");
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(chunk)) = stream.chunk().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await;
        assert!(end.is_ok(), "the stream ended");
        assert!(buffer.contains("Turned off."), "{buffer}");
        drop(http);

        let again = serve_on(&feed, &addr.to_string());
        assert_eq!(again.server.addr(), addr);
        let response = again.get("/api/session", Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        again.login().await;
    }
}
