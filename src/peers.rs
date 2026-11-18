//! What the workers linked with a worker (its peers, which it may message)
//! hear when it finishes, fails or is stopped, so that one waiting for it
//! goes on. Each peer gets a note for its run alone. A running peer reads it
//! at its next step, and its `wait_for_agents` returns at once. A peer that
//! isn't running reads it when it runs again, since a note wakes no one. A
//! note also leaves the count of the messages peers send each other as it is.

use arko_workflows::Workflow;
use arko_workflows::notes::{Note, NoteSource};
use arko_workflows::subagents::{SubagentEventKind, SubagentRecord, SubagentState};
use sha2::Digest;
use uuid::Uuid;

/// At most this many lines of a report go in a note, and this many characters.
const REPORT_LINES: usize = 6;
const REPORT_CHARS: usize = 600;

/// The runs among `runs` that `ended` may message, and that may message it,
/// as the framework links the runs of one lead (named in `peers`, in its
/// group, or `all`). A run whose workspace was removed can't run again, so
/// it isn't one.
pub(crate) fn peers_of<'a>(
    runs: &'a [SubagentRecord],
    ended: &SubagentRecord,
) -> Vec<&'a SubagentRecord> {
    runs.iter()
        .filter(|run| {
            run.run_id != ended.run_id
                && run.caller_run_id == ended.caller_run_id
                && run.state != SubagentState::Closed
                && (ended.all_peers
                    || run.all_peers
                    || ended.peers.contains(&run.run_id)
                    || run.peers.contains(&ended.run_id))
        })
        .collect()
}

/// What the peers of worker `handle` are told when it ended as `kind`, or
/// `None` for an event that isn't an end.
pub(crate) fn notice(handle: &str, kind: &SubagentEventKind) -> Option<String> {
    let why = |text: &str| match crate::ui::cut(text, 300) {
        cut if cut.is_empty() => String::new(),
        cut => format!(" ({cut})"),
    };
    Some(match kind {
        SubagentEventKind::Finished { summary } => format!(
            "{handle} finished, and its report went to your lead. It begins:\n\n{}",
            beginning(summary)
        ),
        SubagentEventKind::Failed { error } => format!(
            "{handle} stopped with an error{}, so don't wait for it. Your lead can continue it.",
            why(error)
        ),
        SubagentEventKind::Stopped { reason, .. } => format!(
            "{handle} was stopped{}, so don't wait for it. Your lead can restart it.",
            why(reason)
        ),
        _ => return None,
    })
}

/// The first lines of `report`, as many as [`REPORT_LINES`] and
/// [`REPORT_CHARS`] allow, with `…` when more is left out.
fn beginning(report: &str) -> String {
    let lines: Vec<&str> = report.trim().lines().collect();
    let text = lines[..lines.len().min(REPORT_LINES)].join("\n");
    let shown: String = text.chars().take(REPORT_CHARS).collect();
    if lines.len() > REPORT_LINES || shown.len() < text.len() {
        format!("{}…", shown.trim_end())
    } else {
        shown
    }
}

/// How `record` ended, as the event of its end says it, while it has.
fn ending(record: &SubagentRecord) -> Option<SubagentEventKind> {
    let said = |text: &Option<String>| text.clone().unwrap_or_default();
    match record.state {
        SubagentState::Finished => Some(SubagentEventKind::Finished {
            summary: said(&record.report),
        }),
        SubagentState::Failed => Some(SubagentEventKind::Failed {
            error: said(&record.error),
        }),
        SubagentState::Stopped => Some(SubagentEventKind::Stopped {
            reason: said(&record.error),
            by_caller: false,
        }),
        _ => None,
    }
}

