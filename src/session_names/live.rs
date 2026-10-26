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

/// Checks that Claude Code is there and signed in, ending at once when its
/// flag is set.
type ClaudeCheck = Arc<
    dyn Fn(&AtomicBool) -> Result<crate::claude_code::Ready, crate::claude_code::Problem>
        + Send
        + Sync,
>;

/// Asks the models for real.
pub(crate) struct Live {
    /// Arko's folder, where Claude Code runs (it reads nothing there).
    root: PathBuf,
    credentials: Arc<dyn Fn() -> Credentials + Send + Sync>,
    /// How Claude Code is found and checked (tests give their own).
    claude: ClaudeCheck,
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
            claude: Arc::new(crate::claude_code::check_plan),
        }
    }

    /// Checks Claude Code with `check`, not by looking for it (tests).
    #[cfg(test)]
    pub(crate) fn checking_claude_with(mut self, check: ClaudeCheck) -> Self {
        self.claude = check;
        self
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
    async fn ask(
        &self,
        way: Way,
        message: &str,
        still: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<String, Failure> {
        let missing = |what: &str| Failure::Unavailable(format!("no {what}"));
        // A message goes to a provider only while its way is wanted.
        let turned_off = || Failure::Unavailable("turned off".to_string());
        match way {
            Way::ClaudePlan => {
                let stop = StopOnDrop::default();
                let cancel = stop.0.clone();
                let check = self.claude.clone();
                // Only a login that is a Claude plan runs it, since the
                // others may be billed per token.
                let ready = tokio::task::spawn_blocking(move || check(&cancel))
                    .await
                    .map_err(|error| Failure::Failed(error.to_string()))?
                    .map_err(|problem| Failure::Unavailable(format!("{problem:?}")))?;
                if !ready.on_plan() {
                    return Err(Failure::Unavailable(
                        "Claude Code isn't signed in with a Claude plan".to_string(),
                    ));
                }
                // The checks took a while. It may have been turned off.
                if !still() {
                    return Err(turned_off());
                }
                ask_claude_code(ready.path, &self.root, message, still).await
            }
            Way::ChatGptPlan => {
                let logins = (self.credentials)().logins;
                let (accounts, _) =
                    crate::logins::chatgpt_accounts(&logins, None).map_err(Failure::Unavailable)?;
                let client = LLMClient::new()
                    .with_chatgpt_accounts(accounts)
                    .with_stream_idle_timeout(STALL);
                if !still() {
                    return Err(turned_off());
                }
                ask_model(&client, "luna", message, false).await
            }
            Way::ClaudeKey => {
                let key = (self.credentials)()
                    .anthropic_key
                    .ok_or(missing("Anthropic key"))?;
                let client = LLMClient::new()
                    .with_anthropic_api_key(&key)
                    .with_stream_idle_timeout(STALL);
                if !still() {
                    return Err(turned_off());
                }
                ask_model(&client, "haiku", message, true).await
            }
            Way::OpenAiKey => {
                let key = (self.credentials)()
                    .openai_key
                    .ok_or(missing("OpenAI key"))?;
                let client = LLMClient::new()
                    .with_openai_api_key(&key)
                    .with_stream_idle_timeout(STALL);
                if !still() {
                    return Err(turned_off());
                }
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
async fn ask_claude_code(
    cli: PathBuf,
    directory: &Path,
    message: &str,
    still: &(dyn Fn() -> bool + Send + Sync),
) -> Result<String, Failure> {
    let options = claude_code_options(cli, directory, crate::claude_code::plan_only_env_here())?;
    run_claude_code(options, message, STALL, still).await
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
/// when this is dropped. The message is sent only if `still` says the way is
/// wanted once Claude Code has started, which takes a moment.
async fn run_claude_code(
    options: cc::ClaudeOptions,
    message: &str,
    stall: Duration,
    still: &(dyn Fn() -> bool + Send + Sync),
) -> Result<String, Failure> {
    let failed = |error: cc::ClaudeError| Failure::Failed(error.to_string());
    let mut session = cc::ClaudeSession::start(options).await.map_err(failed)?;
    if !still() {
        let _ = session.close().await;
        return Err(Failure::Unavailable("turned off".to_string()));
    }
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

    /// A way that is still wanted.
    fn wanted() -> bool {
        true
    }

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
            let result = live.ask(way, "Hello", &wanted).await;
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
        let result = live.ask(Way::ClaudeKey, "Hello", &wanted).await;
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
        let answer =
            run_claude_code(options.clone(), "Fix the flaky scroll test", STALL, &wanted).await;
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
        let answer = run_claude_code(options, "Again", STALL, &wanted).await;
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
        let stalled = run_claude_code(
            options.clone(),
            "Hello",
            Duration::from_millis(500),
            &wanted,
        )
        .await;
        assert!(matches!(stalled, Err(Failure::Failed(_))), "{stalled:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cut off at the stall"
        );
        // Longer in all than the stall time, never silent for it.
        let started = std::time::Instant::now();
        let slow = run_claude_code(options, "Hello", Duration::from_millis(500), &wanted).await;
        assert_eq!(slow.as_deref(), Ok("Slow name"));
        assert!(started.elapsed() >= Duration::from_millis(600));
    }

    /// Options that run the fake Claude Code in `fake`, whose script is
    /// `script` and whose log is `log.jsonl` there.
    fn fake_options(fake: &Path, script: &str) -> cc::ClaudeOptions {
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
        std::fs::write(fake.join("script.json"), script).unwrap();
        options
    }

    /// What the fake Claude Code logged, by event.
    fn logged(fake: &Path, event: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(fake.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|entry| entry["event"] == event)
            .collect()
    }

    /// A way turned off while Claude Code is being checked starts no Claude
    /// Code and sends no message, and one turned off while it starts sends
    /// none. (A request already sent is left to finish.)
    #[tokio::test]
    async fn a_way_turned_off_during_the_checks_sends_nothing() {
        use std::sync::atomic::AtomicBool;
        let temp = tempfile::tempdir().unwrap();
        // The checks end after the user turned the way off, and the program
        // they found would leave a mark if it ran.
        let program = temp.path().join("claude");
        let mark = temp.path().join("started");
        std::fs::write(
            &program,
            format!("#!/bin/sh\ntouch '{}'\nexec sleep 30\n", mark.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let checked = Arc::new(AtomicBool::new(false));
        let live = Live::with(temp.path(), nothing()).checking_claude_with({
            let (program, checked) = (program.clone(), checked.clone());
            Arc::new(move |cancel| {
                while !checked.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(crate::claude_code::Ready {
                    path: program.clone(),
                    method: Some("claude.ai".into()),
                    plan: None,
                })
            })
        });
        let wanted = Arc::new(AtomicBool::new(true));
        let still = {
            let wanted = wanted.clone();
            move || wanted.load(Ordering::Relaxed)
        };
        let (result, ()) = tokio::join!(live.ask(Way::ClaudePlan, "A message", &still), async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            wanted.store(false, Ordering::Relaxed);
            checked.store(true, Ordering::Relaxed);
        });
        assert!(matches!(result, Err(Failure::Unavailable(_))), "{result:?}");
        assert!(!mark.exists(), "Claude Code was not even started");

        // Turned off while Claude Code starts: it starts, gets no message,
        // and ends.
        let fake = tempfile::tempdir().unwrap();
        let options = fake_options(fake.path(), r#"[[{"text": "A name"}]]"#);
        let result = run_claude_code(options, "A message", STALL, &|| false).await;
        assert!(matches!(result, Err(Failure::Unavailable(_))), "{result:?}");
        assert_eq!(logged(fake.path(), "argv").len(), 1, "it started");
        assert!(
            logged(fake.path(), "user").is_empty(),
            "and was sent nothing"
        );
    }

    /// Whether the process `pid` is running (a process that ended and isn't
    /// collected yet isn't).
    #[cfg(target_os = "linux")]
    fn running(pid: u64) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().to_string())
            })
            .is_some_and(|rest| !rest.starts_with('Z'))
    }

    /// Asks the fake Claude Code in `options`, as a way.
    struct ClaudeAsk(cc::ClaudeOptions);

    #[async_trait]
    impl Ask for ClaudeAsk {
        async fn ask(
            &self,
            _way: Way,
            message: &str,
            still: &(dyn Fn() -> bool + Send + Sync),
        ) -> Result<String, Failure> {
            run_claude_code(self.0.clone(), message, STALL, still).await
        }
    }

    /// Quitting stops the namings that are going on (those in the Arko folder
    /// it names, here), and the Claude Code each runs ends with it, though
    /// nothing else would have stopped it. (`stop_all` is this for every
    /// folder, which tests running side by side can't share.)
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_all_namings_ends_the_claude_code_they_run() {
        use crate::session_names::naming::{Naming, Run, stop_in};
        let temp = tempfile::tempdir().unwrap();
        let fake = tempfile::tempdir().unwrap();
        // A Claude Code that is busy for half a minute after it is sent
        // the message.
        let options = fake_options(fake.path(), r#"[[{"sleep": 30}, {"text": "Late name"}]]"#);
        let server = crate::mock_model::ModelServer::start(Arc::new(|_, _| {
            Box::pin(async { crate::mock_model::complete("1", "Done.") })
        }))
        .await;
        let (harness, _inputs, _sender) = crate::tests::harness(temp.path(), server.client(), true);
        // A session's settings are saved as it opens, and say it is there.
        harness.session.save_settings(&harness.settings).unwrap();
        crate::session_names::save(&harness.root, &[Way::ClaudePlan]).unwrap();
        let naming = Naming::default();
        naming.start(Run {
            session: harness.session.clone(),
            gauges: harness.gauges.clone(),
            root: harness.root.clone(),
            text: "Count the pears.".into(),
            asker: Arc::new(ClaudeAsk(options)),
        });
        // It runs, and Claude Code has the message.
        tokio::time::timeout(Duration::from_secs(20), async {
            while logged(fake.path(), "user").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("Claude Code got the message");
        let pid = logged(fake.path(), "argv")[0]["pid"].as_u64().unwrap();
        assert!(running(pid), "Claude Code is busy");
        assert!(naming.running());
        // Quitting stops it, and its Claude Code is gone when that returns.
        let root = harness.root.clone();
        tokio::task::spawn_blocking(move || stop_in(&root))
            .await
            .unwrap();
        // (The kill is sent by then, and the process is gone a moment later.)
        tokio::time::timeout(Duration::from_secs(5), async {
            while running(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Claude Code ended with the naming");
        assert!(!naming.running());
        assert_eq!(harness.session.generated_name(), None);
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
                &wanted,
            )
            .await
            .unwrap();
        println!("answer: {answer:?}");
        let name = clean(&answer).expect("a name");
        println!("name: {name:?}");
        assert!(name.split_whitespace().count() <= 5);
    }
}
