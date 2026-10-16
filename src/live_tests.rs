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
    harness.tools = managers.tools(&shell, harness.handover.runs());
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
        "true || rm -rf ~/arko-outside-test-{}",
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
    // The file tools refuse writes outside the working directory, as in
    // the configuration Arko saved sessions with before.
    for config in [&mut harness.main_config, &mut harness.worker_config] {
        for tool in [
            arko_agents::builtins::WRITE_FILE_TOOL_NAME,
            arko_agents::builtins::EDIT_FILE_TOOL_NAME,
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
    // Arko builds the configurations of today, and resumes with the saved
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

/// A session made by this Arko, resumed with the flag: commands and the
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
        providers: Box::leak(Box::new(providers::Setup::new(
            harness.settings.openai_auth,
        ))),
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
        text_replies: harness.settings.text_replies,
        cc_compact: true,
        extra_rules: harness.settings.extra_rules,
    }
}

/// The shell tool's name.
const SHELL_TOOL: &str = crate::shell::SHELL_TOOL_NAME;

/// The main agent's run as the last turn left it.
fn handovers(harness: &Harness) -> usize {
    match &harness.conversation {
        Conversation::Answered(result) => result.task.handovers.len(),
        _ => panic!("the conversation wasn't answered"),
    }
}

/// What the mock model answers the requests of a conversation that hands
/// over: the report when it is asked for (with the user's message `Second.`
/// when the conversation has it unanswered, as the report request asks),
/// `after` once the fresh conversation has it, else `before`.
fn handing_over(body: &str, before: String, after: String) -> String {
    if body.contains("REPORT:") {
        after
    } else if body.contains("Your conversation is getting long") {
        let message = if body.contains("Second.") {
            " The user's latest message, unanswered: Second."
        } else {
            ""
        };
        tool_call(
            "report",
            arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
            serde_json::json!({"report": format!("REPORT: the work so far.{message}")}),
        )
    } else {
        before
    }
}

/// `/handover` while the agent works: it hands over at its next step, and
/// the fresh conversation starts from its report.
#[cfg(unix)]
#[tokio::test]
async fn a_handover_asked_for_while_the_agent_works_happens_at_its_next_step() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if !body.contains("call_m1") && !body.contains("REPORT:") {
                    hold.notified().await;
                }
                handing_over(
                    &body,
                    tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                    complete("m2", "Done after the handover."),
                )
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let feed = harness.feed.clone();
    let typing = tokio::spawn(async move {
        until(|| feed.activity().working).await;
        sender.send(Input::Line("/handover".into())).unwrap();
        until(|| {
            says(
                &feed,
                "output",
                "The main agent hands over at its next step",
            )
        })
        .await;
        hold.notify_one();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Work on it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done after the handover.");
    assert_eq!(handovers(&harness), 1);
    // The report was asked for after the step, and the fresh conversation
    // has the task and the report, not the step.
    assert!(body(&bodies, "Your conversation is getting long", "").contains("call_m1"));
    let fresh = body(&bodies, "REPORT:", "Your conversation is getting long");
    assert!(
        fresh.contains("Work on it.") && !fresh.contains("call_m1"),
        "{fresh}"
    );
    assert!(says(&harness.feed, "notice", "handed over"));
    assert!(says(
        &harness.feed,
        "notice",
        "the agent is writing a handover report (you asked, at 0k tokens)"
    ));
}

/// `/handover` while no turn runs: nothing to hand over before the first
/// message; after it, the agent hands over when the next message starts
/// its turn, and the report carries that message.
#[cfg(unix)]
#[tokio::test]
async fn a_handover_asked_for_while_idle_happens_when_the_next_turn_starts() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if !body.contains("Second.") {
                    complete("a1", "First done.")
                } else {
                    handing_over(
                        &body,
                        tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                        complete("a2", "Second done."),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    assert!(harness.live_command("/handover", Source::Terminal));
    assert!(says(
        &harness.feed,
        "output",
        "There is no conversation to hand over"
    ));
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.live_command("/handover now", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        "/handover takes nothing after it"
    ));
    harness.live_command("/handover", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        "The main agent hands over when your next message starts its turn"
    ));
    harness.live_command("/handover", Source::Terminal);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Second done.");
    assert_eq!(handovers(&harness), 1);
    assert!(says(
        &harness.feed,
        "notice",
        "the agent is writing a handover report (you asked, at 0k tokens)"
    ));
    // The report was asked for with the message, before the agent acted on
    // it; the fresh conversation starts from the conversation's first
    // message and the report, which has the new message.
    let first = body(&bodies, "Second.", "");
    assert!(
        first.contains("Your conversation is getting long"),
        "{first}"
    );
    assert!(!first.contains("call_m1"), "{first}");
    let fresh = body(&bodies, "REPORT:", "Your conversation is getting long");
    assert!(
        fresh.contains("First.") && fresh.contains("Second."),
        "{fresh}"
    );
}

