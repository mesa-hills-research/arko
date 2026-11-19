// End-to-end tests of the user's messages while the main agent waits for the
// answer to its question. Only the answer panel answers (or a message the user
// chose to send as the answer), and a message from the box reaches the agent
// at once, in Arko's own runtime and in Claude Code alike, with the question
// left open. At the line prompt, which has no answer panel, the next line
// answers. (Included in `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};

/// The question every test's agent asks.
const QUESTION: &str = "Tabs or spaces?";

/// The model's bodies, in the order they came.
type Bodies = Arc<Mutex<Vec<String>>>;

/// The harness as the full-screen view runs it, where lines are what the
/// message box sends.
fn on_screen(harness: &Harness) {
    harness.output.to_feed(harness.feed.clone());
}

/// Whether the agent waits for its question's answer, as the feed says.
fn waits(feed: &remote::Feed) -> bool {
    feed.activity().question.is_some()
}

/// The question, as the session has it now.
fn the_question(harness: &Harness) -> crate::questions::AgentQuestion {
    harness
        .feed
        .questions()
        .into_iter()
        .find(|question| question.question == QUESTION)
        .expect("the agent's question")
}

/// The texts of the user's items in the transcript, in its order.
fn users(feed: &remote::Feed) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "user")
        .map(|(_, text, _)| text)
        .collect()
}

/// The user's items of the transcript a resumed session rebuilds from the
/// conversation, with their deliveries.
fn resumed_users(harness: &Harness) -> Vec<(String, Option<Delivery>)> {
    let task = harness.conversation_task().expect("the conversation");
    remote::history(
        &task.conversation.messages,
        harness.settings.text_replies,
        &crate::images::Images::default(),
    )
    .into_iter()
    .filter(|item| item.kind == remote::Kind::User)
    .map(|item| (item.text, item.delivery))
    .collect()
}

/// An `ask_user` call that waits for the answer.
fn asks(id: &str) -> String {
    tool_call(
        id,
        ASK_USER_TOOL_NAME,
        serde_json::json!({"questions": [QUESTION], "wait": true}),
    )
}

/// Claude Code (`cc:opus`) as arko's scripted stand-in plays it, with
/// `script` (its turns), for `harness`. Returns the fake's folder and the
/// terminal managers.
fn in_claude_code(
    harness: &mut Harness,
    temp: &Path,
    script: serde_json::Value,
) -> (PathBuf, Managers) {
    let fake = temp.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(harness, &cc, &cc, &fake_claude_code(&fake));
    (fake, managers)
}

/// The user messages the fake CLI was sent, in order.
fn sent_to_claude_code(fake: &Path) -> Vec<String> {
    let (log, _) = fake_log(fake);
    log.iter()
        .filter(|entry| entry["event"] == "user" || entry["event"] == "midturn")
        .map(|entry| entry.to_string())
        .collect()
}

