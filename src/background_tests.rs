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
            && text.contains("a background job finished: its update goes to the agent")),
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
        until(|| !background.running().is_empty()).await;
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
        ["1 background job is running: /exit again stops it and quits."]
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
        "2 background jobs are running: /exit again stops them and quits."
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
    assert!(said("A shell command still running after 10 minutes moves to the background"));
    assert!(said("isn't a time"));
    assert!(said("Background after was set to 5m from the web page."));
    assert!(said("A shell command still running after 5 minutes moves to the background"));
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
    assert!(harness.live_command("/background-after off", Source::Terminal));
    assert_eq!(harness.background.after(), None);
    assert_eq!(harness.session.load_settings().unwrap().background_after, 0);
    managers.shutdown().await;
}