/// `/handover-at`: a size it can't read, one too small, one over the
/// configured 200k (the conversation goes past 200k without a handover,
/// also after the session is resumed), one over what the model leaves
/// room for (it stops there), and one under the agent's context (it hands
/// over when the next turn starts). The session and the page's status keep
/// the size.
#[cfg(unix)]
#[tokio::test]
async fn handover_sizes_are_checked_kept_and_acted_on() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                let before = if body.contains("call_m1") {
                    crate::mock_model::with_input(complete("a2", "Second done."), 260_000)
                } else if body.contains("Second.") {
                    crate::mock_model::with_input(
                        tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                        250_000,
                    )
                } else {
                    crate::mock_model::with_input(complete("a1", "First done."), 150_000)
                };
                handing_over(&body, before, complete("a3", "Third done."))
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.flush_printer().await;
    until(|| harness.handover.input() == Some(150_000)).await;
    let output = |harness: &mut Harness, command: &str| {
        assert!(harness.live_command(command, Source::Terminal));
        texts(&harness.feed, "output").last().unwrap().clone()
    };
    assert!(output(&mut harness, "/handover-at lots").contains("isn't a number of tokens"));
    assert!(output(&mut harness, "/handover-at 5k").contains("too small"));
    assert_eq!(harness.handover.at(), 200_000);
    assert_eq!(
        output(&mut harness, "/handover-at 300k"),
        "The main agent now hands over at 300k tokens of input."
    );
    assert_eq!(harness.handover.threshold().get(), Some(300_000));
    // Arko closes, and the session is resumed: its runs go by the size.
    harness.open(true).unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Second done.");
    assert_eq!(handovers(&harness), 0);
    assert!(
        !bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.contains("Your conversation is getting long")),
        "no handover at 250k"
    );
    harness.flush_printer().await;
    until(|| harness.handover.input() == Some(260_000)).await;
    let most = agents::max_handover(&resolve_model("luna").unwrap(), None).unwrap();
    assert_eq!(
        output(&mut harness, "/handover-at 2000k"),
        format!(
            "The main agent now hands over at {}k tokens of input, the largest size Arko allows for its model.",
            most / 1000
        )
    );
    assert_eq!(harness.handover.at(), most);
    let lowered = output(&mut harness, "/handover-at 100,000");
    assert!(
        lowered.starts_with("The main agent now hands over at 100k tokens of input."),
        "{lowered}"
    );
    assert!(lowered.contains("Its context (260k) is over that already. The main agent hands over when your next message starts its turn"), "{lowered}");
    assert_eq!(harness.handover.at(), 100_000);
    assert_eq!(
        harness.session.load_settings().unwrap().main_handover_at,
        Some(100_000)
    );
    assert_eq!(
        remote::Meter::read(harness.gauges.as_ref())
            .settings
            .handover_at,
        100_000
    );
    assert!(output(&mut harness, "/handover-at").starts_with(
        "The main agent hands over at 100k tokens of input (its latest request: 260k)."
    ));
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Third.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Third done.");
    assert_eq!(handovers(&harness), 1);
}

/// A size from the web page is announced at the terminal.
#[tokio::test]
async fn a_handover_size_set_from_the_web_page_is_announced() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    harness.live_command("/handover-at 150k", Source::Web);
    assert!(says(
        &harness.feed,
        "notice",
        "The handover size was set to 150k from the web page."
    ));
    assert_eq!(harness.handover.at(), 150_000);
}

