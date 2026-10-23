//! The remote is a web page Arko serves (`--remote`, `/remote`) for
//! following a session and taking part in it from a browser, such as a
//! phone's.
//!
//! The page shows the main agent's work as the terminal does (its text,
//! calls, results in brief, the workers' activity, handovers and errors)
//! and the session's status, and it sends messages (with the meaning a
//! line typed at the prompt has), answers to the agent's questions and
//! stops. The interface is small. A [`Feed`] takes the workflows' events
//! (and what Arko says of the turns), and the [`Action`]s the page sends
//! come out of a channel, which Arko reads as it reads the terminal.
//!
//! Access needs the session's key, which Arko shows only in its console.
//! The key goes in the link's fragment, which browsers don't send, and the
//! page trades it for a session cookie. The page is served without TLS.

mod feed;
mod history;
mod links;
#[cfg(not(feature = "remote"))]
mod no_server;
#[cfg(feature = "remote")]
mod qr;
mod render;
#[cfg(feature = "remote")]
mod server;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use tokio::sync::mpsc;

// The page's server and the terminal's full-screen view are both clients
// of the feed.
pub(crate) use feed::family_order_of;
pub use feed::{
    Activity, Batches, CallStatus, Delivery, Feed, Item, JobKind, JobStart, JobState, JobSummary,
    Kind, Level, MAIN_AGENT, Question, REFRESH_MESSAGE, Snapshot, Source, Update, WorkerChange,
    WorkerInfo, WorkerState, WorkerSummary, family_order, family_order_with_levels,
};
pub(crate) use history::all_items;
pub use history::items as history;
pub(crate) use history::own_words;
#[cfg(any(test, feature = "remote"))]
pub use links::Kind as LinkKind;
pub use links::{HOST_VARIABLE, Interface, Link};
#[cfg(not(feature = "remote"))]
pub use no_server::Server;
pub(crate) use render::duration_words;
#[cfg(feature = "remote")]
pub use server::Server;

/// What the page's header says of the session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Info {
    /// The models, as the banner says them.
    pub model: String,
    pub workspace: String,
    pub session: String,
    /// The app in this process the user also types in, as the page names
    /// it ("typed in …").
    pub app: String,
}

/// The port served on when none is given, or another free one when it is
/// taken.
pub const DEFAULT_PORT: u16 = 8765;

/// The file, in the session's folder, that keeps its key.
pub const KEY_FILE: &str = "remote.key";

/// What the page asks of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "remote"), allow(dead_code))]
#[non_exhaustive]
pub enum Action {
    /// A message, as if typed at the prompt. It's a note while the agent
    /// works, a new turn when it is idle, the answer to its question, or a
    /// command.
    Message(String),
    /// The answer to the question `question` (the ID the feed gave it).
    Answer { question: u64, text: String },
    /// Stops the turn, as Ctrl-C does.
    Stop,
    /// Moves the running call `item` (a call item [`Reading::movable`]
    /// lists) to the background, or with `None` the main agent's calls
    /// that can move, as Ctrl+B does.
    Background { item: Option<u64> },
    /// Stops the background job `job` (as [`JobSummary::id`] names it).
    StopJob { job: uuid::Uuid },
}

/// Reads what the page's status shows of the session as it asks.
pub trait Meter: Send + Sync {
    fn read(&self) -> Reading;
}

