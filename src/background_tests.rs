// End-to-end tests of shell commands that move to the background: after
// the session's time (`/background-after`), or when the user presses
// Ctrl+B, and the finished job waking the idle main agent. Each follows a
// path a user takes, with the mock model. (Included in `main`'s tests,
// whose helpers they use.)

#[cfg(unix)]
use crate::mock_model::tool_call;
use crate::mock_model::{ModelServer, complete};

const SHELL_TOOL: &str = crate::shell::SHELL_TOOL_NAME;

/// Gives `harness` the session's shell and terminal tools, with commands
/// moving to the background after `after` seconds (0: never), and opens
/// its workflow again. The jobs that finish in the background reach
/// `inputs`, as in a session the user takes part in.
fn equip(harness: &mut Harness, inputs: &mut Inputs, after: u64) -> Managers {
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    managers.background.set_after(after);
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.background = managers.background.clone();
    harness.open(false).unwrap();
    inputs.finished = Some(managers.jobs.subscribe_finished());
    inputs.alerts = Some(managers.jobs.subscribe_alerts());
    managers
}

/// Monitor limits that send news soon: notes `interval` apart at least,
/// news gathered for a tenth of a second, and a start that doesn't wait.
fn quick_monitors(managers: &Managers, interval: Duration) {
    let mut limits = arko_agents::terminal::MonitorLimits::default();
    limits.alert_interval = interval;
    limits.gather = Duration::from_millis(100);
    limits.start_wait = Duration::ZERO;
    managers.jobs.set_monitor_limits(limits).unwrap();
}

/// The next event, which is a monitor's alert, within `within`.
async fn next_alert(inputs: &mut Inputs, within: Duration) -> crate::inputs::Alert {
    match tokio::time::timeout(within, inputs.next_event(true))
        .await
        .expect("an alert in time")
    {
        Event::Alert(alert) => alert,
        _ => panic!("expected a monitor's alert"),
    }
}

/// A model that records the requests' bodies.
type Bodies = Arc<Mutex<Vec<String>>>;

/// A command still running at the set time moves to the background and the
/// turn ends. When the job finishes, with the agent idle, its update
/// starts a turn, which the transcript shows as it shows workers' news.
#[cfg(unix)]
#[tokio::test]
async fn a_long_command_moves_to_the_background_and_its_end_wakes_the_idle_agent() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A background job of yours finished") {
                    complete("m3", "The build finished.")
                } else if body.contains("call_m1") {
                    complete("m2", "The build goes on in the background.")
                } else {
                    tool_call(
                        "m1",
                        SHELL_TOOL,
                        serde_json::json!({"command": "echo started; sleep 2; echo built"}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 1);
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Build it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "The build goes on in the background.");
    let moved = bodies.lock().unwrap()[1].clone();
    assert!(moved.contains("in the background as job "), "{moved}");
    assert!(
        moved.contains("It ran longer than 1 second, so it moved to the background."),
        "{moved}"
    );
    // The call shows as moved, not as failed.
    let call = harness
        .feed
        .subscribe()
        .0
        .items
        .into_iter()
        .find(|item| item.kind == remote::Kind::Call)
        .unwrap();
    assert_eq!(call.status, Some(remote::CallStatus::Ok));
    assert!(
        call.outcome
            .as_deref()
            .is_some_and(|outcome| outcome.starts_with("in the background · job ")),
        "{call:?}"
    );

    // The job ends with the agent idle: that starts a turn.
    let finished = match tokio::time::timeout(Duration::from_secs(10), inputs.next_event(true))
        .await
        .unwrap()
    {
        Event::Finished(finished) => finished,
        _ => panic!("expected a finished job"),
    };
    assert_eq!(finished.state, arko_agents::terminal::JobState::Succeeded);
    assert!(matches!(
        harness.deliver_finished(finished, &mut inputs).await,
        News::Done
    ));
    assert_eq!(summary(&harness), "The build finished.");
    let woken = bodies.lock().unwrap()[2].clone();
    assert!(woken.contains("Terminal update"), "{woken}");
    assert!(woken.contains("succeeded"), "{woken}");
    assert!(woken.contains("built"), "{woken}");
    assert!(
        shown(&harness.feed)
            .iter()
            .any(|(kind, text, _)| kind == "notice"
                && text.contains("a background job finished, and its update goes to the agent")),
        "{:?}",
        shown(&harness.feed)
    );
    managers.shutdown().await;
}

