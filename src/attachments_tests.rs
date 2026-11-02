// End-to-end tests of the files the web page sends with a message, from
// the page's send to what the model reads and what the transcript shows.
// (Included in `main`'s tests, whose helpers they use.)

use crate::attachments::{Attached, FileRef, Kind};
use crate::mock_model::{ModelServer, complete};
use crate::remote::{RefusalKind, Verdict, VerdictReply};
use tokio::sync::Notify;

/// Sends `action` as the page does, and waits for its verdict.
async fn page_send(actions: &mpsc::UnboundedSender<Action>, action: Action) -> Verdict {
    let (reply, verdict) = VerdictReply::new();
    actions
        .send(Action::Send {
            action: Box::new(action),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), verdict)
        .await
        .expect("a verdict in time")
        .expect("a verdict")
}

/// A PNG of one pixel.
fn one_pixel() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
        .unwrap()
}

/// The file `name` saved in `dir` with `bytes`, as the page sends it.
fn attached(dir: &Path, name: &str, kind: Kind, mime: &str, bytes: &[u8]) -> Attached {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    let lines = (kind == Kind::Text).then(|| bytes.iter().filter(|b| **b == b'\n').count() as u64);
    Attached {
        file: FileRef {
            name: name.into(),
            mime: mime.into(),
            bytes: bytes.len() as u64,
            path,
            lines,
        },
        kind,
        content: (kind != Kind::File).then(|| Arc::from(bytes)),
    }
}

fn three_files(dir: &Path) -> Vec<Attached> {
    vec![
        attached(dir, "bug.png", Kind::Image, "image/png", &one_pixel()),
        attached(
            dir,
            "notes.md",
            Kind::Text,
            "text/markdown",
            b"# Notes\nThe gap is 13 px.\n",
        ),
        attached(dir, "RPReplay.MP4", Kind::File, "video/mp4", &[0; 64]),
    ]
}

/// A model that answers each request, and keeps their bodies.
async fn keeping(bodies: &Arc<Mutex<Vec<String>>>, held: Option<Arc<Notify>>) -> ModelServer {
    let bodies = bodies.clone();
    ModelServer::start(Arc::new(move |_, body: String| {
        let first = {
            let mut kept = lock(&bodies);
            kept.push(body);
            kept.len() == 1
        };
        let held = held.clone();
        Box::pin(async move {
            if first && let Some(held) = held {
                held.notified().await;
            }
            complete("1", "Done.")
        })
    }))
    .await
}

/// A page's message with an image, a short text and a video starts a turn
/// in which the model reads the attachments block (the text whole in it),
/// the image and then what the user typed. The transcript shows only what
/// they typed, with the image. A command with files is refused, and so is
/// a file a chat can't take.
#[tokio::test]
async fn a_pages_files_reach_the_agent_as_their_kinds_say() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let client = async move {
        let message = Action::Message {
            text: "The spacing is wrong.".into(),
            files: three_files(&dir),
        };
        let Verdict::Accepted {
            delivery,
            item,
            partial,
        } = page_send(&actions, message).await
        else {
            panic!("accepted");
        };
        assert_eq!(delivery, Delivery::Turn);
        assert_eq!(partial, None);
        let item = feed.main_item(item.unwrap()).unwrap();
        assert_eq!(item.text, "The spacing is wrong.");
        assert_eq!(item.images.len(), 1);
        assert_eq!(item.images[0].label, "bug.png");
        until(|| !feed.activity().working).await;
        let command = Action::Message {
            text: "/workers".into(),
            files: three_files(&dir),
        };
        let Verdict::Refused(refusal) = page_send(&actions, command).await else {
            panic!("refused");
        };
        assert_eq!(refusal.kind, RefusalKind::Files);
        assert_eq!(
            refusal.words,
            "A command takes no attachments. Remove them, or send them with a message."
        );
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let bodies = lock(&bodies);
    assert_eq!(bodies.len(), 1, "only the message started a turn");
    let body = &bodies[0];
    let block = body.find("<attachments>").expect("the block");
    let typed = body.find("The spacing is wrong.").expect("the text");
    assert!(block < typed, "the block comes first");
    for wanted in [
        "RPReplay.MP4",
        "video/mp4",
        "The gap is 13 px.",
        "as an image after this message",
    ] {
        assert!(body.contains(wanted), "{wanted} in {body}");
    }
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(one_pixel());
    assert!(body.contains(&image), "the image goes to the model");
}

