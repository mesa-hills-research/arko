// End-to-end tests of the agents' file editing. New sessions edit files
// with `edit_file`, and sessions saved before it resume with `apply_diff`.
// Each follows a path a user takes, with the mock model, and checks what
// the model is offered and what the user sees. (Included in `main`'s tests,
// whose helpers they use.)

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

/// A new session offers `edit_file`, not `apply_diff`. The agent's edits
/// change the file, and each shows as "edit PATH" with what it did, such as "1
/// edit", or with `replace_all` "(all)" and the replacements.
#[tokio::test]
async fn a_new_session_edits_files_with_edit_file() {
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
                        EDIT_FILE_TOOL_NAME,
                        serde_json::json!({"path": "notes.txt", "old_string": "pear", "new_string": "plum", "replace_all": true}),
                    )
                } else {
                    tool_call(
                        "m1",
                        EDIT_FILE_TOOL_NAME,
                        serde_json::json!({"path": "notes.txt", "old_string": "apple", "new_string": "fig"}),
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
    std::fs::write(&notes, "apple\npear\npear tart\n").unwrap();
    let end = harness
        .turn(Prompt::User("Rename the fruit.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "fig\nplum\nplum tart\n"
    );
    let tools = tools_of(&bodies.lock().unwrap()[0]);
    assert!(tools.contains(&EDIT_FILE_TOOL_NAME.to_string()), "{tools:?}");
    assert!(!tools.contains(&APPLY_DIFF_TOOL_NAME.to_string()), "{tools:?}");
    let calls = call_outcomes(&harness.feed);
    assert_eq!(
        calls,
        [
            ("edit notes.txt".to_string(), "1 edit".to_string()),
            ("edit notes.txt (all)".to_string(), "2 replacements".to_string()),
        ]
    );
    let printed = lines.0.lock().unwrap().clone();
    for line in ["● edit notes.txt", "● edit notes.txt (all)"] {
        assert!(printed.iter().any(|shown| shown == line), "{line}: {printed:?}");
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A session saved before `edit_file` (its agents have `apply_diff`)
/// resumes with its saved tool list. It isn't told it lacks the file
/// tools, `apply_diff` is still offered and works, and its edits show as
/// before.
#[tokio::test]
async fn a_session_saved_with_apply_diff_resumes_with_it() {
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
                        APPLY_DIFF_TOOL_NAME,
                        serde_json::json!({
                            "path": "notes.txt",
                            "diff": "<<<<<<< SEARCH\n:start_line:1\n-------\napple\n=======\nfig\n>>>>>>> REPLACE"
                        }),
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
    // The agents as Arko built them before edit_file.
    for config in [&mut harness.main_config, &mut harness.worker_config] {
        let tools = &mut config.tool_config;
        for name in &mut tools.available_tool_names {
            if name == EDIT_FILE_TOOL_NAME {
                *name = APPLY_DIFF_TOOL_NAME.into();
            }
        }
        let policy = tools
            .tool_specific_configs
            .remove(EDIT_FILE_TOOL_NAME)
            .unwrap();
        tools
            .tool_specific_configs
            .insert(APPLY_DIFF_TOOL_NAME.into(), policy);
    }
    harness.open(false).unwrap();
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;

    // Arko now builds its agents with edit_file, and the session is resumed.
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
    assert!(tools.contains(&APPLY_DIFF_TOOL_NAME.to_string()), "{tools:?}");
    assert!(!tools.contains(&EDIT_FILE_TOOL_NAME.to_string()), "{tools:?}");
    assert!(
        call_outcomes(&harness.feed).contains(&("edit notes.txt".to_string(), "1 edit".to_string()))
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}


/// An edit's diff in the transcript is the edit that ran: the whole lines
/// it changed as the file had them before and has them after, numbered,
/// with the lines around them. That holds for a deletion inside a line, a
/// replacement whose text the line has twice, a line break the edit adds or
/// takes away, a file with CRLF line breaks, tabs and wide characters, a
/// whole line deleted, every match of an edit of all of them, and a file
/// with both CRLF and LF line breaks, whose LF line `edit_file` leaves alone.
/// A failed edit shows why it failed and no diff.
#[tokio::test]
async fn an_edits_diff_is_the_edit_that_ran() {
    // Each edit: its file, the file's text, and the call's parameters.
    let edits: Vec<(&str, &str, serde_json::Value)> = vec![
        ("a.txt", "abcde\n", serde_json::json!({"old_string": "bc", "new_string": ""})),
        ("b.txt", "x ax\n", serde_json::json!({"old_string": "ax", "new_string": "x"})),
        (
            "c.txt",
            "prefix foo\nsuffix\n",
            serde_json::json!({"old_string": "foo\n", "new_string": "bar"}),
        ),
        (
            "d.txt",
            "prefix foosuffix\n",
            serde_json::json!({"old_string": "foo", "new_string": "bar\n"}),
        ),
        ("e.txt", "a\r\nb\r\nc\r\n", serde_json::json!({"old_string": "b", "new_string": "B"})),
        (
            "f.txt",
            "\tlet 名前 = 1;\n",
            serde_json::json!({"old_string": "1", "new_string": "2"}),
        ),
        ("g.txt", "a\nb\nc\n", serde_json::json!({"old_string": "b\n", "new_string": ""})),
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
    let expected: [Option<&str>; 10] = [
        Some("-1\tabcde\n+1\tade"),
        Some("-1\tx ax\n+1\tx x"),
        Some("-1\tprefix foo\n-2\tsuffix\n+1\tprefix barsuffix"),
        Some("-1\tprefix foosuffix\n+1\tprefix bar\n+2\tsuffix"),
        Some(" 1\ta\n-2\tb\n+2\tB\n 3\tc"),
        Some("-1\t\tlet 名前 = 1;\n+1\t\tlet 名前 = 2;"),
        Some(" 1\ta\n-2\tb\n 2\tc"),
        None,
        Some("-1\tx\n+1\ty\n 2\t1\n 3\t2\n 4\t3\n…\n 6\t5\n 7\t6\n 8\t7\n-9\tx\n+9\ty"),
        Some("-1\told\n+1\tnew\n 2\told\n 3\tend"),
    ];
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
                Some((_, parameters)) => tool_call(
                    &format!("e{}", done + 1),
                    EDIT_FILE_TOOL_NAME,
                    parameters.clone(),
                ),
                None => complete("done", "Edited."),
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.open(false).unwrap();
    for (file, text, _) in &edits {
        std::fs::write(harness.settings.workspace.join(file), text).unwrap();
    }
    let end = harness
        .turn(Prompt::User("Edit the files.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let items = harness.feed.subscribe().0.items;
    let edit_calls: Vec<&remote::Item> = items
        .iter()
        .filter(|item| item.tool.as_deref() == Some(EDIT_FILE_TOOL_NAME))
        .collect();
    assert_eq!(edit_calls.len(), edits.len());
    for ((call, expected), (file, ..)) in edit_calls.iter().zip(expected).zip(&edits) {
        let block = items.iter().find(|item| item.call == Some(call.id));
        match expected {
            Some(diff) => {
                let block = block.unwrap_or_else(|| panic!("{file}: no diff"));
                assert_eq!(block.content, Some(remote::ContentKind::Diff), "{file}");
                assert_eq!(block.text, diff, "{file}");
            }
            None => {
                assert_eq!(call.status, Some(remote::CallStatus::Failed), "{file}");
                let block = block.unwrap_or_else(|| panic!("{file}: no reason"));
                assert_eq!(block.content, Some(remote::ContentKind::Muted), "{file}");
            }
        }
    }
    harness.crew.as_ref().unwrap().shutdown().await;
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
                    EDIT_FILE_TOOL_NAME,
                    serde_json::json!({"path": "pipe", "old_string": "", "new_string": "x"}),
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
        .find(|item| item.tool.as_deref() == Some(EDIT_FILE_TOOL_NAME))
        .expect("the edit's call");
    assert_eq!(call.status, Some(remote::CallStatus::Failed));
    harness.crew.as_ref().unwrap().shutdown().await;
}