/// The session's figures, for the page's status.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reading {
    /// What the session has cost, in dollars, as billed.
    pub cost: f64,
    /// What the whole session's usage (the main agent's and the workers')
    /// would have cost at API list prices, in dollars. That is `cost` plus
    /// the list-price estimate of the calls a plan covered (Claude Code, the
    /// ChatGPT login), which bill nothing per call. It's present only when
    /// some of it ran on a plan. For a session entirely on keys, `cost`
    /// says it all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_price: Option<f64>,
    /// The main agent's part of the costs and the workers'.
    #[serde(skip)]
    pub costs: CostSplit,
    /// The input of the main agent's latest model request, in tokens.
    pub context_tokens: Option<usize>,
    /// The plans' usage windows (ChatGPT, Claude), as last reported.
    pub plan_usage: Option<String>,
    /// The workers running.
    pub workers_running: Vec<String>,
    /// Each worker, a line or two (as `/workers` says them).
    pub workers: Option<String>,
    /// The settings a client may change (with commands).
    pub settings: LiveSettings,
    /// The agents running a call now that can move to the background (a
    /// shell command, an MCP tool's call), which Ctrl+B moves, as
    /// [`MAIN_AGENT`] or a worker's ID.
    #[serde(skip)]
    pub commands: Vec<String>,
    /// The feed items of the calls running now that can move to the
    /// background, for a client that offers to move one
    /// (`Request::MoveCall`, the page's Move to background).
    pub movable: Vec<u64>,
    /// The jobs running in the background, which stop when Arko exits. The
    /// full-screen view's quit popup shows them. (The page can't quit Arko.)
    #[serde(skip)]
    pub jobs: Vec<arko_agents::terminal::BackgroundJob>,
    /// The agents' models, as `/model` changes them.
    pub models: Models,
    /// The session's conversations with the main agent, newest first, for
    /// the full-screen view's `/resume` picker. (The page lists them with
    /// `/resume`.)
    #[serde(skip)]
    pub conversations: Vec<ConversationEntry>,
    /// The web page, while it is served.
    #[serde(skip)]
    pub page: Option<Page>,
    /// The session's name. That is the one the user gave it, else its
    /// current conversation's, else its first message's start.
    #[serde(skip)]
    pub title: String,
    /// What the main agent's model accepts, in tokens (the ChatGPT login's
    /// limit where the session reaches OpenAI models with it).
    #[serde(skip)]
    pub context_window: Option<usize>,
    /// Each plan's usage windows, as last reported.
    #[serde(skip)]
    pub plans: Vec<PlanStatus>,
    /// Each worker's context and handover size, which the page shows in a
    /// worker's own view.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentStatus>,
    /// The main agent's prompt cache, as its latest request left it. The
    /// page counts it down, and the status leaves it out until the cache's
    /// lifetime and last use are known.
    #[serde(skip_serializing_if = "CacheStatus::is_unknown")]
    pub cache: CacheStatus,
    /// The main agent's latest model request.
    #[serde(skip)]
    pub last_request: Option<LastRequest>,
}

/// The main agent's prompt cache, as its latest request left it, with when
/// it lapses and what a miss and keeping it warm cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CacheStatus {
    /// How long the cache keeps the conversation after a request. `None`
    /// where the provider doesn't say (Gemini, OpenRouter models other than
    /// Claude, Claude Code until it writes the cache) or nothing is cached.
    /// The page's status has it in milliseconds.
    #[serde(
        serialize_with = "milliseconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub lifetime: Option<std::time::Duration>,
    /// When the latest request went out, or the refresh that kept the cache
    /// warm (Unix milliseconds). The cache lasts `lifetime` from then. A
    /// session resumed after Arko closed has the time its latest finished
    /// request left, so the cache may have lapsed meanwhile. `None` for a
    /// new conversation, and for one whose cache the session can't vouch for
    /// (another model, a handover or a compaction since).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<i64>,
    /// The conversation's input at its latest request, in tokens, which a
    /// miss writes to the cache again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<usize>,
    /// What the latest request read from the cache and wrote to it, in
    /// tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<usize>,
    /// What a miss costs, with `tokens` written again at the model's price.
    /// `None` on a plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_cost: Option<f64>,
    /// What keeping it warm costs, with `tokens` read from the cache at the
    /// model's price. `None` on a plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_cost: Option<f64>,
    /// The plan that pays instead ("your Claude plan").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_plan: Option<&'static str>,
}

impl CacheStatus {
    /// Whether the cache can't be timed yet: its lifetime or its last use
    /// is not known.
    fn is_unknown(&self) -> bool {
        self.lifetime.is_none() || self.refreshed_at.is_none()
    }
}

