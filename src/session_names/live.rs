//! Asking the models for a session's name, for real. Each way reads what it
//! needs (a key, the ChatGPT logins) as it is asked, so a key removed or a
//! login changed while naming waits is the one used.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arko_ai::claude_code as cc;
use arko_ai::{Conversation, LLMClient, StreamEvent};
use async_trait::async_trait;
use futures_util::StreamExt as _;

use super::Way;
use super::naming::{Ask, Failure, PROMPT, STALL};
use crate::keys::Keys;
use crate::providers;

/// The most a model may write for a name, where the provider takes a limit.
const MAX_TOKENS: usize = 1024;

/// What the ways that take a key or a login need, as the settings hold it
/// now.
pub(crate) struct Credentials {
    /// The ChatGPT logins' files.
    pub(crate) logins: Vec<PathBuf>,
    pub(crate) anthropic_key: Option<String>,
    pub(crate) openai_key: Option<String>,
}

/// Asks the models for real.
pub(crate) struct Live {
    /// Arko's folder, where Claude Code runs (it reads nothing there).
    root: PathBuf,
    credentials: Arc<dyn Fn() -> Credentials + Send + Sync>,
}

impl Live {
    /// Asks as the session `keys` and its settings say, each time.
    pub(crate) fn capture(keys: Arc<Keys>, root: &Path) -> Self {
        let context = crate::context::handle();
        Self::with(
            root,
            Arc::new(move || {
                crate::context::within(context.clone(), || Credentials {
                    logins: keys.login_paths(),
                    anthropic_key: crate::credentials::key_value(&crate::credentials::ANTHROPIC),
                    openai_key: crate::credentials::key_value(&crate::credentials::OPENAI),
                })
            }),
        )
    }

    pub(crate) fn with(
        root: &Path,
        credentials: Arc<dyn Fn() -> Credentials + Send + Sync>,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            credentials,
        }
    }
}

/// Stops the checks of Claude Code that were started for it when it is
/// dropped, with the request that waits for them.
#[derive(Default)]
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[async_trait]
impl Ask for Live {
    async fn ask(&self, way: Way, message: &str) -> Result<String, Failure> {
        let missing = |what: &str| Failure::Unavailable(format!("no {what}"));
        match way {
            Way::ClaudePlan => {
                let stop = StopOnDrop::default();
                let cancel = stop.0.clone();
                // Only a login that is a Claude plan runs it, since the
                // others may be billed per token.
                let ready =
                    tokio::task::spawn_blocking(move || crate::claude_code::check_plan(&cancel))
                        .await
                        .map_err(|error| Failure::Failed(error.to_string()))?
                        .map_err(|problem| Failure::Unavailable(format!("{problem:?}")))?;
                if !ready.on_plan() {
                    return Err(Failure::Unavailable(
                        "Claude Code isn't signed in with a Claude plan".to_string(),
                    ));
                }
                ask_claude_code(ready.path, &self.root, message).await
            }
            Way::ChatGptPlan => {
                let logins = (self.credentials)().logins;
                let (accounts, _) =
                    crate::logins::chatgpt_accounts(&logins, None).map_err(Failure::Unavailable)?;
                let client = LLMClient::new()
                    .with_chatgpt_accounts(accounts)
                    .with_stream_idle_timeout(STALL);
                ask_model(&client, "luna", message, false).await
            }
            Way::ClaudeKey => {
                let key = (self.credentials)()
                    .anthropic_key
                    .ok_or(missing("Anthropic key"))?;
                let client = LLMClient::new()
                    .with_anthropic_api_key(&key)
                    .with_stream_idle_timeout(STALL);
                ask_model(&client, "haiku", message, true).await
            }
            Way::OpenAiKey => {
                let key = (self.credentials)()
                    .openai_key
                    .ok_or(missing("OpenAI key"))?;
                let client = LLMClient::new()
                    .with_openai_api_key(&key)
                    .with_stream_idle_timeout(STALL);
                ask_model(&client, "luna", message, true).await
            }
        }
    }
}

