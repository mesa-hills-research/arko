// End-to-end tests of switching the main agent's model once the
// conversation has started, and of `/new continue`: a new conversation that
// opens with a brief of the old one and the old model's report, reads the
// old one with `earlier_conversation`, and keeps the workers. Each follows a
// path a user takes, with the mock model (and the fake Claude Code CLI), and
// checks what the user sees and what the models are sent. (Included in
// `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};
use tokio::sync::Notify;

/// The requests' bodies, as the model got them.
type Bodies = Arc<Mutex<Vec<String>>>;

/// What the old model's report says in these tests.
const REPORT: &str = "I renamed the parser module, and the tests pass.";

/// The model a request asks for.
fn model_in(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["model"].as_str().unwrap_or_default().to_string()
}

/// The names of the tools a request offers.
fn tools_in(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| {
            tool["name"]
                .as_str()
                .or_else(|| tool["function"]["name"].as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The text of a request's last user message, and whether the request
/// answers a tool call.
fn last_user_of(body: &str) -> (String, bool) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let items = body["input"].as_array().cloned().unwrap_or_default();
    let answers = items
        .last()
        .is_some_and(|item| item["type"] == "function_call_output");
    let text = items
        .iter()
        .rev()
        .find(|item| item["role"] == "user")
        .map(|item| match &item["content"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default();
    (text, answers)
}

/// Whether a request is the one asking for the old model's report.
fn asks_report(body: &str) -> bool {
    let (user, answers) = last_user_of(body);
    !answers && user.contains("goes on from a report you write now")
}

/// The last request whose last user message has `text`.
fn turn_with(bodies: &Bodies, text: &str) -> String {
    bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| last_user_of(body).0.contains(text))
        .unwrap_or_else(|| panic!("no request with {text:?}"))
        .clone()
}

/// What Arko said, its lines and notices, joined.
fn lines(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A model that writes the report it is asked for with `handover_report`,
/// looks up the earlier conversation when told to, and ends every other
/// turn.
fn reporting_model(bodies: Bodies) -> Arc<crate::mock_model::Respond> {
    Arc::new(move |_, body: String| {
        bodies.lock().unwrap().push(body.clone());
        Box::pin(async move {
            let (user, answers) = last_user_of(&body);
            if asks_report(&body) {
                tool_call(
                    "report",
                    "handover_report",
                    serde_json::json!({ "report": REPORT }),
                )
            } else if !answers && user.contains("Look it up.") {
                tool_call(
                    "lookup",
                    crate::earlier::TOOL_NAME,
                    serde_json::json!({"operation": "search", "words": "parser module"}),
                )
            } else {
                complete("m", "Done.")
            }
        })
    })
}

/// The output of the call `id` that a request answers.
fn call_output(body: &str, id: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()?
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
        .map(|item| item["output"].to_string())
}

async fn say(harness: &mut Harness, inputs: &mut Inputs, text: &str) {
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User(text.into()), inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered), "{text}");
}

/// Runs `command` as the prompt does between turns: a switch, or `/new`.
async fn switch(harness: &mut Harness, inputs: &mut Inputs, command: &str) {
    match harness.switch_of(command) {
        Some(switch) => {
            let quit = tokio::time::timeout(
                Duration::from_secs(30),
                harness.switch(switch, Source::Terminal, inputs),
            )
            .await
            .unwrap();
            assert!(!quit);
        }
        None => {
            assert_eq!(command, "/new");
            harness.new_conversation();
        }
    }
}

/// `/model sol` once the conversation has started asks the old model for
/// its report, then starts a new conversation on sol whose first message
/// opens with the brief and the report, offered `earlier_conversation`.
/// `/resume 1` goes back to the old conversation on its own model, and
/// both go on on their own models after the session reopens.
#[tokio::test]
async fn a_switch_starts_a_new_conversation_from_the_old_models_report_and_a_brief() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "Rename the parser module.").await;
    assert_eq!(
        model_in(&turn_with(&bodies, "Rename the parser module.")),
        "gpt-6-luna"
    );

    switch(&mut harness, &mut inputs, "/model sol").await;
    let report = bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| asks_report(body))
        .cloned()
        .expect("the report was asked for");
    assert_eq!(model_in(&report), "gpt-6-luna", "of the old model");
    assert!(
        last_user_of(&report)
            .0
            .contains("a new conversation on gpt-6.1-sol"),
        "{report}"
    );
    let said = lines(&harness.feed);
    assert!(
        said.contains("Asking gpt-6-luna for a report. Stopping skips it."),
        "{said}"
    );
    assert!(
        !said.contains("Ctrl-C"),
        "the page and apps have no keys: {said}"
    );
    assert!(
        said.contains(
            "New conversation on gpt-6.1-sol (high effort), from gpt-6-luna's report and a brief of conversation 1."
        ),
        "{said}"
    );
    // The session, the next one's default and the clients have the model.
    assert_eq!(harness.settings.main.model, "gpt-6.1-sol");
    assert_eq!(
        harness.session.load_settings().unwrap().main.model,
        "gpt-6.1-sol"
    );
    assert_eq!(
        providers::LastModels::load(&harness.root).map(|last| last.main),
        Some("gpt-6.1-sol".to_string())
    );
    let reading = remote::Meter::read(harness.gauges.as_ref());
    assert_eq!(reading.models.main, "gpt-6.1-sol");
    assert_eq!(reading.models.conversation, "gpt-6.1-sol");

    say(
        &mut harness,
        &mut inputs,
        "Go on with the docs. Look it up.",
    )
    .await;
    let first = turn_with(&bodies, "Go on with the docs.");
    assert_eq!(model_in(&first), "gpt-6.1-sol");
    let (message, _) = last_user_of(&first);
    assert!(message.contains("# Earlier conversation"), "{message}");
    assert!(message.contains(REPORT), "{message}");
    // The brief is of the conversation before the request for the report.
    assert!(
        !message.contains("goes on from a report you write now"),
        "{message}"
    );
    assert!(
        message.find("# Earlier conversation") < message.find("Go on with the docs."),
        "the brief comes first: {message}"
    );
    assert!(
        tools_in(&first)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    );
    // Its first turn reads the conversation it continues.
    let all = bodies.lock().unwrap().clone();
    let found = all
        .iter()
        .find_map(|body| call_output(body, "call_lookup"))
        .unwrap_or_else(|| panic!("the lookup's result went back"));
    assert!(found.contains("Rename the parser module."), "{found}");
    // The record says which conversation it continues.
    assert_eq!(harness.conversations.list.len(), 2);
    assert_eq!(harness.conversations.list[1].continues, Some(0));
    assert_eq!(
        harness.conversations.list[1].title,
        "Go on with the docs. Look it up."
    );
    // The clients' list says each conversation's model.
    let listed: Vec<(usize, String)> = remote::Meter::read(harness.gauges.as_ref())
        .conversations
        .iter()
        .map(|entry| (entry.number, entry.model.clone()))
        .collect();
    assert_eq!(
        listed,
        [
            (2, "gpt-6.1-sol".to_string()),
            (1, "gpt-6-luna".to_string())
        ]
    );
    assert_eq!(harness.conversations.list[0].model, "gpt-6-luna");
    assert_eq!(harness.conversations.list[1].model, "gpt-6.1-sol");
    // Its next turn goes on without the brief again.
    say(&mut harness, &mut inputs, "And the changelog.").await;
    let next = turn_with(&bodies, "And the changelog.");
    assert_eq!(model_in(&next), "gpt-6.1-sol");
    assert!(!last_user_of(&next).0.contains("# Earlier conversation"));

    // Back to the old conversation, on its own model, which holds the
    // report, without the tool.
    harness.resume_command("1").await;
    assert_eq!(
        remote::Meter::read(harness.gauges.as_ref())
            .models
            .conversation,
        "gpt-6-luna"
    );
    say(&mut harness, &mut inputs, "Back here.").await;
    let back = turn_with(&bodies, "Back here.");
    assert_eq!(model_in(&back), "gpt-6-luna");
    assert!(back.contains(REPORT), "the report stays in it");
    assert!(
        !tools_in(&back)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    );

    // The session reopens: each conversation goes on on its own model.
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    say(&mut harness, &mut inputs, "Still here after the reopen.").await;
    assert_eq!(
        model_in(&turn_with(&bodies, "Still here after the reopen.")),
        "gpt-6-luna"
    );
    harness.resume_command("2").await;
    say(&mut harness, &mut inputs, "And there after the reopen.").await;
    let there = turn_with(&bodies, "And there after the reopen.");
    assert_eq!(model_in(&there), "gpt-6.1-sol");
    assert!(
        tools_in(&there)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A plain `/new` starts clean, with no brief and no tool. `/new continue`
/// carries the conversation over on the same model, with the model's
/// report. `/new` then `/model sol` starts clean on sol.
#[tokio::test]
async fn new_continue_carries_the_conversation_over_and_a_plain_new_starts_clean() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let offers_tool = |body: &str| {
        tools_in(body)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    };
    say(&mut harness, &mut inputs, "First.").await;

    switch(&mut harness, &mut inputs, "/new").await;
    say(&mut harness, &mut inputs, "Second.").await;
    let second = turn_with(&bodies, "Second.");
    assert!(!last_user_of(&second).0.contains("# Earlier conversation"));
    assert!(!offers_tool(&second));
    assert!(!bodies.lock().unwrap().iter().any(|body| asks_report(body)));

    switch(&mut harness, &mut inputs, "/new continue").await;
    let report = bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| asks_report(body))
        .cloned()
        .expect("the report was asked for");
    assert!(
        last_user_of(&report)
            .0
            .starts_with("The user is starting a new conversation that goes on from a report"),
        "{report}"
    );
    assert!(
        lines(&harness.feed)
            .contains("New conversation, from gpt-6-luna's report and a brief of conversation 2."),
        "{}",
        lines(&harness.feed)
    );
    say(&mut harness, &mut inputs, "Third.").await;
    let third = turn_with(&bodies, "Third.");
    assert_eq!(model_in(&third), "gpt-6-luna");
    assert!(last_user_of(&third).0.contains("# Earlier conversation"));
    assert!(last_user_of(&third).0.contains(REPORT));
    assert!(offers_tool(&third));

    // `/new` then `/model sol`: a clean start on sol.
    switch(&mut harness, &mut inputs, "/new").await;
    switch(&mut harness, &mut inputs, "/model sol").await;
    assert!(
        lines(&harness.feed).contains(
            "The next conversation runs gpt-6.1-sol (high effort), and new workers follow it."
        ),
        "{}",
        lines(&harness.feed)
    );
    say(&mut harness, &mut inputs, "Fourth.").await;
    let fourth = turn_with(&bodies, "Fourth.");
    assert_eq!(model_in(&fourth), "gpt-6.1-sol");
    assert!(!last_user_of(&fourth).0.contains("# Earlier conversation"));
    assert!(!offers_tool(&fourth));
    // And a model switch right after `/model` is a switch again.
    switch(&mut harness, &mut inputs, "/model luna").await;
    assert!(
        lines(&harness.feed).contains(
            "New conversation on gpt-6-luna (high effort), from gpt-6.1-sol's report and a brief of conversation 4."
        ),
        "{}",
        lines(&harness.feed)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Ctrl-C while the old model writes its report skips it, and the switch
/// goes on from the brief alone. A report that fails says why, and the
/// switch goes on too. The old conversation stays as it was.
#[tokio::test]
async fn the_report_is_skipped_with_ctrl_c_and_a_failed_one_falls_back_to_the_brief() {
    let bodies: Bodies = Arc::default();
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = ModelServer::start({
        let bodies = bodies.clone();
        let failing = failing.clone();
        Arc::new(move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let failing = failing.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if asks_report(&body) && failing {
                    let error = r#"{"error":{"message":"Incorrect API key provided.","type":"invalid_request_error","code":"invalid_api_key"}}"#;
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                        error.len()
                    )
                } else if asks_report(&body) {
                    // A report that takes long, until it is skipped.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    complete("late", "Too late.")
                } else {
                    complete("m", "Done.")
                }
            })
        })
    })
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "First.").await;

    // Ctrl-C once the report is asked for.
    let feed = harness.feed.clone();
    let pressing = tokio::spawn(async move {
        until(|| lines(&feed).contains("Asking gpt-6-luna for a report")).await;
        assert!(feed.activity().working, "the clients show it working");
        sender.send(Input::Interrupt).unwrap();
        sender
    });
    switch(&mut harness, &mut inputs, "/model sol").await;
    let sender = pressing.await.unwrap();
    let said = lines(&harness.feed);
    assert!(said.contains("Skipped gpt-6-luna's report."), "{said}");
    assert!(
        said.contains(
            "New conversation on gpt-6.1-sol (high effort), from a brief of conversation 1."
        ),
        "{said}"
    );
    assert!(!harness.feed.activity().working);
    say(&mut harness, &mut inputs, "Second.").await;
    let second = turn_with(&bodies, "Second.");
    assert!(last_user_of(&second).0.contains("# Earlier conversation"));
    // The old conversation goes on as it was, without the request.
    harness.resume_command("1").await;
    say(&mut harness, &mut inputs, "First again.").await;
    let again = turn_with(&bodies, "First again.");
    assert_eq!(model_in(&again), "gpt-6-luna");
    assert!(
        !again.contains("goes on from a report you write now"),
        "{again}"
    );

    // A report that fails says why.
    failing.store(true, std::sync::atomic::Ordering::SeqCst);
    switch(&mut harness, &mut inputs, "/model sol").await;
    let said = lines(&harness.feed);
    assert!(
        said.contains("gpt-6-luna couldn't write a report (its key or login was refused)."),
        "{said}"
    );
    assert!(
        said.contains(
            "New conversation on gpt-6.1-sol (high effort), from a brief of conversation 1."
        ),
        "{said}"
    );
    drop(sender);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker running when the model switches reports to the new