/// A duration in milliseconds, which is how the page counts.
fn milliseconds<S: serde::Serializer>(
    duration: &Option<std::time::Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match duration {
        Some(duration) => serializer.serialize_u64(duration.as_millis() as u64),
        None => serializer.serialize_none(),
    }
}

/// The main agent's latest model request.
#[derive(Debug, Clone, PartialEq)]
pub struct LastRequest {
    /// When it went out (Unix milliseconds).
    pub at: i64,
    /// The model it went to.
    pub model: String,
    /// What it held, by section, in bytes (for Claude Code, without what
    /// Claude Code adds of its own).
    pub sizes: Option<arko_ai::RequestSizes>,
    /// The bytes per token that turn `sizes` into estimated tokens.
    pub bytes_per_token: f64,
}

/// The costs, the main agent's and the workers'.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostSplit {
    pub main: CostPart,
    pub workers: CostPart,
}

/// Some agents' model calls, with what they billed and would cost at list
/// prices (more, where a plan covered them), their tokens and how many.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostPart {
    pub billed: f64,
    pub list_price: f64,
    /// Input tokens, cached input included.
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub calls: usize,
}

/// A subscription plan's usage (the ChatGPT plan, the Claude plan).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStatus {
    /// The plan, as `/cost` names it ("ChatGPT plan").
    pub plan: String,
    pub windows: Vec<PlanWindow>,
    /// The windows in a line ("ChatGPT plan 5 h 12%, 7 d 40%").
    pub text: String,
}

/// One of a plan's usage windows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanWindow {
    /// "5 h", "7 d", or its length in minutes.
    pub label: String,
    pub used_percent: f64,
    /// When it starts over (Unix seconds), if the plan says.
    pub resets_at: Option<i64>,
}

/// A worker's context, as its latest request reported it, and the size it
/// hands over (or Claude Code compacts) at.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentStatus {
    /// Its ID, such as r1 or r2.
    pub id: String,
    pub context_tokens: Option<usize>,
    pub context_window: Option<usize>,
    pub handover_at: Option<usize>,
    /// Claude Code compacts its conversation at the size, instead of a
    /// handover.
    pub compacts: bool,
}

/// The web page while it is served, with its address, its links as they
/// were when it started (best first), and what to tell the user about them.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub addr: SocketAddr,
    pub links: Vec<Link>,
    pub note: Option<String>,
}

/// One of the session's conversations with the main agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationEntry {
    /// Its number, as `/resume` takes it, 1 for the session's first.
    pub number: usize,
    /// The first line of the user's first message.
    pub title: String,
    /// When its last turn ended.
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    /// The next message goes to it.
    pub current: bool,
}

/// The agents' models, as `/model` changes them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Models {
    /// The main agent's model as `/model` takes it (`gpt-6.1-sol`), and its
    /// effort (`high`).
    pub main: String,
    pub main_effort: String,
    /// The model workers start on, and their effort.
    pub workers: String,
    pub workers_effort: String,
    /// The workers follow the main agent's model.
    pub workers_follow: bool,
    /// The conversation has started, so the main agent keeps its model, and
    /// `/model worker` changes the workers started from now on.
    pub started: bool,
    /// The banner's rows for the main agent's model and the workers.
    pub banner_model: String,
    pub banner_workers: String,
    /// The providers the session's client reaches (`anthropic`, `openai`,
    /// `gemini`, `openrouter`), whose models `/model` offers.
    pub reachable: Vec<String>,
    /// How the session reaches OpenAI models (`ChatGPT login`, `OpenAI API
    /// key`), when it can.
    pub openai_auth: Option<String>,
    /// The session has workers (not `--workers 0`).
    pub has_workers: bool,
    /// The main agent's model runs in its fast mode, at its price.
    #[serde(skip)]
    pub main_fast: bool,
}

