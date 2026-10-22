//! The sessions in Arko's folder, as an app lists them, with their names,
//! what they are and where they work, when they were last used, their
//! conversations and who has them open, and renaming, branching and
//! deleting them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use arko_ai::process::NoConsoleWindow;
use chrono::{DateTime, Utc};

use crate::conversations::Conversations;
use crate::remote;
pub use crate::remote::ConversationEntry;
use crate::session::{self, Session};
pub use crate::session::{Holder, Mode, data_root};

/// A session, as the list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub dir: PathBuf,
    /// Its name. That's the one the user gave it, else a branch's ("Branch
    /// of …"), else its current conversation's (the first line of its first
    /// message), else "New session" (or "New chat").
    pub title: String,
    pub mode: Mode,
    /// Where its agents work, or `None` for a chat.
    pub workspace: Option<PathBuf>,
    /// The main agent's model, as `/session` says it.
    pub model: String,
    pub created: DateTime<Utc>,
    /// When it was last worked in, or `None` before its first message.
    pub last_active: Option<SystemTime>,
    /// Its conversations, the first first.
    pub conversations: Vec<ConversationEntry>,
    /// Who has it open now, if anyone.
    pub holder: Option<Holder>,
}

/// The sessions in Arko's folder `root`, the latest worked in first (those
/// never worked in by when they were made).
pub fn list(root: &Path) -> Vec<SessionEntry> {
    let mut entries: Vec<SessionEntry> = session::list(root)
        .into_iter()
        .map(|(session, settings)| {
            let conversations = Conversations::load(&session.dir).unwrap_or_default();
            let entries: Vec<ConversationEntry> = conversations
                .list
                .iter()
                .enumerate()
                .map(|(index, record)| ConversationEntry {
                    number: index + 1,
                    title: record.title.clone(),
                    last_used: None,
                    current: conversations.current == Some(index),
                })
                .collect();
            SessionEntry {
                title: title(&session, &conversations, &settings),
                mode: settings.mode,
                workspace: (settings.mode == Mode::Agent).then(|| settings.workspace.clone()),
                model: settings.describe(&settings.main),
                created: settings.created,
                last_active: session.last_active(),
                conversations: entries,
                holder: session::holder(&session.dir),
                id: session.id,
                dir: session.dir,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        let when = |entry: &SessionEntry| {
            entry
                .last_active
                .unwrap_or_else(|| SystemTime::from(entry.created))
        };
        when(b).cmp(&when(a))
    });
    entries
}

/// The name of `session`, with its `conversations` and `settings`. That's
/// the one the user gave it, else a branch's ("Branch of …"), else its
/// current conversation's (the first line of its first message), else the
/// one its settings keep, else "New session" (or "New chat").
pub(crate) fn title(
    session: &Session,
    conversations: &Conversations,
    settings: &session::Settings,
) -> String {
    session
        .renamed()
        .or_else(|| {
            settings
                .title
                .clone()
                .filter(|_| settings.branched_from.is_some())
        })
        .or_else(|| {
            conversations
                .current
                .and_then(|index| conversations.list.get(index))
                .map(|record| record.title.clone())
                .filter(|title| !title.is_empty())
        })
        .or_else(|| settings.title.clone())
        .unwrap_or_else(|| match settings.mode {
            Mode::Agent => "New session".to_string(),
            Mode::Chat => "New chat".to_string(),
        })
}

/// Why a session wasn't branched.
#[derive(Debug, Clone, PartialEq)]
pub enum BranchError {
    /// It is open (with who has it, if they said), and its own requests
    /// branch it there (`Request::Branch`).
    Locked {
        holder: Option<Holder>,
    },
    /// It has no conversation to go on from.
    NoConversation,
    Io(String),
}

impl std::fmt::Display for BranchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchError::Locked { holder } => write!(
                f,
                "the session is open in {}, so branch it there",
                holder
                    .as_ref()
                    .map_or("another Arko".to_string(), Holder::describe)
            ),
            BranchError::NoConversation => f.write_str(NO_CONVERSATION),
            BranchError::Io(error) => f.write_str(error),
        }
    }
}

