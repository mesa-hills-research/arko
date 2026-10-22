// End-to-end tests of text replies: the main agent ends its turns with a
// reply in plain text, which Arko shows once, as the reply. Also the default
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

impl crate::ui::Printer for Lines {
    fn print(&mut self, text: String) -> Result<(), ()> {
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

fn args(words: &[&str], terminal: bool) -> Options {
    Args::parse_from(std::iter::once("arko").chain(words.iter().copied()))
        .options_at(terminal)
        .unwrap()
}

/// Text replies are on by default where the user takes part (the full
/// screen and the line prompt in a terminal, the web page), and off with
/// -p and with lines piped in. Either flag overrides the default, and they
/// can't be given together.
#[test]
fn the_default_follows_whether_the_user_takes_part_and_the_flags_override_it() {
    assert!(text_replies_for(&args(&[], true)), "the full screen");
    assert!(text_replies_for(&args(&["--repl"], true)), "the line prompt");
    assert!(text_replies_for(&args(&["--remote"], false)), "the web page");
    assert!(!text_replies_for(&args(&["-p", "Hi"], true)), "-p");
    assert!(!text_replies_for(&args(&[], false)), "lines piped in");

    assert!(!text_replies_for(&args(&["--no-text-replies"], true)));
    assert!(!text_replies_for(&args(&["--repl", "--no-text-replies"], true)));
    assert!(!text_replies_for(&args(&["--remote", "--no-text-replies"], false)));
    assert!(text_replies_for(&args(&["-p", "Hi", "--text-replies"], true)));
    assert!(text_replies_for(&args(&["--text-replies"], false)));
    assert!(
        Args::try_parse_from(["arko", "--text-replies", "--no-text-replies"]).is_err(),
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
        text_replies: true,
        ..Settings::for_test(temp.path(), &model)
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
/// agent is offered `message_user` and no `complete_task`, and its
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
    for offered in [ASK_USER_TOOL_NAME, message::MESSAGE_USER] {
        assert!(tools.contains(&offered.to_string()), "{offered}: {tools:?}");
    }
    assert!(
        !tools.contains(&"complete_task".to_string()),
        "no complete_task: {tools:?}"
    );
    assert!(body.contains("When you're done, reply without calling a tool."));
    assert!(body.contains("answer it with `message_user` and carry on with your work"));

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

/// With text replies the agent answers mid-turn with `message_user`, as
/// without them: the message shows now, in its order among the agent's text
/// and calls, the turn goes on, and it ends with a plain reply. Text beside
/// a `wait_for_agents` call shows too, before the waiting row, and nothing
/// shows twice.
#[tokio::test]
async fn message_user_answers_mid_turn_and_the_turn_ends_with_a_reply() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("call_m2") {
                    reply("m3", "Done: nothing left to do.")
                } else if body.contains("call_m1") {
                    say_and_call(
                        "m2",
                        "Waiting for the check.",
                        crate::crew::WAIT_FOR_AGENTS,
                        serde_json::json!({}),
                    )
                } else {
                    say_and_call(
                        "m1",
                        "Looking at it.",
                        message::MESSAGE_USER,
                        serde_json::json!({"message": "Halfway: two files to go."}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    replying(&mut harness);
    let lines = recording(&mut harness, false);
    let end = harness
        .turn(Prompt::User("Check the files.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done: nothing left to do.");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "the message didn't end the turn");
    assert!(tools_of(&bodies[0]).contains(&message::MESSAGE_USER.to_string()));
    assert!(bodies[1].contains("The user has your message. Carry on with your work."));

    // In the feed, in the order the agent wrote them.
    let order: Vec<(String, String)> = shown(&harness.feed)
        .into_iter()
        .map(|(kind, text, _)| (kind, text))
        .filter(|(kind, _)| ["text", "message", "call", "reply"].contains(&kind.as_str()))
        .collect();
    let item = |kind: &str, text: &str| (kind.to_string(), text.to_string());
    assert_eq!(
        order,
        [
            item("text", "Looking at it."),
            item("message", "Halfway: two files to go."),
            item("text", "Waiting for the check."),
            item("call", "waiting for a message"),
            item("reply", "Done: nothing left to do."),
        ]
    );
    assert_eq!(items_with(&harness.feed, "Halfway: two files to go."), 1);
    assert_eq!(items_with(&harness.feed, "Done: nothing left to do."), 1);
    harness.flush_printer().await;
    for line in [
        "Looking at it.",
        "✉ Halfway: two files to go.",
        "Waiting for the check.",
        "Done: nothing left to do.",
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
/// it for the session, and Arko says so.
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
        &"Text replies are off, so the agent ends its turn by calling complete_task. /text-replies on changes that before the conversation starts."
            .to_string()
    ));
    harness.text_replies_command("on", Source::Terminal);
    assert!(
        texts(&harness.feed, "output")
            .contains(&"Text replies are on, so the agent ends its turn with a plain reply.".to_string())
    );
    assert!(harness.settings.text_replies);
    assert!(harness.main_config.tool_config.ends_task_with_text());
    assert!(harness.session.load_settings().unwrap().text_replies);
    let now = reading(&harness);
    assert!(now.settings.text_replies);
    assert!(!now.models.started);
    harness.text_replies_command("on", Source::Web);
    assert!(says(
        &harness.feed,
        "From the web page. Text replies are on already, so the agent ends its turn with a plain reply."
    ));
    harness.text_replies_command("off", Source::Web);
    assert!(says(
        &harness.feed,
        "Changed from the web page. Text replies are off, so the agent ends its turn by calling complete_task."
    ));
    harness.text_replies_command("on", Source::Terminal);
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
        "From the web page. The main agent keeps text replies on for this session, so the agent ends its turn with a plain reply. A new session takes --text-replies or --no-text-replies."
    ));
    assert!(harness.settings.text_replies);
    assert!(harness.session.load_settings().unwrap().text_replies);
    harness.text_replies_command("", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Text replies are on, so the agent ends its turn with a plain reply. It keeps this for the session."
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

    // Arko closes, and the session is resumed with its saved settings.
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

    // Arko now builds its main agent with text replies, and the session is
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

/// The main agent has `message_user` with text replies and without. A
/// session saved with text replies before it did resumes with its saved
/// configuration, which has none, and is told so. One saved after, and one
/// saved without text replies, miss nothing.
#[test]
fn a_session_saved_before_text_replies_had_message_user_is_told_it_lacks_it() {
    let model = ModelSettings {
        model: resolve_model("luna").unwrap().id,
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let settings = |text_replies| AgentSettings {
        max_workers: NO_LIMIT,
        text_replies,
        ..AgentSettings::for_test(&model, &model)
    };
    let replying = agents::main_config(&settings(true)).unwrap();
    let calling = agents::main_config(&settings(false)).unwrap();
    for config in [&replying, &calling] {
        assert!(
            config
                .tool_config
                .available_tool_names
                .contains(&message::MESSAGE_USER.to_string())
        );
    }
    let saved = |config: &arko_agents::AgentConfig| SavedSetup {
        name: WORKFLOW_NAME.into(),
        configs: [(MAIN.to_string(), config.clone())].into(),
    };
    assert_eq!(unoffered(&saved(&replying), &[(MAIN, &calling)]), None);
    assert_eq!(unoffered(&saved(&calling), &[(MAIN, &replying)]), None);
    assert_eq!(unoffered(&saved(&replying), &[(MAIN, &replying)]), None);
    let before = without_message_user(&replying);
    assert_eq!(
        unoffered(&saved(&before), &[(MAIN, &replying)]).as_deref(),
        Some(
            "This session started without answers to you while it works (message_user), so its agents don't get them. A new session does."
        )
    );
}

/// `config` as a main agent with text replies was saved before it had
/// `message_user`.
fn without_message_user(config: &arko_agents::AgentConfig) -> arko_agents::AgentConfig {
    let mut config = config.clone();
    config
        .tool_config
        .available_tool_names
        .retain(|name| name != message::MESSAGE_USER);
    config
}

// ----- where the agent answers a note the user typed -----

/// What a context update with a note from the user ends with: for an agent
/// that has `message_user` (with text replies or without), and for one saved
/// with text replies before it had it.
const TOOL_HINT: &str = "Text you write only in your thinking may never reach the user, and a long wait is where that is most likely. Answer a question in the notes above with `message_user` first, even if they cut a wait short, then carry on with your work.";
const TEXT_HINT: &str = "Text you write only in your thinking may never reach the user, and a long wait is where that is most likely. Answer a question in the notes above in the text you write beside your next tool call.";

/// A model that notes a message at once, except "List the files.", where it
/// lists them once `go` is notified. It ends its turns the way the main
/// agent does with `text` replies. The requests it got are in the bodies.
async fn listing_model(text: bool) -> (ModelServer, Bodies, Arc<Notify>) {
    let bodies: Bodies = Arc::default();
    let go = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, go) = (bodies.clone(), go.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let go = go.clone();
            Box::pin(async move {
                let finish = |id, answer| {
                    if text {
                        reply(id, answer)
                    } else {
                        complete(id, answer)
                    }
                };
                if body.contains("call_m1") {
                    finish("m2", "Done.")
                } else if body.contains("List the files.") {
                    go.notified().await;
                    tool_call("m1", "list_files", serde_json::json!({"path": "."}))
                } else {
                    finish("m0", "Noted.")
                }
            })
        }
    }))
    .await;
    (server, bodies, go)
}

/// A turn in which the user types `note` while the agent works, and the
/// agent reads it at its next request. Returns the requests' bodies.
async fn turn_with_a_typed_note(
    harness: &mut Harness,
    inputs: &mut Inputs,
    sender: mpsc::UnboundedSender<Input>,
    (bodies, go): (Bodies, Arc<Notify>),
    note: &str,
) -> Vec<String> {
    let feed = harness.feed.clone();
    let note = note.to_string();
    let typing = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        sender.send(Input::Line(note.clone())).unwrap();
        until(|| has(&feed, "user", &note, "note")).await;
        go.notify_one();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("List the files.".into()), inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(harness), "Done.");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    bodies
}

/// A note the user types while the main agent works ends in a reminder to
/// answer where the user reads it, with `message_user`, with text replies
/// and without. The first request, before any note, has none.
#[tokio::test]
async fn a_note_the_user_types_comes_with_where_to_answer_it() {
    for text in [true, false] {
        let (server, bodies, go) = listing_model(text).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
        if text {
            replying(&mut harness);
        }
        let bodies =
            turn_with_a_typed_note(&mut harness, &mut inputs, sender, (bodies, go), "progress?")
                .await;
        let (hint, other) = (TOOL_HINT, TEXT_HINT);
        assert!(!bodies[0].contains(hint), "no note yet");
        assert!(!bodies[0].contains(other));
        assert_eq!(bodies[1].matches(hint).count(), 1, "text replies {text}");
        assert!(!bodies[1].contains(other), "text replies {text}");
        // After the note, in a paragraph of its own.
        let note = bodies[1].find("progress?").unwrap();
        assert!(bodies[1].find(hint).unwrap() > note);
    }
}

/// A note from Arko alone (the agents are told something), and a turn
/// without a note, have no such reminder.
#[tokio::test]
async fn the_reminder_goes_with_the_users_notes_only() {
    for text in [true, false] {
        let (server, bodies, go) = listing_model(text).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        if text {
            replying(&mut harness);
        }
        let workflow = harness.workflow().clone();
        let feed = harness.feed.clone();
        let telling = tokio::spawn(async move {
            until(|| feed.activity().working).await;
            workflow
                .post_note(Note::new("The build is slow.").source(NoteSource::Host))
                .unwrap();
            go.notify_one();
        });
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("List the files.".into()), &mut inputs),
        )
        .await
        .unwrap();
        telling.await.unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        let bodies = bodies.lock().unwrap().clone();
        assert!(bodies[1].contains("The build is slow."), "{text}");
        for body in &bodies {
            assert!(!body.contains("only in your thinking may never reach the user"), "{text}");
        }
    }
}

