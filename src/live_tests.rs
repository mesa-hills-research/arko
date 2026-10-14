// End-to-end tests of the settings that change while a session runs:
// outside access (`/outside`). Each follows a path a user takes, with the
// mock model, and checks what the user and the agents see. (Included in
// `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};
use tokio::sync::Notify;

/// Gives `harness` the session's shell and terminal tools, with outside
/// access `on` as `run` sets it up, and opens its workflow again.
fn equip(harness: &mut Harness, on: bool) -> Managers {
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell);
    harness.outside = Arc::new(outside::Outside::new(
        on,
        managers.list(),
        harness.settings.workspace.clone(),
    ));
    harness.settings.allow_outside_workspace = on;
    harness.session.save_settings(&harness.settings).unwrap();
    harness.open(false).unwrap();
    managers
}

/// A command the guard judges as deleting a path outside the working
/// directory (in the home directory, not a temporary one), which never
/// runs: `true ||` ends the list first.
fn outside_command() -> String {
    format!(
        "true || rm -rf ~/fin-outside-test-{}",
        uuid::Uuid::new_v4().simple()
    )
}

/// The texts of the feed's items of `kind`.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

fn says(feed: &remote::Feed, kind: &str, start: &str) -> bool {
    texts(feed, kind).iter().any(|text| text.starts_with(start))
}

/// A model that records the requests' bodies.
type Bodies = Arc<Mutex<Vec<String>>>;

/// The first body with `with` and without `without`.
fn body(bodies: &Bodies, with: &str, without: &str) -> String {
    let bodies = bodies.lock().unwrap();
    bodies
        .iter()
        .find(|body| body.contains(with) && (without.is_empty() || !body.contains(without)))
        .unwrap_or_else(|| panic!("no request with {with:?} without {without:?}"))
        .clone()
}

const ON_NOTE: &str = "The user turned on outside access";
const OFF_NOTE: &str = "The user turned off outside access";
/// The start of the command guard's refusals.
const GUARD_REFUSAL: &str = "Refused: ";
/// What the `outside` hook's refusal of a file tool's write says.
const HOOK_REFUSAL: &str = "and the user hasn't turned on outside access";