/// The session's settings that can change while it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveSettings {
    /// The agents may work outside the workspace.
    pub outside: bool,
    /// The main agent hands over at this many input tokens, or 0 when it is
    /// not known.
    pub handover_at: usize,
    /// New workers hand over at this many input tokens, on their model (the
    /// size `/handover-at` set for the workers, held under what the model
    /// leaves room for, else the model's own). A worker on another model
    /// goes by its own. 0 when the session has no workers.
    pub workers_handover_at: usize,
    /// The main agent ends its turns with a reply in plain text (else with
    /// `complete_task`). It changes only before the conversation starts
    /// (`Models::started`).
    pub text_replies: bool,
    /// The agents on `cc:` models compact at the handover size (else they
    /// hand over). It changes only before the conversation starts.
    pub cc_compact: bool,
    /// Which agents get the extra rules for small models. It changes only
    /// before the conversation starts.
    pub extra_rules: crate::agents::ExtraRules,
    /// Which of the project's instruction files the agents get, from the
    /// next conversation and worker on.
    pub project_instructions: crate::project::Files,
    /// Workers started from now on get the project's instructions too.
    pub worker_project_instructions: bool,
    /// After how many seconds a shell command still running moves to the
    /// background, or 0 for never.
    pub background_after: u64,
    /// How long after the user's last message a monitor's warning before it
    /// ends still wakes the agent (`/monitor-wakes`).
    pub monitor_wakes: crate::monitors::Wakes,
    /// Who chooses the workers' models (`/worker-models`). It applies to the
    /// next worker started.
    pub worker_models: crate::agents::ModelChoice,
    /// How many levels of workers the session allows (`/worker-depth`).
    pub worker_depth: u32,
    /// The agent that started a worker gets a copy of the user's messages to
    /// it (`/message-copies`). It applies from the next message.
    pub message_copies: bool,
}

/// A random token of `bytes` bytes, in URL-safe base64.
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system gives random bytes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

/// The key's random bytes: 192 bits.
const KEY_BYTES: usize = 24;

/// Whether `key` looks like a key Arko made.
fn is_key(key: &str) -> bool {
    key.len() >= 22
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The session's key, from `path`, else a new one saved there (readable by
/// the user only).
pub fn session_key(path: &Path) -> Result<String, String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let key = text.trim();
        if is_key(key) {
            return Ok(key.to_string());
        }
    }
    new_key(path)
}

/// A new key for the session, saved at `path` in place of the old one.
pub fn new_key(path: &Path) -> Result<String, String> {
    let key = random_token(KEY_BYTES);
    let _ = std::fs::remove_file(path);
    write_private(path, &key)
        .map_err(|error| format!("could not save the key in {} ({error})", path.display()))?;
    Ok(key)
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Where to serve, an address with a port or without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub ip: IpAddr,
    /// `None` for [`DEFAULT_PORT`], else any free one.
    pub port: Option<u16>,
}

impl Default for Bind {
    fn default() -> Self {
        Self {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: None,
        }
    }
}

impl Bind {
    /// Every address of this computer (0.0.0.0), for the devices on its
    /// networks, on [`DEFAULT_PORT`], else any free one.
    pub fn network() -> Self {
        Self {
            ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: None,
        }
    }
}

/// Reads `--remote`'s value, which is nothing (127.0.0.1), a port, an
/// address or a host name, or an address (or name) and a port.
pub fn parse_bind(text: &str) -> Result<Bind, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Bind::default());
    }
    if let Ok(port) = text.parse::<u16>() {
        return Ok(Bind {
            port: Some(port),
            ..Bind::default()
        });
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(Bind {
            ip: addr.ip(),
            port: Some(addr.port()),
        });
    }
    let bare = text.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(Bind { ip, port: None });
    }
    let invalid = || {
        format!(
            "`{text}` is not an address to serve on. Give a port (8765), an address (0.0.0.0) or both (0.0.0.0:8765)"
        )
    };
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().map_err(|_| invalid())?)),
        None => (text, None),
    };
    let ip = (host, port.unwrap_or(DEFAULT_PORT))
        .to_socket_addrs()
        .map_err(|_| invalid())?
        .map(|addr| addr.ip())
        .min_by_key(|ip| !ip.is_ipv4())
        .ok_or_else(invalid)?;
    Ok(Bind { ip, port })
}

