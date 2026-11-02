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

/// A page's message shows its files the same live and restored from what
/// the agent read. Live, its item has every file in order, the image among
/// them, and the text as typed. Restored, the attachments block becomes the
/// same files, and the image is named as its file.
#[tokio::test]
async fn a_pages_files_show_the_same_live_and_restored() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let live: Arc<Mutex<Option<crate::remote::Item>>> = Arc::default();
    let shown = live.clone();
    let client = async move {
        let message = Action::Message {
            text: "The spacing is wrong.".into(),
            files: three_files(&dir),
        };
        let Verdict::Accepted { item, .. } = page_send(&actions, message).await else {
            panic!("accepted");
        };
        *lock(&shown) = feed.main_item(item.unwrap());
        until(|| !feed.activity().working).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let live = lock(&live).take().unwrap();
    assert_eq!(live.text, "The spacing is wrong.");
    assert_eq!(
        live.files
            .iter()
            .map(|sent| (sent.file.name.as_str(), sent.shown))
            .collect::<Vec<_>>(),
        [("bug.png", true), ("notes.md", false), ("RPReplay.MP4", false)]
    );
    let Conversation::Answered(result) = &harness.conversation else {
        panic!("answered");
    };
    let task = result.task().unwrap();
    let restored = crate::remote::history(
        &task.conversation.messages,
        task.agent_config.tool_config.ends_task_with_text(),
        &harness.images,
    );
    let user = restored
        .iter()
        .find(|item| item.kind == crate::remote::Kind::User)
        .unwrap();
    assert_eq!(user.text, live.text);
    assert_eq!(user.files, live.files);
    let images = |item: &crate::remote::Item| {
        item.images
            .iter()
            .map(|image| (image.label.clone(), image.mime.clone(), image.bytes))
            .collect::<Vec<_>>()
    };
    assert_eq!(images(user), images(&live));
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
        let Verdict::Accepted { delivery, item, .. } = page_send(&actions, message).await else {
            panic!("accepted");
        };
        assert_eq!(delivery, Delivery::Note);
        // Its note shows what was typed, and its files.
        let note = feed.main_item(item.unwrap()).unwrap();
        assert_eq!(note.text, "Also this log.");
        let names: Vec<&str> = note.files.iter().map(|sent| sent.file.name.as_str()).collect();
        assert_eq!(names, ["server.log", "bug.png"]);
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

/// Asks the session `request` as an app does, and waits for its answer.
async fn ask<T>(
    requests: &mpsc::UnboundedSender<crate::engine::Request>,
    request: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> crate::engine::Request,
) -> Result<T, String> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    requests.send(request(reply)).unwrap();
    tokio::time::timeout(Duration::from_secs(20), answer)
        .await
        .expect("an answer in time")
        .expect("an answer")
}

