// End-to-end tests of text replies: the main agent ends its turns with a
// reply in plain text, which fin shows once, as the reply. Also the default
// and flags that choose them, `/text-replies` and the settings, and sessions
// saved before them, whose main agent goes on calling `complete_task`. Each
// follows a path a user takes, with the mock model. (Included in `main`'s
// tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, reply, say_and_call, tool_call};
use tokio::sync::Notify;

/// The requests' bodies, as the model got them.
type Bodies = Arc<Mutex<Vec<String>>>;

/// The lines printed at the line prompt (or as progress, with -p).
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl rustyline::ExternalPrinter for Lines {
    fn print(&mut self, text: String) -> rustyline::Result<()> {
        let mut lines = self.0.lock().unwrap();
        lines.extend(text.lines().map(str::to_string));
        Ok(())
    }
}

impl Lines {
    /// How many printed lines are `text`.
    fn count(&self, text: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.trim() == text)
            .count()
    }
}

/// Prints `harness`'s output to `Lines` from now on: at the line prompt,
/// or (`one_shot`) as -p does, the progress there and the answer to
/// standard output. Its workflow is opened again with it.
fn recording(harness: &mut Harness, one_shot: bool) -> Lines {
    let lines = Lines::default();
    harness.output = Arc::new(Output::new(Some(Box::new(lines.clone())), one_shot));
    harness.open(false).unwrap();
    lines
}

/// Text replies on, before the conversation starts, as `/text-replies on`
/// turns them on.
fn replying(harness: &mut Harness) {
    harness.text_replies_command("on", Source::Terminal);
    assert!(harness.settings.text_replies);
    assert!(harness.main_config.tool_config.ends_task_with_text());
}

/// The feed's items of `kind`, oldest first.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

/// Whether the feed has a notice starting with `start`.
fn says(feed: &remote::Feed, start: &str) -> bool {
    texts(feed, "notice")
        .iter()
        .any(|text| text.starts_with(start))
}

/// What the harness's agents are built from, with its settings.
fn agent_settings_of(harness: &Harness) -> AgentSettings<'_> {
    AgentSettings {
        main: &harness.settings.main,
        worker: &harness.settings.worker,
        max_workers: harness.settings.max_workers,
        handover_at: harness.settings.handover_at,
        show_thinking: false,
        providers: Box::leak(Box::new(providers::Setup::new(
            harness.settings.openai_auth,
        ))),
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
        text_replies: harness.settings.text_replies,
    }
}

/// How many of the feed's items, of any kind, are `text`.
fn items_with(feed: &remote::Feed, text: &str) -> usize {
    shown(feed)
        .iter()
        .filter(|(_, shown, _)| shown == text)
        .count()
}

