//! Handovers fin asks the main agent for: at the handover size the user
//! sets while the session runs (`/handover-at`), and at once (`/handover`).
//!
//! An agent's configuration holds the size it hands over at by itself, and
//! it is saved with the session: a resumed conversation keeps it. So fin
//! watches the main agent's input (the size each request reports) and asks
//! it for a handover with `UserCommand::RequestHandover` when the user's
//! size is the smaller. The agent then writes its report at its next step
//! and goes on from it in a fresh conversation. An agent in Claude Code
//! compacts instead (see `agents::compacts`): Claude Code's `/compact`
//! summarises the conversation, and the agent goes on from the summary in
//! the same conversation. fin reaches a run's command channel through the
//! tools the run is bound to (`Tool::bind_runtime`): the shell tool keeps
//! them in [`Runs`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arko_agents::UserCommand;
use arko_agents::runtime::event::AgentEvent;
use arko_agents::task::TaskState;
use arko_workflows::prelude::*;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::agents::{self, MAIN};
use crate::remote::{self, Level};
use crate::ui::Output;

/// The smallest handover size `/handover-at` takes: a fresh conversation
/// (the instructions, the tools, the task and the report) starts at about
/// a tenth of it, so a smaller size would hand over again and again.
pub const MIN_TOKENS: usize = 20_000;

/// The command channels of the runs the session's tools are bound to, by
/// run ID.
#[derive(Debug, Default)]
pub struct Runs {
    channels: Mutex<HashMap<Uuid, mpsc::WeakSender<UserCommand>>>,
    /// The run asked for a handover as soon as it is bound: the main
    /// agent's, whose next turn starts with one (`/handover` while no turn
    /// runs). Asked then, before its first request, the report comes
    /// first, with the turn's message in view.
    handover_on_bind: Mutex<Option<Uuid>>,
}

impl Runs {
    /// Keeps run `run`'s channel (a run bound again replaces its old one).
    pub fn bind(&self, run: Uuid, sender: mpsc::WeakSender<UserCommand>) {
        {
            let mut channels = crate::lock(&self.channels);
            channels.retain(|_, sender| sender.strong_count() > 0);
            channels.insert(run, sender);
        }
        let mut due = crate::lock(&self.handover_on_bind);
        if *due == Some(run) && self.send(run, UserCommand::RequestHandover) {
            *due = None;
        }
    }

    /// Sends `command` to run `run`; false when it isn't running.
    pub fn send(&self, run: Uuid, command: UserCommand) -> bool {
        let sender = crate::lock(&self.channels)
            .get(&run)
            .and_then(mpsc::WeakSender::upgrade);
        sender.is_some_and(|sender| sender.try_send(command).is_ok())
    }
}

/// The main agent's runs the user asked for a handover (`/handover`, or
/// `/handover-at` under the agent's size) that hasn't happened yet: the
/// terminal and the feed say whose the handover is when it starts. By run
/// ID, so tests running at once don't meet.
static USER_ASKED: Mutex<Vec<Uuid>> = Mutex::new(Vec::new());

fn user_asked(run: Uuid, asked: bool) {
    let mut runs = crate::lock(&USER_ASKED);
    runs.retain(|other| *other != run);
    if asked {
        runs.push(run);
    }
}

/// What the terminal and the feed say when run `run`'s handover starts,
/// at `input_tokens`.
pub fn requested_text(run: Uuid, input_tokens: usize) -> String {
    let asked = if crate::lock(&USER_ASKED).contains(&run) {
        "you asked, "
    } else {
        ""
    };
    format!(
        "the agent is writing a handover report ({asked}at {}k tokens)",
        input_tokens / 1000
    )
}