/// With a model that takes no images, the image is saved as a file, the
/// send says so, and the model gets its path instead. A chat refuses a
/// video, which it has no tools to read.
#[tokio::test]
async fn an_image_a_model_cant_take_goes_as_a_file() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    harness.main_config.llm_config.model.supports_image_input = false;
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let client = async move {
        let message = Action::Message {
            text: String::new(),
            files: vec![attached(
                &dir,
                "bug.png",
                Kind::Image,
                "image/png",
                &one_pixel(),
            )],
        };
        let Verdict::Accepted { partial, item, .. } = page_send(&actions, message).await else {
            panic!("accepted");
        };
        let words = partial.and_then(|partial| partial.words).unwrap();
        assert!(
            words.starts_with("bug.png went as a file, because"),
            "{words}"
        );
        assert!(feed.main_item(item.unwrap()).unwrap().images.is_empty());
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let body = lock(&bodies)[0].clone();
    assert!(
        body.contains("bug.png") && body.contains("image/png"),
        "{body}"
    );
    assert!(!body.contains("as an image after this message"), "{body}");

    // A chat.
    let temp = tempfile::tempdir().unwrap();
    let (mut chat, mut inputs, sender) = super::harness(temp.path(), server.client(), true);
    chat.settings.mode = crate::session::Mode::Chat;
    let actions = chat.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let client = async move {
        let message = Action::Message {
            text: "Here.".into(),
            files: vec![attached(
                &dir,
                "RPReplay.MP4",
                Kind::File,
                "video/mp4",
                &[0; 64],
            )],
        };
        let Verdict::Refused(refusal) = page_send(&actions, message).await else {
            panic!("refused");
        };
        assert_eq!(refusal.kind, RefusalKind::Files);
        assert!(
            refusal.words.contains("A chat takes images and text files"),
            "{}",
            refusal.words
        );
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(chat.repl(&mut inputs), client),
    )
    .await
    .unwrap();
}

/// A page's message during a turn becomes a note that carries the block.
/// When the turn ends before the agent reads it, it goes with the next
/// message, its image too.
#[tokio::test]
async fn files_sent_during_a_turn_go_with_its_note() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let held = Arc::new(Notify::new());
    let server = keeping(&bodies, Some(held.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let kept = bodies.clone();
    let client = async move {
        sender.send(Input::Line("Start.".into())).unwrap();
        until(|| feed.activity().working).await;
        let message = Action::Message {
            text: "Also this log.".into(),
            files: vec![
                attached(
                    &dir,
                    "server.log",
                    Kind::Text,
                    "text/plain",
                    b"error at line 9\n",
                ),
                attached(&dir, "bug.png", Kind::Image, "image/png", &one_pixel()),
            ],
        };
        let Verdict::Accepted { delivery, .. } = page_send(&actions, message).await else {
            panic!("accepted");
        };
        assert_eq!(delivery, Delivery::Note);
        held.notify_one();
        until(|| !feed.activity().working && lock(&kept).len() >= 2).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let bodies = lock(&bodies);
    let later = bodies[1..].join("\n");
    assert!(later.contains("error at line 9"), "{later}");
    assert!(later.contains("server.log"), "{later}");
    assert!(later.contains("Also this log."), "{later}");
    assert!(later.contains("as an image after this message"), "{later}");
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(one_pixel());
    assert!(
        later.contains(&image),
        "the image goes with the next message"
    );
}

/// An app's image sent during a turn goes with the next message when the
/// agent didn't get to read its note, and its saved copy goes once it did.
#[tokio::test]
async fn an_unread_notes_image_goes_with_the_next_message() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let held = Arc::new(Notify::new());
    let server = keeping(&bodies, Some(held.clone())).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let session = harness.session.dir.clone();
    let kept = bodies.clone();
    let client = async move {
        sender.send(Input::Line("Start.".into())).unwrap();
        until(|| feed.activity().working).await;
        sender
            .send(Input::Message(crate::ui::Message {
                text: "Look at this too.".into(),
                images: vec![crate::ui::Attachment {
                    bytes: Arc::from(one_pixel()),
                    mime: "image/png".into(),
                    name: Some("dot.png".into()),
                }],
                ..Default::default()
            }))
            .unwrap();
        until(|| {
            shown(&feed)
                .iter()
                .any(|(_, text, _)| text == "Look at this too.")
        })
        .await;
        held.notify_one();
        until(|| !feed.activity().working && lock(&kept).len() >= 2).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let bodies = lock(&bodies);
    let later = bodies[1..].join("\n");
    assert!(later.contains("Look at this too."), "{later}");
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(one_pixel());
    assert!(
        later.contains(&image),
        "the image goes with the next message"
    );
    // Once it went, its file isn't kept.
    let kept: Vec<_> = std::fs::read_dir(session.join("unread-images"))
        .map(|files| files.flatten().map(|file| file.file_name()).collect())
        .unwrap_or_default();
    assert!(kept.is_empty(), "{kept:?}");
}
