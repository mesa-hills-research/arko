//! Serves the remote page for a made-up session with a long transcript, and
//! streams replies into it, to try and time the page without a model.
//!
//! ```text
//! cargo run --example remote_demo -- [ITEMS] [ADDRESS]
//! ```
//!
//! `ITEMS` is how many items the transcript starts with (2000), `ADDRESS`
//! where to serve (127.0.0.1:8799, or 0.0.0.0:8799 for other devices). It
//! prints the links to the current page and to the one under `/next/`. A
//! message sent from the page gets a streamed reply. With
//! `ARKO_DEMO_STAMPS=1`, each streamed piece of text ends with the time it
//! was sent (`⟦t=…⟧`, Unix milliseconds), for timing how fast a page shows
//! it. `ARKO_DEMO_PAUSE_MS` is the pause between replies (3000), 0 for a
//! stream that never stops.

use std::sync::Arc;
use std::time::Duration;

use arko::remote::{Action, CallStatus, Delivery, Feed, Info, Item, Kind, Meter, Reading, Source};
use arko_agents::AgentEvent;
use arko_workflows::WorkflowEvent;

struct Still;

impl Meter for Still {
    fn read(&self) -> Reading {
        Reading {
            cost: 3.42,
            context_tokens: Some(48_210),
            ..Reading::default()
        }
    }
}

const REPLY: &str = "Here's what I found. The **parser** reads each line once, so the \
cost is linear in the file's size, and `read_until` stops at the first blank line.\n\n\
```rust\nfn parse(text: &str) -> Vec<Line> {\n    text.lines().map(Line::from).collect()\n}\n```\n\n\
- The tests cover empty input and a missing final newline.\n\
- A [link](https://example.com) and some *emphasis*.\n\n\
The change is small, and the benchmarks didn't move.";

/// A transcript of `count` items, in turns of a message, some calls and a
/// reply.
fn transcript(count: usize) -> Vec<Item> {
    let mut items = Vec::with_capacity(count);
    let mut turn = 0;
    while items.len() < count {
        turn += 1;
        let mut push = |mut item: Item| {
            item.id = items.len() as u64 + 1;
            item.turn = Some(turn);
            items.push(item);
            items.len() as u64
        };
        let mut user = Item::new(
            Kind::User,
            format!("Turn {turn}. Look at the parser and tell me why the last test fails."),
        );
        user.source = Some(Source::Web);
        user.delivery = Some(Delivery::Turn);
        push(user);
        push(Item::new(
            Kind::Text,
            "Reading the parser and its tests first.",
        ));
        for (call, outcome) in [
            ("read_file src/parse.rs", "212 lines"),
            ("shell cargo test -p parse", "1 failed"),
            ("edit_file src/parse.rs", "+4 -1"),
        ] {
            let mut item = Item::new(Kind::Call, call);
            item.status = Some(CallStatus::Ok);
            item.outcome = Some(outcome.into());
            let id = push(item);
            if call.starts_with("shell") {
                let mut result = Item::new(
                    Kind::Result,
                    "running 14 tests\ntest empty ... ok\ntest trailing_newline ... FAILED\n\ntest result: FAILED. 13 passed; 1 failed",
                );
                result.call = Some(id);
                push(result);
            }
        }
        push(Item::new(Kind::Reply, REPLY));
        push(Item::new(Kind::Turn, "38s · $0.04 · 48k context"));
    }
    items.truncate(count);
    items
}

fn chunk(text: String) -> WorkflowEvent {
    WorkflowEvent {
        timestamp: chrono::Utc::now(),
        workflow_run_id: uuid::Uuid::nil(),
        agent_name: "main".into(),
        agent_run_id: uuid::Uuid::nil(),
        event: AgentEvent::AssistantResponseChunk(text),
    }
}

/// Streams a reply into the feed, a few words every 20 ms (about 50
/// appends a second, as a fast model streams).
async fn stream_reply(feed: &Feed, stamps: bool) {
    feed.turn_started();
    let words: Vec<&str> = REPLY.split_inclusive(' ').collect();
    for piece in words.chunks(3) {
        let mut text: String = piece.concat();
        if stamps {
            text.push_str(&format!("⟦t={}⟧ ", chrono::Utc::now().timestamp_millis()));
        }
        feed.event(&chunk(text), None);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    feed.turn_ended("9s · $0.01", None);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let count: usize = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(2000);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:8799".into());
    let stamps = std::env::var("ARKO_DEMO_STAMPS").is_ok_and(|value| value == "1");
    let pause = std::env::var("ARKO_DEMO_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000);
    let key = "ZGVtbyBrZXkgZm9yIHRyeWluZyB0aGUgcGFnZQ".to_string();

    let feed = Feed::new("main");
    feed.restore(transcript(count));
    let (actions, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = arko::remote::Server::start(
        address.parse().expect("an address such as 127.0.0.1:8799"),
        key.clone(),
        Info::default(),
        feed.clone(),
        actions,
        Arc::new(Still),
    )
    .expect("the port is free");
    let addr = server.addr();
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        addr.ip().to_string()
    };
    println!("{count} items");
    println!("current page  http://{host}:{}/#key={key}", addr.port());
    println!(
        "Rust page     http://{host}:{}/next/#key={key}",
        addr.port()
    );

    // A reply streams every few seconds, and after each message sent.
    let streaming = feed.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(pause)).await;
            stream_reply(&streaming, stamps).await;
        }
    });
    while let Some(action) = received.recv().await {
        if let Action::Message(text) = action {
            feed.user(&text, Source::Web, Delivery::Turn);
        }
    }
}
