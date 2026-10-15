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
/// What the user typed during the other's last turn, which its agent
/// didn't read, stays with it: it doesn't reach the first conversation,
/// and goes with the next message once the user goes back to the other.
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
            hold.notify_one();
            until(|| has(&feed, "reply", "Pears noted.", "")).await;
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
            until(|| count(&feed, "notice", "Your next message continues it.") == 2).await;
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

    // Back in the second: what the user typed during its last turn comes
    // with the next message, once.
    let second = request_with(&bodies, "And the colour?");
    assert!(second.contains("Pears are green."), "{second}");
    assert!(!second.contains("Which fruit?"), "{second}");
    assert_eq!(second.matches("Use the metric system.").count(), 1, "{second}");
    assert_eq!(harness.conversations.current, Some(1));
    assert_eq!(summary(&harness), "Green.");
}

/// A line typed during a turn that ends before the agent reads it goes
/// with the next message of the same conversation, once: not also as a
/// note when the run continues.
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
    let next = request_with(&bodies, "And the colour?");
    assert_eq!(next.matches("Use the metric system.").count(), 1, "{next}");
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
    assert_eq!(handed_over.task.handovers.len(), 1);

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

/// The record is kept with the session: resumed (`fin -c`), it lists the
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

    // fin closes, and the session is resumed.
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
