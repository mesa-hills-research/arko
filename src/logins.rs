//! The ChatGPT logins in a session. The model client signs in with all of
//! them, in order, as ChatGPT accounts: requests go to the first, and move on
//! to the next at once when one reaches its usage limit, then come back when
//! it is usable again. A login that is changed while Arko runs (`/chatgpt`, an
//! app, another session of the folder) or whose file `codex login` writes
//! again with another account is used from the next request, by the main
//! agent and the workers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arko_ai::{ChatGptAccount, ChatGptAccounts, CodexAuthFile};

use crate::credentials::{self, ChatGptLogin, LoginSource, LoginState};
use crate::keys::{Keys, Outcome};
use crate::remote::Source;

/// The ChatGPT accounts of some Codex auth.json files, and what stopped
/// others from being used.
pub(crate) struct Built {
    pub(crate) accounts: Vec<ChatGptAccount>,
    /// A sentence for each file that is not used.
    pub(crate) problems: Vec<String>,
}

/// The names of the logins `paths` in failover notices and usage: the name
/// of the folder of each file (`.codex-200` is `codex-200`), or its whole
/// path where two logins would share a name.
pub(crate) fn labels(paths: &[PathBuf]) -> Vec<String> {
    let name = |path: &Path| {
        path.parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().trim_start_matches('.').to_string())
            .filter(|name| !name.is_empty())
    };
    paths
        .iter()
        .map(|path| match name(path) {
            Some(name) if paths.iter().filter(|other| name_is(other, &name)).count() == 1 => name,
            _ => path.display().to_string(),
        })
        .collect()
}

fn name_is(path: &Path, name: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|folder| folder.to_string_lossy().trim_start_matches('.') == name)
}

/// The accounts of the Codex auth.json files `paths`, in order. A file that
/// can't be read, holds no ChatGPT login, or holds an account an earlier one
/// has is left out, with a sentence that says why. With `url`, requests go
/// to the server there (tests).
pub(crate) fn build(paths: &[PathBuf], url: Option<&str>) -> Built {
    let mut accounts = Vec::new();
    let mut problems = Vec::new();
    let mut seen: Vec<(String, &Path)> = Vec::new();
    for (path, label) in paths.iter().zip(labels(paths)) {
        let file = match CodexAuthFile::load(path) {
            Ok(file) => file,
            Err(error) => {
                problems.push(error.to_string());
                continue;
            }
        };
        let id = match file.credentials() {
            Ok(credentials) => credentials.account_id,
            Err(error) => {
                problems.push(format!(
                    "{} holds no ChatGPT login ({error}). Sign in with `codex login`",
                    path.display()
                ));
                continue;
            }
        };
        if let Some((_, other)) = seen.iter().find(|(seen, _)| *seen == id) {
            problems.push(format!(
                "{} is the same ChatGPT account as {}",
                path.display(),
                other.display()
            ));
            continue;
        }
        let account = match url {
            Some(url) => file.openai_client().map(|client| {
                ChatGptAccount::new(label, client.with_base_url(url)).with_file(&file)
            }),
            None => ChatGptAccount::from_codex_auth_file(label, &file),
        };
        match account {
            Ok(account) => {
                seen.push((id, path));
                accounts.push(account);
            }
            Err(error) => problems.push(error.to_string()),
        }
    }
    Built { accounts, problems }
}

/// The model client's ChatGPT accounts for `paths`: the logins that work,
/// following their files, with the sentences about the others. When none
/// works, the sentence about the first. With `url`, requests go to the
/// server there (tests).
pub(crate) fn chatgpt_accounts(
    paths: &[PathBuf],
    url: Option<&str>,
) -> Result<(ChatGptAccounts, Vec<String>), String> {
    if paths.is_empty() {
        // (Codex's own login has no folder to be in.)
        return Err(CodexAuthFile::load_default().err().map_or_else(
            || "There is no ChatGPT login.".to_string(),
            |error| error.to_string(),
        ));
    }
    let built = build(paths, url);
    let accounts = ChatGptAccounts::new(built.accounts).map_err(|error| {
        built
            .problems
            .first()
            .cloned()
            .unwrap_or_else(|| error.to_string())
    })?;
    Ok((accounts.with_file_following(), built.problems))
}

