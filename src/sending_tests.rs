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
    inputs.reply = Some(reply);
    let event = Event::Input(Some(message("Go on.")));
    let Read::Sent(sent) = harness.read_sent(event, &mut inputs, Phase::Reporting).await else {
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
    let Read::Sent(sent) = harness.read_sent(event, &mut inputs, Phase::Reporting).await else {
        panic!("read as sent");
    };
    assert!(harness.admit(sent, Phase::Reporting).is_none());
    assert_eq!(
        notices(&harness),
        ["“Go on.” wasn't sent. The report is being written. Stopping skips it."]
    );
    let event = Event::Input(Some(message("/workers")));
    let Read::Sent(sent) = harness.read_sent(event, &mut inputs, Phase::Reporting).await else {
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