/// A note typed while the agent worked, which it hadn't read when it began
/// to wait for its question's answer, reaches it at once. The agent goes on
/// with it, and the question stays open.
#[tokio::test]
async fn a_note_the_agent_had_not_read_when_it_began_to_wait_reaches_it() {
    let bodies: Bodies = Arc::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, release) = (bodies.clone(), release.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let release = release.clone();
            Box::pin(async move {
                if body.contains("Testing testing.") {
                    complete("m2", "Got your note.")
                } else {
                    // (The note comes while the agent decides to ask.)
                    release.notified().await;
                    asks("m1")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    on_screen(&harness);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        sender.send(Input::Line("Testing testing.".into())).unwrap();
        until(|| has(&feed, "user", "Testing testing.", "note")).await;
        release.notify_one();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .expect("the note reached the waiting agent, and it went on");
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Got your note.");
    let bodies = bodies.lock().unwrap().clone();
    let last = bodies.last().unwrap();
    assert!(
        last.contains(crate::questions::WAIT_MESSAGE),
        "the wait says why it ended: {last}"
    );
    assert_eq!(last.matches("Testing testing.").count(), 1, "{last}");
    let question = the_question(&harness);
    assert!(question.state.is_open() && !question.waits, "{question:?}");
    assert!(!waits(&harness.feed));
    // A resumed session shows the note as the user wrote it, and nothing
    // of the wait's end.
    assert_eq!(
        resumed_users(&harness),
        [
            ("Indent the file.".to_string(), Some(Delivery::Turn)),
            ("Testing testing.".to_string(), Some(Delivery::Note)),
        ]
    );
}

/// The same in Claude Code, where the agent begins to wait as its turn
/// ends. A note typed after it asked, while its turn ended, reaches it in a
/// turn of its own.
#[tokio::test]
async fn a_note_typed_as_a_claude_code_turn_ends_with_a_question_reaches_the_agent() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), LLMClient::new(), true);
    on_screen(&harness);
    let script = serde_json::json!([
        [
            {"tool": "mcp__arko__ask_user", "input": {"questions": [QUESTION]}},
            {"sleep": 1.5},
            {"text": "Waiting for your answer."},
        ],
        [{"tool": "mcp__arko__complete_task", "input": {"summary": "Got your note."}}],
    ]);
    let (fake, managers) = in_claude_code(&mut harness, temp.path(), script);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        // Once the call ran, while the turn ends.
        until(|| !feed.open_questions().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        sender.send(Input::Line("Testing testing.".into())).unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .expect("the note reached the waiting agent, and it went on");
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Got your note.");
    let sent = sent_to_claude_code(&fake);
    assert!(
        sent.iter()
            .any(|message| message.contains("Testing testing.")),
        "{sent:#?}"
    );
    assert!(
        !sent
            .iter()
            .any(|message| message.contains("(This message answers q1.)")),
        "{sent:#?}"
    );
    let question = the_question(&harness);
    assert!(question.state.is_open() && !question.waits, "{question:?}");
    assert_eq!(
        resumed_users(&harness),
        [
            ("Indent the file.".to_string(), Some(Delivery::Turn)),
            ("Testing testing.".to_string(), Some(Delivery::Note)),
        ]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A message from the message box while the agent waits doesn't answer
/// its question. It reaches the agent at once, as what it is, and the
/// question stays open in the answer panel, which still answers it.
#[tokio::test]
async fn a_message_from_the_box_leaves_the_question_open() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("The user answered q1.") {
                    complete("m3", "Spaces it is.")
                } else if body.contains("Testing testing.") {
                    complete(
                        "m2",
                        "Noted. The question about indenting still waits for your answer.",
                    )
                } else {
                    asks("m1")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    on_screen(&harness);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| waits(&feed)).await;
        sender.send(Input::Line("Testing testing.".into())).unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let question = the_question(&harness);
    assert!(question.state.is_open(), "{question:?}");
    let last = bodies.lock().unwrap().last().unwrap().clone();
    assert!(last.contains(crate::questions::WAIT_MESSAGE), "{last}");
    assert!(last.contains("(Still open: q1.)"), "{last}");
    assert!(!last.contains("(This message answers q1.)"), "{last}");
    assert!(has(&harness.feed, "user", "Testing testing.", "note"));
    assert_eq!(
        resumed_users(&harness),
        [
            ("Indent the file.".to_string(), Some(Delivery::Turn)),
            ("Testing testing.".to_string(), Some(Delivery::Note)),
        ]
    );
    // The answer panel still answers it, and its answer starts a turn.
    assert!(
        harness
            .answer_questions(
                answers(vec![text_answer(&question.id, "Spaces")]),
                &mut crate::sending::Reply::default()
            )
            .is_some()
    );
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(30),
            harness.deliver_answers(&mut inputs)
        )
        .await
        .unwrap(),
        News::Done
    ));
    assert_eq!(summary(&harness), "Spaces it is.");
}

