// End-to-end tests of `/model`: the main agent's model before the
// conversation starts, the workers' at any time, and the workers already
// started keeping theirs (resumed, or continued). Each follows a path a
// user takes, with the mock model, and checks the requests the model gets.
// (Included in `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};

/// What a request asks for: (who sent it, its model, its effort).
type Asked = Arc<Mutex<Vec<(String, String, String)>>>;

/// The request's model and reasoning effort.
fn model_of(body: &str) -> (String, String) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    (
        body["model"].as_str().unwrap_or_default().to_string(),
        body["reasoning"]["effort"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// The last user message's text, and whether the request answers a tool
/// call (its last input item is a call's output).
fn last_user(body: &str) -> (String, bool) {
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

/// The commands' output the feed shows, joined.
fn said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Before the conversation starts, `/model` changes the main agent (and
/// the workers that follow it), the session and the next default keep it,
/// and the first request runs on it. Once it has started, the main agent
/// keeps its model, after `/new` too.
#[tokio::test]
async fn before_the_start_model_sets_the_main_agent_and_after_it_says_it_keeps_it() {
    let asked: Asked = Arc::default();
    let seen = asked.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let (model, effort) = model_of(&body);
        seen.lock().unwrap().push(("main".into(), model, effort));
        Box::pin(async move { complete("m", "Done.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(!harness.conversation_started());
    assert!(harness.settings.workers_follow_main());

    // A name and an effort Arko can't take say what was meant.
    assert!(harness.live_command("/model opsu", Source::Terminal));
    assert!(said(&harness.feed).contains("Did you mean opus?"));
    assert!(harness.live_command("/model sol extreme", Source::Terminal));
    assert!(said(&harness.feed).contains("`extreme` isn't an effort"));
    assert!(harness.live_command("/model gpt-5.4-nano-2026-03-17 max", Source::Terminal));
    assert!(
        said(&harness.feed)
            .contains("gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max")
    );
    // A provider the session can't reach says what it needs.
    assert!(harness.live_command("/model gemini high", Source::Terminal));
    assert!(said(&harness.feed).contains(
        "gemini-3.1-pro-preview needs GEMINI_API_KEY. /keys adds API keys."
    ));

    assert!(harness.live_command("/model sol high", Source::Terminal));
    assert!(
        said(&harness.feed)
            .contains("The main agent now runs gpt-6.1-sol (high effort), and new workers follow it.")
    );
    assert_eq!(harness.settings.main.model, "gpt-6.1-sol");
    assert_eq!(harness.settings.main.effort, Effort::High);
    assert_eq!(harness.settings.worker.model, "gpt-6.1-sol");
    assert!(harness.session.latest_checkpoint().is_none(), "still before the start");
    let saved = harness.session.load_settings().unwrap();
    assert_eq!(saved.main.model, "gpt-6.1-sol");
    assert_eq!(
        providers::LastModels::load(&harness.root).map(|last| last.main),
        Some("gpt-6.1-sol".to_string())
    );
    let reading = remote::Meter::read(harness.gauges.as_ref());
    assert_eq!(reading.models.main, "gpt-6.1-sol");
    assert!(reading.models.banner_model.starts_with("gpt-6.1-sol (high effort)"));
    assert!(!reading.models.started);

    // The first request runs on it.
    assert!(matches!(
        harness.turn(Prompt::User("Hello.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    assert_eq!(
        asked.lock().unwrap()[0],
        ("main".into(), "gpt-6.1-sol".into(), "high".into())
    );
    assert!(remote::Meter::read(harness.gauges.as_ref()).models.started);

    // Once the conversation has started, the main agent keeps its model,
    // and a new conversation (/new) is no different.
    harness.conversation = Conversation::New;
    assert!(harness.live_command("/model luna", Source::Terminal));
    assert!(said(&harness.feed).contains(
        "The main agent keeps gpt-6.1-sol (high effort) for this session. /model worker sets the model for workers started from now on."
    ));
    assert_eq!(harness.settings.main.model, "gpt-6.1-sol");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The main agent's mock: starts a worker or messages one as the user's
/// message says, and ends its turn when a call's result comes back.
fn lead(body: &str, step: &str) -> String {
    let (user, answers) = last_user(body);
    if answers {
        return complete(step, "Done.");
    }
    if user.contains("Start the first worker.") {
        tool_call(
            step,
            crew::START_AGENT,
            serde_json::json!({"agent": WORKER, "task": "The first task."}),
        )
    } else if user.contains("Start the second worker.") {
        tool_call(
            step,
            crew::START_AGENT,
            serde_json::json!({"agent": WORKER, "task": "The second task."}),
        )
    } else if user.contains("Follow up with the first.") {
        tool_call(
            step,
            crew::MESSAGE_AGENT,
            serde_json::json!({"to": "r1", "message": "Follow up please."}),
        )
    } else {
        complete(step, "Noted.")
    }
}

/// After the start, `/model worker` changes the workers started from now
/// on: a worker started before keeps its model when Arko stops and resumes
/// it, and when the main agent continues it later, while a new worker
/// starts on the new model and the main agent keeps its own.
#[tokio::test]
async fn a_worker_keeps_the_model_it_started_on_and_new_ones_start_on_the_new_one() {
    let asked: Asked = Arc::default();
    let seen = asked.clone();
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold = held.clone();
    let first_started = Arc::new(tokio::sync::Notify::new());
    let started = first_started.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let (model, effort) = model_of(&body);
        let who = if body.contains("You are agent run r1") {
            "r1"
        } else if body.contains("You are agent run r2") {
            "r2"
        } else {
            "main"
        };
        seen.lock()
            .unwrap()
            .push((who.to_string(), model, effort));
        let hold = hold.clone();
        let started = started.clone();
        Box::pin(async move {
            match who {
                // r1's first request waits, as if the model were slow, until
                // Arko stops.
                "r1" if !hold.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                    started.notify_one();
                    std::future::pending::<String>().await
                }
                "r1" if body.contains("Follow up please.") => complete("w3", "Followed up."),
                "r1" => complete("w1", "First done."),
                "r2" => complete("w2", "Second done."),
                _ => lead(&body, "m"),
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let models = |who: &str| -> Vec<String> {
        asked
            .lock()
            .unwrap()
            .iter()
            .filter(|(sender, _, _)| sender == who)
            .map(|(_, model, _)| model.clone())
            .collect()
    };

    // r1 starts on the workers' model, luna.
    assert!(matches!(
        harness
            .turn(Prompt::User("Start the first worker.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(10), first_started.notified())
        .await
        .unwrap();

    // Workers started from now on run sol.
    assert!(harness.live_command("/model worker sol high", Source::Terminal));
    assert!(said(&harness.feed).contains(
        "Workers started from now on without a profile or a model of their own run gpt-6.1-sol (high effort). The main agent keeps gpt-6-luna (high effort) for this session, and the workers started before keep theirs."
    ));
    assert_eq!(harness.session.load_settings().unwrap().worker.model, "gpt-6.1-sol");
    let reading = remote::Meter::read(harness.gauges.as_ref());
    assert_eq!(reading.models.workers, "gpt-6.1-sol");
    assert!(reading.models.banner_workers.contains("gpt-6.1-sol"));

    // Arko stops while r1 runs, and resumes the session: r1 goes on on luna.
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    let finished = |harness: &Harness| {
        harness
            .crew
            .as_ref()
            .is_some_and(|crew| crew.running().is_empty())
    };
    until(|| models("r1").len() >= 2).await;
    assert_eq!(models("r1"), ["gpt-6-luna", "gpt-6-luna"]);
    tokio::time::timeout(Duration::from_secs(20), async {
        while !finished(&harness) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // A new worker starts on sol, and the main agent stays on luna.
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    assert!(matches!(
        harness
            .turn(Prompt::User("Start the second worker.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    until(|| !models("r2").is_empty()).await;
    assert_eq!(models("r2")[0], "gpt-6.1-sol");
    assert!(
        asked
            .lock()
            .unwrap()
            .iter()
            .any(|(who, _, effort)| who == "r2" && effort == "high")
    );
    assert!(models("main").iter().all(|model| model == "gpt-6-luna"));

    // The main agent continues r1: on luna still.
    let before = models("r1").len();
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    assert!(matches!(
        harness
            .turn(Prompt::User("Follow up with the first.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    until(|| models("r1").len() > before).await;
    assert!(models("r1").iter().all(|model| model == "gpt-6-luna"), "{:?}", models("r1"));

    // And when the session is resumed again, every worker keeps its own.
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    // `/workers` says what each runs, those on another model than new
    // workers' too.
    assert!(
        harness
            .crew
            .as_ref()
            .unwrap()
            .describe()
            .contains("runs gpt-6-luna")
    );
    // The others say what they run, with the effort they started at.
    assert!(
        harness
            .crew
            .as_ref()
            .unwrap()
            .describe()
            .contains("runs gpt-6.1-sol at high effort\n")
    );
    // `/model worker main`: new workers follow the main agent again.
    assert!(harness.live_command("/model worker main", Source::Terminal));
    assert!(said(&harness.feed).contains(
        "Workers started from now on follow the main agent's model, gpt-6-luna (high effort)."
    ));
    assert!(harness.settings.workers_follow_main());
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Before the start, `/model worker` takes the workers off the main
/// agent's model, `/model` then changes the main agent's alone, and
/// `/model worker main` makes them follow it again.
#[tokio::test]
async fn before_the_start_model_worker_sets_the_workers_own_model() {
    let server =
        ModelServer::start(Arc::new(move |_, _| Box::pin(async move { complete("m", "Done.") })))
            .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/model worker astra", Source::Terminal));
    assert!(said(&harness.feed).contains("Workers without a profile or a model of their own now run gpt-6-astra (high effort)."));
    assert!(!harness.settings.workers_follow_main());
    assert_eq!(
        harness.worker_config.llm_config.model.id, "gpt-6-astra",
        "the workers' configuration, built again"
    );
    assert!(harness.live_command("/model sol", Source::Terminal));
    assert!(
        said(&harness.feed)
            .contains("The main agent now runs gpt-6.1-sol (high effort). Workers keep gpt-6-astra (high effort).")
    );
    assert!(harness.live_command("/model worker main", Source::Terminal));
    assert!(harness.settings.workers_follow_main());
    assert_eq!(harness.worker_config.llm_config.model.id, "gpt-6.1-sol");
    // The line prompt's forms without a model say the setting and the use.
    assert!(harness.live_command("/model worker", Source::Terminal));
    assert!(said(&harness.feed).contains(
        "Workers without a profile or a model of their own run the main agent's model, gpt-6.1-sol (high effort). /model worker MODEL [EFFORT] sets"
    ));
    assert!(harness.live_command("/model", Source::Terminal));
    assert!(said(&harness.feed).contains("The main agent runs gpt-6.1-sol (high effort). /model MODEL [EFFORT]"));
    // From the web page, the same, and the terminal says where it came from.
    assert!(harness.live_command("/model worker astra low", Source::Web));
    assert!(
        said(&harness.feed)
            .contains("Changed from the web page. Workers without a profile or a model of their own now run gpt-6-astra (low effort).")
    );
    assert_eq!(harness.settings.worker.effort, Effort::Low);
    assert!(harness.live_command("/model worker astra low", Source::Web));
    assert!(
        said(&harness.feed)
            .contains("From the web page. Workers without a profile or a model of their own now run gpt-6-astra (low effort).")
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The status a page that connects now gets with its history.
async fn page_status(http: &reqwest::Client, base: &str, cookie: &str) -> serde_json::Value {
    let mut stream = http
        .get(format!("{base}/api/events"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let mut buffer = String::new();
    loop {
        if let Some(start) = buffer.find("event: reset\n")
            && let Some(end) = buffer[start..].find("\n\n")
        {
            let block = &buffer[start..start + end];
            let data: String = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            let data: serde_json::Value = serde_json::from_str(&data).unwrap();
            return data["status"].clone();
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
            .await
            .expect("the history in time")
            .unwrap()
            .expect("the stream goes on");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// The page served before the first message (`arko --remote`) names the
/// model `/model` then sets, before the conversation starts, and after
/// it: its Model row reads the status's `info.model`.
#[tokio::test]
async fn the_page_names_the_model_set_after_it_started() {
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    // As `run` does before it serves the page.
    harness.publish_models();
    let (http, base, cookie) = web(&mut harness).await;
    let model = |status: &serde_json::Value| status["info"]["model"].as_str().unwrap().to_string();
    assert!(model(&page_status(&http, &base, &cookie).await).starts_with("gpt-6-luna"));
    assert!(harness.live_command("/model sol high", Source::Terminal));
    let status = page_status(&http, &base, &cookie).await;
    assert!(model(&status).starts_with("gpt-6.1-sol (high effort)"), "{status}");
    assert_eq!(status["info"]["model"], status["models"]["banner_model"]);
    assert!(matches!(
        harness.turn(Prompt::User("Hello.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    let status = page_status(&http, &base, &cookie).await;
    assert!(model(&status).starts_with("gpt-6.1-sol"), "{status}");
    harness.crew.as_ref().unwrap().shutdown().await;
}