/// Why a session without a conversation isn't branched.
pub(crate) const NO_CONVERSATION: &str = "Send a message first, then branch from it.";

/// Branches the session `id`, closed, from what it saved. That's a new
/// session with all of its main agent's current conversation and its
/// settings, as the session's own `Request::Branch` with no message makes
/// one. Replies the new session's ID. The session is held while it is
/// read, so no one opens it meanwhile, and an open one is refused
/// (`Locked`). It reads every run the session saved, so call it off a UI
/// thread. Workers the session had running when it closed stay with it,
/// and the branch's conversation doesn't hear from them.
pub fn branch(root: &Path, id: &str) -> Result<String, BranchError> {
    let session = Session::open(root, id).map_err(BranchError::Io)?;
    let locked = |error| match error {
        session::LockError::Held(holder) => BranchError::Locked { holder },
        session::LockError::Io(error) => BranchError::Io(error.to_string()),
    };
    // As `delete` holds it: the terminal's lock first (an Arko from before
    // session locks holds only that one), then the session's.
    let _terminal = session.lock_terminal().map_err(|error| match error {
        session::LockError::Held(_) => BranchError::Locked {
            holder: session::holder(&session.dir).filter(|holder| !holder.app.is_empty()),
        },
        session::LockError::Io(error) => BranchError::Io(error.to_string()),
    })?;
    let _lock = session.lock("arko", "Arko, branching it").map_err(locked)?;
    let settings = session.load_settings().map_err(BranchError::Io)?;
    let (task, conversations) = saved_conversation(&session).map_err(BranchError::Io)?;
    let task = task.ok_or(BranchError::NoConversation)?;
    let title = title(&session, &conversations, &settings);
    let index = task.conversation.messages.len();
    write_branch(root, &session, &settings, &title, &task, index).map_err(BranchError::Io)
}

/// The task of the main agent's current conversation in what `session`
/// saved (as the session finds it when it opens, a run that answered, or
/// one stopped or interrupted that goes on), if it has one, and its
/// conversations.
fn saved_conversation(
    session: &Session,
) -> Result<(Option<arko_agents::Task>, Conversations), String> {
    let saved = saved_runs(session)?;
    if let Some(seed) = saved.seed {
        return Ok((Some(seed), saved.conversations));
    }
    let task = saved.conversations.current_run().and_then(|run| {
        saved
            .results
            .into_iter()
            .find(|result| result.agent_run_id == run)
            .filter(goes_on)
            .map(|result| result.task)
    });
    Ok((task, saved.conversations))
}

/// What a session saved of its agents' runs.
struct SavedRuns {
    /// Every agent run, none before the first checkpoint.
    results: Vec<arko_workflows::prelude::AgentResult>,
    /// The main agent's conversations, worked out from its runs for a
    /// session saved before they were recorded.
    conversations: Conversations,
    /// The conversation of a branch not opened yet, from its seed.
    seed: Option<arko_agents::Task>,
}

