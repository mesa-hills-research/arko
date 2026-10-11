//! Credentials: the Anthropic API key and the ChatGPT login (a Codex
//! auth.json), from the environment or `.env` files, and the setup screen
//! that asks for them when the chosen model has none or its file fails.
//! `.env` files can also turn on the 1-hour prompt cache (FIN_CACHE_1H),
//! choose the shell on Windows (FIN_SHELL) and hold the Voyage API key the
//! search indexes embed with (VOYAGE_API_KEY), which the setup screen also
//! takes.
//!
//! What the screen takes is saved in fin's own `.env` (in its folder,
//! readable only by the user) and used in this run at once. Nothing read
//! here is put in the environment, so the commands the agents run don't
//! see it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use arko_ai::CodexAuthFile;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{ColorMode, Context, Helper};

use crate::session;
use crate::ui::Output;

/// The Anthropic API key.
pub const ANTHROPIC_KEY: &str = "ANTHROPIC_API_KEY";
/// The Codex auth.json that holds the ChatGPT login.
pub const CODEX_AUTH: &str = "FIN_CODEX_AUTH";
/// Anthropic's 1-hour prompt cache for new sessions: `1` or `0`.
pub const CACHE_1H: &str = "FIN_CACHE_1H";
/// The shell new sessions run commands in on Windows: `cmd` or `powershell`.
pub const SHELL: &str = "FIN_SHELL";
/// The Voyage API key, for the search indexes' embeddings.
pub const VOYAGE_KEY: &str = "VOYAGE_API_KEY";
/// The settings a `.env` file can hold; it may set others, which are left
/// alone.
const DOTENV_KEYS: [&str; 6] = [
    ANTHROPIC_KEY,
    CODEX_AUTH,
    CACHE_1H,
    SHELL,
    VOYAGE_KEY,
    crate::glyphs::SYMBOLS,
];

/// What was entered on the setup screen in this run: it comes before every
/// other source.
static ENTERED: Mutex<Vec<(&str, String)>> = Mutex::new(Vec::new());

/// Where a setting comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Entered,
    Environment,
    DotEnv(PathBuf),
}

/// A setting, if it is set: entered on the setup screen, else from the
/// environment, else (ANTHROPIC_API_KEY and FIN_CODEX_AUTH) from a `.env`
/// file.
pub fn var(name: &str) -> Option<String> {
    lookup(name).map(|(value, _)| value)
}

fn lookup(name: &str) -> Option<(String, Source)> {
    if let Some(value) = entered(name) {
        return Some((value, Source::Entered));
    }
    if let Some(value) = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Some((value, Source::Environment));
    }
    dotenv()
        .get(name)
        .map(|(value, path)| (value.clone(), Source::DotEnv(path.clone())))
}

fn entered(name: &str) -> Option<String> {
    let entered = ENTERED.lock().unwrap();
    entered
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.clone())
}

fn enter(name: &'static str, value: String) {
    let mut entered = ENTERED.lock().unwrap();
    entered.retain(|(key, _)| *key != name);
    entered.push((name, value));
}

/// The settings in `.env` files, with the file each came from: the nearest
/// `.env` in the current directory or a parent first, then the one in fin's
/// folder. Read once.
fn dotenv() -> &'static HashMap<&'static str, (String, PathBuf)> {
    static VALUES: OnceLock<HashMap<&'static str, (String, PathBuf)>> = OnceLock::new();
    VALUES.get_or_init(|| {
        let nearest = std::env::current_dir().ok().and_then(|directory| {
            directory
                .ancestors()
                .map(|directory| directory.join(".env"))
                .find(|path| path.is_file())
        });
        let mut values = HashMap::new();
        for path in nearest.into_iter().chain(fin_dotenv().ok()) {
            for (key, value) in read_dotenv(&path) {
                values.entry(key).or_insert((value, path.clone()));
            }
        }
        values
    })
}

