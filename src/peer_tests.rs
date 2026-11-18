// End-to-end tests of what linked workers hear of each other, with the mock
// model. Two workers start in a group, and one finishes while the other
// waits, or Arko stops and the session resumes. (Included in `main`'s tests,
// whose helpers they use.)

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
    harness.crew.as_ref().unwrap().shutdown().await;
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