/// What the terminal and the feed say when Claude Code compacted the
/// conversation, from `input_tokens` to `compacted_tokens`.
pub fn compacted_text(input_tokens: usize, compacted_tokens: Option<usize>) -> String {
    match compacted_tokens {
        Some(after) => format!(
            "Claude Code compacted the conversation: {} → {} tokens",
            shown(input_tokens),
            shown(after)
        ),
        None => format!(
            "Claude Code compacted the conversation at {} tokens",
            shown(input_tokens)
        ),
    }
}

/// What became of a request for a handover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// The running agent hands over at its next step.
    Now,
    /// It hands over when its next turn starts.
    Next,
    /// A handover is under way already.
    Already,
}

/// The main agent's handover size, and the watch that asks for handovers.
pub struct Handover {
    runs: Arc<Runs>,
    /// The size the user set: the main agent hands over there.
    at: AtomicUsize,
    /// The size the main agent's configuration hands over at by itself:
    /// `at` can't be larger.
    configured: AtomicUsize,
    /// The main agent compacts in Claude Code instead of handing over.
    compacts: AtomicBool,
    watch: Mutex<Watch>,
    /// The task watching the workflow's events.
    watching: Mutex<Option<tokio::task::JoinHandle<()>>>,
    output: Arc<Output>,
    feed: remote::Feed,
}

/// What the watch knows of the main agent's conversation.
#[derive(Debug, Default)]
struct Watch {
    /// The main agent's run, as its latest event says.
    run: Option<Uuid>,
    /// The input of its latest request, in tokens.
    input: Option<usize>,
    /// A handover was asked for (by fin, or by the agent's configuration)
    /// and hasn't happened yet.
    asked: bool,
    /// The conversation started from a handover...
    fresh: bool,
    /// ...and its first request was this large.
    fresh_input: Option<usize>,
    /// A handover to ask for at the run's next event (asked for as a turn
    /// started, before its run was bound).
    pending: bool,
    /// The user was told that the fresh conversation is over the size.
    warned: bool,
}

impl Drop for Handover {
    fn drop(&mut self) {
        if let Some(task) = crate::lock(&self.watching).take() {
            task.abort();
        }
    }
}

impl Handover {
    pub fn new(runs: Arc<Runs>, at: usize, output: Arc<Output>, feed: remote::Feed) -> Self {
        Self {
            runs,
            at: AtomicUsize::new(at),
            configured: AtomicUsize::new(usize::MAX),
            compacts: AtomicBool::new(false),
            watch: Mutex::default(),
            watching: Mutex::default(),
            output,
            feed,
        }
    }

    /// Where the runs' command channels are kept.
    #[cfg(test)]
    pub fn runs(&self) -> Arc<Runs> {
        self.runs.clone()
    }

    /// The size the main agent hands over at.
    pub fn at(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }

    /// The size its configuration hands over at by itself.
    pub fn configured(&self) -> usize {
        self.configured.load(Ordering::SeqCst)
    }

    /// Whether the main agent compacts in Claude Code at the size, instead
    /// of handing over.
    pub fn compacts(&self) -> bool {
        self.compacts.load(Ordering::SeqCst)
    }

    /// The main agent's configuration, as the workflow runs it (the saved
    /// one in a resumed session), hands over at `configured`. The size the
    /// user set stays, up to it.
    fn configure(&self, configured: Option<usize>) {
        let configured = configured.unwrap_or(usize::MAX);
        self.configured.store(configured, Ordering::SeqCst);
        self.at.fetch_min(configured, Ordering::SeqCst);
    }

    /// The input of the main agent's latest request.
    pub fn input(&self) -> Option<usize> {
        crate::lock(&self.watch).input
    }

    /// Sets the size the main agent hands over at, up to the configured
    /// one; returns the size set.
    pub fn set_at(&self, tokens: usize) -> usize {
        let tokens = tokens.min(self.configured());
        self.at.store(tokens, Ordering::SeqCst);
        tokens
    }

