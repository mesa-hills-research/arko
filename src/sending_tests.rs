// End-to-end tests of what becomes of what the user sends: the verdict a
// client that sends with `Request::Send` gets, at the moment its send went
// or was refused, and the notice a send without one gets, as before.
// (Included in `main`'s tests, whose helpers they use.)

use crate::engine::Request;
use crate::mock_model::{ModelServer, complete};
use crate::remote::{RefusalKind, Verdict};
use crate::sending::{Admitted, Phase, Read};
use tokio::sync::Notify;

/// Where an app's sends go: the requests `inputs` reads.
fn wire(inputs: &mut Inputs) -> mpsc::UnboundedSender<Request> {
    let (sender, receiver) = mpsc::unbounded_channel();
    inputs.requests = receiver;
    sender
}

/// Sends `input` as an app does, and waits for its verdict.
async fn send(requests: &mpsc::UnboundedSender<Request>, input: Input) -> Verdict {
    let (reply, verdict) = oneshot::channel();
    requests.send(Request::Send { input, reply }).unwrap();
    tokio::time::timeout(Duration::from_secs(20), verdict)
        .await
        .expect("a verdict in time")
        .expect("a verdict")
}

fn message(text: &str) -> Input {
    Input::Message(ui::Message {
        text: text.into(),
        ..Default::default()
    })
}

fn refused(verdict: &Verdict) -> Option<&RefusalKind> {
    match verdict {
        Verdict::Refused(refusal) => Some(&refusal.kind),
        _ => None,
    }
}

fn accepted(verdict: &Verdict) -> Option<(Delivery, Option<u64>)> {
    match verdict {
        Verdict::Accepted { delivery, item, .. } => Some((*delivery, *item)),
        _ => None,
    }
}

