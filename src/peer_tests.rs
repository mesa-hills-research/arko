// End-to-end tests of what linked workers hear of each other, and what a
// worker hears of its jobs, with the mock model. Two workers start in a
// group, and one finishes while the other waits, or Arko stops and the
// session resumes. (Included in `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call, tool_calls};

/// The requests the model server got, as (the agent run that sent it, its
/// body).
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// The agent run that sent a request, such as `r1`, or `main`.
fn sender(body: &str) -> String {
    let marker = "You are agent run ";
    match body.find(marker) {
        Some(at) => body[at + marker.len()..]
            .split(',')
            .next()
            .unwrap_or_default()
            .to_string(),
        None => "main".to_string(),
    }
}

/// How many tool results a request carries.
fn results(body: &str) -> usize {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .count()
}

/// The requests `who` sent.
fn requests(seen: &Seen, who: &str) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(sender, _)| sender == who)
        .map(|(_, body)| body.clone())
        .collect()
}

/// The text of the notes posted for run `run` alone.
fn notes_for(harness: &Harness, run: uuid::Uuid) -> Vec<String> {
    harness
        .workflow()
        .notes()
        .into_iter()
        .filter(|posted| posted.note.run == Some(run))
        .map(|posted| posted.note.text)
        .collect()
}

/// The main agent's first response starts r1 and r2 in group `pair`.
fn start_pair(index: usize) -> String {
    tool_calls(
        &format!("m{index}"),
        &[
            (
                crew::START_AGENT,
                serde_json::json!({"agent": WORKER, "task": "Review the change.", "group": "pair"}),
            ),
            (
                crew::START_AGENT,
                serde_json::json!({"agent": WORKER, "task": "Make the change.", "group": "pair"}),
            ),
        ],
    )
}

async fn turn(harness: &mut Harness, inputs: &mut Inputs, message: &str) {
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User(message.into()), inputs),
    )
    .await
    .expect("the turn ended in time");
    assert!(matches!(end, TurnEnd::Answered));
}