    /// Asks the main agent, which is working, for a handover at its next
    /// step.
    pub fn ask_now(&self) -> Asked {
        let mut watch = crate::lock(&self.watch);
        if watch.asked {
            return Asked::Already;
        }
        match watch.run {
            Some(run) if self.runs.send(run, UserCommand::RequestHandover) => {
                watch.asked = true;
                user_asked(run, true);
            }
            // The turn's run isn't bound yet: at its first event.
            _ => watch.pending = true,
        }
        Asked::Now
    }

    /// Asks the main agent, which isn't working, for a handover when its
    /// next turn starts: in run `run`, its conversation's.
    pub fn ask_next(&self, run: Uuid) -> Asked {
        *crate::lock(&self.runs.handover_on_bind) = Some(run);
        user_asked(run, true);
        Asked::Next
    }

    /// Forgets the main agent's conversation, and a handover asked for its
    /// next turn: the next message starts a new one (`/new`).
    pub fn forget(&self) {
        let mut watch = crate::lock(&self.watch);
        if let Some(run) = watch.run {
            user_asked(run, false);
        }
        *watch = Watch::default();
        if let Some(run) = crate::lock(&self.runs.handover_on_bind).take() {
            user_asked(run, false);
        }
    }

    /// Watches the main agent's events in `workflow` (in place of the
    /// workflow watched before), whose main agent has configuration `main`.
    pub fn watch(self: &Arc<Self>, workflow: &Workflow, main: &arko_agents::AgentConfig) {
        self.configure(
            main.handover
                .as_ref()
                .and_then(|handover| handover.at_input_tokens),
        );
        self.compacts
            .store(agents::compacts(main), Ordering::SeqCst);
        let mut events = workflow.subscribe_events();
        let handover = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(handover) = handover.upgrade() else {
                    break;
                };
                handover.event(&event);
            }
        });
        if let Some(old) = crate::lock(&self.watching).replace(task) {
            old.abort();
        }
    }

    fn event(&self, event: &WorkflowEvent) {
        if event.agent_name != MAIN {
            return;
        }
        let mut watch = crate::lock(&self.watch);
        if watch.run != Some(event.agent_run_id) {
            *watch = Watch {
                run: Some(event.agent_run_id),
                pending: watch.pending,
                ..Watch::default()
            };
        }
        match &event.event {
            AgentEvent::ModelCallUsageReported(usage) => {
                watch.input = Some(usage.input_tokens);
                if watch.fresh && watch.fresh_input.is_none() {
                    watch.fresh_input = Some(usage.input_tokens);
                }
            }
            AgentEvent::HandoverRequested { .. } => watch.asked = true,
            AgentEvent::ConversationHandedOver { .. }
            | AgentEvent::ConversationCompacted { .. } => {
                user_asked(event.agent_run_id, false);
                // The old conversation's size is no longer the agent's.
                watch.input = None;
                watch.asked = false;
                watch.fresh = true;
                watch.fresh_input = None;
                watch.warned = false;
            }
            // A turn that ends drops the handover or compaction it hadn't
            // made: fin asks again in the next turn when it is still due.
            AgentEvent::TaskStateChanged(TaskState::Completed { .. }) => {
                user_asked(event.agent_run_id, false);
                watch.asked = false;
            }
            _ => {}
        }
        self.ask_if_due(&mut watch);
    }

    /// Asks the running main agent for a handover when one is pending, or
    /// its input reached the size the user set (below the configured one,
    /// where the agent hands over by itself).
    fn ask_if_due(&self, watch: &mut Watch) {
        let Some(run) = watch.run else {
            return;
        };
        let at = self.at();
        let reached =
            !watch.asked && at < self.configured() && watch.input.is_some_and(|input| input >= at);
        if !watch.pending && !reached {
            return;
        }
        // A conversation that starts over the size would start over it
        // again after another handover.
        if !watch.pending
            && let Some(first) = watch.fresh_input.filter(|first| *first >= at)
        {
            if !std::mem::replace(&mut watch.warned, true) {
                let text = if self.compacts() {
                    format!(
                        "The main agent's compacted conversation started at {}k tokens, over the handover size ({}k), so fin asks for no other compaction in it. /handover-at sets a larger size.",
                        first / 1000,
                        at / 1000
                    )
                } else {
                    format!(
                        "The main agent's fresh conversation started at {}k tokens, over the handover size ({}k), so fin asks for no other handover in it. /handover-at sets a larger size.",
                        first / 1000,
                        at / 1000
                    )
                };
                self.output.line(&self.output.warn(&text));
                self.feed.notice(Level::Warn, &text);
            }
            return;
        }
        if self.runs.send(run, UserCommand::RequestHandover) {
            // A pending handover is one the user asked for.
            if watch.pending {
                user_asked(run, true);
            }
            watch.pending = false;
            watch.asked = true;
        }
    }
}

