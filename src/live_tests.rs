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
const GUARD_REFUSAL: &str = "Refused";
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
            until(|| says(&feed, "output", "Outside access is on,")).await;
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
            until(|| says(&feed, "output", "Outside access is off,")).await;
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
        "Outside access is on, so the agents may delete",
        "Outside access is already on.",
        "`sideways`: use /outside on or /outside off.",
        "Outside access is on, so the agents may delete",
        "Outside access is off, so the agents can't",
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
            until(|| says(&feed, "output", "Outside access is on,")).await;
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

/// The shell tool's name.
const SHELL_TOOL: &str = crate::shell::SHELL_TOOL_NAME;

/// The main agent's run as the last turn left it.
fn handovers(harness: &Harness) -> usize {
    match &harness.conversation {
        Conversation::Answered(result) => result.task().unwrap().handovers.len(),
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
    // The size for the main agent and the workers.
    assert_eq!(
        output(&mut harness, "/handover-at 300k"),
        "The main agent now hands over at 300k tokens of input.\nWorkers now hand over at 300k tokens of input."
    );
    assert_eq!(harness.handover.threshold().get(), Some(300_000));
    assert_eq!(harness.handover.workers_threshold().get(), Some(300_000));
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
            "The main agent now hands over at {} tokens of input, the largest size Arko allows for its model.\nWorkers now hand over at {} tokens of input, the largest size Arko allows for their model.",
            handover::shown(most),
            handover::shown(most)
        )
    );
    assert_eq!(harness.handover.at(), most);
    // The workers' size stays as asked: each worker is held under its own
    // model's room.
    assert_eq!(harness.handover.workers_at(), Some(2_000_000));
    assert_eq!(
        remote::Meter::read(harness.gauges.as_ref())
            .settings
            .workers_handover_at,
        most
    );
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
        harness.session.load_settings().unwrap().workers_handover_at,
        Some(100_000)
    );
    assert_eq!(
        remote::Meter::read(harness.gauges.as_ref())
            .settings
            .handover_at,
        100_000
    );
    assert!(output(&mut harness, "/handover-at").starts_with(
        "The main agent hands over at 100k tokens of input (its latest request was 260k)."
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
                "The page has no link on `{name}`. Its links are on ens33, tailscale0, wg0-mullvad."
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
        "The web page isn't on. /remote starts it."
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

/// The page's sessions are kept in the session's folder, so a page started
/// again lets a browser in without the key, and `/remote off` ends them.
#[tokio::test]
async fn the_pages_sessions_outlast_a_restart_and_end_when_it_is_turned_off() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let local = remote::Bind {
        ip: std::net::Ipv4Addr::LOCALHOST.into(),
        port: Some(0),
    };
    harness.start_remote(local).await;
    let base = format!("http://{}", harness.remote.as_ref().unwrap().addr());
    let key = std::fs::read_to_string(harness.session.dir.join(remote::KEY_FILE)).unwrap();
    let http = arko_ai::http_client();
    let opened = http
        .post(format!("{base}/api/session"))
        .header("content-type", "application/json")
        .body(format!(r#"{{"key": "{}"}}"#, key.trim()))
        .send()
        .await
        .unwrap();
    let cookie = opened.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    // Arko starts the page again, as when the session is resumed: the
    // cookie for the old port is still good on the new one's name.
    harness.stop_remote().await;
    harness.start_remote(local).await;
    let base = format!("http://{}", harness.remote.as_ref().unwrap().addr());
    let port = harness.remote.as_ref().unwrap().addr().port();
    let token = cookie.split_once('=').unwrap().1;
    let again = format!("arko_remote_{port}={token}");
    let checked = |base: String, again: String| {
        let http = http.clone();
        async move {
            http.get(format!("{base}/api/session"))
                .header("cookie", again)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(checked(base.clone(), again.clone()).await, 204);
    // Turned off and on, the page lets it in no more.
    harness.stop_page(Source::Terminal).await;
    assert!(!harness.session.dir.join(remote::SESSIONS_FILE).exists());
    harness.start_remote(local).await;
    let base = format!("http://{}", harness.remote.as_ref().unwrap().addr());
    let port = harness.remote.as_ref().unwrap().addr().port();
    let again = format!("arko_remote_{port}={token}");
    assert_eq!(checked(base, again).await, 401);
    harness.stop_remote().await;
}

/// `/remote off` ends the page's saved sessions also when the page isn't
/// on, as in a session resumed without it, and when they can't be deleted,
/// the session gets a new key, which ends them.
#[tokio::test]
async fn remote_off_ends_the_saved_sessions_when_the_page_isnt_on() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let local = remote::Bind {
        ip: std::net::Ipv4Addr::LOCALHOST.into(),
        port: Some(0),
    };
    harness.start_remote(local).await;
    let addr = harness.remote.as_ref().unwrap().addr();
    let key_path = harness.session.dir.join(remote::KEY_FILE);
    let key = std::fs::read_to_string(&key_path).unwrap();
    let http = arko_ai::http_client();
    let opened = http
        .post(format!("http://{addr}/api/session"))
        .header("content-type", "application/json")
        .body(format!(r#"{{"key": "{}"}}"#, key.trim()))
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status(), 204);
    let token = opened.headers()["set-cookie"].to_str().unwrap();
    let token = token.split(';').next().unwrap().split_once('=').unwrap().1;
    let token = token.to_string();
    harness.stop_remote().await;
    let saved = harness.session.dir.join(remote::SESSIONS_FILE);
    assert!(saved.exists());

    harness
        .remote_command("/remote off", Source::Terminal)
        .await;
    assert!(!saved.exists());
    harness.start_remote(local).await;
    let addr = harness.remote.as_ref().unwrap().addr();
    let checked = http
        .get(format!("http://{addr}/api/session"))
        .header("cookie", format!("arko_remote_{}={token}", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(checked.status(), 401);
    harness.stop_remote().await;

    // A folder in the file's place can't be deleted.
    std::fs::create_dir(&saved).unwrap();
    harness
        .remote_command("/remote off", Source::Terminal)
        .await;
    assert_ne!(std::fs::read_to_string(&key_path).unwrap(), key);
    assert!(
        says(
            &harness.feed,
            "notice",
            "Couldn't delete the web page's saved sessions"
        ),
        "{:?}",
        texts(&harness.feed, "notice")
    );
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
        "Claude Code compacted the conversation from 642k to 38k tokens"
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
        "Claude Code compacted the conversation from 1k to 0k tokens"
    ));
    let sent = sent_to_claude_code(&fake);
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert_eq!(sent[1], "/compact");
    assert!(sent[2].contains("Second."), "{}", sent[2]);
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A main agent in Claude Code shows what it writes beside its calls: its
/// thinking, folded, then each response's text, an item of its own before
/// the call it goes with, with text replies and with `complete_task`.
#[cfg(unix)]
#[tokio::test]
async fn a_main_agent_in_claude_code_shows_its_thinking_and_text_beside_its_calls() {
    for text_replies in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let list = serde_json::json!({"directory": "."});
        let end = if text_replies {
            serde_json::json!({"text": "Done: the workspace is listed."})
        } else {
            serde_json::json!({
                "tool": "mcp__arko__complete_task",
                "input": {"summary": "Done: the workspace is listed."},
            })
        };
        let script = serde_json::json!([[
            {"thinking": "The user wants the files.\nList them first."},
            {"text": "Listing the files.", "continues": true},
            {"tool": "mcp__arko__list_files", "input": list, "continues": true},
            {"text": "Halfway: the files are listed."},
            {"tool": "mcp__arko__list_files", "input": list, "continues": true},
            end,
        ]]);
        let (mut harness, mut inputs, _sender, managers, _fake) =
            in_claude_code(temp.path(), script);
        if text_replies {
            harness.text_replies_command("on", Source::Terminal);
            assert!(harness.main_config.tool_config.ends_task_with_text());
        }
        let end = tokio::time::timeout(
            Duration::from_secs(60),
            harness.turn(Prompt::User("What's in the workspace?".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered), "{text_replies}");
        assert_eq!(summary(&harness), "Done: the workspace is listed.");
        let shown: Vec<(String, String)> = shown(&harness.feed)
            .into_iter()
            .filter(|(kind, _, _)| ["thinking", "text", "call", "reply"].contains(&kind.as_str()))
            .map(|(kind, text, _)| (kind, text))
            .collect();
        let kinds: Vec<&str> = shown.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["thinking", "text", "call", "text", "call", "reply"],
            "{text_replies}: {shown:#?}"
        );
        assert_eq!(shown[0].1, "The user wants the files.\nList them first.");
        assert_eq!(shown[1].1, "Listing the files.");
        assert_eq!(shown[3].1, "Halfway: the files are listed.");
        assert_eq!(shown[5].1, "Done: the workspace is listed.");
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }
}

/// A main agent in Claude Code with text replies answers with `message_user`
/// mid-turn, as it does without them: Arko's tool is served to the CLI (the
/// model sees it as `mcp__arko__message_user`, and the instructions name it
/// `message_user`), the message shows in its order, and the turn ends with
/// the plain reply.
#[cfg(unix)]
#[tokio::test]
async fn a_main_agent_in_claude_code_with_text_replies_answers_with_message_user() {
    let temp = tempfile::tempdir().unwrap();
    let script = serde_json::json!([[
        {"text": "Looking at it.", "continues": true},
        {"tool": "mcp__arko__message_user", "input": {"message": "Halfway: two files to go."}, "continues": true},
        {"text": "Done: nothing left to do."},
    ]]);
    let (mut harness, mut inputs, _sender, managers, fake) = in_claude_code(temp.path(), script);
    harness.text_replies_command("on", Source::Terminal);
    assert!(harness.main_config.tool_config.ends_task_with_text());
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Check the files.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Done: nothing left to do.");
    let shown: Vec<(String, String)> = shown(&harness.feed)
        .into_iter()
        .filter(|(kind, _, _)| ["text", "message", "call", "reply"].contains(&kind.as_str()))
        .map(|(kind, text, _)| (kind, text))
        .collect();
    let item = |kind: &str, text: &str| (kind.to_string(), text.to_string());
    assert_eq!(
        shown,
        [
            item("text", "Looking at it."),
            item("message", "Halfway: two files to go."),
            item("reply", "Done: nothing left to do."),
        ]
    );
    let (log, argv) = fake_log(&fake);
    let served: Vec<String> = log.iter().find(|entry| entry["event"] == "tools").unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(served.contains(&message::MESSAGE_USER.to_string()), "{served:?}");
    assert!(!served.contains(&"complete_task".to_string()), "{served:?}");
    let prompt = after(&argv, "--system-prompt").unwrap();
    assert!(prompt.contains("answer it with `message_user` and carry on with your work"));
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A worker in Claude Code that was running when Arko closed goes on when
/// the session resumes: its Claude Code session resumes, without a message
/// from the lead, and its report reaches the lead.
#[cfg(unix)]
#[tokio::test]
async fn resuming_a_session_restarts_its_workers_in_claude_code() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let kept = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        kept.lock().unwrap().push(body.clone());
        Box::pin(async move {
            if body.contains("Recovered the worker.") {
                complete("m3", "Worker recovery finished.")
            } else if body.contains("call_m1") {
                complete("m2", "Worker is running.")
            } else {
                tool_call(
                    "m1",
                    crew::START_AGENT,
                    serde_json::json!({"agent": WORKER, "task": "Resume this work."}),
                )
            }
        })
    }))
    .await;
    let fake = temp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    // The worker's first turn runs a long command, until Arko closes. Its
    // second, as it resumes, reports.
    let script = serde_json::json!([
        [{"tool": "mcp__arko__shell", "input": {"command": "sleep 30"}}],
        [{"tool": "mcp__arko__complete_task", "input": {"summary": "Recovered the worker."}}],
    ]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let main = harness.settings.main.clone();
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let managers = on_models(&mut harness, &main, &cc, &fake_claude_code(&fake));
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Start it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Worker is running.");
    // Its command runs.
    until(|| {
        std::fs::read_to_string(fake.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .any(|line| line.contains(r#""event": "pre", "tool": "mcp__arko__shell""#))
    })
    .await;
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    let end = tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs))
        .await
        .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Worker recovery finished.");
    let (log, _) = fake_log(&fake);
    let argvs: Vec<Vec<String>> = log
        .iter()
        .filter(|entry| entry["event"] == "argv")
        .map(|entry| serde_json::from_value(entry["argv"].clone()).unwrap())
        .collect();
    assert_eq!(argvs.len(), 2, "{argvs:?}");
    assert!(
        argvs[1].iter().any(|arg| arg.starts_with("--resume=")),
        "its Claude Code session resumes: {:?}",
        argvs[1]
    );
    // The lead isn't told that it stopped as Arko closed: it goes on.
    let told = bodies.lock().unwrap().clone();
    assert!(
        told.iter()
            .all(|body| !body.contains("stopped before it finished")),
        "{told:#?}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A chat in Claude Code becomes an agent's session working in a folder:
/// its conversation goes on in the same Claude Code session, which resumes
/// in the folder it started in with the new one added, and Arko's tools work
/// in the new folder.
#[cfg(unix)]
#[tokio::test]
async fn a_claude_code_chat_moves_to_a_folder_as_it_becomes_an_agent() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("fake");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let script = serde_json::json!([
        [{"text": "Hello."}],
        [
            {"tool": "mcp__arko__shell", "input": {"command": "touch made-here.txt && pwd"}},
            {"text": "Made it."},
        ],
    ]);
    std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let cc = ModelSettings {
        model: "cc:claude-opus-5-5".into(),
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let setup = fake_claude_code(&fake);
    // A new chat: in its session's folder, without tools or workers.
    harness.crew = None;
    harness.providers = setup.clone();
    harness.settings.mode = Mode::Chat;
    harness.settings.workspace = harness.session.dir.clone();
    harness.settings.max_workers = 0;
    harness.settings.text_replies = true;
    harness.settings.main = cc.clone();
    harness.settings.worker = cc.clone();
    let configs = |harness: &Harness| {
        harness
            .agent_configs(&harness.settings.clone(), &setup)
            .unwrap()
    };
    (harness.main_config, harness.worker_config) = configs(&harness);
    harness.open(false).unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Hi.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Hello.");

    // To Agent, in the project: the session opens again, equipped.
    harness
        .switch_mode(Mode::Agent, Some(project.clone()))
        .unwrap();
    assert_eq!(harness.settings.workspace, project);
    harness.reopen = false;
    harness.settings.max_workers = 0;
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.background = managers.background.clone();
    harness.outside = Arc::new(outside::Outside::new(
        false,
        managers.list(),
        project.clone(),
    ));
    (harness.main_config, harness.worker_config) = configs(&harness);
    harness.open(true).unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Make a file.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Made it.");
    assert!(
        project.join("made-here.txt").is_file(),
        "Arko's shell works there"
    );

    // Claude Code resumed its session where it started, with the folder.
    let (log, _) = fake_log(&fake);
    let launches: Vec<(Vec<String>, String)> = log
        .iter()
        .filter(|entry| entry["event"] == "argv")
        .map(|entry| {
            (
                serde_json::from_value(entry["argv"].clone()).unwrap(),
                entry["cwd"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let (argv, cwd) = &launches[1];
    let started = std::fs::canonicalize(&harness.session.dir).unwrap();
    assert_eq!(std::fs::canonicalize(cwd).unwrap(), started);
    assert!(
        argv.iter().any(|arg| arg.starts_with("--resume")),
        "{argv:?}"
    );
    let added = argv
        .iter()
        .position(|arg| arg == "--add-dir")
        .map(|at| argv[at + 1].clone())
        .or_else(|| {
            argv.iter()
                .find_map(|arg| arg.strip_prefix("--add-dir=").map(str::to_string))
        })
        .expect("--add-dir");
    assert_eq!(
        std::fs::canonicalize(added).unwrap(),
        std::fs::canonicalize(&project).unwrap()
    );
    managers.shutdown().await;
}

/// What the user sent worker r1 directly, as the feed has it: its items
/// for r1 that are the user's.
fn to_r1(feed: &remote::Feed) -> Vec<(String, Option<remote::Source>)> {
    feed.subscribe()
        .0
        .items
        .into_iter()
        .filter(|item| item.agent == "r1" && item.kind == remote::Kind::User)
        .map(|item| (item.text, item.source))
        .collect()
}

/// `/message r1 …` typed while the main agent waits for its worker: the
/// running worker reads it at its next step, marked as from the user, and
/// the lead's wait returns a copy with the report. The feed has it as the
/// user's message to r1.
#[tokio::test]
async fn a_message_to_a_running_worker_reaches_it_at_its_next_step() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    if body.contains("call_w1") {
                        complete("w2", "Used tabs.")
                    } else {
                        hold.notified().await;
                        tool_call("w1", SHELL_TOOL, serde_json::json!({"command": "echo hi"}))
                    }
                } else if body.contains("Used tabs.") {
                    complete("m3", "All done.")
                } else if body.contains("call_m1") {
                    let n = body.matches("call_m2").count();
                    tool_call(&format!("m2_{n}"), crew::WAIT_FOR_AGENTS, serde_json::json!({"timeout_seconds": 20}))
                } else {
                    tool_call("m1", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Indent it."}))
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
            sender
                .send(Input::Line("/message r1 Use tabs.".into()))
                .unwrap();
            until(|| !to_r1(&feed).is_empty()).await;
            hold.notify_one();
            sender
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Indent the code.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "All done.");
    let worker = body(&bodies, "call_w1", "");
    assert!(
        worker.contains("The user sent you a message directly. Text you write only in your thinking may never reach them, so answer in your text or in your report:\\n  Use tabs."),
        "{worker}"
    );
    let lead = body(&bodies, "Used tabs.", "You are agent run r1");
    assert!(
        lead.contains("The user sent r1 a message directly (a copy for you):"),
        "{lead}"
    );
    assert_eq!(
        to_r1(&harness.feed),
        [("Use tabs.".to_string(), Some(remote::Source::Terminal))]
    );
    // The command itself isn't in the transcript.
    assert!(!says(&harness.feed, "user", "/message"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/message r1 …` from the web page while the main agent is idle and r1
/// finished: r1 continues with it and reports again. The lead hears of it
/// with its next news, once, before the new report. A worker that isn't
/// there, or no message, gets a warning that keeps the text.
#[tokio::test]
async fn a_message_to_a_finished_worker_continues_it_and_its_lead_hears_of_it() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    if body.contains("Also the docs.") {
                        complete("w2", "Docs done.")
                    } else {
                        complete("w1", "Code done.")
                    }
                } else if body.contains("Docs done.") {
                    complete("m3", "Noted.")
                } else if body.contains("call_m1") {
                    complete("m2", "Started r1.")
                } else {
                    tool_call("m1", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Write the code."}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Start a worker.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let crew = harness.crew.clone().unwrap();
    let workflow = harness.workflow().clone();
    let report = |text: &str| {
        workflow
            .subagents()
            .first()
            .is_some_and(|run| run.report.as_deref() == Some(text))
    };
    until(|| report("Code done.")).await;
    harness.live_command("/message r9 Hello?", Source::Terminal);
    assert!(says(
        &harness.feed,
        "notice",
        "The message didn't reach r9 (there is no worker r9). It said “Hello?”."
    ));
    harness.live_command("/message r1", Source::Terminal);
    assert!(says(&harness.feed, "notice", "/message takes a worker"));
    harness.live_command("/message R1 Also the docs.", Source::Web);
    until(|| report("Docs done.")).await;
    assert_eq!(
        to_r1(&harness.feed),
        [("Also the docs.".to_string(), Some(remote::Source::Web))]
    );
    let news = tokio::time::timeout(Duration::from_secs(30), harness.deliver_news(&mut inputs))
        .await
        .unwrap();
    assert!(matches!(news, News::Done));
    assert_eq!(summary(&harness), "Noted.");
    let lead = body(&bodies, "Docs done.", "You are agent run r1");
    let copy = "The user sent r1 a message directly (a copy for you):";
    assert_eq!(lead.matches(copy).count(), 1, "{lead}");
    assert!(lead.find(copy) < lead.find("Docs done."), "{lead}");
    crew.shutdown().await;
}

/// The user's message to r1, while r1 and r2 both run, sent each way a
/// front end sends it: as the terminal's view of r1 does (a line), as an
/// app does (a message) and as the web page does. Only r1's conversation
/// gets it, as the user's, and the feed has it as r1's item alone. r2 never
/// sees it. The lead gets a copy marked as one, with its next news.
#[tokio::test]
async fn the_users_message_to_a_worker_reaches_that_worker_only() {
    const SENT: [&str; 3] = [
        "Tabs, from the terminal.",
        "Tabs, from an app.",
        "Tabs, from the web page.",
    ];
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let (go, gone) = tokio::sync::watch::channel(false);
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let mut gone = gone.clone();
            Box::pin(async move {
                // Each worker waits for the messages, then takes a step,
                // then reports.
                for (run, step, report) in [("r1", "w", "Indented."), ("r2", "v", "Tested.")] {
                    if body.contains(&format!("You are agent run {run}")) {
                        if body.contains(&format!("call_{step}1")) {
                            return complete(&format!("{step}2"), report);
                        }
                        gone.wait_for(|go| *go).await.unwrap();
                        return tool_call(
                            &format!("{step}1"),
                            SHELL_TOOL,
                            serde_json::json!({"command": "echo hi"}),
                        );
                    }
                }
                if body.contains("Indented.") && body.contains("Tested.") {
                    complete("m9", "All done.")
                } else if body.contains("call_m2") {
                    let n = body.matches("call_m3").count();
                    tool_call(
                        &format!("m3_{n}"),
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 20}),
                    )
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Test it."}),
                    )
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Indent it."}),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let feed = harness.feed.clone();
    let actions = harness.remote_actions.clone();
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            let started = |run: &str| {
                let head = format!("You are agent run {run}");
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains(&head))
            };
            until(|| started("r1") && started("r2")).await;
            let line = |text: &str| format!("/message r1 {text}");
            sender.send(Input::Line(line(SENT[0]))).unwrap();
            sender
                .send(Input::Message(crate::ui::Message {
                    text: line(SENT[1]),
                    images: Vec::new(),
                    answers: Vec::new(),
                }))
                .unwrap();
            actions.send(Action::Message(line(SENT[2]))).unwrap();
            until(|| to_r1(&feed).len() == SENT.len()).await;
            go.send(true).unwrap();
            sender
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Indent and test.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "All done.");

    let bodies = bodies.lock().unwrap().clone();
    let of = |run: &str| -> Vec<&String> {
        let head = format!("You are agent run {run}");
        bodies.iter().filter(|body| body.contains(&head)).collect()
    };
    let r1 = of("r1");
    let read = r1.last().unwrap();
    for text in SENT {
        assert!(
            read.contains(&format!("{}\\n  {text}", crew::USER_MESSAGE_HEADING)),
            "{read}"
        );
    }
    for body in of("r2") {
        assert!(!body.contains("Tabs, from"), "{body}");
        assert!(!body.contains("The user sent"), "{body}");
    }
    // The main agent's conversation has each message once, in the copy of
    // it that its news brings.
    let lead = bodies
        .iter()
        .rfind(|body| !body.contains("You are agent run"))
        .unwrap();
    let copy = "The user sent r1 a message directly (a copy for you):";
    for text in SENT {
        assert_eq!(lead.matches(text).count(), 1, "{lead}");
        let at = lead.find(text).unwrap();
        let heading = lead[..at].rfind(copy).expect(lead);
        assert!(at - heading - copy.len() <= "\\n\\n".len(), "{lead}");
    }
    // The feed has them as r1's, and the user's (in the order the
    // session took them from its inputs).
    let mut items: Vec<(String, remote::Kind, String)> = harness
        .feed
        .subscribe()
        .0
        .items
        .into_iter()
        .filter(|item| item.text.contains("Tabs, from"))
        .map(|item| (item.agent, item.kind, item.text))
        .collect();
    items.sort_by(|a, b| a.2.cmp(&b.2));
    let mut expected = SENT.map(|text| ("r1".to_string(), remote::Kind::User, text.to_string()));
    expected.sort_by(|a, b| a.2.cmp(&b.2));
    assert_eq!(items, expected);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/handover-at workers` reaches a worker that is already running: it
/// hands over at the new size from its next check, under its configured
/// one. `/handover-at main` leaves the workers' size alone, and in a
/// session without workers `/handover-at workers` says there are none.
#[cfg(unix)]
#[tokio::test]
async fn the_workers_size_reaches_a_running_worker() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    let first = !body.contains("call_w1") && !body.contains("REPORT:");
                    if first {
                        hold.notified().await;
                    }
                    // A first request well under the size, then one over it.
                    let (id, input) = if first { ("w1", 10_000) } else { ("w2", 150_000) };
                    let before = crate::mock_model::with_input(
                        tool_call(id, SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                        input,
                    );
                    handing_over(&body, before, complete("w3", "Read it."))
                } else if body.contains("call_m1") {
                    complete("m2", "Started r1.")
                } else {
                    tool_call("m1", crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Read it."}))
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    // The workers' own size, by default: 200k on luna by API key.
    let configured = harness
        .worker_config
        .handover
        .as_ref()
        .and_then(|handover| handover.at_input_tokens);
    assert_eq!(configured, Some(200_000));
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Start a worker.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let output = |harness: &mut Harness, command: &str| {
        assert!(harness.live_command(command, Source::Terminal));
        texts(&harness.feed, "output").last().unwrap().clone()
    };
    assert_eq!(
        output(&mut harness, "/handover-at main 300k"),
        "The main agent now hands over at 300k tokens of input."
    );
    assert_eq!(harness.handover.workers_at(), None);
    assert!(
        output(&mut harness, "/handover-at workers").starts_with(
            "Workers hand over at 200k tokens of input, their model's own size. /handover-at [main|workers] 200k sets another size"
        )
    );
    // While r1 waits for its first answer.
    until(|| !harness.crew.as_ref().unwrap().running().is_empty()).await;
    assert_eq!(
        output(&mut harness, "/handover-at workers 100k"),
        "Workers now hand over at 100k tokens of input, those running from their next step."
    );
    assert_eq!(harness.handover.at(), 300_000, "the main agent's stays");
    assert_eq!(
        harness.session.load_settings().unwrap().workers_handover_at,
        Some(100_000)
    );
    hold.notify_one();
    let workflow = harness.workflow().clone();
    until(|| {
        workflow
            .subagents()
            .first()
            .is_some_and(|run| run.report.as_deref() == Some("Read it."))
    })
    .await;
    let asked = body(&bodies, "Your conversation is getting long", "REPORT:");
    assert!(asked.contains("You are agent run r1"), "{asked}");
    harness.crew.as_ref().unwrap().shutdown().await;

    // A session without workers.
    let temp = tempfile::tempdir().unwrap();
    let (mut alone, _inputs, _sender) = super::harness(temp.path(), LLMClient::new(), true);
    alone.settings.max_workers = 0;
    alone.crew = None;
    assert!(alone.live_command("/handover-at workers 100k", Source::Terminal));
    assert!(says(&alone.feed, "output", "This session has no workers"));
    assert_eq!(
        output(&mut alone, "/handover-at 250k"),
        "The main agent now hands over at 250k tokens of input."
    );
    assert_eq!(alone.handover.workers_at(), None);
}

/// A response whose stream Claude Code abandons, then the same response
/// again without streaming, with its call. The feed keeps the replacement's
/// text beside the call.
#[cfg(unix)]
#[tokio::test]
async fn text_of_a_response_asked_again_without_streaming_stays_in_the_feed() {
    let temp = tempfile::tempdir().unwrap();
    let script = serde_json::json!([[
        {"stream_abandoned": "Let me che"},
        {"text": "Let me check the notes.", "unstreamed": true},
        {"tool": "mcp__arko__shell", "input": {"command": "echo hi"}, "continues": true, "unstreamed": true},
        {"text": "It printed hi.", "stream": true},
        {"tool": "mcp__arko__complete_task", "input": {"summary": "Done."}, "continues": true},
    ]]);
    let (mut harness, mut inputs, _sender, managers, _fake) = in_claude_code(temp.path(), script);
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Check the notes.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    harness.flush_printer().await;
    assert_eq!(
        texts(&harness.feed, "text"),
        ["Let me check the notes.", "It printed hi."]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// The note kept in these tests.
const BUILD_BOX: &str = "Tear the build box down when the last build is done.";

/// A note the main agent keeps comes back, listed as current, in the fresh
/// conversation after a handover. The turns before don't carry it, and a
/// new conversation's first message starts with it.
#[cfg(unix)]
#[tokio::test]
async fn kept_notes_come_back_after_a_handover_and_start_a_new_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if !body.contains("Second.") {
                    if body.contains("call_k1") {
                        complete("a1", "First done.")
                    } else {
                        tool_call(
                            "k1",
                            kept_notes::KEEP_NOTE,
                            serde_json::json!({"text": BUILD_BOX}),
                        )
                    }
                } else {
                    // The fresh conversation goes on until it has the notes.
                    let steps = body.matches("call_s").count();
                    let after = if body.contains("# Your kept notes") || steps > 100 {
                        complete("a2", "Second done.")
                    } else {
                        tool_call(
                            &format!("s{steps}"),
                            SHELL_TOOL,
                            serde_json::json!({"command": "echo hi"}),
                        )
                    };
                    handing_over(
                        &body,
                        tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                        after,
                    )
                }
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
    assert_eq!(harness.kept.list().unwrap()[0].text, BUILD_BOX);
    harness.live_command("/handover", Source::Terminal);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(handovers(&harness), 1);
    let heading = "# Your kept notes";
    // Only the fresh conversation has them, not each turn before.
    for body in bodies.lock().unwrap().iter() {
        assert!(!body.contains(heading) || body.contains("REPORT:"));
    }
    assert!(body(&bodies, heading, "").contains(BUILD_BOX));
    // A new conversation starts with them.
    harness.new_conversation();
    let message = harness.compose(Prompt::User("Third.".into()));
    assert!(message.starts_with(heading) && message.contains(BUILD_BOX));
}

/// Claude Code's compaction leaves the main agent a summary, and its kept
/// notes come back right after it.
#[cfg(unix)]
#[tokio::test]
async fn kept_notes_come_back_after_claude_code_compacts_the_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let script = serde_json::json!([
        [
            {"tool": "mcp__arko__keep_note", "input": {"text": BUILD_BOX}, "input_tokens": 1000},
            {"tool": "mcp__arko__shell", "input": {"command": "echo two"}, "input_tokens": 642_000},
            {"text": "Not reached: the turn was interrupted."},
        ],
        {"compact": {"pre_tokens": 642_000, "post_tokens": 38_000}},
        [
            // (The note is posted as the event arrives, a moment after.)
            {"sleep": 0.5},
            {"tool": "mcp__arko__shell", "input": {"command": "echo two"}, "input_tokens": 40_000},
            {"tool": "mcp__arko__shell", "input": {"command": "echo three"}, "input_tokens": 40_000},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Done after the compaction."}},
        ],
    ]);
    let (mut harness, mut inputs, _sender, managers, fake) = in_claude_code(temp.path(), script);
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User("Work on it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    // The note reaches the agent once after the compaction, in a message
    // Arko sends it or with a tool result, and not before.
    let (log, _) = fake_log(&fake);
    let compacted = log
        .iter()
        .position(|entry| entry["text"] == "/compact")
        .unwrap();
    let given = |entries: &[serde_json::Value]| -> Vec<String> {
        entries
            .iter()
            .flat_map(|entry| {
                [
                    entry["text"].as_str(),
                    entry["answer"]["hookSpecificOutput"]["additionalContext"].as_str(),
                ]
            })
            .flatten()
            .filter(|text| text.contains("# Your kept notes"))
            .map(str::to_string)
            .collect()
    };
    assert!(given(&log[..compacted]).is_empty());
    let after = given(&log[compacted..]);
    assert!(
        after.len() == 1 && after[0].contains(BUILD_BOX),
        "{after:?}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A worker gets the notes for it in its first message, and a note kept
/// while it runs in its next step, with its own heading. Another worker
/// doesn't get a note for the first.
#[cfg(unix)]
#[tokio::test]
async fn workers_get_the_kept_notes_for_them() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                let keep = |id: &str, text: &str, audience: &str| {
                    tool_call(
                        id,
                        kept_notes::KEEP_NOTE,
                        serde_json::json!({"text": text, "audience": audience}),
                    )
                };
                let start = |id: &str, task: &str| {
                    tool_call(
                        id,
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": task}),
                    )
                };
                if body.contains("You are agent run r1") {
                    if body.contains("call_w1") {
                        complete("w2", "First done.")
                    } else {
                        hold.notified().await;
                        tool_call("w1", SHELL_TOOL, serde_json::json!({"command": "echo hi"}))
                    }
                } else if body.contains("You are agent run r2") {
                    complete("v1", "Second done.")
                } else if body.contains("First done.") && body.contains("Second done.") {
                    complete("m6", "All done.")
                } else if body.contains("call_m4") {
                    let n = body.matches("call_m5").count();
                    tool_call(
                        &format!("m5_{n}"),
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 20}),
                    )
                } else if body.contains("call_m3") {
                    start("m4", "Second worker.")
                } else if body.contains("call_m2") {
                    keep("m3", "Skip the slow tests.", "lead, r1")
                } else if body.contains("call_m1") {
                    start("m2", "First worker.")
                } else {
                    keep("m1", "Use tabs.", "workers")
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let typing = tokio::spawn({
        let bodies = bodies.clone();
        async move {
            until(|| {
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains("call_m3"))
            })
            .await;
            hold.notify_one();
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Start the workers.".into()), &mut inputs),
    )
    .await
    .unwrap();
    typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "All done.");
    // The main agent has its own call and the result, and no copy of the
    // message to r1.
    for main in bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|body| !body.contains("You are agent run"))
    {
        assert!(main.matches("Skip the slow tests.").count() <= 1);
    }
    assert!(body(&bodies, "call_m3", "You are agent run").contains("Kept note 2. r1 has it now."));
    let heading = "# Notes from the main agent";
    let first = body(&bodies, "You are agent run r1", "call_w1");
    assert!(first.contains(heading) && first.contains("Use tabs."));
    assert!(!first.contains("Skip the slow tests."));
    let later = body(&bodies, "call_w1", "");
    assert!(later.contains("The main agent's notes for you."));
    assert!(later.contains("Use tabs.") && later.contains("Skip the slow tests."));
    let second = body(&bodies, "You are agent run r2", "");
    assert!(second.contains(heading) && second.contains("Use tabs."));
    assert!(!second.contains("Skip the slow tests."));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A note forgotten before a handover is replaced by a list that says none
/// are kept, since the restated first message still has it.
#[cfg(unix)]
#[tokio::test]
async fn a_forgotten_note_is_not_current_after_a_handover() {
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
                    // The fresh conversation goes on until it has the list.
                    let steps = body.matches("call_s").count();
                    let after = if body.contains("No notes are kept now.") || steps > 100 {
                        complete("a2", "Second done.")
                    } else {
                        tool_call(
                            &format!("s{steps}"),
                            SHELL_TOOL,
                            serde_json::json!({"command": "echo hi"}),
                        )
                    };
                    handing_over(
                        &body,
                        tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                        after,
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    harness
        .kept
        .add(BUILD_BOX, kept_notes::Audience::Lead)
        .unwrap();
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    // The build box is down, and the agent forgets the note.
    harness.live_command("/notes remove 1", Source::Terminal);
    harness.live_command("/handover", Source::Terminal);
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(handovers(&harness), 1);
    // The restated first message has the note, and what follows replaces it.
    let fresh = body(&bodies, "No notes are kept now.", "");
    assert!(fresh.contains("REPORT:"));
    assert!(fresh.find(BUILD_BOX).unwrap() < fresh.find("No notes are kept now.").unwrap());
}

/// A branch of a session has its kept notes, which its conversation holds
/// the calls for.
#[tokio::test]
async fn a_branch_has_the_kept_notes() {
    let temp = tempfile::tempdir().unwrap();
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Hi.") }))).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness
        .kept
        .add(BUILD_BOX, kept_notes::Audience::Lead)
        .unwrap();
    let branch = harness.branch(None).unwrap();
    let branch = Session::open(&harness.root, &branch).unwrap();
    let notes = kept_notes::KeptNotes::new(&branch.dir).list().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].text, BUILD_BOX);
}

/// `/notes` lists the session's notes, removes one by its number, and says
/// what it can't do.
#[tokio::test]
async fn notes_lists_and_removes_the_kept_notes() {
    let temp = tempfile::tempdir().unwrap();
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Hi.") }))).await;
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/notes", Source::Terminal));
    assert!(says(&harness.feed, "output", "No notes are kept."));
    harness
        .kept
        .add(BUILD_BOX, kept_notes::Audience::Lead)
        .unwrap();
    harness
        .kept
        .add("Use tabs.", kept_notes::Audience::AllWorkers)
        .unwrap();
    harness.live_command("/notes", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        &format!("1  for the main agent\n   {BUILD_BOX}\n2  for every worker\n   Use tabs.")
    ));
    harness.live_command("/notes remove 1", Source::Terminal);
    assert!(says(&harness.feed, "output", "Removed note 1."));
    assert_eq!(harness.kept.list().unwrap().len(), 1);
    harness.live_command("/notes remove 1", Source::Terminal);
    assert!(says(
        &harness.feed,
        "output",
        "There is no kept note 1. /notes lists them."
    ));
    harness.live_command("/notes remove soon", Source::Terminal);
    assert!(says(&harness.feed, "output", "/notes lists the kept notes"));
}

/// A worker that hands over gets its notes again in the fresh conversation,
/// including one kept after it started, which its restated first message
/// can't have.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_gets_its_kept_notes_again_after_its_own_handover() {
    a_worker_hands_over(false).await;
}

/// A note forgotten after the worker started is replaced by a list that says
/// none are kept, since the restated first message still has the note.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_is_told_a_forgotten_note_is_gone_after_its_own_handover() {
    a_worker_hands_over(true).await;
}

/// The worker r1 has a note for it, and hands over. With `forget` the note
/// is in its first message and is forgotten while it runs, else it is kept
/// while it runs.
#[cfg(unix)]
async fn a_worker_hands_over(forget: bool) {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(Notify::new());
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    let steps = body.matches("call_s").count();
                    let told = body.contains("The main agent's notes for you.");
                    let after = if (body.contains("REPORT:") && told) || steps > 100 {
                        complete("w9", "First done.")
                    } else {
                        tool_call(
                            &format!("s{steps}"),
                            SHELL_TOOL,
                            serde_json::json!({"command": "echo hi"}),
                        )
                    };
                    if !body.contains("call_w1") && !body.contains("REPORT:") {
                        hold.notified().await;
                    }
                    // A first request under the size, then one over it.
                    let (id, input) = if body.contains("call_w1") {
                        ("w2", 100_000)
                    } else {
                        ("w1", 10_000)
                    };
                    handing_over(
                        &body,
                        crate::mock_model::with_input(
                            tool_call(id, SHELL_TOOL, serde_json::json!({"command": "echo hi"})),
                            input,
                        ),
                        after,
                    )
                } else if body.contains("First done.") {
                    complete("m4", "All done.")
                } else if body.contains("call_m2") || (forget && body.contains("call_m1")) {
                    let n = body.matches("call_m3").count();
                    tool_call(
                        &format!("m3_{n}"),
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 20}),
                    )
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        kept_notes::KEEP_NOTE,
                        serde_json::json!({"text": "Skip the slow tests.", "audience": "r1"}),
                    )
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "First worker."}),
                    )
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    harness.live_command("/handover-at workers 20k", Source::Terminal);
    let r1 = kept_notes::Audience::Workers(vec!["r1".into()]);
    if forget {
        harness.kept.add("Skip the slow tests.", r1).unwrap();
    }
    let typing = tokio::spawn({
        let (bodies, kept) = (bodies.clone(), harness.kept.clone());
        // Once r1 has started, and with it the note in its first message.
        let started = if forget { "call_m1" } else { "call_m2" };
        async move {
            until(|| {
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains(started))
            })
            .await;
            if forget {
                kept.remove(1).unwrap();
            }
            hold.notify_one();
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Start the worker.".into()), &mut inputs),
    )
    .await
    .unwrap();
    typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "All done.");
    let fresh = |text: &str| {
        bodies
            .lock()
            .unwrap()
            .iter()
            .find(|body| body.contains("REPORT:") && body.contains(text))
            .cloned()
    };
    if forget {
        // The restated first message has the note, and the list after it
        // says none are kept.
        let body = fresh("No notes are kept now.").unwrap();
        assert!(
            body.find("Skip the slow tests.").unwrap()
                < body.find("No notes are kept now.").unwrap()
        );
    } else {
        // The note was kept after the worker started, so its restated first
        // message doesn't have it.
        let body = fresh("Skip the slow tests.").unwrap();
        assert!(body.contains("The main agent's notes for you."));
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A note the user removes with `/notes remove` is announced to the main
/// agent in its next request: once, with the start of its first line.
#[tokio::test]
async fn a_removed_note_is_announced_to_the_main_agent_once() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            let mut bodies = bodies.lock().unwrap();
            bodies.push(body);
            let id = format!("m{}", bodies.len());
            Box::pin(async move { complete(&id, "Hi.") })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let note = "Tear the build box down when the last build is done, and tell the user what it cost.\nThe box is named build-1.";
    harness.kept.add(note, kept_notes::Audience::Lead).unwrap();
    for message in ["First.", "Second.", "Third."] {
        if message == "Second." {
            harness.live_command("/notes remove 1", Source::Terminal);
        }
        let end = harness
            .turn(Prompt::User(message.into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
    }
    // The request after the removal has the line, and the one after that
    // has the same single line in the conversation. (The bodies are JSON.)
    let line = r#"The user removed kept note 1 (\"Tear the build box down when the last build is done, and tel…\")."#;
    let held: Vec<usize> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| body.matches(line).count())
        .collect();
    assert_eq!(held, [0, 1, 1]);
}

/// The user removes a note for the worker r1 while r1 and r2 work, and r1
/// hears of it under its notes heading. The worker r2 doesn't.
#[cfg(unix)]
#[tokio::test]
async fn a_removed_note_reaches_the_workers_that_had_it() {
    workers_hear_of_a_removed_note(false).await;
}

/// The main agent removes the note with `forget_note`: r1 hears of it as
/// the main agent's doing, and the main agent isn't told what it did.
#[cfg(unix)]
#[tokio::test]
async fn a_note_the_main_agent_forgets_reaches_the_workers_that_had_it() {
    workers_hear_of_a_removed_note(true).await;
}

/// The note for r1 is removed (by the main agent with `by_agent`, else by
/// the user) once r1 and r2 both wait for their first answer.
#[cfg(unix)]
async fn workers_hear_of_a_removed_note(by_agent: bool) {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Bodies = Arc::default();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    // Both workers have asked for their first answer.
    fn both_wait(bodies: &Bodies) -> bool {
        let bodies = bodies.lock().unwrap();
        ["You are agent run r1", "You are agent run r2"]
            .iter()
            .all(|worker| bodies.iter().any(|body| body.contains(worker)))
    }
    let server = ModelServer::start(Arc::new({
        let (bodies, hold) = (bodies.clone(), hold.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let (bodies, hold) = (bodies.clone(), hold.clone());
            Box::pin(async move {
                let start = |id: &str, task: &str| {
                    tool_call(
                        id,
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": task}),
                    )
                };
                let shell =
                    |id: &str| tool_call(id, SHELL_TOOL, serde_json::json!({"command": "echo hi"}));
                if body.contains("You are agent run r1") {
                    if body.contains("call_w1") {
                        complete("w2", "First done.")
                    } else {
                        hold.acquire().await.unwrap().forget();
                        shell("w1")
                    }
                } else if body.contains("You are agent run r2") {
                    if body.contains("call_v1") {
                        complete("v2", "Second done.")
                    } else {
                        hold.acquire().await.unwrap().forget();
                        shell("v1")
                    }
                } else if body.contains("First done.") && body.contains("Second done.") {
                    complete("m6", "All done.")
                } else if body.contains("call_m3") || (!by_agent && body.contains("call_m2")) {
                    let n = body.matches("call_m4").count();
                    tool_call(
                        &format!("m4_{n}"),
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 20}),
                    )
                } else if body.contains("call_m2") {
                    until(|| both_wait(&bodies)).await;
                    tool_call("m3", kept_notes::FORGET_NOTE, serde_json::json!({"id": 1}))
                } else if body.contains("call_m1") {
                    start("m2", "Second worker.")
                } else {
                    start("m1", "First worker.")
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let _managers = equip(&mut harness, false);
    let note = "Skip the slow tests.";
    harness
        .kept
        .add(note, kept_notes::Audience::Workers(vec!["r1".into()]))
        .unwrap();
    let typing = tokio::spawn({
        let (kept, workflow) = (harness.kept.clone(), harness.workflow().clone());
        let (bodies, hold) = (bodies.clone(), hold.clone());
        async move {
            if by_agent {
                // The main agent has the result of forgetting the note.
                until(|| {
                    bodies
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|body| body.contains("call_m3"))
                })
                .await;
            } else {
                until(|| both_wait(&bodies)).await;
                let note = kept.remove(1).unwrap();
                kept.tell_removed(&workflow, &note, true);
            }
            hold.add_permits(2);
        }
    });
    let end = tokio::time::timeout(
        Duration::from_secs(40),
        harness.turn(Prompt::User("Start the workers.".into()), &mut inputs),
    )
    .await
    .unwrap();
    typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "All done.");
    let by = if by_agent {
        "The main agent"
    } else {
        "The user"
    };
    let line = format!(r#"{by} removed kept note 1 (\"Skip the slow tests.\")."#);
    let count = |bodies: &[String], line: &str| -> Vec<usize> {
        bodies
            .iter()
            .map(|body| body.matches(line).count())
            .collect()
    };
    let bodies = bodies.lock().unwrap().clone();
    let (workers, mains): (Vec<String>, Vec<String>) = bodies
        .into_iter()
        .partition(|body| body.contains("You are agent run"));
    let (r1, r2): (Vec<String>, Vec<String>) = workers
        .into_iter()
        .partition(|body| body.contains("You are agent run r1"));
    // r1 has the line once, under its notes heading, and r2 never does.
    assert_eq!(count(&r1, &line), [0, 1]);
    assert!(r1[1].contains("The main agent's notes for you."));
    assert!(!r1[1].contains("This list replaces any earlier one."));
    assert!(count(&r2, "removed kept note").iter().all(|n| *n == 0));
    // The main agent hears of the user's doing once, and not of its own.
    let mains = count(&mains, "removed kept note");
    assert!(mains.iter().all(|n| *n <= usize::from(!by_agent)));
    assert_eq!(mains.contains(&1), !by_agent);
    harness.crew.as_ref().unwrap().shutdown().await;
}
