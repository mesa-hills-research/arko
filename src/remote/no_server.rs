//! The web page's server in a build without it (no `remote` feature):
//! starting it says this build has none.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::{Action, Feed, Info, Meter};

/// A running server, which this build never has.
pub enum Server {}

impl Server {
    pub fn start(
        _addr: SocketAddr,
        _key: String,
        _info: Info,
        _feed: Feed,
        _actions: mpsc::UnboundedSender<Action>,
        _meter: Arc<dyn Meter>,
        _sessions_file: Option<std::path::PathBuf>,
    ) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this build of Arko has no web page",
        ))
    }

    pub fn addr(&self) -> SocketAddr {
        match *self {}
    }

    pub async fn stop(self) {
        match self {}
    }
}