/// Reads a number of tokens as `/handover-at` takes it: `200000`,
/// `200,000`, `200_000` or `200k`.
pub fn parse_tokens(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let digits: String = text.chars().filter(|c| !matches!(c, ',' | '_')).collect();
    let (digits, scale) = match digits.strip_suffix(['k', 'K']) {
        Some(digits) => (digits, 1000),
        None => (digits.as_str(), 1),
    };
    let tokens = digits
        .parse::<usize>()
        .ok()
        .and_then(|tokens| tokens.checked_mul(scale))
        .ok_or_else(|| {
            format!("`{text}` isn't a number of tokens: /handover-at 200000 (or 200k).")
        })?;
    if tokens < MIN_TOKENS {
        return Err(format!(
            "{tokens} tokens is too small: the main agent's fresh conversation would start near it. Give at least {MIN_TOKENS}."
        ));
    }
    Ok(tokens)
}

/// `tokens` as the user reads it: `250k`.
pub fn shown(tokens: usize) -> String {
    format!("{}k", tokens / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_read_with_separators_or_k() {
        assert_eq!(parse_tokens("200000"), Ok(200_000));
        assert_eq!(parse_tokens(" 200,000 "), Ok(200_000));
        assert_eq!(parse_tokens("200_000"), Ok(200_000));
        assert_eq!(parse_tokens("250k"), Ok(250_000));
        assert!(parse_tokens("lots").unwrap_err().contains("isn't a number"));
        assert!(parse_tokens("").is_err());
        assert!(parse_tokens("-5").is_err());
        assert!(parse_tokens("5k").unwrap_err().contains("too small"));
        assert!(parse_tokens("99999999999999999999999k").is_err());
    }

    fn event(run: Uuid, event: AgentEvent) -> WorkflowEvent {
        WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: run,
            event,
        }
    }

    fn usage(input: usize) -> AgentEvent {
        AgentEvent::ModelCallUsageReported(
            serde_json::from_value(serde_json::json!({
                "call_id": "c", "provider": "openai", "model": "gpt-6-luna",
                "input_tokens": input, "output_tokens": 10,
            }))
            .unwrap(),
        )
    }

    /// The watch asks the running run once its input reaches the size,
    /// once until the handover happens, and not again in a fresh
    /// conversation that starts over the size. A size above the configured
    /// one is left to the configuration.
    #[tokio::test]
    async fn the_watch_asks_once_at_the_size() {
        let runs = Arc::new(Runs::default());
        let handover = Handover::new(
            runs.clone(),
            100_000,
            Arc::new(Output::new(None, true)),
            remote::Feed::new(MAIN),
        );
        handover.configure(Some(250_000));
        let run = Uuid::new_v4();
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(run, sender.downgrade());
        let asked = |commands: &mut mpsc::Receiver<UserCommand>| {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter(|command| matches!(command, UserCommand::RequestHandover))
                .count()
        };
        handover.event(&event(run, usage(90_000)));
        assert_eq!(asked(&mut commands), 0);
        handover.event(&event(run, usage(110_000)));
        handover.event(&event(run, usage(120_000)));
        assert_eq!(asked(&mut commands), 1);
        assert_eq!(handover.ask_now(), Asked::Already);
        handover.event(&event(
            run,
            AgentEvent::ConversationHandedOver {
                report: "R".into(),
                input_tokens: 120_000,
                handovers: 1,
            },
        ));
        // The fresh conversation starts over the size: no other handover.
        handover.event(&event(run, usage(105_000)));
        handover.event(&event(run, usage(130_000)));
        assert_eq!(asked(&mut commands), 0);
        // At the configured size, the configuration hands over itself.
        assert_eq!(handover.set_at(300_000), 250_000);
        handover.event(&event(run, usage(260_000)));
        assert_eq!(asked(&mut commands), 0);
        drop(sender);
    }

    /// A compaction resets the watch as a handover does, and a turn that
    /// ends drops a request it didn't act on: fin asks again later.
    #[tokio::test]
    async fn the_watch_starts_over_after_a_compaction_or_a_turn() {
        let runs = Arc::new(Runs::default());
        let handover = Handover::new(
            runs.clone(),
            100_000,
            Arc::new(Output::new(None, true)),
            remote::Feed::new(MAIN),
        );
        handover.configure(Some(250_000));
        let run = Uuid::new_v4();
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(run, sender.downgrade());
        let mut asked = || {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter(|command| matches!(command, UserCommand::RequestHandover))
                .count()
        };
        handover.event(&event(run, usage(110_000)));
        assert_eq!(asked(), 1);
        handover.event(&event(
            run,
            AgentEvent::TaskStateChanged(TaskState::Completed {
                summary: "Done.".into(),
            }),
        ));
        handover.event(&event(run, usage(120_000)));
        assert_eq!(asked(), 1, "asked again in the next turn");
        handover.event(&event(
            run,
            AgentEvent::ConversationCompacted {
                input_tokens: 120_000,
                compacted_tokens: Some(20_000),
            },
        ));
        assert_eq!(handover.input(), None);
        handover.event(&event(run, usage(25_000)));
        handover.event(&event(run, usage(105_000)));
        assert_eq!(asked(), 1, "the compacted conversation reached the size");
        drop(sender);
    }

    #[test]
    fn a_compaction_is_told_with_its_sizes() {
        assert_eq!(
            compacted_text(642_400, Some(38_100)),
            "Claude Code compacted the conversation: 642k → 38k tokens"
        );
        assert_eq!(
            compacted_text(642_400, None),
            "Claude Code compacted the conversation at 642k tokens"
        );
    }

    /// A handover asked for while the agent doesn't work goes to its run
    /// as the next turn binds it; one asked for as a turn starts, before
    /// its run is bound, at its first event.
    #[tokio::test]
    async fn a_handover_asked_between_turns_goes_to_the_next() {
        let runs = Arc::new(Runs::default());
        let handover = Handover::new(
            runs.clone(),
            250_000,
            Arc::new(Output::new(None, true)),
            remote::Feed::new(MAIN),
        );
        let run = Uuid::new_v4();
        handover.event(&event(run, usage(50_000)));
        assert_eq!(handover.ask_next(run), Asked::Next);
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(Uuid::new_v4(), sender.downgrade());
        assert!(commands.try_recv().is_err(), "another run");
        runs.bind(run, sender.downgrade());
        assert!(matches!(
            commands.try_recv(),
            Ok(UserCommand::RequestHandover)
        ));
        runs.bind(run, sender.downgrade());
        assert!(commands.try_recv().is_err(), "once");
        // That turn ended: its run's channel is closed.
        drop((sender, commands));

        let next = Uuid::new_v4();
        assert_eq!(handover.ask_now(), Asked::Now);
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(next, sender.downgrade());
        handover.event(&event(next, AgentEvent::AssistantResponseChunk("…".into())));
        assert!(matches!(
            commands.try_recv(),
            Ok(UserCommand::RequestHandover)
        ));
        assert_eq!(handover.ask_now(), Asked::Already);
    }
}
