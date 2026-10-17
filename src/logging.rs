//! One log for a process with several sessions (an app's): each line
//! says the session it is for, as the thread that wrote it works for one
//! (`arko::spawn` runs each session on threads of its own). The command line
//! logs to the session's `arko.log` instead.

use std::io::Write;
use std::path::Path;

/// The process's log.
pub struct ProcessLog;

impl ProcessLog {
    /// Logs to the file at `path` (appended) from now on, at the level
    /// `RUST_LOG` says (default: info). A line written for a session is
    /// tagged with its ID, `[20261016-140213-a1b2]`, or `[opening]` while it
    /// opens. The process's own lines have no tag. Once per process.
    pub fn install(path: &Path) -> Result<(), String> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| format!("could not open {} ({error})", path.display()))?;
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(file)))
            .format(|buf, record| {
                writeln!(
                    buf,
                    "[{} {:<5} {}] {}{}",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    record.level(),
                    record.target(),
                    tag(),
                    record.args()
                )
            })
            .try_init()
            .map_err(|error| error.to_string())
    }
}

/// The tag of a line this thread writes: its session's ID.
pub(crate) fn tag() -> String {
    match crate::context::session_id() {
        None => String::new(),
        Some(id) if id.is_empty() => "[opening] ".to_string(),
        Some(id) => format!("[{id}] "),
    }
}