/// One low-effort request to `model` (a name `resolve_model` takes) on
/// `client`. `limited` sends a limit on the answer's length, which the
/// ChatGPT backend refuses. The answer is streamed, so the client's idle
/// timeout (see [`STALL`]) ends a request that goes silent, and nothing
/// else ends one that makes progress.
pub(crate) async fn ask_model(
    client: &LLMClient,
    model: &str,
    message: &str,
    limited: bool,
) -> Result<String, Failure> {
    let failed = |error: String| Failure::Failed(error);
    let model = providers::resolve_model(model).map_err(failed)?;
    let mut generation = providers::generation(&model, crate::Effort::Low, false, false, false)
        .map_err(failed)?
        .with_prompt_caching(false);
    if limited {
        generation = generation.with_max_tokens(MAX_TOKENS);
    }
    let mut conversation = Conversation::new(model).with_system_message(PROMPT);
    conversation.add_user_message(message);
    let failed = |error: arko_ai::Error| Failure::Failed(error.to_string());
    let mut stream = client
        .generate_stream(&conversation, generation)
        .await
        .map_err(failed)?;
    while let Some(event) = stream.next().await {
        if let StreamEvent::End(response) = event.map_err(failed)? {
            return Ok(response.text());
        }
    }
    Err(Failure::Failed(
        "the response ended before it was whole".to_string(),
    ))
}

/// One low-effort turn in the Claude Code at `cli`, on Haiku 5.5 and the
/// user's Claude plan.
async fn ask_claude_code(cli: PathBuf, directory: &Path, message: &str) -> Result<String, Failure> {
    let options = claude_code_options(cli, directory, crate::claude_code::plan_only_env_here())?;
    run_claude_code(options, message, STALL).await
}

/// How Claude Code runs for a name. It has no tools, none of the user's
/// settings or MCP servers and saves no transcript. The variables `env`
/// sets (empty, for the ones that could bill it or sign it in elsewhere, see
/// `claude_code::plan_only_env`) keep it on the Claude plan.
fn claude_code_options(
    cli: PathBuf,
    directory: &Path,
    env: std::collections::BTreeMap<String, String>,
) -> Result<cc::ClaudeOptions, Failure> {
    let model = providers::resolve_model("cc:haiku").map_err(Failure::Failed)?;
    Ok(cc::ClaudeOptions {
        cli_path: Some(cli),
        working_directory: Some(directory.to_path_buf()),
        model: Some(model.id),
        effort: Some(cc::Effort::Low),
        system_prompt: cc::SystemPrompt::Replace(PROMPT.to_string()),
        built_in_tools: cc::BuiltInTools::None,
        max_turns: Some(1),
        setting_sources: Some(Vec::new()),
        env: env.into_iter().collect(),
        extra_args: vec![
            "--strict-mcp-config".into(),
            "--no-session-persistence".into(),
        ],
        ..cc::ClaudeOptions::default()
    })
}