/// `/outside on` typed while the agent works: its next command and its
/// next write outside the working directory run, and it reads a note
/// saying so. `/outside off` then refuses both again, with a note. The
/// session keeps the last setting.
#[cfg(unix)]
#[tokio::test]
async fn outside_access_turned_on_and_off_mid_turn_reaches_the_shell_the_file_tools_and_the_agent()
{
    let temp = tempfile::tempdir().unwrap();
    let allowed = temp.path().join("outside.txt");
    let refused = temp.path().join("outside-2.txt");
    let command = outside_command();
    let bodies: Bodies = Arc::default();
    let (hold_on, hold_off) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let server = ModelServer::start(Arc::new({
        let (bodies, hold_on, hold_off) = (bodies.clone(), hold_on.clone(), hold_off.clone());
        let (allowed, refused, command) = (
            allowed.display().to_string(),
            refused.display().to_string(),
            command.clone(),
        );
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let (hold_on, hold_off) = (hold_on.clone(), hold_off.clone());
            let (allowed, refused, command) = (allowed.clone(), refused.clone(), command.clone());
            Box::pin(async move {
                if body.contains("call_m4") {
                    complete("m5", "Done.")
                } else if body.contains("call_m3") {
                    tool_call(
                        "m4",
                        "write_file",
                        serde_json::json!({"path": refused, "content": "two"}),
                    )
                } else if body.contains("call_m2") {
                    hold_off.notified().await;
                    tool_call("m3", SHELL_TOOL, serde_json::json!({"command": command}))
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        "write_file",
                        serde_json::json!({"path": allowed, "content": "one"}),
                    )
                } else {
                    hold_on.notified().await;
                    tool_call("m1", SHELL_TOOL, serde_json::json!({"command": command}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let feed = harness.feed.clone();
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            until(|| feed.activity().working).await;
            sender.send(Input::Line("/outside on".into())).unwrap();
            until(|| says(&feed, "output", "Outside access is on:")).await;
            hold_on.notify_one();
            until(|| {
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains("call_m2"))
            })
            .await;
            sender.send(Input::Line("/outside off".into())).unwrap();
            until(|| says(&feed, "output", "Outside access is off:")).await;
            hold_off.notify_one();
            sender
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Do the outside work.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done.");

    // On: the command ran, the file was written, and the agent was told.
    let after_command = body(&bodies, "call_m1", "call_m2");
    assert!(!after_command.contains(GUARD_REFUSAL), "{after_command}");
    assert!(after_command.contains(ON_NOTE), "{after_command}");
    assert_eq!(std::fs::read_to_string(&allowed).unwrap(), "one");
    // Off again: both refused, and the agent was told.
    let after_command = body(&bodies, "call_m3", "call_m4");
    assert!(after_command.contains(GUARD_REFUSAL), "{after_command}");
    assert!(after_command.contains(OFF_NOTE), "{after_command}");
    assert!(body(&bodies, "call_m4", "").contains(HOOK_REFUSAL));
    assert!(!refused.exists());

    // The user saw the commands and their effect; the session keeps the
    // last setting.
    assert!(has(&harness.feed, "user", "/outside on", "command"));
    assert!(has(&harness.feed, "user", "/outside off", "command"));
    assert!(!harness.outside.is_on());
    assert!(
        !harness
            .session
            .load_settings()
            .unwrap()
            .allow_outside_workspace
    );
    assert_eq!(
        harness
            .workflow()
            .notes()
            .iter()
            .map(|posted| posted.note.text.split(':').next().unwrap().to_string())
            .collect::<Vec<_>>(),
        [ON_NOTE, OFF_NOTE]
    );
}

/// The web page turns outside access on and off with the same commands:
/// the terminal and the page get a notice, a repeated or wrong command
/// says so, and the page's status shows the setting.
#[tokio::test]
async fn outside_access_changed_from_the_web_page_is_announced() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), LLMClient::new(), true);
    let actions = harness.remote_actions.clone();
    let feed = harness.feed.clone();
    let gauges = harness.gauges.clone();
    let outside = harness.outside.clone();
    let typing = async move {
        actions.send(Action::Message("/outside on".into())).unwrap();
        until(|| {
            says(
                &feed,
                "notice",
                "Outside access turned on from the web page.",
            )
        })
        .await;
        assert!(remote::Meter::read(gauges.as_ref()).settings.outside);
        actions.send(Action::Message("/outside on".into())).unwrap();
        actions
            .send(Action::Message("/outside sideways".into()))
            .unwrap();
        actions.send(Action::Message("/outside".into())).unwrap();
        actions
            .send(Action::Message("/outside off".into()))
            .unwrap();
        until(|| {
            says(
                &feed,
                "notice",
                "Outside access turned off from the web page.",
            )
        })
        .await;
        assert!(!remote::Meter::read(gauges.as_ref()).settings.outside);
        sender.send(Input::Eof).unwrap();
    };
    let (_, ()) = tokio::time::timeout(
        Duration::from_secs(20),
        futures_util::future::join(harness.repl(&mut inputs), typing),
    )
    .await
    .unwrap();
    assert!(!outside.is_on());
    let outputs = texts(&harness.feed, "output");
    for expected in [
        "Outside access is on: the agents may delete",
        "Outside access is already on.",
        "`sideways`: use /outside on or /outside off.",
        "Outside access is on: the agents may delete",
        "Outside access is off: the agents can't",
    ] {
        assert!(
            outputs.iter().any(|text| text.starts_with(expected)),
            "{expected}: {outputs:?}"
        );
    }
    assert_eq!(harness.workflow().notes().len(), 2);
}

/// A new session started with `--allow-outside-workspace`: the agent is
/// told in its first message, and it writes outside the working
/// directory.
#[cfg(unix)]
#[tokio::test]
async fn a_session_started_with_outside_access_says_so_and_allows_it() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("outside.txt");
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        let target = target.display().to_string();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let target = target.clone();
            Box::pin(async move {
                if body.contains("call_m1") {
                    complete("m2", "Written.")
                } else {
                    tool_call(
                        "m1",
                        "write_file",
                        serde_json::json!({"path": target, "content": "x"}),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, true);
    harness.outside_at_start(true, true);
    let end = harness
        .turn(Prompt::User("Write it.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert!(body(&bodies, "Write it.", "call_m1").contains(ON_NOTE));
    assert!(!body(&bodies, "call_m1", "").contains(HOOK_REFUSAL));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    assert!(
        harness
            .outside
            .describe()
            .starts_with("Outside access is on")
    );
}

/// A session saved before outside access could change (its agents'
/// configuration refuses the file tools' writes outside the working
/// directory), resumed: without the flag it stays off; with it, commands
/// may act outside, the user is told the file tools still can't, and so is
/// the agent.
#[cfg(unix)]
#[tokio::test]
async fn an_old_session_resumed_with_the_flag_gets_the_shell_part_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("outside.txt");
    let command = outside_command();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        let (target, command) = (target.display().to_string(), command.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let (target, command) = (target.clone(), command.clone());
            Box::pin(async move {
                if !body.contains("Now act outside.") {
                    complete("m0", "Saved.")
                } else if body.contains("call_m2") {
                    complete("m3", "Done.")
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        "write_file",
                        serde_json::json!({"path": target, "content": "x"}),
                    )
                } else {
                    tool_call("m1", SHELL_TOOL, serde_json::json!({"command": command}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    // The configuration fin saved sessions with before.
    for config in [&mut harness.main_config, &mut harness.worker_config] {
        for tool in [
            arko_agents::builtins::WRITE_FILE_TOOL_NAME,
            arko_agents::builtins::APPLY_DIFF_TOOL_NAME,
        ] {
            config
                .tool_config
                .tool_specific_configs
                .get_mut(tool)
                .unwrap()
                .out_of_workspace_path_policy =
                Some(arko_agents::agent_config::PathSafetyPolicy::AutoReject);
        }
    }
    harness.open(false).unwrap();
    let end = harness
        .turn(Prompt::User("Save.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    // fin builds the configurations of today, and resumes with the saved
    // ones.
    harness.main_config = agents::main_config(&agent_settings_of(&harness)).unwrap();
    harness.worker_config = agents::worker_config(&agent_settings_of(&harness)).unwrap();
    harness.open(true).unwrap();
    harness.outside_at_start(false, false);
    assert!(!harness.outside.is_on());
    assert!(
        harness
            .outside
            .describe()
            .starts_with("Outside access is off")
    );

    harness.outside_at_start(false, true);
    assert!(harness.outside.is_on());
    assert!(!harness.outside.files_follow());
    assert!(harness.outside.describe().ends_with(outside::FILES_FIXED));
    assert!(
        harness
            .session
            .load_settings()
            .unwrap()
            .allow_outside_workspace
    );
    let end = harness
        .turn(Prompt::User("Now act outside.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let first = body(&bodies, "Now act outside.", "call_m1");
    assert!(first.contains("In this session the file tools still can't write outside"));
    assert!(!body(&bodies, "call_m1", "call_m2").contains(GUARD_REFUSAL));
    let after_write = body(&bodies, "call_m2", "");
    assert!(
        after_write.contains("outside the workspace"),
        "{after_write}"
    );
    assert!(!target.exists());
}

/// A session made by this fin, resumed with the flag: commands and the
/// file tools both act outside.
#[cfg(unix)]
#[tokio::test]
async fn a_new_session_resumed_with_the_flag_gets_both_parts() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("outside.txt");
    let command = outside_command();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        let (target, command) = (target.display().to_string(), command.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let (target, command) = (target.clone(), command.clone());
            Box::pin(async move {
                if !body.contains("Now act outside.") {
                    complete("m0", "Saved.")
                } else if body.contains("call_m2") {
                    complete("m3", "Done.")
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        "write_file",
                        serde_json::json!({"path": target, "content": "x"}),
                    )
                } else {
                    tool_call("m1", SHELL_TOOL, serde_json::json!({"command": command}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let end = harness
        .turn(Prompt::User("Save.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.open(true).unwrap();
    harness.outside_at_start(false, true);
    assert!(harness.outside.files_follow());
    let end = harness
        .turn(Prompt::User("Now act outside.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    // The resumed conversation reads the note at its next step.
    assert!(body(&bodies, "call_m1", "call_m2").contains(ON_NOTE));
    assert!(!body(&bodies, "call_m1", "call_m2").contains(GUARD_REFUSAL));
    assert!(!body(&bodies, "call_m2", "").contains(HOOK_REFUSAL));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
}

/// Workers follow the switch: one started before it is turned on writes
/// outside afterwards and reads the note; a workspace outside the
/// working directory is refused while it is off and given once it is on,
/// and that worker starts knowing.
#[tokio::test]
async fn workers_started_before_and_after_a_change_follow_it() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("outside.txt");
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        let target = target.display().to_string();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let (hold, target) = (hold.clone(), target.clone());
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    if body.contains("call_w1") {
                        complete("w2", "Wrote it.")
                    } else {
                        hold.notified().await;
                        tool_call("w1", "write_file", serde_json::json!({"path": target, "content": "x"}))
                    }
                } else if body.contains("You are agent run r2") {
                    complete("v1", "Second done.")
                } else if body.contains("Second done.") {
                    complete("m6", "All done.")
                } else if body.contains("call_m4") {
                    let n = body.matches("call_m5").count();
                    tool_call(&format!("m5_{n}"), crew::WAIT_FOR_AGENTS, serde_json::json!({"timeout_seconds": 20}))
                } else if body.contains("Wrote it.") {
                    tool_call("m4", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Second.", "workspace": "../elsewhere"}))
                } else if body.contains("call_m2") {
                    let n = body.matches("call_m3").count();
                    tool_call(&format!("m3_{n}"), crew::WAIT_FOR_AGENTS, serde_json::json!({"timeout_seconds": 20}))
                } else if body.contains("call_m1") {
                    tool_call("m2", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Write outside."}))
                } else {
                    tool_call("m1", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Write outside.", "workspace": "../elsewhere"}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let feed = harness.feed.clone();
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            until(|| {
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains("You are agent run r1"))
            })
            .await;
            sender.send(Input::Line("/outside on".into())).unwrap();
            until(|| says(&feed, "output", "Outside access is on:")).await;
            hold.notify_one();
            sender
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Start the workers.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(
        matches!(end, TurnEnd::Answered),
        "{:?}",
        bodies.lock().unwrap().last()
    );
    assert_eq!(summary(&harness), "All done.");
    // Off: the workspace outside was refused.
    let refusal = body(&bodies, "call_m1", "call_m2");
    assert!(refusal.contains("Refused: the workspace"), "{refusal}");
    // r1 started before the change, wrote after it, and was told.
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    let r1 = body(&bodies, "call_w1", "");
    assert!(r1.contains(ON_NOTE) && !r1.contains(HOOK_REFUSAL), "{r1}");
    // r2 works in the directory outside, and knew from the start.
    let r2 = body(&bodies, "You are agent run r2", "");
    assert!(r2.contains(ON_NOTE), "{r2}");
    let workers = harness.workflow().subagents();
    assert_eq!(
        arko_agents::utils::normalize_path(&workers[1].workspace),
        arko_agents::utils::normalize_path(&elsewhere)
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
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
    }
}

/// The shell tool's name.
const SHELL_TOOL: &str = crate::shell::SHELL_TOOL_NAME;