/// What `session` saved of its agents' runs, read without opening it.
fn saved_runs(session: &Session) -> Result<SavedRuns, String> {
    let marker = session.run_dir().join("latest-checkpoint");
    let checkpoint = match std::fs::read_to_string(&marker) {
        Ok(name) => Some(session.run_dir().join(name.trim())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("could not read {} ({error})", marker.display())),
    };
    let Some(checkpoint) = checkpoint else {
        // A branch not opened yet holds its conversation in its seed.
        let unreadable =
            |error: String| format!("could not read the branch's conversation ({error})");
        let seed = match std::fs::read_to_string(session.dir.join(session::SEED_FILE)) {
            Ok(text) => Some(
                serde_json::from_str::<arko_agents::Task>(&text)
                    .map_err(|error| unreadable(error.to_string()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(unreadable(error.to_string())),
        };
        return Ok(SavedRuns {
            results: Vec::new(),
            conversations: Conversations::load(&session.dir).unwrap_or_default(),
            seed,
        });
    };
    let unreadable = |error: String| {
        format!(
            "could not read the session's checkpoint {} ({error})",
            checkpoint.display()
        )
    };
    let text =
        std::fs::read_to_string(&checkpoint).map_err(|error| unreadable(error.to_string()))?;
    let state: arko_workflows::persistence::state::WorkflowState =
        serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
    let results = state
        .agent_results_in(&session.run_dir())
        .map_err(|error| unreadable(error.to_string()))?;
    let runs = crate::conversations::main_runs_of(&results);
    let mut conversations =
        Conversations::load(&session.dir).unwrap_or_else(|| Conversations::derive(&runs));
    conversations.take_in(&runs);
    Ok(SavedRuns {
        results,
        conversations,
        seed: None,
    })
}

/// A conversation of a session as the session saved it, whole
/// ([`conversation`]).
#[derive(Clone)]
pub struct SavedConversation {
    /// Its number, as `/resume` takes it (1 for the session's first).
    pub number: usize,
    /// Its name, the first line of its first message.
    pub title: String,
    /// All its items, as a reopened session's transcript shows them, from
    /// its first message on (or its latest handover's), numbered from 1 with
    /// their turns. When a failed turn left a run that can't go on, the
    /// items of the run that went on after it follow.
    pub items: Vec<remote::Item>,
    /// The images its items show (by [`remote::Item::images`]), the latest
    /// within 128 MB.
    pub images: crate::images::Images,
    /// The reports of its handovers, oldest first. The messages before a
    /// handover aren't kept, and its report stands for them.
    pub reports: Vec<String>,
}

// By hand, since its images aren't `Debug`. It shows what it holds, without
// the images.
impl std::fmt::Debug for SavedConversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedConversation")
            .field("number", &self.number)
            .field("title", &self.title)
            .field("items", &self.items)
            .field("reports", &self.reports)
            .finish_non_exhaustive()
    }
}

/// The conversation `number` of the session `id` (as `/resume` numbers
/// them, or `None` for its current one), read whole from what the session
/// saved, for an export. It doesn't hold the session, so a session open
/// now gives what it saved last, and a turn running may already have more.
/// A front end exporting a session it has open asks the session instead
/// ([`crate::engine::Request::ExportConversation`]), which answers once
/// nothing can still add to it. After `/new`, `None` returns an error until
/// the new conversation has messages. An explicit number still reads an
/// earlier conversation. Missing saved data returns an error.
/// It reads every run the session saved, so call it off a UI thread.
pub fn conversation(
    root: &Path,
    id: &str,
    number: Option<usize>,
) -> Result<SavedConversation, String> {
    conversation_of(&Session::open(root, id)?, number)
}

/// The conversation `number` of `session`, as [`conversation`] reads it.
pub(crate) fn conversation_of(
    session: &Session,
    number: Option<usize>,
) -> Result<SavedConversation, String> {
    let settings = session.load_settings()?;
    let unreadable = |error| format!("The saved conversation can't be read ({error}).");
    let saved = saved_runs(session).map_err(&unreadable)?;
    let images = crate::images::Images::default();
    let mut found = SavedConversation {
        number: 1,
        title: String::new(),
        items: Vec::new(),
        images: images.clone(),
        reports: Vec::new(),
    };
    let add = |found: &mut SavedConversation, task: &arko_agents::Task| {
        let items = remote::all_items(&task.conversation.messages, settings.text_replies, &images);
        let base = found.items.len() as u64;
        let turns = found
            .items
            .iter()
            .filter_map(|item| item.turn)
            .max()
            .unwrap_or(0);
        found.items.extend(items.into_iter().map(|mut item| {
            item.id += base;
            item.call = item.call.map(|call| call + base);
            item.turn = item.turn.map(|turn| turn + turns);
            item
        }));
        found.reports.extend(
            task.handovers
                .iter()
                .map(|handover| handover.report.clone()),
        );
    };
    if let Some(seed) = &saved.seed
        && number.is_none_or(|number| number == 1)
    {
        found.title = crate::conversations::title_of(&seed.initial_task_prompt);
        add(&mut found, seed);
        return Ok(found);
    }
    let list = &saved.conversations.list;
    let index = match number {
        Some(number) => number
            .checked_sub(1)
            .filter(|index| *index < list.len())
            .ok_or_else(|| format!("The session has no conversation {number}."))?,
        None => saved.conversations.current.ok_or(if list.is_empty() {
            "The session has no conversation yet."
        } else {
            "The session's new conversation has no messages yet."
        })?,
    };
    let record = &list[index];
    found.number = index + 1;
    found.title = record.title.clone();
    for run in &record.runs {
        let result = saved
            .results
            .iter()
            .find(|result| result.agent_run_id == *run)
            .ok_or_else(|| unreadable(format!("run {run} is missing")))?;
        add(&mut found, &result.task);
    }
    Ok(found)
}

/// Whether the main agent's saved run `result` goes on when its session
/// opens (see `conversation_of`): it answered, or it was interrupted or
/// stopped (not for good).
fn goes_on(result: &arko_workflows::prelude::AgentResult) -> bool {
    use arko_workflows::prelude::AgentRunStatus;
    match result.status {
        AgentRunStatus::Completed | AgentRunStatus::Interrupted => true,
        AgentRunStatus::Cancelled => {
            result.task.parent.is_none()
                && result
                    .stopped_by
                    .as_ref()
                    .is_some_and(|stop| !stop.is_retired())
        }
        _ => false,
    }
}

/// Writes a branch of the session `from` (with its `settings` and named
/// `title`) holding the conversation of `task` up to just before message
/// `index`: all of it with the conversation's length, and none with 0 (it
/// keeps the reports of the handovers the conversation had, the latest of
/// which its first message goes on from). The branch is a new session
/// with the settings copied, named "Branch of <title>", that knows where
/// it came from (`branched_from`): a chat's own folder is its workspace.
/// It takes in the conversation from its seed when it opens (see
/// `Harness::import_seed`). Its ID.
pub(crate) fn write_branch(
    root: &Path,
    from: &Session,
    settings: &session::Settings,
    title: &str,
    task: &arko_agents::Task,
    index: usize,
) -> Result<String, String> {
    let branch = task.branch(index).map_err(|error| error.to_string())?;
    let seed = serde_json::to_string(&branch)
        .map_err(|error| format!("could not write the branch's conversation ({error})"))?;
    let session =
        Session::create(root).map_err(|error| format!("could not create a session ({error})"))?;
    let mut settings = settings.clone();
    settings.created = Utc::now();
    settings.branched_from = Some(from.id.clone());
    settings.title = Some(format!("Branch of {title}"));
    // A chat works in its own folder (also when named another way).
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if real(&settings.workspace) == real(&from.dir) {
        settings.workspace = session.dir.clone();
    }
    // (The branch's conversation can name a profile the session saved.)
    let written = session
        .save_settings(&settings)
        .and_then(|()| std::fs::write(session.dir.join(session::SEED_FILE), seed))
        .and_then(|()| crate::profiles::copy_session_profiles(&from.dir, &session.dir));
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&session.dir);
        return Err(format!("could not save the branch ({error})"));
    }
    Ok(session.id)
}

