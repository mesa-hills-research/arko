// End-to-end tests of the agents' file editing: new sessions edit files
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

impl rustyline::ExternalPrinter for Lines {
    fn print(&mut self, text: String) -> rustyline::Result<()> {
        let mut lines = self.0.lock().unwrap();
        lines.extend(text.lines().map(str::to_string));
        Ok(())
    }
}

/// A new session offers `edit_file`, not `apply_diff`. The agent's edits
/// change the file, and each shows as "edit PATH" with what it did: "1
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
/// resumes with its saved tool list: it isn't told it lacks the file
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
    // The agents as fin built them before edit_file.
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

    // fin now builds its agents with edit_file, and the session is resumed.
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

/// The agent settings `harness`'s configurations are built from.
fn agent_settings_of(harness: &Harness) -> AgentSettings<'_> {
    AgentSettings {
        main: &harness.settings.main,
        worker: &harness.settings.worker,
        max_workers: harness.settings.max_workers,
        handover_at: harness.settings.handover_at,
        show_thinking: false,
        shell_name: "bash",
        providers: Box::leak(Box::new(providers::Setup::new(
            harness.settings.openai_auth,
        ))),
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
        text_replies: harness.settings.text_replies,
    }
}
