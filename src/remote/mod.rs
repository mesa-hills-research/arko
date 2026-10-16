//! The remote: a web page Arko serves (`--remote`, `/remote`) for following
//! a session and taking part in it from a browser, such as a phone's.
//!
//! The page shows the main agent's work as the terminal does (its text,
//! calls, results in brief, the workers' activity, handovers and errors)
//! and the session's status, and it sends messages (with the meaning a
//! line typed at the prompt has), answers to the agent's questions and
//! stops. The interface is small: a [`Feed`] takes the workflows' events
//! (and what Arko says of the turns), and the [`Action`]s the page sends
//! come out of a channel, which Arko reads as it reads the terminal.
//!
//! Access needs the session's key, which Arko shows only in its console: it
//! goes in the link's fragment, which browsers don't send, and the page
//! trades it for a session cookie. There is no TLS.

mod feed;
mod history;
mod links;
mod qr;
mod render;
mod server;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use tokio::sync::mpsc;

// The page's server and the terminal's full-screen view are both clients
// of the feed.
#[allow(unused_imports)]
pub use feed::{
    Activity, CallStatus, Delivery, Feed, Item, Kind, Level, MAIN_AGENT, Question, Snapshot,
    Source, Update, WorkerChange, WorkerState, WorkerSummary,
};
pub use history::items as history;
pub use links::{HOST_VARIABLE, Interface, Kind as LinkKind, Link};
pub use server::{Info, Server};

/// The port served on when none is given; another free one when it is
/// taken.
pub const DEFAULT_PORT: u16 = 8765;

/// The file, in the session's folder, that keeps its key.
pub const KEY_FILE: &str = "remote.key";

/// What the page asks of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A message, as if typed at the prompt: a note while the agent works,
    /// a new turn when it is idle, the answer to its question, a command.
    Message(String),
    /// The answer to the question `question` (the ID the feed gave it).
    Answer { question: u64, text: String },
    /// Stops the turn, as Ctrl-C does.
    Stop,
}

/// Reads what the page's status shows of the session as it asks.
pub trait Meter: Send + Sync {
    fn read(&self) -> Reading;
}

/// The session's figures, for the page's status.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reading {
    /// What the session has cost, in dollars: what was billed.
    pub cost: f64,
    /// What the whole session's usage (the main agent's and the workers')
    /// would have cost at API list prices, in dollars: `cost` plus the
    /// list-price estimate of the calls a plan covered (Claude Code, the
    /// ChatGPT login), which bill nothing per call. Present only when some
    /// of it ran on a plan; for a session entirely on keys, `cost` says it
    /// all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_price: Option<f64>,
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
    /// The agents running a shell command now, which Ctrl+B moves to the
    /// background: [`MAIN_AGENT`], or a worker's ID.
    #[serde(skip)]
    pub commands: Vec<String>,
    /// The agents' models, as `/model` changes them.
    pub models: Models,
    /// The session's conversations with the main agent, newest first, for
    /// the full-screen view's `/resume` picker. (The page lists them with
    /// `/resume`.)
    #[serde(skip)]
    pub conversations: Vec<ConversationEntry>,
}

/// One of the session's conversations with the main agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationEntry {
    /// Its number, as `/resume` takes it: 1 for the session's first.
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
    /// The conversation has started: the main agent keeps its model, and
    /// `/model worker` changes the workers started from now on.
    pub started: bool,
    /// The banner's rows for the main agent's model and the workers.
    pub banner_model: String,
    pub banner_workers: String,
    /// The providers the session's client reaches (`anthropic`, `openai`,
    /// `gemini`, `openrouter`): `/model` offers their models.
    pub reachable: Vec<String>,
    /// How the session reaches OpenAI models (`ChatGPT login`, `OpenAI API
    /// key`), when it can.
    pub openai_auth: Option<String>,
    /// The session has workers (not `--workers 0`).
    pub has_workers: bool,
}

/// The session's settings that can change while it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveSettings {
    /// The agents may work outside the workspace.
    pub outside: bool,
    /// The main agent hands over at this many input tokens; 0 when it is
    /// not known.
    pub handover_at: usize,
    /// The main agent ends its turns with a reply in plain text (else with
    /// `complete_task`). It changes only before the conversation starts
    /// (`Models::started`).
    pub text_replies: bool,
    /// Which agents get the extra rules for small models. It changes only
    /// before the conversation starts.
    pub extra_rules: crate::agents::ExtraRules,
    /// Which of the project's instruction files the agents get, from the
    /// next conversation and worker on.
    pub project_instructions: crate::project::Files,
    /// Workers started from now on get the project's instructions too.
    pub worker_project_instructions: bool,
    /// After how many seconds a shell command still running moves to the
    /// background; 0: never.
    pub background_after: u64,
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
        .map_err(|error| format!("could not save the key in {}: {error}", path.display()))?;
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

/// Where to serve: an address, with a port or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub ip: IpAddr,
    /// `None`: [`DEFAULT_PORT`], else any free one.
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

/// Reads `--remote`'s value: nothing (127.0.0.1), a port, an address or a
/// host name, or an address (or name) and a port.
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
            "`{text}` is not an address to serve on: give a port (8765), an address (0.0.0.0), or both (0.0.0.0:8765)"
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

/// Starts serving where `bind` says: on [`DEFAULT_PORT`] when it names
/// no port, or on another free one when that one is taken. `earlier`, the
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
        (Some(port), _) => start(port),
        (None, None) => any_port(),
    };
    result.map_err(|error| {
        format!(
            "could not serve on {}: {error}",
            SocketAddr::new(bind.ip, bind.port.unwrap_or(DEFAULT_PORT))
        )
    })
}

/// The links to a server on `addr`, with `key` in their fragments, best
/// first (see [`links`](self::links)): on every interface's address when
/// it listens on all of them, the one `ARKO_REMOTE_HOST` names first. With
/// what to tell the user when that names none of them.
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
/// line), light modules drawn: for a terminal with a dark background.
pub fn qr_lines(text: &str) -> Option<Vec<String>> {
    qr::lines(text)
}

/// `text` as a QR code in SVG, dark on light.
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
