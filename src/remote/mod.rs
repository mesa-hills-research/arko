//! The remote: a web page fin serves (`--remote`, `/remote`) for following
//! a session and taking part in it from a browser, such as a phone's.
//!
//! The page shows the main agent's work as the terminal does (its text,
//! calls, results in brief, the workers' activity, handovers and errors)
//! and the session's status, and it sends messages (with the meaning a
//! line typed at the prompt has), answers to the agent's questions and
//! stops. The interface is small: a [`Feed`] takes the workflows' events
//! (and what fin says of the turns), and the [`Action`]s the page sends
//! come out of a channel, which fin reads as it reads the terminal.
//!
//! Access needs the session's key, which fin shows only in its console: it
//! goes in the link's fragment, which browsers don't send, and the page
//! trades it for a session cookie. There is no TLS.

mod feed;
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
    /// What the session has cost, in dollars.
    pub cost: f64,
    /// The input of the main agent's latest model request, in tokens.
    pub context_tokens: Option<usize>,
    /// The ChatGPT plan's usage windows.
    pub plan_usage: Option<String>,
    /// The workers running.
    pub workers_running: Vec<String>,
    /// Each worker, a line or two (as `/workers` says them).
    pub workers: Option<String>,
    /// The settings a client may change (with commands).
    pub settings: LiveSettings,
}

/// The session's settings that can change while it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveSettings {
    /// The agents may work outside the workspace.
    pub outside: bool,
    /// The main agent hands over at this many input tokens; 0 when it is
    /// not known.
    pub handover_at: usize,
}

/// A random token of `bytes` bytes, in URL-safe base64.
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system gives random bytes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

/// The key's random bytes: 192 bits.
const KEY_BYTES: usize = 24;

/// Whether `key` looks like a key fin made.
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

/// A link to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    /// The network interface, when the server listens on all of them.
    pub interface: Option<String>,
    pub loopback: bool,
}

/// The links to a server on `addr`, with `key` in their fragments: on
/// every interface's address when it listens on all of them (the other
/// machines' first, this one's last).
pub fn links(addr: SocketAddr, key: &str) -> Vec<Link> {
    let link = |ip: IpAddr, interface: Option<String>| {
        let host = match ip {
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        };
        Link {
            url: format!("http://{host}:{}/#key={key}", addr.port()),
            interface,
            loopback: ip.is_loopback(),
        }
    };
    if !addr.ip().is_unspecified() {
        return vec![link(addr.ip(), None)];
    }
    let v6 = addr.ip().is_ipv6();
    let mut links: Vec<Link> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter(|interface| match interface.ip() {
            IpAddr::V4(_) => !v6,
            // Link-local addresses need a zone in the URL.
            IpAddr::V6(ip) => v6 && (ip.segments()[0] & 0xffc0) != 0xfe80,
        })
        .map(|interface| link(interface.ip(), Some(interface.name)))
        .collect();
    links.dedup_by(|a, b| a.url == b.url);
    links.push(link(
        if v6 {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        },
        None,
    ));
    links
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
            links(addr, "abc"),
            [Link {
                url: "http://127.0.0.1:8765/#key=abc".into(),
                interface: None,
                loopback: true
            }]
        );
        let all = links("0.0.0.0:9000".parse().unwrap(), "abc");
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
