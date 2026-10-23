//! Credentials: the API keys of the model providers (Anthropic, OpenAI,
//! Google Gemini, OpenRouter) and the ChatGPT login (a Codex auth.json),
//! from the environment or `.env` files, and the setup screen that asks for
//! them when the chosen model has none or its file fails. The screen also
//! checks Claude Code, whose own login Arko uses. `.env` files can also
//! turn on the 1-hour prompt cache (ARKO_CACHE_1H), choose how sessions
//! reach OpenAI models (ARKO_OPENAI_AUTH), choose the shell on Windows
//! (ARKO_SHELL) and hold the Voyage API key the search indexes embed with
//! (VOYAGE_API_KEY), which the setup screen also takes.
//!
//! What the screen takes is saved in Arko's own `.env` (in its folder,
//! readable only by the user) and used in this run at once, and so is what
//! the keys menu (`/keys`, see `keys`) takes. Nothing read here is put in the
//! environment, so the commands the agents run don't see it.

#[cfg(feature = "tui")]
use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(feature = "tui")]
use arko_ai::CodexAuthFile;
#[cfg(feature = "tui")]
use rustyline::completion::{Completer, FilenameCompleter, Pair};
#[cfg(feature = "tui")]
use rustyline::config::Configurer;
#[cfg(feature = "tui")]
use rustyline::error::ReadlineError;
#[cfg(feature = "tui")]
use rustyline::highlight::{CmdKind, Highlighter};
#[cfg(feature = "tui")]
use rustyline::hint::Hinter;
#[cfg(feature = "tui")]
use rustyline::validate::Validator;
#[cfg(feature = "tui")]
use rustyline::{ColorMode, Context, Helper};

use crate::claude_code;
use crate::session;
#[cfg(feature = "tui")]
use crate::ui::Output;

/// The Anthropic API key.
pub const ANTHROPIC_KEY: &str = "ANTHROPIC_API_KEY";
/// The OpenAI API key, for sessions that reach OpenAI models with one.
pub const OPENAI_KEY: &str = "OPENAI_API_KEY";
/// The Google Gemini API key, as Google names it now.
pub const GEMINI_KEY: &str = "GEMINI_API_KEY";
/// The Google Gemini API key's older name, which comes first.
pub const GOOGLE_KEY: &str = "GOOGLE_API_KEY";
/// The OpenRouter API key.
pub const OPENROUTER_KEY: &str = "OPENROUTER_API_KEY";
/// The Codex auth.json that holds the ChatGPT login.
pub const CODEX_AUTH: &str = "ARKO_CODEX_AUTH";
/// How new sessions reach OpenAI models: `chatgpt` or `api-key`.
pub const OPENAI_AUTH: &str = "ARKO_OPENAI_AUTH";
/// Anthropic's 1-hour prompt cache for new sessions: `1` or `0`.
pub const CACHE_1H: &str = "ARKO_CACHE_1H";
/// Whether new sessions have Arko's built-in skills: `1` or `0`.
pub const BUILTIN_SKILLS: &str = "ARKO_BUILTIN_SKILLS";
/// The built-in skills new sessions leave out, by name, separated by commas.
pub const BUILTIN_SKILLS_OFF: &str = "ARKO_BUILTIN_SKILLS_OFF";
/// The shell new sessions run commands in on Windows: `cmd` or `powershell`.
pub const SHELL: &str = "ARKO_SHELL";
/// The Voyage API key, for the search indexes' embeddings.
pub const VOYAGE_KEY: &str = "VOYAGE_API_KEY";
/// The full-screen view's theme, by name.
pub const THEME: &str = "ARKO_THEME";
/// The settings a `.env` file can hold; it may set others, which are left
/// alone.
const DOTENV_KEYS: [&str; 15] = [
    ANTHROPIC_KEY,
    OPENAI_KEY,
    GEMINI_KEY,
    GOOGLE_KEY,
    OPENROUTER_KEY,
    CODEX_AUTH,
    OPENAI_AUTH,
    CACHE_1H,
    BUILTIN_SKILLS,
    BUILTIN_SKILLS_OFF,
    SHELL,
    VOYAGE_KEY,
    crate::glyphs::SYMBOLS,
    THEME,
    crate::remote::HOST_VARIABLE,
];

/// What was entered on the setup screen, in the keys menu or in an app's
/// accounts in this run, by the Arko `.env` it was saved in: it comes
/// before every other source for the sessions of that Arko folder.
static ENTERED: Mutex<Vec<(PathBuf, &str, String)>> = Mutex::new(Vec::new());

/// The settings removed from an Arko `.env` in this run (with that file):
/// what it held as Arko started is no longer read.
static FORGOTTEN: Mutex<Vec<(PathBuf, &str)>> = Mutex::new(Vec::new());

/// Where a setting comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Entered,
    Environment,
    DotEnv(PathBuf),
}

/// A setting, if it is set: entered on the setup screen or in the keys
/// menu, else from the environment, else (the settings Arko reads there) from
/// a `.env` file, as the session's [`Env`] finds them.
pub fn var(name: &str) -> Option<String> {
    lookup(name).map(|(value, _)| value)
}

fn lookup(name: &str) -> Option<(String, Source)> {
    current().lookup(name)
}

/// Where a session's settings and keys are looked up. The first place that
/// sets one wins, in this order:
///
/// 1. What was entered in this process for its Arko folder (the setup
///    screen, the keys menu, an app's accounts).
/// 2. The process's environment.
/// 3. The nearest `.env` in the session's folder or a parent.
/// 4. The `.env` beside Arko's program.
/// 5. The `.env` in Arko's folder, where keys are saved.
///
/// The files are read once, as the `Env` is made.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// The settings in the files, each with the first file that sets it.
    files: Arc<HashMap<&'static str, (String, PathBuf)>>,
    /// The `.env` in Arko's folder: where keys are saved, and what was
    /// entered and removed in this run goes by. `None` when Arko's folder
    /// isn't known.
    arko: Option<PathBuf>,
}

impl Env {
    /// The `.env` files for `folder` (a session's workspace, else the
    /// current directory's), with the one beside Arko's program when
    /// `beside_binary`, and the one in Arko's folder (ARKO_HOME, else the
    /// user's data folder).
    pub fn for_folder(folder: Option<&Path>, beside_binary: bool) -> Env {
        let root = session::data_root().ok();
        Env::for_folder_in(folder, beside_binary, root.as_deref())
    }

    /// As [`for_folder`](Self::for_folder), with Arko's folder `root` (an
    /// app's, or [`Options::root`](crate::Options::root)). Its `.env` is
    /// read last, and keys are saved there.
    pub fn for_folder_in(folder: Option<&Path>, beside_binary: bool, root: Option<&Path>) -> Env {
        let program = beside_binary
            .then(|| std::env::current_exe().ok())
            .flatten();
        Env::for_program(folder, program.as_deref(), root)
    }

    /// As [`for_folder_in`](Self::for_folder_in), with the `.env` beside
    /// `program` (Arko's program, when it is read).
    fn for_program(folder: Option<&Path>, program: Option<&Path>, root: Option<&Path>) -> Env {
        let beside = program.and_then(|program| program.parent().map(|folder| folder.join(".env")));
        let arko = root.map(|root| root.join(".env"));
        Env::read(&dotenv_paths(folder, beside, arko.clone()), arko)
    }

    /// The settings in the files at `paths`, in order: the first that sets
    /// one is read. `arko` is Arko's own `.env`.
    fn read(paths: &[PathBuf], arko: Option<PathBuf>) -> Env {
        let mut values = HashMap::new();
        for path in paths {
            for (key, value) in read_dotenv(path) {
                values.entry(key).or_insert((value, path.clone()));
            }
        }
        Env {
            files: Arc::new(values),
            arko,
        }
    }

    /// The `.env` in Arko's folder, where keys are saved.
    pub fn arko_dotenv(&self) -> Result<PathBuf, String> {
        self.arko
            .clone()
            .ok_or_else(|| "Arko's folder isn't known. Set ARKO_HOME".to_string())
    }

    /// What was entered as `name` in this run, for Arko's folder.
    fn entered(&self, name: &str) -> Option<String> {
        let arko = self.arko.as_ref()?;
        ENTERED
            .lock()
            .unwrap()
            .iter()
            .find(|(file, key, _)| file == arko && *key == name)
            .map(|(_, _, value)| value.clone())
    }

    /// `value` was entered as `name` (and saved in Arko's `.env`): it comes
    /// first for this Arko folder's sessions from now on.
    fn enter(&self, name: &'static str, value: String) {
        let Some(arko) = &self.arko else {
            return;
        };
        let mut entered = ENTERED.lock().unwrap();
        entered.retain(|(file, key, _)| !(file == arko && *key == name));
        entered.push((arko.clone(), name, value));
        FORGOTTEN
            .lock()
            .unwrap()
            .retain(|(file, key)| !(file == arko && *key == name));
    }

    /// `name` was removed from Arko's `.env`: what it held, and what was
    /// entered as it, are no longer read.
    fn forget(&self, name: &'static str) {
        let Some(arko) = &self.arko else {
            return;
        };
        ENTERED
            .lock()
            .unwrap()
            .retain(|(file, key, _)| !(file == arko && *key == name));
        let mut forgotten = FORGOTTEN.lock().unwrap();
        if !forgotten
            .iter()
            .any(|(file, key)| file == arko && *key == name)
        {
            forgotten.push((arko.clone(), name));
        }
    }

    /// A setting's value, if one of the places this looks in sets it.
    pub fn var(&self, name: &str) -> Option<String> {
        self.lookup(name).map(|(value, _)| value)
    }