/// Starts serving where `bind` says, on [`DEFAULT_PORT`] when it names no
/// port, or on another free one when that one is taken. `earlier`, the
/// port of the page served before, is tried first when `bind` names no
/// port, so that the pages open on it reconnect.
pub fn start(
    bind: Bind,
    earlier: Option<u16>,
    key: String,
    info: Info,
    feed: Feed,
    actions: mpsc::UnboundedSender<Action>,
    meter: Arc<dyn Meter>,
) -> Result<Server, String> {
    start_with((bind, false), earlier, key, info, feed, actions, meter)
}

/// Binds the preferred port, falling back to any free port only when it
/// is in use. The returned server owns the listener throughout.
pub(crate) fn start_preferred(
    bind: Bind,
    earlier: Option<u16>,
    key: String,
    info: Info,
    feed: Feed,
    actions: mpsc::UnboundedSender<Action>,
    meter: Arc<dyn Meter>,
) -> Result<Server, String> {
    start_with((bind, true), earlier, key, info, feed, actions, meter)
}

fn start_with(
    (bind, fallback): (Bind, bool),
    earlier: Option<u16>,
    key: String,
    info: Info,
    feed: Feed,
    actions: mpsc::UnboundedSender<Action>,
    meter: Arc<dyn Meter>,
) -> Result<Server, String> {
    let start = |port: u16| {
        Server::start(
            SocketAddr::new(bind.ip, port),
            key.clone(),
            info.clone(),
            feed.clone(),
            actions.clone(),
            meter.clone(),
        )
    };
    let any_port = || {
        start(DEFAULT_PORT).or_else(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                start(0)
            } else {
                Err(error)
            }
        })
    };
    let result = match (bind.port, earlier) {
        (None | Some(0), Some(port)) => start(port).or_else(|_| match bind.port {
            Some(port) => start(port),
            None => any_port(),
        }),
        (Some(port), _) => start(port).or_else(|error| {
            if fallback && error.kind() == std::io::ErrorKind::AddrInUse {
                start(0)
            } else {
                Err(error)
            }
        }),
        (None, None) => any_port(),
    };
    result.map_err(|error| {
        let addr = SocketAddr::new(bind.ip, bind.port.unwrap_or(DEFAULT_PORT));
        // A port given that another page or program has: what to change.
        if error.kind() == std::io::ErrorKind::AddrInUse && bind.port.is_some_and(|port| port != 0)
        {
            format!(
                "could not serve on {addr} because the port is in use (by another session's web page, or another program). Choose another port, or none for any free one"
            )
        } else {
            format!("could not serve on {addr} ({error})")
        }
    })
}

/// The links to a server on `addr`, with `key` in their fragments, best
/// first. When it listens on every interface, there is a link for each
/// interface's address, and the one `ARKO_REMOTE_HOST` names comes first.
/// The second value is what to tell the user when that variable names none
/// of them.
pub fn links(addr: SocketAddr, key: &str) -> (Vec<Link>, Option<String>) {
    ranked_links(
        addr,
        key,
        links::interfaces(),
        crate::credentials::var(HOST_VARIABLE),
    )
}

/// [`links`] on `interfaces`, with `host` as `ARKO_REMOTE_HOST`. It counts
/// only where the server listens on every interface.
pub fn ranked_links(
    addr: SocketAddr,
    key: &str,
    interfaces: Vec<Interface>,
    host: Option<String>,
) -> (Vec<Link>, Option<String>) {
    let mut ranked = links::links(addr, key, interfaces);
    let note = host
        .filter(|host| addr.ip().is_unspecified() && !host.trim().is_empty())
        .and_then(|host| links::prefer(&mut ranked, host.trim()).err());
    (ranked, note)
}

/// The interfaces (else the addresses) of the links other devices can
/// open, as the user is told them.
pub fn link_names(links: &[Link]) -> String {
    links::names(links)
}

/// `text` without its terminal styles (the escape sequences of colors and
/// weights).
pub fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // `ESC [ ... letter`
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