/// fin's own `.env`, in its folder: where the setup screen saves.
pub fn fin_dotenv() -> Result<PathBuf, String> {
    Ok(session::data_root()?.join(".env"))
}

/// The settings fin reads in the `.env` file at `path`. A relative
/// FIN_CODEX_AUTH is relative to the file's folder.
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
            folder
                .join(expand_home(value.trim()))
                .to_string_lossy()
                .into_owned()
        } else {
            value
        };
        found.push((key, value));
    }
    found
}

/// `~` and `~/...` with the home directory in place of `~`.
fn expand_home(path: &str) -> PathBuf {
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
            "{name} ({}) is `{value}`; set it to 1 or 0",
            describe(&source)
        )),
    }
}

/// The Codex auth.json to use: one entered on the setup screen, else
/// `--codex-auth`, else FIN_CODEX_AUTH; `None` leaves it to Codex's default
/// (`$CODEX_HOME/auth.json`, else `~/.codex/auth.json`).
pub fn codex_auth(flag: Option<&Path>) -> Option<PathBuf> {
    entered(CODEX_AUTH)
        .map(PathBuf::from)
        .or_else(|| flag.map(Path::to_path_buf))
        .or_else(|| var(CODEX_AUTH).map(|path| expand_home(&path)))
}

/// Credentials a session can't start without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// No model was chosen, and there is no credential for either provider.
    Any,
    /// The model needs an Anthropic API key.
    AnthropicKey { model: String },
    /// The model needs a ChatGPT login; `problem` says what failed.
    ChatGptLogin { model: String, problem: String },
}

impl Need {
    /// What is missing, as an error where the setup screen can't be shown.
    pub fn message(&self) -> String {
        let what = match self {
            Need::Any => "no credentials: set ANTHROPIC_API_KEY (in the environment or a .env file) for Claude models, or sign in to ChatGPT with `codex login` for OpenAI models".to_string(),
            Need::AnthropicKey { model } => {
                format!("{model} needs ANTHROPIC_API_KEY (in the environment or a .env file)")
            }
            Need::ChatGptLogin { model, problem } => {
                format!("{model} needs a ChatGPT login (`codex login`): {problem}")
            }
        };
        format!("{what}; or run `fin --setup` in a terminal to enter it")
    }
}