    /// A setting and where it comes from.
    pub fn lookup(&self, name: &str) -> Option<(String, Source)> {
        if let Some(value) = self.entered(name) {
            return Some((value, Source::Entered));
        }
        // A Gemini key entered in this run comes before the older name too,
        // which arko reads first.
        if name == GOOGLE_KEY && self.entered(GEMINI_KEY).is_some() {
            return None;
        }
        if let Some(value) = std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return Some((value, Source::Environment));
        }
        self.file_entry(name)
            .map(|(value, path)| (value, Source::DotEnv(path)))
    }

    /// The `.env` setting `name` with the file it came from, unless it was
    /// removed from Arko's own `.env` in this run.
    fn file_entry(&self, name: &str) -> Option<(String, PathBuf)> {
        let (value, path) = self.files.get(name)?;
        let forgotten = FORGOTTEN
            .lock()
            .unwrap()
            .iter()
            .any(|(file, key)| file == path && *key == name);
        if forgotten && self.arko.as_ref() == Some(path) {
            return None;
        }
        Some((value.clone(), path.clone()))
    }
}

/// The `.env` files a session in `folder` reads, in order: the nearest in
/// `folder` or a parent, `beside` (the program's), then `arko` (Arko's
/// folder's), each once.
fn dotenv_paths(
    folder: Option<&Path>,
    beside: Option<PathBuf>,
    arko: Option<PathBuf>,
) -> Vec<PathBuf> {
    let nearest = folder.and_then(|folder| {
        folder
            .ancestors()
            .map(|folder| folder.join(".env"))
            .find(|path| path.is_file())
    });
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in nearest.into_iter().chain(beside).chain(arko) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// The `Env` the process reads, until a session sets its own: the current
/// directory's.
static CURRENT: Mutex<Option<Env>> = Mutex::new(None);

/// The `Env` settings and keys are read from now: the session's (each
/// session's own, where several run in one process).
pub fn current() -> Env {
    if let Some(env) = crate::context::env() {
        return env;
    }
    CURRENT
        .lock()
        .unwrap()
        .get_or_insert_with(|| Env::for_folder(std::env::current_dir().ok().as_deref(), true))
        .clone()
}

/// Reads settings and keys from `env` from now on, such as the session's
/// once it is known.
pub fn use_env(env: Env) {
    if !crate::context::set_env(&env) {
        *CURRENT.lock().unwrap() = Some(env);
    }
}

/// An API key added or removed in this process, which the other sessions
/// open on the same Arko folder use too ([`changes`]).
#[derive(Clone)]
pub struct KeyChange {
    /// Arko's folder, whose sessions it is for.
    pub root: PathBuf,
    pub key: &'static Key,
    /// The key added. `None`: removed, and each session goes by the next
    /// one it reads, if any.
    pub value: Option<String>,
    /// Who changed it (one session's keys), which has it already.
    pub origin: u64,
}

impl std::fmt::Debug for KeyChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyChange")
            .field("root", &self.root)
            .field("key", &self.key.name)
            .field("added", &self.value.is_some())
            .field("origin", &self.origin)
            .finish()
    }
}

fn change_sender() -> &'static tokio::sync::broadcast::Sender<KeyChange> {
    static SENDER: std::sync::OnceLock<tokio::sync::broadcast::Sender<KeyChange>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

/// The keys added and removed in this process from now on.
pub fn changes() -> tokio::sync::broadcast::Receiver<KeyChange> {
    change_sender().subscribe()
}

/// Tells the open sessions that a key was added or removed.
pub fn announce(change: KeyChange) {
    let _ = change_sender().send(change);
}

fn entered(name: &str) -> Option<String> {
    current().entered(name)
}

/// (The setup screen's: it enters what the user typed.)
#[cfg(feature = "tui")]
fn enter(name: &'static str, value: String) {
    current().enter(name, value);
}

/// Arko's own `.env`, in the session's Arko folder: where the setup screen
/// and the keys menu save.
pub fn arko_dotenv() -> Result<PathBuf, String> {
    current().arko_dotenv()
}

/// The settings Arko reads in the `.env` file at `path`. A relative
/// ARKO_CODEX_AUTH is relative to the file's folder.
fn read_dotenv(path: &Path) -> Vec<(&'static str, String)> {
    let Ok(entries) = dotenvy::from_path_iter(path) else {
        return Vec::new();
    };
    let mut found: Vec<(&'static str, String)> = Vec::new();
    for (key, value) in entries.filter_map(Result::ok) {
        let Some(&key) = DOTENV_KEYS.iter().find(|known| **known == key) else {
            continue;
        };
        if value.trim().is_empty() || found.iter().any(|(seen, _)| *seen == key) {
            continue;
        }
        let value = if key == CODEX_AUTH {
            let folder = path.parent().unwrap_or(Path::new(""));
            let paths = login_paths(&value, Some(folder));
            join_logins(&paths).unwrap_or(value)
        } else {
            value
        };
        found.push((key, value));
    }
    found
}

/// `~` and `~/...` with the home directory in place of `~`.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    match home {
        Some(home) if path == "~" => home,
        Some(home) => match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// The Codex auth.json files in ARKO_CODEX_AUTH's `value`, in order. It is
/// one path, or several separated as the PATH variable's are (`:`, and `;`
/// on Windows). A value that names a file as a whole is one path. A
/// relative path is relative to `folder`, if there is one.
fn login_paths(value: &str, folder: Option<&Path>) -> Vec<PathBuf> {
    let resolve = |path: &str| match folder {
        Some(folder) => folder.join(expand_home(path.trim())),
        None => expand_home(path.trim()),
    };
    let whole = resolve(value);
    if whole.is_file() {
        return vec![whole];
    }
    std::env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| resolve(&path.to_string_lossy()))
        .collect()
}

/// `paths` as ARKO_CODEX_AUTH holds them: one path as it is, several as
/// [`login_paths`] reads them.
/// Fails, naming the path, when one of several holds the separator.
fn join_logins(paths: &[PathBuf]) -> Result<String, String> {
    if let [one] = paths {
        return Ok(one.to_string_lossy().into_owned());
    }
    std::env::join_paths(paths)
        .map(|joined| joined.to_string_lossy().into_owned())
        .map_err(|_| {
            let separator = if cfg!(windows) { ';' } else { ':' };
            let path = paths
                .iter()
                .find(|path| std::env::join_paths([path]).is_err());
            format!(
                "{} can't be in a list of logins, because its path holds `{separator}`.",
                path.map_or_else(String::new, |path| path.display().to_string())
            )
        })
}

/// A yes-or-no setting: `1`, `true`, `yes` or `on`, or `0`, `false`, `no`
/// or `off`; `None` when it isn't set. Another value is an error.
pub fn switch(name: &str) -> Result<Option<bool>, String> {
    let Some((value, source)) = lookup(name) else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(format!(
            "{name} ({}) is `{value}`. Set it to 1 or 0",
            describe(&source)
        )),
    }
}

/// The Codex auth.json files to use, in order of preference: those entered
/// on the setup screen or in this process, else `--codex-auth`, else
/// ARKO_CODEX_AUTH (one path, or several as [`login_paths`] says). None
/// leaves it to Codex's default (`$CODEX_HOME/auth.json`, else
/// `~/.codex/auth.json`).
pub fn codex_auths(flag: Option<&Path>) -> Vec<PathBuf> {
    if let Some(entered) = entered(CODEX_AUTH) {
        return login_paths(&entered, None);
    }
    if let Some(flag) = flag {
        return vec![flag.to_path_buf()];
    }
    var(CODEX_AUTH)
        .map(|value| login_paths(&value, None))
        .unwrap_or_default()
}

/// The first of [`codex_auths`].
pub fn codex_auth(flag: Option<&Path>) -> Option<PathBuf> {
    codex_auths(flag).into_iter().next()
}

/// Whether the Codex auth.json at `path` holds a ChatGPT login, else why
/// not.
fn check_login(path: &Path) -> Result<(), String> {
    arko_ai::CodexAuthFile::load(path)
        .map_err(|error| error.to_string())
        .and_then(|auth| {
            auth.credentials()
                .map(|_| ())
                .map_err(|error| format!("it holds no ChatGPT login ({error})"))
        })
}

/// Where the ChatGPT login is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginSource {
    /// Entered in this process (the setup screen, an app's accounts).
    Entered,
    /// `--codex-auth`.
    Flag,
    /// ARKO_CODEX_AUTH, in the environment or a .env file.
    Setting,
    /// Codex's own (`$CODEX_HOME/auth.json`, else `~/.codex/auth.json`).
    Default,
}

/// Whether the ChatGPT login can be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    Ready,
    /// There is no file there.
    Missing,
    /// The file is there but holds no login Arko can use, and the text says
    /// why.
    Invalid(String),
}

/// The ChatGPT login, with its Codex auth.json, where Arko takes it from,
/// and whether it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptLogin {
    /// `None` when no file is set and Codex's own folder isn't known.
    pub path: Option<PathBuf>,
    pub source: LoginSource,
    pub state: LoginState,
}

/// The first of [`chatgpt_logins`]: the ChatGPT login Arko uses first.
pub fn chatgpt_login(flag: Option<&Path>) -> ChatGptLogin {
    chatgpt_logins(flag)
        .into_iter()
        .next()
        .unwrap_or(ChatGptLogin {
            path: None,
            source: LoginSource::Default,
            state: LoginState::Missing,
        })
}

/// The ChatGPT logins Arko uses, in order of preference. They are those
/// entered in this process, else `flag` (`--codex-auth`), else
/// ARKO_CODEX_AUTH, else Codex's own (which is none when Codex's folder
/// isn't known).
pub fn chatgpt_logins(flag: Option<&Path>) -> Vec<ChatGptLogin> {
    let (paths, source) = if let Some(entered) = entered(CODEX_AUTH) {
        (login_paths(&entered, None), LoginSource::Entered)
    } else if let Some(path) = flag {
        (vec![path.to_path_buf()], LoginSource::Flag)
    } else if let Some(value) = var(CODEX_AUTH) {
        (login_paths(&value, None), LoginSource::Setting)
    } else {
        (
            arko_ai::CodexAuthFile::default_path().into_iter().collect(),
            LoginSource::Default,
        )
    };
    paths
        .into_iter()
        .map(|path| {
            let state = if path.is_file() {
                match check_login(&path) {
                    Ok(()) => LoginState::Ready,
                    Err(why) => LoginState::Invalid(why),
                }
            } else {
                LoginState::Missing
            };
            ChatGptLogin {
                path: Some(path),
                source,
                state,
            }
        })
        .collect()
}