/// conversation, and a new worker without a model of its own starts on the
/// new model, as the session's workers follow the main agent's.
#[tokio::test]
async fn workers_move_to_the_new_conversation_and_new_ones_follow_the_model() {
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start({
        let bodies = bodies.clone();
        let hold = hold.clone();
        Arc::new(move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                let (user, answers) = last_user_of(&body);
                if body.contains("You are agent run r1") {
                    // The worker, running until the test lets it finish.
                    hold.notified().await;
                    complete("w1", "3 files.")
                } else if body.contains("You are agent run r2") {
                    complete("w2", "Counted again.")
                } else if asks_report(&body) {
                    tool_call(
                        "report",
                        "handover_report",
                        serde_json::json!({ "report": REPORT }),
                    )
                } else if answers {
                    complete("m", "Done.")
                } else if user.contains("Start a worker.") {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                } else if user.contains("Start another.") {
                    tool_call(
                        "m2",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count them again."}),
                    )
                } else {
                    complete("m", "Noted.")
                }
            })
        })
    })
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.settings.workers_follow_main());
    say(&mut harness, &mut inputs, "Start a worker.").await;
    switch(&mut harness, &mut inputs, "/model sol").await;
    let said = lines(&harness.feed);
    assert!(
        said.contains(
            "New conversation on gpt-6.1-sol (high effort), from gpt-6-luna's report and a brief of conversation 1. Worker r1 keeps running and reports here. New workers without a model of their own follow it."
        ),
        "{said}"
    );
    assert_eq!(harness.settings.worker.model, "gpt-6.1-sol");
    say(&mut harness, &mut inputs, "Hello again.").await;
    let hello = turn_with(&bodies, "Hello again.");
    assert!(
        last_user_of(&hello)
            .0
            .contains("Worker r1 is still running from earlier in this session."),
        "{hello}"
    );
    // The worker's report starts a turn of the new conversation.
    hold.notify_one();
    let crew = harness.crew.clone().unwrap();
    // (Its report, not only earlier news of it.)
    until(|| crew.running().is_empty() && crew.has_mail()).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(30), harness.deliver_news(&mut inputs))
            .await
            .unwrap(),
        News::Done
    ));
    let news = bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| body.contains("3 files.") && !body.contains("You are agent run"))
        .cloned()
        .expect("the report reached the main agent");
    assert_eq!(model_in(&news), "gpt-6.1-sol", "in the new conversation");
    // A new worker starts on the new model.
    say(&mut harness, &mut inputs, "Start another.").await;
    let worker = bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("You are agent run r2"))
        .cloned()
        .expect("the second worker ran");
    assert_eq!(model_in(&worker), "gpt-6.1-sol");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A main agent in Claude Code writes its report as a reply, in a copy of