/// While no turn runs, a message is accepted as the turn it starts, as
/// its item, and a command once it ran. A command that doesn't exist or
/// is given wrongly, nothing to send, a message to a worker that isn't
/// there, images the model can't take, and answers to questions that
/// aren't open are refused, and the transcript doesn't repeat it: the
/// client shows why. A stop is accepted as it is read. A send the session
/// hasn't read as it closes is refused as closed.
#[tokio::test]
async fn an_idle_sessions_sends_get_their_verdicts() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("1", "Done.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let requests = wire(&mut inputs);
    // (A model that takes no images.)
    harness.main_config.llm_config.model.supports_image_input = false;
    let feed = harness.feed.clone();
    let client = {
        let requests = requests.clone();
        async move {
            let started = send(&requests, message("Count the pears.")).await;
            let (delivery, item) = accepted(&started).expect("accepted");
            assert_eq!(delivery, Delivery::Turn);
            assert_eq!(
                feed.main_item(item.unwrap()).unwrap().text,
                "Count the pears."
            );
            until(|| !feed.activity().working).await;
            let command = send(&requests, message("/workers")).await;
            assert!(matches!(
                accepted(&command),
                Some((Delivery::Command, Some(_)))
            ));
            let image = ui::Attachment {
                bytes: Arc::from(&b"pixels"[..]),
                mime: "image/png".into(),
                name: None,
            };
            let with_image = |text: &str| {
                Input::Message(ui::Message {
                    text: text.into(),
                    images: vec![image.clone()],
                    ..Default::default()
                })
            };
            for (input, kind) in [
                (message("/nosuch"), RefusalKind::UnknownCommand),
                (message("   "), RefusalKind::Empty),
                (message("/message r1"), RefusalKind::UnknownCommand),
                (message("/message r9 Check it."), RefusalKind::Undelivered),
                (with_image("Look."), RefusalKind::Images),
                (
                    Input::Answer {
                        question: 999,
                        text: "Tabs.".into(),
                    },
                    RefusalKind::NotOpen,
                ),
                (
                    Input::DismissQuestions(vec!["q9".into()]),
                    RefusalKind::NotOpen,
                ),
            ] {
                let shown_before = shown(&feed).len();
                let what = format!("{input:?}");
                let verdict = send(&requests, input).await;
                assert_eq!(refused(&verdict), Some(&kind), "{what}: {verdict:?}");
                assert_eq!(shown(&feed).len(), shown_before, "{what} left no notice");
            }
            // A stop is accepted as it is read.
            assert!(accepted(&send(&requests, Input::Interrupt).await).is_some());
            sender.send(Input::Eof).unwrap();
        }
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    // A send the session hadn't read as it closed.
    let (reply, verdict) = oneshot::channel();
    requests
        .send(Request::Send {
            input: message("Late."),
            reply,
        })
        .unwrap();
    inputs.close_sends();
    assert_eq!(
        refused(&verdict.await.unwrap()),
        Some(&RefusalKind::Closed)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// While a turn runs, a message is accepted as a note once the agent's
/// note board took it, as its pending item, and a command once it ran.
#[tokio::test]
async fn a_turns_sends_get_their_verdicts() {
    let gate = Arc::new(Notify::new());
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = {
        let (gate, held) = (gate.clone(), held.clone());
        ModelServer::start(Arc::new(move |_, body: String| {
            let (gate, held) = (gate.clone(), held.clone());
            Box::pin(async move {
                // The turn's first request waits until the test lets it go.
                if body.contains("Start the work.")
                    && !held.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    gate.notified().await;
                }
                complete("1", "Done.")
            })
        }))
        .await
    };
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let requests = wire(&mut inputs);
    let feed = harness.feed.clone();
    let client = async move {
        let started = send(&requests, message("Start the work.")).await;
        assert_eq!(accepted(&started).map(|(delivery, _)| delivery), Some(Delivery::Turn));
        until(|| held.load(std::sync::atomic::Ordering::SeqCst)).await;
        let note = send(&requests, message("Also check the tests.")).await;
        let (delivery, item) = accepted(&note).expect("accepted");
        assert_eq!(delivery, Delivery::Note);
        let item = feed.main_item(item.unwrap()).unwrap();
        assert!(item.pending && item.text == "Also check the tests.", "{item:?}");
        let command = send(&requests, message("/workers")).await;
        assert!(matches!(
            accepted(&command),
            Some((Delivery::Command, Some(_)))
        ));
        assert_eq!(
            refused(&send(&requests, message("/nosuch")).await),
            Some(&RefusalKind::UnknownCommand)
        );
        gate.notify_one();
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// While the old model writes its report for a switch, a message is
/// refused as waiting for the report, and the transcript doesn't repeat
/// it. Without a verdict to come, the notice says it with its words, as
/// before. A command goes on to run.
#[tokio::test]
async fn while_the_report_is_written_a_message_is_refused_and_a_command_goes() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let notices = |harness: &Harness| {
        shown(&harness.feed)
            .into_iter()
            .filter(|(kind, _, _)| kind == "notice")
            .map(|(_, text, _)| text)
            .collect::<Vec<_>>()
    };
    let (reply, verdict) = oneshot::channel();
    let event = Event::Input(Some(message("Go on.")));
    let reply = crate::sending::Reply::to(Some(reply));
    let Read::Sent(sent) = harness.read_sent(event, reply, &mut inputs, Phase::Reporting).await else {
        panic!("read as sent");
    };
    assert!(harness.admit(sent, Phase::Reporting).is_none());
    match verdict.await.unwrap() {
        Verdict::Refused(refusal) => {
            assert_eq!(refusal.kind, RefusalKind::Reporting);
            assert_eq!(refusal.words, crate::switch::REPORTING);
        }
        other => panic!("{other:?}"),
    }
    assert!(notices(&harness).is_empty());
    let event = Event::Input(Some(message("Go on.")));
    let untracked = crate::sending::Reply::default();
    let Read::Sent(sent) = harness.read_sent(event, untracked, &mut inputs, Phase::Reporting).await else {
        panic!("read as sent");
    };
    assert!(harness.admit(sent, Phase::Reporting).is_none());
    assert_eq!(
        notices(&harness),
        ["“Go on.” wasn't sent. The report is being written. Stopping skips it."]
    );
    let event = Event::Input(Some(message("/workers")));
    let untracked = crate::sending::Reply::default();
    let Read::Sent(sent) = harness.read_sent(event, untracked, &mut inputs, Phase::Reporting).await else {
        panic!("read as sent");
    };
    assert!(matches!(
        harness.admit(sent, Phase::Reporting),
        Some(Admitted::Command { command, .. }) if command == "/workers"
    ));
}

/// Answers to questions some of which are no longer open are accepted for
/// those still open, and the verdict names the others. With none open,
/// they are refused.
#[tokio::test]
async fn answers_some_no_longer_open_are_accepted_in_part() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let run = uuid::Uuid::new_v4();
    harness.feed.questions_run(Some(run));
    ask_later(&mut harness, run, "q1", "Tabs or spaces?");
    let (reply, verdict) = oneshot::channel();
    harness.answer_questions(
        answers(vec![text_answer("q1", "Tabs"), text_answer("q2", "Main")]),
        &mut crate::sending::Reply::to(Some(reply)),
    );
    match verdict.await.unwrap() {
        Verdict::Accepted {
            delivery: Delivery::Answer,
            partial: Some(partial),
            ..
        } => assert_eq!(partial.dropped, ["q2"]),
        other => panic!("{other:?}"),
    }
    let (reply, verdict) = oneshot::channel();
    harness.answer_questions(
        answers(vec![text_answer("q1", "Spaces")]),
        &mut crate::sending::Reply::to(Some(reply)),
    );
    assert_eq!(
        refused(&verdict.await.unwrap()),
        Some(&RefusalKind::NotOpen)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// While Arko only waits for the workers (without a terminal, after the
/// last turn), a client's line or message is refused as closed, and none
/// is left without its verdict.
#[tokio::test]
async fn sends_while_arko_only_waits_for_the_workers_are_refused() {
    use crate::mock_model::tool_call;
    let gate = Arc::new(Notify::new());
    let server = {
        let gate = gate.clone();
        ModelServer::start(Arc::new(move |_, body: String| {
            let gate = gate.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    gate.notified().await;
                    complete("w", "There are 3 files.")
                } else if body.contains("There are 3 files.") {
                    complete("m3", "Final: 3 files.")
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
        }))
        .await
    };
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
    let requests = wire(&mut inputs);
    let end = harness
        .turn(Prompt::User("How many files?".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let client = async move {
        for input in [Input::Line("Hello.".into()), message("Hello.")] {
            let verdict = send(&requests, input).await;
            assert_eq!(refused(&verdict), Some(&RefusalKind::Closed), "{verdict:?}");
        }
        gate.notify_one();
    };
    let (end, ()) = tokio::time::timeout(
        Duration::from_secs(30),
        futures_util::future::join(harness.finish_workers(&mut inputs), client),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A send's verdict dropped unanswered is a mistake in Arko, which tests
/// see.
#[test]
#[should_panic(expected = "dropped unanswered")]
fn a_verdict_dropped_unanswered_is_seen() {
    let (sender, _verdict) = oneshot::channel();
    drop(crate::sending::Reply::to(Some(sender)));
}

/// The words a verdict gives for what of the send didn't go.
fn partial_words(verdict: &Verdict) -> Option<&str> {
    match verdict {
        Verdict::Accepted {
            partial: Some(partial),
            ..
        } => partial.words.as_deref(),
        _ => None,
    }
}

/// When the agent's note board refuses the note that carries a message's
/// answers, the message still went: its questions are settled and the
/// note is kept, which Arko posts again after the next turn. The verdict
/// says so, and a send without one gets the notice it always had.
#[tokio::test]
async fn answers_whose_note_cant_be_posted_now_are_kept_and_said() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("1", "Done.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let requests = wire(&mut inputs);
    let run = uuid::Uuid::new_v4();
    harness.feed.questions_run(Some(run));
    ask_later(&mut harness, run, "q1", "Tabs or spaces?");
    ask_later(&mut harness, run, "q2", "Which branch?");
    harness.notes_refused = true;
    let feed = harness.feed.clone();
    let reply = |text: &str, id: &str| {
        Input::Message(ui::Message {
            text: text.into(),
            answers: vec![id.into()],
            ..Default::default()
        })
    };
    let client = {
        let sender = sender.clone();
        async move {
            let verdict = send(&requests, reply("Tabs.", "q1")).await;
            assert!(
                matches!(
                    verdict,
                    Verdict::Accepted {
                        delivery: Delivery::Answer,
                        ..
                    }
                ),
                "{verdict:?}"
            );
            assert_eq!(
                partial_words(&verdict),
                Some(
                    "Couldn't pass your answers on (Channel error: the note board is closed). Arko tries again after the next turn."
                )
            );
            until(|| !feed.activity().working).await;
            // Without a verdict to come, the notice says it.
            sender.send(reply("Main.", "q2")).unwrap();
            until(|| {
                shown(&feed).iter().any(|(kind, text, _)| {
                    kind == "notice"
                        && text
                            == "Couldn't pass your answers on (Channel error: the note board is closed). Arko tries again after the next turn."
                })
            })
            .await;
            until(|| !feed.activity().working).await;
            sender.send(Input::Eof).unwrap();
        }
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    // Both settled, and a note is kept to post again.
    assert!(harness.feed.open_questions().is_empty());
    assert!(!harness.feed.queued_notes(run).is_empty());
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// While a turn runs, a message whose note the board refuses didn't go,
/// and nothing of it shows. Answers from the panel whose note the board
/// refuses went, kept for the agent, and the verdict says so.
#[tokio::test]
async fn a_note_the_board_refuses_during_a_turn() {
    let gate = Arc::new(Notify::new());
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = {
        let (gate, held) = (gate.clone(), held.clone());
        ModelServer::start(Arc::new(move |_, body: String| {
            let (gate, held) = (gate.clone(), held.clone());
            Box::pin(async move {
                if body.contains("Start the work.")
                    && !held.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    gate.notified().await;
                }
                complete("1", "Done.")
            })
        }))
        .await
    };
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let requests = wire(&mut inputs);
    let run = uuid::Uuid::new_v4();
    harness.feed.questions_run(Some(run));
    ask_later(&mut harness, run, "q1", "Tabs or spaces?");
    harness.notes_refused = true;
    let feed = harness.feed.clone();
    let client = async move {
        let started = send(&requests, message("Start the work.")).await;
        assert!(accepted(&started).is_some());
        until(|| held.load(std::sync::atomic::Ordering::SeqCst)).await;
        let shown_before = shown(&feed).len();
        let note = send(&requests, message("Also check the tests.")).await;
        assert_eq!(refused(&note), Some(&RefusalKind::Undelivered), "{note:?}");
        assert_eq!(shown(&feed).len(), shown_before, "nothing of it shows");
        let answers = Input::AnswerQuestions(answers(vec![text_answer("q1", "Tabs")]));
        let verdict = send(&requests, answers).await;
        assert!(
            partial_words(&verdict).is_some_and(|words| words.starts_with("Couldn't pass your answers on")),
            "{verdict:?}"
        );
        assert!(feed.open_questions().is_empty());
        gate.notify_one();
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Each refusal reads, without a verdict to come, as the notice it always
/// was, word for word, and with one, as its kind, with nothing in the
/// transcript. Where a send without a verdict goes on (an untracked
/// `/message` with images, which drops them), one with a verdict is
/// refused.
#[tokio::test]
async fn refusals_read_as_before_without_a_verdict_and_as_kinds_with_one() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let notices = |harness: &Harness| {
        shown(&harness.feed)
            .into_iter()
            .filter(|(kind, _, _)| kind == "notice")
            .map(|(_, text, _)| text)
            .collect::<Vec<_>>()
    };
    let image = ui::Attachment {
        bytes: Arc::from(&b"pixels"[..]),
        mime: "image/png".into(),
        name: None,
    };
    let with_image = |text: &str| {
        Input::Message(ui::Message {
            text: text.into(),
            images: vec![image.clone()],
            ..Default::default()
        })
    };
    let unknown = |line: &str| {
        crate::commands::unknown(line, crate::commands::of_session()).expect("unknown")
    };
    // (Phase, what is sent, whether the model takes images, the refusal
    // with a verdict, the notice without one.)
    type Case = (Phase, Event, bool, Option<RefusalKind>, Option<String>);
    let cases = |images: &dyn Fn(&str) -> Input| -> Vec<Case> {
        vec![
            (
                Phase::Idle,
                Event::Input(Some(message("/nosuch"))),
                true,
                Some(RefusalKind::UnknownCommand),
                Some(unknown("/nosuch")),
            ),
            (
                Phase::Idle,
                Event::Input(Some(message("   "))),
                true,
                Some(RefusalKind::Empty),
                None,
            ),
            (
                Phase::Idle,
                Event::Input(Some(images("Look."))),
                false,
                Some(RefusalKind::Images),
                Some(
                    "The message wasn't sent (model 'gpt-6-luna' doesn't take images). Send it without images, or switch to a model that takes them."
                        .into(),
                ),
            ),
            (
                Phase::Idle,
                Event::Input(Some(images("/message r1 Look."))),
                true,
                Some(RefusalKind::WorkerTextOnly),
                None,
            ),
            (
                Phase::Idle,
                Event::Input(Some(Input::Answer {
                    question: 999,
                    text: "Tabs.".into(),
                })),
                true,
                Some(RefusalKind::NotOpen),
                Some("That question is no longer open.".into()),
            ),
            (
                Phase::Idle,
                Event::Remote(Action::Message("/exit".into())),
                true,
                Some(RefusalKind::TerminalOnly),
                Some("/exit works only at the terminal.".into()),
            ),
            (
                Phase::Reporting,
                Event::Input(Some(message("Go on."))),
                true,
                Some(RefusalKind::Reporting),
                Some("“Go on.” wasn't sent. The report is being written. Stopping skips it.".into()),
            ),
            // While the report is written, a space before the slash leaves it a
            // command, which doesn't exist.
            (
                Phase::Reporting,
                Event::Input(Some(message(" /nosuch"))),
                true,
                Some(RefusalKind::UnknownCommand),
                Some(unknown("/nosuch")),
            ),
        ]
    };
    let count = cases(&with_image).len();
    for at in 0..count {
        let (phase, event, takes_images, kind, notice) = cases(&with_image).swap_remove(at);
        let what = format!("case {at}");
        harness.main_config.llm_config.model.supports_image_input = takes_images;
        // Without a verdict to come: the notice it always was.
        let before = notices(&harness).len();
        let read = harness
            .read_sent(event, crate::sending::Reply::default(), &mut inputs, phase)
            .await;
        if let Read::Sent(sent) = read {
            let _ = harness.admit(sent, phase);
        }
        assert_eq!(
            &notices(&harness)[before..],
            notice.as_slice(),
            "{what}, without a verdict"
        );
        // With one: its kind, and nothing in the transcript.
        let (phase, event, _, _, _) = cases(&with_image).swap_remove(at);
        let (sender, verdict) = oneshot::channel();
        let before = notices(&harness).len();
        let read = harness
            .read_sent(event, crate::sending::Reply::to(Some(sender)), &mut inputs, phase)
            .await;
        if let Read::Sent(sent) = read {
            assert!(harness.admit(sent, phase).is_none(), "{what} refused");
        }
        assert_eq!(refused(&verdict.await.unwrap()), kind.as_ref(), "{what}");
        assert_eq!(notices(&harness).len(), before, "{what} left no notice");
    }
}

/// Posts `body` to the page's `path`, as the page does, and gives the
/// session's verdict.
async fn post_verdict(
    http: &reqwest::Client,
    base: &str,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> Verdict {
    let response = http
        .post(format!("{base}{path}"))
        .header("cookie", cookie)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{path} {body}");
    response.json().await.unwrap()
}

/// The web page's sends get the session's own verdicts: a message as the
/// turn it starts, and refusals for a command that doesn't exist (also one
/// mistyped in a worker's view), a message to a worker that isn't there,
/// answers to questions and approvals that aren't open, and a send from a
/// page of another feed.
#[tokio::test]
async fn the_web_pages_sends_get_the_sessions_verdicts() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("1", "Done.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let (http, base, cookie) = web(&mut harness).await;
    let feed = harness.feed.clone();
    let client = async move {
        let started = post_verdict(
            &http,
            &base,
            &cookie,
            "/api/message",
            serde_json::json!({"text": "Count the pears."}),
        )
        .await;
        let (delivery, item) = accepted(&started).expect("accepted");
        assert_eq!(delivery, Delivery::Turn);
        assert_eq!(feed.main_item(item.unwrap()).unwrap().text, "Count the pears.");
        until(|| !feed.activity().working).await;
        for (path, body, kind) in [
            ("/api/message", serde_json::json!({"text": "/nosuch"}), RefusalKind::UnknownCommand),
            (
                "/api/message",
                serde_json::json!({"text": "/sesion", "to": "r1"}),
                RefusalKind::UnknownCommand,
            ),
            (
                "/api/message",
                serde_json::json!({"text": "/message r9 Check it."}),
                RefusalKind::Undelivered,
            ),
            (
                "/api/answer",
                serde_json::json!({"question": 999, "text": "Tabs."}),
                RefusalKind::NotOpen,
            ),
            (
                "/api/questions/dismiss",
                serde_json::json!({"ids": ["q9"]}),
                RefusalKind::NotOpen,
            ),
            (
                "/api/questions/answer",
                serde_json::json!({"answers": [{"id": "q9", "text": "Main."}]}),
                RefusalKind::NotOpen,
            ),
            (
                "/api/approvals",
                serde_json::json!({"choices": [{"id": "a9", "approve": true}]}),
                RefusalKind::NotOpen,
            ),
            (
                "/api/message",
                serde_json::json!({"text": "Hello.", "feed": "0000000000000001"}),
                RefusalKind::Stale,
            ),
        ] {
            let verdict = post_verdict(&http, &base, &cookie, path, body.clone()).await;
            assert_eq!(refused(&verdict), Some(&kind), "{path} {body}: {verdict:?}");
        }
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    harness.stop_remote().await;
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A model whose first request with `held` in it waits for `gate`, and
/// which ends every turn with "Done.". `started` says the request came.
async fn held_turn(held: &'static str, gate: Arc<Notify>, started: Arc<std::sync::atomic::AtomicBool>) -> ModelServer {
    ModelServer::start(Arc::new(move |_, body: String| {
        let (gate, started) = (gate.clone(), started.clone());
        Box::pin(async move {
            if body.contains(held) && !started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                gate.notified().await;
            }
            complete("1", "Done.")
        })
    }))
    .await
}

/// Without a terminal, a message while a turn runs waits for a turn of its
/// own, and is accepted as queued, with its item.
#[tokio::test]
async fn a_message_queued_for_a_turn_of_its_own_is_accepted_as_queued() {
    let gate = Arc::new(Notify::new());
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = held_turn("Start the work.", gate.clone(), started.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), false);
    let requests = wire(&mut inputs);
    let feed = harness.feed.clone();
    let client = async move {
        assert!(accepted(&send(&requests, message("Start the work.")).await).is_some());
        until(|| started.load(std::sync::atomic::Ordering::SeqCst)).await;
        let queued = send(&requests, message("Then the docs.")).await;
        let (delivery, item) = accepted(&queued).expect("accepted");
        assert_eq!(delivery, Delivery::Queued);
        assert_eq!(feed.main_item(item.unwrap()).unwrap().text, "Then the docs.");
        gate.notify_one();
        until(|| has(&feed, "user", "Then the docs.", "turn") || !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// While a turn runs, a message that answers a question went as their note
/// even when the board refuses the note and the images with it: the
/// verdict says both wait.
#[tokio::test]
async fn a_note_and_its_images_the_board_refuses_are_said_in_the_verdict() {
    let gate = Arc::new(Notify::new());
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = held_turn("Start the work.", gate.clone(), started.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let requests = wire(&mut inputs);
    let run = uuid::Uuid::new_v4();
    harness.feed.questions_run(Some(run));
    ask_later(&mut harness, run, "q1", "Tabs or spaces?");
    harness.notes_refused = true;
    let feed = harness.feed.clone();
    let client = async move {
        assert!(accepted(&send(&requests, message("Start the work.")).await).is_some());
        until(|| started.load(std::sync::atomic::Ordering::SeqCst)).await;
        let answer = Input::Message(ui::Message {
            text: "Tabs, as in this picture.".into(),
            answers: vec!["q1".into()],
            images: vec![ui::Attachment {
                bytes: Arc::from(&b"pixels"[..]),
                mime: "image/png".into(),
                name: None,
            }],
            ..Default::default()
        });
        let verdict = send(&requests, answer).await;
        assert!(
            matches!(
                verdict,
                Verdict::Accepted {
                    delivery: Delivery::Note,
                    item: Some(_),
                    ..
                }
            ),
            "{verdict:?}"
        );
        let words = partial_words(&verdict).expect("words");
        assert!(
            words.starts_with("Couldn't pass your answers on")
                && words.contains("The images didn't go with it"),
            "{words}"
        );
        gate.notify_one();
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}