/// A turn whose message carried files keeps its place in the conversation,
/// so it can be sent again, edited and branched from, live and once
/// restored. Sent again, the model reads it as it did, its files' block and
/// image too. Edited, its files stay as files the agent reads where they
/// are, and the image goes no more. A message of files alone can be sent
/// again too.
#[tokio::test]
async fn a_turn_with_files_can_be_sent_again_and_edited() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let (requests, asked) = mpsc::unbounded_channel();
    inputs.requests = asked;
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let kept = bodies.clone();
    let turns = |count: usize| {
        let (feed, kept) = (feed.clone(), kept.clone());
        move || lock(&kept).len() >= count && !feed.activity().working
    };
    let item = |text: &str| {
        feed.subscribe()
            .0
            .items
            .into_iter()
            .rfind(|item| item.kind == crate::remote::Kind::User && item.text == text)
            .unwrap()
    };
    let client = async {
        let message = Action::Message {
            text: "Look.".into(),
            files: three_files(&dir),
        };
        page_send(&actions, message).await;
        until(turns(1)).await;
        let look = item("Look.");
        assert_eq!(look.message_index, Some(0), "a turn with files keeps its place");
        assert_eq!(
            ask(&requests, |reply| crate::engine::Request::Regenerate { reply }).await,
            Ok(())
        );
        until(turns(2)).await;
        let again = item("Look.");
        assert_eq!(again.files, look.files);
        assert_eq!(again.message_index, Some(0));
        let message = crate::ui::Message {
            text: "Look again.".into(),
            ..Default::default()
        };
        let edited = ask(&requests, |reply| crate::engine::Request::Edit {
            item: again.id,
            message,
            reply,
        })
        .await;
        assert_eq!(edited, Ok(()));
        until(turns(3)).await;
        let edited = item("Look again.");
        assert_eq!(
            edited
                .files
                .iter()
                .map(|sent| (sent.file.name.as_str(), sent.shown))
                .collect::<Vec<_>>(),
            [("bug.png", false), ("notes.md", false), ("RPReplay.MP4", false)]
        );
        // A message of files alone.
        let message = Action::Message {
            text: String::new(),
            files: vec![attached(&dir, "spec.pdf", Kind::File, "application/pdf", b"%PDF-1.7")],
        };
        page_send(&actions, message).await;
        until(turns(4)).await;
        assert_eq!(
            ask(&requests, |reply| crate::engine::Request::Regenerate { reply }).await,
            Ok(())
        );
        until(turns(5)).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(one_pixel());
    let bodies = lock(&bodies);
    // (Each request holds the conversation so far, so the last message is
    // the one sent.)
    let last = |body: &str, wanted: &str| body.rfind(wanted);
    let again = &bodies[1];
    assert!(again.contains("The gap is 13 px."), "{again}");
    assert!(again.contains("RPReplay.MP4"), "{again}");
    assert!(again.contains("as an image after this message"), "{again}");
    assert_eq!(again.matches(&image).count(), 1, "the image goes again, once");
    assert_eq!(again.matches("The gap is 13 px.").count(), 1, "{again}");
    let edited = &bodies[2];
    assert!(last(edited, "Look again.") > last(edited, "RPReplay.MP4"), "{edited}");
    assert!(!edited.contains("as an image after this message"), "{edited}");
    assert!(!edited.contains(&image), "the edit sent no image");
    assert!(edited.contains("The gap is 13 px."), "{edited}");
    assert_eq!(bodies[4].matches("spec.pdf").count(), bodies[3].matches("spec.pdf").count());
    // Restored, the turn has the same place, and passes the same check.
    let Conversation::Answered(result) = &harness.conversation else {
        panic!("answered");
    };
    let task = result.task().unwrap();
    let restored = crate::remote::history(
        &task.conversation.messages,
        task.agent_config.tool_config.ends_task_with_text(),
        &harness.images,
    );
    let pdf = restored
        .iter()
        .rfind(|item| item.kind == crate::remote::Kind::User)
        .unwrap();
    assert_eq!(pdf.files[0].file.name, "spec.pdf");
    let index = pdf.message_index.unwrap();
    let live = feed
        .subscribe()
        .0
        .items
        .into_iter()
        .rfind(|item| item.kind == crate::remote::Kind::User)
        .unwrap();
    assert_eq!(live.message_index, Some(index));
    assert_eq!(harness.message_at(live.id, "sent again"), Ok(index));
}

/// Notes with images restore each with its own, as they showed live: two
/// the agent didn't read, which went in a turn of their own.
#[tokio::test]
async fn notes_restore_with_their_own_images() {
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
        for (text, name) in [("First photo.", "first.png"), ("Second photo.", "second.png")] {
            let files = vec![attached(&dir, name, Kind::Image, "image/png", &one_pixel())];
            let message = Action::Message {
                text: text.into(),
                files,
            };
            assert!(matches!(
                page_send(&actions, message).await,
                Verdict::Accepted { .. }
            ));
        }
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
    let Conversation::Answered(result) = &harness.conversation else {
        panic!("answered");
    };
    let task = result.task().unwrap();
    let restored = crate::remote::history(
        &task.conversation.messages,
        task.agent_config.tool_config.ends_task_with_text(),
        &harness.images,
    );
    assert_eq!(shown_notes(&restored), shown_notes(&harness.feed.subscribe().0.items));
}