/// `text` as a QR code, in lines of half blocks (two rows of modules a
/// line), with the light modules drawn, for a terminal with a dark
/// background.
pub fn qr_lines(text: &str) -> Option<Vec<String>> {
    #[cfg(feature = "remote")]
    return qr::lines(text);
    #[cfg(not(feature = "remote"))]
    {
        let _ = text;
        None
    }
}

/// `text` as a QR code in SVG, dark on light.
#[cfg(feature = "remote")]
pub fn qr_svg(text: &str) -> Option<String> {
    qr::svg(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_taken_out() {
        assert_eq!(
            plain("\x1b[2m$1.20\x1b[0m · \x1b[1mw1\x1b[0m"),
            "$1.20 · w1"
        );
    }

    #[test]
    fn addresses_are_read_as_ports_hosts_or_both() {
        let local = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(parse_bind("").unwrap(), Bind::default());
        assert_eq!(
            parse_bind("9000").unwrap(),
            Bind {
                ip: local,
                port: Some(9000)
            }
        );
        assert_eq!(
            parse_bind("0.0.0.0").unwrap(),
            Bind {
                ip: any,
                port: None
            }
        );
        assert_eq!(
            parse_bind("0.0.0.0:8765").unwrap(),
            Bind {
                ip: any,
                port: Some(8765)
            }
        );
        assert_eq!(
            parse_bind("[::]").unwrap().ip,
            "::".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            parse_bind("[::1]:80").unwrap(),
            Bind {
                ip: "::1".parse().unwrap(),
                port: Some(80)
            }
        );
        assert!(parse_bind("localhost:7000").unwrap().ip.is_loopback());
        assert!(parse_bind("0.0.0.0:http").is_err());
    }

    #[test]
    fn links_carry_the_key_in_their_fragment() {
        let addr: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        assert_eq!(
            links(addr, "abc").0,
            [Link {
                url: "http://127.0.0.1:8765/#key=abc".into(),
                interface: None,
                kind: LinkKind::Loopback,
                loopback: true
            }]
        );
        let all = links("0.0.0.0:9000".parse().unwrap(), "abc").0;
        // ARKO_REMOTE_HOST counts only where the server listens on every
        // interface, and one that names nothing changes nothing.
        let interfaces = vec![
            Interface {
                name: "wg0-mullvad".into(),
                ip: "10.64.1.2".parse().unwrap(),
                physical: None,
            },
            Interface {
                name: "ens33".into(),
                ip: "192.168.1.12".parse().unwrap(),
                physical: None,
            },
        ];
        let any: SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let (ranked, note) = ranked_links(any, "k", interfaces.clone(), Some("wlan0".into()));
        assert!(
            note.unwrap()
                .starts_with("ARKO_REMOTE_HOST is `wlan0`, which none")
        );
        assert_eq!(ranked[0].interface.as_deref(), Some("ens33"));
        let (ranked, note) =
            ranked_links(any, "k", interfaces.clone(), Some(" wg0-mullvad ".into()));
        assert!(note.is_none());
        assert_eq!(ranked[0].interface.as_deref(), Some("wg0-mullvad"));
        let (_, note) = ranked_links(addr, "k", interfaces, Some("wlan0".into()));
        assert!(note.is_none());
        let last = all.last().unwrap();
        assert_eq!(last.url, "http://127.0.0.1:9000/#key=abc");
        assert!(all.iter().all(|link| link.url.ends_with(":9000/#key=abc")));
    }

    /// A session keeps its key, which only its owner can read, until a
    /// new one replaces it.
    #[test]
    fn the_key_is_kept_in_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(KEY_FILE);
        let key = session_key(&path).unwrap();
        assert!(is_key(&key) && key.len() >= 32, "{key}");
        assert_eq!(session_key(&path).unwrap(), key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let fresh = new_key(&path).unwrap();
        assert_ne!(fresh, key);
        assert_eq!(session_key(&path).unwrap(), fresh);
        std::fs::write(&path, "short").unwrap();
        assert_ne!(session_key(&path).unwrap(), "short");
    }
}
