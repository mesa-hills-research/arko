//! The session's background jobs as the feed shows them: the commands and
//! calls that moved to the background, the commands `terminal_start`
//! started, and the monitors `monitor_start` started. [`follow`] keeps the feed's [`JobSummary`](crate::remote::JobSummary)s
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
/// the call that made it and its agent as `agent_of` names it, a monitor
/// says its alerts and its end as they change, and one that ended says
/// how. Runs until the manager goes or it is aborted.
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
        // The monitors' alerts and ends, as the feed has them.
        let mut monitors: HashMap<Uuid, (u32, i64)> = HashMap::new();
        // A job that finished, which may have ended before it was seen run.
        let mut just_ended: Option<terminal::JobFinished> = None;
        loop {
            {
                let Some(jobs) = jobs.upgrade() else {
                    break;
                };
                sync(&feed, &agent_of, &jobs, &mut known, &mut monitors);
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
    monitors: &mut HashMap<Uuid, (u32, i64)>,
) {
    let running = jobs.jobs();
    for job in &running {
        if known.insert(job.job_id, job.owner).is_none() {
            feed.job_started(start(agent_of, job));
        }
        if let terminal::JobKind::Monitor(info) = &job.kind {
            let now = (info.alerts, unix_ms(info.expires_at));
            if monitors.insert(job.job_id, now) != Some(now) {
                feed.job_changed(job.job_id, now.0, now.1);
            }
        }
    }
    monitors.retain(|id, _| running.iter().any(|job| job.job_id == *id));
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
            terminal::JobKind::Monitor(_) => remote::JobKind::Monitor,
            _ => remote::JobKind::Command,
        },
        label: job.label.clone(),
        started_at: started,
        monitor: match &job.kind {
            terminal::JobKind::Monitor(info) => {
                Some((watching(&info.trigger, false), unix_ms(info.expires_at)))
            }
            _ => None,
        },
    }
}

/// What a monitor watches on one line, as `/jobs`, the feed and the
/// resume message show it ("lines that match error|warn", "a check every
/// 15 s"). A pattern shows its line breaks and other control characters as
/// escapes (`\n`), and at most 80 characters, with `…` when more is left
/// out. With `code`, the pattern is in backticks, as the agent reads it.
pub(crate) fn watching(trigger: &terminal::MonitorTrigger, code: bool) -> String {
    let terminal::MonitorTrigger::Lines {
        pattern: Some(pattern),
        ..
    } = trigger
    else {
        return trigger.watching();
    };
    let shown: String = pattern
        .chars()
        .flat_map(|c| {
            let escaped: Vec<char> = if c.is_control() {
                c.escape_default().collect()
            } else {
                vec![c]
            };
            escaped
        })
        .collect();
    let mut cut: String = shown.chars().take(80).collect();
    if cut.len() < shown.len() {
        cut.push('…');
    }
    if code {
        format!("lines that match `{cut}`")
    } else {
        format!("lines that match {cut}")
    }
}

/// A job's label on one line: its first line, at most 80 characters, with
/// `…` when more is left out.
pub(crate) fn label(label: &str) -> String {
    let line = label.lines().next().unwrap_or_default().trim();
    let cut: String = line.chars().take(80).collect();
    if cut.len() < line.len() || label.trim().lines().count() > 1 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// A job as `/jobs` and the quit popup name it: its command, or a moved
/// call as the transcript shows it, on one line.
pub(crate) fn job_name(feed: &remote::Feed, job: &JobInfo) -> String {
    let text = match (&job.kind, job.tool_call_id) {
        (terminal::JobKind::Call { .. }, Some(call)) => feed.call_text(call),
        _ => None,
    };
    label(text.as_deref().unwrap_or(&job.label))
}

/// The names the quit popup shows of the jobs `jobs` runs, by job: as
/// [`job_name`] has them, a monitor's saying it is one ("monitor · npm run
/// dev").
pub(crate) fn popup_names(feed: &remote::Feed, jobs: &TerminalManager) -> HashMap<Uuid, String> {
    jobs.jobs()
        .iter()
        .map(|job| {
            let name = job_name(feed, job);
            let name = match job.kind {
                terminal::JobKind::Monitor(_) => format!("monitor · {name}"),
                _ => name,
            };
            (job.job_id, name)
        })
        .collect()
}

/// `running`, the jobs running in the background, with the names the quit
/// popup shows (see [`popup_names`]).
pub(crate) fn named(
    feed: Option<&remote::Feed>,
    jobs: Option<&TerminalManager>,
    mut running: Vec<terminal::BackgroundJob>,
) -> Vec<terminal::BackgroundJob> {
    if let (Some(feed), Some(jobs)) = (feed, jobs) {
        let names = popup_names(feed, jobs);
        for job in &mut running {
            if let Some(name) = names.get(&job.job_id) {
                job.command = name.clone();
            }
        }
    }
    running
}

/// `at` in Unix milliseconds.
fn unix_ms(at: std::time::SystemTime) -> i64 {
    at.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
    })
}