/// The user's items of `items`, their words, files and images.
fn shown_notes(items: &[crate::remote::Item]) -> Vec<(String, Vec<String>, Vec<String>)> {
    items
        .iter()
        .filter(|item| item.kind == crate::remote::Kind::User)
        .map(|item| {
            (
                item.text.clone(),
                item.files.iter().map(|sent| sent.file.name.clone()).collect(),
                item.images.iter().map(|image| image.label.clone()).collect(),
            )
        })
        .collect()
}

/// A message `text` of the user's with `images`.
fn with_images(text: &str, images: usize) -> arko_ai::Message {
    let mut contents = vec![arko_ai::Content::Text(text.into())];
    for _ in 0..images {
        contents.push(arko_ai::Content::Image(arko_ai::Image::from_bytes(
            &one_pixel(),
            "image/png",
        )));
    }
    arko_ai::Message::new_user_with_contents(contents)
}

/// The block of a message with one image `name` that went to the model.
fn image_block(name: &str) -> String {
    format!(
        "<attachments>\n<file name=\"{name}\" type=\"image/png\" size=\"70 bytes\" bytes=\"70\" path=\"/w/{name}\" shown=\"as an image after this message\"/>\n</attachments>\n\n"
    )
}

/// Restored, each note keeps the images it went with: notes the agent read
/// in an update (an app's image without a block too, and a note of images
/// alone that shows none), and notes it didn't, before a message with its
/// own image.
#[test]
fn restored_notes_keep_their_own_images() {
    let note = |n: usize, text: &str, line: &str| {
        let text = text.replace('\n', "\n  ");
        format!(
            "- Note {n} from the operator (the person running this workflow), 2026-11-02 08:00 UTC:\n  {text}\n  {line}\n"
        )
    };
    let update = format!(
        "# Workflow context update\n\nNew notes on your task, oldest first.\n{}{}{}{}",
        note(1, &format!("{}First.", image_block("first.png")), "(With an image, shown after this update.)"),
        note(2, "From the app.", "(With 2 images, shown after this update.)"),
        note(3, "", "(With an image, shown after this update.)"),
        note(4, &format!("{}Fourth.", image_block("fourth.png")), "(With an image, shown after this update.)"),
    );
    let unread = format!(
        "# What I typed during your last turn, which you didn't get to read\n\n- {}\n- {}\n\n# My message\n\n{}Mine.",
        format!("{}Fifth.", image_block("fifth.png")).replace('\n', "\n  "),
        "No image.",
        image_block("mine.png"),
    );
    let messages = vec![
        arko_ai::Message::new_user("Start."),
        arko_ai::Message::new_assistant("Working."),
        with_images(&update, 5),
        arko_ai::Message::new_assistant("Done."),
        with_images(&unread, 2),
    ];
    let images = crate::images::Images::default();
    let restored = crate::remote::history(&messages, false, &images);
    let shown = shown_notes(&restored);
    let images_of = |text: &str| {
        shown
            .iter()
            .find(|(words, ..)| words == text)
            .map(|(_, _, images)| images.clone())
            .unwrap()
    };
    assert_eq!(images_of("First."), ["first.png"]);
    assert_eq!(images_of("From the app."), ["image", "image"]);
    assert_eq!(images_of("Fourth."), ["fourth.png"]);
    assert_eq!(images_of("Fifth."), ["fifth.png"]);
    assert!(images_of("No image.").is_empty());
    assert_eq!(images_of("Mine."), ["mine.png"]);
    assert!(shown.iter().all(|(words, ..)| !words.is_empty()), "{shown:?}");
}