/// A session on the ChatGPT login hands over by itself at 250k: its
/// configuration says so, and Arko doesn't ask too.
#[cfg(unix)]
#[tokio::test]
async fn a_chatgpt_session_hands_over_at_250k() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                handing_over(
                    &body,
                    if body.contains("call_m1") {
                        crate::mock_model::with_input(
                            tool_call("m2", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                            251_000,
                        )
                    } else {
                        crate::mock_model::with_input(
                            tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                            240_000,
                        )
                    },
                    complete("m3", "Done."),
                )
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.chatgpt_client(), true);
    let model = resolve_model(&harness.settings.main.model).unwrap();
    harness.settings.main.handover_at = Some(agents::default_handover(
        &model,
        Some(providers::OpenAiAuth::ChatGpt),
    ));
    assert_eq!(harness.settings.main.handover_at, Some(250_000));
    harness.main_config = agents::main_config(&agent_settings_of(&harness)).unwrap();
    let _managers = equip(&mut harness, false);
    assert_eq!(harness.handover.at(), 250_000);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Work long.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(handovers(&harness), 1);
    // Not at 240k; at 251k.
    assert!(!body(&bodies, "call_m1", "call_m2").contains("Your conversation is getting long"));
    assert!(body(&bodies, "Your conversation is getting long", "").contains("call_m2"));
    assert!(says(
        &harness.feed,
        "notice",
        "the agent is writing a handover report (at 251k tokens)"
    ));
}

/// A tool that returns `0` bytes of text: a large result.
#[derive(Debug)]
struct Big(usize);