/// Whether the setup screen can be shown: standard input and output are a
/// terminal.
pub fn can_ask() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The setup screen: asks for what `need` names (either credential with
/// `None` or `Need::Any`), saves it in fin's `.env` and uses it in this run.
/// An error when the user quits.
pub fn setup(need: Option<&Need>) -> Result<(), String> {
    let out = Output::new(None, false);
    let dotenv = fin_dotenv()?;
    out.line("");
    out.line(&out.bold("fin setup"));
    match need {
        None => {
            out.line(&out.dim("Credentials now:"));
            let key = match lookup(ANTHROPIC_KEY) {
                None => "not set".to_string(),
                Some((_, source)) => format!("set ({})", describe(&source)),
            };
            out.line(&format!("  Anthropic API key  {key}"));
            let login = codex_auth(None)
                .or_else(|| CodexAuthFile::default_path().filter(|path| path.is_file()))
                .map_or("none found".to_string(), |path| path.display().to_string());
            out.line(&format!("  ChatGPT login      {login}"));
            let voyage = match lookup(VOYAGE_KEY) {
                None => "not set (search indexes are keyword-only)".to_string(),
                Some((_, source)) => format!("set ({})", describe(&source)),
            };
            out.line(&format!("  Voyage API key     {voyage}"));
        }
        Some(Need::Any) => out.line(
            "No credentials found: ANTHROPIC_API_KEY isn't set (in the environment or a .env file), and there is no ChatGPT login.",
        ),
        Some(Need::AnthropicKey { model }) => out.line(&format!(
            "{model} needs an Anthropic API key: ANTHROPIC_API_KEY isn't set in the environment or a .env file."
        )),
        Some(Need::ChatGptLogin { model, problem }) => {
            out.line(&format!("{model} needs a ChatGPT login: {problem}"))
        }
    }
    out.line("");
    let mut editor = Prompt::editor()?;
    match need {
        Some(Need::AnthropicKey { .. }) => ask_key(&out, &mut editor, &dotenv),
        Some(Need::ChatGptLogin { .. }) => ask_login(&out, &mut editor, &dotenv),
        None | Some(Need::Any) => {
            // The Voyage key is no model's credential: offered on its own.
            let voyage = need.is_none();
            out.line("  1  Paste an Anthropic API key, for Claude models");
            out.line(
                "  2  Give the path to a Codex auth.json with a ChatGPT login, for OpenAI models",
            );
            if voyage {
                out.line("  3  Paste a Voyage API key, for semantic search in the search indexes");
            }
            out.line("  q  Quit");
            let prompt = if voyage {
                "Choose 1, 2, 3 or q: "
            } else {
                "Choose 1, 2 or q: "
            };
            loop {
                let choice = read(&mut editor, prompt, "", false)?;
                match choice.trim() {
                    "1" => return ask_key(&out, &mut editor, &dotenv),
                    "2" => return ask_login(&out, &mut editor, &dotenv),
                    "3" if voyage => return ask_voyage_key(&out, &mut editor, &dotenv),
                    "" | "q" | "Q" => return Err(cancelled()),
                    _ => {}
                }
            }
        }
    }
}

fn describe(source: &Source) -> String {
    match source {
        Source::Entered => "entered now".into(),
        Source::Environment => "from the environment".into(),
        Source::DotEnv(path) => format!("from {}", path.display()),
    }
}

fn cancelled() -> String {
    "setup cancelled".into()
}

fn ask_key(out: &Output, editor: &mut Editor, dotenv: &Path) -> Result<(), String> {
    out.line(&out.dim(
        "Paste your Anthropic API key (from console.anthropic.com; the input is hidden). An empty line quits.",
    ));
    let key = loop {
        let key = read(editor, "API key: ", "", true)?;
        let key = key.trim();
        if key.is_empty() {
            return Err(cancelled());
        }
        if key.chars().any(char::is_whitespace) {
            out.line(&out.warn("An API key has no spaces in it; paste it again."));
            continue;
        }
        break key.to_string();
    };
    if !key.starts_with("sk-ant-") {
        out.line(&out.warn(
            "Note: Anthropic API keys start with sk-ant-; this one doesn't, so requests may be refused.",
        ));
    }
    save(dotenv, ANTHROPIC_KEY, &key)?;
    enter(ANTHROPIC_KEY, key);
    saved(out, dotenv, ANTHROPIC_KEY);
    Ok(())
}

fn ask_voyage_key(out: &Output, editor: &mut Editor, dotenv: &Path) -> Result<(), String> {
    out.line(&out.dim(
        "Paste your Voyage API key (from dashboard.voyageai.com; the input is hidden). An empty line quits.",
    ));
    let key = loop {
        let key = read(editor, "Voyage API key: ", "", true)?;
        let key = key.trim();
        if key.is_empty() {
            return Err(cancelled());
        }
        if key.chars().any(char::is_whitespace) {
            out.line(&out.warn("An API key has no spaces in it; paste it again."));
            continue;
        }
        break key.to_string();
    };
    if !key.starts_with("pa-") {
        out.line(&out.warn(
            "Note: Voyage API keys start with pa-; this one doesn't, so requests may be refused.",
        ));
    }
    save(dotenv, VOYAGE_KEY, &key)?;
    enter(VOYAGE_KEY, key);
    saved(out, dotenv, VOYAGE_KEY);
    Ok(())
}

fn ask_login(out: &Output, editor: &mut Editor, dotenv: &Path) -> Result<(), String> {
    out.line(&out.dim(
        "Give the path to a Codex auth.json holding a ChatGPT login (`codex login` writes ~/.codex/auth.json). Tab completes; an empty line quits.",
    ));
    let initial = CodexAuthFile::default_path()
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let path = loop {
        let line = read(editor, "auth.json: ", &initial, false)?;
        let line = line.trim();
        if line.is_empty() {
            return Err(cancelled());
        }
        let path = std::path::absolute(expand_home(line)).unwrap_or_else(|_| line.into());
        let checked = CodexAuthFile::load(&path)
            .map_err(|error| error.to_string())
            .and_then(|auth| {
                auth.credentials()
                    .map(|_| ())
                    .map_err(|error| format!("it holds no ChatGPT login ({error})"))
            });
        match checked {
            Ok(()) => break path,
            Err(error) => out.line(&out.warn(&format!(
                "Could not use {}: {error}. Try another path.",
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
fn saved(out: &Output, dotenv: &Path, name: &str) {
    out.line(&format!(
        "Saved as {name} in {} (readable only by you).",
        dotenv.display()
    ));
    let before = if std::env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
        Some(format!("{name} in your environment"))
    } else {
        match dotenv_get(name) {
            Some(path) if path != dotenv => {
                Some(format!("{} (it sets {name} too)", path.display()))
            }
            _ => None,
        }
    };
    if let Some(before) = before {
        out.line(&out.warn(&format!(
            "Note: {before} comes first in later runs; this run uses what you entered."
        )));
    }
    out.line("");
}

fn dotenv_get(name: &str) -> Option<PathBuf> {
    dotenv().get(name).map(|(_, path)| path.clone())
}

/// Sets `name` to `value` in the `.env` file at `path`, keeping its other
/// lines; the file is created readable only by the user.
fn save(path: &Path, name: &str, value: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("could not save {}: {error}", path.display());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(failed(error)),
    };
    let line = format!("{name}={}", quote(value));
    let mut lines = Vec::new();
    let mut replaced = false;
    for existing in text.lines() {
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
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(failed)?;
    }
    let temporary = path.with_file_name(".env.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    {
        use std::io::Write;
        let mut file = options.open(&temporary).map_err(failed)?;
        file.write_all(text.as_bytes()).map_err(failed)?;
    }
    std::fs::rename(&temporary, path).map_err(failed)
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

type Editor = rustyline::Editor<Prompt, rustyline::history::DefaultHistory>;

/// The setup screen's prompts: file names complete, and a key is hidden.
struct Prompt {
    files: FilenameCompleter,
    hidden: bool,
}

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
        Err(error) => Err(format!("could not read the terminal: {error}")),
    }
}

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

impl Hinter for Prompt {
    type Hint = String;
}

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

impl Validator for Prompt {}

impl Helper for Prompt {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotenv_file_gives_only_the_known_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        assert!(read_dotenv(&path).is_empty());
        std::fs::write(
            &path,
            "# keys\nOPENAI_API_KEY=sk-openai\nANTHROPIC_API_KEY=\n",
        )
        .unwrap();
        assert!(read_dotenv(&path).is_empty());
        std::fs::write(
            &path,
            "OTHER=1\nexport ANTHROPIC_API_KEY=\"sk-ant-test\" # comment\nFIN_CODEX_AUTH=auth.json\n",
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
        let path = temp.path().join("fin").join(".env");
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
        std::fs::write(&path, "FIN_CACHE_1H=1\n").unwrap();
        assert_eq!(read_dotenv(&path), vec![(CACHE_1H, "1".to_string())]);
    }

    #[test]
    fn a_setting_line_is_recognised() {
        assert!(sets("ANTHROPIC_API_KEY=x", ANTHROPIC_KEY));
        assert!(sets("  export ANTHROPIC_API_KEY = x", ANTHROPIC_KEY));
        assert!(!sets("ANTHROPIC_API_KEY_OLD=x", ANTHROPIC_KEY));
        assert!(!sets("# ANTHROPIC_API_KEY=x", ANTHROPIC_KEY));
    }
}