/// Uses the Codex auth.json at `path` as the ChatGPT login from now on,
/// for the sessions of Arko's folder `root`, in this process and (saved as
/// ARKO_CODEX_AUTH in its .env) in later ones. Replies the login when the
/// file holds one, else why not.
pub fn set_chatgpt_login(root: &Path, path: &Path) -> Result<ChatGptLogin, String> {
    let env = Env::for_folder_in(None, true, Some(root));
    let path = checked_login(path)?;
    save_logins(&env, &[path])?;
    announce_logins(root);
    Ok(crate::context::with_env(env, || chatgpt_login(None)))
}

/// The absolute path of the Codex auth.json at `path`, when it holds a
/// ChatGPT login, else why not.
fn checked_login(path: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    check_login(&path).map_err(|why| format!("Could not use {} ({why}).", path.display()))?;
    Ok(path)
}

/// Saves `paths` as the ChatGPT logins of `env`'s Arko folder, in this
/// process and (as ARKO_CODEX_AUTH in its .env) in later ones.
fn save_logins(env: &Env, paths: &[PathBuf]) -> Result<(), String> {
    let text = join_logins(paths)?;
    save(&env.arko_dotenv()?, CODEX_AUTH, &text)?;
    env.enter(CODEX_AUTH, text);
    Ok(())
}

/// Adds the Codex auth.json at `path` after the ChatGPT logins the
/// sessions of Arko's folder `root` use, and saves the list as
/// [`set_chatgpt_login`] saves one. While none is set, the list is Codex's
/// own login (when its file holds one) and then `path`. Fails when the file
/// holds no ChatGPT login, is in the list already, or holds an account
/// another login already is. Replies the logins now.
pub fn add_chatgpt_login(root: &Path, path: &Path) -> Result<Vec<ChatGptLogin>, String> {
    let env = Env::for_folder_in(None, true, Some(root));
    let logins = with_logins(&env, None, |paths, _| add_login_to(paths, path))?;
    announce_logins(root);
    Ok(logins)
}

/// Removes the Codex auth.json at `path` from the ChatGPT logins the
/// sessions of Arko's folder `root` use. The last one removed goes back to
/// Codex's own login, as [`clear_chatgpt_login`] does. Replies the logins
/// now.
pub fn remove_chatgpt_login(root: &Path, path: &Path) -> Result<Vec<ChatGptLogin>, String> {
    let env = Env::for_folder_in(None, true, Some(root));
    let logins = with_logins(&env, None, |paths, codex_own| {
        remove_login_from(paths, path, codex_own)
    })?;
    announce_logins(root);
    Ok(logins)
}

/// Puts the Codex auth.json at `path` first among the ChatGPT logins the
/// sessions of Arko's folder `root` use, which are tried in order. Replies
/// the logins now.
pub fn first_chatgpt_login(root: &Path, path: &Path) -> Result<Vec<ChatGptLogin>, String> {
    let env = Env::for_folder_in(None, true, Some(root));
    let logins = with_logins(&env, None, |paths, _| first_login_of(paths, path))?;
    announce_logins(root);
    Ok(logins)
}

/// Adds `path` to `paths`, as [`add_chatgpt_login`] says.
pub(crate) fn add_login_to(paths: &mut Vec<PathBuf>, path: &Path) -> Result<(), String> {
    let path = checked_login(path)?;
    if paths.contains(&path) {
        return Err(format!("{} is in the list already.", path.display()));
    }
    let account = login_account(&path);
    if let Some(same) = paths
        .iter()
        .find(|other| account.is_some() && login_account(other) == account)
    {
        return Err(format!(
            "{} is the same ChatGPT account as {}.",
            path.display(),
            same.display()
        ));
    }
    // (It has to be savable with the others.)
    join_logins(&[paths.as_slice(), std::slice::from_ref(&path)].concat())?;
    paths.push(path);
    Ok(())
}

/// Removes `path` from `paths`, as [`remove_chatgpt_login`] says.
/// `codex_own`: `paths` is Codex's own login, which no setting holds.
pub(crate) fn remove_login_from(
    paths: &mut Vec<PathBuf>,
    path: &Path,
    codex_own: bool,
) -> Result<(), String> {
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let at = paths
        .iter()
        .position(|login| *login == path)
        .ok_or_else(|| format!("{} isn't in the list.", path.display()))?;
    if codex_own {
        return Err(format!(
            "{} is Codex's own login, which Arko uses while no other is set.",
            path.display()
        ));
    }
    paths.remove(at);
    Ok(())
}

/// Puts `path` first in `paths`, as [`first_chatgpt_login`] says.
pub(crate) fn first_login_of(paths: &mut Vec<PathBuf>, path: &Path) -> Result<(), String> {
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let at = paths
        .iter()
        .position(|login| *login == path)
        .ok_or_else(|| format!("{} isn't in the list.", path.display()))?;
    let login = paths.remove(at);
    paths.insert(0, login);
    Ok(())
}

/// Changes the ChatGPT logins in `env` with `change`, which gets the list
/// in use (Codex's own login while none is set, when its file holds a login,
/// with whether that is so; `flag` is a session's `--codex-auth`). It saves
/// the new list in Arko's folder, and replies the logins now. The open
/// sessions are told ([`announce_logins`]) by the caller, once it has done
/// what it does of its own.
pub(crate) fn with_logins(
    env: &Env,
    flag: Option<&Path>,
    change: impl FnOnce(&mut Vec<PathBuf>, bool) -> Result<(), String>,
) -> Result<Vec<ChatGptLogin>, String> {
    crate::context::with_env(env.clone(), || {
        let logins: Vec<ChatGptLogin> = chatgpt_logins(flag)
            .into_iter()
            .filter(|login| {
                login.source != LoginSource::Default || login.state == LoginState::Ready
            })
            .collect();
        let codex_own = logins
            .first()
            .is_some_and(|login| login.source == LoginSource::Default);
        let mut paths: Vec<PathBuf> = logins.into_iter().filter_map(|login| login.path).collect();
        change(&mut paths, codex_own)?;
        if paths.is_empty() {
            unset(&env.arko_dotenv()?, CODEX_AUTH)?;
            env.forget(CODEX_AUTH);
        } else {
            save_logins(env, &paths)?;
        }
        Ok(chatgpt_logins(flag))
    })
}

/// The ChatGPT account the Codex auth.json at `path` holds.
fn login_account(path: &Path) -> Option<String> {
    let file = arko_ai::CodexAuthFile::load(path).ok()?;
    Some(file.credentials().ok()?.account_id)
}

/// Goes back to Codex's own ChatGPT login (the auth.json `codex login`
/// writes) for the sessions of Arko's folder `root`. The login chosen
/// with [`set_chatgpt_login`] is removed, from this process and from the
/// folder's `.env` (ARKO_CODEX_AUTH), under the lock every change to that
/// file takes. ARKO_CODEX_AUTH set elsewhere, in the environment or
/// another `.env` Arko reads, still comes first. Replies the login used
/// from now on.
pub fn clear_chatgpt_login(root: &Path) -> Result<ChatGptLogin, String> {
    let env = Env::for_folder_in(None, true, Some(root));
    unset(&env.arko_dotenv()?, CODEX_AUTH)?;
    env.forget(CODEX_AUTH);
    announce_logins(root);
    Ok(chatgpt_login_in(root))
}

/// The ChatGPT logins the sessions of Arko's folder `root` use, in order
/// (see [`chatgpt_login_in`]).
pub fn chatgpt_logins_in(root: &Path) -> Vec<ChatGptLogin> {
    crate::context::with_env(Env::for_folder_in(None, true, Some(root)), || {
        chatgpt_logins(None)
    })
}

fn login_sender() -> &'static tokio::sync::broadcast::Sender<PathBuf> {
    static SENDER: std::sync::OnceLock<tokio::sync::broadcast::Sender<PathBuf>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| tokio::sync::broadcast::channel(16).0)
}

/// The Arko folders whose ChatGPT logins changed in this process from now
/// on. The open sessions of a folder use its logins as they are then.
pub fn login_changes() -> tokio::sync::broadcast::Receiver<PathBuf> {
    login_sender().subscribe()
}

/// Tells the open sessions of Arko's folder `root` that its ChatGPT logins
/// changed.
pub(crate) fn announce_logins(root: &Path) {
    let _ = login_sender().send(root.to_path_buf());
}

/// The ChatGPT login the sessions of Arko's folder `root` use, read from
/// the environment and the `.env` files as they are now (as
/// [`chatgpt_login`] reads it without a `--codex-auth` file). What this
/// process reads otherwise ([`use_env`]) stays as it is.
pub fn chatgpt_login_in(root: &Path) -> ChatGptLogin {
    crate::context::with_env(Env::for_folder_in(None, true, Some(root)), || {
        chatgpt_login(None)
    })
}

/// Credentials a session can't start without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// No model was chosen, and there is no credential for any provider.
    Any,
    /// The model needs an Anthropic API key.
    AnthropicKey { model: String },
    /// The model needs a ChatGPT login, and `problem` says what failed.
    ChatGptLogin { model: String, problem: String },
    /// The model needs an OpenAI API key, since the session reaches OpenAI
    /// models with one.
    OpenAiKey { model: String },
    /// The model needs a Google Gemini API key.
    GeminiKey { model: String },
    /// The model needs an OpenRouter API key.
    OpenRouterKey { model: String },
    /// The model runs in Claude Code, which isn't installed or signed in.
    ClaudeCode {
        model: String,
        problem: claude_code::Problem,
    },
}

