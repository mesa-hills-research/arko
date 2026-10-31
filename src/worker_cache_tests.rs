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

    // A client that waits for the verdict gets the refusal and what
    // confirming takes, and the transcript stays as it was. Its refusal
    // leaves nothing to confirm with the same message again.
    let (reply, verdict) = oneshot::channel();
    harness.message_command(
        "r1 Check the docs.",
        Source::App,
        &mut crate::sending::Reply::to(Some(reply)),
        false,
    );
    match verdict.await.unwrap() {
        crate::remote::Verdict::Refused(refusal) => {
            assert_eq!(
                refusal.kind,
                crate::remote::RefusalKind::Uncached {
                    worker: "r1".into()
                }
            );
            assert_eq!(refusal.confirm, Some(crate::remote::Confirm::ContinueUncached));
            assert_eq!(
                refusal.words,
                "r1's cache expired just now. Continuing re-reads about 50k tokens without it, using your ChatGPT plan."
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(warnings(&harness), 0);

    // (A tab and the worker's name in capitals are read as `/message r1`.)
    assert!(harness.live_command("/message\tR1 Check the docs.", Source::Terminal));
    assert_eq!(warnings(&harness), 1);
    assert!(shown(&harness.feed).iter().any(|(_, text, _)| text
        == "r1's cache expired just now. Continuing re-reads about 50k tokens without it, using your ChatGPT plan. The same /message again continues it."));
    // That warning at the line prompt confirms nothing for a client that
    // sends the same message unconfirmed: it is refused.
    let (reply, verdict) = oneshot::channel();
    harness.message_command(
        "r1 Check the docs.",
        Source::App,
        &mut crate::sending::Reply::to(Some(reply)),
        false,
    );
    assert!(matches!(
        verdict.await.unwrap(),
        crate::remote::Verdict::Refused(crate::remote::Refusal {
            kind: crate::remote::RefusalKind::Uncached { .. },
            ..
        })
    ));
    assert!(!got(&seen, "r1", "Check the docs."));
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
    // A client's send confirmed but refused for its images leaves no
    // permission behind: the same message from the line prompt asks.
    let image = ui::Attachment {
        bytes: Arc::from(&b"pixels"[..]),
        mime: "image/png".into(),
        name: None,
    };
    let (reply, verdict) = oneshot::channel();
    let confirmed = Input::Message(ui::Message {
        text: "/message r1 Check the lint.".into(),
        images: vec![image],
        continue_uncached: true,
        ..Default::default()
    });
    let crate::sending::Read::Sent(sent) = harness
        .read_sent(
            Event::Input(Some(confirmed)),
            crate::sending::Reply::to(Some(reply)),
            &mut inputs,
            crate::sending::Phase::Idle,
        )
        .await
    else {
        panic!("read as sent");
    };
    assert!(harness.admit(sent, crate::sending::Phase::Idle).is_none());
    assert!(matches!(
        verdict.await.unwrap(),
        crate::remote::Verdict::Refused(crate::remote::Refusal {
            kind: crate::remote::RefusalKind::WorkerTextOnly,
            ..
        })
    ));
    assert!(harness.live_command("/message r1 Check the lint.", Source::Terminal));
    assert_eq!(warnings(&harness), 3);
    assert!(!got(&seen, "r1", "Check the lint."));

    // A client's send, confirmed, is accepted once r1 has it, as the item
    // of the message in r1's transcript.
    let (reply, verdict) = oneshot::channel();
    harness.message_command(
        "r1 Check the build.",
        Source::App,
        &mut crate::sending::Reply::to(Some(reply)),
        true,
    );
    match verdict.await.unwrap() {
        crate::remote::Verdict::Accepted {
            delivery: Delivery::Command,
            item: Some(item),
            ..
        } => {
            let shown = harness.feed.subscribe().0.items;
            let copy = shown.iter().find(|shown| shown.id == item).unwrap();
            assert_eq!((copy.agent.as_str(), copy.text.as_str()), ("r1", "Check the build."));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(warnings(&harness), 3);
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
    // ChatGPT keeps the cache 30 minutes from the request's start, and it
    // counts as lapsed 30 seconds before.
    let went_out = cache.expires_at - chrono::Duration::seconds(30 * 60 - 30);
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

/// A model server for a worker r1 whose conversation is replaced. Its first
/// two requests call a tool, the second reading `input` tokens, and the
/// request that asks for its handover report (over the handover size) reads
/// as many. The first request of its fresh conversation waits, after it
/// notifies `fresh`. Any other third request waits too.
async fn replacing(input: usize, fresh: Arc<tokio::sync::Notify>) -> ModelServer {
    ModelServer::start(Arc::new(move |index, body: String| {
        let who = sender(&body);
        let fresh = fresh.clone();
        Box::pin(async move {
            let step = format!("s{index}");
            if who != "r1" {
                return lead(&body, &step);
            }
            if body.contains("Your conversation is getting long") {
                return with_input(
                    tool_call(
                        &step,
                        arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
                        serde_json::json!({"report": "REPORT: the work so far."}),
                    ),
                    input,
                );
            }
            let results = body.matches("function_call_output").count();
            if body.contains("REPORT:") || results > 1 {
                fresh.notify_one();
                return std::future::pending().await;
            }
            // (A conversation whose first request is over the handover size
            // doesn't hand over, so the first is small.)
            let read = tool_call(
                &step,
                arko_agents::builtins::READ_FILE_TOOL_NAME,
                serde_json::json!({"path": "missing.txt"}),
            );
            if results == 0 {
                read
            } else {
                with_input(read, input)
            }
        })
    }))
    .await
}

/// Stops r1 and waits until it has stopped. Then neither the session nor
/// the summary has a cache for it, and its task records no request.
async fn stopped_without_a_cache(harness: &Harness) {
    let crew = harness.crew.clone().unwrap();
    crew.stop("r1", "the test stopped it").unwrap();
    until(|| {
        harness
            .feed
            .worker_summary("r1")
            .is_some_and(|summary| !summary.state.is_active())
    })
    .await;
    assert_eq!(crew.worker_cache("r1"), None);
    assert_eq!(crew.cache_state("r1"), None);
    assert_eq!(harness.feed.worker_summary("r1").unwrap().cache(), None);
    let workflow = harness.workflow();
    let run = workflow
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap();
    assert_eq!(
        workflow.agent_result(run.run_id).unwrap().info().latest_request,
        arko_agents::RequestRecord::NoRequest
    );
    crew.shutdown().await;
}

/// A handover replaces a worker's conversation with a fresh one that no
/// cache holds. Stopped before a request of the fresh one ended, it has no
/// cache to price or ask about, not the replaced conversation's.
#[tokio::test]
async fn a_worker_stopped_after_its_handover_has_no_cache_of_the_old_conversation() {
    let fresh = Arc::new(tokio::sync::Notify::new());
    let server = replacing(300_000, fresh.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/handover-at workers 100k", Source::Terminal));
    assert!(matches!(
        harness
            .turn(Prompt::User("Start r1.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(20), fresh.notified())
        .await
        .expect("r1 handed over");
    stopped_without_a_cache(&harness).await;
}

/// A session that opens again tells each worker that ended with its cache
/// from the first: no client sees it ended without one.
#[tokio::test]
async fn a_restored_worker_is_told_with_its_cache_from_the_first() {
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    let (_, mut updates, _again) = reopened(&harness);
    let first = std::iter::from_fn(|| updates.try_recv().ok())
        .find_map(|update| match update {
            remote::Update::Worker(summary) if summary.id == "r1" => Some(summary),
            _ => None,
        })
        .expect("r1 was told");
    assert_eq!(first.state, remote::WorkerState::Finished);
    assert_eq!(first.cache().unwrap().cost.tokens, 50_000);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The session's workers as another session over the same folder and
/// workflow finds them: its feed, the feed's updates from before it found
/// them, and its crew.
fn reopened(
    harness: &Harness,
) -> (
    remote::Feed,
    tokio::sync::broadcast::Receiver<remote::Update>,
    Arc<Crew>,
) {
    let feed = remote::Feed::new(MAIN);
    let (_, updates) = feed.subscribe();
    let (again, _) = Crew::new(crew::Setup {
        session_id: harness.session.id.clone(),
        session_dir: harness.session.dir.clone(),
        output: harness.output.clone(),
        max_running: NO_LIMIT,
        cache_1h: false,
        openai_auth: None,
        cache_lifetime: arko_ai::prompt_cache::PromptCacheLifetime::Known(Duration::from_secs(300)),
        feed: Some(feed.clone()),
        refuse: None,
        asker: None,
        root: None,
    });
    again.attach(harness.workflow(), None);
    (feed, updates, again)
}

/// A worker that goes on after its end, and ends again, counts its cache
/// from the new end's last request, which its result holds.
#[tokio::test]
async fn a_worker_that_went_on_counts_from_its_new_last_request() {
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    let crew = harness.crew.clone().unwrap();
    let first = crew.worker_cache("r1").unwrap().expires_at;
    tokio::time::sleep(Duration::from_millis(20)).await;
    turn(&mut harness, &mut inputs, "Continue r1.").await;
    assert!(got(&seen, "r1", "More of the big task."));
    let second = crew.worker_cache("r1").unwrap().expires_at;
    assert!(second > first, "{second} after {first}");
    assert_eq!(
        harness.feed.worker_summary("r1").unwrap().cache().unwrap().expires_at,
        second
    );
    crew.shutdown().await;
}

/// Takes the workers' latest requests out of the session's latest
/// checkpoint and their task files, as an older Arko saved them.
fn saved_by_an_older_arko(harness: &Harness) {
    let checkpoint = harness.session.latest_checkpoint().unwrap();
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&checkpoint).unwrap()).unwrap();
    for record in state["agent_runs"].as_array_mut().unwrap() {
        if record["agent_name"] != WORKER {
            continue;
        }
        if let Some(info) = record["info"].as_object_mut() {
            info.remove("latest_request");
        }
        let file = harness
            .session
            .run_dir()
            .join(record["task_file"].as_str().unwrap());
        let mut task: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        task.as_object_mut().unwrap().remove("latest_request");
        std::fs::write(&file, task.to_string()).unwrap();
    }
    std::fs::write(&checkpoint, state.to_string()).unwrap();
}

/// A worker an older Arko saved has no record of its latest request, so its
/// cache isn't known. The session shows none for it, and continuing it asks
/// first, by `/message` and by `message_agent`.
#[tokio::test]
async fn a_worker_an_older_arko_saved_asks_before_continuing() {
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    harness.crew.clone().unwrap().shutdown().await;
    harness.workflow().checkpoint().await.unwrap();
    saved_by_an_older_arko(&harness);
    harness.open(true).unwrap();
    let crew = harness.crew.clone().unwrap();
    assert_eq!(
        crew.cache_state("r1"),
        Some(crate::worker_cache::WorkerCacheState::Unknown("r1".into()))
    );
    assert_eq!(crew.worker_cache("r1"), None);
    // (A fresh feed, as a session that opens again has.)
    let (feed, _, again) = reopened(&harness);
    assert_eq!(feed.worker_summary("r1").unwrap().cache(), None);
    again.shutdown().await;
    assert!(harness.live_command("/message r1 Check the docs.", Source::Terminal));
    assert!(shown(&harness.feed).iter().any(|(_, text, _)| text
        == "r1's cache isn't known, because an older Arko saved it. Continuing may re-read its whole conversation without it. The same /message again continues it."));
    assert!(!got(&seen, "r1", "Check the docs."));
    let params = std::collections::HashMap::from([("to".to_string(), serde_json::json!("r1"))]);
    assert!(
        crew.uncached_refusal(&params)
            .is_some_and(|refusal| refusal.starts_with("r1's cache isn't known")),
        "{:?}",
        crew.uncached_refusal(&params)
    );
    crew.shutdown().await;
}

/// A client's `/message` to a worker an older Arko saved, sent to the
/// running session, is refused with the question and what confirming
/// takes, and the transcript stays as it was. The same send again asks
/// again, because a refusal confirms nothing. A fresh send, confirmed,
/// continues the worker, whose task then records its latest request, so
/// its cache is known.
#[tokio::test]
async fn a_client_continues_a_worker_an_older_arko_saved_once_it_confirms() {
    use crate::remote::{Confirm, Refusal, RefusalKind, Verdict};
    use crate::worker_cache::WorkerCacheState;
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    harness.crew.clone().unwrap().shutdown().await;
    harness.workflow().checkpoint().await.unwrap();
    saved_by_an_older_arko(&harness);
    harness.open(true).unwrap();
    let crew = harness.crew.clone().unwrap();
    assert_eq!(
        crew.cache_state("r1"),
        Some(WorkerCacheState::Unknown("r1".into()))
    );
    let (requests, receiver) = mpsc::unbounded_channel();
    inputs.requests = receiver;
    let send = move |continue_uncached: bool| {
        let (reply, verdict) = oneshot::channel();
        let input = Input::Message(ui::Message {
            text: "/message r1 Check the docs.".into(),
            continue_uncached,
            ..Default::default()
        });
        requests
            .send(crate::engine::Request::Send { input, reply })
            .unwrap();
        async move {
            tokio::time::timeout(Duration::from_secs(20), verdict)
                .await
                .expect("a verdict in time")
                .expect("a verdict")
        }
    };
    let feed = harness.feed.clone();
    let client = {
        let seen = seen.clone();
        async move {
            let before = shown(&feed);
            let asks = Verdict::Refused(Refusal {
                kind: RefusalKind::Uncached {
                    worker: "r1".into(),
                },
                words: "r1's cache isn't known, because an older Arko saved it. Continuing may re-read its whole conversation without it.".into(),
                confirm: Some(Confirm::ContinueUncached),
            });
            assert_eq!(send(false).await, asks);
            assert_eq!(send(false).await, asks);
            assert_eq!(shown(&feed), before);
            assert!(!got(&seen, "r1", "Check the docs."));
            match send(true).await {
                Verdict::Accepted {
                    delivery: Delivery::Command,
                    item: Some(item),
                    ..
                } => {
                    let shown = feed.subscribe().0.items;
                    let copy = shown.iter().find(|shown| shown.id == item).unwrap();
                    assert_eq!(
                        (copy.agent.as_str(), copy.text.as_str()),
                        ("r1", "Check the docs.")
                    );
                }
                other => panic!("{other:?}"),
            }
            until(|| got(&seen, "r1", "Check the docs.")).await;
            sender.send(Input::Eof).unwrap();
        }
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    until(|| matches!(crew.cache_state("r1"), Some(WorkerCacheState::Known(_)))).await;
    crew.shutdown().await;
}
