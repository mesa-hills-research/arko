// Sessions whose agents run code (`--tools both`), with the mock model, and
// the gate as a session has it: the outside-access hook, the command guard
// and the approval policy. Each gate test fails if a call from code skips
// the gate. (Included in `harness`'s tests, whose helpers they use.)

use crate::code_mode::{RUN_CODE, Tools};
use crate::mock_model::{ModelServer, complete, tool_call};

/// A model that records the requests' bodies.
type Bodies = Arc<Mutex<Vec<String>>>;

/// Gives `harness` the session's shell and terminal tools and the
/// outside-access hook (off), its agents offered `tools`, and opens its
/// workflow. `change` changes the main agent's configuration first.
fn equip(
    harness: &mut Harness,
    tools: Tools,
    change: impl FnOnce(&mut arko_agents::AgentConfig),
) -> Managers {
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.run_code = Arc::new(crate::code_mode::RunCodeTool::new(
        crate::code_mode::file_tool_definitions()
            .into_iter()
            .chain(harness.tools.iter().map(|tool| tool.definition())),
    ));
    harness.outside = Arc::new(outside::Outside::new(
        false,
        managers.list(),
        harness.settings.workspace.clone(),
    ));
    harness.settings.tools = tools;
    let (mut main, worker) = {
        let settings = agent_settings_of(harness);
        (
            agents::main_config(&settings).unwrap(),
            agents::worker_config(&settings).unwrap(),
        )
    };
    change(&mut main);
    harness.main_config = main;
    harness.worker_config = worker;
    harness.session.save_settings(&harness.settings).unwrap();
    harness.open(false).unwrap();
    managers
}

/// A model that calls `run_code` with `code`, then completes, recording
/// the bodies of its requests.
async fn model_running(code: &str, bodies: &Bodies) -> ModelServer {
    let (code, bodies) = (code.to_string(), bodies.clone());
    ModelServer::start(Arc::new(move |_, body: String| {
        bodies.lock().unwrap().push(body.clone());
        let code = code.clone();
        Box::pin(async move {
            if body.contains("call_c1") {
                complete("c2", "Done.")
            } else {
                tool_call(
                    "c1",
                    RUN_CODE,
                    serde_json::json!({"code": code, "purpose": "test the gate"}),
                )
            }
        })
    }))
    .await
}

/// The tools a request offered, by name.
fn offered(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The description a request offered `tool` with.
fn offered_description(body: &str, tool: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|offered| offered["name"] == tool))
        .and_then(|offered| offered["description"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// What the request after the run read of `run_code`'s result.
fn run_result(bodies: &Bodies) -> String {
    call_result(bodies, "call_c1")
}

/// What the request after call `call_id` read of its result.
fn call_result(bodies: &Bodies, call_id: &str) -> String {
    let bodies = bodies.lock().unwrap();
    let body = bodies
        .iter()
        .find(|body| body.contains(call_id))
        .expect("a request after the call");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| item["output"].as_str())
        .unwrap_or_default()
        .to_string()
}

async fn run_turn(harness: &mut Harness, inputs: &mut Inputs) {
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Run the code.".into()), inputs),
    )
    .await
    .expect("the turn ended");
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(harness), "Done.");
}

/// Under `both` the model is offered `run_code`, and code fans reads out
/// over the working directory, filters them, and catches a missing file.
#[tokio::test]
async fn a_session_with_both_runs_code_that_reads_and_filters() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = model_running(
        "const hits = [];\n\
         for (const path of ['a.txt', 'b.txt', 'c.txt']) {\n\
           const text = await tools.read_file({ path });\n\
           const count = (text.match(/TODO/g) ?? []).length;\n\
           if (count) hits.push(`${count} ${path}`);\n\
         }\n\
         try { await tools.read_file({ path: 'missing.txt' }); } catch (e) { hits.push('missing rejects'); }\n\
         return hits.join(' | ');",
        &bodies,
    )
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::Both, |_| {});
    let workspace = harness.settings.workspace.clone();
    std::fs::write(workspace.join("a.txt"), "TODO one\nTODO two\n").unwrap();
    std::fs::write(workspace.join("b.txt"), "nothing\n").unwrap();
    std::fs::write(workspace.join("c.txt"), "TODO three\n").unwrap();
    run_turn(&mut harness, &mut inputs).await;
    let first = bodies.lock().unwrap()[0].clone();
    assert!(offered(&first).contains(&RUN_CODE.to_string()));
    assert_eq!(run_result(&bodies), "2 a.txt | 1 c.txt | missing rejects");
}

/// Under `classic`, the default, the model isn't offered `run_code`.
#[tokio::test]
async fn a_classic_session_offers_no_run_code() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async move { complete("c1", "Done.") })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::NEW_SESSION, |_| {});
    run_turn(&mut harness, &mut inputs).await;
    let first = bodies.lock().unwrap()[0].clone();
    let tools = offered(&first);
    assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
    assert!(!tools.contains(&RUN_CODE.to_string()), "{tools:?}");
}

