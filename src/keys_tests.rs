// End-to-end tests of `/keys`: a key added while Arko runs reaches the
// session at once (its models can be chosen, and new workers run on them),
// and a key removed stops new work starting on it. Each follows a path a
// user takes, with the mock model, and checks the requests it gets.
// (Included in `main`'s tests, whose helpers they use.)

use crate::credentials;
use crate::mock_model::{ModelServer, complete, openrouter_tool_call, tool_call};

/// The commands' output the feed shows, joined.
fn said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the last command printed.
fn last_said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .rev()
        .find(|(kind, _, _)| kind == "output")
        .map(|(_, text, _)| text)
        .unwrap_or_default()
}

/// The main agent's mock: each "Start a worker." starts one, and a call's
/// result or a worker's news ends the turn.
fn lead(body: &serde_json::Value) -> String {
    let items = body["input"].as_array().cloned().unwrap_or_default();
    let asked = items
        .iter()
        .filter(|item| item["role"] == "user" && item.to_string().contains("Start a worker."))
        .count();
    let started = items
        .iter()
        .filter(|item| item["type"] == "function_call" && item["name"] == crew::START_AGENT)
        .count();
    let answers = items
        .last()
        .is_some_and(|item| item["type"] == "function_call_output");
    if !answers && asked > started {
        tool_call(
            &format!("m{asked}"),
            crew::START_AGENT,
            serde_json::json!({"agent": WORKER, "task": "Count the files."}),
        )
    } else {
        complete(&format!("m{asked}"), "Done.")
    }
}

/// An OpenRouter key added after the conversation started: OpenRouter's
/// models can be chosen for new workers at once, and a new worker runs on
/// one. Removed, they can't be chosen again, and a new worker isn't started
/// on them, while the main agent goes on.
#[tokio::test]
async fn a_key_added_while_arko_runs_reaches_new_workers_at_once() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, text: String| {
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        seen.lock().unwrap().push(body.clone());
        Box::pin(async move {
            if body.get("messages").is_some() {
                // A worker on OpenRouter (Chat Completions).
                openrouter_tool_call(
                    "w",
                    "complete_task",
                    serde_json::json!({"summary": "Counted on OpenRouter."}),
                    0.01,
                )
            } else if text.contains("You are agent run r") {
                complete("w", "Counted on luna.")
            } else {
                lead(&body)
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let openrouter = "openrouter:anthropic/claude-opus-5.5";
    let reachable = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref()).models.reachable;
    let on_openrouter = |bodies: &Arc<Mutex<Vec<serde_json::Value>>>| {
        bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body.get("messages").is_some())
            .count()
    };
    assert_eq!(reachable(&harness), ["openai"]);
    assert!(harness.live_command(&format!("/model worker {openrouter}"), Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        format!("{openrouter} needs OPENROUTER_API_KEY. /keys adds API keys.")
    );

    // The conversation starts, with a worker on luna.
    assert!(matches!(
        harness.turn(Prompt::User("Start a worker.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    assert!(harness.conversation_started());

    // The key, added: OpenRouter is reached, and new workers can run there.
    harness.keys.use_local(server.url());
    assert_eq!(
        harness.keys.apply(&credentials::OPENROUTER, Some("sk-or-test")),
        "OpenRouter's models can be used now."
    );
    assert_eq!(reachable(&harness), ["openai", "openrouter"]);
    assert!(harness.live_command(&format!("/model worker {openrouter}"), Source::Terminal));
    assert!(
        last_said(&harness.feed).starts_with(&format!(
            "Workers that get no profile or model of their own now run {openrouter}"
        )),
        "{}",
        last_said(&harness.feed)
    );
    assert!(matches!(
        harness.turn(Prompt::User("Start a worker.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    until(|| on_openrouter(&bodies) > 0).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    let worker = bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.get("messages").is_some())
        .cloned()
        .unwrap();
    assert_eq!(worker["model"], "anthropic/claude-opus-5.5");

    // Removed: OpenRouter's models can't be chosen, and no new worker
    // starts on them. The main agent goes on.
    assert_eq!(
        harness.keys.apply(&credentials::OPENROUTER, None),
        "Agents already using it keep it until Arko quits."
    );
    assert_eq!(reachable(&harness), ["openai"]);
    assert!(harness.live_command(&format!("/model worker {openrouter}"), Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        format!("{openrouter} needs OPENROUTER_API_KEY. /keys adds API keys.")
    );
    let before = on_openrouter(&bodies);
    assert!(matches!(
        harness.turn(Prompt::User("Start a worker.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    let refused = bodies.lock().unwrap().iter().any(|body| {
        body.to_string()
            .contains("Can't start a worker on openrouter:anthropic/claude-opus-5.5. The user removed its API key (OPENROUTER_API_KEY) while Arko runs.")
    });
    assert!(refused, "the main agent heard why");
    assert_eq!(on_openrouter(&bodies), before);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/keys` lists the keys at the terminal, and from the web page says only
/// which are set; neither adds one.
#[tokio::test]
async fn keys_are_listed_and_added_only_at_the_terminal() {
    let server = ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/keys", Source::Web));
    let text = said(&harness.feed);
    assert!(text.contains("API keys\n  Anthropic"), "{text}");
    assert!(text.contains("Add or remove keys at the terminal."), "{text}");
    assert!(harness.live_command("/keys add openai", Source::Web));
    assert!(said(&harness.feed).contains(
        "API keys are added and removed at the terminal, not from the web page."
    ));
    assert!(harness.live_command("/keys add openai", Source::Terminal));
    assert!(said(&harness.feed).contains("/keys add reads the key with the input hidden"));
    assert!(harness.live_command("/keys add nothing", Source::Terminal));
    assert!(said(&harness.feed).contains("Arko takes no API key for `nothing`."));
    harness.crew.as_ref().unwrap().shutdown().await;
}