/// Gives the session `id` the name `title`, or back its conversation's when
/// `title` is empty. An open session takes it too, told so through its
/// requests (`Request::Rename`).
pub fn rename(root: &Path, id: &str, title: &str) -> Result<(), String> {
    Session::open(root, id)?.rename(title)
}

/// Who has the session `id` open, if anyone.
pub fn holder(root: &Path, id: &str) -> Option<Holder> {
    if !session::is_session_id(id) {
        return None;
    }
    session::holder(&root.join("sessions").join(id))
}

/// What deleting a session leaves. Its workers' worktrees with changes
/// that aren't committed stay (with their branches), and the clean ones go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeletePlan {
    pub worktrees_clean: Vec<PathBuf>,
    /// Each with its branch.
    pub worktrees_dirty: Vec<(PathBuf, Option<String>)>,
}

/// What deleting a session left.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deleted {
    /// The worktrees with changes, kept in place.
    pub kept_worktrees: Vec<PathBuf>,
}

/// Why a session wasn't deleted.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteError {
    /// It is open (who has it, if they said).
    Locked {
        holder: Option<Holder>,
    },
    Io(String),
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeleteError::Locked { holder } => write!(
                f,
                "the session is open in {}, so close it there first",
                holder
                    .as_ref()
                    .map_or("another Arko".to_string(), Holder::describe)
            ),
            DeleteError::Io(error) => f.write_str(error),
        }
    }
}