#[async_trait::async_trait]
impl Tool for Big {
    fn definition(&self) -> arko_agents::ToolDefinition {
        arko_agents::ToolDefinition {
            name: "big".into(),
            description: "Returns a large text.".into(),
            output_based_example: None,
            parameters: Vec::new(),
            parallel_safe: true,
            tool_type_details: arko_agents::ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    async fn execute(
        &self,
        _: &arko_agents::TaskContext<'_>,
        _: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<String, arko_agents::ToolError> {
        let line = "a line of the large result, as a log or a listing has them\n";
        Ok(line.repeat(self.0 / line.len()))
    }
}

/// How a turn on the ChatGPT login goes when its first call returns
/// `bytes` of text at 240k tokens of input, 10k under the handover size:
/// whether it ends answered, how many requests the backend refused for
/// their size, the page's notices, the handovers, the reply and the
/// requests' bodies. The mock backend counts a request's tokens as 240k
/// for the first one plus 3 bytes a token for what was added since, and
/// refuses one over 272k, as the ChatGPT backend does.
async fn over_the_limit(bytes: usize) -> (bool, usize, Vec<String>, usize, String, Bodies) {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = ModelServer::start(Arc::new({
        let (bodies, refused) = (bodies.clone(), refused.clone());
        move |_, body: String| {
            let tokens = {
                let mut bodies = bodies.lock().unwrap();
                bodies.push(body.clone());
                240_000 + body.len().saturating_sub(bodies[0].len()) / 3
            };
            let refused = refused.clone();
            Box::pin(async move {
                if tokens > arko_ai::input_limit::CHATGPT_OAUTH_MAX_INPUT_TOKENS {
                    refused.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return crate::mock_model::overflow();
                }
                let turn = handing_over(
                    &body,
                    if body.contains("call_m1") {
                        complete("m2", "Done without a handover.")
                    } else {
                        tool_call("m1", "big", serde_json::json!({}))
                    },
                    complete("m3", "Done after the handover."),
                );
                crate::mock_model::with_input(turn, tokens)
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.chatgpt_client(), true);
    harness.settings.main.handover_at = Some(250_000);
    harness.main_config = agents::main_config(&agent_settings_of(&harness)).unwrap();
    harness
        .main_config
        .tool_config
        .available_tool_names
        .push("big".into());
    let _managers = equip(&mut harness, false);
    harness.tools.push(Arc::new(Big(bytes)));
    harness.open(false).unwrap();
    assert_eq!(harness.handover.at(), 250_000);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Read the large result.".into()), &mut inputs),
    )
    .await
    .unwrap();
    harness.flush_printer().await;
    let answered = matches!(end, TurnEnd::Answered);
    let (handovers, reply) = if answered {
        (handovers(&harness), summary(&harness))
    } else {
        (0, String::new())
    };
    (
        answered,
        refused.load(std::sync::atomic::Ordering::SeqCst),
        texts(&harness.feed, "notice"),
        handovers,
        reply,
        bodies,
    )
}

/// One tool result (300 KB, about 100k tokens) takes the next request
/// past the ChatGPT login's limit just before the handover size, by less
/// than arko's lower bound (10 bytes a token) sees, so it goes into the
/// conversation whole. arko asks for the handover report in that request,
/// as its estimate is past the size; the backend refuses it, arko
/// shortens the result and sends it again, and the handover goes ahead.
/// The user sees the refusal as an error notice.
#[cfg(unix)]
#[tokio::test]
async fn a_result_past_the_input_limit_is_shortened_after_a_refusal_and_the_handover_goes_ahead() {
    let (answered, refused, notices, handovers, reply, bodies) = over_the_limit(300_000).await;
    assert!(answered, "{notices:#?}");
    assert_eq!(reply, "Done after the handover.");
    assert_eq!(handovers, 1);
    assert_eq!(refused, 1);
    assert!(
        notices.iter().any(|text| text.starts_with(
            "The request was over the model's input limit, so the result of big was shortened and the request is sent again (1/2)"
        )),
        "{notices:#?}"
    );
    // The report was asked for in the request the result first went in.
    let whole = body(&bodies, "call_m1", "Shortened");
    assert!(whole.len() > 300_000 && whole.contains("Your conversation is getting long"));
    assert!(body(&bodies, "Shortened", "REPORT:").contains("Your conversation is getting long"));
}

/// A result so large (1 MB) that even arko's lower bound puts the request
/// past the limit: arko shortens it as it joins the conversation, before
/// sending, and the backend refuses nothing. What is left is under the
/// handover size, so this turn ends without one.
#[cfg(unix)]
#[tokio::test]
async fn a_result_far_past_the_input_limit_is_shortened_before_sending() {
    let (answered, refused, notices, handovers, reply, bodies) = over_the_limit(1_000_000).await;
    assert!(answered, "{notices:#?}");
    assert_eq!(reply, "Done without a handover.");
    assert_eq!(refused, 0, "{notices:#?}");
    assert_eq!(handovers, 0);
    let shortened = body(&bodies, "call_m1", "");
    assert!(shortened.contains("Shortened") && shortened.len() < 100_000);
}

/// The interface lists a machine may have, by how many links other
/// devices can open.
fn made_up(interfaces: &[(&str, &str)]) -> Vec<remote::Interface> {
    interfaces
        .iter()
        .map(|(name, ip)| remote::Interface {
            name: name.to_string(),
            ip: ip.parse().unwrap(),
            physical: None,
        })
        .collect()
}

/// `/remote qr` picks the first link other devices can open, or the one
/// named; with none, or a name that has none, it says why.
#[test]
fn the_qr_code_shows_the_first_link_or_the_one_named() {
    let any: std::net::SocketAddr = "0.0.0.0:8765".parse().unwrap();
    let ranked =
        |interfaces: &[(&str, &str)]| remote::ranked_links(any, "k", made_up(interfaces), None).0;
    // None: only this machine.
    let local = remote::ranked_links("127.0.0.1:8765".parse().unwrap(), "k", Vec::new(), None).0;
    let error = qr_link(&local, "", "127.0.0.1:8765".parse().unwrap()).unwrap_err();
    assert!(
        error.starts_with("Only this machine can open the page (it listens on 127.0.0.1:8765)"),
        "{error}"
    );
    let lonely = ranked(&[("lo", "127.0.0.1")]);
    assert!(
        qr_link(&lonely, "", any)
            .unwrap_err()
            .starts_with("Only this machine")
    );
    // One.
    let one = ranked(&[("ens33", "192.168.1.12")]);
    assert_eq!(
        qr_link(&one, "", any).unwrap().interface.as_deref(),
        Some("ens33")
    );
    assert_eq!(
        qr_link(&one, "192.168.1.12", any)
            .unwrap()
            .interface
            .as_deref(),
        Some("ens33")
    );
    // Several: the LAN first, the others by name.
    let several = ranked(&[
        ("wg0-mullvad", "10.64.1.2"),
        ("tailscale0", "100.101.102.103"),
        ("ens33", "192.168.1.12"),
    ]);
    assert_eq!(
        qr_link(&several, "", any).unwrap().interface.as_deref(),
        Some("ens33")
    );
    assert_eq!(
        qr_link(&several, "tailscale0", any).unwrap().kind,
        remote::LinkKind::Tailscale
    );
    assert_eq!(
        qr_link(&several, "wg0-mullvad", any).unwrap().kind,
        remote::LinkKind::Vpn
    );
    // An unknown name, or this machine's.
    for name in ["wlan9", "lo", "127.0.0.1"] {
        assert_eq!(
            qr_link(&several, name, any).unwrap_err(),
            format!(
                "The page has no link on `{name}`: its links are on ens33, tailscale0, wg0-mullvad."
            )
        );
    }
}

/// `/remote qr` at the terminal: with the page off, on this machine only,
/// and on every interface, where it draws the code of the first link,
/// and says so for a name that has no link.
#[tokio::test]
async fn remote_qr_draws_the_code_or_says_why_not() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let shown = |harness: &mut Harness, interface: &str| {
        harness.mirrored(|harness| harness.remote_qr(interface));
        texts(&harness.feed, "output")
            .last()
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(
        shown(&mut harness, ""),
        "The web page isn't on: /remote starts it."
    );
    harness
        .start_remote(remote::Bind {
            ip: std::net::Ipv4Addr::LOCALHOST.into(),
            port: Some(0),
        })
        .await;
    assert!(shown(&mut harness, "").starts_with("Only this machine can open the page"));
    harness
        .start_remote(remote::Bind {
            ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
            port: Some(0),
        })
        .await;
    let addr = harness.remote.as_ref().unwrap().addr();
    let key = std::fs::read_to_string(harness.session.dir.join(remote::KEY_FILE)).unwrap();
    let (links, _) = remote::links(addr, key.trim());
    let code = shown(&mut harness, "");
    match links.iter().find(|link| !link.loopback) {
        // This machine has a network: the code of its best link.
        Some(link) => {
            assert!(code.starts_with(&link.url), "{code}");
            assert!(code.lines().count() > 10, "{code}");
        }
        None => assert!(code.starts_with("Only this machine"), "{code}"),
    }
    let unknown = shown(&mut harness, "no-such-interface");
    assert!(
        unknown.starts_with("The page has no link on `no-such-interface`"),
        "{unknown}"
    );
    harness.stop_remote().await;
}

/// Agents in Claude Code are offered Arko's tools only, so outside access
/// reaches them as it reaches the others: the command guard and the file
/// tools' hook decide their calls, and they read the note.
#[cfg(unix)]
#[tokio::test]
async fn outside_access_reaches_agents_in_claude_code() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let allowed = temp.path().join("outside.txt");
    let refused = temp.path().join("outside-2.txt");
    let command = outside_command();
    let turn = |path: &Path, summary: &str| {
        serde_json::json!([
            {"tool": "mcp__arko__shell", "input": {"command": command}},
            {"tool": "mcp__arko__write_file", "input": {"path": path.display().to_string(), "content": "x"}},
            {"tool": "mcp__arko__complete_task", "input": {"summary": summary}},
        ])
    };
    let script = serde_json::json!([turn(&allowed, "On."), turn(&refused, "Off.")]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));
    for (switch, message, reply) in [
        ("/outside on", "Act outside.", "On."),
        ("/outside off", "Again.", "Off."),
    ] {
        assert!(harness.live_command(switch, Source::Terminal));
        let end = tokio::time::timeout(
            Duration::from_secs(60),
            harness.turn(Prompt::User(message.into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), reply);
    }
    assert_eq!(std::fs::read_to_string(&allowed).unwrap(), "x");
    assert!(!refused.exists());
    let (log, _) = fake_log(&fake);
    let results: Vec<(String, bool, String)> = log
        .iter()
        .filter(|entry| entry["event"] == "post")
        .filter(|entry| {
            entry["tool"] == "mcp__arko__shell" || entry["tool"] == "mcp__arko__write_file"
        })
        .map(|entry| {
            (
                entry["tool"].as_str().unwrap().to_string(),
                entry["is_error"] == true,
                entry["output"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(results.len(), 4, "{results:#?}");
    assert!(!results[0].1 && !results[1].1, "on: {results:#?}");
    assert!(
        results[2].1 && results[2].2.contains(GUARD_REFUSAL),
        "{results:#?}"
    );
    assert!(
        results[3].1 && results[3].2.contains(HOOK_REFUSAL),
        "{results:#?}"
    );
    let told = log
        .iter()
        .map(|entry| entry.to_string())
        .collect::<String>();
    assert!(told.contains(ON_NOTE) && told.contains(OFF_NOTE), "{told}");
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// The main agent on `cc:opus`, through arko's scripted stand-in for the
/// `claude` CLI with `script`.
fn in_claude_code(
    temp: &Path,
    script: serde_json::Value,
) -> (Harness, Inputs, mpsc::UnboundedSender<Input>, Managers, PathBuf) {
    let fake = temp.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, inputs, sender) = harness(temp, LLMClient::new(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));
    (harness, inputs, sender, managers, fake)
}

/// The user messages the stand-in CLI received.
fn sent_to_claude_code(fake: &Path) -> Vec<String> {
    let (log, _) = fake_log(fake);
    log.iter()
        .filter(|entry| entry["event"] == "user")
        .map(|entry| entry["text"].as_str().unwrap_or_default().to_string())
        .collect()
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

/// A main agent in Claude Code compacts at the handover size instead of
/// handing over: the call it makes there is held, Claude Code's `/compact`
/// runs in the same conversation, and the agent makes the call again from
/// the summary. The transcript says it in one line.
#[cfg(unix)]
#[tokio::test]
async fn a_main_agent_in_claude_code_compacts_at_the_handover_size() {
    let temp = tempfile::tempdir().unwrap();
    let script = serde_json::json!([
        [
            {"tool": "mcp__arko__shell", "input": {"command": "echo one"}, "input_tokens": 1000},
            {"tool": "mcp__arko__shell", "input": {"command": "echo two"}, "input_tokens": 642_000},
            {"text": "Not reached: the turn was interrupted."},
        ],
        {"compact": {"pre_tokens": 642_000, "post_tokens": 38_000}},
        [
            {"tool": "mcp__arko__shell", "input": {"command": "echo two"}, "input_tokens": 40_000},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Done after the compaction."}},
        ],
    ]);
    let (mut harness, mut inputs, _sender, managers, fake) = in_claude_code(temp.path(), script);
    assert!(harness.handover.compacts());
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Work on it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done after the compaction.");
    assert_eq!(handovers(&harness), 0);
    harness.flush_printer().await;
    assert!(says(
        &harness.feed,
        "notice",
        "Claude Code compacted the conversation: 642k → 38k tokens"
    ));
    assert!(!says(&harness.feed, "notice", "handed over"));
    let calls = call_outcomes(&harness.feed);
    assert!(
        calls
            .iter()
            .any(|(text, outcome)| text.contains("echo two") && outcome == "held for the compaction"),
        "{calls:#?}"
    );
    let sent = sent_to_claude_code(&fake);
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[1], "/compact");
    assert!(sent[2].contains("echo two"), "{}", sent[2]);
    // One Claude Code conversation throughout.
    let (log, _) = fake_log(&fake);
    assert_eq!(log.iter().filter(|entry| entry["event"] == "argv").count(), 1);
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// `/handover` on a main agent in Claude Code compacts: while no turn
/// runs, Claude Code compacts the conversation when the next message
/// starts its turn, before the agent reads the message.
#[cfg(unix)]
#[tokio::test]
async fn handover_compacts_a_main_agent_in_claude_code() {
    let temp = tempfile::tempdir().unwrap();
    let script = serde_json::json!([
        [{"tool": "mcp__arko__complete_task", "input": {"summary": "First done."}, "input_tokens": 1500}],
        {"compact": {"pre_tokens": 1500, "post_tokens": 400}},
        [{"tool": "mcp__arko__complete_task", "input": {"summary": "Second done."}}],
    ]);
    let (mut harness, mut inputs, _sender, managers, fake) = in_claude_code(temp.path(), script);
    assert!(harness.live_command("/handover", Source::Terminal));
    assert!(says(
        &harness.feed,
        "output",
        "There is no conversation to compact"
    ));
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("First.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    harness.live_command("/handover-at", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        "Claude Code compacts the main agent's conversation at 200k tokens of input"
    ));
    harness.live_command("/handover", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        "Claude Code compacts the main agent's conversation when your next message starts its turn"
    ));
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Second done.");
    assert_eq!(handovers(&harness), 0);
    harness.flush_printer().await;
    assert!(says(
        &harness.feed,
        "notice",
        "Claude Code compacted the conversation: 1k → 0k tokens"
    ));
    let sent = sent_to_claude_code(&fake);
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[1], "/compact");
    assert!(sent[2].contains("Second."), "{}", sent[2]);
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}
