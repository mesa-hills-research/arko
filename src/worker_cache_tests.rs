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

/// A model server for a worker r1 whose conversation is replaced. Its first
/// two requests call a tool, the second reading `input` tokens, and the
/// request that asks for its handover report (over the handover size) reads
/// as many. The first request of its fresh conversation reports with
/// `after` tokens of input, or without `after` waits, after it notifies
/// `fresh`. Any other third request waits too.
async fn replacing(
    input: usize,
    after: Option<usize>,
    fresh: Arc<tokio::sync::Notify>,
) -> ModelServer {
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
            if let (true, Some(after)) = (body.contains("REPORT:"), after) {
                return with_input(complete(&step, "Done after the handover."), after);
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
/// the summary has a cache for it, and the record keeps none.
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
    assert_eq!(harness.feed.worker_summary("r1").unwrap().cache(), None);
    let record = std::fs::read_to_string(harness.session.dir.join("worker-requests.json"))
        .unwrap_or_default();
    assert!(!record.contains("\"input\""), "{record}");
    crew.shutdown().await;
}

/// A handover replaces a worker's conversation with a fresh one that no
/// cache holds. Stopped before a request of the fresh one ended, it has no
/// cache to price or ask about, not the replaced conversation's.
#[tokio::test]
async fn a_worker_stopped_after_its_handover_has_no_cache_of_the_old_conversation() {
    let fresh = Arc::new(tokio::sync::Notify::new());
    let server = replacing(300_000, None, fresh.clone()).await;
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

/// Claude Code's compaction replaces the conversation too. (No mock runs
/// Claude Code, so the compaction is told to the crew as Claude Code's
/// runtime tells it.)
#[tokio::test]
async fn a_worker_stopped_after_a_compaction_has_no_cache_of_the_old_conversation() {
    let fresh = Arc::new(tokio::sync::Notify::new());
    let server = replacing(50_000, None, fresh.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(matches!(
        harness
            .turn(Prompt::User("Start r1.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(20), fresh.notified())
        .await
        .expect("r1 read the file");
    let run = harness
        .workflow()
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap()
        .run_id;
    harness
        .crew
        .as_ref()
        .unwrap()
        .note_request(&arko_workflows::WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: uuid::Uuid::nil(),
            agent_name: WORKER.into(),
            agent_run_id: run,
            event: arko_agents::AgentEvent::ConversationCompacted {
                input_tokens: 50_000,
                compacted_tokens: Some(5_000),
            },
        });
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

/// Puts in the session's record what a crash can leave of r1's: its
/// replaced conversation's latest request, 300k tokens an hour ago, which
/// its end didn't seal.
fn crash_left_the_old_record(harness: &Harness) -> uuid::Uuid {
    let run = harness
        .workflow()
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap()
        .run_id;
    let an_hour_ago = chrono::Utc::now().timestamp_millis() - 3_600_000;
    let record = serde_json::json!({run.to_string(): {"at": an_hour_ago, "input": 300_000}});
    std::fs::write(
        harness.session.dir.join("worker-requests.json"),
        record.to_string(),
    )
    .unwrap();
    run
}

/// After a crash, the saved task decides. When it handed over, and no
/// request of the fresh conversation ended before the crash, the old
/// record goes. After a small request of the fresh conversation, its input
/// counts, from the task's end.
#[tokio::test]
async fn a_restored_record_of_a_replaced_conversation_gives_way_to_the_task() {
    for after in [None, Some(5_000)] {
        let fresh = Arc::new(tokio::sync::Notify::new());
        let server = replacing(300_000, after, fresh.clone()).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        assert!(harness.live_command("/handover-at workers 100k", Source::Terminal));
        let crew = harness.crew.clone().unwrap();
        match after {
            None => {
                assert!(matches!(
                    harness
                        .turn(Prompt::User("Start r1.".into()), &mut inputs)
                        .await,
                    TurnEnd::Answered
                ));
                tokio::time::timeout(Duration::from_secs(20), fresh.notified())
                    .await
                    .expect("r1 handed over");
                crew.stop("r1", "the test stopped it").unwrap();
                until(|| crew.running().is_empty()).await;
            }
            Some(_) => turn(&mut harness, &mut inputs, "Start r1.").await,
        }
        let run = crash_left_the_old_record(&harness);
        let (feed, _, again) = reopened(&harness);
        let record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(harness.session.dir.join("worker-requests.json")).unwrap(),
        )
        .unwrap();
        let restored = feed.worker_summary("r1").unwrap().cache();
        assert_eq!(again.worker_cache("r1"), restored);
        match after {
            None => {
                assert_eq!(restored, None);
                assert_eq!(record.get(run.to_string()), None, "{record}");
            }
            Some(after) => {
                let restored = restored.expect("the fresh conversation's cache");
                assert_eq!(restored.cost.tokens, after);
                // From the task's end, which it keeps to the second.
                let from = restored.expires_at - chrono::Duration::minutes(30);
                assert!(chrono::Utc::now() - from < chrono::Duration::seconds(10));
                assert_eq!(record[run.to_string()]["input"], after);
                assert!(record[run.to_string()]["calls"].is_u64(), "{record}");
            }
        }
        crew.shutdown().await;
    }
}

/// A worker's end seals its record with its run's model calls only while
/// the run's result is the one that ended: told late, after the run went on
/// and ended again, an end leaves the record as it is.
#[tokio::test]
async fn an_end_told_after_the_run_went_on_seals_nothing() {
    let seen: Seen = Arc::default();
    let notify = Arc::new(tokio::sync::Notify::new());
    let server = serve(seen.clone(), notify.clone(), notify).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    turn(&mut harness, &mut inputs, "Start r1.").await;
    let crew = harness.crew.clone().unwrap();
    let run = harness
        .workflow()
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap()
        .run_id;
    let first = crew.requests().get(run).unwrap();
    let first_calls = first.calls.expect("sealed as r1 ended");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let between = chrono::Utc::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    // r1 goes on, and ends again with more calls.
    turn(&mut harness, &mut inputs, "Continue r1.").await;
    assert!(got(&seen, "r1", "More of the big task."));
    let second = crew.requests().get(run).unwrap();
    assert!(second.calls.unwrap() > first_calls, "{second:?}");
    // As if the first end were told only now, with the record still at the
    // first run's last request: it stays unsealed.
    crew.requests().put([(
        run,
        crate::worker_cache::LastRequest {
            calls: None,
            ..first
        },
    )]);
    let ended = |at| arko_workflows::subagents::SubagentEvent {
        timestamp: at,
        workflow_run_id: uuid::Uuid::nil(),
        run_id: run,
        handle: "r1".into(),
        agent_name: WORKER.into(),
        caller_run_id: uuid::Uuid::nil(),
        caller_agent: MAIN.into(),
        kind: arko_workflows::subagents::SubagentEventKind::Finished {
            summary: "Big done.".into(),
        },
    };
    assert!(crew.worker_event(Ok(ended(between))));
    assert_eq!(crew.requests().get(run).unwrap().calls, None);
    // The latest end seals it.
    assert!(crew.worker_event(Ok(ended(chrono::Utc::now()))));
    assert_eq!(crew.requests().get(run).unwrap().calls, second.calls);
    crew.shutdown().await;
}

/// Makes the saved task of run `run` look as Claude Code's compaction
/// leaves it: a conversation that starts after every model call so far,
/// with as many calls as before.
fn compact_saved_task(dir: &std::path::Path, run: uuid::Uuid) {
    fn files(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    files(dir, &mut found);
    let mut compacted = 0;
    for path in found
        .iter()
        .filter(|path| path.to_string_lossy().contains(&run.to_string()))
    {
        let Ok(mut task) =
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(path).unwrap())
        else {
            continue;
        };
        let Some(calls) = task["usage_history"].as_array().map(Vec::len) else {
            continue;
        };
        let model_calls = task["model_call_usage_history"]
            .as_array()
            .map_or(0, Vec::len);
        task["conversation_usage_start"] = calls.into();
        task["conversation_model_call_start"] = model_calls.into();
        std::fs::write(path, task.to_string()).unwrap();
        compacted += 1;
    }
    assert!(compacted > 0, "no saved task of {run}");
}

/// Claude Code's compaction makes no model call, so a worker whose record
/// was sealed before it has as many calls after it. When Arko dies before
/// the record forgets the old conversation, the session that opens again
/// reads the saved task of a run that didn't complete cleanly, and its
/// compacted conversation leaves no cache to ask about.
#[tokio::test]
async fn a_sealed_record_gives_way_to_a_task_compacted_since() {
    let fresh = Arc::new(tokio::sync::Notify::new());
    let server = replacing(50_000, None, fresh.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(matches!(
        harness
            .turn(Prompt::User("Start r1.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(20), fresh.notified())
        .await
        .expect("r1 read the file");
    let crew = harness.crew.clone().unwrap();
    crew.stop("r1", "the test stopped it").unwrap();
    until(|| crew.running().is_empty()).await;
    let run = harness
        .workflow()
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap()
        .run_id;
    // Sealed with the task's model calls as r1 stopped.
    let calls = harness
        .workflow()
        .agent_result(run)
        .unwrap()
        .info()
        .model_calls;
    let sealed = crew.requests().get(run).unwrap();
    assert_eq!((sealed.input, sealed.calls), (50_000, Some(calls)));
    crew.shutdown().await;
    compact_saved_task(&harness.session.dir, run);
    harness.open(true).unwrap();
    let reopened_calls = harness
        .workflow()
        .agent_result(run)
        .unwrap()
        .info()
        .model_calls;
    assert_eq!(reopened_calls, calls, "a compaction adds no model call");
    assert_eq!(crew.requests().get(run), None);
    assert_eq!(crew.worker_cache("r1"), None);
    let record = std::fs::read_to_string(harness.session.dir.join("worker-requests.json"))
        .unwrap_or_default();
    assert!(!record.contains(&run.to_string()), "{record}");
}
