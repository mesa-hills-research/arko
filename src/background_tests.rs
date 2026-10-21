// End-to-end tests of shell commands that move to the background: after
// the session's time (`/background-after`), or when the user presses
// Ctrl+B, and the finished job waking the idle main agent. Each follows a
// path a user takes, with the mock model. (Included in `main`'s tests,
// whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call};

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
    managers
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
        shown(&harness.feed).iter().any(|(kind, text, _)| kind == "notice"
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
        jobs.iter().map(|job| job.command.as_str()).collect::<Vec<_>>(),
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
    assert!(start("sleep 30").is_ok(), "asking off doesn't ready the jobs");
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
    assert_eq!(
        harness.background.after(),
        Some(Duration::from_secs(300))
    );
    assert_eq!(harness.session.load_settings().unwrap().background_after, 300);
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
    assert!(said("still running after 5 minutes moves to the background"));
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
        body.contains("From now on, a shell command still running after 5 minutes moves to the background"),
        "{body}"
    );
    assert!(body.contains("timeout_seconds now defaults to 1800"), "{body}");
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
    let follower = crate::jobs::follow(
        harness.feed.clone(),
        Arc::downgrade(&managers.jobs),
        |_| remote::MAIN_AGENT.to_string(),
    );
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
    assert!(
        ended.starts_with("finished in the background"),
        "{ended}"
    );
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
    let follower = crate::jobs::follow(
        harness.feed.clone(),
        Arc::downgrade(&managers.jobs),
        |_| remote::MAIN_AGENT.to_string(),
    );
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
        actions.send(Action::Background { item: Some(item) }).unwrap();
    });
    let end = tokio::time::timeout(
        Duration::from_secs(20),
        harness.turn(Prompt::User("Look it up.".into()), &mut inputs),
    )
    .await
    .unwrap();
    pressing.await.unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    assert!(
        body_with(&bodies, "call_m1").contains("The user moved this call to the background."),
    );
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

/// A job's update that came too late for the agent's last turn is found by
/// the job's ID, whatever its form (a command's terminal update, a moved
/// call's note), and also when no runtime got it.
#[tokio::test]
async fn an_update_that_came_too_late_is_found_by_its_job() {
    let temp = tempfile::tempdir().unwrap();
    let server = ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
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
    assert!(waits_with(&task, &finished(kept.job_id), Some(&managers.jobs)));
    assert!(!waits_with(&task, &finished(kept.job_id), None));
    managers.shutdown().await;
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
        harness.turn(Prompt::User("Start.".into()), &mut inputs).await,
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
    // The user quits, and agrees to stop them.
    assert!(!harness.may_exit(Some(Vec::new())));
    assert_eq!(harness.jobs_shown.len(), 3);
    let agreed = harness.jobs_shown.clone();
    assert!(harness.may_exit(Some(agreed)));
    managers.shutdown().await;
    drop(managers);

    // The session opens again (`arko -c`).
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.background = managers.background.clone();
    harness.new_conversation();
    assert!(matches!(
        harness.turn(Prompt::User("Something else.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    assert!(matches!(
        harness.turn(Prompt::User("And more.".into()), &mut inputs).await,
        TurnEnd::Answered
    ));
    for text in ["Something else.", "And more."] {
        assert!(!body_with(&bodies, text).contains("# Background jobs"));
    }
    harness.resume_command("1").await;
    assert!(matches!(
        harness.turn(Prompt::User("Go on.".into()), &mut inputs).await,
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
    assert!(!body.contains("sleep 61"), "{body}");
    assert!(matches!(
        harness.turn(Prompt::User("Once more.".into()), &mut inputs).await,
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
    let server = ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("m1", "Done.") }))).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.interrupted_told = true;
    harness.tell_stopped_jobs_again(false);
    assert!(harness.interrupted_told, "a message that didn't name them");
    harness.tell_stopped_jobs_again(true);
    assert!(!harness.interrupted_told);
}