/// r2 waits for mail, r1 finishes, and the note that it did ends r2's wait,
/// with the start of r1's report. When r2 finishes, r1, finished already,
/// gets its note to read when it runs again, and stays finished.
#[tokio::test]
async fn a_waiting_peer_hears_its_peer_finished_and_a_finished_one_stays_finished() {
    let seen: Seen = Arc::default();
    let r2_waits = Arc::new(tokio::sync::Notify::new());
    let server = ModelServer::start(Arc::new({
        let seen = seen.clone();
        move |index, body: String| {
            let who = sender(&body);
            seen.lock().unwrap().push((who.clone(), body.clone()));
            let r2_waits = r2_waits.clone();
            Box::pin(async move {
                let done = results(&body);
                match who.as_str() {
                    "main" if done == 0 => start_pair(index),
                    "main" => complete(&format!("m{index}"), "Started."),
                    // r1 finishes once r2 waits.
                    "r1" => {
                        r2_waits.notified().await;
                        complete(
                            &format!("a{index}"),
                            "Reviewed. It looks right.\nNo findings.",
                        )
                    }
                    "r2" if done == 0 => {
                        r2_waits.notify_one();
                        tool_call(
                            &format!("b{index}"),
                            crew::WAIT_FOR_AGENTS,
                            serde_json::json!({}),
                        )
                    }
                    "r2" => complete(&format!("b{index}"), "Made it."),
                    other => panic!("an unexpected agent {other}"),
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Change it, with a review.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    let runs = harness.workflow().subagents();
    let (r1, r2) = (runs[0].clone(), runs[1].clone());
    until(|| notes_for(&harness, r1.run_id).len() == 1).await;

    // r2's wait ended with the note, which its next request carries.
    let r2_asked = requests(&seen, "r2");
    assert_eq!(r2_asked.len(), 2);
    assert!(
        r2_asked[1].contains(
            "r1 finished, and its report went to your lead. It begins:\\n  \\n  Reviewed. It looks right.\\n  No findings."
        ),
        "{}",
        r2_asked[1]
    );
    assert_eq!(notes_for(&harness, r2.run_id).len(), 1);
    // r1 finished first, so the note of r2's end waits for it, and it
    // doesn't run for it.
    assert_eq!(
        notes_for(&harness, r1.run_id),
        ["r2 finished, and its report went to your lead. It begins:\n\nMade it."]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&seen, "r1").len(), 1);
    let states: Vec<_> = harness
        .workflow()
        .subagents()
        .iter()
        .map(|run| run.state)
        .collect();
    assert_eq!(
        states,
        [
            arko_workflows::subagents::SubagentState::Finished,
            arko_workflows::subagents::SubagentState::Finished
        ]
    );
    // An end's event handled late finds r1's record as it is by then, so it
    // can't tell r2 of an end r1 no longer has.
    let crew = harness.crew.clone().unwrap();
    assert!(crew.worker_event(Ok(arko_workflows::subagents::SubagentEvent {
        timestamp: r1.created,
        workflow_run_id: uuid::Uuid::nil(),
        run_id: r1.run_id,
        handle: "r1".into(),
        agent_name: WORKER.into(),
        caller_run_id: r1.caller_run_id,
        caller_agent: MAIN.into(),
        kind: arko_workflows::subagents::SubagentEventKind::Failed {
            error: "An old error.".into(),
        },
    })));
    assert_eq!(notes_for(&harness, r2.run_id).len(), 1);
    crew.shutdown().await;
}

/// The checkpoint saved r1 running, though its result says it finished, and
/// the note to r2 was lost (as when Arko dies between the two). When the
/// session resumes, r2 runs again and is told, once, also when the workflow
/// opens again.
#[tokio::test]
async fn a_relaunched_peer_hears_of_a_worker_that_ended_unseen_once() {
    let seen: Seen = Arc::default();
    let server = ModelServer::start(Arc::new({
        let seen = seen.clone();
        move |index, body: String| {
            let who = sender(&body);
            seen.lock().unwrap().push((who.clone(), body.clone()));
            let asked = requests(&seen, &who).len();
            Box::pin(async move {
                match who.as_str() {
                    "main" if results(&body) == 0 => start_pair(index),
                    "main" => complete(&format!("m{index}"), "Started."),
                    "r1" => complete(&format!("a{index}"), "Reviewed. It looks right."),
                    // r2 waits a second on its first request after the
                    // session resumes, and stays on the others.
                    "r2" if asked == 2 => tool_call(
                        &format!("b{index}"),
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 1}),
                    ),
                    "r2" => std::future::pending::<String>().await,
                    other => panic!("an unexpected agent {other}"),
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Change it, with a review.").await;
    let crew = harness.crew.clone().unwrap();
    let runs = harness.workflow().subagents();
    let r2 = runs[1].run_id;
    until(|| crew.running() == ["r2"] && notes_for(&harness, r2).len() == 1).await;
    crew.shutdown().await;

    // As if Arko had ended before the board and the note were saved.
    let run_dir = harness.session.run_dir();
    let latest = harness.session.latest_checkpoint().unwrap();
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
    assert_eq!(saved["subagents"]["runs"][0]["state"], "finished");
    saved["subagents"]["runs"][0]["state"] = "running".into();
    saved["notes"] = serde_json::Value::Null;
    std::fs::write(run_dir.join("edited-checkpoint.json"), saved.to_string()).unwrap();
    std::fs::write(run_dir.join("latest-checkpoint"), "edited-checkpoint.json").unwrap();

    harness.open(true).unwrap();
    harness.start_new_workers();
    // It reads the note at its first step or after its wait, whichever
    // comes after the note.
    until(|| requests(&seen, "r2").len() == 3).await;
    let told = "r1 finished, and its report went to your lead. It begins:\\n  \\n  Reviewed. It looks right.";
    let asked = requests(&seen, "r2");
    assert_eq!(asked[2].matches(told).count(), 1, "{}", asked[2]);
    assert_eq!(notes_for(&harness, r2).len(), 1);

    // The workflow opens again, and r2 runs again, told nothing new.
    let crew = harness.crew.clone().unwrap();
    crew.shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    until(|| requests(&seen, "r2").len() == 4).await;
    assert_eq!(notes_for(&harness, r2).len(), 1);
    assert_eq!(requests(&seen, "r2")[3].matches(told).count(), 1);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Both workers had finished when Arko died, before the checkpoint saved
/// r1's end or the notes. When the session resumes, each is told of the
/// other's end, to read when something continues it, and neither runs for it.
#[tokio::test]
async fn an_ended_peer_is_told_of_an_end_it_missed_without_running() {
    let seen: Seen = Arc::default();
    let server = ModelServer::start(Arc::new({
        let seen = seen.clone();
        move |index, body: String| {
            let who = sender(&body);
            seen.lock().unwrap().push((who.clone(), body.clone()));
            Box::pin(async move {
                match who.as_str() {
                    "main" if results(&body) == 0 => start_pair(index),
                    "main" => complete(&format!("m{index}"), "Started."),
                    "r1" => complete(&format!("a{index}"), "Reviewed."),
                    "r2" => complete(&format!("b{index}"), "Made it."),
                    other => panic!("an unexpected agent {other}"),
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Change it, with a review.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    let runs = harness.workflow().subagents();
    let (r1, r2) = (runs[0].run_id, runs[1].run_id);
    until(|| harness.workflow().notes().len() == 2).await;
    harness.crew.as_ref().unwrap().shutdown().await;

    let run_dir = harness.session.run_dir();
    let latest = harness.session.latest_checkpoint().unwrap();
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
    saved["subagents"]["runs"][0]["state"] = "running".into();
    saved["notes"] = serde_json::Value::Null;
    std::fs::write(run_dir.join("edited-checkpoint.json"), saved.to_string()).unwrap();
    std::fs::write(run_dir.join("latest-checkpoint"), "edited-checkpoint.json").unwrap();

    harness.open(true).unwrap();
    harness.start_new_workers();
    assert_eq!(
        notes_for(&harness, r2),
        ["r1 finished, and its report went to your lead. It begins:\n\nReviewed."]
    );
    assert_eq!(
        notes_for(&harness, r1),
        ["r2 finished, and its report went to your lead. It begins:\n\nMade it."]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&seen, "r1").len(), 1);
    assert_eq!(requests(&seen, "r2").len(), 1);
    assert!(harness.crew.as_ref().unwrap().running().is_empty());
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The checkpoint saved r1 running while its result was the end r2 was told
/// of, as when Arko dies just after r1 is continued, before its run starts.
/// The resume settles r1 again, as ended now, so r2 is told of that end a
/// second time. That duplicate is accepted (see `peers::key`).
#[tokio::test]
async fn an_end_the_resume_settles_again_is_told_again() {
    let seen: Seen = Arc::default();
    let server = ModelServer::start(Arc::new({
        let seen = seen.clone();
        move |index, body: String| {
            let who = sender(&body);
            seen.lock().unwrap().push((who.clone(), body.clone()));
            Box::pin(async move {
                match who.as_str() {
                    "main" if results(&body) == 0 => start_pair(index),
                    "main" => complete(&format!("m{index}"), "Started."),
                    "r1" => complete(&format!("a{index}"), "Reviewed."),
                    "r2" => std::future::pending::<String>().await,
                    other => panic!("an unexpected agent {other}"),
                }
            })
        }
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Change it, with a review.").await;
    let crew = harness.crew.clone().unwrap();
    let r2 = harness.workflow().subagents()[1].run_id;
    until(|| crew.running() == ["r2"] && notes_for(&harness, r2).len() == 1).await;
    crew.shutdown().await;

    let run_dir = harness.session.run_dir();
    let latest = harness.session.latest_checkpoint().unwrap();
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
    saved["subagents"]["runs"][0]["state"] = "running".into();
    std::fs::write(run_dir.join("edited-checkpoint.json"), saved.to_string()).unwrap();
    std::fs::write(run_dir.join("latest-checkpoint"), "edited-checkpoint.json").unwrap();

    harness.open(true).unwrap();
    harness.start_new_workers();
    let told = "r1 finished, and its report went to your lead. It begins:\n\nReviewed.";
    assert_eq!(notes_for(&harness, r2), [told, told]);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// What a worker whose monitor ran does, in a session that Arko closes:
/// it starts the monitor, then waits for mail (when `waits`) or stays on
/// its request. Its first request after the session resumes waits a second
/// for mail, and it stays on the others.
async fn monitoring_worker(seen: Seen, waits: bool) -> ModelServer {
    ModelServer::start(Arc::new(move |index, body: String| {
        let who = sender(&body);
        seen.lock().unwrap().push((who.clone(), body.clone()));
        let asked = requests(&seen, &who).len();
        Box::pin(async move {
            let done = results(&body);
            match who.as_str() {
                "main" if done == 0 => tool_call(
                    &format!("m{index}"),
                    crew::START_AGENT,
                    serde_json::json!({"agent": WORKER, "task": "Watch the log."}),
                ),
                "main" => complete(&format!("m{index}"), "Started."),
                "r1" if done == 0 => tool_call(
                    &format!("a{index}"),
                    "monitor_start",
                    serde_json::json!({"command": "sleep 60", "pattern": "ERROR"}),
                ),
                "r1" if asked == 2 && waits => tool_call(
                    &format!("a{index}"),
                    crew::WAIT_FOR_AGENTS,
                    serde_json::json!({}),
                ),
                "r1" if asked == 3 => tool_call(
                    &format!("a{index}"),
                    crew::WAIT_FOR_AGENTS,
                    serde_json::json!({"timeout_seconds": 1}),
                ),
                "r1" => std::future::pending::<String>().await,
                other => panic!("an unexpected agent {other}"),
            }
        })
    }))
    .await
}

/// A session whose worker r1 started a monitor of `sleep 60` and waits,
/// with the shell and terminal tools, and the monitor's job.
async fn monitoring_session(
    temp: &Path,
    server: &ModelServer,
    seen: &Seen,
) -> (Harness, Inputs, mpsc::UnboundedSender<Input>, Managers, uuid::Uuid) {
    let (mut harness, mut inputs, sender) = harness(temp, server.client(), true);
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.background = managers.background.clone();
    harness.open(false).unwrap();
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Watch the log.").await;
    until(|| requests(seen, "r1").len() == 2 && harness.background.jobs().len() == 1).await;
    // (Its second request is under way, or its wait is.)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let monitor = harness.background.jobs()[0].job_id;
    (harness, inputs, sender, managers, monitor)
}

/// The session opens again (`arko -c`) with its jobs as the closed Arko
/// left them. r1 runs again and reads the note of its stopped jobs, which
/// names the monitor, once, also when the workflow opens again.
async fn resume_and_check(harness: &mut Harness, seen: &Seen, monitor: uuid::Uuid) {
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.background = managers.background.clone();
    harness.open(true).unwrap();
    harness.start_new_workers();
    until(|| requests(seen, "r1").len() == 4).await;
    let r1 = harness.workflow().subagents()[0].run_id;
    let told = format!(
        "Arko was closed, so these background jobs of yours stopped, and they don't start again on their own. Start again any you still need.\\n  - the monitor of `sleep 60` (lines that match `ERROR`, job {}…)",
        &monitor.to_string()[..8]
    );
    let asked = requests(seen, "r1");
    assert_eq!(asked[3].matches(&told).count(), 1, "{}", asked[3]);
    assert_eq!(notes_for(harness, r1).len(), 1);

    // Opened again in the same process, it tells r1 nothing new.
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    until(|| requests(seen, "r1").len() == 5).await;
    assert_eq!(notes_for(harness, r1).len(), 1);
    assert_eq!(requests(seen, "r1")[4].matches(&told).count(), 1);
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// Arko closes as it does on /exit, while r1 waits for mail. r1's monitors
/// stop as r1's run does, so r1 is told once it has stopped, before the
/// session's jobs are saved, and its wait doesn't end with the note. The
/// jobs are read at once before the stop, so a monitor started as the close
/// began is named too.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_hears_its_monitors_stopped_when_arko_closed() {
    let seen: Seen = Arc::default();
    let server = monitoring_worker(seen.clone(), true).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender, managers, monitor) =
        monitoring_session(temp.path(), &server, &seen).await;
    let r1 = harness.workflow().subagents()[0].run_id;
    let workspace = harness.settings.workspace.clone();
    managers
        .jobs
        .start_monitor(
            &arko_agents::TaskContext::new(r1, WORKER, "", &workspace),
            "sleep 61",
            arko_agents::terminal::MonitorOptions::lines(None),
        )
        .await
        .unwrap();
    let crew = harness.crew.clone().unwrap();
    crew.shutdown_and(
        || harness.workers_watchers(),
        |workflow, watchers| harness.tell_workers_of_jobs(workflow, watchers),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&seen, "r1").len(), 2, "r1 read nothing before it stopped");
    managers.shutdown().await;
    drop(managers);
    resume_and_check(&mut harness, &seen, monitor).await;
    let told = notes_for(&harness, r1);
    assert!(
        told[0].contains("\n- the monitor of `sleep 61` (every line, job "),
        "{told:?}"
    );
}

/// Arko dies (a VM restart, say) with r1's monitor running, so the session
/// that opens next finds it interrupted and tells r1.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_hears_its_monitor_stopped_when_arko_died() {
    let seen: Seen = Arc::default();
    let server = monitoring_worker(seen.clone(), false).await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender, managers, monitor) =
        monitoring_session(temp.path(), &server, &seen).await;
    // The jobs end with the process while r1 still runs, and nothing tells
    // r1 before.
    managers.shutdown().await;
    drop(managers);
    harness.crew.as_ref().unwrap().shutdown().await;
    resume_and_check(&mut harness, &seen, monitor).await;
}
