//! Claude Code: Claude models run in the user's installed `claude` CLI,
//! under its Claude plan (`-m cc:opus`). arko_agents' Claude Code runtime
//! drives the CLI; Arko finds it, checks that it is signed in, and says how
//! it runs.
//!
//! The agents get Arko's own tools only, served to Claude Code in-process,
//! and none of Claude Code's built-in ones (`Bash`, `Edit`, ...): every
//! command still goes through Arko's `shell` tool and its command guard, as
//! on the other providers. Arko's system prompt replaces Claude Code's,
//! which explains those built-in tools.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arko_agents::ClaudeCodeSettings;
use arko_ai::providers::claude_code::NO_SELF_UPDATE;

/// The variable Claude Code bills instead of the user's Claude plan when it
/// is set. Claude Code 2.1 treats an empty value as unset (`claude auth
/// status` reports `claude.ai` with `ANTHROPIC_API_KEY=` and `api_key` with
/// a key), so Arko sets it empty: `cc:` models always run on the plan, even
/// where the user's environment holds the API key Arko's Anthropic models
/// use.
const API_KEY: &str = "ANTHROPIC_API_KEY";

/// How long `claude auth status` may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// How Arko runs Claude Code for an agent on a `cc:` model: `claude` on the
/// PATH, Arko's tools and system prompt, and never an API key.
pub fn settings() -> ClaudeCodeSettings {
    ClaudeCodeSettings {
        replace_system_prompt: true,
        env: BTreeMap::from([(API_KEY.to_string(), String::new())]),
        ..ClaudeCodeSettings::default()
    }
}

/// Why Claude Code can't run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// There is no `claude` on the PATH.
    Missing,
    /// `claude` (at this path) isn't signed in.
    SignedOut(PathBuf),
}

/// Claude Code, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub path: PathBuf,
    /// How it signs in (`claude.ai` for a Claude plan), when it said.
    pub method: Option<String>,
    /// The Claude plan (`pro`, `max`), when it said.
    pub plan: Option<String>,
}

impl Ready {
    /// How it signs in, as the setup screen says it.
    #[cfg(feature = "tui")]
    pub fn describe(&self) -> String {
        match (&self.method, &self.plan) {
            (Some(method), Some(plan)) if method == "claude.ai" => {
                format!("signed in with claude.ai ({plan} plan)")
            }
            (Some(method), _) => format!("signed in with {method}"),
            (None, _) => "found".to_string(),
        }
    }

    /// A warning when it doesn't sign in with a Claude plan: then its calls
    /// may be billed, which Arko doesn't count.
    pub fn warning(&self) -> Option<String> {
        let method = self.method.as_deref()?;
        (method != "claude.ai").then(|| {
            format!(
                "Claude Code signs in with `{method}` here, not a Claude plan: its calls may be billed per token, and Arko doesn't count their cost."
            )
        })
    }
}

/// Finds `claude` on the PATH and checks that it is signed in.
pub fn check() -> Result<Ready, Problem> {
    let result = find_cli()
        .ok_or(Problem::Missing)
        .and_then(|path| check_at(&path));
    *LAST_CHECK.lock().unwrap() = Some((Instant::now(), result.clone()));
    result
}

/// The last check's result while it is fresh, else a new check: starting
/// a session asks several times (for the default model, then for the
/// session's client), and one `claude auth status` answers them all. The
/// setup screen checks anew each time.
pub fn check_recent() -> Result<Ready, Problem> {
    let last = LAST_CHECK.lock().unwrap().clone();
    match last {
        Some((at, result)) if at.elapsed() < RECENT => result,
        _ => check(),
    }
}

/// How long a check stays fresh for [`check_recent`].
const RECENT: Duration = Duration::from_secs(10);

static LAST_CHECK: std::sync::Mutex<Option<(Instant, Result<Ready, Problem>)>> =
    std::sync::Mutex::new(None);

/// Checks that the Claude Code CLI at `path` is signed in: `claude auth
/// status --json`, which makes no model request. An answer Arko can't read
/// doesn't stop the session: the first turn reports a missing login too.
pub fn check_at(path: &Path) -> Result<Ready, Problem> {
    let ready = |method: Option<String>, plan: Option<String>| Ready {
        path: path.to_path_buf(),
        method,
        plan,
    };
    let output = match auth_status(path) {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Problem::Missing);
        }
        Err(error) => {
            log::warn!("`{} auth status` failed: {error}", path.display());
            return Ok(ready(None, None));
        }
    };
    let Ok(status) = serde_json::from_str::<serde_json::Value>(output.trim()) else {
        log::warn!(
            "`{} auth status --json` gave no JSON: {}",
            path.display(),
            output.trim()
        );
        return Ok(ready(None, None));
    };
    let text = |name: &str| status.get(name).and_then(|value| value.as_str());
    match status.get("loggedIn").and_then(|value| value.as_bool()) {
        Some(false) => Err(Problem::SignedOut(path.to_path_buf())),
        _ => Ok(ready(
            text("authMethod").map(String::from),
            text("subscriptionType").map(String::from),
        )),
    }
}

