//! The session's background jobs as the feed shows them: the commands and
//! calls that moved to the background, and the commands `terminal_start`
//! started. [`follow`] keeps the feed's [`JobSummary`](crate::remote::JobSummary)s
//! in step with the terminal manager, which runs them.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use arko_agents::terminal::{self, JobInfo, TerminalManager};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::remote::{self, JobStart, Update};
use crate::status::Gauges;

/// How often the jobs are checked while one runs: a job ends without an
/// event when the user or the end of a worker's run stops it.
const WHILE_RUNNING: Duration = Duration::from_secs(1);
/// How often they are checked while none runs.
const WHILE_IDLE: Duration = Duration::from_secs(5);

/// Keeps `feed`'s jobs in step with `jobs`, the session's terminal
/// manager: a job that started or moved to the background is added, with
/// the call that made it and its agent as `agent_of` names it, and one that
/// ended says how. Runs until the manager goes or it is aborted.
pub(crate) fn follow(
    feed: remote::Feed,
    jobs: Weak<TerminalManager>,
    agent_of: impl Fn(Uuid) -> String + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    // Taken now, so what happens before the task first runs is seen.
    let (_, mut updates) = feed.subscribe();
    let mut finished = jobs.upgrade().map(|jobs| jobs.subscribe_finished());
    tokio::spawn(async move {
        // The jobs running, as the feed has them, with their owners.
        let mut known: HashMap<Uuid, Uuid> = HashMap::new();
        // A job that finished, which may have ended before it was seen run.
        let mut just_ended: Option<terminal::JobFinished> = None;
        loop {
            {
                let Some(jobs) = jobs.upgrade() else {
                    break;
                };
                sync(&feed, &agent_of, &jobs, &mut known);
                if let Some(job) = just_ended.take()
                    && !known.contains_key(&job.job_id)
                {
                    // The feed ends it, or knows it ended, so the line of a
                    // call that moved there says so.
                    let (state, outcome) = match jobs.status(job.owner, job.job_id, None) {
                        Ok(snapshot) => ending(&snapshot),
                        Err(_) => (remote::JobState::Finished, None),
                    };
                    feed.job_ended(job.job_id, state, outcome);
                }
            }
            let wait = if known.is_empty() {
                WHILE_IDLE
            } else {
                WHILE_RUNNING
            };
            let ended = async {
                match finished.as_mut() {
                    Some(finished) => finished.recv().await.ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                job = ended => just_ended = job,
                () = tokio::time::sleep(wait) => {}
                closed = calls_changed(&mut updates) => if closed {
                    break;
                },
            }
        }
    })
}

/// Waits for a change of a call (its result may say it moved), and says
/// whether the feed closed.
async fn calls_changed(updates: &mut broadcast::Receiver<Update>) -> bool {
    loop {
        match updates.recv().await {
            Ok(Update::Change(item)) if item.kind == remote::Kind::Call => return false,
            Ok(_) => {}
            // Some went by unseen: one of them may have been a call's.
            Err(broadcast::error::RecvError::Lagged(_)) => return false,
            Err(broadcast::error::RecvError::Closed) => return true,
        }
    }
}

/// Adds the jobs `jobs` runs that the feed doesn't have, and ends those it
/// no longer runs.
fn sync(
    feed: &remote::Feed,
    agent_of: &dyn Fn(Uuid) -> String,
    jobs: &TerminalManager,
    known: &mut HashMap<Uuid, Uuid>,
) {
    let running = jobs.jobs();
    for job in &running {
        if known.insert(job.job_id, job.owner).is_none() {
            feed.job_started(start(agent_of, job));
        }
    }
    let ended: Vec<(Uuid, Uuid)> = known
        .iter()
        .filter(|(id, _)| !running.iter().any(|job| job.job_id == **id))
        .map(|(id, owner)| (*id, *owner))
        .collect();
    for (id, owner) in ended {
        known.remove(&id);
        let (state, outcome) = match jobs.status(owner, id, None) {
            Ok(snapshot) => ending(&snapshot),
            // Forgotten as it ended.
            Err(_) => (remote::JobState::Finished, None),
        };
        feed.job_ended(id, state, outcome);
    }
}

/// `job` as the feed takes it.
fn start(agent_of: &dyn Fn(Uuid) -> String, job: &JobInfo) -> JobStart {
    let started = chrono::Utc::now().timestamp_millis()
        - i64::try_from(job.started.elapsed().as_millis()).unwrap_or(0);
    JobStart {
        id: job.job_id,
        agent: agent_of(job.owner),
        tool_call: job.tool_call_id,
        kind: match job.kind {
            terminal::JobKind::Call { .. } => remote::JobKind::Call,
            _ => remote::JobKind::Command,
        },
        label: job.label.clone(),
        started_at: started,
    }
}

/// How a job that ended did, for the feed, with a few words of it.
fn ending(job: &terminal::TerminalSnapshot) -> (remote::JobState, Option<String>) {
    use terminal::JobState;
    match job.state {
        JobState::Succeeded => (remote::JobState::Finished, None),
        JobState::Failed => (
            remote::JobState::Failed,
            match (job.exit_code, &job.error) {
                (Some(code), _) => Some(format!("exit {code}")),
                (None, Some(error)) => Some(error.clone()),
                (None, None) => None,
            },
        ),
        JobState::TimedOut => (remote::JobState::Failed, Some("timed out".into())),
        JobState::Cancelled => (remote::JobState::Stopped, job.error.clone()),
        JobState::Interrupted => (remote::JobState::Interrupted, None),
        JobState::Running => (remote::JobState::Running, None),
    }
}

/// The calls of `jobs` that can move to the background now, as `feed`'s
/// items, for a client that offers to move them.
pub(crate) fn movable(feed: &remote::Feed, jobs: &TerminalManager) -> Vec<u64> {
    jobs.foreground_calls()
        .into_iter()
        .filter_map(|call| feed.running_call_item(call.tool_call_id))
        .collect()
}

/// The owner and ID of the call `item` (a feed item), while it runs and can
/// move to the background.
pub(crate) fn movable_call(
    feed: &remote::Feed,
    jobs: &TerminalManager,
    item: u64,
) -> Option<(Uuid, Uuid)> {
    let call = feed.running_call(item)?;
    jobs.foreground_calls()
        .into_iter()
        .find(|running| running.tool_call_id == call)
        .map(|running| (running.owner, running.tool_call_id))
}

/// [`follow`] for the session `gauges` show: its agents are named as the
/// status names them.
pub(crate) fn follower(gauges: &Arc<Gauges>, feed: &remote::Feed) -> tokio::task::JoinHandle<()> {
    let jobs = gauges
        .background
        .manager()
        .map_or_else(Weak::new, |jobs| Arc::downgrade(&jobs));
    let gauges = Arc::downgrade(gauges);
    follow(feed.clone(), jobs, move |run| {
        gauges.upgrade().map_or_else(
            || remote::MAIN_AGENT.to_string(),
            |gauges| gauges.agent_of(run),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_agents::AgentEvent;

    fn event(event: AgentEvent) -> arko_workflows::WorkflowEvent {
        arko_workflows::WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: "main".into(),
            agent_run_id: Uuid::nil(),
            event,
        }
    }

    /// A moved call whose job ends before the follower sees it run still
    /// has its line say the job finished.
    #[tokio::test]
    async fn a_job_that_ends_before_it_is_seen_ends_its_calls_line() {
        let temp = tempfile::tempdir().unwrap();
        let jobs =
            TerminalManager::new(terminal::TerminalConfig::new(temp.path().join("jobs"))).unwrap();
        let feed = remote::Feed::new("main");
        let (owner, call) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(
            &event(AgentEvent::ToolCallFormatted {
                tool_call_id: call,
                tool_name: "lookup".into(),
                parameters: Default::default(),
            }),
            None,
        );
        // Its job runs first, and ends at once.
        let job = jobs
            .adopt_call(
                owner,
                call,
                "lookup",
                "lookup".into(),
                arko_agents::stop::StopSource::new(),
                Box::pin(async { terminal::CallOutput::text("found") }),
            )
            .unwrap()
            .job_id;
        let follower = follow(feed.clone(), Arc::downgrade(&jobs), |_| {
            remote::MAIN_AGENT.to_string()
        });
        feed.event(
            &event(AgentEvent::ToolResult {
                tool_call_id: call,
                tool_name: "lookup".into(),
                output: format!(
                    "{}{job}, 0.2 s\nThe user moved this call to the background.",
                    terminal::IN_THE_BACKGROUND
                ),
            }),
            None,
        );
        let line = || {
            feed.subscribe()
                .0
                .items
                .into_iter()
                .find(|item| item.kind == remote::Kind::Call)
                .and_then(|item| item.outcome)
                .unwrap_or_default()
        };
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let outcome = line();
                if !outcome.starts_with("in the background") {
                    return outcome;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        follower.abort();
        let ended = ended.unwrap_or_else(|_| panic!("the line still reads {:?}", line()));
        assert!(ended.starts_with("finished in the background"), "{ended}");
    }
}