impl Need {
    /// What is missing, as an error where the setup screen can't be shown.
    pub fn message(&self) -> String {
        let setup = "run `arko --setup` in a terminal to enter it";
        let key = |model: &str, name: &str| {
            format!("{model} needs {name}. Set it in the environment or a .env file, or {setup}")
        };
        match self {
            Need::Any => format!(
                "there are no credentials. Set ANTHROPIC_API_KEY (Claude models), OPENAI_API_KEY (OpenAI models), GEMINI_API_KEY (Gemini models) or OPENROUTER_API_KEY (any model on OpenRouter) in the environment or a .env file, sign in to ChatGPT with `codex login`, install Claude Code for Claude models on your Claude plan, or {setup}"
            ),
            Need::AnthropicKey { model } => key(model, ANTHROPIC_KEY),
            Need::OpenAiKey { model } => key(model, OPENAI_KEY),
            Need::GeminiKey { model } => key(model, "GEMINI_API_KEY (or GOOGLE_API_KEY)"),
            Need::OpenRouterKey { model } => key(model, OPENROUTER_KEY),
            Need::ChatGptLogin { model, problem } => format!(
                "{model} needs a ChatGPT login (`codex login`). {}. Or {setup}. With --openai-auth api-key, a new session uses OPENAI_API_KEY instead",
                crate::ui::sentence(problem)
            ),
            Need::ClaudeCode {
                model,
                problem: claude_code::Problem::Missing,
            } => format!(
                "{model} runs in Claude Code, and there is no `claude` on the PATH. {}",
                crate::ui::sentence(claude_code::INSTALL)
            ),
            Need::ClaudeCode {
                model,
                problem: claude_code::Problem::SignedOut(path),
            } => format!(
                "{model} runs in Claude Code ({}), which isn't signed in. Sign in with `claude auth login`",
                path.display()
            ),
            Need::ClaudeCode {
                problem: claude_code::Problem::Outdated { version },
                ..
            } => claude_code::outdated_text(version),
        }
    }
}

/// Whether the setup screen can be shown: standard input and output are a
/// terminal.
#[cfg(feature = "tui")]
pub fn can_ask() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// An API key Arko takes on the setup screen and in the keys menu.
#[derive(Debug, PartialEq, Eq)]
pub struct Key {
    /// The provider, as the keys menu names it ("Anthropic").
    pub provider: &'static str,
    /// The provider as `/keys add` takes it ("anthropic").
    pub id: &'static str,
    pub name: &'static str,
    /// What it is, with its article ("an Anthropic API key").
    pub what: &'static str,
    /// Where to get one.
    pub from: &'static str,
    /// How its keys start.
    pub prefix: &'static str,
    /// What it is for, as a sentence.
    pub purpose: &'static str,
}

pub const ANTHROPIC: Key = Key {
    provider: "Anthropic",
    id: "anthropic",
    name: ANTHROPIC_KEY,
    what: "an Anthropic API key",
    from: "console.anthropic.com",
    prefix: "sk-ant-",
    purpose: "For Claude models on the Anthropic API, billed per token.",
};
pub const OPENAI: Key = Key {
    provider: "OpenAI",
    id: "openai",
    name: OPENAI_KEY,
    what: "an OpenAI API key",
    from: "platform.openai.com/api-keys",
    prefix: "sk-",
    purpose: "For OpenAI models, billed per token.",
};
pub const GEMINI: Key = Key {
    provider: "Google Gemini",
    id: "gemini",
    name: GEMINI_KEY,
    what: "a Gemini API key",
    from: "aistudio.google.com/apikey",
    prefix: "AIza",
    purpose: "For Google's Gemini models, billed per token.",
};
pub const OPENROUTER: Key = Key {
    provider: "OpenRouter",
    id: "openrouter",
    name: OPENROUTER_KEY,
    what: "an OpenRouter API key",
    from: "openrouter.ai/keys",
    prefix: "sk-or-",
    purpose: "For the models OpenRouter serves (openrouter:<slug>), billed per token.",
};
pub const VOYAGE: Key = Key {
    provider: "Voyage",
    id: "voyage",
    name: VOYAGE_KEY,
    what: "a Voyage API key",
    from: "dashboard.voyageai.com",
    prefix: "pa-",
    purpose: "The search indexes embed with it, to search by meaning.",
};
/// The API keys, in the keys menu's order.
pub const KEYS: [&Key; 5] = [&ANTHROPIC, &OPENAI, &GEMINI, &OPENROUTER, &VOYAGE];

/// The key `/keys add` and `/keys remove` name: `anthropic`, `openai`,
/// `gemini` (or `google`), `openrouter` or `voyage`.
pub fn key_named(word: &str) -> Option<&'static Key> {
    let word = word.trim().to_ascii_lowercase();
    let word = if word == "google" { "gemini" } else { &word };
    KEYS.into_iter().find(|key| key.id == word)
}

/// Where the key in use comes from, as the keys menu says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Arko's own `.env`, at this path, where the setup screen and the keys
    /// menu save keys.
    Arko(PathBuf),
    /// Arko's environment.
    Environment,
    /// Another `.env` file.
    DotEnv(PathBuf),
}

/// An API key as the keys menu shows it, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyState {
    pub key: &'static Key,
    /// The key in use, masked as its prefix and its last four characters
    /// (`sk-…wxyz`), or `None` when none is set.
    pub masked: Option<String>,
    /// The variable it is read from, such as GOOGLE_API_KEY for a Gemini key
    /// under its older name.
    pub variable: &'static str,
    pub source: Option<KeySource>,
}

impl KeyState {
    /// Whether the keys menu can remove it, which it can when Arko saved it.
    #[cfg(feature = "tui")]
    pub fn removable(&self) -> bool {
        matches!(self.source, Some(KeySource::Arko(_)))
    }
}

/// Every API key's state, in the keys menu's order.
pub fn key_states() -> Vec<KeyState> {
    KEYS.into_iter().map(key_state).collect()
}

/// The API keys for the sessions of Arko's folder `root`, as
/// [`key_states`] gives them, read from the environment and the `.env`
/// files as they are now. What this process reads otherwise
/// ([`use_env`]) stays as it is.
pub fn key_states_in(root: &Path) -> Vec<KeyState> {
    crate::context::with_env(Env::for_folder_in(None, true, Some(root)), key_states)
}

/// `key`'s state now.
pub fn key_state(key: &'static Key) -> KeyState {
    let arko = arko_dotenv().ok();
    match in_use(key) {
        None => KeyState {
            key,
            masked: None,
            variable: key.name,
            source: None,
        },
        Some((variable, value, source)) => KeyState {
            key,
            masked: Some(masked(&value, key.prefix)),
            variable,
            source: Some(match source {
                Source::Entered => KeySource::Arko(arko.unwrap_or_default()),
                Source::Environment => KeySource::Environment,
                Source::DotEnv(path) if arko.as_ref() == Some(&path) => KeySource::Arko(path),
                Source::DotEnv(path) => KeySource::DotEnv(path),
            }),
        },
    }
}

/// The value of `key` in use (for the session's model client and search
/// indexes), if one is set.
pub fn key_value(key: &Key) -> Option<String> {
    in_use(key).map(|(_, value, _)| value)
}

/// The key in use for `key`: the variable it is read as, its value and
/// where it comes from. A Gemini key is read as GOOGLE_API_KEY first, as
/// arko reads it (unless one was entered in this run).
fn in_use(key: &Key) -> Option<(&'static str, String, Source)> {
    if key.name == GEMINI_KEY
        && let Some((value, source)) = lookup(GOOGLE_KEY)
    {
        return Some((GOOGLE_KEY, value, source));
    }
    lookup(key.name).map(|(value, source)| (key.name, value, source))
}

/// `value` as Arko shows a key: its provider's `prefix` (when it starts with
/// it) and its last 4 characters, `sk-ant-…a1b2`. A short key shows no end.
pub fn masked(value: &str, prefix: &str) -> String {
    let start = if value.starts_with(prefix) {
        prefix
    } else {
        ""
    };
    let count = value.chars().count();
    if count >= start.chars().count() + 12 {
        let end: String = value.chars().skip(count - 4).collect();
        format!("{start}…{end}")
    } else {
        format!("{start}…")
    }
}

/// Why `value` (pasted as `key`) can't be it: it has spaces, or it is
/// another provider's key.
pub fn check_pasted(value: &str, key: &Key) -> Result<(), String> {
    if value.chars().any(char::is_whitespace) {
        return Err("An API key has no spaces in it. Paste it again.".into());
    }
    if let Some(other) = mistaken_for(value, key) {
        return Err(format!(
            "That looks like {} (it starts with {}), not {}. Paste the {}.",
            other.what,
            other.prefix,
            key.what,
            without_article(key.what)
        ));
    }
    Ok(())
}

/// What saving a key or removing one says: what was done, and a note on
/// what comes before it in later runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub said: String,
    pub note: Option<String>,
}

/// Saves `value` as `key` in Arko's `.env` and uses it in this run, as the
/// setup screen does.
pub fn save_key(key: &'static Key, value: &str) -> Result<Saved, String> {
    let env = current();
    let dotenv = env.arko_dotenv()?;
    save(&dotenv, key.name, value)?;
    env.enter(key.name, value.to_string());
    Ok(saved_text(&dotenv, key.name))
}

/// Removes `key` from Arko's `.env` and from this run, when Arko saved the
/// key in use ([`KeyState::removable`]). Returns its state after: the key
/// another source sets, if one does.
pub fn remove_key(key: &'static Key) -> Result<KeyState, String> {
    let state = key_state(key);
    let Some(KeySource::Arko(path)) = &state.source else {
        return Err(match &state.source {
            None => format!("No {} is set.", without_article(key.what)),
            Some(source) => format!(
                "Arko didn't save this key. It comes from {}, so change it there.",
                source_text(source, state.variable)
            ),
        });
    };
    unset(path, state.variable)?;
    current().forget(state.variable);
    Ok(key_state(key))
}

