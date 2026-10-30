// End-to-end tests of continuing a worker whose prompt cache expired, with
// the mock model: the agents' `message_agent` is refused until it confirms,
// `list_agents` says each worker's cache, `/message` asks first, and a
// front end's confirmed message goes straight to the worker.
// (Included in `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call, with_input};

/// The requests the model got: (the agent run that sent it, its body).
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// The agent run that sent a request: `r1`, `r2`, or `main`.
fn sender(body: &str) -> String {
    let marker = "You are agent run ";
    match body.find(marker) {
        Some(at) => body[at + marker.len()..]
            .split(',')
            .next()
            .unwrap_or_default()
            .to_string(),
        None => "main".to_string(),
    }
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

/// The output of the last tool call a request answers.
fn last_output(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

/// The main agent's mock: starts, lists or messages workers as the user's
/// message says, and ends its turn when a call's result comes back.
fn lead(body: &str, step: &str) -> String {
    let (user, answers) = last_user(body);
    if answers {
        return complete(step, "Done.");
    }
    let message = |to: &str, text: &str, confirm: Option<serde_json::Value>| {
        let mut arguments = serde_json::json!({"to": to, "message": text});
        if let Some(confirm) = confirm {
            arguments[crate::worker_cache::CONFIRM_PARAM] = confirm;
        }
        tool_call(step, crew::MESSAGE_AGENT, arguments)
    };
    if user.contains("Start r1.") {
        tool_call(
            step,
            crew::START_AGENT,
            serde_json::json!({"agent": WORKER, "task": "The big task."}),
        )
    } else if user.contains("Start r2.") {
        tool_call(
            step,
            crew::START_AGENT,
            serde_json::json!({"agent": WORKER, "task": "The small task."}),
        )
    } else if user.contains("List them.") {
        tool_call(step, crew::LIST_AGENTS, serde_json::json!({}))
    } else if user.contains("Continue r1 anyway.") {
        message("r1", "Go on anyway.", Some(true.into()))
    } else if user.contains("Continue r1, confirmed in words.") {
        message("r1", "Go on, in words.", Some("true".into()))
    } else if user.contains("Continue r1 while it runs.") {
        message("r1", "And the docs.", None)
    } else if user.contains("Continue r1.") {
        message("r1", "More of the big task.", None)
    } else if user.contains("Continue r2.") {
        message("r2", "More of the small task.", None)
    } else {
        complete(step, "Noted.")
    }
}

/// A model server for the main agent (see [`lead`]) and its workers: r1's
/// conversation reports 50k tokens of input, r2's 10. r1's turn after
/// "Go on anyway." waits for `release` the first time, after it notifies
/// `held`.
async fn serve(
    seen: Seen,
    held: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> ModelServer {
    let holding = Arc::new(std::sync::atomic::AtomicBool::new(false));
    ModelServer::start(Arc::new(move |index, body: String| {
        let who = sender(&body);
        seen.lock().unwrap().push((who.clone(), body.clone()));
        let (held, release, holding) = (held.clone(), release.clone(), holding.clone());
        Box::pin(async move {
            let step = format!("s{index}");
            match who.as_str() {
                "r1" => {
                    if body.contains("Go on anyway.")
                        && !holding.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        held.notify_one();
                        release.notified().await;
                    }
                    with_input(complete(&step, "Big done."), 50_000)
                }
                "r2" => complete(&step, "Small done."),
                _ => lead(&body, &step),
            }
        })
    }))
    .await
}

/// The bodies of `who`'s requests so far.
fn bodies(seen: &Seen, who: &str) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(sender, _)| sender == who)
        .map(|(_, body)| body.clone())
        .collect()
}

/// Whether one of `who`'s requests holds `text`.
fn got(seen: &Seen, who: &str, text: &str) -> bool {
    bodies(seen, who).iter().any(|body| body.contains(text))
}

/// A user turn, then the workers' news until they have all finished.
async fn turn(harness: &mut Harness, inputs: &mut Inputs, text: &str) {
    assert!(matches!(
        harness.turn(Prompt::User(text.into()), inputs).await,
        TurnEnd::Answered
    ));
    settle(harness, inputs).await;
}