/// The reminder follows the configuration the session's main agent has,
/// also after a resume. A session saved with `message_user` (with text
/// replies or without) is reminded to answer with it. One saved with text
/// replies before the agent had it goes on with its saved configuration, is
/// offered no such tool, and is reminded to answer in text beside its calls,
/// also where a new session would have the tool.
#[tokio::test]
async fn the_reminder_follows_the_saved_configuration_after_a_resume() {
    // (saved with text replies, saved with `message_user`)
    for (saved_text, saved_tool) in [(true, true), (true, false), (false, true)] {
        let (server, bodies, go) = listing_model(saved_text).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
        if saved_text {
            replying(&mut harness);
        }
        if !saved_tool {
            harness.main_config = without_message_user(&harness.main_config);
            harness.open(false).unwrap();
        }
        harness
            .turn(Prompt::User("Hello.".into()), &mut inputs)
            .await;
        assert_eq!(summary(&harness), "Noted.");
        // Arko builds its main agent with text replies now, and the session
        // is resumed.
        harness.settings = harness.session.load_settings().unwrap();
        let settings = AgentSettings {
            text_replies: true,
            ..agent_settings_of(&harness)
        };
        harness.main_config = agents::main_config(&settings).unwrap();
        harness.open(true).unwrap();
        // What a session saved without the tool lacks, it says.
        assert_eq!(
            harness.unoffered.is_some(),
            !saved_tool,
            "{:?}",
            harness.unoffered
        );
        bodies.lock().unwrap().clear();
        let bodies =
            turn_with_a_typed_note(&mut harness, &mut inputs, sender, (bodies, go), "progress?")
                .await;
        let (hint, other) = if saved_tool {
            (TOOL_HINT, TEXT_HINT)
        } else {
            (TEXT_HINT, TOOL_HINT)
        };
        let case = format!("saved with text {saved_text}, with the tool {saved_tool}");
        assert_eq!(bodies[1].matches(hint).count(), 1, "{case}");
        assert!(!bodies[1].contains(other), "{case}");
        // The tool is offered only where the saved configuration has it.
        let tools = tools_of(&bodies[1]);
        assert_eq!(tools.contains(&message::MESSAGE_USER.to_string()), saved_tool, "{case}: {tools:?}");
    }
}