/// Where a key comes from, as a sentence names it: "ANTHROPIC_API_KEY in
/// Arko's environment".
pub fn source_text(source: &KeySource, variable: &str) -> String {
    match source {
        KeySource::Arko(path) => format!("{variable} in Arko's .env ({})", path.display()),
        KeySource::Environment => format!("{variable} in Arko's environment"),
        KeySource::DotEnv(path) => format!("{variable} in {}", path.display()),
    }
}

/// What the setup screen's menu offers.
#[cfg(feature = "tui")]
#[derive(Clone, Copy)]
enum Choice {
    Key(&'static Key),
    ChatGptLogin,
    ClaudeCode,
}

/// The menu: what each choice is for.
#[cfg(feature = "tui")]
const MENU: [(Choice, &str); 7] = [
    (
        Choice::Key(&ANTHROPIC),
        "Paste an Anthropic API key, for Claude models (billed per token)",
    ),
    (
        Choice::ChatGptLogin,
        "Give the path to a Codex auth.json with a ChatGPT login, for OpenAI models on your ChatGPT plan",
    ),
    (
        Choice::Key(&OPENAI),
        "Paste an OpenAI API key, for OpenAI models (billed per token)",
    ),
    (
        Choice::Key(&GEMINI),
        "Paste a Gemini API key, for Google's Gemini models (billed per token)",
    ),
    (
        Choice::Key(&OPENROUTER),
        "Paste an OpenRouter API key, for the models OpenRouter serves (-m openrouter:<slug>)",
    ),
    (
        Choice::ClaudeCode,
        "Use Claude Code, for Claude models on your Claude plan (-m cc:opus)",
    ),
    (
        Choice::Key(&VOYAGE),
        "Paste a Voyage API key, for semantic search in the search indexes",
    ),
];

/// The setup screen: asks for what `need` names (anything with `None` or
/// `Need::Any`), saves it in Arko's `.env` and uses it in this run. An error
/// when the user quits.
#[cfg(feature = "tui")]
pub fn setup(need: Option<&Need>) -> Result<(), String> {
    let out = Output::new(None, false);
    let dotenv = arko_dotenv()?;
    out.line("");
    out.line(&out.bold("Arko setup"));
    match need {
        None => {
            out.line(&out.dim("Credentials now"));
            let key = |name: &str| match lookup(name) {
                None => "not set".to_string(),
                Some((_, source)) => format!("set ({})", describe(&source)),
            };
            out.line(&format!("  Anthropic API key   {}", key(ANTHROPIC_KEY)));
            let login = codex_auth(None)
                .or_else(|| CodexAuthFile::default_path().filter(|path| path.is_file()))
                .map_or("none found".to_string(), |path| path.display().to_string());
            out.line(&format!("  ChatGPT login       {login}"));
            out.line(&format!("  OpenAI API key      {}", key(OPENAI_KEY)));
            let gemini = match lookup(GOOGLE_KEY) {
                Some((_, source)) => format!("set as GOOGLE_API_KEY ({})", describe(&source)),
                None => key(GEMINI_KEY),
            };
            out.line(&format!("  Gemini API key      {gemini}"));
            out.line(&format!("  OpenRouter API key  {}", key(OPENROUTER_KEY)));
            let claude = match claude_code::check() {
                Ok(ready) => format!("{}, {}", ready.path.display(), ready.describe()),
                Err(claude_code::Problem::Missing) => "not installed".to_string(),
                Err(claude_code::Problem::SignedOut(path)) => {
                    format!("not signed in ({})", path.display())
                }
                Err(claude_code::Problem::Outdated { version }) => {
                    format!("{version}, too old (update it with `claude update`)")
                }
            };
            out.line(&format!("  Claude Code         {claude}"));
            let voyage = match lookup(VOYAGE_KEY) {
                None => "not set (search indexes are keyword-only)".to_string(),
                Some((_, source)) => format!("set ({})", describe(&source)),
            };
            out.line(&format!("  Voyage API key      {voyage}"));
        }
        Some(Need::Any) => out.line(
            "No credentials found. No API key is set (in the environment or a .env file), there is no ChatGPT login, and Claude Code isn't installed.",
        ),
        Some(Need::AnthropicKey { model }) => out.line(&format!(
            "{model} needs an Anthropic API key, and ANTHROPIC_API_KEY isn't set in the environment or a .env file."
        )),
        Some(Need::OpenAiKey { model }) => out.line(&format!(
            "{model} needs an OpenAI API key (this session reaches OpenAI models with one), and OPENAI_API_KEY isn't set in the environment or a .env file."
        )),
        Some(Need::GeminiKey { model }) => out.line(&format!(
            "{model} needs a Gemini API key, and neither GEMINI_API_KEY nor GOOGLE_API_KEY is set in the environment or a .env file."
        )),
        Some(Need::OpenRouterKey { model }) => out.line(&format!(
            "{model} needs an OpenRouter API key, and OPENROUTER_API_KEY isn't set in the environment or a .env file."
        )),
        Some(Need::ChatGptLogin { model, problem }) => {
            out.line(&format!(
                "{model} needs a ChatGPT login. {}",
                crate::ui::sentence(problem)
            ));
            out.line(&out.dim(
                "(A new session started with arko --openai-auth api-key reaches OpenAI models with an API key instead.)",
            ));
        }
        Some(Need::ClaudeCode { model, problem }) => out.line(&match problem {
            claude_code::Problem::Missing => format!(
                "{model} runs in Claude Code, and there is no `claude` on the PATH."
            ),
            claude_code::Problem::SignedOut(path) => format!(
                "{model} runs in Claude Code ({}), which isn't signed in.",
                path.display()
            ),
            claude_code::Problem::Outdated { version } => claude_code::outdated_text(version),
        }),
    }
    out.line("");
    let mut editor = Prompt::editor()?;
    let choice = match need {
        Some(Need::AnthropicKey { .. }) => Choice::Key(&ANTHROPIC),
        Some(Need::OpenAiKey { .. }) => Choice::Key(&OPENAI),
        Some(Need::GeminiKey { .. }) => Choice::Key(&GEMINI),
        Some(Need::OpenRouterKey { .. }) => Choice::Key(&OPENROUTER),
        Some(Need::ChatGptLogin { .. }) => Choice::ChatGptLogin,
        Some(Need::ClaudeCode { problem, .. }) => {
            out.line(&out.dim(
                "(For another provider instead, quit (q) and start Arko with --setup, or with -m and another model.)",
            ));
            return ask_claude_code(&out, &mut editor, Some(problem.clone()));
        }
        None | Some(Need::Any) => {
            // The Voyage key is no model's credential: offered on its own.
            let offered = if need.is_none() {
                MENU.len()
            } else {
                MENU.len() - 1
            };
            for (number, (_, text)) in MENU.iter().take(offered).enumerate() {
                out.line(&format!("  {}  {text}", number + 1));
            }
            out.line("  q  Quit");
            let prompt = format!("Which one (1-{offered}, or q to quit)? ");
            loop {
                let line = read(&mut editor, &prompt, "", false)?;
                match line.trim() {
                    "" | "q" | "Q" => return Err(cancelled()),
                    number => {
                        if let Some(choice) = number
                            .parse::<usize>()
                            .ok()
                            .filter(|number| (1..=offered).contains(number))
                            .map(|number| MENU[number - 1].0)
                        {
                            break choice;
                        }
                    }
                }
            }
        }
    };
    match choice {
        Choice::Key(key) => ask_key(&out, &mut editor, &dotenv, key),
        Choice::ChatGptLogin => ask_login(&out, &mut editor, &dotenv),
        Choice::ClaudeCode => ask_claude_code(&out, &mut editor, None),
    }
}

fn describe(source: &Source) -> String {
    match source {
        Source::Entered => "entered now".into(),
        Source::Environment => "from the environment".into(),
        Source::DotEnv(path) => format!("from {}", path.display()),
    }
}

#[cfg(feature = "tui")]
fn cancelled() -> String {
    "setup cancelled".into()
}

/// Asks for `key`, saves it in Arko's `.env` and uses it in this run.
#[cfg(feature = "tui")]
fn ask_key(out: &Output, editor: &mut Editor, dotenv: &Path, key: &Key) -> Result<(), String> {
    out.line(&out.dim(&format!(
        "Paste {} (from {}). The input is hidden, and an empty line quits.",
        key.what, key.from
    )));
    let value = ask_value(out, editor, key)?;
    if let Some(note) = prefix_note(&value, key) {
        out.line(&out.warn(&note));
    }
    save(dotenv, key.name, &value)?;
    enter(key.name, value);
    saved(out, dotenv, key.name);
    Ok(())
}

/// Reads `key` with the input hidden until it looks like one: an error when
/// the line is empty or the user presses Ctrl-C.
#[cfg(feature = "tui")]
fn ask_value(out: &Output, editor: &mut Editor, key: &Key) -> Result<String, String> {
    let prompt = format!("{}: ", capitalized(key.what));
    loop {
        let value = read(editor, &prompt, "", true)?;
        let value = value.trim();
        if value.is_empty() {
            return Err(cancelled());
        }
        match check_pasted(value, key) {
            Ok(()) => return Ok(value.to_string()),
            Err(problem) => out.line(&out.warn(&problem)),
        }
    }
}

/// The note on a key that doesn't start as `key`'s do.
pub fn prefix_note(value: &str, key: &Key) -> Option<String> {
    (!value.starts_with(key.prefix)).then(|| {
        format!(
            "{}s start with {}, and this one doesn't, so requests may be refused.",
            capitalized(without_article(key.what)),
            key.prefix
        )
    })
}

/// The line prompt's `/keys add`: reads `key` with the input hidden, as the
/// setup screen does. `None` when the user leaves it empty or presses
/// Ctrl-C.
#[cfg(feature = "tui")]
pub fn read_key(out: &Output, key: &Key) -> Result<Option<String>, String> {
    out.line(&out.dim(&format!(
        "Paste {} (from {}). The input is hidden, and an empty line goes back.",
        key.what, key.from
    )));
    let mut editor = Prompt::editor()?;
    match ask_value(out, &mut editor, key) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error == cancelled() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The other provider's key `value` looks like, when it has that key's
/// longer prefix: an Anthropic key pasted where an OpenAI one goes.
fn mistaken_for(value: &str, key: &Key) -> Option<&'static Key> {
    KEYS.into_iter()
        .filter(|other| other.name != key.name && value.starts_with(other.prefix))
        .filter(|other| other.prefix.len() > key.prefix.len() || !value.starts_with(key.prefix))
        .max_by_key(|other| other.prefix.len())
}

/// "an OpenAI API key" without its article: "OpenAI API key".
fn without_article(what: &str) -> &str {
    what.split_once(' ').map_or(what, |(_, rest)| rest)
}

/// "OpenAI API key" as a prompt starts it: with a capital, without its
/// article.
fn capitalized(what: &str) -> String {
    let what = without_article(what);
    let mut chars = what.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Checks Claude Code until it is installed and signed in, while the user
/// sets it up in another terminal. `known` is what a check before found,
/// which the screen has said already.
#[cfg(feature = "tui")]
fn ask_claude_code(
    out: &Output,
    editor: &mut Editor,
    known: Option<claude_code::Problem>,
) -> Result<(), String> {
    let (mut problem, mut said) = match known {
        Some(problem) => (problem, true),
        None => match claude_code::check() {
            Ok(ready) => return claude_code_ready(out, &ready),
            Err(problem) => (problem, false),
        },
    };
    let mut instructed = None;
    loop {
        // What is wrong and what to do, once for each problem.
        if instructed.as_ref() != Some(&problem) {
            match &problem {
                claude_code::Problem::Missing => {
                    if !said {
                        out.line("Claude Code isn't installed (there is no `claude` on the PATH).");
                    }
                    out.line(&format!("In another terminal, {}.", claude_code::INSTALL));
                    out.line(&out.dim(
                        "If the installer adds a folder to your PATH, quit (q) and start Arko again from a new terminal.",
                    ));
                }
                claude_code::Problem::SignedOut(path) => {
                    if !said {
                        out.line(&format!(
                            "Claude Code ({}) isn't signed in.",
                            path.display()
                        ));
                    }
                    out.line(
                        "In another terminal, sign in with `claude auth login`, with the Claude account whose plan Arko should use.",
                    );
                }
                claude_code::Problem::Outdated { version } => {
                    if !said {
                        out.line(&claude_code::outdated_text(version));
                    }
                    out.line("In another terminal, update it with `claude update`.");
                }
            }
            instructed = Some(problem.clone());
            said = false;
        }
        let line = read(
            editor,
            "Press Enter to check again, or q to quit. ",
            "",
            false,
        )?;
        if matches!(line.trim(), "q" | "Q") {
            return Err(cancelled());
        }
        match claude_code::check() {
            Ok(ready) => return claude_code_ready(out, &ready),
            Err(still) if still == problem => out.line(&out.warn(match still {
                claude_code::Problem::Missing => "Not yet. There is still no `claude` on the PATH.",
                claude_code::Problem::SignedOut(_) => "Not yet. Claude Code still isn't signed in.",
                claude_code::Problem::Outdated { .. } => "Not yet. Claude Code is still too old.",
            })),
            Err(other) => problem = other,
        }
    }
}

#[cfg(feature = "tui")]
fn claude_code_ready(out: &Output, ready: &claude_code::Ready) -> Result<(), String> {
    out.line(&format!(
        "Claude Code ({}) is ready and {}. Arko runs Claude models in it with -m cc:opus or -m cc:fable.",
        ready.path.display(),
        ready.describe()
    ));
    if let Some(warning) = ready.warning() {
        out.line(&out.warn(&warning));
    }
    out.line("");
    Ok(())
}

#[cfg(feature = "tui")]
fn ask_login(out: &Output, editor: &mut Editor, dotenv: &Path) -> Result<(), String> {
    out.line(&out.dim(
        "Give the path to a Codex auth.json holding a ChatGPT login (`codex login` writes ~/.codex/auth.json). Tab completes, and an empty line quits.",
    ));
    let initial = CodexAuthFile::default_path()
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let path = loop {
        let line = read(editor, "auth.json path › ", &initial, false)?;
        let line = line.trim();
        if line.is_empty() {
            return Err(cancelled());
        }
        let path = std::path::absolute(expand_home(line)).unwrap_or_else(|_| line.into());
        match check_login(&path) {
            Ok(()) => break path,
            Err(error) => out.line(&out.warn(&format!(
                "Could not use {} ({error}). Try another path.",
                path.display()
            ))),
        }
    };
    let path = path.to_string_lossy().into_owned();
    save(dotenv, CODEX_AUTH, &path)?;
    enter(CODEX_AUTH, path);
    saved(out, dotenv, CODEX_AUTH);
    Ok(())
}

/// Says where the setting was saved, and what comes before it in later runs.
#[cfg(feature = "tui")]
fn saved(out: &Output, dotenv: &Path, name: &str) {
    let saved = saved_text(dotenv, name);
    out.line(&saved.said);
    if let Some(note) = &saved.note {
        out.line(&out.warn(note));
    }
    out.line("");
}

/// Where the setting `name` was saved, and what comes before it in later
/// runs: the environment or another `.env` (for a Gemini key, under its
/// older name too).
fn saved_text(dotenv: &Path, name: &str) -> Saved {
    let names: &[&str] = if name == GEMINI_KEY {
        &[GOOGLE_KEY, GEMINI_KEY]
    } else {
        &[name]
    };
    let before = names.iter().find_map(|name| {
        if std::env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
            Some(format!("{name} in your environment"))
        } else {
            match dotenv_get(name) {
                Some(path) if path != dotenv => {
                    Some(format!("{} (it sets {name} too)", path.display()))
                }
                _ => None,
            }
        }
    });
    Saved {
        said: format!(
            "Saved as {name} in {} (readable only by you).",
            dotenv.display()
        ),
        note: before.map(|before| {
            format!("{before} comes first in later runs, but this run uses what you entered.")
        }),
    }
}

fn dotenv_get(name: &str) -> Option<PathBuf> {
    current().file_entry(name).map(|(_, path)| path)
}

/// Sets `name` to `value` in the `.env` file at `path`, keeping its other
/// lines; the file is created readable only by the user.
fn save(path: &Path, name: &str, value: &str) -> Result<(), String> {
    let line = format!("{name}={}", quote(value));
    change_dotenv(path, |text| {
        let mut lines = Vec::new();
        let mut replaced = false;
        for existing in text.unwrap_or_default().lines() {
            if sets(existing, name) {
                if !replaced {
                    lines.push(line.clone());
                    replaced = true;
                }
            } else {
                lines.push(existing.to_string());
            }
        }
        if !replaced {
            lines.push(line);
        }
        let mut text = lines.join("\n");
        text.push('\n');
        Some(text)
    })
    .map_err(|error| format!("could not save {} ({error})", path.display()))
}

/// Removes the lines that set `name` from the `.env` file at `path`,
/// keeping its other lines. A file without one is left alone.
fn unset(path: &Path, name: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    change_dotenv(path, |text| {
        let text = text?;
        if !text.lines().any(|line| sets(line, name)) {
            return None;
        }
        let mut kept: String = text
            .lines()
            .filter(|line| !sets(line, name))
            .collect::<Vec<_>>()
            .join("\n");
        if !kept.is_empty() {
            kept.push('\n');
        }
        Some(kept)
    })
    .map_err(|error| format!("could not change {} ({error})", path.display()))
}

/// Changes the `.env` file at `path`: `change` gets its text (`None`
/// when there is no file) and gives the new one, or `None` to leave it
/// alone. It runs under a lock on the file `.env.lock` beside it, held
/// from the reading to the writing, so programs that change the file at
/// once (an app and Arko in a terminal, or two sessions) each read what
/// the other wrote, and none loses a setting.
fn change_dotenv(
    path: &Path,
    change: impl FnOnce(Option<String>) -> Option<String>,
) -> std::io::Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let lock = options.open(path.with_file_name(format!("{name}.lock")))?;
    // (Released when `lock` is dropped, at the end.)
    lock.lock()?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    match change(text) {
        Some(text) => write_private(path, &text),
        None => Ok(()),
    }
}