/// A message whose inline file holds lines like Arko's framing ("# My
/// message", "# Handover report") keeps its place in the conversation, and
/// can be sent again as it was.
#[tokio::test]
async fn framing_inside_an_inline_file_is_the_files_own() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let (requests, asked) = mpsc::unbounded_channel();
    inputs.requests = asked;
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let kept = bodies.clone();
    let client = async move {
        let notes = b"# My message\n\nWords from a file.\n\n# Handover report\n\nNot one.\n";
        let files = vec![attached(&dir, "notes.md", Kind::Text, "text/markdown", notes)];
        let message = Action::Message {
            text: "Look.".into(),
            files,
        };
        page_send(&actions, message).await;
        until(|| !feed.activity().working && !lock(&kept).is_empty()).await;
        assert_eq!(
            ask(&requests, |reply| crate::engine::Request::Regenerate { reply }).await,
            Ok(())
        );
        until(|| !feed.activity().working && lock(&kept).len() >= 2).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    let item = harness
        .feed
        .subscribe()
        .0
        .items
        .into_iter()
        .find(|item| item.text == "Look.")
        .unwrap();
    assert_eq!(item.message_index, Some(0));
    assert_eq!(harness.message_at(item.id, "sent again"), Ok(0));
    let bodies = lock(&bodies);
    assert_eq!(bodies[1].matches("Words from a file.").count(), 1, "{}", bodies[1]);
}

/// An app's note with an image and a page's note with a file image, which
/// the agent didn't read, each restore with their own image.
#[tokio::test]
async fn an_unread_app_note_keeps_its_image_beside_a_file_note() {
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
        sender
            .send(Input::Message(crate::ui::Message {
                text: "An app photo.".into(),
                images: vec![crate::ui::Attachment {
                    bytes: Arc::from(one_pixel()),
                    mime: "image/png".into(),
                    name: Some("app.png".into()),
                }],
                ..Default::default()
            }))
            .unwrap();
        until(|| {
            feed.subscribe()
                .0
                .items
                .iter()
                .any(|item| item.text == "An app photo.")
        })
        .await;
        let files = vec![attached(&dir, "file.png", Kind::Image, "image/png", &one_pixel())];
        let message = Action::Message {
            text: "A file photo.".into(),
            files,
        };
        page_send(&actions, message).await;
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
    let Conversation::Answered(result) = &harness.conversation else {
        panic!("answered");
    };
    let task = result.task().unwrap();
    let restored = crate::remote::history(
        &task.conversation.messages,
        task.agent_config.tool_config.ends_task_with_text(),
        &harness.images,
    );
    let counts = |items: &[crate::remote::Item]| {
        items
            .iter()
            .filter(|item| item.kind == crate::remote::Kind::User)
            .map(|item| (item.text.clone(), item.images.len()))
            .collect::<Vec<_>>()
    };
    assert_eq!(counts(&restored), counts(&harness.feed.subscribe().0.items));
    let file = restored
        .iter()
        .find(|item| item.text == "A file photo.")
        .unwrap();
    assert_eq!(file.images[0].label, "file.png");
    // The agent was told whose image is whose.
    let bodies = lock(&bodies);
    assert!(
        bodies[1].contains("(With an image, shown after this message.)"),
        "{}",
        bodies[1]
    );
}