/// The same in Claude Code, where the agent waits once its turn ended.
#[tokio::test]
async fn a_message_from_the_box_leaves_the_question_open_in_claude_code() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), LLMClient::new(), true);
    on_screen(&harness);
    let script = serde_json::json!([
        [
            {"tool": "mcp__arko__ask_user", "input": {"questions": [QUESTION]}},
            {"text": "Waiting for your answer."},
        ],
        [{"tool": "mcp__arko__complete_task", "input": {"summary": "Noted."}}],
    ]);
    let (fake, managers) = in_claude_code(&mut harness, temp.path(), script);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| waits(&feed)).await;
        sender.send(Input::Line("Testing testing.".into())).unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(
        matches!(end, TurnEnd::Answered),
        "{:#?}",
        shown(&harness.feed)
    );
    let _sender = typing.await.unwrap();
    let question = the_question(&harness);
    assert!(question.state.is_open(), "{question:?}");
    let sent = sent_to_claude_code(&fake);
    assert!(
        sent.iter()
            .any(|message| message.contains("Testing testing.")
                && message.contains("(Still open: q1.)")),
        "{sent:#?}"
    );
    assert!(
        !sent
            .iter()
            .any(|message| message.contains("(This message answers q1.)")),
        "{sent:#?}"
    );
    assert_eq!(
        resumed_users(&harness),
        [
            ("Indent the file.".to_string(), Some(Delivery::Turn)),
            ("Testing testing.".to_string(), Some(Delivery::Note)),
        ]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A message from the web page is an ordinary message too, also when the
/// terminal is the line prompt.
#[tokio::test]
async fn a_message_from_the_page_leaves_the_question_open() {
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        Box::pin(async move {
            if body.contains("Testing testing.") {
                complete("m2", "Noted.")
            } else {
                asks("m1")
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let (page, remote) = mpsc::unbounded_channel();
    inputs.remote = remote;
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| waits(&feed)).await;
        page.send(Action::Message {
            text: "Testing testing.".into(),
            files: Vec::new(),
        })
        .unwrap();
        page
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _page = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Noted.");
    assert!(the_question(&harness).state.is_open());
}

/// Two messages typed while the agent asks show in the order they were
/// sent: one typed as the agent asks, before it waits, then one typed
/// while it goes on.
#[tokio::test]
async fn messages_typed_while_the_agent_asks_show_in_the_order_sent() {
    let release = Arc::new(tokio::sync::Notify::new());
    let second = Arc::new(tokio::sync::Notify::new());
    let server = ModelServer::start(Arc::new({
        let (release, second) = (release.clone(), second.clone());
        move |_, body: String| {
            let (release, second) = (release.clone(), second.clone());
            Box::pin(async move {
                if body.contains("Second message.") {
                    complete("m4", "Both read.")
                } else if body.contains("First message.") {
                    second.notified().await;
                    complete("m3", "Read the first.")
                } else {
                    release.notified().await;
                    asks("m1")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    on_screen(&harness);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        sender.send(Input::Line("First message.".into())).unwrap();
        until(|| has(&feed, "user", "First message.", "note")).await;
        release.notify_one();
        // The agent asks, and its first message reaches it (or it waits).
        tokio::time::sleep(Duration::from_secs(1)).await;
        sender.send(Input::Line("Second message.".into())).unwrap();
        until(|| users(&feed).iter().any(|text| text == "Second message.")).await;
        second.notify_one();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(users(&harness.feed), ["First message.", "Second message."]);
    assert!(the_question(&harness).state.is_open());
}

/// A message sent as the answer to an earlier question, which the agent
/// didn't wait for, reaches it at once while it waits for another. The
/// one it answers is settled, and the one waited for stays open.
#[tokio::test]
async fn an_answer_to_another_question_reaches_an_agent_that_waits() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("(This message answers q1.)") {
                    complete("m3", "Squashing them.")
                } else if body.contains("Asked q1.") {
                    asks("m2")
                } else {
                    tool_call(
                        "m1",
                        ASK_USER_TOOL_NAME,
                        serde_json::json!({"questions": ["Squash the commits?"], "wait": false}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    on_screen(&harness);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| waits(&feed)).await;
        sender
            .send(Input::Message(crate::ui::Message {
                text: "Yes, squash them.".into(),
                answers: vec!["q1".into()],
                ..Default::default()
            }))
            .unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Tidy the branch.".into()), &mut inputs),
    )
    .await
    .expect("the answer reached the waiting agent, and it went on");
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Squashing them.");
    let last = bodies.lock().unwrap().last().unwrap().clone();
    assert!(last.contains(crate::questions::WAIT_MESSAGE), "{last}");
    assert!(last.contains("(Still open: q2.)"), "{last}");
    let state = |id: &str| {
        harness
            .feed
            .questions()
            .into_iter()
            .find(|question| question.id == id)
            .unwrap()
            .state
    };
    assert_eq!(
        state("q1"),
        crate::questions::QuestionState::AnsweredInMessage
    );
    assert!(state("q2").is_open());
}
