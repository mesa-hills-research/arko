// End-to-end tests of `/resume`: going back to an earlier conversation of
// the session after `/new`, after a handover, and after the session is
// resumed. Each follows a path a user takes, with the mock model, and
// checks what the user sees and what the model is sent. (Included in
// `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call, with_input};
use tokio::sync::Notify;

/// The requests' bodies, as the model got them.
type Bodies = Arc<Mutex<Vec<String>>>;

/// The request that has `text`, the last one if several do.
fn request_with(bodies: &Bodies, text: &str) -> String {
    bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| body.contains(text))
        .unwrap_or_else(|| panic!("no request with {text:?}"))
        .clone()
}

/// The feed's items of `kind`, oldest first.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

/// How many of the feed's items of `kind` are `text`.
fn count(feed: &remote::Feed, kind: &str, text: &str) -> usize {
    texts(feed, kind).iter().filter(|shown| *shown == text).count()
}

/// Answers each message with a reply of its own. The first request of
/// the turn about pears waits for `hold`.
fn fruit_model(bodies: Bodies, hold: Arc<Notify>) -> Arc<crate::mock_model::Respond> {
    Arc::new(move |_, body: String| {
        bodies.lock().unwrap().push(body.clone());
        let hold = hold.clone();
        Box::pin(async move {
            if body.contains("And the colour?") {
                complete("m5", "Green.")
            } else if body.contains("Which fruit?") {
                complete("m4", "Apples.")
            } else if body.contains("What I typed during your last turn") {
                complete("m3", "Metric noted.")
            } else if body.contains("Pears are green.") {
                hold.notified().await;
                complete("m2", "Pears noted.")
            } else {
                complete("m1", "Apples noted.")
            }
        })
    })
}

/// At the prompt: a conversation, `/new` and another, then `/resume` lists
/// both and `/resume 1` goes back to the first. Its history shows again,
/// and the model's next request has that conversation and not the other.
/// What the user typed during the other's last turn, which the user
/// stopped before its agent read it, stays with it: it doesn't reach the
/// first conversation, and goes with the next message once the user goes
/// back to the other.
#[tokio::test]
async fn resume_goes_back_to_the_conversation_new_left() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(fruit_model(bodies.clone(), hold.clone())).await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            let say = |line: &str| sender.send(Input::Line(line.into())).unwrap();
            say("Apples are red.");
            until(|| has(&feed, "reply", "Apples noted.", "")).await;
            say("/new");
            until(|| {
                texts(&feed, "output").iter().any(|text| {
                    text == "The next message starts a new conversation. /resume goes back to the last one."
                })
            })
            .await;
            say("Pears are green.");
            until(|| bodies.lock().unwrap().iter().any(|body| body.contains("Pears are green.")))
                .await;
            say("Use the metric system.");
            until(|| has(&feed, "user", "Use the metric system.", "note")).await;
            sender.send(Input::Interrupt).unwrap();
            until(|| has(&feed, "notice", "Stopped the turn.", "")).await;
            say("/resume");
            until(|| {
                texts(&feed, "output")
                    .iter()
                    .any(|text| text.starts_with("This session's conversations"))
            })
            .await;
            say("/resume 1");
            until(|| has(&feed, "notice", "Your next message continues it.", "")).await;
            say("Which fruit?");
            until(|| has(&feed, "reply", "Apples.", "")).await;
            say("/resume 2");
            until(|| {
                has(
                    &feed,
                    "notice",
                    "Its last turn was stopped. Your next message continues it.",
                    "",
                )
            })
            .await;
            say("And the colour?");
            until(|| has(&feed, "reply", "Green.", "")).await;
            sender.send(Input::Eof).unwrap();
            sender
        }
    });
    tokio::time::timeout(Duration::from_secs(60), harness.repl(&mut inputs))
        .await
        .unwrap();
    let _sender = typing.await.unwrap();

    // The list: newest first, by number, with the current one marked.
    let list = texts(&harness.feed, "output")
        .into_iter()
        .find(|text| text.starts_with("This session's conversations"))
        .unwrap();
    let rows: Vec<&str> = list.lines().collect();
    assert_eq!(rows.len(), 4, "{list}");
    assert!(
        rows[1].starts_with("  2  Pears are green.  (") && rows[1].ends_with(", current)"),
        "{list}"
    );
    assert!(rows[2].starts_with("  1  Apples are red.  ("), "{list}");
    assert_eq!(
        rows[3],
        "Go back to one with /resume and its number, such as /resume 1."
    );

    // Back in the first conversation: its history shows again, and the
    // model has it, without the second's.
    assert!(says(&harness.feed, "Back to conversation 1, last used "));
    assert_eq!(count(&harness.feed, "user", "Apples are red."), 2);
    assert_eq!(count(&harness.feed, "reply", "Apples noted."), 2);
    let first = request_with(&bodies, "Which fruit?");
    assert!(
        first.contains("Apples are red.") && first.contains("Apples noted."),
        "{first}"
    );
    assert!(!first.contains("Pears"), "{first}");
    assert!(!first.contains("Use the metric system."), "{first}");

    // The turn stopped before the agent read the line, so the user is told
    // it waits for the next message.
    assert!(says(
        &harness.feed,
        "(what you typed during the turn, which the agent didn't read, goes with your next message)"
    ));
    // Back in the second: what the user typed during its last turn comes
    // with the next message, once.
    assert!(says(
        &harness.feed,
        "(what you typed during its last turn, which the agent didn't read, goes with your next message)"
    ));
    let second = request_with(&bodies, "And the colour?");
    assert!(second.contains("Pears are green."), "{second}");
    assert!(!second.contains("Which fruit?"), "{second}");
    assert_eq!(second.matches("Use the metric system.").count(), 1, "{second}");
    assert_eq!(harness.conversations.current, Some(1));
    assert_eq!(summary(&harness), "Green.");
    // The line waited, pending, until that message took it, and shows just
    // before it.
    let items = harness.feed.subscribe().0.items;
    let at = |text: &str| items.iter().position(|item| item.text == text).unwrap();
    assert!(!items[at("Use the metric system.")].pending);
    assert_eq!(at("Use the metric system.") + 1, at("And the colour?"));
}

