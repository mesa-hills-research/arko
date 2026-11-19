// End-to-end tests of the agents' file editing. New sessions edit files
// with `apply_diff`, several changes to a file in one call, and sessions
// saved while the agents had `edit_file` resume with it. Each follows a
// path a user takes, with the mock model, and checks what the model is
// offered and what the user sees. (Included in `main`'s tests, whose
// helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};

/// The requests' bodies, as the model got them.
type Bodies = Arc<Mutex<Vec<String>>>;

/// The names of the tools a request offered.
fn tools_of(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The feed's calls, with their outcomes.
fn call_outcomes(feed: &remote::Feed) -> Vec<(String, String)> {
    feed.subscribe()
        .0
        .items
        .iter()
        .filter_map(|item| {
            let item = serde_json::to_value(item).unwrap();
            (item["kind"] == "call").then(|| {
                (
                    item["text"].as_str().unwrap_or_default().to_string(),
                    item["outcome"].as_str().unwrap_or_default().to_string(),
                )
            })
        })
        .collect()
}

/// The lines printed at the line prompt.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl crate::ui::Printer for Lines {
    fn print(&mut self, text: String) -> Result<(), ()> {
        let mut lines = self.0.lock().unwrap();
        lines.extend(text.lines().map(str::to_string));
        Ok(())
    }
}

/// A diff of `blocks`, each its search lines and its replace lines.
fn diff(blocks: &[(&str, &str)]) -> String {
    blocks
        .iter()
        .map(|(search, replace)| {
            let part = |text: &str| {
                if text.is_empty() {
                    String::new()
                } else {
                    format!("{text}\n")
                }
            };
            format!(
                "<<<<<<< SEARCH\n{}=======\n{}>>>>>>> REPLACE\n",
                part(search),
                part(replace)
            )
        })
        .collect()
}

/// The agents as Arko built them while they edited with `edit_file`.
fn edit_with_edit_file(harness: &mut Harness) {
    for config in [&mut harness.main_config, &mut harness.worker_config] {
        let tools = &mut config.tool_config;
        for name in &mut tools.available_tool_names {
            if name == APPLY_DIFF_TOOL_NAME {
                *name = EDIT_FILE_TOOL_NAME.into();
            }
        }
        let policy = tools
            .tool_specific_configs
            .remove(APPLY_DIFF_TOOL_NAME)
            .unwrap();
        tools
            .tool_specific_configs
            .insert(EDIT_FILE_TOOL_NAME.into(), policy);
    }
}