/// The note's key for `peer` about the end `ended` records, made of the
/// run, how it ended and when (its record's `since`, which a checkpoint keeps
/// as it is). Its event and the check after a resume ([`tell_of_ended`]) make
/// the same key, so the note is posted once, and a later end, even one just
/// like it, makes another.
// shortcut: a record the checkpoint saved running whose result says it ended
// is settled again as the workflow is built, with a fresh `since`. When that
// result is an end its peers were told of (Arko died just after the run was
// continued), they're told again. Telling ends apart by their words would
// lose a new end that reads like an old one. An end generation that
// arko_workflows keeps would make the key exact.
fn key(ended: &SubagentRecord, peer: Uuid) -> Uuid {
    let since = ended
        .since
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let digest = sha2::Sha256::digest(format!(
        "peer notice {} {:?} {since} {peer}",
        ended.run_id, ended.state
    ));
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// Tells the peers of `ended` among `runs` how it ended, as its record says,
/// and nothing while it hasn't. An end's event that is handled late finds
/// the record as it is by then, so it tells of the latest end, if any, and
/// that end's own event makes the same key. A peer that isn't running reads
/// the note when it runs again. It posts notes, which writes the workflow's
/// event log, so a caller on the runtime calls it through
/// [`crate::blocking`].
pub(crate) fn tell(workflow: &Workflow, runs: &[SubagentRecord], ended: &SubagentRecord) {
    let Some(text) = ending(ended).and_then(|kind| notice(&ended.handle, &kind)) else {
        return;
    };
    for peer in peers_of(runs, ended) {
        let note = Note::new(text.clone())
            .source(NoteSource::Host)
            .to([peer.agent.clone()])
            .for_run(peer.run_id);
        if let Err(error) = workflow.post_note_once(key(ended, peer.run_id), note) {
            log::warn!(
                "could not tell {} that {} ended: {error}",
                peer.handle,
                ended.handle
            );
        }
    }
}

/// After a resume, tells the peers of each worker that ended what they
/// weren't told, such as of one that the checkpoint saved running and whose
/// result says it ended, or of one whose note was lost when Arko stopped. A
/// peer told before isn't told again. Called after the workers are
/// relaunched, so a worker relaunched with them isn't counted as ended.
pub(crate) fn tell_of_ended(workflow: &Workflow) {
    let runs = workflow.subagents();
    for ended in &runs {
        tell(workflow, &runs, ended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(handle: &str, lead: Uuid) -> SubagentRecord {
        let now = chrono::Utc::now();
        SubagentRecord {
            run_id: Uuid::new_v4(),
            handle: handle.to_string(),
            agent: "worker".into(),
            caller_run_id: lead,
            caller_agent: "main".into(),
            depth: 1,
            task: String::new(),
            workspace: Default::default(),
            isolated: None,
            parameters: Default::default(),
            peers: Vec::new(),
            all_peers: false,
            group: None,
            state: SubagentState::Running,
            created: now,
            since: now,
            report: None,
            error: None,
            pending: None,
            stopped_by_caller: None,
            stop_requested: None,
            asking: false,
            detached: false,
        }
    }

    /// A worker's peers are the runs of its lead it is linked with, either
    /// way round, or all of them with `all`, and never itself, another
    /// lead's run or a closed one.
    #[test]
    fn the_peers_are_the_runs_linked_with_it() {
        let lead = Uuid::new_v4();
        let mut ended = record("r1", lead);
        let mut named = record("r2", lead);
        let mut naming = record("r3", lead);
        let unlinked = record("r4", lead);
        let mut other_lead = record("r5", Uuid::new_v4());
        let mut closed = record("r6", lead);
        ended.peers = vec![named.run_id, other_lead.run_id, closed.run_id];
        naming.peers = vec![ended.run_id];
        other_lead.peers = vec![ended.run_id];
        closed.state = SubagentState::Closed;
        named.state = SubagentState::Finished;
        let runs = vec![
            ended.clone(),
            named.clone(),
            naming.clone(),
            unlinked.clone(),
            other_lead,
            closed,
        ];
        let handles = |runs: &[SubagentRecord], ended: &SubagentRecord| -> Vec<String> {
            peers_of(runs, ended)
                .iter()
                .map(|run| run.handle.clone())
                .collect()
        };
        assert_eq!(handles(&runs, &ended), ["r2", "r3"]);
        // `all` links it with every run of its lead, either way round.
        let mut everyone = runs.clone();
        everyone[0].all_peers = true;
        assert_eq!(handles(&everyone, &everyone[0]), ["r2", "r3", "r4"]);
        let mut joined = runs;
        joined[3].all_peers = true;
        assert_eq!(handles(&joined, &joined[0]), ["r2", "r3", "r4"]);
    }

    /// A finished worker's peers read the start of its report, and a failed
    /// or stopped one's why it ended and that they shouldn't wait.
    #[test]
    fn the_notice_says_how_it_ended() {
        let report = (1..=10)
            .map(|line| format!("Line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            notice("r2", &SubagentEventKind::Finished { summary: report }).unwrap(),
            "r2 finished, and its report went to your lead. It begins:\n\nLine 1.\nLine 2.\nLine 3.\nLine 4.\nLine 5.\nLine 6.…"
        );
        let short = SubagentEventKind::Finished {
            summary: " All good. ".into(),
        };
        assert!(
            notice("r2", &short)
                .unwrap()
                .ends_with("It begins:\n\nAll good.")
        );
        let long = SubagentEventKind::Finished {
            summary: "é".repeat(700),
        };
        let told = notice("r2", &long).unwrap();
        assert!(
            told.ends_with(&format!("{}…", "é".repeat(REPORT_CHARS))),
            "{told}"
        );
        assert_eq!(
            notice(
                "r2",
                &SubagentEventKind::Failed {
                    error: "Usage limit reached (resets in about 2 h).\nMore.".into()
                }
            )
            .unwrap(),
            "r2 stopped with an error (Usage limit reached (resets in about 2 h). More.), so don't wait for it. Your lead can continue it."
        );
        assert_eq!(
            notice(
                "r2",
                &SubagentEventKind::Stopped {
                    reason: String::new(),
                    by_caller: true
                }
            )
            .unwrap(),
            "r2 was stopped, so don't wait for it. Your lead can restart it."
        );
        assert_eq!(
            notice("r2", &SubagentEventKind::Started),
            None,
            "a start is no end"
        );
    }

    /// One end of a run gives each peer one key, also after a checkpoint
    /// kept its record, and another end or another peer another.
    #[test]
    fn one_end_gives_each_peer_one_key() {
        let (peer, other) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ended = record("r1", Uuid::new_v4());
        ended.state = SubagentState::Failed;
        ended.error = Some("Usage limit reached.".into());
        let saved: SubagentRecord =
            serde_json::from_str(&serde_json::to_string(&ended).unwrap()).unwrap();
        assert_eq!(key(&ended, peer), key(&saved, peer));
        assert_ne!(key(&ended, peer), key(&ended, other));
        // Continued into the same limit, it fails again just the same.
        let mut again = ended.clone();
        again.since += chrono::Duration::milliseconds(1);
        assert_ne!(key(&ended, peer), key(&again, peer));
        let mut finished = ended.clone();
        finished.state = SubagentState::Finished;
        assert_ne!(key(&ended, peer), key(&finished, peer));
        // A record tells how it ended while it has.
        assert_eq!(ending(&record("r1", Uuid::new_v4())), None, "it runs");
        finished.report = Some("Done.".into());
        assert_eq!(
            ending(&finished),
            Some(SubagentEventKind::Finished {
                summary: "Done.".into()
            })
        );
    }
}