/// Under `code` the model is offered `run_code`, whose description declares
/// the tools code calls, and none of those tools. A direct call of one is
/// answered as unavailable, and the same call from code runs.
#[tokio::test]
async fn a_session_with_code_reaches_the_tools_code_calls_only_from_code() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("call_c1") {
                    complete("c2", "Done.")
                } else if body.contains("call_d1") {
                    tool_call(
                        "c1",
                        RUN_CODE,
                        serde_json::json!({
                            "code": "return await tools.read_file({ path: 'a.txt' });",
                            "purpose": "read a file"
                        }),
                    )
                } else {
                    tool_call("d1", "read_file", serde_json::json!({"path": "a.txt"}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::Code, |_| {});
    std::fs::write(harness.settings.workspace.join("a.txt"), "hello from a\n").unwrap();
    run_turn(&mut harness, &mut inputs).await;
    let first = bodies.lock().unwrap()[0].clone();
    let tools = offered(&first);
    assert!(tools.contains(&RUN_CODE.to_string()), "{tools:?}");
    for name in crate::code_mode::CALLABLE {
        assert!(!tools.contains(&name.to_string()), "{name}: {tools:?}");
    }
    let description = offered_description(&first, RUN_CODE);
    assert!(
        description.contains("The tools declared below are reachable only from here")
            && description.contains("\nread_file(args: {")
            && description.contains("\nshell(args: {"),
        "{description}"
    );
    let direct = call_result(&bodies, "call_d1");
    assert!(
        direct.contains("Tool 'read_file' is not available."),
        "{direct}"
    );
    let read = run_result(&bodies);
    assert!(read.contains("1 | hello from a"), "{read}");
}

/// A conversation saved under one `--tools` keeps its tools and run_code's
/// description when the session resumes with another, while a worker it
/// starts then and a new conversation take the new setting.
#[tokio::test]
async fn a_saved_conversation_keeps_its_tools_when_the_setting_changes() {
    let code_only = |body: &str| {
        let tools = offered(body);
        let description = offered_description(body, RUN_CODE);
        assert!(tools.contains(&RUN_CODE.to_string()), "{tools:?}");
        (
            !tools.contains(&"read_file".to_string()),
            description.contains("reachable only from here"),
        )
    };
    for (was, now) in [(Tools::Code, Tools::Both), (Tools::Both, Tools::Code)] {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Bodies = Arc::default();
        // After the resume the main agent starts a worker.
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("You are agent run r") {
                        complete("w1", "Looked.")
                    } else if body.contains("call_m2") {
                        complete("m3", "Started r1.")
                    } else if body.contains("Again.") {
                        tool_call(
                            "m2",
                            crew::START_AGENT,
                            serde_json::json!({"agent": WORKER, "task": "Look around."}),
                        )
                    } else {
                        complete("m1", "Noted.")
                    }
                })
            }
        }))
        .await;
        // The main agent's latest request that holds `text`, or the worker's.
        let request = |text: &str| {
            bodies
                .lock()
                .unwrap()
                .iter()
                .rfind(|body| {
                    body.contains(text)
                        && (text.starts_with("You are agent run")
                            || !body.contains("You are agent run r"))
                })
                .unwrap_or_else(|| panic!("no request with {text}"))
                .clone()
        };
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let _managers = equip(&mut harness, was, |_| {});
        harness
            .turn(Prompt::User("First.".into()), &mut inputs)
            .await;
        let started = was == Tools::Code;
        assert_eq!(code_only(&request("First.")), (started, started), "{was:?}");
        // The session is resumed with the other setting.
        harness.settings.tools = now;
        harness.main_config = agents::main_config(&agent_settings_of(&harness)).unwrap();
        harness.worker_config = agents::worker_config(&agent_settings_of(&harness)).unwrap();
        harness.open(true).unwrap();
        harness
            .turn(Prompt::User("Again.".into()), &mut inputs)
            .await;
        assert_eq!(
            code_only(&request("Again.")),
            (started, started),
            "{was:?} resumed with {now:?}"
        );
        until(|| {
            bodies
                .lock()
                .unwrap()
                .iter()
                .any(|body| body.contains("You are agent run r1,"))
        })
        .await;
        assert_eq!(
            code_only(&request("You are agent run r1,")),
            (!started, !started),
            "a worker started under {now:?}"
        );
        harness.new_conversation();
        harness
            .turn(Prompt::User("Fresh.".into()), &mut inputs)
            .await;
        assert_eq!(
            code_only(&request("Fresh.")),
            (!started, !started),
            "a new conversation under {now:?}"
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }
}

