//! Semantic code and docs search for Arko, built on upstream `arko_index`
//! (local tree-sitter chunking, `.gitignore` respected) with Voyage embeddings
//! (`voyage-code-4` for code, `voyage-4` for docs).
//!
//! This module is self-contained: Arko passes in its data folder and the Voyage
//! key; nothing here reads the process environment for keys.
//!
//! Entry points:
//! - [`Indexes`]: one per Arko process (`Indexes::open`), shared by the agent
//!   tools ([`Indexes::tools`], named in [`TOOL_NAMES`]); it watches monitored
//!   indexes related to the workspace while Arko runs, counts embedding spend
//!   ([`Indexes::spent`]) and stops its threads in [`Indexes::shutdown`].
//! - [`IndexCommand`] + [`run_command`]: the `arko index ...` subcommands.
//!
//! Wiring (Arko's main.rs): in an interactive session, once the workspace and
//! data folder are known, `let indexes = index::Indexes::open(&data_root,
//! &workspace, voyage_key, !one_shot)?;`, add `indexes.tools()` to the agent's
//! tools, show `indexes.summary()` in the banner and `indexes.spent()` in
//! `/cost`, and `indexes.shutdown().await` before returning. `arko index ...`
//! runs `index::run_command(cmd, &data_root, voyage_key).await`.
//!
//! Storage: `<data root>/indexes/<slug>-<kind>-<hash>/` holds `index.json` (the
//! manifest), `state/` (the arko_index state) and the watcher state (`watch2/`
//! for inotify on Linux, `watch/` for the polling watcher). Indexes are shared by
//! all Arko processes; the one that holds an index's watch lock keeps it current.

mod census;
#[cfg(feature = "tui")]
mod cli;
mod embed;
mod engine;
mod monitor;
mod store;
mod tools;

#[cfg(test)]
mod tests;

#[cfg(feature = "tui")]
pub use cli::{IndexCommand, run_command};
pub(crate) use engine::WatchState;
pub use engine::{Indexes, Known};
pub(crate) use store::Kind;
pub use tools::TOOL_NAMES;