/// Without a terminal (with -p) what the user types waits for a turn of its
/// own and no note comes, so the agent has no reminder to answer one. Nor
/// does a chat, which has no calls to answer beside.
#[tokio::test]
async fn the_reminder_is_for_sessions_with_a_terminal_and_agents_with_calls() {
    let (server, bodies, go) = listing_model(false).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
    let hint = |text_replies: bool, mode: Mode| {
        let settings = AgentSettings {
            text_replies,
            mode,
            ..agent_settings_of(&harness)
        };
        agents::operator_note_hint(&agents::main_config(&settings).unwrap())
    };
    assert_eq!(hint(true, Mode::Agent), Some(TOOL_HINT));
    assert_eq!(hint(false, Mode::Agent), Some(TOOL_HINT));
    assert_eq!(hint(true, Mode::Chat), None);
    assert_eq!(hint(false, Mode::Chat), None);

    // With -p a note posted all the same (the web page can't send one)
    // reaches the agent without it.
    let workflow = harness.workflow().clone();
    let feed = harness.feed.clone();
    let posting = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        workflow
            .post_note(Note::new("progress?").to([MAIN]).once())
            .unwrap();
        go.notify_one();
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("List the files.".into()), &mut inputs),
    )
    .await
    .unwrap();
    posting.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let bodies = bodies.lock().unwrap().clone();
    assert!(bodies[1].contains("progress?"));
    assert!(!bodies[1].contains("only in your thinking may never reach the user"));
}
