//! What build of Arko this is, as `build.rs` found it.
//!
//! Each session folder keeps a line in `launches.jsonl` for every time the
//! session opens, with the build that opened it, and every checkpoint names
//! that build in its `workflow_metadata.version`, so a session's records
//! say which build wrote them, also across builds that share a version.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::session::Settings;

/// The build's ID, made from the package version and the first 12 hex digits
/// of a SHA-256 over every file it was built from, such as
/// `0.24.0+a4f8c91d2e0b`. A build from uncommitted changes or with a
/// dependency from a local path has `-dev` after the version, such as
/// `0.24.0-dev+7b2e109f4c3a`.
pub const ID: &str = env!("ARKO_BUILD_ID");

/// Everything `build.rs` recorded of the build as JSON, which is the ID,
/// version, whole hash, profile, target, features, compiler, Git commit,
/// the framework's versions and each dependency that doesn't come from
/// crates.io.
pub const DETAILS: &str = include_str!(concat!(env!("OUT_DIR"), "/build.json"));

/// The file in a session's folder with a line for each time it opened.
pub const LAUNCHES_FILE: &str = "launches.jsonl";

/// How a session came to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind {
    New,
    Resume,
    /// The first opening of a session branched from another.
    Branch,
}

#[derive(Serialize)]
struct Launch<'a> {
    /// The line's format, raised when a field changes meaning.
    schema: u32,
    launch_id: Uuid,
    timestamp: DateTime<Utc>,
    kind: LaunchKind,
    /// The app that opened it, `arko` or one built on the library.
    app: &'a str,
    pid: u32,
    build: serde_json::Value,
    settings: &'a Settings,
}

/// Adds a line for this opening of the session in `dir` to its
/// `launches.jsonl`.
pub(crate) fn record_launch(
    dir: &Path,
    timestamp: DateTime<Utc>,
    kind: LaunchKind,
    app: &str,
    settings: &Settings,
) -> std::io::Result<()> {
    let launch = Launch {
        schema: 1,
        launch_id: Uuid::new_v4(),
        timestamp,
        kind,
        app,
        pid: std::process::id(),
        build: serde_json::from_str(DETAILS)?,
        settings,
    };
    let mut line = serde_json::to_vec(&launch)?;
    line.push(b'\n');
    // One write of the whole line, so a line never interleaves with
    // another's.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LAUNCHES_FILE))?
        .write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_details_name_the_id() {
        let details: serde_json::Value = serde_json::from_str(DETAILS).unwrap();
        assert_eq!(details["id"], ID);
        assert_eq!(details["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(details["source_hash"].as_str().unwrap().len(), 64);
        let (version, hash) = ID.split_once('+').unwrap();
        assert!(version.starts_with(env!("CARGO_PKG_VERSION")));
        assert_eq!(hash.len(), 12);
        assert!(details["source_hash"].as_str().unwrap().starts_with(hash));
    }

    #[test]
    fn each_launch_adds_a_line() {
        let temp = tempfile::tempdir().unwrap();
        let model = crate::agents::ModelSettings {
            model: "gpt-6-luna".into(),
            effort: crate::agents::Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let settings = Settings::for_test(Path::new("/home/ada/proj"), &model);
        let first = Utc::now();
        record_launch(temp.path(), first, LaunchKind::New, "arko", &settings).unwrap();
        record_launch(
            temp.path(),
            Utc::now(),
            LaunchKind::Resume,
            "Example Studio",
            &settings,
        )
        .unwrap();
        record_launch(
            temp.path(),
            Utc::now(),
            LaunchKind::Branch,
            "arko",
            &settings,
        )
        .unwrap();
        let text = std::fs::read_to_string(temp.path().join(LAUNCHES_FILE)).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["schema"], 1);
        assert_eq!(lines[0]["kind"], "new");
        assert_eq!(lines[1]["kind"], "resume");
        assert_eq!(lines[1]["app"], "Example Studio");
        assert_eq!(lines[2]["kind"], "branch");
        assert_eq!(lines[0]["build"]["id"], ID);
        assert_eq!(lines[0]["settings"]["workspace"], "/home/ada/proj");
        assert_ne!(lines[0]["launch_id"], lines[1]["launch_id"]);
        assert_ne!(lines[1]["launch_id"], lines[2]["launch_id"]);
    }
}
