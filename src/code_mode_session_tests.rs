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

/// What the request after the run read of `run_code`'s result.
fn run_result(bodies: &Bodies) -> String {
    let bodies = bodies.lock().unwrap();
    let body = bodies
        .iter()
        .find(|body| body.contains("call_c1"))
        .expect("a request after the run");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_c1")
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
    let diff = "<<<<<<< SEARCH\nkeep\n=======\nchanged\n>>>>>>> REPLACE\n";
    let code = format!(
        "try {{ await tools.apply_diff({{ path: {outside:?}, diff: {diff:?} }}); return 'edited'; }} catch (e) {{ return `${{e.refused}} ${{e.message}}`; }}",
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
/// while the agents edited with edit_file has edit_file in code and no
/// apply_diff, and a new one has apply_diff and no edit_file.
#[tokio::test]
async fn code_edits_with_the_editing_tool_its_agent_has() {
    use arko_agents::builtins::{APPLY_DIFF_TOOL_NAME, EDIT_FILE_TOOL_NAME};
    let code = "const has = [typeof tools.apply_diff, typeof tools.edit_file].join();\n\
                if (typeof tools.edit_file === 'function') {\n\
                  await tools.edit_file({ path: 'notes.txt', old_string: 'apple', new_string: 'fig' });\n\
                } else {\n\
                  await tools.apply_diff({ path: 'notes.txt', diff: '<<<<<<< SEARCH\\napple\\n=======\\nfig\\n>>>>>>> REPLACE\\n' });\n\
                }\n\
                return has;";
    for (saved_with_edit_file, has) in [(true, "undefined,function"), (false, "function,undefined")]
    {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Bodies = Arc::default();
        let server = model_running(code, &bodies).await;
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let _managers = equip(&mut harness, Tools::Both, |config| {
            if !saved_with_edit_file {
                return;
            }
            // The main agent as Arko built it while it edited with edit_file.
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
        });
        let notes = harness.settings.workspace.join("notes.txt");
        std::fs::write(&notes, "apple\npear\n").unwrap();
        run_turn(&mut harness, &mut inputs).await;
        let result = run_result(&bodies);
        assert!(result.contains(has), "{saved_with_edit_file}: {result}");
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            "fig\npear\n",
            "{saved_with_edit_file}: {result}"
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