/// its session, and the new conversation on an API model opens with it.
#[tokio::test]
async fn a_claude_code_lead_writes_its_report_as_a_reply_before_a_switch() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let script = serde_json::json!([
        [
            {"text": "Making the file.", "input_tokens": 1200},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Made it."}},
        ],
        [
            {"text": "REPORT: made the file with the shell.", "input_tokens": 1300},
        ],
    ]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));
    say(&mut harness, &mut inputs, "Make a file.").await;

    switch(&mut harness, &mut inputs, "/model luna").await;
    let said = lines(&harness.feed);
    assert!(
        said.contains(
            "New conversation on gpt-6-luna (high effort), from cc:claude-opus-5-5's report and a brief of conversation 1."
        ),
        "{said}"
    );
    let (log, _) = fake_log(&fake);
    let launches: Vec<Vec<String>> = log
        .iter()
        .filter(|entry| entry["event"] == "argv")
        .map(|entry| serde_json::from_value(entry["argv"].clone()).unwrap())
        .collect();
    assert!(
        launches
            .iter()
            .any(|argv| argv.iter().any(|arg| arg == "--fork-session")),
        "the report is written in a copy of the session: {launches:?}"
    );
    let asked = log
        .iter()
        .filter(|entry| entry["event"] == "user")
        .any(|entry| entry.to_string().contains("Reply with it as plain text"));
    assert!(asked, "{log:?}");

    say(&mut harness, &mut inputs, "Go on.").await;
    let first = turn_with(&bodies, "Go on.");
    assert_eq!(model_in(&first), "gpt-6-luna");
    assert!(
        last_user_of(&first)
            .0
            .contains("REPORT: made the file with the shell."),
        "{first}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// After a switch from Claude Code to sol on the ChatGPT login, `/cost`
/// keeps each conversation's usage on its own plan, as the terminal, the
/// page and an app's cost view read it.
#[tokio::test]
async fn cost_keeps_each_conversations_plan_after_a_switch_across_plans() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let script = serde_json::json!([
        [
            {"text": "Making the file.", "input_tokens": 1200},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Made it."}},
        ],
        [
            {"text": "REPORT: made the file with the shell.", "input_tokens": 1300},
        ],
    ]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.chatgpt_client(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));
    harness.settings.openai_auth = Some(providers::OpenAiAuth::ChatGpt);
    say(&mut harness, &mut inputs, "Make a file.").await;
    switch(&mut harness, &mut inputs, "/model sol").await;
    say(&mut harness, &mut inputs, "Go on.").await;
    assert_eq!(model_in(&turn_with(&bodies, "Go on.")), "gpt-6.1-sol");
    assert_eq!(harness.spent(), 0.0, "both on plans");

    harness.publish_details();
    let report = harness.details.cost();
    assert_eq!(
        report
            .main_parts
            .iter()
            .map(|part| part.plan)
            .collect::<Vec<_>>(),
        [Some("Claude plan"), Some("ChatGPT plan")]
    );
    assert!(
        report
            .main_parts
            .iter()
            .all(|part| part.api > 0.0 && part.used())
    );
    assert_eq!(report.main.plan, None, "paid for in two ways");
    assert!(
        (report.main.api - report.main_parts.iter().map(|part| part.api).sum::<f64>()).abs() < 1e-9
    );
    harness.mirrored(|harness| harness.command("/cost"));
    let said = lines(&harness.feed);
    assert!(
        said.contains("main agent ≈ $") && said.contains("at API prices, on the Claude plan"),
        "{said}"
    );
    assert!(
        said.contains("at API prices, on the ChatGPT plan"),
        "{said}"
    );
    assert!(!said.contains("which the ChatGPT plan covers"), "{said}");
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A model whose report waits for `release`, and which ends every other
/// turn with "Done.".
fn held_report_model(bodies: Bodies, release: Arc<Notify>) -> Arc<crate::mock_model::Respond> {
    Arc::new(move |_, body: String| {
        bodies.lock().unwrap().push(body.clone());
        let release = release.clone();
        Box::pin(async move {
            if asks_report(&body) {
                release.notified().await;
                tool_call(
                    "report",
                    "handover_report",
                    serde_json::json!({ "report": REPORT }),
                )
            } else {
                complete("m", "Done.")
            }
        })
    })
}

/// While the old model writes its report, what the page and an app send
/// waits for the new conversation and isn't lost, and the approval tray's
/// answers are acted on at once.
#[tokio::test]
async fn what_is_sent_during_the_report_goes_to_the_new_conversation() {
    let bodies: Bodies = Arc::default();
    let release = Arc::new(Notify::new());
    let server = ModelServer::start(held_report_model(bodies.clone(), release.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let page = harness.remote_actions.clone();
    let seen = bodies.clone();
    let typing = tokio::spawn(async move {
        sender.send(Input::Line("First.".into())).unwrap();
        until(|| has(&feed, "reply", "Done.", "")).await;
        sender.send(Input::Line("/model sol".into())).unwrap();
        until(|| lines(&feed).contains("Asking gpt-6-luna for a report")).await;
        page.send(Action::Message("Hello from the page.".into()))
            .unwrap();
        sender
            .send(Input::Message(crate::ui::Message {
                text: "Hello from the app.".into(),
                ..Default::default()
            }))
            .unwrap();
        sender
            .send(Input::AnswerApprovals(crate::approvals::ApprovalAnswers {
                choices: Vec::new(),
                words: None,
            }))
            .unwrap();
        until(|| lines(&feed).contains("Those approvals are no longer open.")).await;
        assert!(feed.activity().working, "still writing its report");
        release.notify_one();
        let reached = |text: &str| {
            seen.lock()
                .unwrap()
                .iter()
                .any(|body| body.contains(text) && model_in(body) == "gpt-6.1-sol")
        };
        until(|| reached("Hello from the page.") && reached("Hello from the app.")).await;
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
        sender
    });
    tokio::time::timeout(Duration::from_secs(60), harness.repl(&mut inputs))
        .await
        .unwrap();
    let _sender = typing.await.unwrap();
    // Both reached the new conversation, the first with the brief and the
    // other as a note of that turn, as a message typed during a turn does.
    // (The page's and the app's arrive by different ways, in either order.)
    let all = bodies.lock().unwrap().clone();
    let first = all
        .iter()
        .find(|body| last_user_of(body).0.contains("# Earlier conversation"))
        .expect("the new conversation started");
    assert_eq!(model_in(first), "gpt-6.1-sol");
    let (message, _) = last_user_of(first);
    assert!(message.contains(REPORT), "{message}");
    assert!(
        message.contains("Hello from the page.") || message.contains("Hello from the app."),
        "{message}"
    );
    for text in ["Hello from the page.", "Hello from the app."] {
        let with = all
            .iter()
            .rfind(|body| body.contains(text))
            .unwrap_or_else(|| panic!("{text} reached the agent"));
        assert_eq!(model_in(with), "gpt-6.1-sol", "{text}");
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Quitting while the old model writes its report stops it and quits.
#[tokio::test]
async fn quitting_during_the_report_quits() {
    let bodies: Bodies = Arc::default();
    let release = Arc::new(Notify::new());
    let server = ModelServer::start(held_report_model(bodies.clone(), release)).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        sender.send(Input::Line("First.".into())).unwrap();
        until(|| has(&feed, "reply", "Done.", "")).await;
        sender.send(Input::Line("/model sol".into())).unwrap();
        until(|| lines(&feed).contains("Asking gpt-6-luna for a report")).await;
        sender.send(Input::Quit(Some(Vec::new()))).unwrap();
        sender
    });
    tokio::time::timeout(Duration::from_secs(30), harness.repl(&mut inputs))
        .await
        .expect("it quits without the report");
    let _sender = typing.await.unwrap();
    assert!(!harness.feed.activity().working);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A Ctrl-C that comes before the report's request is under way (read at
/// once, on a runtime of one thread) still skips it.
#[tokio::test]
async fn a_ctrl_c_before_the_report_starts_skips_it() {
    let bodies: Bodies = Arc::default();
    let release = Arc::new(Notify::new());
    let server = ModelServer::start(held_report_model(bodies.clone(), release)).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "First.").await;
    sender.send(Input::Interrupt).unwrap();
    switch(&mut harness, &mut inputs, "/model sol").await;
    let said = lines(&harness.feed);
    assert!(said.contains("Skipped gpt-6-luna's report."), "{said}");
    assert!(
        said.contains(
            "New conversation on gpt-6.1-sol (high effort), from a brief of conversation 1."
        ),
        "{said}"
    );
    // The old conversation goes on as it was.
    harness.resume_command("1").await;
    say(&mut harness, &mut inputs, "First again.").await;
    let again = turn_with(&bodies, "First again.");
    assert_eq!(model_in(&again), "gpt-6-luna");
    assert!(
        !again.contains("goes on from a report you write now"),
        "{again}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The brief waits for the new conversation's first run to be recorded.
/// A first message whose run didn't start, or whose run Arko closed
/// before it saved, leaves it for the next message, also after the
/// session opens again. A plain `/new` after that starts clean.
#[tokio::test]
async fn the_brief_waits_for_the_new_conversations_first_run() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "First.").await;
    switch(&mut harness, &mut inputs, "/model sol").await;
    // The first message's configuration was made, and no run came of it.
    assert!(harness.new_conversation_config().is_some());
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    assert!(
        harness.conversations.carry.is_some(),
        "kept with the session"
    );

    say(&mut harness, &mut inputs, "Go on.").await;
    let first = turn_with(&bodies, "Go on.");
    assert_eq!(model_in(&first), "gpt-6.1-sol");
    assert!(last_user_of(&first).0.contains("# Earlier conversation"));
    assert!(
        tools_in(&first)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    );
    assert!(harness.conversations.carry.is_none(), "its run took it");
    assert_eq!(harness.conversations.list[1].continues, Some(0));

    harness.new_conversation();
    say(&mut harness, &mut inputs, "Clean.").await;
    let clean = turn_with(&bodies, "Clean.");
    assert!(!last_user_of(&clean).0.contains("# Earlier conversation"));
    assert!(
        !tools_in(&clean)
            .iter()
            .any(|tool| tool == crate::earlier::TOOL_NAME)
    );
    assert_eq!(harness.conversations.list[2].continues, None);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A session that opens again goes on with the handover size and the
/// window of the conversation it was in: the next one's model after a
/// switch whose first message hasn't come, and the model of an older
/// conversation `/resume` went back to.
#[tokio::test]
async fn a_reopened_session_takes_the_size_and_window_of_its_conversation() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let script = serde_json::json!([
        [
            {"text": "Making the file.", "input_tokens": 1200},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Made it."}},
        ],
        [
            {"text": "REPORT: made the file with the shell.", "input_tokens": 1300},
        ],
    ]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));
    say(&mut harness, &mut inputs, "Make a file.").await;
    let luna = resolve_model("luna").unwrap();
    let opus = resolve_model("cc:opus").unwrap();
    let window =
        |model: &arko_ai::Model| arko_ai::input_limit::effective_max_input_tokens(model, false);
    assert_ne!(window(&luna), window(&opus), "they differ");
    let reopen = |harness: &mut Harness| {
        harness.open(true).unwrap();
        harness.start_new_workers();
        (harness.handover.most(), lock(&harness.stats).context_window)
    };

    // A switch, and Arko closes before the next message.
    switch(&mut harness, &mut inputs, "/model luna").await;
    harness.crew.as_ref().unwrap().shutdown().await;
    assert_eq!(
        reopen(&mut harness),
        (agents::max_handover(&luna, None).unwrap(), window(&luna))
    );

    // Back in the older conversation, on Claude Code, and Arko closes.
    say(&mut harness, &mut inputs, "Go on.").await;
    harness.resume_command("1").await;
    harness.crew.as_ref().unwrap().shutdown().await;
    assert_eq!(
        reopen(&mut harness),
        (agents::max_handover(&opus, None).unwrap(), window(&opus))
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// `/cost` counts the calls of a turn under way, before the turn records
/// them in its run.
#[tokio::test]
async fn cost_counts_a_turn_under_way() {
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start({
        let hold = hold.clone();
        Arc::new(move |_, body: String| {
            let hold = hold.clone();
            Box::pin(async move {
                let (user, answers) = last_user_of(&body);
                if answers {
                    hold.notified().await;
                    complete("m2", "Counted.")
                } else if user.contains("Count.") {
                    crate::mock_model::with_input(
                        tool_call(
                            "m1",
                            message::MESSAGE_USER,
                            serde_json::json!({"message": "Counting."}),
                        ),
                        7_777,
                    )
                } else {
                    complete("m", "Done.")
                }
            })
        })
    })
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "Hello.").await;
    harness.publish_details();
    let details = harness.details.clone();
    let before = details.cost().main.input;
    let reading = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(20), async {
            while details.cost().main.input < before + 7_777 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    });
    let turn = harness.turn(Prompt::User("Count.".into()), &mut inputs);
    let release = async {
        let counted = reading.await.unwrap();
        hold.notify_one();
        counted
    };
    let (end, counted) = tokio::join!(turn, release);
    assert!(matches!(end, TurnEnd::Answered));
    assert!(counted, "the call under way counted before the turn ended");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The reasoning effort a request asks for.
fn effort_in(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["reasoning"]["effort"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A conversation `/resume` went back to says its own model and effort,
/// where they differ from those new conversations start on: in the
/// banner, `/model` and the models published, and `/new continue` and a
/// switch go on from its effort.
#[tokio::test]
async fn a_resumed_conversation_says_its_own_model_and_effort() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(reporting_model(bodies.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    say(&mut harness, &mut inputs, "At high.").await;
    switch(&mut harness, &mut inputs, "/model luna low").await;
    say(&mut harness, &mut inputs, "At low.").await;
    assert_eq!(effort_in(&turn_with(&bodies, "At low.")), "low");
    assert_eq!(harness.settings.main.effort, Effort::Low);

    harness.resume_command("1").await;
    let models = remote::Meter::read(harness.gauges.as_ref()).models;
    assert_eq!(
        (
            models.conversation.as_str(),
            models.conversation_effort.as_str()
        ),
        ("gpt-6-luna", "high")
    );
    assert_eq!(models.main_effort, "low", "new conversations start on it");
    assert!(
        models.banner_model.starts_with("gpt-6-luna (high effort)"),
        "{}",
        models.banner_model
    );
    assert!(harness.live_command("/model", Source::Terminal));
    assert!(
        lines(&harness.feed).contains("This conversation runs gpt-6-luna (high effort)."),
        "{}",
        lines(&harness.feed)
    );
    // It goes on at its own effort, and so does what carries it over.
    switch(&mut harness, &mut inputs, "/new continue").await;
    assert!(
        lines(&harness.feed)
            .contains("New conversation, from gpt-6-luna's report and a brief of conversation 1."),
        "{}",
        lines(&harness.feed)
    );
    say(&mut harness, &mut inputs, "Carried over.").await;
    assert_eq!(effort_in(&turn_with(&bodies, "Carried over.")), "high");
    harness.crew.as_ref().unwrap().shutdown().await;
}