/// Sent again after an edit that added an image, a message carries that
/// image again, with its files.
#[tokio::test]
async fn a_retry_after_an_edit_with_an_image_sends_that_image() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = keeping(&bodies, None).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let (requests, asked) = mpsc::unbounded_channel();
    inputs.requests = asked;
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let dir = temp.path().join("work").join(".arko").join("attachments");
    let kept = bodies.clone();
    let client = async move {
        let message = Action::Message {
            text: "Look.".into(),
            files: three_files(&dir),
        };
        page_send(&actions, message).await;
        until(|| !feed.activity().working && !lock(&kept).is_empty()).await;
        let old = feed
            .subscribe()
            .0
            .items
            .into_iter()
            .find(|item| item.text == "Look.")
            .unwrap();
        let message = crate::ui::Message {
            text: "Edited.".into(),
            images: vec![crate::ui::Attachment {
                bytes: Arc::from(other_pixel()),
                mime: "image/png".into(),
                name: Some("edited.png".into()),
            }],
            ..Default::default()
        };
        let edited = ask(&requests, |reply| crate::engine::Request::Edit {
            item: old.id,
            message,
            reply,
        })
        .await;
        assert_eq!(edited, Ok(()));
        until(|| !feed.activity().working && lock(&kept).len() >= 2).await;
        assert_eq!(
            ask(&requests, |reply| crate::engine::Request::Regenerate { reply }).await,
            Ok(())
        );
        until(|| !feed.activity().working && lock(&kept).len() >= 3).await;
        sender.send(Input::Eof).unwrap();
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        futures_util::future::join(harness.repl(&mut inputs), client),
    )
    .await
    .unwrap();
    use base64::Engine as _;
    let (first, added) = (
        base64::engine::general_purpose::STANDARD.encode(one_pixel()),
        base64::engine::general_purpose::STANDARD.encode(other_pixel()),
    );
    let bodies = lock(&bodies);
    for (n, body) in bodies[1..3].iter().enumerate() {
        assert_eq!(body.matches(&added).count(), 1, "request {} has the edit's image", n + 2);
        assert_eq!(body.matches(&first).count(), 0, "request {}", n + 2);
        assert!(body.contains("RPReplay.MP4") && body.contains("The gap is 13 px."));
    }
}

/// A PNG of one pixel of another colour.
fn other_pixel() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==")
        .unwrap()
}

/// Restored, a note of images alone after a note with a file image takes
/// its own image, and the note before keeps one.
#[test]
fn an_image_only_note_at_the_end_keeps_its_image() {
    let first = format!("{}First.", image_block("first.png")).replace('\n', "\n  ");
    let update = format!(
        "# Workflow context update\n\nNew notes on your task, oldest first.\n- Note 1 from the operator (the person running this workflow), 2026-11-02 08:00 UTC:\n  {first}\n  (With an image, shown after this update.)\n- Note 2 from the operator (the person running this workflow), 2026-11-02 08:00 UTC:\n  \n  (With an image, shown after this update.)\n"
    );
    let messages = vec![with_images(&update, 2)];
    let images = crate::images::Images::default();
    let restored = crate::remote::history(&messages, false, &images);
    assert_eq!(
        shown_notes(&restored),
        [("First.".into(), vec!["first.png".into()], vec!["first.png".into()])]
    );
}

/// A message is read from its start and its files' block whole, so framing
/// a file holds is the file's own: in a plain message, after Arko's label,
/// in a message Arko composed with unread notes, and before a handover's
/// report. The notes' images are those their lines say, and an older note
/// without a line owns as many as its block says went as images.
#[test]
fn a_message_reads_the_same_whatever_its_files_hold() {
    use crate::remote::{as_sent, notes_images, own_words, shown_words};
    let inline = "# My message\n\n# Handover report\n\nNo.\n";
    let plain = format!(
        "<attachments>\n<file name=\"notes.md\" type=\"text/markdown\" size=\"{0} bytes\" bytes=\"{0}\" path=\"/w/notes.md\">\n{inline}\n</file>\n</attachments>\n\nLook.",
        inline.len()
    );
    let labelled = format!("(This message answers q3.)\n{plain}\n\n(Still open: q5.)");
    let report = format!("{plain}\n\n# Handover report\n\nThe report.");
    let composed = format!(
        "# What I typed during your last turn, which you didn't get to read\n\n- Earlier.\n  (With an image, shown after this message.)\n- {}\n\n# My message\n\n{labelled}",
        image_block("old.png").replace('\n', "\n  ") + "Older."
    );
    for text in [&plain, &labelled, &report, &composed] {
        assert_eq!(shown_words(text), "Look.", "{text}");
        assert!(as_sent(text).starts_with("<attachments>\n"), "{text}");
        assert!(as_sent(text).contains(inline), "{text}");
    }
    assert_eq!(own_words(&report), plain);
    assert_eq!(notes_images(&plain, 1), 0);
    // One stated, and one guessed from an older note's block.
    assert_eq!(notes_images(&composed, 3), 2);
    assert_eq!(notes_images(&composed, 1), 1);
}