/// What deleting the session `id` would do, for the user to agree to.
pub fn delete_plan(root: &Path, id: &str) -> Result<DeletePlan, DeleteError> {
    let session = Session::open(root, id).map_err(DeleteError::Io)?;
    let mut plan = DeletePlan::default();
    for worktree in worktrees(&session.dir) {
        if is_clean(&worktree) {
            plan.worktrees_clean.push(worktree);
        } else {
            let branch = git_branch(&worktree);
            plan.worktrees_dirty.push((worktree, branch));
        }
    }
    Ok(plan)
}

/// Deletes the session `id`, which removes its folder. Its workers'
/// worktrees with uncommitted changes stay in place (and registered with
/// their repository), and so do all branches. The clean worktrees are
/// removed as git removes them (`git worktree remove`, never forced). The
/// workspace is left alone. A session open anywhere isn't deleted.
pub fn delete(root: &Path, id: &str) -> Result<Deleted, DeleteError> {
    let session = Session::open(root, id).map_err(DeleteError::Io)?;
    // A link to a folder elsewhere isn't the session's own folder: what is
    // there stays.
    let linked = std::fs::symlink_metadata(&session.dir)
        .map_err(|error| DeleteError::Io(error.to_string()))?
        .file_type()
        .is_symlink();
    if linked {
        return Err(DeleteError::Io(format!(
            "{} is a link to a folder elsewhere, and Arko deletes only a session's own folder.",
            session.dir.display()
        )));
    }
    let locked = |error| match error {
        session::LockError::Held(holder) => DeleteError::Locked { holder },
        session::LockError::Io(error) => DeleteError::Io(error.to_string()),
    };
    // Held while its files go: no Arko opens it meanwhile. An Arko from
    // before session locks holds only its terminal's, which is tried first
    // (its session gets no lock file of Arko's then).
    let terminal = session.lock_terminal().map_err(|error| match error {
        session::LockError::Held(_) => DeleteError::Locked {
            holder: session::holder(&session.dir).filter(|holder| !holder.app.is_empty()),
        },
        session::LockError::Io(error) => DeleteError::Io(error.to_string()),
    })?;
    let lock = session.lock("arko", "Arko, deleting it").map_err(locked)?;
    let mut kept = Vec::new();
    for worktree in worktrees(&session.dir) {
        if !(is_clean(&worktree) && remove_worktree(&worktree)) {
            kept.push(worktree);
        }
    }
    let io = |error: std::io::Error| DeleteError::Io(error.to_string());
    let locks = [
        session.dir.join(session::LOCK_FILE),
        session.dir.join(session::HOLDER_FILE),
        session.dir.join(session::TERMINAL_LOCK),
    ];
    clear(&session.dir, &kept, &locks).map_err(io)?;
    // Then the locks themselves (a held file can't be removed everywhere).
    drop(terminal);
    drop(lock);
    for path in &locks {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(io(error)),
            _ => {}
        }
    }
    if let Some(jobs) = locks[2].parent()
        && jobs.exists()
    {
        std::fs::remove_dir(jobs).map_err(io)?;
    }
    if kept.is_empty() {
        std::fs::remove_dir(&session.dir).map_err(io)?;
    }
    Ok(Deleted {
        kept_worktrees: kept,
    })
}