/// The Codex auth.json files a session signs in with: those set (`paths`),
/// else Codex's own.
pub(crate) fn paths_in_use_of(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        return CodexAuthFile::default_path().into_iter().collect();
    }
    paths.to_vec()
}

/// How `/chatgpt` is used.
pub const USAGE: &str = "/chatgpt lists the ChatGPT logins. /chatgpt add PATH adds a Codex auth.json, /chatgpt remove N removes a login, and /chatgpt first N puts one first. N is the login's number in the list, or its path. Logins are used in order, and the next one takes over when one reaches its usage limit.";

/// `path` made absolute, as the logins are saved.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Keys {
    /// Uses `path`, the session's `--codex-auth` file, ahead of the
    /// ARKO_CODEX_AUTH setting.
    pub(crate) fn use_codex_auth(&self, path: Option<PathBuf>) {
        if let Some(path) = path {
            let _ = self.codex_auth.set(path);
        }
    }

    /// Reads the logins as the session's context does.
    fn within<R>(&self, run: impl FnOnce() -> R) -> R {
        crate::context::within(self.context.clone(), run)
    }

    /// The Codex auth.json files the session signs in with, in order.
    pub(crate) fn login_paths(&self) -> Vec<PathBuf> {
        self.within(|| {
            paths_in_use_of(&credentials::codex_auths(
                self.codex_auth.get().map(PathBuf::as_path),
            ))
        })
    }

    /// The ChatGPT logins in use, in order.
    pub fn logins(&self) -> Vec<ChatGptLogin> {
        self.within(|| credentials::chatgpt_logins(self.codex_auth.get().map(PathBuf::as_path)))
    }

    /// Adds the Codex auth.json at `path` after the ChatGPT logins, used
    /// from the agents' next request.
    pub fn add_login(&self, path: &Path) -> Outcome {
        self.change_logins(|paths, _| {
            credentials::add_login_to(paths, path)?;
            Ok(format!("Added {}.", absolute(path).display()))
        })
    }

    /// Removes the Codex auth.json at `path` from the ChatGPT logins.
    pub fn remove_login(&self, path: &Path) -> Outcome {
        self.change_logins(|paths, codex_own| {
            credentials::remove_login_from(paths, path, codex_own)?;
            Ok(format!("Removed {}.", absolute(path).display()))
        })
    }

    /// Puts the Codex auth.json at `path` first among the ChatGPT logins.
    pub fn first_login(&self, path: &Path) -> Outcome {
        self.change_logins(|paths, _| {
            credentials::first_login_of(paths, path)?;
            Ok(format!(
                "{} is the first login now.",
                absolute(path).display()
            ))
        })
    }

    /// Changes the saved logins with `change` (which says what it did),
    /// then has this session and the other sessions of the folder use them.
    fn change_logins(
        &self,
        change: impl FnOnce(&mut Vec<PathBuf>, bool) -> Result<String, String>,
    ) -> Outcome {
        let mut said = String::new();
        let result = self.within(|| {
            credentials::with_logins(
                &credentials::current(),
                self.codex_auth.get().map(PathBuf::as_path),
                |paths, codex_own| {
                    said = change(paths, codex_own)?;
                    Ok(())
                },
            )
        });
        if let Err(why) = result {
            return Outcome::Failed(why);
        }
        let (now, warnings) = match self.apply_logins() {
            Ok(problems) => (
                "The agents use the list from their next request.".to_string(),
                problems,
            ),
            Err(Unused::Other) => ("A new session uses it.".to_string(), Vec::new()),
            Err(Unused::Failed(why)) => (
                "A new session uses it.".to_string(),
                vec![format!("This session goes on as it was. {why}")],
            ),
        };
        if let Some(root) = self.shared.get() {
            credentials::announce_logins(root);
        }
        Outcome::Done {
            said: format!("{said} {now}"),
            warnings,
        }
    }

    /// Has the model client sign in with the logins in use, which the
    /// agents' next request goes by. Replies the sentences about the files
    /// left out. Fails when the session signs in another way, or with a
    /// sentence about why no login works.
    pub(crate) fn apply_logins(&self) -> Result<Vec<String>, Unused> {
        let Some(accounts) = self.client.chatgpt_accounts() else {
            return Err(Unused::Other);
        };
        let built = build(&self.login_paths(), self.local.get().map(String::as_str));
        if built.accounts.is_empty() {
            return Err(Unused::Failed(
                built
                    .problems
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "There is no ChatGPT login.".to_string()),
            ));
        }
        accounts
            .set_accounts(built.accounts)
            .map_err(|error| Unused::Failed(error.to_string()))?;
        Ok(built.problems)
    }

    /// Has this session use the logins another session or an app changed
    /// (see [`credentials::login_changes`]), and says so.
    pub(crate) fn share_logins(self: &Arc<Self>, root: PathBuf) {
        let mut changes = credentials::login_changes();
        let keys = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(changed) if changed == root => {}
                    // (A change may have been missed: the logins are read again.)
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                let Some(keys) = keys.upgrade() else {
                    break;
                };
                let before = keys.account_labels();
                match keys.apply_logins() {
                    Ok(problems) => {
                        if keys.account_labels() != before {
                            keys.notice("The ChatGPT logins changed. The agents use them from their next request.");
                        }
                        for problem in problems {
                            keys.notice(&problem);
                        }
                    }
                    Err(Unused::Other) => {}
                    Err(Unused::Failed(why)) => keys.notice(&format!(
                        "The ChatGPT logins changed, and this session goes on as it was. {why}"
                    )),
                }
            }
        });
    }

    /// The labels of the logins the model client signs in with now.
    fn account_labels(&self) -> Vec<String> {
        self.client
            .chatgpt_accounts()
            .map(|accounts| {
                accounts
                    .accounts()
                    .iter()
                    .map(|account| account.label().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Why the model client doesn't take the logins in use.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unused {
    /// The session signs in another way (an API key, or the OPENAI_OAUTH_*
    /// settings). A new session uses the logins.
    Other,
    /// No login works, as the sentence says.
    Failed(String),
}

impl crate::Harness {
    /// `/chatgpt`, typed in the terminal or sent from the page: lists the
    /// ChatGPT logins, and adds, removes and orders them (at the terminal).
    pub(crate) fn chatgpt_command(&mut self, argument: &str, source: Source) {
        let (word, rest) = argument
            .split_once(char::is_whitespace)
            .unwrap_or((argument, ""));
        let rest = rest.trim().trim_matches(['"', '\'']);
        let logins = self.keys.logins();
        // A login is named by its number in the list, or its path.
        let named = |rest: &str| -> Result<PathBuf, String> {
            match rest.parse::<usize>() {
                Ok(n) => n
                    .checked_sub(1)
                    .and_then(|at| logins.get(at))
                    .and_then(|login| login.path.clone())
                    .ok_or_else(|| format!("There is no login {n}. {USAGE}")),
                Err(_) if rest.is_empty() => Err(USAGE.to_string()),
                Err(_) => Ok(credentials::expand_home(rest)),
            }
        };
        let outcome = match word {
            "" => {
                let lines = self.login_lines(source);
                self.mirrored(|harness| {
                    for line in &lines {
                        harness.output.line(line);
                    }
                    if source == Source::Terminal {
                        harness.output.line(&harness.output.dim(USAGE));
                    }
                });
                return;
            }
            "add" | "remove" | "first" if source == Source::Web => {
                Outcome::Failed("ChatGPT logins are changed at the terminal.".to_string())
            }
            "add" if rest.is_empty() => Outcome::Failed(USAGE.to_string()),
            "add" => self.keys.add_login(&credentials::expand_home(rest)),
            "remove" => match named(rest) {
                Ok(path) => self.keys.remove_login(&path),
                Err(why) => Outcome::Failed(why),
            },
            "first" => match named(rest) {
                Ok(path) => self.keys.first_login(&path),
                Err(why) => Outcome::Failed(why),
            },
            _ => Outcome::Failed(USAGE.to_string()),
        };
        self.mirrored(|harness| match outcome {
            Outcome::Done { said, warnings } => {
                harness.output.line(&said);
                for warning in warnings {
                    harness.output.line(&harness.output.warn(&warning));
                }
            }
            Outcome::Failed(why) => harness.output.line(&harness.output.error(&why)),
        });
    }

    /// The lines `/chatgpt` lists: each login with its number, its file
    /// (only its name on the page), and what is unusual about it.
    fn login_lines(&self, source: Source) -> Vec<String> {
        let logins = self.keys.logins();
        if logins.is_empty() {
            return vec![
                "No ChatGPT login. `codex login` signs in, and /chatgpt add PATH adds a Codex auth.json."
                    .to_string(),
            ];
        }
        let paths: Vec<PathBuf> = logins
            .iter()
            .filter_map(|login| login.path.clone())
            .collect();
        let labels = labels(&paths);
        let statuses = self
            .client
            .chatgpt_accounts()
            .map(ChatGptAccounts::statuses)
            .unwrap_or_default();
        let mut lines = vec!["ChatGPT logins".to_string()];
        for (n, (login, label)) in logins.iter().zip(&labels).enumerate() {
            let shown = match (source, &login.path) {
                (Source::Terminal, Some(path)) => path.display().to_string(),
                _ => label.clone(),
            };
            let mut notes: Vec<String> = Vec::new();
            match &login.state {
                LoginState::Ready => {}
                LoginState::Missing => notes.push("file missing".to_string()),
                // (The reason holds the path, which the page leaves out.)
                LoginState::Invalid(why) if source == Source::Terminal => {
                    notes.push(format!("not usable ({why})"))
                }
                LoginState::Invalid(_) => notes.push("not usable".to_string()),
            }
            if login.source == LoginSource::Default {
                notes.push("Codex's own".to_string());
            }
            if let Some(status) = statuses.iter().find(|status| status.label == *label) {
                if status.exhausted {
                    notes.push(match status.resets_at {
                        Some(at) => format!("set aside until {}", until(at)),
                        None => "set aside".to_string(),
                    });
                } else if status.active {
                    notes.push("serving".to_string());
                }
                if let Some(text) = status
                    .plan_usage
                    .as_ref()
                    .and_then(crate::providers::plan_usage_text)
                {
                    notes.push(text);
                }
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" · {}", notes.join(" · "))
            };
            lines.push(format!("  {}  {shown}{notes}", n + 1));
        }
        if self.client.chatgpt_accounts().is_none() {
            lines.push(
                "A new session uses them. This one reaches OpenAI models another way.".to_string(),
            );
        }
        lines
    }
}

/// How many of the ChatGPT logins are set aside.
pub(crate) fn set_aside(accounts: Option<&ChatGptAccounts>) -> usize {
    accounts.map_or(0, |accounts| {
        accounts
            .statuses()
            .iter()
            .filter(|status| status.exhausted)
            .count()
    })
}

/// What the ChatGPT plan's usage adds when `set_aside` of the logins are set
/// aside: ", 1 set aside".
pub(crate) fn set_aside_note(set_aside: usize) -> String {
    match set_aside {
        0 => String::new(),
        n => format!(", {n} set aside"),
    }
}

/// A local time for a Unix time: "14:30" today, else "Oct 20 14:30".
fn until(at: std::time::SystemTime) -> String {
    use chrono::{DateTime, Local};
    let at: DateTime<Local> = at.into();
    if at.date_naive() == Local::now().date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%b %-d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_ai::{Conversation, GenerationConfig, LLMClient};
    use base64::Engine;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A ChatGPT backend that answers each account (by its
    /// `chatgpt-account-id`) from its own script, and notes who asked.
    struct Backend {
        url: String,
        asked: Arc<Mutex<Vec<String>>>,
        script: Arc<Mutex<VecDeque<Answer>>>,
    }

    /// An account's next answer, with what to do just before giving it.
    type Answer = (String, String, Option<Box<dyn FnOnce() + Send>>);

    impl Backend {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let asked = Arc::new(Mutex::new(Vec::new()));
            let script: Arc<Mutex<VecDeque<Answer>>> = Arc::default();
            let (noted, answers) = (asked.clone(), script.clone());
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let (noted, answers) = (noted.clone(), answers.clone());
                    tokio::spawn(async move {
                        let mut bytes = Vec::new();
                        let head = loop {
                            let mut chunk = [0; 8192];
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            bytes.extend_from_slice(&chunk[..n]);
                            let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                            let length: usize = head
                                .to_ascii_lowercase()
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("content-length:").map(str::to_string)
                                })
                                .map_or(0, |value| value.trim().parse().unwrap());
                            if bytes.len() >= end + 4 + length {
                                break head;
                            }
                        };
                        let account = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("chatgpt-account-id")
                                    .then(|| value.trim().to_string())
                            })
                            .unwrap_or_default();
                        noted.lock().unwrap().push(account.clone());
                        let answer = {
                            let mut answers = answers.lock().unwrap();
                            let at = answers.iter().position(|(who, _, _)| *who == account);
                            at.and_then(|at| answers.remove(at))
                        };
                        let reply = match answer {
                            Some((_, reply, before)) => {
                                before.into_iter().for_each(|before| before());
                                reply
                            }
                            None => "HTTP/1.1 500 Unscripted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                        };
                        let reply = if reply.starts_with("HTTP/1.1 ") {
                            reply
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                                reply.len()
                            )
                        };
                        let _ = socket.write_all(reply.as_bytes()).await;
                    });
                }
            });
            Self { url, asked, script }
        }

        /// `account`'s next answer: a model turn, or a whole HTTP response.
        fn answer(&self, account: &str, reply: String) {
            self.answer_after(account, reply, None);
        }

        fn answer_after(
            &self,
            account: &str,
            reply: String,
            before: Option<Box<dyn FnOnce() + Send>>,
        ) {
            self.script
                .lock()
                .unwrap()
                .push_back((account.to_string(), reply, before));
        }

        /// The accounts that asked since the last call.
        fn asked(&self) -> Vec<String> {
            std::mem::take(&mut *self.asked.lock().unwrap())
        }
    }

    fn refusal(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn usage_limit() -> String {
        refusal(
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"pro","resets_at":4102444800}}"#,
        )
    }

    fn token_revoked() -> String {
        refusal(
            401,
            r#"{"error":{"code":"token_revoked","message":"Your authentication token has been invalidated."}}"#,
        )
    }

    /// A Codex auth.json for `account` with a token that doesn't expire.
    fn sign_in(path: &Path, account: &str) {
        let part = |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let token = format!(
            "{}.{}.signature",
            part(r#"{"alg":"none"}"#),
            part(r#"{"exp":4102444800}"#)
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({"auth_mode": "chatgpt", "tokens": {
                "access_token": token,
                "refresh_token": format!("refresh-{account}"),
                "account_id": account,
            }})
            .to_string(),
        )
        .unwrap();
    }

    /// A client over the logins `paths`, sending to `backend`.
    fn client(paths: &[PathBuf], backend: &Backend) -> Arc<LLMClient> {
        let (accounts, problems) = chatgpt_accounts(paths, Some(&backend.url)).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        Arc::new(LLMClient::new().with_chatgpt_accounts(accounts))
    }

    /// Which login served a request.
    async fn served_by(client: &LLMClient) -> String {
        let mut conversation = Conversation::new(crate::providers::resolve_model("sol").unwrap());
        conversation.add_user_message("Hello.");
        let response = client
            .generate_text(&conversation, GenerationConfig::default())
            .await
            .unwrap();
        response.model_call_usage.account.unwrap()
    }

    fn labels_of(client: &LLMClient) -> Vec<String> {
        client
            .chatgpt_accounts()
            .unwrap()
            .accounts()
            .iter()
            .map(|account| account.label().to_string())
            .collect()
    }

    /// The logins are named by their folders, in the order they are listed.
    #[test]
    fn logins_are_named_by_their_folders() {
        let paths = |list: &[&str]| list.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            labels(&paths(&["/h/.codex/auth.json", "/h/.codex-100/auth.json"])),
            ["codex", "codex-100"]
        );
        // Two in folders of one name are told apart by their paths.
        assert_eq!(
            labels(&paths(&["/a/codex/auth.json", "/b/codex/auth.json"])),
            ["/a/codex/auth.json", "/b/codex/auth.json"]
        );
    }

    /// A request goes to the first login, and to the next at once when the
    /// first has reached its usage limit.
    #[tokio::test]
    async fn the_next_login_takes_over_when_the_first_reaches_its_limit() {
        let temp = tempfile::tempdir().unwrap();
        let (first, second) = (
            temp.path().join("one/auth.json"),
            temp.path().join("two/auth.json"),
        );
        sign_in(&first, "account-1");
        sign_in(&second, "account-2");
        let backend = Backend::start().await;
        let client = client(&[first, second], &backend);
        backend.answer("account-1", usage_limit());
        backend.answer("account-2", crate::mock_model::reply("1", "hello"));
        assert_eq!(served_by(&client).await, "two");
        assert_eq!(backend.asked(), ["account-1", "account-2"]);
        // The plan usage says the first is set aside.
        assert_eq!(set_aside(client.chatgpt_accounts()), 1);
        assert_eq!(set_aside_note(1), ", 1 set aside");
        assert_eq!(set_aside(None), 0);
        assert_eq!(set_aside_note(0), "");
    }

    /// Logins added, ordered and removed during a session, here or in
    /// another session, serve the next request.
    #[tokio::test]
    async fn logins_changed_during_a_session_reach_the_next_request() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let files: Vec<PathBuf> = ["one", "two", "three"]
            .iter()
            .map(|name| temp.path().join(name).join("auth.json"))
            .collect();
        for (n, file) in files.iter().enumerate() {
            sign_in(file, &format!("account-{}", n + 1));
        }
        credentials::set_chatgpt_login(&root, &files[0]).unwrap();
        let backend = Backend::start().await;
        let client = client(&files[..1], &backend);
        let feed = crate::remote::Feed::new("main");
        let env = credentials::Env::for_folder_in(None, false, Some(&root));
        let keys = crate::context::with_env(env, || {
            Keys::new(client.clone(), None, None, Some(feed.clone()))
        });
        keys.use_local(&backend.url);
        keys.share(root.clone());
        let reply = || crate::mock_model::reply("1", "hello");

        // A second login joins after the first, which still serves.
        assert!(matches!(keys.add_login(&files[1]), Outcome::Done { .. }));
        assert_eq!(labels_of(&client), ["one", "two"]);
        backend.answer("account-1", reply());
        assert_eq!(served_by(&client).await, "one");

        // Put first, it serves the next request alone.
        assert!(matches!(keys.first_login(&files[1]), Outcome::Done { .. }));
        assert_eq!(labels_of(&client), ["two", "one"]);
        backend.asked();
        backend.answer("account-2", reply());
        assert_eq!(served_by(&client).await, "two");
        assert_eq!(backend.asked(), ["account-2"]);

        // Removed, it is not asked again.
        assert!(matches!(keys.remove_login(&files[1]), Outcome::Done { .. }));
        assert_eq!(labels_of(&client), ["one"]);
        backend.answer("account-1", reply());
        assert_eq!(served_by(&client).await, "one");
        assert!(matches!(
            keys.remove_login(&files[1]),
            Outcome::Failed(why) if why.contains("isn't in the list")
        ));

        // A login added by an app or another session reaches this one.
        assert!(feed.subscribe().0.items.is_empty());
        credentials::add_chatgpt_login(&root, &files[2]).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while labels_of(&client) != ["one", "three"] {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    /// A request rejected as revoked reads the login's file again once, and
    /// goes on to the account the file holds now. A login whose file holds
    /// another account is used as that account before a request too.
    #[tokio::test]
    async fn a_revoked_login_is_read_from_its_file_again() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("login/auth.json");
        sign_in(&file, "account-1");
        let backend = Backend::start().await;
        let client = client(std::slice::from_ref(&file), &backend);

        // The file changes while the request is rejected.
        let after = {
            let file = file.clone();
            Box::new(move || sign_in(&file, "account-2")) as Box<dyn FnOnce() + Send>
        };
        backend.answer_after("account-1", token_revoked(), Some(after));
        backend.answer("account-2", crate::mock_model::reply("1", "hello"));
        assert_eq!(served_by(&client).await, "login");
        assert_eq!(backend.asked(), ["account-1", "account-2"]);

        // The file changes between requests.
        sign_in(&file, "account-3");
        backend.answer("account-3", crate::mock_model::reply("2", "hello"));
        assert_eq!(served_by(&client).await, "login");
        assert_eq!(backend.asked(), ["account-3"]);

        // A rejection that the file doesn't explain is the error.
        backend.answer("account-3", token_revoked());
        let mut conversation = Conversation::new(crate::providers::resolve_model("sol").unwrap());
        conversation.add_user_message("Hello.");
        let error = client
            .generate_text(&conversation, GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), arko_ai::ErrorKind::Authentication);
        assert_eq!(backend.asked(), ["account-3"]);
    }

    /// A reply that reports the weekly window at 82% and 12.50 credits.
    fn with_plan_headers(turn: String) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nx-codex-primary-used-percent: 82\r\nx-codex-primary-window-minutes: 10080\r\nx-codex-credits-has-credits: True\r\nx-codex-credits-unlimited: False\r\nx-codex-credits-balance: 12.50\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{turn}",
            turn.len()
        )
    }

    /// The status line shows the credits of the login that served the turn,
    /// and how many others are set aside.
    #[tokio::test]
    async fn the_status_shows_the_serving_logins_credits_and_the_others_set_aside() {
        let temp = tempfile::tempdir().unwrap();
        let (first, second) = (
            temp.path().join("one/auth.json"),
            temp.path().join("two/auth.json"),
        );
        sign_in(&first, "account-1");
        sign_in(&second, "account-2");
        let backend = Backend::start().await;
        backend.answer("account-1", usage_limit());
        backend.answer(
            "account-2",
            with_plan_headers(crate::mock_model::reply("1", "hello")),
        );
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let running = crate::spawn(
            crate::Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
                model: Some("sol".into()),
                openai_auth: Some(crate::OpenAiAuth::ChatGpt),
                cache_1h: Some(false),
                text_replies: Some(true),
                client: Some(crate::options::GivenClient(client(
                    &[first, second],
                    &backend,
                ))),
                ..crate::Options::default()
            },
            Arc::new(crate::NoPrompter),
        );
        let seam = tokio::time::timeout(std::time::Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seam.input
            .send(crate::Input::Message(crate::Message {
                text: "Hi.".into(),
                images: Vec::new(),
                answers: Vec::new(),
                continue_uncached: false,
            }))
            .unwrap();
        let line = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(line) = seam.meter.read().plan_usage {
                    return line;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            line,
            "ChatGPT plan 7 d 82%, credits left 12.50, 1 set aside"
        );
        // The status has them as data too.
        let plan = seam.meter.read().plans.remove(0);
        assert_eq!(plan.set_aside, 1);
        assert_eq!(plan.credits.unwrap().balance.as_deref(), Some("12.50"));
        seam.input
            .send(crate::Input::Quit(Some(Vec::new())))
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), running.ended).await;
    }

    /// `--codex-auth` is the first login of the session, whatever else is
    /// set.
    #[tokio::test]
    async fn the_session_flag_is_the_first_login() {
        let temp = tempfile::tempdir().unwrap();
        let (flag, set) = (
            temp.path().join("flag/auth.json"),
            temp.path().join("set/auth.json"),
        );
        sign_in(&flag, "account-1");
        sign_in(&set, "account-2");
        let root = temp.path().join("arko");
        let env = credentials::Env::for_folder_in(None, false, Some(&root));
        let keys = crate::context::with_env(env, || {
            Keys::new(Arc::new(LLMClient::new()), None, None, None)
        });
        keys.use_codex_auth(Some(flag.clone()));
        if std::env::var_os(credentials::CODEX_AUTH).is_none() {
            assert_eq!(keys.login_paths(), std::slice::from_ref(&flag));
            assert!(matches!(keys.add_login(&set), Outcome::Done { .. }));
            assert_eq!(keys.login_paths(), [flag, set]);
        }
    }
}