/// The workers' news, until they have all finished.
async fn settle(harness: &mut Harness, inputs: &mut Inputs) {
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(inputs)).await;
}

/// The output the main agent got for its last tool call.
fn main_output(seen: &Seen) -> String {
    last_output(bodies(seen, "main").last().unwrap())
}

/// Has the caches of `workers` expire a second after their last request,
/// and waits for that.
async fn expire(harness: &Harness, workers: &[&str]) {
    let crew = harness.crew.as_ref().unwrap();
    for worker in workers {
        crew.shorten_cache(worker, Duration::from_secs(1));
    }
    tokio::time::sleep(Duration::from_millis(1_200)).await;
}

/// The main agent's `message_agent` to a finished worker whose cache expired
/// and whose conversation is past the threshold is refused with what
/// continuing costs, and the same call with `confirm_uncached` continues it.
/// A warm cache, a small conversation and a running worker get the message
/// at once, and `list_agents` says each worker's cache.
#[tokio::test]
async fn message_agent_asks_before_continuing_a_large_worker_whose_cache_expired() {
    let seen: Seen = Arc::default();
    let held = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), held.clone(), release.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    turn(&mut harness, &mut inputs, "Start r2.").await;
    assert!(got(&seen, "r1", "The big task.") && got(&seen, "r2", "The small task."));

    // While r1's cache lasts, the message goes to it.
    turn(&mut harness, &mut inputs, "Continue r1.").await;
    assert!(got(&seen, "r1", "More of the big task."));
    turn(&mut harness, &mut inputs, "List them.").await;
    let listed = main_output(&seen);
    assert!(listed.contains("min more, about 50k tokens"), "{listed}");

    // Once it expired, `list_agents` says so, with what continuing re-reads.
    expire(&harness, &["r1", "r2"]).await;
    turn(&mut harness, &mut inputs, "List them.").await;
    let listed = main_output(&seen);
    assert!(
        listed.contains("cache expired 1 min ago, continuing re-reads about 50k tokens"),
        "{listed}"
    );

    // The message isn't sent, and the agent reads why and what to do.
    let before = bodies(&seen, "r1").len();
    turn(&mut harness, &mut inputs, "Continue r1.").await;
    let refused = main_output(&seen);
    assert!(
        refused.contains(
            "r1's cache expired just now. Continuing re-reads about 50k tokens without it, using your ChatGPT plan. Start a fresh worker for this, or call message_agent again with confirm_uncached: true to continue r1."
        ),
        "{refused}"
    );
    assert_eq!(bodies(&seen, "r1").len(), before);

    // r2's conversation is small, so it continues at once.
    turn(&mut harness, &mut inputs, "Continue r2.").await;
    assert!(got(&seen, "r2", "More of the small task."));
    assert!(!main_output(&seen).contains("cache expired"));

    // `confirm_uncached` given as the word "true" confirms too.
    turn(
        &mut harness,
        &mut inputs,
        "Continue r1, confirmed in words.",
    )
    .await;
    assert!(got(&seen, "r1", "Go on, in words."));
    assert!(!main_output(&seen).contains("cache expired"));
    // (It went on and ended, and its cache expired again.)
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        harness
            .crew
            .as_ref()
            .unwrap()
            .worker_cache("r1")
            .unwrap()
            .asks(chrono::Utc::now())
    );

    // Confirmed, r1 continues. While it runs, a message reaches it at once.
    assert!(matches!(
        harness
            .turn(Prompt::User("Continue r1 anyway.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(20), held.notified())
        .await
        .expect("r1 continued");
    assert!(!main_output(&seen).contains("cache expired"));
    assert!(matches!(
        harness
            .turn(
                Prompt::User("Continue r1 while it runs.".into()),
                &mut inputs
            )
            .await,
        TurnEnd::Answered
    ));
    assert!(
        !main_output(&seen).contains("cache expired"),
        "{}",
        main_output(&seen)
    );
    release.notify_one();
    settle(&mut harness, &mut inputs).await;
    until(|| got(&seen, "r1", "And the docs.")).await;
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The user's `/message` to a worker whose cache expired says what
/// continuing costs first, and the same `/message` again sends it. A
/// different message asks again, and one a front end confirmed goes at
/// once. The worker's summary carries its cache for the front ends.
#[tokio::test]
async fn message_asks_the_user_before_continuing_a_large_worker_whose_cache_expired() {
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    expire(&harness, &["r1"]).await;
    let summary = harness.feed.worker_summary("r1").unwrap();
    assert_eq!(
        (
            summary.reread_tokens,
            summary.reread_dollars,
            summary.reread_plan.as_deref()
        ),
        (Some(50_000), None, Some("your ChatGPT plan"))
    );
    assert!(summary.cache().unwrap().asks(chrono::Utc::now()));
    let warnings = |harness: &Harness| {
        shown(&harness.feed)
            .into_iter()
            .filter(|(_, text, _)| text.contains("The same /message again continues it."))
            .count()
    };

    // (A tab and the worker's name in capitals are read as `/message r1`.)
    assert!(harness.live_command("/message\tR1 Check the docs.", Source::Terminal));
    assert_eq!(warnings(&harness), 1);
    assert!(shown(&harness.feed).iter().any(|(_, text, _)| text
        == "r1's cache expired just now. Continuing re-reads about 50k tokens without it, using your ChatGPT plan. The same /message again continues it."));
    // Another message asks again, and the first again then asks again too.
    assert!(harness.live_command("/message r1 Check the tests.", Source::Terminal));
    assert_eq!(warnings(&harness), 2);
    assert!(!got(&seen, "r1", "Check the docs.") && !got(&seen, "r1", "Check the tests."));
    // The same one again continues r1.
    assert!(harness.live_command("/message r1 Check the tests.", Source::Terminal));
    assert_eq!(warnings(&harness), 2);
    until(|| got(&seen, "r1", "Check the tests.")).await;
    settle(&mut harness, &mut inputs).await;

    // A message a front end confirmed (the screen's second Enter, or the
    // page's Continue) goes at once.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        harness
            .feed
            .worker_summary("r1")
            .unwrap()
            .cache()
            .unwrap()
            .asks(chrono::Utc::now())
    );
    harness.confirm_continue("/message r1 Check the build.");
    assert!(harness.live_command("/message r1 Check the build.", Source::Terminal));
    assert_eq!(warnings(&harness), 2);
    until(|| got(&seen, "r1", "Check the build.")).await;
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker's cache counts from when its latest request went out, not from
/// when its run ended, so a final request that took long leaves less of
/// the cache, and continuing asks sooner.
#[tokio::test]
async fn a_workers_cache_counts_from_its_latest_request_not_its_end() {
    let arrived: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>> = Arc::default();
    let seen = arrived.clone();
    let server = ModelServer::start(Arc::new(move |index, body: String| {
        let who = sender(&body);
        let seen = seen.clone();
        Box::pin(async move {
            let step = format!("s{index}");
            if who == "r1" {
                // The model takes two seconds over r1's last request.
                *seen.lock().unwrap() = Some(chrono::Utc::now());
                tokio::time::sleep(Duration::from_secs(2)).await;
                with_input(complete(&step, "Big done."), 50_000)
            } else {
                lead(&body, &step)
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    let crew = harness.crew.clone().unwrap();
    let cache = crew.worker_cache("r1").unwrap();
    let arrived = arrived.lock().unwrap().unwrap();
    // ChatGPT keeps the cache 30 minutes from the request's start.
    let went_out = cache.expires_at - chrono::Duration::minutes(30);
    assert!(
        went_out <= arrived && arrived - went_out < chrono::Duration::seconds(1),
        "{went_out} against {arrived}"
    );
    let workflow = harness.workflow().clone();
    let run = workflow
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap();
    let ended = workflow
        .agent_result(run.run_id)
        .unwrap()
        .info()
        .end_time
        .unwrap();
    assert!(ended - went_out >= chrono::Duration::seconds(2));
    // With a one-second cache, it expired before the run ended, so
    // continuing asks at once.
    crew.shorten_cache("r1", Duration::from_secs(1));
    let now = chrono::Utc::now();
    assert!(crew.worker_cache("r1").unwrap().asks(now));
    assert!(
        harness
            .feed
            .worker_summary("r1")
            .unwrap()
            .cache()
            .unwrap()
            .asks(now)
    );
    crew.shutdown().await;
}