/// A line typed during a turn that ends before the agent reads it starts
/// a turn of its own once the agent replies, and reaches the agent once:
/// not also as a note when the run continues, nor again with the next
/// message.
#[tokio::test]
async fn a_line_the_agent_did_not_read_reaches_it_once() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(fruit_model(bodies.clone(), hold.clone())).await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            let say = |line: &str| sender.send(Input::Line(line.into())).unwrap();
            say("Pears are green.");
            until(|| bodies.lock().unwrap().iter().any(|body| body.contains("Pears are green.")))
                .await;
            say("Use the metric system.");
            until(|| has(&feed, "user", "Use the metric system.", "note")).await;
            hold.notify_one();
            until(|| has(&feed, "reply", "Pears noted.", "")).await;
            until(|| has(&feed, "reply", "Metric noted.", "")).await;
            say("And the colour?");
            until(|| has(&feed, "reply", "Green.", "")).await;
            sender.send(Input::Eof).unwrap();
            sender
        }
    });
    tokio::time::timeout(Duration::from_secs(60), harness.repl(&mut inputs))
        .await
        .unwrap();
    let _sender = typing.await.unwrap();
    // The turn of its own: the line once, under its heading.
    let unread = bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("What I typed during your last turn"))
        .cloned()
        .unwrap();
    assert!(!unread.contains("And the colour?"), "{unread}");
    assert_eq!(
        unread.matches("Use the metric system.").count(),
        1,
        "{unread}"
    );
    let next = request_with(&bodies, "And the colour?");
    assert_eq!(next.matches("Use the metric system.").count(), 1, "{next}");
}