/// The names of the tools a request offered.
fn tools_of(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn args(words: &[&str]) -> Args {
    Args::parse_from(std::iter::once("fin").chain(words.iter().copied()))
}

/// Text replies are on by default where the user takes part (the full
/// screen and the line prompt in a terminal, the web page), and off with
/// -p and with lines piped in. Either flag overrides the default, and they
/// can't be given together.
#[test]
fn the_default_follows_whether_the_user_takes_part_and_the_flags_override_it() {
    assert!(text_replies_for(&args(&[]), true), "the full screen");
    assert!(text_replies_for(&args(&["--repl"]), true), "the line prompt");
    assert!(text_replies_for(&args(&["--remote"]), false), "the web page");
    assert!(!text_replies_for(&args(&["-p", "Hi"]), true), "-p");
    assert!(!text_replies_for(&args(&[]), false), "lines piped in");

    assert!(!text_replies_for(&args(&["--no-text-replies"]), true));
    assert!(!text_replies_for(&args(&["--repl", "--no-text-replies"]), true));
    assert!(!text_replies_for(&args(&["--remote", "--no-text-replies"]), false));
    assert!(text_replies_for(&args(&["-p", "Hi", "--text-replies"]), true));
    assert!(text_replies_for(&args(&["--text-replies"]), false));
    assert!(
        Args::try_parse_from(["fin", "--text-replies", "--no-text-replies"]).is_err(),
        "one or the other"
    );
}

/// The session keeps the mode in its settings, and a session saved before
/// there was one loads with it off, as its saved agents are.
#[test]
fn the_mode_is_saved_with_the_session_and_old_sessions_load_with_it_off() {
    let temp = tempfile::tempdir().unwrap();
    let session = Session::create(temp.path()).unwrap();
    let model = ModelSettings {
        model: resolve_model("luna").unwrap().id,
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let mut settings = Settings {
        workspace: temp.path().to_path_buf(),
        main: model.clone(),
        worker: model,
        max_workers: NO_LIMIT,
        handover_at: 200_000,
        main_handover_at: None,
        allow_outside_workspace: false,
        show_thinking: false,
        created: chrono::Utc::now(),
        title: None,
        windows_shell: None,
        openai_auth: None,
        own_worker_model: false,
        text_replies: true,
    };
    session.save_settings(&settings).unwrap();
    assert!(session.load_settings().unwrap().text_replies);

    // Off is left out, as sessions saved before it have it.
    settings.text_replies = false;
    session.save_settings(&settings).unwrap();
    let file = session.dir.join("session.json");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(saved.get("text_replies").is_none(), "{saved}");
    assert!(!session.load_settings().unwrap().text_replies);
}

/// A reply in text ends the turn, with the reply as the turn's answer. The
/// agent is offered neither `complete_task` nor `message_user`, and its
/// instructions say how its turn ends. The reply shows once, as the reply:
/// in the feed (the full screen and the web page), and at the line prompt.
#[tokio::test]
async fn a_reply_in_text_ends_the_turn_and_shows_once() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { reply("m1", "17 * 23 is 391.") })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    replying(&mut harness);
    let lines = recording(&mut harness, false);
    let end = harness
        .turn(Prompt::User("What is 17 * 23?".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "17 * 23 is 391.");
    assert_eq!(bodies.lock().unwrap().len(), 1, "one request");

    let body = bodies.lock().unwrap()[0].clone();
    let tools = tools_of(&body);
    assert!(tools.contains(&ASK_USER_TOOL_NAME.to_string()), "{tools:?}");
    for absent in ["complete_task", message::MESSAGE_USER] {
        assert!(!tools.contains(&absent.to_string()), "{absent}: {tools:?}");
    }
    assert!(body.contains("When you're done, reply without calling a tool."));
    assert!(!body.contains("message_user"));

    // Once in the feed, as the reply.
    assert_eq!(items_with(&harness.feed, "17 * 23 is 391."), 1);
    assert_eq!(texts(&harness.feed, "reply"), ["17 * 23 is 391."]);
    // Once at the line prompt.
    assert_eq!(lines.count("17 * 23 is 391."), 1);
}

/// With -p and text replies, the reply isn't printed as progress: standard
/// output has it, once.
#[tokio::test]
async fn with_p_the_reply_is_not_printed_as_progress() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { reply("m1", "17 * 23 is 391.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
    replying(&mut harness);
    let progress = recording(&mut harness, true);
    let end = harness
        .turn(Prompt::User("What is 17 * 23?".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "17 * 23 is 391.");
    harness.flush_printer().await;
    assert_eq!(progress.count("17 * 23 is 391."), 0);
}

/// Text beside a call is progress: the turn goes on. What the user types
/// meanwhile reaches the agent as a note, and it answers in text beside its
/// next call, also progress, and the turn ends with its reply.
#[tokio::test]
async fn text_beside_a_call_is_progress_and_answers_a_note_mid_turn() {
    let bodies: Bodies = Arc::default();
    let go = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, go) = (bodies.clone(), go.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let go = go.clone();
            Box::pin(async move {
                let list = || serde_json::json!({"path": "."});
                if body.contains("call_m2") {
                    reply("m3", "Done: the workspace is empty.")
                } else if body.contains("call_m1") {
                    say_and_call("m2", "Halfway: the files are listed.", "list_files", list())
                } else {
                    go.notified().await;
                    say_and_call("m1", "Listing the files.", "list_files", list())
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    replying(&mut harness);
    let lines = recording(&mut harness, false);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        sender.send(Input::Line("progress?".into())).unwrap();
        until(|| has(&feed, "user", "progress?", "note")).await;
        go.notify_one();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("What's in the workspace?".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done: the workspace is empty.");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "the text beside the calls didn't end the turn");
    assert!(bodies[1].contains("progress?"), "the note reached the agent");

    let feed = &harness.feed;
    assert_eq!(
        texts(feed, "text"),
        ["Listing the files.", "Halfway: the files are listed."]
    );
    assert_eq!(texts(feed, "reply"), ["Done: the workspace is empty."]);
    assert_eq!(items_with(feed, "Done: the workspace is empty."), 1);
    harness.flush_printer().await;
    for line in [
        "Listing the files.",
        "Halfway: the files are listed.",
        "Done: the workspace is empty.",
    ] {
        assert_eq!(lines.count(line), 1, "{line}");
    }
}

/// With text replies, workers still report with `complete_task`: the lead
/// starts one, waits for its report, and replies in text.
#[tokio::test]
async fn workers_report_with_complete_task_and_the_lead_replies_in_text() {
    let worker_tools: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let worker_tools = worker_tools.clone();
        move |_, body: String| {
            if body.contains("You are agent run r1") {
                worker_tools.lock().unwrap().push(body.clone());
            }
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    complete("w1", "There are 3 files.")
                } else if body.contains("There are 3 files.") {
                    reply("m3", "Final: 3 files.")
                } else if body.contains("Started r1") {
                    tool_call(
                        "m2",
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 10}),
                    )
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    replying(&mut harness);
    assert!(!harness.worker_config.tool_config.ends_task_with_text());
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("How many files?".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Final: 3 files.");
    let worker = worker_tools.lock().unwrap()[0].clone();
    assert!(tools_of(&worker).contains(&"complete_task".to_string()));
    let (snapshot, _) = harness.feed.subscribe();
    assert!(snapshot.items.iter().any(|item| item.agent == "r1"
        && item.kind == remote::Kind::Reply
        && item.text == "There are 3 files."));
    let lead_replies: Vec<&str> = snapshot
        .items
        .iter()
        .filter(|item| item.is_main() && item.kind == remote::Kind::Reply)
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(lead_replies, ["Final: 3 files."]);
    assert_eq!(
        snapshot
            .items
            .iter()
            .filter(|item| item.text == "Final: 3 files.")
            .count(),
        1
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/text-replies` says how the main agent ends its turns, and before the
/// conversation starts turns text replies on or off: the agents are built
/// again, and the session and the settings (the full screen's, the web
/// page's) have it. Once the conversation has started, the main agent keeps
/// it for the session, and fin says so.
#[tokio::test]
async fn text_replies_change_before_the_conversation_starts_only() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { reply("m1", "Hello.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let reading = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref());
    harness.publish_models();
    assert!(!reading(&harness).settings.text_replies);

    harness.text_replies_command("", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Text replies are off: the agent ends its turn by calling complete_task. /text-replies on changes that before the conversation starts."
            .to_string()
    ));
    harness.text_replies_command("on", Source::Terminal);
    assert!(
        texts(&harness.feed, "output")
            .contains(&"Text replies are on: the agent ends its turn with a plain reply.".to_string())
    );
    assert!(harness.settings.text_replies);
    assert!(harness.main_config.tool_config.ends_task_with_text());
    assert!(harness.session.load_settings().unwrap().text_replies);
    let now = reading(&harness);
    assert!(now.settings.text_replies);
    assert!(!now.models.started);
    harness.text_replies_command("sometimes", Source::Terminal);
    assert!(
        texts(&harness.feed, "output")
            .contains(&"/text-replies takes on or off (/text-replies off).".to_string())
    );

    let end = harness
        .turn(Prompt::User("Hi.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Hello.");
    harness.text_replies_command("off", Source::Web);
    assert!(says(
        &harness.feed,
        "From the web page: The main agent keeps text replies on for this session: the agent ends its turn with a plain reply. A new session takes --text-replies or --no-text-replies."
    ));
    assert!(harness.settings.text_replies);
    assert!(harness.session.load_settings().unwrap().text_replies);
    harness.text_replies_command("", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Text replies are on: the agent ends its turn with a plain reply. It keeps this for the session."
            .to_string()
    ));
    let now = reading(&harness);
    assert!(now.settings.text_replies);
    assert!(now.models.started);
}

/// A session with text replies keeps them when it is resumed: the agent's
/// saved configuration ends its turns with a reply, and the conversation
/// shows its replies as replies again.
#[tokio::test]
async fn a_resumed_session_keeps_its_text_replies() {
    let server = ModelServer::start(Arc::new(|_, body: String| {
        Box::pin(async move {
            if body.contains("Which fruit?") {
                reply("m2", "Apples.")
            } else {
                reply("m1", "Apples noted.")
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    replying(&mut harness);
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Apples noted.");

    // fin closes, and the session is resumed with its saved settings.
    harness.settings = harness.session.load_settings().unwrap();
    assert!(harness.settings.text_replies);
    harness.open(true).unwrap();
    assert!(harness.unoffered.is_none(), "{:?}", harness.unoffered);
    harness.banner_end(false);
    // The history shows the reply as a reply (after the turn's own).
    assert_eq!(texts(&harness.feed, "reply"), ["Apples noted.", "Apples noted."]);
    assert!(!texts(&harness.feed, "text").contains(&"Apples noted.".to_string()));
    harness
        .turn(Prompt::User("Which fruit?".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Apples.");
}

/// A session saved before text replies (its main agent calls
/// `complete_task` and has `message_user`) resumes with its saved
/// configuration, also where a new session would reply in text, and goes
/// on through `complete_task`, its `message_user` still offered.
#[tokio::test]
async fn a_session_saved_before_text_replies_resumes_with_complete_task() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("call_m2") {
                    complete("m3", "Pears noted.")
                } else if body.contains("Pears are green.") {
                    tool_call(
                        "m2",
                        message::MESSAGE_USER,
                        serde_json::json!({"message": "Still with you."}),
                    )
                } else {
                    complete("m1", "Apples noted.")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    // The harness's agents are as before text replies.
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(!harness.settings.text_replies);
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Apples noted.");

    // fin now builds its main agent with text replies, and the session is
    // resumed: its saved configuration stands.
    let settings = AgentSettings {
        text_replies: true,
        ..agent_settings_of(&harness)
    };
    harness.main_config = agents::main_config(&settings).unwrap();
    assert!(harness.main_config.tool_config.ends_task_with_text());
    harness.open(true).unwrap();
    assert!(harness.unoffered.is_none(), "{:?}", harness.unoffered);
    harness
        .turn(Prompt::User("Pears are green.".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Pears noted.");
    let body = bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| body.contains("Pears are green."))
        .unwrap()
        .clone();
    let tools = tools_of(&body);
    for offered in ["complete_task", message::MESSAGE_USER] {
        assert!(tools.contains(&offered.to_string()), "{offered}: {tools:?}");
    }
    assert!(has(&harness.feed, "message", "Still with you.", ""));
    assert_eq!(texts(&harness.feed, "reply").last().unwrap(), "Pears noted.");
}

/// The fresh configuration a resumed session with text replies would have
/// without them isn't a reason to say it lacks `message_user`: it answers
/// in text.
#[test]
fn a_text_reply_session_is_not_told_it_lacks_message_user() {
    let model = ModelSettings {
        model: resolve_model("luna").unwrap().id,
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let settings = |text_replies| AgentSettings {
        main: &model,
        worker: &model,
        providers: Box::leak(Box::new(providers::Setup::new(None))),
        max_workers: NO_LIMIT,
        handover_at: 200_000,
        show_thinking: false,
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
        text_replies,
    };
    let replying = agents::main_config(&settings(true)).unwrap();
    let calling = agents::main_config(&settings(false)).unwrap();
    let saved = SavedSetup {
        name: WORKFLOW_NAME.into(),
        configs: [(MAIN.to_string(), replying.clone())].into(),
    };
    assert_eq!(unoffered(&saved, &[(MAIN, &calling)]), None);
    let saved = SavedSetup {
        name: WORKFLOW_NAME.into(),
        configs: [(MAIN.to_string(), calling)].into(),
    };
    assert_eq!(unoffered(&saved, &[(MAIN, &replying)]), None);
}