/// From code, a write outside the working directory is refused while
/// outside access is off, and so is a command the guard refuses. The third
/// refusal ends the run. Were either call to skip the gate, the file would
/// be written or the run would return.
#[cfg(unix)]
#[tokio::test]
async fn from_code_the_outside_hook_and_the_guard_refuse_and_the_third_refusal_ends_the_run() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.txt");
    let guarded = format!(
        "true || rm -rf ~/arko-code-mode-test-{}",
        uuid::Uuid::new_v4().simple()
    );
    let code = format!(
        "const seen = [];\n\
         try {{ await tools.write_file({{ path: {outside:?}, content: 'x' }}); }} catch (e) {{ seen.push(e.refused); }}\n\
         try {{ await tools.shell({{ command: {guarded:?} }}); }} catch (e) {{ seen.push(e.refused); }}\n\
         if (seen.join() !== 'true,true') return 'not refused: ' + seen.join();\n\
         await tools.shell({{ command: {guarded:?} }});\n\
         return 'went on';",
        outside = outside.display().to_string(),
    );
    let bodies: Bodies = Arc::default();
    let server = model_running(&code, &bodies).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::Both, |_| {});
    run_turn(&mut harness, &mut inputs).await;
    let result = run_result(&bodies);
    assert!(
        result.contains("3 of the code's calls were refused, so the run was stopped.")
            && result.contains("Refused"),
        "{result}"
    );
    assert!(!outside.exists());
}

/// From code, an edit of a file outside the working directory is refused
/// while outside access is off, and the file stays byte for byte as it was.
/// Were the call to skip the gate, the file would change.
#[tokio::test]
async fn from_code_an_edit_outside_the_working_directory_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, b"keep\r\nme\n").unwrap();
    let code = format!(
        "try {{ await tools.edit_file({{ path: {outside:?}, old_string: 'keep', new_string: 'changed' }}); return 'edited'; }} catch (e) {{ return `${{e.refused}} ${{e.message}}`; }}",
        outside = outside.display().to_string(),
    );
    let bodies: Bodies = Arc::default();
    let server = model_running(&code, &bodies).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::Both, |_| {});
    run_turn(&mut harness, &mut inputs).await;
    let result = run_result(&bodies);
    assert!(result.starts_with("true "), "{result}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep\r\nme\n");
}

/// Code edits a file with the editing tool its agent has. An agent saved
/// while the agents edited with apply_diff has apply_diff in code and no
/// edit_file, and a new one has edit_file and no apply_diff.
#[tokio::test]
async fn code_edits_with_the_editing_tool_its_agent_has() {
    use arko_agents::builtins::{APPLY_DIFF_TOOL_NAME, EDIT_FILE_TOOL_NAME};
    let code = "const has = [typeof tools.edit_file, typeof tools.apply_diff].join();\n\
                if (typeof tools.edit_file === 'function') {\n\
                  await tools.edit_file({ path: 'notes.txt', old_string: 'apple', new_string: 'fig' });\n\
                } else {\n\
                  await tools.apply_diff({ path: 'notes.txt', diff: '<<<<<<< SEARCH\\napple\\n=======\\nfig\\n>>>>>>> REPLACE\\n' });\n\
                }\n\
                return has;";
    for (saved_with_apply_diff, has) in
        [(true, "undefined,function"), (false, "function,undefined")]
    {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Bodies = Arc::default();
        let server = model_running(code, &bodies).await;
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let _managers = equip(&mut harness, Tools::Both, |config| {
            if !saved_with_apply_diff {
                return;
            }
            // The main agent as Arko built it while it edited with apply_diff.
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
        });
        let notes = harness.settings.workspace.join("notes.txt");
        std::fs::write(&notes, "apple\npear\n").unwrap();
        run_turn(&mut harness, &mut inputs).await;
        let result = run_result(&bodies);
        assert!(result.contains(has), "{saved_with_apply_diff}: {result}");
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            "fig\npear\n",
            "{saved_with_apply_diff}: {result}"
        );
    }
}

/// From code, a call that would wait for the user's approval is refused at
/// once, and the turn goes on. Were it to skip the gate, the file would be
/// written, and were it to wait, the turn wouldn't end.
#[tokio::test]
async fn from_code_a_call_that_needs_approval_is_refused_instead_of_waiting() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = model_running(
        "try { await tools.write_file({ path: 'approved.txt', content: 'x' }); return 'written'; } catch (e) { return `${e.refused} ${e.message}`; }",
        &bodies,
    )
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, Tools::Both, |config| {
        config.tool_config.tool_specific_configs.insert(
            arko_agents::builtins::WRITE_FILE_TOOL_NAME.into(),
            arko_agents::PerToolConfigOverride {
                auto_approve_tool_use: Some(false),
                ..Default::default()
            },
        );
    });
    run_turn(&mut harness, &mut inputs).await;
    let result = run_result(&bodies);
    assert!(
        result.starts_with("true This call of write_file needs the user's approval"),
        "{result}"
    );
    assert!(!harness.settings.workspace.join("approved.txt").exists());
}