/// A job that finishes while the agent makes its last request still starts
/// a turn once the agent is idle: its update came too late for that request,
/// and the agent reads it once, in the turn the job starts.
#[cfg(unix)]
#[tokio::test]
async fn a_job_that_ends_during_the_last_request_still_wakes_the_agent() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A background job of yours finished") {
                    complete("m3", "The build finished.")
                } else if body.contains("call_m1") {
                    // The job ends while this reply is on its way.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    complete("m2", "The build goes on in the background.")
                } else {
                    tool_call(
                        "m1",
                        SHELL_TOOL,
                        serde_json::json!({"command": "sleep 2; echo built"}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 1);
    let turn = harness.turn(Prompt::User("Build it.".into()), &mut inputs);
    let end = tokio::time::timeout(Duration::from_secs(20), turn).await;
    assert!(matches!(end, Ok(TurnEnd::Answered)));
    assert_eq!(summary(&harness), "The build goes on in the background.");
    let finished = match tokio::time::timeout(Duration::from_secs(1), inputs.next_event(true))
        .await
        .expect("the job that ended during the turn wakes the agent")
    {
        Event::Finished(finished) => finished,
        _ => panic!("expected a finished job"),
    };
    assert!(matches!(
        harness.deliver_finished(finished, &mut inputs).await,
        News::Done
    ));
    assert_eq!(summary(&harness), "The build finished.");
    let woken = body_with(&bodies, "A background job of yours finished");
    assert_eq!(woken.matches("Terminal update").count(), 1, "{woken}");
    assert!(woken.contains("built"), "{woken}");
    managers.shutdown().await;
}

/// Ctrl+B moves the command running at once, also with the setting off,
/// and what the user types next reaches the agent as a note.
#[cfg(unix)]
#[tokio::test]
async fn ctrl_b_moves_the_running_command_and_the_user_writes_at_once() {
    let bodies: Bodies = Arc::default();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let server = ModelServer::start(Arc::new({
        let (bodies, calls) = (bodies.clone(), calls.clone());
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let calls = calls.clone();
            Box::pin(async move {
                if body.contains("Check the logs too.") {
                    complete("done", "Moved, and I read your note.")
                } else {
                    // The command, then quick ones until the note is read.
                    let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let command = if call == 1 { "sleep 30" } else { "true" };
                    tool_call(
                        &format!("m{call}"),
                        SHELL_TOOL,
                        serde_json::json!({"command": command}),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let background = harness.background.clone();
    let typing = tokio::spawn(async move {
        until(|| !background.movable().is_empty()).await;
        sender
            .send(Input::Background(remote::MAIN_AGENT.into()))
            .unwrap();
        sender
            .send(Input::Line("Check the logs too.".into()))
            .unwrap();
        // (Kept until the turn ends: a closed input quits.)
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Run it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    typing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "Moved, and I read your note.");
    let moved = body_with(&bodies, "call_m1");
    assert!(
        moved.contains("The user moved it to the background."),
        "{moved}"
    );
    assert!(
        shown(&harness.feed)
            .iter()
            .any(|(kind, text, delivery)| kind == "user"
                && text == "Check the logs too."
                && delivery == "note"),
        "{:?}",
        shown(&harness.feed)
    );
    // The command runs on as a job the terminal tools see, until stopped.
    let run = match &harness.conversation {
        Conversation::Answered(result) => result.agent_run_id,
        _ => unreachable!(),
    };
    let job: uuid::Uuid = moved
        .split("in the background as job ")
        .nth(1)
        .and_then(|rest| rest.get(..36))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        managers.jobs.status(run, job, None).unwrap().state,
        arko_agents::terminal::JobState::Running
    );
    managers.shutdown().await;
}

/// At the line prompt, `/exit` with a job in the background only warns, as
/// the job stops when Arko exits, and the next `/exit` quits. A job that
/// started after the warning is asked about too, rather than stopped. The
/// full-screen view is told to ask in its popup instead. Quitting readies
/// the jobs for the exit: none starts after it.
#[cfg(unix)]
#[tokio::test]
async fn exit_with_a_background_job_warns_first_at_the_line_prompt() {
    let server = ModelServer::start(Arc::new(|_, body: String| {
        Box::pin(async move {
            if body.contains("call_m1") {
                complete("m2", "It goes on in the background.")
            } else {
                tool_call("m1", SHELL_TOOL, serde_json::json!({"command": "sleep 30"}))
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 1);
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Wait a while.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    // The job shows the command the agent gave, not the shell's wrapping.
    let jobs = harness.background.jobs();
    assert_eq!(
        jobs.iter()
            .map(|job| job.command.as_str())
            .collect::<Vec<_>>(),
        ["sleep 30"]
    );
    let first = jobs[0].job_id;
    // `/exit` at the line prompt (Ctrl+D and Ctrl+C twice are `/exit`).
    let exit = |harness: &mut Harness| {
        let shown = harness.jobs_shown.clone();
        harness.mirrored(|harness| harness.may_exit(Some(shown)))
    };
    let warnings = |harness: &Harness| -> Vec<String> {
        shown(&harness.feed)
            .into_iter()
            .filter(|(kind, text, _)| kind == "output" && text.contains("/exit again"))
            .map(|(_, text, _)| text)
            .collect()
    };
    assert!(!exit(&mut harness));
    assert_eq!(
        warnings(&harness),
        ["1 background job is running. /exit again stops it and quits."]
    );

    let workspace = temp.path().to_path_buf();
    let start = |command: &str| {
        let context = arko_agents::TaskContext::new(uuid::Uuid::new_v4(), "t", "t", &workspace);
        managers
            .jobs
            .start(&context, command, Default::default())
            .map(|job| job.job_id)
    };
    let second = start("sleep 30").unwrap();
    assert!(!exit(&mut harness), "the second job wasn't warned of");
    assert_eq!(
        warnings(&harness)[1],
        "2 background jobs are running. /exit again stops them and quits."
    );

    // The full-screen view asks in its popup, with the jobs running.
    let (popup, mut asks) = tokio::sync::mpsc::unbounded_channel();
    harness.quit_popup = Some(popup);
    harness.exit_warned = false;
    assert!(!harness.mirrored(|harness| harness.may_exit(Some(Vec::new()))));
    let asked = asks.try_recv().unwrap();
    assert_eq!(
        asked.jobs.iter().map(|job| job.job_id).collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(warnings(&harness).len(), 2, "the popup asks instead");
    // With asking turned off, it quits; with both agreed to, too.
    assert!(harness.mirrored(|harness| harness.may_exit(None)));
    assert!(
        start("sleep 30").is_ok(),
        "asking off doesn't ready the jobs"
    );
    harness.exit_warned = false;
    let third = harness.background.jobs()[2].job_id;
    assert!(harness.mirrored(|harness| harness.may_exit(Some(vec![first, second, third]))));
    assert!(start("sleep 30").is_err(), "no job starts once Arko quits");
    managers.shutdown().await;
}

/// The first body with `with`.
fn body_with(bodies: &Bodies, with: &str) -> String {
    bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains(with))
        .unwrap_or_else(|| panic!("no request with {with:?}"))
        .clone()
}

/// `/background-after` says when commands move, sets it (or turns it off),
/// keeps it in the session, and tells the agents: the shell follows at once
/// and the next request carries the note.
#[tokio::test]
async fn the_background_after_command_sets_keeps_and_tells() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { complete("m1", "Noted.") })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 600);
    assert!(harness.live_command("/background-after", Source::Terminal));
    assert!(harness.live_command("/background-after soon", Source::Terminal));
    assert!(harness.live_command("/background-after 5m", Source::Web));
    assert_eq!(harness.background.after(), Some(Duration::from_secs(300)));
    assert_eq!(
        harness.session.load_settings().unwrap().background_after,
        300
    );
    let outputs = shown(&harness.feed);
    let said = |start: &str| {
        outputs
            .iter()
            .any(|(kind, text, _)| (kind == "output" || kind == "notice") && text.contains(start))
    };
    assert!(said(
        "A shell command or another long call (an MCP tool, index_create) still running after 10 minutes moves to the background"
    ));
    assert!(said("isn't a time"));
    assert!(said("Background after was set to 5m from the web page."));
    assert!(said(
        "still running after 5 minutes moves to the background"
    ));
    // The calls the runtime moves follow the same time.
    assert_eq!(
        managers.jobs.background_after(),
        Some(Duration::from_secs(300))
    );
    assert!(matches!(
        harness
            .turn(Prompt::User("Go on.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    let body = body_with(&bodies, "Go on.");
    assert!(
        body.contains(
            "From now on, a shell command still running after 5 minutes moves to the background"
        ),
        "{body}"
    );
    assert!(
        body.contains("timeout_seconds now defaults to 1800"),
        "{body}"
    );
    assert!(
        body.contains("Other long calls (MCP tools, index_create) move after the same time"),
        "{body}"
    );
    assert!(harness.live_command("/background-after off", Source::Terminal));
    assert_eq!(harness.background.after(), None);
    assert_eq!(managers.jobs.background_after(), None);
    assert_eq!(harness.session.load_settings().unwrap().background_after, 0);
    managers.shutdown().await;
}

/// A tool whose calls the runtime moves to the background (as an MCP
/// tool's), and which runs until the test releases it.
#[derive(Debug, Default)]
struct SlowLookup {
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl arko_agents::Tool for SlowLookup {
    fn definition(&self) -> arko_agents::ToolDefinition {
        arko_agents::ToolDefinition {
            name: "slow_lookup".into(),
            description: "Looks something up, slowly.".into(),
            output_based_example: None,
            parameters: vec![],
            parallel_safe: false,
            tool_type_details: arko_agents::ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    fn background_mode(&self) -> arko_agents::ToolBackground {
        arko_agents::ToolBackground::Detach
    }

    async fn execute(
        &self,
        _: &arko_agents::TaskContext<'_>,
        _: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<String, arko_agents::ToolError> {
        self.release.notified().await;
        Ok("Found 3 matches.".into())
    }
}

/// A parallel-safe tool, of `mode`, whose calls run until the test
/// releases them.
#[derive(Debug)]
struct Lookup {
    name: &'static str,
    mode: arko_agents::ToolBackground,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl arko_agents::Tool for Lookup {
    fn definition(&self) -> arko_agents::ToolDefinition {
        arko_agents::ToolDefinition {
            name: self.name.into(),
            description: "Looks something up.".into(),
            output_based_example: None,
            parameters: vec![],
            parallel_safe: true,
            tool_type_details: arko_agents::ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    fn background_mode(&self) -> arko_agents::ToolBackground {
        self.mode
    }

    async fn execute(
        &self,
        _: &arko_agents::TaskContext<'_>,
        _: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<String, arko_agents::ToolError> {
        self.release.notified().await;
        Ok(format!("{} found it.", self.name))
    }
}

/// A model turn calling `first` and `second` together.
fn two_calls(id: &str, first: &str, second: &str) -> String {
    let call = |index: usize, suffix: &str, name: &str| {
        serde_json::json!({"type": "response.output_item.done", "output_index": index, "item": {
            "type": "function_call", "id": format!("fc_{id}{suffix}"), "call_id": format!("call_{id}{suffix}"),
            "name": name, "arguments": "{}", "status": "completed",
        }})
    };
    let completed = serde_json::json!({"type": "response.completed", "response": {
        "id": format!("resp_{id}"), "object": "response", "created_at": 0, "status": "completed",
        "model": "gpt-6-luna", "output": [], "usage": {"input_tokens": 10, "output_tokens": 5},
        "metadata": {},
    }});
    [call(0, "a", first), call(1, "b", second), completed]
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// In a parallel group, a call moved alone (the page's Move to background)
/// whose job ends before the group's other call does: its line says the
/// job finished, though its result came after.
#[cfg(unix)]
#[tokio::test]
async fn a_moved_call_of_a_parallel_group_that_ends_first_says_it_finished() {
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        Box::pin(async move {
            if body.contains("call_m1a") {
                complete("m2", "Both done.")
            } else {
                two_calls("m1", "mover", "stayer")
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let lookup = |name, mode| {
        Arc::new(Lookup {
            name,
            mode,
            release: Default::default(),
        })
    };
    let mover = lookup("mover", arko_agents::ToolBackground::Detach);
    let stayer = lookup("stayer", arko_agents::ToolBackground::Never);
    harness
        .main_config
        .tool_config
        .available_tool_names
        .push("stayer".into());
    let managers = equip_with(&mut harness, &mut inputs, mover.clone());
    harness.tools.push(stayer.clone());
    harness.open(false).unwrap();
    let follower =
        crate::jobs::follow(harness.feed.clone(), Arc::downgrade(&managers.jobs), |_| {
            remote::MAIN_AGENT.to_string()
        });
    let (jobs, feed) = (managers.jobs.clone(), harness.feed.clone());
    let (moving, staying) = (mover.clone(), stayer.clone());
    let pressing = tokio::spawn(async move {
        until(|| !crate::jobs::movable(&feed, &jobs).is_empty()).await;
        let call = jobs.foreground_calls()[0].clone();
        assert_eq!(call.tool_name, "mover");
        assert_eq!(
            jobs.move_to_background(call.owner, Some(call.tool_call_id)),
            1
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The moved call ends in its job, before the other one.
        moving.release.notify_one();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        staying.release.notify_one();
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Look both up.".into()), &mut inputs),
    )
    .await
    .unwrap();
    pressing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let line = || {
        harness
            .feed
            .subscribe()
            .0
            .items
            .into_iter()
            .find(|item| item.kind == remote::Kind::Call && item.text.contains("mover"))
            .and_then(|item| item.outcome)
            .unwrap_or_default()
    };
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let outcome = line();
            if !outcome.starts_with("in the background") {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    follower.abort();
    managers.shutdown().await;
    let ended = ended.unwrap_or_else(|_| panic!("the line still reads {:?}", line()));
    assert!(ended.starts_with("finished in the background"), "{ended}");
}

/// Gives the main agent `tool` beside the session's shell and terminal
/// tools.
fn equip_with(
    harness: &mut Harness,
    inputs: &mut Inputs,
    tool: Arc<dyn arko_agents::Tool>,
) -> Managers {
    harness
        .main_config
        .tool_config
        .available_tool_names
        .push(tool.name());
    let managers = equip(harness, inputs, 0);
    harness.tools.push(tool);
    harness.open(false).unwrap();
    managers
}

/// Ctrl+B moves a long call that isn't a shell command (an MCP tool's, say):
/// the agent gets its job's ID at once and finishes its turn, and when the
/// call ends, with the agent idle, its result starts a turn.
#[cfg(unix)]
#[tokio::test]
async fn ctrl_b_moves_another_tools_call_and_its_result_wakes_the_agent() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("It ran your slow_lookup call") {
                    complete("m3", "The lookup found 3 matches.")
                } else if body.contains("call_m1") {
                    complete("m2", "The lookup goes on in the background.")
                } else {
                    tool_call("m1", "slow_lookup", serde_json::json!({}))
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let lookup = Arc::new(SlowLookup::default());
    let managers = equip_with(&mut harness, &mut inputs, lookup.clone());
    let follower =
        crate::jobs::follow(harness.feed.clone(), Arc::downgrade(&managers.jobs), |_| {
            remote::MAIN_AGENT.to_string()
        });
    let background = harness.background.clone();
    let (feed, jobs) = (harness.feed.clone(), managers.jobs.clone());
    let pressing = tokio::spawn(async move {
        until(|| !background.movable().is_empty()).await;
        // A client offers to move the call by its item.
        let movable = crate::jobs::movable(&feed, &jobs);
        assert_eq!(movable.len(), 1);
        sender
            .send(Input::Background(remote::MAIN_AGENT.into()))
            .unwrap();
        (sender, movable[0])
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Look it up.".into()), &mut inputs),
    )
    .await
    .unwrap();
    // (Kept: a closed input quits.)
    let (_sender, item) = pressing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(summary(&harness), "The lookup goes on in the background.");
    let moved = body_with(&bodies, "call_m1");
    assert!(
        moved.contains("The user moved this call to the background."),
        "{moved}"
    );
    let jobs = managers.jobs.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].kind,
        arko_agents::terminal::JobKind::Call {
            tool_name: "slow_lookup".into()
        }
    );

    // The call's line names its job, which the feed follows.
    let job = jobs[0].job_id.to_string();
    until(|| {
        harness
            .feed
            .subscribe()
            .0
            .jobs
            .iter()
            .any(|summary| summary.id == job && summary.call == Some(item))
    })
    .await;
    let call = |feed: &remote::Feed| {
        feed.subscribe()
            .0
            .items
            .into_iter()
            .find(|found| found.id == item)
            .unwrap()
    };
    assert_eq!(call(&harness.feed).job.as_deref(), Some(job.as_str()));
    assert!(
        call(&harness.feed)
            .outcome
            .is_some_and(|outcome| outcome.starts_with("in the background · job ")),
    );

    lookup.release.notify_one();
    let finished = match tokio::time::timeout(Duration::from_secs(10), inputs.next_event(true))
        .await
        .unwrap()
    {
        Event::Finished(finished) => finished,
        _ => panic!("expected a finished job"),
    };
    assert_eq!(finished.job_id, jobs[0].job_id);
    assert!(matches!(
        harness.deliver_finished(finished, &mut inputs).await,
        News::Done
    ));
    assert_eq!(summary(&harness), "The lookup found 3 matches.");
    let woken = body_with(&bodies, "It ran your slow_lookup call");
    assert!(woken.contains("Found 3 matches."), "{woken}");
    // Its line says it finished in the background.
    until(|| {
        call(&harness.feed)
            .outcome
            .is_some_and(|outcome| outcome.starts_with("finished in the background · "))
    })
    .await;
    follower.abort();
    managers.shutdown().await;
}

/// The page's Move to background moves the call its line names, while the
/// turn runs. `/jobs` lists the job and stops it for the user, which wakes
/// no one, and the agent reads that the user stopped it with its next
/// message. The app's requests answer when there's nothing to move or
/// stop.
#[cfg(unix)]
#[tokio::test]
async fn the_page_moves_a_call_and_jobs_lists_and_stops_it() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("Look again.") {
                    complete("m3", "Noted.")
                } else if body.contains("call_m1") {
                    complete("m2", "It goes on in the background.")
                } else {
                    tool_call("m1", "slow_lookup", serde_json::json!({}))
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let lookup = Arc::new(SlowLookup::default());
    let managers = equip_with(&mut harness, &mut inputs, lookup.clone());
    let (feed, jobs, actions) = (
        harness.feed.clone(),
        managers.jobs.clone(),
        harness.remote_actions.clone(),
    );
    let pressing = tokio::spawn(async move {
        until(|| !crate::jobs::movable(&feed, &jobs).is_empty()).await;
        let item = crate::jobs::movable(&feed, &jobs)[0];
        actions
            .send(Action::Background { item: Some(item) })
            .unwrap();
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Look it up.".into()), &mut inputs),
    )
    .await
    .unwrap();
    pressing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert!(body_with(&bodies, "call_m1").contains("The user moved this call to the background."),);
    assert_eq!(managers.jobs.jobs().len(), 1);

    let mut finished = managers.jobs.subscribe_finished();
    let said = |harness: &Harness, text: &str| {
        shown(&harness.feed)
            .iter()
            .any(|(kind, shown, _)| kind == "output" && shown.contains(text))
    };
    assert!(harness.live_command("/jobs", Source::Terminal));
    assert!(
        said(&harness, "1  main  call     slow_lookup · 0 s"),
        "{:?}",
        shown(&harness.feed)
    );
    assert!(said(&harness, "/jobs stop 1 stops it."));
    assert!(harness.live_command("/jobs stop 2", Source::Terminal));
    assert!(said(&harness, "There's no job 2. /jobs lists them."));
    let job = managers.jobs.jobs()[0].job_id;
    assert!(harness.live_command("/jobs stop 1", Source::Web));
    assert!(said(&harness, "Stopped job 1 (slow_lookup)."));
    // The call has its grace, then the job ends.
    until(|| managers.jobs.jobs().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(finished.try_recv().is_err(), "the user's stop wakes no one");
    assert!(harness.live_command("/jobs", Source::Terminal));
    assert!(said(&harness, "No background jobs are running."));

    // Nothing to move or stop now.
    let (reply, answer) = tokio::sync::oneshot::channel();
    harness
        .request(crate::engine::Request::StopJob { job, reply }, false)
        .await;
    assert_eq!(answer.await.unwrap(), Err("That job isn't running.".into()));
    let (reply, answer) = tokio::sync::oneshot::channel();
    harness
        .request(crate::engine::Request::MoveCall { item: 1, reply }, false)
        .await;
    assert_eq!(
        answer.await.unwrap(),
        Err("That call can't move to the background now.".into())
    );

    // The next message carries the note.
    assert!(matches!(
        harness
            .turn(Prompt::User("Look again.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    let next = body_with(&bodies, "Look again.");
    assert!(
        next.contains(&format!(
            "The user stopped job {job} (your slow_lookup call) before it ended"
        )),
        "{next}"
    );
    managers.shutdown().await;
}

/// `/jobs stop N` stops the job the last `/jobs` showed as N, though a job
/// ended since, and says so when that job is the one that ended.
#[tokio::test]
async fn jobs_stop_n_stops_the_job_the_list_showed() {
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let owner = uuid::Uuid::new_v4();
    let mut ids = Vec::new();
    for name in ["first", "second", "third"] {
        let snapshot = managers
            .jobs
            .adopt_call(
                owner,
                uuid::Uuid::new_v4(),
                name,
                name.into(),
                arko_agents::stop::StopSource::new(),
                Box::pin(std::future::pending()),
            )
            .unwrap();
        ids.push(snapshot.job_id);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let said = |harness: &Harness, text: &str| {
        shown(&harness.feed)
            .iter()
            .any(|(kind, shown, _)| kind == "output" && shown.contains(text))
    };
    assert!(harness.live_command("/jobs", Source::Terminal));
    assert!(
        said(&harness, "2  main  call     second"),
        "{:?}",
        shown(&harness.feed)
    );
    // The first ends (its agent stopped it) before the user types.
    managers.jobs.cancel(owner, ids[0]).unwrap();
    until(|| managers.jobs.jobs().len() == 2).await;
    assert!(harness.live_command("/jobs stop 2", Source::Terminal));
    assert!(
        said(&harness, "Stopped job 2 (second)."),
        "{:?}",
        shown(&harness.feed)
    );
    until(|| managers.jobs.jobs().len() == 1).await;
    assert_eq!(managers.jobs.jobs()[0].job_id, ids[2], "the third runs on");
    assert!(harness.live_command("/jobs stop 1", Source::Terminal));
    assert!(said(&harness, "Job 1 (first) already ended."));
    // A new listing numbers them again.
    assert!(harness.live_command("/jobs", Source::Terminal));
    assert!(harness.live_command("/jobs stop 1", Source::Terminal));
    assert!(said(&harness, "Stopped job 1 (third)."));
    managers.shutdown().await;
}

/// `/jobs` and the quit popup name a moved call by its line in the
/// transcript, so two calls of one tool can be told apart.
#[tokio::test]
async fn moved_calls_are_named_by_what_they_do() {
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let owner = uuid::Uuid::new_v4();
    for word in ["mesa", "butte"] {
        let call = uuid::Uuid::new_v4();
        harness.feed.event(
            &arko_workflows::WorkflowEvent {
                timestamp: chrono::Utc::now(),
                workflow_run_id: uuid::Uuid::nil(),
                agent_name: MAIN.into(),
                agent_run_id: owner,
                event: arko_agents::AgentEvent::ToolCallFormatted {
                    tool_call_id: call,
                    tool_name: "mcp__geo__lookup".into(),
                    parameters: [("word".to_string(), serde_json::json!(word))].into(),
                },
            },
            None,
        );
        managers
            .jobs
            .adopt_call(
                owner,
                call,
                "mcp__geo__lookup",
                "mcp__geo__lookup".into(),
                arko_agents::stop::StopSource::new(),
                Box::pin(std::future::pending()),
            )
            .unwrap();
    }
    assert!(harness.live_command("/jobs", Source::Terminal));
    let lines: Vec<String> = shown(&harness.feed)
        .into_iter()
        .filter(|(kind, ..)| kind == "output")
        .map(|(_, text, _)| text)
        .collect();
    for word in ["mesa", "butte"] {
        assert!(
            lines
                .iter()
                .any(|line| line.contains("lookup (geo)") && line.contains(word)),
            "{lines:?}"
        );
    }
    let (popup, mut asked) = mpsc::unbounded_channel();
    harness.quit_popup = Some(popup);
    assert!(!harness.may_exit(Some(Vec::new())));
    let ask = asked.try_recv().unwrap();
    let mut named: Vec<&str> = ask.jobs.iter().map(|job| job.command.as_str()).collect();
    named.sort();
    assert_eq!(named.len(), 2);
    assert!(
        named[0].contains("butte") && named[1].contains("mesa"),
        "{named:?}"
    );
    managers.shutdown().await;
}

/// A job's update that came too late for the agent's last turn is found by
/// the job's ID, whatever its form (a command's terminal update, a moved
/// call's note), and also when no runtime got it.
#[tokio::test]
async fn an_update_that_came_too_late_is_found_by_its_job() {
    let temp = tempfile::tempdir().unwrap();
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let mut task = arko_agents::Task::new(
        harness.main_config.clone(),
        "Look it up.".into(),
        harness.settings.workspace.clone(),
        None,
    );
    let owner = task.id;
    let finished = |job_id| arko_agents::terminal::JobFinished {
        owner,
        job_id,
        state: arko_agents::terminal::JobState::Succeeded,
    };
    let late = uuid::Uuid::new_v4();
    assert!(!waits_with(&task, &finished(late), Some(&managers.jobs)));
    task.pending_context_injections.push(format!(
        "Job {late} ended. It ran your slow_lookup call, which moved to the background 1 min ago. Its result follows.\n\nFound 3 matches."
    ));
    assert!(waits_with(&task, &finished(late), Some(&managers.jobs)));

    // An update the manager keeps for the owner's next runtime.
    let kept = managers
        .jobs
        .adopt_call(
            owner,
            uuid::Uuid::new_v4(),
            "slow_lookup",
            "slow_lookup".into(),
            arko_agents::StopSource::new(),
            Box::pin(async { arko_agents::terminal::CallOutput::text("Found it.") }),
        )
        .unwrap();
    until(|| managers.jobs.has_news(owner)).await;
    task.pending_context_injections.clear();
    assert!(waits_with(
        &task,
        &finished(kept.job_id),
        Some(&managers.jobs)
    ));
    assert!(!waits_with(&task, &finished(kept.job_id), None));
    managers.shutdown().await;
}

/// A monitor's news that waits in the task after the last turn wakes the
/// agent for an alert, and for a warning only while the user wrote lately.
/// Its other news waits for the next turn.
#[tokio::test]
async fn a_late_alert_wakes_and_a_late_warning_wakes_while_the_user_is_here() {
    let temp = tempfile::tempdir().unwrap();
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let (harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    let mut task = arko_agents::Task::new(
        harness.main_config.clone(),
        "Watch it.".into(),
        harness.settings.workspace.clone(),
        None,
    );
    let owner = task.id;
    let id = uuid::Uuid::new_v4();
    let alert = format!(
        "Monitor alert. Job {id} (`dev server`, lines that match `ERROR`) printed 1 matching line since 10:41:05 UTC.\n  ERROR boom\nIt ends in 22 min."
    );
    let warning = format!(
        "Monitor {id} (`dev server`) ends in 5 min, at 11:00 UTC. monitor_control rearm keeps it going for up to 30 min from now. If you no longer need it, let it end."
    );
    let ended = format!(
        "Monitor {id} (`dev server`) ended at 11:00 UTC and its command was stopped. It alerted once."
    );
    for away in [false, true] {
        task.pending_context_injections = vec![ended.clone()];
        assert!(alert_waits(&task, owner, !away).is_none());
        task.pending_context_injections = vec![warning.clone()];
        let woken = alert_waits(&task, owner, !away);
        assert_eq!(woken.is_some(), !away);
        assert!(woken.is_none_or(|alert| alert.ends_soon && alert.late));
        task.pending_context_injections = vec![warning.clone(), alert.clone(), ended.clone()];
        let woken = alert_waits(&task, owner, !away).expect("an alert wakes");
        assert!(!woken.ends_soon && woken.late && woken.owner == owner);
    }
}

/// After Arko quit with jobs of the main agent's running, the user agreeing
/// to stop them, the first message of their conversation in the session
/// that opens again says which stopped, once. A message in another
/// conversation leaves them for theirs.
#[cfg(unix)]
#[tokio::test]
async fn the_first_message_after_a_quit_names_the_jobs_that_stopped() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { complete("m1", "Done.") })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(matches!(
        harness
            .turn(Prompt::User("Start.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    let run = match &harness.conversation {
        Conversation::Answered(result) => result.agent_run_id,
        _ => unreachable!(),
    };
    // This Arko's jobs: a command and a moved call of the main agent's, and
    // a worker's command.
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.background = managers.background.clone();
    let workspace = temp.path().join("work");
    let start = |owner: uuid::Uuid, command: &str| {
        managers
            .jobs
            .start(
                &arko_agents::TaskContext::new(owner, MAIN, "", &workspace),
                command,
                arko_agents::terminal::StartOptions {
                    terminal: false,
                    detect_prompts: false,
                    ..Default::default()
                },
            )
            .unwrap()
            .job_id
    };
    let command = start(run, "sleep 60");
    start(uuid::Uuid::new_v4(), "sleep 61");
    let call = managers
        .jobs
        .adopt_call(
            run,
            uuid::Uuid::new_v4(),
            "index_create",
            "index_create".into(),
            arko_agents::stop::StopSource::new(),
            Box::pin(std::future::pending::<arko_agents::terminal::CallOutput>()),
        )
        .unwrap()
        .job_id;
    let monitor = managers
        .jobs
        .start_monitor(
            &arko_agents::TaskContext::new(run, MAIN, "", &workspace),
            "sleep 62",
            arko_agents::terminal::MonitorOptions::lines(Some("ERROR".into())),
        )
        .await
        .unwrap()
        .job_id;
    // The quit popup names a monitor as one.
    assert_eq!(harness.job_names()[&monitor], "monitor · sleep 62");
    // The user quits, and agrees to stop them.
    assert!(!harness.may_exit(Some(Vec::new())));
    assert_eq!(harness.jobs_shown.len(), 4);
    let agreed = harness.jobs_shown.clone();
    assert!(harness.may_exit(Some(agreed)));
    managers.shutdown().await;
    drop(managers);

    // The session opens again (`arko -c`).
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.background = managers.background.clone();
    harness.new_conversation();
    assert!(matches!(
        harness
            .turn(Prompt::User("Something else.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    assert!(matches!(
        harness
            .turn(Prompt::User("And more.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    for text in ["Something else.", "And more."] {
        assert!(!body_with(&bodies, text).contains("# Background jobs"));
    }
    harness.resume_command("1").await;
    assert!(matches!(
        harness
            .turn(Prompt::User("Go on.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    let body = body_with(&bodies, "Go on.");
    let short = |id: uuid::Uuid| id.to_string()[..8].to_string();
    assert!(
        body.contains("# Background jobs\\n\\nArko was closed, so these background jobs of yours stopped, and they don't start again on their own. Start again any you still need."),
        "{body}"
    );
    assert!(
        body.contains(&format!("- your index_create call (job {}…)", short(call))),
        "{body}"
    );
    assert!(
        body.contains(&format!("- `sleep 60` (job {}…)", short(command))),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "- the monitor of `sleep 62` (lines that match ERROR, job {}…)",
            short(monitor)
        )),
        "{body}"
    );
    assert!(!body.contains("sleep 61"), "{body}");
    assert!(matches!(
        harness
            .turn(Prompt::User("Once more.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    // Once: the next message has only the earlier one's.
    assert_eq!(
        body_with(&bodies, "Once more.")
            .matches("# Background jobs")
            .count(),
        1
    );
    managers.shutdown().await;
}

/// A failed turn whose message named the stopped jobs, and which the
/// conversation doesn't hold, leaves them for the next message.
#[tokio::test]
async fn a_failed_turn_leaves_the_stopped_jobs_for_the_next_message() {
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.interrupted_told = true;
    harness.tell_stopped_jobs_again(false);
    assert!(harness.interrupted_told, "a message that didn't name them");
    harness.tell_stopped_jobs_again(true);
    assert!(!harness.interrupted_told);
}

/// A monitor the agent started alerts while the agent waits for the user:
/// that starts a turn, whose message says why and carries the alert, and
/// the transcript says so. Another alert soon after waits for the minute
/// (here two seconds) since the first.
#[cfg(unix)]
#[tokio::test]
async fn a_monitors_alert_starts_a_turn_while_the_agent_waits_for_the_user() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A monitor of yours alerted") {
                    complete("m3", "The server logged an error.")
                } else if body.contains("call_m1") {
                    complete("m2", "Watching the server.")
                } else {
                    tool_call(
                        "m1",
                        "monitor_start",
                        serde_json::json!({
                            "command": "sleep 1; echo 'ERROR boom'; sleep 1; echo 'ERROR again'; sleep 30",
                            "pattern": "ERROR"
                        }),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(2));
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Start the server and watch it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let started = bodies.lock().unwrap()[1].clone();
    assert!(started.contains("Monitor ") && started.contains(" started."), "{started}");

    let alert = next_alert(&mut inputs, Duration::from_secs(10)).await;
    let first = std::time::Instant::now();
    assert!(!alert.ends_soon);
    assert!(matches!(
        harness.deliver_alert(alert, &mut inputs).await,
        News::Done
    ));
    assert_eq!(summary(&harness), "The server logged an error.");
    let woken = bodies.lock().unwrap()[2].clone();
    assert!(
        woken.contains("A monitor of yours alerted after your last turn ended. The alert follows."),
        "{woken}"
    );
    assert!(woken.contains("Monitor alert.") && woken.contains("ERROR boom"), "{woken}");
    assert!(
        shown(&harness.feed)
            .iter()
            .any(|(kind, text, _)| kind == "notice"
                && text.contains("a monitor alerted, and its alert goes to the agent")),
        "{:?}",
        shown(&harness.feed)
    );

    // The second line came a second after the first: its alert waits.
    let alert = next_alert(&mut inputs, Duration::from_secs(10)).await;
    assert!(first.elapsed() >= Duration::from_millis(1500), "{:?}", first.elapsed());
    harness.deliver_alert(alert, &mut inputs).await;
    let again = bodies.lock().unwrap()[3].clone();
    assert!(again.contains("\\n  ERROR again\\n"), "{again}");
    assert_eq!(again.matches("\\n  ERROR boom\\n").count(), 1, "said once");
    managers.shutdown().await;
}

/// A monitor's alert that reaches the agent while it makes its last request
/// still starts a turn once the agent is idle: the agent's task kept it for
/// the next turn, which reads it once.
#[cfg(unix)]
#[tokio::test]
async fn an_alert_during_the_last_request_still_wakes_the_agent() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A monitor of yours alerted") {
                    complete("m3", "The server logged an error.")
                } else if body.contains("call_m1") {
                    // The line comes while this reply is on its way.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    complete("m2", "Watching the server.")
                } else {
                    tool_call(
                        "m1",
                        "monitor_start",
                        serde_json::json!({
                            "command": "sleep 1; echo 'ERROR boom'; sleep 30",
                            "pattern": "ERROR"
                        }),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(2));
    tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Start the server and watch it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert_eq!(summary(&harness), "Watching the server.");
    let alert = next_alert(&mut inputs, Duration::from_secs(1)).await;
    assert!(alert.late && !alert.ends_soon);
    harness.deliver_alert(alert, &mut inputs).await;
    assert_eq!(summary(&harness), "The server logged an error.");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(
        bodies[2].contains("A monitor of yours alerted") && bodies[2].contains("ERROR boom"),
        "{}",
        bodies[2]
    );
    assert_eq!(bodies[2].matches("\\n  ERROR boom\\n").count(), 1, "read once");
    managers.shutdown().await;
}

/// An alert whose news a turn of the user's already took starts no turn
/// when it's read, although the update of a job the user stopped waits
/// (that wakes no one).
#[cfg(unix)]
#[tokio::test]
async fn an_alert_a_turn_took_starts_none_beside_a_job_the_user_stopped() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A monitor of yours alerted") {
                    complete("m9", "Woken for nothing.")
                } else if body.contains("What is up") {
                    complete("m4", "Saw the error.")
                } else if body.contains("call_m2") {
                    complete("m3", "Watching the server.")
                } else if body.contains("call_m1") {
                    tool_call(
                        "m2",
                        "terminal_start",
                        serde_json::json!({"command": "sleep 60"}),
                    )
                } else {
                    tool_call(
                        "m1",
                        "monitor_start",
                        serde_json::json!({
                            "command": "sleep 2; echo 'ERROR boom'; sleep 60",
                            "pattern": "ERROR"
                        }),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(2));
    tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Start the server and watch it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert_eq!(summary(&harness), "Watching the server.");
    // The alert is announced while the agent is idle, and the user writes
    // before the session reads it.
    tokio::time::timeout(Duration::from_secs(10), async {
        while inputs.alerts.as_ref().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an alert announced");
    harness
        .turn(Prompt::User("What is up?".into()), &mut inputs)
        .await;
    assert!(body_with(&bodies, "What is up").contains("ERROR boom"));
    let command = managers
        .jobs
        .jobs()
        .into_iter()
        .find(|job| matches!(job.kind, arko_agents::terminal::JobKind::Command))
        .unwrap()
        .job_id;
    harness.stop_job(command).unwrap();
    until(|| managers.jobs.has_news(harness_run(&harness))).await;
    let before = bodies.lock().unwrap().len();
    let alert = next_alert(&mut inputs, Duration::from_secs(1)).await;
    assert!(matches!(
        harness.deliver_alert(alert, &mut inputs).await,
        News::Done
    ));
    assert_eq!(bodies.lock().unwrap().len(), before, "no turn started");
    assert_eq!(summary(&harness), "Saw the error.");
    managers.shutdown().await;
}

/// While the user's stopped turn waits for their next message, a monitor's
/// alerts start no turn. The transcript says once that they go with that
/// message, which carries them.
#[cfg(unix)]
#[tokio::test]
async fn alerts_while_the_turn_is_stopped_wait_for_the_next_message() {
    let bodies: Bodies = Arc::default();
    let asked = Arc::new(tokio::sync::Notify::new());
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        let asked = asked.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let asked = asked.clone();
            Box::pin(async move {
                if body.contains("What happened") {
                    complete("m3", "It logs errors.")
                } else if body.contains("call_m1") {
                    asked.notify_one();
                    std::future::pending::<String>().await
                } else {
                    tool_call(
                        "m1",
                        "monitor_start",
                        serde_json::json!({
                            "command": "while true; do echo 'ERROR tick'; sleep 0.3; done",
                            "pattern": "ERROR"
                        }),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(1));
    let interrupt = tokio::spawn(async move {
        asked.notified().await;
        sender.send(Input::Interrupt).unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Watch the server.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = interrupt.await.unwrap();
    assert!(matches!(end, TurnEnd::Stopped));
    let requests = bodies.lock().unwrap().len();
    for _ in 0..2 {
        let alert = next_alert(&mut inputs, Duration::from_secs(10)).await;
        assert!(matches!(
            harness.deliver_alert(alert, &mut inputs).await,
            News::Waiting
        ));
    }
    assert_eq!(bodies.lock().unwrap().len(), requests, "no turn started");
    let hints = shown(&harness.feed)
        .into_iter()
        .filter(|(_, text, _)| text.contains("a monitor alerted, and its alert goes with your next message"))
        .count();
    assert_eq!(hints, 1);
    harness
        .turn(Prompt::User("What happened?".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "It logs errors.");
    assert!(body_with(&bodies, "What happened").contains("ERROR tick"));
    managers.shutdown().await;
}

/// A worker's monitor alerts while no runtime of the worker's is bound:
/// that starts no turn of the main agent's.
#[cfg(unix)]
#[tokio::test]
async fn a_workers_alert_starts_no_turn() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { complete("m1", "Hi.") })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(1));
    harness
        .turn(Prompt::User("Hello.".into()), &mut inputs)
        .await;
    let worker = uuid::Uuid::new_v4();
    let workspace = temp.path().join("work");
    managers
        .jobs
        .start_monitor(
            &arko_agents::TaskContext::new(worker, WORKER, "", &workspace),
            "echo 'ERROR boom'; sleep 60",
            arko_agents::terminal::MonitorOptions::lines(Some("ERROR".into())),
        )
        .await
        .unwrap();
    let alert = next_alert(&mut inputs, Duration::from_secs(10)).await;
    assert_eq!(alert.owner, worker);
    assert!(matches!(
        harness.deliver_alert(alert, &mut inputs).await,
        News::Done
    ));
    assert_eq!(bodies.lock().unwrap().len(), 1, "no turn started");
    managers.shutdown().await;
}

/// The main agent's run in `harness`'s conversation.
fn harness_run(harness: &Harness) -> uuid::Uuid {
    match &harness.conversation {
        Conversation::Answered(result) => result.agent_run_id,
        Conversation::Stopped(run) => run.result.agent_run_id,
        Conversation::New => panic!("no conversation yet"),
    }
}

/// An alert that reaches the agent during its last request wakes it once
/// the turn ends, also when its monitor ended (it expired) before then.
#[cfg(unix)]
#[tokio::test]
async fn an_alert_in_the_last_request_wakes_the_agent_after_its_monitor_ended() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A monitor of yours alerted") {
                    complete("m9", "Read the alert.")
                } else if body.contains("Keep going") {
                    // The line, then the monitor's end, come during this
                    // request.
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    complete("m2", "Still here.")
                } else {
                    complete("m1", "Hi.")
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    quick_monitors(&managers, Duration::from_secs(1));
    harness
        .turn(Prompt::User("Hello.".into()), &mut inputs)
        .await;
    let workspace = temp.path().join("work");
    managers
        .jobs
        .start_monitor(
            &arko_agents::TaskContext::new(harness_run(&harness), MAIN, "", &workspace),
            "sleep 1; echo 'ERROR boom'; sleep 60",
            arko_agents::terminal::MonitorOptions::lines(Some("ERROR".into()))
                .with_expiry(Duration::from_secs(3)),
        )
        .await
        .unwrap();
    harness
        .turn(Prompt::User("Keep going.".into()), &mut inputs)
        .await;
    assert_eq!(summary(&harness), "Still here.");
    assert!(
        managers
            .jobs
            .jobs()
            .iter()
            .all(|job| !matches!(job.kind, arko_agents::terminal::JobKind::Monitor(_))),
        "the monitor ended"
    );
    let alert = next_alert(&mut inputs, Duration::from_secs(1)).await;
    assert!(alert.late && !alert.ends_soon);
    harness.deliver_alert(alert, &mut inputs).await;
    assert_eq!(summary(&harness), "Read the alert.");
    let woken = body_with(&bodies, "A monitor of yours alerted");
    assert_eq!(woken.matches("\\n  ERROR boom\\n").count(), 1, "{woken}");
    managers.shutdown().await;
}

/// A monitor's warning before it ends that would come after the idle
/// agent's prompt cache expires comes just before it instead, and starts a
/// turn that says the monitor is about to end and how long it has left.
#[cfg(unix)]
#[tokio::test]
async fn a_monitors_warning_wakes_the_agent_before_its_cache_expires() {
    let bodies: Bodies = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("A monitor of yours is about to end") {
                    complete("m3", "Kept it.")
                } else if body.contains("call_m1") {
                    complete("m2", "Watching the server.")
                } else {
                    tool_call(
                        "m1",
                        "monitor_start",
                        serde_json::json!({
                            "command": "sleep 60",
                            "pattern": "ERROR",
                            "expires_minutes": 10
                        }),
                    )
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let mut limits = arko_agents::terminal::MonitorLimits::default();
    limits.gather = Duration::from_millis(100);
    limits.start_wait = Duration::ZERO;
    limits.cache_margin = Duration::from_secs(1);
    managers.jobs.set_monitor_limits(limits).unwrap();
    // A cache that lasts 3 seconds after each request.
    harness.stats.lock().unwrap().cache_lifetime = Some(Duration::from_secs(3));
    tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Start the server and watch it.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let ended = std::time::Instant::now();
    let alert = next_alert(&mut inputs, Duration::from_secs(10)).await;
    assert!(alert.ends_soon);
    assert!(ended.elapsed() < Duration::from_secs(5), "{:?}", ended.elapsed());
    harness.deliver_alert(alert, &mut inputs).await;
    assert_eq!(summary(&harness), "Kept it.");
    let woken = bodies.lock().unwrap()[2].clone();
    assert!(
        woken.contains("A monitor of yours is about to end. The notice follows.")
            && woken.contains(") ends in 10 min, at "),
        "{woken}"
    );
    assert!(
        shown(&harness.feed).iter().any(|(kind, text, _)| kind == "notice"
            && text.contains("a monitor is about to end, and the agent is asked whether to keep it")),
        "{:?}",
        shown(&harness.feed)
    );
    managers.shutdown().await;
}

/// `/monitor-wakes` says how long a monitor's warning wakes the agent after
/// your last message, sets it (or always, or off), keeps it in the session,
/// and the monitors follow at once. A change from the web page is said in
/// the terminal too.
#[tokio::test]
async fn the_monitor_wakes_command_sets_and_keeps() {
    let server = ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Noted.") })))
        .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    assert!(harness.live_command("/monitor-wakes", Source::Terminal));
    assert!(harness.live_command("/monitor-wakes soon", Source::Terminal));
    assert!(harness.live_command("/monitor-wakes 30m", Source::Web));
    assert_eq!(
        managers.jobs.warning_wakes_within(),
        Some(Duration::from_secs(1_800))
    );
    assert_eq!(
        harness.session.load_settings().unwrap().monitor_wakes,
        crate::monitors::Wakes(Some(1_800))
    );
    assert!(harness.live_command("/monitor-wakes always", Source::Terminal));
    assert_eq!(managers.jobs.warning_wakes_within(), None);
    assert!(harness.live_command("/monitor-wakes always", Source::Terminal));
    let outputs = shown(&harness.feed);
    let said = |start: &str| {
        outputs
            .iter()
            .any(|(kind, text, _)| (kind == "output" || kind == "notice") && text.contains(start))
    };
    assert!(said(
        "A monitor's warning before it ends wakes the agent for 2 hours after your last message"
    ));
    assert!(said("isn't a time"));
    assert!(said("Monitor wakes was set to 30m from the web page."));
    assert!(said("No change. A monitor's warning before it ends always wakes the agent"));
    managers.shutdown().await;
}

/// `--monitor-wakes` sets how long monitors' warnings wake the agent for a
/// new session, which keeps it, and two hours is the default.
#[test]
fn the_monitor_wakes_flag_sets_a_new_sessions_time() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    let settings = |extra: &[&str]| {
        let mut words = vec!["arko", "-m", "opus", "-C", dir];
        words.extend(extra);
        let args = Args::try_parse_from(words).unwrap();
        new_settings(&args.options().unwrap(), &NoPrompter).unwrap().0
    };
    assert_eq!(settings(&[]).monitor_wakes, crate::monitors::Wakes(Some(7_200)));
    assert_eq!(
        settings(&["--monitor-wakes", "off"]).monitor_wakes,
        crate::monitors::Wakes(Some(0))
    );
    assert_eq!(
        settings(&["--monitor-wakes", "always"]).monitor_wakes,
        crate::monitors::Wakes(None)
    );
    assert!(Args::try_parse_from(["arko", "--monitor-wakes", "soon"]).is_err());
}

/// `/jobs` lists a monitor with what it watches, its alerts and when it
/// ends, and `/jobs stop N` stops it for the user.
#[cfg(unix)]
#[tokio::test]
async fn jobs_lists_and_stops_a_monitor() {
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let managers = equip(&mut harness, &mut inputs, 0);
    let workspace = temp.path().join("work");
    let monitor = managers
        .jobs
        .start_monitor(
            &arko_agents::TaskContext::new(uuid::Uuid::new_v4(), MAIN, "", &workspace),
            "sleep 60",
            arko_agents::terminal::MonitorOptions::lines(Some("ERROR".into())),
        )
        .await
        .unwrap()
        .job_id;
    harness.jobs_command("");
    harness.jobs_command("stop 1");
    let outputs: Vec<String> = shown(&harness.feed)
        .into_iter()
        .filter(|(kind, ..)| kind == "output")
        .map(|(_, text, _)| text)
        .collect();
    assert!(
        outputs.iter().any(|line| line.contains(
            "  monitor  sleep 60 · lines that match ERROR · ends in 30 min"
        )),
        "{outputs:?}"
    );
    assert!(
        outputs.iter().any(|line| line == "Stopped job 1 (sleep 60)."),
        "{outputs:?}"
    );
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = managers
                .jobs
                .jobs()
                .into_iter()
                .find(|job| job.job_id == monitor);
            if status.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(status.is_ok(), "the monitor stopped");
    managers.shutdown().await;
}