/// Sends `message` as the one turn of a Claude Code run with `options`, and
/// returns its text. The run fails if it sends nothing for `stall`, and
/// nothing else ends one that goes on. The process ends with the run, or
/// when this is dropped.
async fn run_claude_code(
    options: cc::ClaudeOptions,
    message: &str,
    stall: Duration,
) -> Result<String, Failure> {
    let failed = |error: cc::ClaudeError| Failure::Failed(error.to_string());
    let mut session = cc::ClaudeSession::start(options).await.map_err(failed)?;
    session.send(message).await.map_err(failed)?;
    let result = loop {
        match tokio::time::timeout(stall, session.next_message()).await {
            Ok(Some(cc::Message::Result(result))) => break result,
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(Failure::Failed(
                    "Claude Code exited before it answered".to_string(),
                ));
            }
            Err(_) => {
                return Err(Failure::Failed(
                    "Claude Code sent nothing for a long time".to_string(),
                ));
            }
        }
    };
    let _ = session.close().await;
    match result.result {
        Some(text) if !result.is_error => Ok(text),
        _ => Err(Failure::Failed(format!(
            "Claude Code ended with {}",
            result.subtype
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::session_names::naming::clean;

    /// A request to a model by API key or the ChatGPT login is a low-effort
    /// one with the naming prompt and the message, and its answer comes back.
    #[tokio::test]
    async fn a_model_is_asked_at_low_effort_with_the_prompt_and_the_message() {
        use crate::mock_model::{ModelServer, reply};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let server = ModelServer::start(Arc::new({
            let seen = seen.clone();
            move |index, body| {
                seen.lock().unwrap().push(body);
                let _ = index;
                Box::pin(async move { reply("1", "\"Flaky scroll test.\"") })
            }
        }))
        .await;
        for (client, limited) in [(server.client(), true), (server.chatgpt_client(), false)] {
            let answer = ask_model(&client, "luna", "Fix the flaky scroll test", limited)
                .await
                .unwrap();
            assert_eq!(clean(&answer).as_deref(), Some("Flaky scroll test"));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for body in seen.iter() {
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "gpt-6-luna");
            assert_eq!(body["stream"], true, "streamed, so a silent one is cut off");
            assert_eq!(body["reasoning"]["effort"], "low", "{body}");
            let text = body.to_string();
            assert!(text.contains("You name conversations"), "{text}");
            assert!(text.contains("Fix the flaky scroll test"), "{text}");
        }
        let limit = |body: &str| body.contains("max_output_tokens");
        assert!(limit(&seen[0]), "an API key's request limits the answer");
        assert!(!limit(&seen[1]), "the ChatGPT backend takes no limit");
    }

    fn nothing() -> Arc<dyn Fn() -> Credentials + Send + Sync> {
        Arc::new(|| Credentials {
            logins: Vec::new(),
            anthropic_key: None,
            openai_key: None,
        })
    }

    /// A key or login that isn't there makes the way unavailable.
    #[tokio::test]
    async fn the_live_ways_that_lack_what_they_need_are_unavailable() {
        let live = Live::with(&std::env::temp_dir(), nothing());
        for way in [Way::ClaudeKey, Way::OpenAiKey] {
            let result = live.ask(way, "Hello").await;
            assert!(
                matches!(result, Err(Failure::Unavailable(_))),
                "{way:?} {result:?}"
            );
        }
    }

    /// A key is read as each way is asked, so one removed while an earlier
    /// way waited is not sent.
    #[tokio::test]
    async fn a_key_is_read_when_its_way_is_asked() {
        let key = Arc::new(Mutex::new(Some("sk-ant-test".to_string())));
        let live = Live::with(&std::env::temp_dir(), {
            let key = key.clone();
            Arc::new(move || Credentials {
                logins: Vec::new(),
                anthropic_key: key.lock().unwrap().clone(),
                openai_key: None,
            })
        });
        // Removed before the way is asked, it is unavailable.
        key.lock().unwrap().take();
        let result = live.ask(Way::ClaudeKey, "Hello").await;
        assert!(matches!(result, Err(Failure::Unavailable(_))), "{result:?}");
    }

    /// Claude Code is started for a name with Haiku 5.5 at low effort, no
    /// tools, none of the user's settings, no transcript saved and the
    /// variables that could bill it or sign it in elsewhere empty, one turn,
    /// and its text comes back.
    #[tokio::test]
    async fn claude_code_is_run_for_one_low_effort_turn_without_tools() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path();
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        let inherited = [
            "PATH",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_USE_BEDROCK",
        ]
        .map(String::from);
        let env = crate::claude_code::plan_only_env(inherited);
        let mut options = claude_code_options(python.into(), fake, env).unwrap();
        options.cli_prefix_args = vec![
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake_claude_agent.py"
            )
            .into(),
        ];
        for (name, file) in [
            ("FAKE_CLAUDE_SCRIPT", "script.json"),
            ("FAKE_CLAUDE_LOG", "log.jsonl"),
            ("FAKE_CLAUDE_STATE", "state"),
        ] {
            options
                .env
                .insert(name.into(), fake.join(file).display().to_string());
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            assert_eq!(
                options.env.get(name).map(String::as_str),
                Some(""),
                "{name}"
            );
        }
        assert!(!options.env.contains_key("PATH"));
        std::fs::write(
            fake.join("script.json"),
            r#"[[{"text": "\"Flaky scroll test.\""}], [{"fail": "overloaded"}]]"#,
        )
        .unwrap();
        let answer = run_claude_code(options.clone(), "Fix the flaky scroll test", STALL).await;
        assert_eq!(answer.as_deref(), Ok("\"Flaky scroll test.\""));
        let log: Vec<serde_json::Value> = std::fs::read_to_string(fake.join("log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let argv: Vec<String> = serde_json::from_value(
            log.iter().find(|entry| entry["event"] == "argv").unwrap()["argv"].clone(),
        )
        .unwrap();
        let after = |flag: &str| {
            let at = argv.iter().position(|arg| arg == flag).unwrap();
            argv[at + 1].clone()
        };
        assert_eq!(after("--model"), "claude-haiku-5-5");
        assert_eq!(after("--effort"), "low");
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--max-turns"), "1");
        assert_eq!(after("--system-prompt"), PROMPT);
        for flag in [
            "--setting-sources=",
            "--strict-mcp-config",
            "--no-session-persistence",
        ] {
            assert!(argv.iter().any(|arg| arg == flag), "{flag} in {argv:?}");
        }
        let sent: Vec<&serde_json::Value> = log
            .iter()
            .filter(|entry| entry["event"] == "user")
            .collect();
        assert_eq!(sent.len(), 1, "{log:?}");
        assert!(sent[0].to_string().contains("Fix the flaky scroll test"));
        // A turn that ends in an error is a failure.
        let answer = run_claude_code(options, "Again", STALL).await;
        assert!(matches!(answer, Err(Failure::Failed(_))), "{answer:?}");
    }

    /// A Claude Code run that is still sending is not cut off, however long
    /// it takes. One that sends nothing for the stall time fails, so the
    /// next way is asked.
    #[tokio::test]
    async fn claude_code_that_sends_nothing_for_a_long_time_fails() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path();
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        let mut options = claude_code_options(python.into(), fake, Default::default()).unwrap();
        options.cli_prefix_args = vec![
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake_claude_agent.py"
            )
            .into(),
        ];
        for (name, file) in [
            ("FAKE_CLAUDE_SCRIPT", "script.json"),
            ("FAKE_CLAUDE_LOG", "log.jsonl"),
            ("FAKE_CLAUDE_STATE", "state"),
        ] {
            options
                .env
                .insert(name.into(), fake.join(file).display().to_string());
        }
        // The first run is silent for 3 s, the second answers after steps
        // 0.3 s apart.
        std::fs::write(
            fake.join("script.json"),
            r#"[[{"sleep": 3}, {"text": "Late name"}], [{"text": "Step one"}, {"sleep": 0.3}, {"text": "Step two"}, {"sleep": 0.3}, {"text": "Slow name"}]]"#,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let stalled = run_claude_code(options.clone(), "Hello", Duration::from_millis(500)).await;
        assert!(matches!(stalled, Err(Failure::Failed(_))), "{stalled:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cut off at the stall"
        );
        // Longer in all than the stall time, never silent for it.
        let started = std::time::Instant::now();
        let slow = run_claude_code(options, "Hello", Duration::from_millis(500)).await;
        assert_eq!(slow.as_deref(), Ok("Slow name"));
        assert!(started.elapsed() >= Duration::from_millis(600));
    }

    /// Asks the real Claude Code on the user's Claude plan, which costs one
    /// small request.
    #[tokio::test]
    #[ignore = "runs the installed Claude Code on the user's Claude plan"]
    async fn the_installed_claude_code_names_a_conversation() {
        let live = Live::with(&std::env::temp_dir(), nothing());
        let answer = live
            .ask(
                Way::ClaudePlan,
                "Fix the flaky scroll test in the sidebar before the release",
            )
            .await
            .unwrap();
        println!("answer: {answer:?}");
        let name = clean(&answer).expect("a name");
        println!("name: {name:?}");
        assert!(name.split_whitespace().count() <= 5);
    }
}