/// Writes `text` to `path` through a temporary file of its own beside it,
/// readable only by the user, renamed into place.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // A temporary file of the same name left by a process that ended (its
    // ID used again) is passed over for the next name.
    let mut count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (temporary, file) = loop {
        let temporary = path.with_file_name(format!("{name}.{}.{count}.tmp", std::process::id()));
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(error) => return Err(error),
        }
    };
    let written = {
        use std::io::Write;
        let mut file = file;
        file.write_all(text.as_bytes())
    }
    .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Whether a `.env` line sets `name`.
fn sets(line: &str, name: &str) -> bool {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").map_or(line, str::trim_start);
    line.strip_prefix(name)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// A value as a `.env` file writes it: bare when it can be, else quoted.
fn quote(value: &str) -> String {
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:@+,%".contains(c);
    if !value.is_empty() && value.chars().all(bare) {
        value.to_string()
    } else if !value.contains(['\'', '\n']) {
        format!("'{value}'")
    } else {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('\n', "\\n");
        format!("\"{escaped}\"")
    }
}

#[cfg(feature = "tui")]
type Editor = rustyline::Editor<Prompt, rustyline::history::DefaultHistory>;

/// The setup screen's prompts: file names complete, and a key is hidden.
#[cfg(feature = "tui")]
struct Prompt {
    files: FilenameCompleter,
    hidden: bool,
}

#[cfg(feature = "tui")]
impl Prompt {
    fn editor() -> Result<Editor, String> {
        let config = rustyline::Config::builder().auto_add_history(false).build();
        let mut editor = Editor::with_config(config).map_err(|error| error.to_string())?;
        editor.set_helper(Some(Prompt {
            files: FilenameCompleter::new(),
            hidden: false,
        }));
        Ok(editor)
    }
}

/// Reads a line; an error when the user presses Ctrl-C or Ctrl-D.
#[cfg(feature = "tui")]
fn read(editor: &mut Editor, prompt: &str, initial: &str, hidden: bool) -> Result<String, String> {
    if let Some(helper) = editor.helper_mut() {
        helper.hidden = hidden;
    }
    // Masking is done by the highlighter, which colour settings would turn off.
    editor.set_color_mode(if hidden {
        ColorMode::Forced
    } else {
        ColorMode::Enabled
    });
    let line = editor.readline_with_initial(prompt, (initial, ""));
    match line {
        Ok(line) => Ok(line),
        Err(ReadlineError::Interrupted | ReadlineError::Eof) => Err(cancelled()),
        Err(error) => Err(format!("could not read the terminal ({error})")),
    }
}

#[cfg(feature = "tui")]
impl Completer for Prompt {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        context: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if self.hidden {
            Ok((0, Vec::new()))
        } else {
            self.files.complete(line, pos, context)
        }
    }
}