/// How a job that ended did, for the feed, with a few words of it.
pub(crate) fn ending(job: &terminal::TerminalSnapshot) -> (remote::JobState, Option<String>) {
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
        // A monitor's time was up.
        JobState::TimedOut if job.error.as_deref() == Some("expired") => {
            (remote::JobState::Expired, None)
        }
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

    /// The jobs the quit popup shows are named as /jobs names them, a
    /// monitor's saying it is one, also in the status a front end reads.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_quit_popups_jobs_say_what_they_are() {
        let temp = tempfile::tempdir().unwrap();
        let jobs =
            TerminalManager::new(terminal::TerminalConfig::new(temp.path().join("jobs"))).unwrap();
        let mut limits = terminal::MonitorLimits::default();
        limits.start_wait = Duration::ZERO;
        jobs.set_monitor_limits(limits).unwrap();
        let feed = remote::Feed::new("main");
        let monitor = jobs
            .start_monitor(
                &arko_agents::TaskContext::new(Uuid::new_v4(), "main", "", temp.path()),
                "sleep 60",
                terminal::MonitorOptions::lines(Some("ERROR".into())),
            )
            .await
            .unwrap()
            .job_id;
        let shown = named(Some(&feed), Some(&jobs), jobs.background_jobs());
        assert_eq!(
            shown
                .iter()
                .map(|job| (job.job_id, job.command.as_str()))
                .collect::<Vec<_>>(),
            [(monitor, "monitor · sleep 60")]
        );
        jobs.shutdown().await;
    }

    /// A monitor's pattern shows on one line, with its line breaks as
    /// escapes, cut after 80 characters, and in backticks for the agent.
    #[test]
    fn a_monitors_pattern_shows_on_one_line() {
        let shown = |pattern: &str, code| {
            watching(
                &terminal::MonitorOptions::lines(Some(pattern.into())).trigger,
                code,
            )
        };
        assert_eq!(
            shown("ERROR\n|WARN\t", false),
            "lines that match ERROR\\n|WARN\\t"
        );
        assert_eq!(shown("error|warn", true), "lines that match `error|warn`");
        let long = format!("ERROR|{}", "x".repeat(600));
        assert_eq!(
            shown(&long, true),
            format!("lines that match `ERROR|{}…`", "x".repeat(74))
        );
        assert_eq!(
            watching(&terminal::MonitorOptions::lines(None).trigger, true),
            "every line"
        );
    }

    /// A monitor whose time was up reads as expired, and other jobs that
    /// ran out of time as failed.
    #[test]
    fn an_expired_monitor_reads_as_expired() {
        let snapshot = |error: &str| -> terminal::TerminalSnapshot {
            serde_json::from_value(serde_json::json!({
                "job_id": Uuid::nil(), "revision": 1, "state": "timed_out", "input": null,
                "exit_code": null, "error": error,
                "output": {"start": 0, "end": 0, "available_end": 0, "gap": false, "text": ""},
                "log_path": "/tmp/output.log", "log_truncated": false, "terminal": true,
            }))
            .unwrap()
        };
        assert_eq!(
            ending(&snapshot("expired")),
            (remote::JobState::Expired, None)
        );
        assert_eq!(
            ending(&snapshot("timed out")),
            (remote::JobState::Failed, Some("timed out".into()))
        );
    }

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