/// Deletes what the session's folder `dir` holds, but its worktrees `kept`
/// and its lock files `locks` (and the folders they are in).
fn clear(dir: &Path, kept: &[PathBuf], locks: &[PathBuf]) -> std::io::Result<()> {
    let worktrees = dir.join(WORKTREES);
    for entry in std::fs::read_dir(dir)?.filter_map(Result::ok) {
        let path = entry.path();
        if locks.contains(&path) {
            continue;
        }
        let keep: Vec<PathBuf> = if path == worktrees {
            kept.to_vec()
        } else {
            locks
                .iter()
                .filter(|lock| lock.starts_with(&path))
                .cloned()
                .collect()
        };
        let result = if !keep.is_empty() || path == worktrees {
            remove_all_but(&path, &keep)
        } else if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        result.map_err(|error| {
            std::io::Error::other(format!("could not delete {} ({error})", path.display()))
        })?;
    }
    Ok(())
}

/// The folder in a session's folder that holds its workers' worktrees.
const WORKTREES: &str = "worktrees";

/// The worktrees in the session's folder `dir`.
fn worktrees(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir.join(WORKTREES)) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").exists())
        .collect();
    found.sort();
    found
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .no_console_window()
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether the worktree at `dir` has nothing that isn't committed (git
/// can't tell: it has).
fn is_clean(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain"]).is_some_and(|status| status.is_empty())
}

fn git_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD")
}