/// A new session offers `apply_diff`, not `edit_file`. One call makes
/// several edits to a file, and each call shows as "edit PATH" with how
/// many edits it made, or with `replace_all` "(all)" and the replacements.
/// The line prompt, which shows no outcome, says how many edits a call of
/// several makes.
#[tokio::test]
async fn a_new_session_edits_files_with_apply_diff() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("call_m2") {
                    complete("m3", "Renamed.")
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        APPLY_DIFF_TOOL_NAME,
                        serde_json::json!({"path": "notes.txt", "diff": diff(&[("pear", "plum")]), "replace_all": true}),
                    )
                } else {
                    tool_call(
                        "m1",
                        APPLY_DIFF_TOOL_NAME,
                        serde_json::json!({"path": "notes.txt", "diff": diff(&[("apple", "fig"), ("kiwi", "lime")])}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let lines = Lines::default();
    harness.output = Arc::new(Output::new(Some(Box::new(lines.clone())), false));
    harness.open(false).unwrap();
    let notes = harness.settings.workspace.join("notes.txt");
    std::fs::write(&notes, "apple\npear\npear\nkiwi\n").unwrap();
    let end = harness
        .turn(Prompt::User("Rename the fruit.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "fig\nplum\nplum\nlime\n"
    );
    let tools = tools_of(&bodies.lock().unwrap()[0]);
    assert!(tools.contains(&APPLY_DIFF_TOOL_NAME.to_string()), "{tools:?}");
    assert!(!tools.contains(&EDIT_FILE_TOOL_NAME.to_string()), "{tools:?}");
    let calls = call_outcomes(&harness.feed);
    assert_eq!(
        calls,
        [
            ("edit notes.txt".to_string(), "2 edits".to_string()),
            ("edit notes.txt (all)".to_string(), "2 edits".to_string()),
        ]
    );
    let printed = lines.0.lock().unwrap().clone();
    for line in ["● edit notes.txt (2 edits)", "● edit notes.txt (all)"] {
        assert!(printed.iter().any(|shown| shown == line), "{line}: {printed:?}");
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A session saved while the agents had `edit_file` resumes with its saved
/// tool list. It isn't told it lacks the file tools, `edit_file` is still
/// offered and works, and its edits show as before.
#[tokio::test]
async fn a_session_saved_with_edit_file_resumes_with_it() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("call_m2") {
                    complete("m3", "Changed.")
                } else if body.contains("Change it.") {
                    tool_call(
                        "m2",
                        EDIT_FILE_TOOL_NAME,
                        serde_json::json!({"path": "notes.txt", "old_string": "apple", "new_string": "fig"}),
                    )
                } else {
                    complete("m1", "Noted.")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    edit_with_edit_file(&mut harness);
    harness.open(false).unwrap();
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;

    // Arko now builds its agents with apply_diff, and the session is resumed.
    harness.main_config = agents::main_config(&agent_settings_of(&harness)).unwrap();
    harness.worker_config = agents::worker_config(&agent_settings_of(&harness)).unwrap();
    harness.open(true).unwrap();
    assert!(harness.unoffered.is_none(), "{:?}", harness.unoffered);
    assert!(harness.outside.files_follow());
    let notes = harness.settings.workspace.join("notes.txt");
    std::fs::write(&notes, "apple\npear\n").unwrap();
    let end = harness
        .turn(Prompt::User("Change it.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "fig\npear\n");
    let body = bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| body.contains("Change it."))
        .unwrap()
        .clone();
    let tools = tools_of(&body);
    assert!(tools.contains(&EDIT_FILE_TOOL_NAME.to_string()), "{tools:?}");
    assert!(!tools.contains(&APPLY_DIFF_TOOL_NAME.to_string()), "{tools:?}");
    assert!(
        call_outcomes(&harness.feed).contains(&("edit notes.txt".to_string(), "1 edit".to_string()))
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Each of `edits`, a file, its text and a call's parameters, made in turn
/// in one session, `tool` for every call, and each call's item and its
/// result's item in the feed.
async fn edits_shown(
    tool: &'static str,
    edits: &[(&str, &str, serde_json::Value)],
    old_session: bool,
) -> Vec<(remote::Item, Option<remote::Item>)> {
    let calls: Vec<(String, serde_json::Value)> = edits
        .iter()
        .map(|(file, _, parameters)| {
            let mut parameters = parameters.clone();
            parameters["path"] = (*file).into();
            (file.to_string(), parameters)
        })
        .collect();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let calls = calls.clone();
        Box::pin(async move {
            // The next edit, after the results of those before it.
            let done = (1..=calls.len())
                .take_while(|n| body.contains(&format!("call_e{n}\"")))
                .count();
            match calls.get(done) {
                Some((_, parameters)) => {
                    tool_call(&format!("e{}", done + 1), tool, parameters.clone())
                }
                None => complete("done", "Edited."),
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    if old_session {
        edit_with_edit_file(&mut harness);
    }
    harness.open(false).unwrap();
    for (file, text, _) in edits {
        std::fs::write(harness.settings.workspace.join(file), text).unwrap();
    }
    let end = harness
        .turn(Prompt::User("Edit the files.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let items = harness.feed.subscribe().0.items;
    let shown = items
        .iter()
        .filter(|item| item.tool.as_deref() == Some(tool))
        .map(|call| {
            let block = items.iter().find(|item| item.call == Some(call.id));
            (call.clone(), block.cloned())
        })
        .collect();
    harness.crew.as_ref().unwrap().shutdown().await;
    shown
}

/// Checks that each of `shown` has its diff in `expected`, and that a call
/// with none failed and shows why.
fn check_diffs(shown: &[(remote::Item, Option<remote::Item>)], expected: &[Option<&str>]) {
    assert_eq!(shown.len(), expected.len());
    for ((call, block), expected) in shown.iter().zip(expected) {
        let file = &call.text;
        match expected {
            Some(diff) => {
                let block = block.as_ref().unwrap_or_else(|| panic!("{file}: no diff"));
                assert_eq!(block.content, Some(remote::ContentKind::Diff), "{file}");
                assert_eq!(block.text, *diff, "{file}");
            }
            None => {
                assert_eq!(call.status, Some(remote::CallStatus::Failed), "{file}");
                let block = block.as_ref().unwrap_or_else(|| panic!("{file}: no reason"));
                assert_eq!(block.content, Some(remote::ContentKind::Muted), "{file}");
            }
        }
    }
}

/// An apply_diff call's diff in the transcript is the edit that ran: the
/// whole lines each of its blocks changed as the file had them before and
/// has them after, numbered, with the lines around them, and its changes
/// far apart with "…" between them. That holds for lines replaced, added
/// and deleted, a file with CRLF line breaks, tabs and wide characters,
/// and every match of a block of all of them. A call with a block that
/// didn't apply shows why it failed and no diff.
#[tokio::test]
async fn an_apply_diff_calls_diff_is_the_edit_that_ran() {
    let far: String = (1..=12)
        .map(|n| match n {
            2 | 11 => "x\n".to_string(),
            n => format!("{n}\n"),
        })
        .collect();
    let edits: Vec<(&str, &str, serde_json::Value)> = vec![
        (
            "a.txt",
            "one\ntwo\nthree\n",
            serde_json::json!({"diff": diff(&[("two", "2\n2.5")])}),
        ),
        (
            "b.txt",
            "a\nb\nc\n",
            serde_json::json!({"diff": diff(&[("b", "")])}),
        ),
        (
            "c.txt",
            "a\r\nb\r\nc\r\n",
            serde_json::json!({"diff": diff(&[("b", "B")])}),
        ),
        (
            "d.txt",
            "\tlet 名前 = 1;\n",
            serde_json::json!({"diff": diff(&[("\tlet 名前 = 1;", "\tlet 名前 = 2;")])}),
        ),
        (
            "e.txt",
            "a\n",
            serde_json::json!({"diff": diff(&[("a", "b"), ("zzz", "y")])}),
        ),
        (
            "f.txt",
            &far,
            serde_json::json!({"diff": diff(&[("x", "y")]), "replace_all": true}),
        ),
        (
            "g.txt",
            &far,
            serde_json::json!({"diff": diff(&[("3", "three"), ("12", "twelve")])}),
        ),
    ];
    let expected = [
        Some(" 1\tone\n-2\ttwo\n+2\t2\n+3\t2.5\n 4\tthree"),
        Some(" 1\ta\n-2\tb\n 2\tc"),
        Some(" 1\ta\n-2\tb\n+2\tB\n 3\tc"),
        Some("-1\t\tlet 名前 = 1;\n+1\t\tlet 名前 = 2;"),
        None,
        Some(
            " 1\t1\n-2\tx\n+2\ty\n 3\t3\n 4\t4\n 5\t5\n…\n 8\t8\n 9\t9\n 10\t10\n-11\tx\n+11\ty\n 12\t12",
        ),
        Some(
            " 1\t1\n 2\tx\n-3\t3\n+3\tthree\n 4\t4\n 5\t5\n 6\t6\n…\n 9\t9\n 10\t10\n 11\tx\n-12\t12\n+12\ttwelve",
        ),
    ];
    let shown = edits_shown(APPLY_DIFF_TOOL_NAME, &edits, false).await;
    check_diffs(&shown, &expected);
}

/// An edit_file call's diff in a session saved with it is the edit that
/// ran, as it was: the whole lines it changed as the file had them before
/// and has them after, numbered, with the lines around them. That holds
/// for a deletion inside a line, a line break the edit takes away, a file
/// with CRLF line breaks, every match of an edit of all of them, and a
/// file with both CRLF and LF line breaks, whose LF line `edit_file` leaves
/// alone. A failed edit shows why it failed and no diff.
#[tokio::test]
async fn an_edit_file_calls_diff_is_the_edit_that_ran() {
    let edits: Vec<(&str, &str, serde_json::Value)> = vec![
        ("a.txt", "abcde\n", serde_json::json!({"old_string": "bc", "new_string": ""})),
        (
            "c.txt",
            "prefix foo\nsuffix\n",
            serde_json::json!({"old_string": "foo\n", "new_string": "bar"}),
        ),
        ("e.txt", "a\r\nb\r\nc\r\n", serde_json::json!({"old_string": "b", "new_string": "B"})),
        ("h.txt", "a\n", serde_json::json!({"old_string": "zzz", "new_string": "y"})),
        (
            "i.txt",
            "x\n1\n2\n3\n4\n5\n6\n7\nx\n",
            serde_json::json!({"old_string": "x", "new_string": "y", "replace_all": true}),
        ),
        (
            "j.txt",
            "old\r\nold\nend\n",
            serde_json::json!({"old_string": "old\n", "new_string": "new\n", "replace_all": true}),
        ),
    ];
    let expected = [
        Some("-1\tabcde\n+1\tade"),
        Some("-1\tprefix foo\n-2\tsuffix\n+1\tprefix barsuffix"),
        Some(" 1\ta\n-2\tb\n+2\tB\n 3\tc"),
        None,
        Some("-1\tx\n+1\ty\n 2\t1\n 3\t2\n 4\t3\n…\n 6\t5\n 7\t6\n 8\t7\n-9\tx\n+9\ty"),
        Some("-1\told\n+1\tnew\n 2\told\n 3\tend"),
    ];
    let shown = edits_shown(EDIT_FILE_TOOL_NAME, &edits, true).await;
    check_diffs(&shown, &expected);
}

/// An edit the tool refuses before it opens the file fails at once, here of
/// a FIFO nothing writes to, which nothing else reads first.
#[cfg(unix)]
#[tokio::test]
async fn an_edit_the_tool_refuses_fails_without_reading_the_file() {
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        Box::pin(async move {
            if body.contains("call_e1\"") {
                complete("done", "It failed.")
            } else {
                tool_call(
                    "e1",
                    APPLY_DIFF_TOOL_NAME,
                    serde_json::json!({"path": "pipe", "diff": diff(&[("a", "x")])}),
                )
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.open(false).unwrap();
    let pipe = harness.settings.workspace.join("pipe");
    let made = std::process::Command::new("mkfifo").arg(&pipe).status().unwrap();
    assert!(made.success());
    let turn = harness.turn(Prompt::User("Edit the pipe.".into()), &mut inputs);
    let end = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the turn ends");
    assert!(matches!(end, TurnEnd::Answered));
    let items = harness.feed.subscribe().0.items;
    let call = items
        .iter()
        .find(|item| item.tool.as_deref() == Some(APPLY_DIFF_TOOL_NAME))
        .expect("the edit's call");
    assert_eq!(call.status, Some(remote::CallStatus::Failed));
    harness.crew.as_ref().unwrap().shutdown().await;
}