/// The standard output of `claude auth status --json`, with the API key
/// unset as sessions have it.
fn auth_status(path: &Path) -> std::io::Result<String> {
    run(path, &["auth", "status", "--json"])
}

/// The standard output of `claude` at `path` run with `args`, as sessions
/// run it: the API key unset, no self-update, at most 15 s.
fn run(path: &Path, args: &[&str]) -> std::io::Result<String> {
    let launcher = path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    });
    let mut command = if launcher {
        // npm's `claude.cmd` runs only through cmd.exe.
        let mut command = Command::new("cmd");
        command.arg("/C").arg(path);
        command
    } else {
        Command::new(path)
    };
    let mut child = command
        .args(args)
        .env(API_KEY, "")
        .env(NO_SELF_UPDATE.0, NO_SELF_UPDATE.1)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "it didn't answer in 15 s",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut text = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut text)?;
    }
    Ok(text)
}

/// `claude` on the PATH: on Windows also `claude.exe`, or the `claude.cmd`
/// launcher npm installs (arko follows it to the program it starts).
pub fn find_cli() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in(std::env::split_paths(&path))
}

/// The names `claude` has in a folder: on Windows also `claude.exe`, or
/// the `claude.cmd` npm installs.
fn names() -> Vec<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("claude{}", extension.to_ascii_lowercase()))
            .collect()
    } else {
        vec!["claude".to_string()]
    }
}

/// `claude` in the first of `folders` that has it.
fn find_in(folders: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let names = names();
    folders
        .into_iter()
        .flat_map(|folder| names.iter().map(move |name| folder.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Where Claude Code's installers put `claude`, besides the PATH: the
/// native installer's `~/.local/bin` and `~/.claude/local`, npm's global
/// folders, Homebrew's and Bun's. An app started from a desktop doesn't get
/// the PATH a shell sets up, so it looks there too.
pub fn install_dirs() -> Vec<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    install_dirs_in(
        var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).as_deref(),
        var("NPM_CONFIG_PREFIX").as_deref(),
        var("APPDATA").as_deref(),
        var("LOCALAPPDATA").as_deref(),
    )
}

fn install_dirs_in(
    home: Option<&Path>,
    npm_prefix: Option<&Path>,
    appdata: Option<&Path>,
    local_appdata: Option<&Path>,
) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(home) = home {
        folders.push(home.join(".local").join("bin"));
        folders.push(home.join(".claude").join("local"));
    }
    if let Some(prefix) = npm_prefix {
        // npm puts the commands in the prefix itself on Windows.
        folders.push(if cfg!(windows) {
            prefix.to_path_buf()
        } else {
            prefix.join("bin")
        });
    }
    if cfg!(windows) {
        if let Some(appdata) = appdata {
            folders.push(appdata.join("npm"));
        }
        if let Some(local) = local_appdata {
            folders.push(local.join("Programs").join("claude"));
        }
    } else {
        if let Some(home) = home {
            folders.push(home.join(".npm-global").join("bin"));
        }
        folders.push(PathBuf::from("/opt/homebrew/bin"));
        folders.push(PathBuf::from("/usr/local/bin"));
    }
    if let Some(home) = home {
        folders.push(home.join(".bun").join("bin"));
    }
    folders
}

/// `claude` on the PATH, else where Claude Code installs it.
pub fn find_installed() -> Option<PathBuf> {
    find_cli().or_else(|| find_in(install_dirs()))
}

/// When `claude` isn't on the PATH but is where Claude Code installs it,
/// puts its folder first on this process's PATH, so the sessions (and the
/// programs they start) find it as a shell would. Returns the folder added.
///
/// It changes the process's environment: call it before any other thread
/// starts (an app, first thing in `main`).
pub fn add_to_path() -> Option<PathBuf> {
    if find_cli().is_some() {
        return None;
    }
    let folder = find_in(install_dirs())?.parent()?.to_path_buf();
    let mut folders = vec![folder.clone()];
    if let Some(path) = std::env::var_os("PATH") {
        folders.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(folders).ok()?;
    // SAFETY: as documented, called before other threads read the
    // environment.
    unsafe { std::env::set_var("PATH", path) };
    Some(folder)
}

/// Claude Code, as an app's accounts page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeStatus {
    /// Where `claude` is, if it is installed.
    pub path: Option<PathBuf>,
    /// It is on the PATH (sessions start it from there, see
    /// [`add_to_path`]).
    pub on_path: bool,
    /// What `claude --version` says.
    pub version: Option<String>,
    /// Whether it is signed in, and how.
    pub ready: Result<Ready, Problem>,
}