/// Removes the clean worktree at `dir` as git does, from its repository.
fn remove_worktree(dir: &Path) -> bool {
    let Some(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return false;
    };
    Command::new("git")
        .arg("--git-dir")
        .arg(&common)
        .args(["worktree", "remove"])
        .arg(dir)
        .no_console_window()
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Removes what is in `dir` but the folders `keep`.
fn remove_all_but(dir: &Path, keep: &[PathBuf]) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.filter_map(Result::ok) {
        let path = entry.path();
        if keep.contains(&path) {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    if keep.is_empty() {
        std::fs::remove_dir(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Effort, ModelSettings, resolve_model};
    use crate::session::Settings;

    fn settings(workspace: &Path) -> Settings {
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        Settings::for_test(workspace, &luna)
    }

    #[test]
    fn export_refuses_missing_saved_runs_and_checkpoints() {
        let temp = tempfile::tempdir().unwrap();
        let session = Session::create(temp.path()).unwrap();
        session.save_settings(&settings(temp.path())).unwrap();
        let conversations = Conversations {
            list: vec![crate::conversations::Record {
                runs: vec![uuid::Uuid::new_v4()],
                title: "Saved words".into(),
                unread: Vec::new(),
            }],
            current: Some(0),
        };
        conversations.save(&session.dir).unwrap();
        let check = || {
            for number in [None, Some(1)] {
                let error = conversation(temp.path(), &session.id, number)
                    .err()
                    .unwrap();
                assert!(
                    error.starts_with("The saved conversation can't be read"),
                    "{error}"
                );
            }
        };
        check();
        std::fs::create_dir_all(session.run_dir()).unwrap();
        std::fs::write(session.run_dir().join("latest-checkpoint"), "gone.json").unwrap();
        check();
    }

    /// The list names each session (as the user renamed it, else as its
    /// first message), says what it is and where it works, and who has it
    /// open. An open session is renamed but not deleted.
    #[test]
    fn sessions_are_listed_renamed_and_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let first = Session::create(&root).unwrap();
        first
            .save_settings(&Settings {
                title: Some("Count the pears".into()),
                ..settings(&workspace)
            })
            .unwrap();
        let chat = Session::create(&root).unwrap();
        chat.save_settings(&Settings {
            mode: Mode::Chat,
            created: Utc::now() + chrono::Duration::seconds(1),
            ..settings(&workspace)
        })
        .unwrap();
        let listed = list(&root);
        assert_eq!(
            listed
                .iter()
                .map(|entry| (entry.id.as_str(), entry.title.as_str(), entry.mode))
                .collect::<Vec<_>>(),
            [
                (chat.id.as_str(), "New chat", Mode::Chat),
                (first.id.as_str(), "Count the pears", Mode::Agent),
            ],
            "the newest first"
        );
        assert_eq!(listed[0].workspace, None, "a chat works nowhere");
        assert_eq!(listed[1].workspace.as_deref(), Some(workspace.as_path()));
        assert!(listed.iter().all(|entry| entry.holder.is_none()));

        let lock = first.lock("arko", "Arko in a terminal").unwrap();
        rename(&root, &first.id, "  Pears  ").unwrap();
        let listed = list(&root);
        let entry = listed.iter().find(|entry| entry.id == first.id).unwrap();
        assert_eq!(entry.title, "Pears");
        assert_eq!(
            entry.holder.as_ref().map(|holder| holder.app.as_str()),
            Some("arko")
        );
        assert!(matches!(
            delete(&root, &first.id),
            Err(DeleteError::Locked { holder: Some(_) })
        ));
        drop(lock);
        rename(&root, &first.id, "").unwrap();
        assert_eq!(
            list(&root)
                .iter()
                .find(|entry| entry.id == first.id)
                .unwrap()
                .title,
            "Count the pears"
        );
        assert_eq!(delete(&root, &first.id), Ok(Deleted::default()));
        assert!(!first.dir.exists());
        assert_eq!(list(&root).len(), 1);
        assert!(workspace.is_dir(), "the workspace stays");
    }

    /// An Arko from before session locks holds only its terminal's: the
    /// session is listed as open, by someone who didn't say, and isn't
    /// deleted until it lets go.
    #[test]
    fn an_older_arko_holding_a_session_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let session = Session::create(&root).unwrap();
        session.save_settings(&settings(temp.path())).unwrap();
        std::fs::create_dir_all(session.jobs_dir()).unwrap();
        let terminal = std::fs::File::create(session.dir.join(session::TERMINAL_LOCK)).unwrap();
        terminal.try_lock().unwrap();
        let someone = holder(&root, &session.id).unwrap();
        assert_eq!(someone.app, "", "someone who didn't say");
        assert!(list(&root)[0].holder.is_some());
        assert_eq!(
            delete(&root, &session.id),
            Err(DeleteError::Locked { holder: None })
        );
        assert!(
            session.dir.join(session::SETTINGS_FILE).exists(),
            "nothing deleted"
        );
        assert!(
            !session.dir.join(session::LOCK_FILE).exists(),
            "no lock file of Arko's left in its session"
        );
        drop(terminal);
        assert_eq!(holder(&root, &session.id), None);
        assert_eq!(delete(&root, &session.id), Ok(Deleted::default()));
        assert!(!session.dir.exists());
    }

    /// A session held through a lock file that can only be read (another
    /// user's) is listed as open, and isn't deleted.
    #[cfg(unix)]
    #[test]
    fn a_lock_file_that_can_only_be_read_still_holds_the_session() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let session = Session::create(&root).unwrap();
        session.save_settings(&settings(temp.path())).unwrap();
        std::fs::create_dir_all(session.jobs_dir()).unwrap();
        let only_read = std::fs::Permissions::from_mode(0o444);
        for name in [session::LOCK_FILE, session::TERMINAL_LOCK] {
            let path = session.dir.join(name);
            let file = std::fs::File::create(&path).unwrap();
            std::fs::set_permissions(&path, only_read.clone()).unwrap();
            file.try_lock().unwrap();
            assert!(holder(&root, &session.id).is_some(), "{name}");
            assert!(list(&root)[0].holder.is_some(), "{name}");
            assert!(
                matches!(delete(&root, &session.id), Err(DeleteError::Locked { .. })),
                "{name}"
            );
            assert!(session.dir.join(session::SETTINGS_FILE).exists());
            drop(file);
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A session's folder that is a link to a folder elsewhere isn't
    /// deleted, nor is anything there.
    #[cfg(unix)]
    #[test]
    fn a_session_folder_that_is_a_link_isnt_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(session::SETTINGS_FILE), "{}").unwrap();
        std::fs::write(elsewhere.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("sessions").join("linked")).unwrap();
        let refused = delete(&root, "linked");
        assert!(
            matches!(&refused, Err(DeleteError::Io(why)) if why.contains("link")),
            "{refused:?}"
        );
        assert!(elsewhere.join("main.rs").exists());
        assert!(root.join("sessions").join("linked").exists());
    }

    /// A session's ID is a folder's name in `sessions/`, never a path: a
    /// path given for one names no session, and nothing there is deleted.
    #[test]
    fn a_session_id_is_never_a_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(session::SETTINGS_FILE), "{}").unwrap();
        std::fs::write(project.join("main.rs"), "fn main() {}").unwrap();
        let outside = project.to_string_lossy().into_owned();
        assert!(matches!(delete(&root, &outside), Err(DeleteError::Io(_))));
        assert!(project.join("main.rs").exists());
        for id in ["", ".", "..", "../project", "a/b", "a\\b", outside.as_str()] {
            assert!(!session::is_session_id(id), "{id}");
            assert!(Session::open(&root, id).is_err(), "{id}");
            assert_eq!(holder(&root, id), None, "{id}");
        }
        assert!(session::is_session_id("20261016-170000-ab12"));
    }

    /// A deleted session's files go while its locks are still held (so no
    /// Arko opens it half deleted), then the locks.
    #[test]
    fn a_session_is_deleted_while_it_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let session = Session::create(&root).unwrap();
        session.save_settings(&settings(temp.path())).unwrap();
        std::fs::create_dir_all(session.jobs_dir().join("job-1")).unwrap();
        std::fs::create_dir_all(session.run_dir()).unwrap();
        std::fs::File::create(session.dir.join(session::TERMINAL_LOCK)).unwrap();
        let lock = session.lock("arko", "Arko, deleting it").unwrap();
        let terminal = session.lock_terminal().unwrap();
        let locks = [
            session.dir.join(session::LOCK_FILE),
            session.dir.join(session::HOLDER_FILE),
            session.dir.join(session::TERMINAL_LOCK),
        ];
        clear(&session.dir, &[], &locks).unwrap();
        let mut left: Vec<String> = walk(&session.dir);
        left.sort();
        assert_eq!(left, ["holder.json", "jobs", "jobs/.lock", "lock"]);
        assert!(session::holder(&session.dir).is_some(), "still held");
        drop((terminal, lock));
    }

    /// The paths under `dir`, relative to it.
    fn walk(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            let name = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                found.extend(
                    walk(&path)
                        .into_iter()
                        .map(|inner| format!("{name}/{inner}")),
                );
            }
            found.push(name);
        }
        found
    }

    /// A deleted session's clean worker worktrees go as git removes them.
    /// One with uncommitted changes stays in place, and every branch stays.
    #[test]
    fn a_dirty_worker_worktree_is_kept_on_delete() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=Arko", "-c", "user.email=arko@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "a"]);
        let root = temp.path().join("arko");
        let session = Session::create(&root).unwrap();
        session.save_settings(&settings(&repo)).unwrap();
        let clean = session.dir.join(WORKTREES).join("r1");
        let dirty = session.dir.join(WORKTREES).join("r2");
        for (path, branch) in [(&clean, "arko/s/r1"), (&dirty, "arko/s/r2")] {
            run(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }
        std::fs::write(dirty.join("b.txt"), "work in progress").unwrap();

        let plan = delete_plan(&root, &session.id).unwrap();
        assert_eq!(plan.worktrees_clean, std::slice::from_ref(&clean));
        assert_eq!(
            plan.worktrees_dirty,
            [(dirty.clone(), Some("arko/s/r2".to_string()))]
        );

        let deleted = delete(&root, &session.id).unwrap();
        assert_eq!(deleted.kept_worktrees, std::slice::from_ref(&dirty));
        assert!(!clean.exists());
        assert!(dirty.join("b.txt").is_file());
        assert!(!session.dir.join("session.json").exists());
        let branches = run(&repo, &["branch", "--list"]);
        assert!(
            branches.contains("arko/s/r1") && branches.contains("arko/s/r2"),
            "{branches}"
        );
        let worktrees = run(&repo, &["worktree", "list"]);
        assert!(
            !worktrees.contains("/r1 ") && worktrees.contains("/r2 "),
            "{worktrees}"
        );
    }
}