/// After `/new`, the first message names the workers still running from
/// the conversation before, whose reports now come to the new one.
#[tokio::test]
async fn the_next_conversation_hears_of_the_workers_still_running() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start({
        let bodies = bodies.clone();
        let hold = hold.clone();
        Arc::new(move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("Hello again.") {
                    complete("m3", "Hello.")
                } else if body.contains("Count the files.") && !body.contains("Start a worker.") {
                    // The worker, running until the test lets it finish.
                    hold.notified().await;
                    complete("w1", "3 files.")
                } else if body.contains("Started r1") {
                    complete("m2", "r1 is counting.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                }
            })
        })
    })
    .await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        let say = |line: &str| sender.send(Input::Line(line.into())).unwrap();
        say("Start a worker.");
        until(|| has(&feed, "reply", "r1 is counting.", "")).await;
        say("/new");
        until(|| {
            texts(&feed, "output")
                .iter()
                .any(|text| text.starts_with("The next message starts a new conversation."))
        })
        .await;
        say("Hello again.");
        until(|| has(&feed, "reply", "Hello.", "")).await;
        // Its report starts a turn in the new conversation.
        hold.notify_one();
        until(|| count(&feed, "reply", "Hello.") == 2).await;
        sender.send(Input::Eof).unwrap();
        sender
    });
    tokio::time::timeout(Duration::from_secs(60), harness.repl(&mut inputs))
        .await
        .unwrap();
    let _sender = typing.await.unwrap();
    let next = request_with(&bodies, "Hello again.");
    assert!(
        next.contains(
            "Worker r1 is still running from earlier in this session. Its reports and messages come to you in this conversation."
        ),
        "{next}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Whether the feed has a notice starting with `start`.
fn says(feed: &remote::Feed, start: &str) -> bool {
    texts(feed, "notice")
        .iter()
        .any(|text| text.starts_with(start))
}

/// A conversation that handed over is one conversation in one run, listed
/// by the user's first message. Gone back to, it goes on from its fresh
/// context: the report and what followed it, not what the report replaced
/// and not the other conversation.
#[tokio::test]
async fn a_conversation_that_handed_over_goes_on_from_its_report() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("Third.") {
                    complete("m5", "Third done.")
                } else if body.contains("Other topic.") {
                    complete("o1", "Other done.")
                } else if body.contains("REPORT:") {
                    complete("m4", "Second done.")
                } else if body.contains("Your conversation is getting long") {
                    tool_call(
                        "report",
                        arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
                        serde_json::json!({"report": "REPORT: the work so far."}),
                    )
                } else if body.contains("call_m1") {
                    // Over the handover size (200k): the next turn hands over.
                    with_input(complete("m2", "First done."), 210_000)
                } else {
                    tool_call(
                        "m1",
                        message::MESSAGE_USER,
                        serde_json::json!({"message": "Working on it."}),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    for message in ["First.", "Second."] {
        let end = harness
            .turn(Prompt::User(message.into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
    }
    assert_eq!(summary(&harness), "Second done.");
    let handed_over = match &harness.conversation {
        Conversation::Answered(result) => result.clone(),
        _ => unreachable!(),
    };
    assert_eq!(handed_over.task().unwrap().handovers.len(), 1);

    harness.new_conversation();
    let end = harness
        .turn(Prompt::User("Other topic.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let records = &harness.conversations.list;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].runs, [handed_over.agent_run_id]);
    assert_eq!(records[0].title, "First.");
    assert_eq!(records[1].title, "Other topic.");

    harness.resume_command("1").await;
    assert_eq!(harness.conversations.current, Some(0));
    assert_eq!(summary(&harness), "Second done.");
    let end = harness
        .turn(Prompt::User("Third.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Third done.");
    let third = request_with(&bodies, "Third.");
    assert!(
        third.contains("REPORT: the work so far.") && third.contains("Second done."),
        "{third}"
    );
    assert!(!third.contains("call_m1"), "{third}");
    assert!(!third.contains("Other topic."), "{third}");
    // Still one run: a handover isn't a new one.
    assert_eq!(harness.conversations.list[0].runs.len(), 1);
}

/// The record is kept with the session: resumed (`arko -c`), it lists the
/// same conversations and goes on in the one it was in, which isn't the
/// one with the latest run, and shows that one's history. A session saved
/// before the record has one conversation for each run, and goes on with
/// the latest, as it did.
#[tokio::test]
async fn a_resumed_session_lists_its_conversations_and_goes_on_in_its_own() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    // (The turn about pears doesn't wait.)
    hold.notify_one();
    let server = ModelServer::start(fruit_model(bodies.clone(), hold)).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    // Nothing to go back to before a second conversation.
    harness.resume_command("").await;
    assert!(
        texts(&harness.feed, "output")
            .contains(&"There is no earlier conversation to go back to.".to_string())
    );
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;
    harness.resume_command("").await;
    assert_eq!(
        count(
            &harness.feed,
            "output",
            "There is no earlier conversation to go back to."
        ),
        2
    );
    harness.new_conversation();
    harness
        .turn(Prompt::User("Pears are green.".into()), &mut inputs)
        .await;
    harness.resume_command("3").await;
    assert!(
        texts(&harness.feed, "output")
            .contains(&"There is no conversation 3. /resume lists them.".to_string())
    );
    harness.resume_command("2").await;
    assert!(
        texts(&harness.feed, "output")
            .contains(&"You are in conversation 2 already.".to_string())
    );
    harness.resume_command("1").await;

    // Arko closes, and the session is resumed.
    harness.open(true).unwrap();
    assert_eq!(harness.conversations.current, Some(0));
    assert_eq!(summary(&harness), "Apples noted.");
    // Its history shows, not the latest run's.
    let (apples, pears) = (
        count(&harness.feed, "reply", "Apples noted."),
        count(&harness.feed, "reply", "Pears noted."),
    );
    harness.banner_end(false);
    assert_eq!(count(&harness.feed, "reply", "Apples noted."), apples + 1);
    assert_eq!(count(&harness.feed, "reply", "Pears noted."), pears);
    let entries = remote::Meter::read(harness.gauges.as_ref()).conversations;
    let shown: Vec<(usize, &str, bool)> = entries
        .iter()
        .map(|entry| (entry.number, entry.title.as_str(), entry.current))
        .collect();
    assert_eq!(
        shown,
        [(2, "Pears are green.", false), (1, "Apples are red.", true)]
    );
    harness
        .turn(Prompt::User("Which fruit?".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Apples.");
    let first = request_with(&bodies, "Which fruit?");
    assert!(first.contains("Apples are red."), "{first}");
    assert!(!first.contains("Pears"), "{first}");
    // (No note was ever kept, so no list was posted on the way.)
    assert!(lists_held(&bodies).iter().all(|held| *held == 0));

    // Saved before the record: each run is a conversation, the latest
    // current.
    std::fs::remove_file(harness.session.dir.join("conversations.json")).unwrap();
    harness.open(true).unwrap();
    let titles: Vec<&str> = harness
        .conversations
        .list
        .iter()
        .map(|record| record.title.as_str())
        .collect();
    assert_eq!(titles, ["Apples are red.", "Pears are green."]);
    assert_eq!(harness.conversations.current, Some(1));
    assert_eq!(summary(&harness), "Pears noted.");
    assert!(harness.session.dir.join("conversations.json").is_file());

    // Closed after /new, it starts a new conversation, and says how to go
    // back.
    harness.new_conversation();
    harness.open(true).unwrap();
    assert_eq!(harness.conversations.current, None);
    assert!(matches!(harness.conversation, Conversation::New));
    harness.banner_end(false);
    assert!(says(
        &harness.feed,
        "Resumed. The next message starts a new conversation. /resume goes back to the last one."
    ));
}

/// The `# Your kept notes` list in each of the model's requests.
fn lists_held(bodies: &Bodies) -> Vec<usize> {
    bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| body.matches("# Your kept notes").count())
        .collect()
}

/// A note kept in the second conversation reaches the first, which
/// `/resume 1` goes back to, once: in its next request, and not again after.
#[tokio::test]
async fn a_conversation_resumed_gets_the_notes_kept_since() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    // (The turn about pears doesn't wait.)
    hold.notify_one();
    let server = ModelServer::start(fruit_model(bodies.clone(), hold)).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;
    harness.new_conversation();
    harness
        .turn(Prompt::User("Pears are green.".into()), &mut inputs)
        .await;
    let note = "Tear the build box down.";
    harness.kept.add(note, kept_notes::Audience::Lead).unwrap();
    harness.resume_command("1").await;
    for message in ["Which fruit?", "And the colour?"] {
        harness
            .turn(Prompt::User(message.into()), &mut inputs)
            .await;
    }
    assert_eq!(lists_held(&bodies), [0, 0, 1, 1]);
    assert!(request_with(&bodies, "Which fruit?").contains(note));
}

/// A conversation `/resume` goes back to is told when every note was
/// removed since, since it may still hold the list.
#[tokio::test]
async fn a_conversation_resumed_is_told_when_no_notes_are_left() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    // (The turn about pears doesn't wait.)
    hold.notify_one();
    let server = ModelServer::start(fruit_model(bodies.clone(), hold)).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness
        .kept
        .add("Tear the build box down.", kept_notes::Audience::Lead)
        .unwrap();
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;
    harness.new_conversation();
    harness
        .turn(Prompt::User("Pears are green.".into()), &mut inputs)
        .await;
    harness.kept.remove(1).unwrap();
    harness.resume_command("1").await;
    for message in ["Which fruit?", "And the colour?"] {
        harness
            .turn(Prompt::User(message.into()), &mut inputs)
            .await;
    }
    // (The second conversation's first message has the list, the first
    // one's too, and the resumed one's next request says none are left.)
    assert_eq!(lists_held(&bodies), [1, 1, 2, 2]);
    let told = "No notes are kept now. This list replaces any earlier one.";
    assert_eq!(
        request_with(&bodies, "Which fruit?").matches(told).count(),
        1
    );
}

/// A conversation's saved task is read when the conversation is opened or
/// gone back to: one that can't be read fails the session's open, as when
/// every task was read on open, and `/resume` to it says so.
#[tokio::test]
async fn a_conversation_whose_task_cant_be_read_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Arc::new(Notify::new());
    hold.notify_one();
    let server = ModelServer::start(fruit_model(Bodies::default(), hold)).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.turn(Prompt::User("Apples are red.".into()), &mut inputs).await;
    harness.new_conversation();
    harness.turn(Prompt::User("Pears are green.".into()), &mut inputs).await;
    harness.workflow().checkpoint().await.unwrap();
    let checkpoint = harness.session.latest_checkpoint().unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint).unwrap()).unwrap();
    let file_of = |summary: &str| {
        let record = state["agent_runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["summary"] == summary)
            .unwrap();
        harness.session.run_dir().join(record["task_file"].as_str().unwrap())
    };
    let (apples, pears) = (file_of("Apples noted."), file_of("Pears noted."));

    // The earlier conversation's task is read when `/resume` goes back to
    // it, and the session stays in the current one.
    std::fs::remove_file(apples).unwrap();
    harness.open(true).unwrap();
    assert_eq!(summary(&harness), "Pears noted.");
    harness.resume_command("1").await;
    assert!(says(
        &harness.feed,
        "Couldn't go back to conversation 1 because its messages can't be read"
    ));
    assert_eq!(harness.conversations.current, Some(1));
    assert_eq!(summary(&harness), "Pears noted.");
    harness.open(true).unwrap();
    assert_eq!(summary(&harness), "Pears noted.");
    // Branching from a conversation whose task can't be read says so.
    let pears_run = harness.conversation_run();
    let apples_run = harness.conversations.list[0].runs.last().copied();
    harness.conversation = conversation_of(harness.workflow(), apples_run);
    let error = harness.branch(None).unwrap_err();
    assert!(error.starts_with("Couldn't branch because the conversation's"), "{error}");
    harness.conversation = conversation_of(harness.workflow(), pears_run);

    // The current one's is read on open.
    std::fs::remove_file(pears).unwrap();
    let error = harness.open(true).err().unwrap();
    assert!(error.contains("task file"), "{error}");
}

/// The JSON files under `dir`, at any depth.
fn json_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(json_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "json") {
            files.push(path);
        }
    }
    files
}

/// A conversation an older Arko saved, before the main agent kept notes,
/// goes on without them, and takes the main agent's tools now at its next
/// handover: its fresh conversation is offered them.
#[tokio::test]
async fn a_saved_conversation_takes_the_current_tools_at_its_next_handover() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("REPORT:") {
                    complete("m3", "Second done.")
                } else if body.contains("Your conversation is getting long") {
                    tool_call(
                        "report",
                        arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
                        serde_json::json!({"report": "REPORT: the work so far."}),
                    )
                } else if body.contains("call_m1") {
                    // Over the handover size (200k): the next turn hands over.
                    with_input(complete("m2", "First done."), 210_000)
                } else {
                    tool_call(
                        "m1",
                        message::MESSAGE_USER,
                        serde_json::json!({"message": "Working on it."}),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.crew.as_ref().unwrap().shutdown().await;
    // The session as an Arko without kept notes saved it.
    for path in json_files(&harness.session.run_dir()) {
        let text = std::fs::read_to_string(&path).unwrap();
        let older = text
            .replace("\"keep_note\",", "")
            .replace("\"forget_note\",", "");
        std::fs::write(&path, older).unwrap();
    }
    harness.open(true).unwrap();
    harness.start_new_workers();
    assert!(
        harness
            .unoffered
            .as_deref()
            .is_some_and(|notice| notice.contains("kept notes (keep_note), so they get them only from their next handover (")),
        "{:?}",
        harness.unoffered
    );
    let end = harness
        .turn(Prompt::User("Second.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Second done.");
    let offers_notes = |body: &str| body.contains("\"name\":\"keep_note\"");
    let asked = request_with(&bodies, "Your conversation is getting long");
    assert!(!offers_notes(&asked), "the old conversation keeps its tools");
    let fresh = request_with(&bodies, "REPORT: the work so far.");
    assert!(offers_notes(&fresh), "{fresh}");
    harness.crew.as_ref().unwrap().shutdown().await;
}