/// Claude Code: where it is, its version, and whether it is signed in.
/// Runs `claude` twice (up to 15 s each): call it off a UI thread.
pub fn status() -> ClaudeCodeStatus {
    let on_path = find_cli();
    let path = on_path.clone().or_else(|| find_in(install_dirs()));
    let Some(path) = path else {
        return ClaudeCodeStatus {
            path: None,
            on_path: false,
            version: None,
            ready: Err(Problem::Missing),
        };
    };
    let version = run(&path, &["--version"])
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    ClaudeCodeStatus {
        ready: check_at(&path),
        on_path: on_path.is_some(),
        version,
        path: Some(path),
    }
}

/// How to install Claude Code and sign in, for the setup screen and errors.
pub const INSTALL: &str = "install it (npm install -g @anthropic-ai/claude-code, or Anthropic's native installer) and sign in with `claude auth login`";

#[cfg(test)]
mod tests {
    use super::*;

    /// `claude` installed where Claude Code's installers put it, off the
    /// PATH, is found there: an app started from a desktop has no shell's
    /// PATH.
    #[cfg(unix)]
    #[test]
    fn claude_code_is_found_outside_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let folders = install_dirs_in(Some(home.path()), None, None, None);
        assert_eq!(folders[0], home.path().join(".local").join("bin"));
        assert!(folders.contains(&home.path().join(".claude").join("local")));
        assert_eq!(find_in(folders.clone()), None);
        let local = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&local).unwrap();
        let claude = local.join("claude");
        std::fs::write(&claude, "#!/bin/sh\necho 2.1.0\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in(folders), Some(claude.clone()));
        assert_eq!(run(&claude, &["--version"]).unwrap().trim(), "2.1.0");
        let npm = install_dirs_in(None, Some(Path::new("/opt/npm")), None, None);
        assert_eq!(npm[0], Path::new("/opt/npm/bin"));
    }

    /// A stand-in `claude` that answers `auth status --json` with `json`,
    /// and records whether ANTHROPIC_API_KEY was set and non-empty and what
    /// DISABLE_AUTOUPDATER was.
    #[cfg(unix)]
    fn fake(dir: &Path, json: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ -n \"$ANTHROPIC_API_KEY\" ]; then echo key > \"{}\"; fi\necho \"$DISABLE_AUTOUPDATER\" > \"{}\"\necho '{json}'\n",
                dir.join("key-seen").display(),
                dir.join("updater").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_login_check_reads_claude_auth_status_without_the_api_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = fake(
            temp.path(),
            r#"{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "max"}"#,
        );
        let ready = check_at(&path).unwrap();
        assert_eq!(ready.describe(), "signed in with claude.ai (max plan)");
        assert_eq!(ready.warning(), None);
        assert!(
            !temp.path().join("key-seen").exists(),
            "the check runs with ANTHROPIC_API_KEY empty, as sessions do"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("updater")).unwrap(),
            "1\n",
            "the check runs with Claude Code's updates off, as sessions do"
        );

        let path = fake(temp.path(), r#"{"loggedIn": false, "authMethod": "none"}"#);
        assert_eq!(check_at(&path), Err(Problem::SignedOut(path.clone())));

        let path = fake(
            temp.path(),
            r#"{"loggedIn": true, "authMethod": "api_key"}"#,
        );
        assert!(
            check_at(&path)
                .unwrap()
                .warning()
                .unwrap()
                .contains("api_key")
        );

        // An answer Arko can't read doesn't stop the session.
        let path = fake(temp.path(), "not json");
        assert_eq!(check_at(&path).unwrap().method, None);

        assert_eq!(
            check_at(&temp.path().join("no-such-claude")),
            Err(Problem::Missing)
        );
    }

    #[test]
    fn sessions_run_arko_s_tools_and_prompt_and_never_the_api_key() {
        let settings = settings();
        assert!(settings.replace_system_prompt);
        assert_eq!(settings.env.get(API_KEY).map(String::as_str), Some(""));
        assert_eq!(settings.cli_path, None, "`claude` on the PATH");
        assert!(!settings.metered_billing, "on the Claude plan");
        assert!(settings.setting_sources.is_empty());
    }
}