#[cfg(feature = "tui")]
impl Hinter for Prompt {
    type Hint = String;
}

#[cfg(feature = "tui")]
impl Highlighter for Prompt {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.hidden {
            Cow::Owned("•".repeat(line.chars().count()))
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, kind: CmdKind) -> bool {
        self.hidden && kind != CmdKind::MoveCursor
    }
}

#[cfg(feature = "tui")]
impl Validator for Prompt {}

#[cfg(feature = "tui")]
impl Helper for Prompt {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Codex auth.json holding a login for `account`.
    pub(crate) fn write_login(path: &Path, account: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                r#"{{"auth_mode": "chatgpt", "tokens": {{"access_token": "access-{account}", "account_id": "{account}", "id_token": "id-{account}", "refresh_token": "refresh-{account}"}}}}"#
            ),
        )
        .unwrap();
    }

    /// The logins an Arko folder uses, as their files.
    fn login_files(root: &Path) -> Vec<PathBuf> {
        chatgpt_logins_in(root)
            .into_iter()
            .filter_map(|login| login.path)
            .collect()
    }

    /// Several logins are kept in order in ARKO_CODEX_AUTH, and a single
    /// path saved by an earlier Arko reads as one login.
    #[test]
    fn the_chatgpt_logins_are_an_ordered_list_that_one_saved_path_still_is() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let (one, two, again) = (
            temp.path().join("one/auth.json"),
            temp.path().join("two/auth.json"),
            temp.path().join("again/auth.json"),
        );
        write_login(&one, "account-1");
        write_login(&two, "account-2");
        write_login(&again, "account-1");
        let mut changes = login_changes();

        // One path is saved as it was.
        set_chatgpt_login(&root, &one).unwrap();
        let saved = || std::fs::read_to_string(root.join(".env")).unwrap();
        assert_eq!(saved().trim(), format!("ARKO_CODEX_AUTH={}", one.display()));
        assert_eq!(login_files(&root), std::slice::from_ref(&one));
        // A path saved relative to the .env, by an Arko that kept one.
        let older = temp.path().join("older");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(
            older.join(".env"),
            "ARKO_CODEX_AUTH=sub/auth.json
",
        )
        .unwrap();
        let read = read_dotenv(&older.join(".env"));
        assert_eq!(login_paths(&read[0].1, None), [older.join("sub/auth.json")]);

        // More are added after it, and the list reads back in order.
        add_chatgpt_login(&root, &two).unwrap();
        assert_eq!(login_files(&root), [one.clone(), two.clone()]);
        let read = read_dotenv(&root.join(".env"));
        assert_eq!(login_paths(&read[0].1, None), [one.clone(), two.clone()]);
        // A path that is there already, or another file of an account that
        // is, is refused.
        assert!(
            add_chatgpt_login(&root, &two)
                .unwrap_err()
                .contains("already")
        );
        assert!(
            add_chatgpt_login(&root, &again)
                .unwrap_err()
                .contains("same ChatGPT account")
        );
        assert!(
            add_chatgpt_login(&root, &temp.path().join("none/auth.json"))
                .unwrap_err()
                .contains("Could not use")
        );

        // A path that holds the separator can't join the others.
        #[cfg(unix)]
        {
            let odd = temp.path().join("odd:folder/auth.json");
            write_login(&odd, "account-3");
            let refused = add_chatgpt_login(&root, &odd).unwrap_err();
            assert!(
                refused.contains("can't be in a list of logins"),
                "{refused}"
            );
            assert_eq!(login_files(&root), [one.clone(), two.clone()]);
        }

        // One is put first, then removed.
        first_chatgpt_login(&root, &two).unwrap();
        assert_eq!(login_files(&root), [two.clone(), one.clone()]);
        assert!(
            first_chatgpt_login(&root, &again)
                .unwrap_err()
                .contains("isn't in the list")
        );
        remove_chatgpt_login(&root, &two).unwrap();
        assert_eq!(login_files(&root), std::slice::from_ref(&one));
        assert_eq!(saved().trim(), format!("ARKO_CODEX_AUTH={}", one.display()));
        // The last one removed goes back to Codex's own login.
        if std::env::var_os(CODEX_AUTH).is_none() {
            remove_chatgpt_login(&root, &one).unwrap();
            assert!(
                chatgpt_logins_in(&root)
                    .iter()
                    .all(|login| login.source == LoginSource::Default)
            );
            assert!(!saved().contains("ARKO_CODEX_AUTH"));
        }

        // The sessions of the folder heard of every change.
        let mut heard = 0;
        loop {
            match changes.try_recv() {
                Ok(heard_root) => heard += usize::from(heard_root == root),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        assert!(heard >= 1, "{heard}");
    }

    /// The ChatGPT login says where Arko reads it and whether it holds a
    /// login it can use.
    #[test]
    fn the_chatgpt_login_says_where_it_is_and_whether_it_works() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth.json");
        let login = chatgpt_login(Some(&path));
        assert_eq!(
            (login.source, login.state),
            (LoginSource::Flag, LoginState::Missing)
        );
        std::fs::write(&path, "{}").unwrap();
        assert!(matches!(
            chatgpt_login(Some(&path)).state,
            LoginState::Invalid(_)
        ));
        std::fs::write(
            &path,
            r#"{"auth_mode": "chatgpt", "tokens": {"access_token": "access-1", "account_id": "account-1", "id_token": "id-1", "refresh_token": "refresh-1"}}"#,
        )
        .unwrap();
        let login = chatgpt_login(Some(&path));
        assert_eq!(login.path.as_deref(), Some(path.as_path()));
        assert_eq!(login.state, LoginState::Ready);
    }

    /// A session reads the `.env` beside Arko's program, after its folder's,
    /// and keys go to the `.env` of the Arko folder it is given.
    #[test]
    fn a_env_beside_the_program_is_read() {
        if std::env::var_os(crate::remote::HOST_VARIABLE).is_some() {
            return; // (Set in this process: it comes first.)
        }
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("bin").join("arko");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(
            program.with_file_name(".env"),
            format!("{}=beside\n", crate::remote::HOST_VARIABLE),
        )
        .unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let root = temp.path().join("arko");
        let env = Env::for_program(Some(&work), Some(&program), Some(&root));
        assert_eq!(
            env.var(crate::remote::HOST_VARIABLE).as_deref(),
            Some("beside")
        );
        assert_eq!(env.arko_dotenv(), Ok(root.join(".env")));
        std::fs::write(
            work.join(".env"),
            format!("{}=work\n", crate::remote::HOST_VARIABLE),
        )
        .unwrap();
        let env = Env::for_program(Some(&work), Some(&program), Some(&root));
        assert_eq!(
            env.var(crate::remote::HOST_VARIABLE).as_deref(),
            Some("work")
        );
        let without = Env::for_program(Some(&temp.path().join("elsewhere")), None, Some(&root));
        assert_eq!(without.var(crate::remote::HOST_VARIABLE), None);
    }

    /// A session reads the nearest `.env` in its folder or a parent first,
    /// then the one beside Arko's program, then Arko's own, each once: a
    /// setting comes from the first that sets it. Another folder reads its
    /// own.
    #[test]
    fn a_sessions_env_reads_its_folder_then_beside_the_program_then_arkos() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("src").join("deep");
        let other = temp.path().join("other");
        let empty = temp.path().join("empty");
        for folder in [
            &nested,
            &other,
            &empty,
            &temp.path().join("bin"),
            &temp.path().join("arko"),
        ] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let beside = temp.path().join("bin").join(".env");
        let arko = temp.path().join("arko").join(".env");
        std::fs::write(
            project.join(".env"),
            "ARKO_SHELL=powershell\nARKO_CACHE_1H=1\n",
        )
        .unwrap();
        std::fs::write(&beside, "ARKO_SHELL=cmd\nARKO_REMOTE_HOST=beside\n").unwrap();
        std::fs::write(&arko, "ARKO_REMOTE_HOST=arko\nARKO_OPENAI_AUTH=api-key\n").unwrap();
        std::fs::write(other.join(".env"), "ARKO_SHELL=cmd\n").unwrap();

        let paths = dotenv_paths(Some(&nested), Some(beside.clone()), Some(arko.clone()));
        assert_eq!(paths, [project.join(".env"), beside.clone(), arko.clone()]);
        let env = Env::read(&paths, Some(arko.clone()));
        let entry = |env: &Env, name: &str| env.file_entry(name);
        assert_eq!(
            entry(&env, SHELL),
            Some(("powershell".into(), project.join(".env")))
        );
        assert_eq!(
            entry(&env, CACHE_1H),
            Some(("1".into(), project.join(".env")))
        );
        assert_eq!(
            entry(&env, crate::remote::HOST_VARIABLE),
            Some(("beside".into(), beside.clone()))
        );
        assert_eq!(
            entry(&env, OPENAI_AUTH),
            Some(("api-key".into(), arko.clone()))
        );

        let other = Env::read(
            &dotenv_paths(Some(&other), None, Some(arko.clone())),
            Some(arko.clone()),
        );
        assert_eq!(
            entry(&other, SHELL).map(|(value, _)| value).as_deref(),
            Some("cmd")
        );
        assert_eq!(entry(&other, CACHE_1H), None);
        // No `.env` in the folder, and a file named twice is read once.
        assert_eq!(
            dotenv_paths(Some(&empty), Some(arko.clone()), Some(arko.clone())),
            [arko]
        );
    }

    #[test]
    fn a_dotenv_file_gives_only_the_known_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        assert!(read_dotenv(&path).is_empty());
        std::fs::write(
            &path,
            "# keys\nAZURE_API_KEY=sk-azure\nANTHROPIC_API_KEY=\n",
        )
        .unwrap();
        assert!(read_dotenv(&path).is_empty());
        std::fs::write(
            &path,
            "OTHER=1\nexport ANTHROPIC_API_KEY=\"sk-ant-test\" # comment\nARKO_CODEX_AUTH=auth.json\n",
        )
        .unwrap();
        assert_eq!(
            read_dotenv(&path),
            vec![
                (ANTHROPIC_KEY, "sk-ant-test".to_string()),
                (
                    CODEX_AUTH,
                    temp.path().join("auth.json").to_string_lossy().into_owned()
                ),
            ]
        );
    }

    #[test]
    fn saving_replaces_the_setting_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("arko").join(".env");
        save(&path, ANTHROPIC_KEY, "sk-ant-one").unwrap();
        std::fs::write(
            &path,
            "OTHER=1\nexport ANTHROPIC_API_KEY=old\n# note\nANTHROPIC_API_KEY=older\n",
        )
        .unwrap();
        save(&path, ANTHROPIC_KEY, "sk-ant-two").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "OTHER=1\nANTHROPIC_API_KEY=sk-ant-two\n# note\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// Writers at once, each saving and removing its own settings, lose
    /// none of the others': each reads the file under the lock, and each
    /// writes through a temporary file of its own.
    #[test]
    fn writers_at_once_keep_each_others_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("arko").join(".env");
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    for n in 0..25 {
                        save(path, &format!("KEEP_{writer}_{n}"), "kept").unwrap();
                        save(path, &format!("GONE_{writer}"), "for now").unwrap();
                        unset(path, &format!("GONE_{writer}")).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        for writer in 0..8 {
            for n in 0..25 {
                assert!(
                    text.contains(&format!("KEEP_{writer}_{n}=")),
                    "{writer} {n}"
                );
            }
            assert!(!text.contains(&format!("GONE_{writer}=")), "{writer}");
        }
        let mut left: Vec<String> = std::fs::read_dir(temp.path().join("arko"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [".env", ".env.lock"]);
    }

    /// A temporary file left by an earlier process with this one's ID is
    /// passed over, and stays as it was.
    #[test]
    fn a_stale_temporary_file_doesnt_stop_a_save() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        let stale = |count: u64| {
            temp.path()
                .join(format!(".env.{}.{count}.tmp", std::process::id()))
        };
        for count in 0..2000 {
            std::fs::write(stale(count), "stale").unwrap();
        }
        save(&path, ANTHROPIC_KEY, "sk-ant-one").unwrap();
        assert_eq!(
            read_dotenv(&path),
            vec![(ANTHROPIC_KEY, "sk-ant-one".to_string())]
        );
        assert_eq!(std::fs::read_to_string(stale(0)).unwrap(), "stale");
    }

    #[test]
    fn saved_values_read_back_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        for value in [
            "/home/me/.codex/auth.json",
            "/home/me/My Files/auth.json",
            "/home/me/it's/auth.json",
            "/home/me/$HOME \"x\" \\ it's #1/auth.json",
        ] {
            save(&path, CODEX_AUTH, value).unwrap();
            assert_eq!(
                read_dotenv(&path),
                vec![(CODEX_AUTH, value.to_string())],
                "{}",
                std::fs::read_to_string(&path).unwrap()
            );
        }
    }

    #[test]
    fn the_cache_setting_is_read_from_a_dotenv_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        std::fs::write(&path, "ARKO_CACHE_1H=1\n").unwrap();
        assert_eq!(read_dotenv(&path), vec![(CACHE_1H, "1".to_string())]);
    }

    /// Every provider's key is read from a `.env` file, and so is how
    /// sessions reach OpenAI models.
    #[test]
    fn a_dotenv_file_holds_every_provider_s_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        std::fs::write(
            &path,
            "OPENAI_API_KEY=sk-proj-1\nGEMINI_API_KEY=AIza1\nOPENROUTER_API_KEY=sk-or-1\nARKO_OPENAI_AUTH=api-key\n",
        )
        .unwrap();
        assert_eq!(
            read_dotenv(&path),
            vec![
                (OPENAI_KEY, "sk-proj-1".to_string()),
                (GEMINI_KEY, "AIza1".to_string()),
                (OPENROUTER_KEY, "sk-or-1".to_string()),
                (OPENAI_AUTH, "api-key".to_string()),
            ]
        );
    }

    /// A key pasted where another provider's goes is caught before it is
    /// saved under the wrong name.
    #[test]
    fn a_key_for_another_provider_is_recognised() {
        let name = |key: Option<&Key>| key.map(|key| key.name);
        assert_eq!(
            name(mistaken_for("sk-ant-api03-x", &OPENAI)),
            Some(ANTHROPIC_KEY)
        );
        assert_eq!(
            name(mistaken_for("sk-or-v1-x", &OPENAI)),
            Some(OPENROUTER_KEY)
        );
        assert_eq!(name(mistaken_for("sk-proj-x", &OPENAI)), None);
        assert_eq!(
            name(mistaken_for("sk-proj-x", &ANTHROPIC)),
            Some(OPENAI_KEY)
        );
        assert_eq!(name(mistaken_for("sk-ant-x", &ANTHROPIC)), None);
        assert_eq!(name(mistaken_for("AIzaSy", &OPENROUTER)), Some(GEMINI_KEY));
        assert_eq!(name(mistaken_for("pa-x", &GEMINI)), Some(VOYAGE_KEY));
        assert_eq!(name(mistaken_for("something", &GEMINI)), None);
        assert_eq!(capitalized(OPENAI.what), "OpenAI API key");
        assert_eq!(capitalized(GEMINI.what), "Gemini API key");
    }

    #[test]
    fn what_is_missing_is_said_where_the_screen_cant_be_shown() {
        let message = Need::GeminiKey {
            model: "gemini-3.1-pro-preview".into(),
        }
        .message();
        assert!(message.contains("needs GEMINI_API_KEY"), "{message}");
        assert!(message.contains("arko --setup"), "{message}");
        let message = Need::ClaudeCode {
            model: "cc:claude-opus-5-5".into(),
            problem: claude_code::Problem::Missing,
        }
        .message();
        assert!(message.contains("no `claude` on the PATH"), "{message}");
        assert!(
            message.contains("npm install -g @anthropic-ai/claude-code"),
            "{message}"
        );
        let message = Need::Any.message();
        for name in [
            ANTHROPIC_KEY,
            OPENAI_KEY,
            GEMINI_KEY,
            OPENROUTER_KEY,
            "Claude Code",
        ] {
            assert!(message.contains(name), "{name}: {message}");
        }
    }

    /// A key shows its provider's prefix and its last 4 characters at most.
    #[test]
    fn a_key_shows_masked() {
        assert_eq!(
            masked("sk-ant-api03-0123456789abcdefa1b2", ANTHROPIC.prefix),
            "sk-ant-…a1b2"
        );
        assert_eq!(masked("sk-proj-0123456789wxyz", OPENAI.prefix), "sk-…wxyz");
        assert_eq!(masked("0123456789abcdef", ANTHROPIC.prefix), "…cdef");
        assert_eq!(masked("sk-ant-short", ANTHROPIC.prefix), "sk-ant-…");
        assert_eq!(masked("tiny", OPENAI.prefix), "…");
        assert_eq!(key_named("Google"), Some(&GEMINI));
        assert_eq!(key_named("voyage"), Some(&VOYAGE));
        assert_eq!(key_named("azure"), None);
        assert!(check_pasted("sk-proj-1 2", &OPENAI).is_err());
        assert!(check_pasted("pa-0123", &OPENAI).unwrap_err().starts_with(
            "That looks like a Voyage API key (it starts with pa-), not an OpenAI API key."
        ));
        assert_eq!(check_pasted("sk-proj-0123", &OPENAI), Ok(()));
    }

    /// Removing a setting keeps the file's other lines, and the file stays
    /// readable only by the user.
    #[test]
    fn unsetting_removes_the_setting_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        unset(&path, ANTHROPIC_KEY).unwrap();
        assert!(!path.exists(), "nothing to remove from");
        std::fs::write(
            &path,
            "OTHER=1\nexport ANTHROPIC_API_KEY=old\n# note\nANTHROPIC_API_KEY=new\n",
        )
        .unwrap();
        unset(&path, ANTHROPIC_KEY).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "OTHER=1\n# note\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        unset(&path, OPENAI_KEY).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "OTHER=1\n# note\n");
    }

    #[test]
    fn a_setting_line_is_recognised() {
        assert!(sets("ANTHROPIC_API_KEY=x", ANTHROPIC_KEY));
        assert!(sets("  export ANTHROPIC_API_KEY = x", ANTHROPIC_KEY));
        assert!(!sets("ANTHROPIC_API_KEY_OLD=x", ANTHROPIC_KEY));
        assert!(!sets("# ANTHROPIC_API_KEY=x", ANTHROPIC_KEY));
    }
}
