//! The handover sizes of the main agent and of the workers, which the user
//! changes while the session runs (`/handover-at`), and the handovers Arko
//! asks the main agent for at once (`/handover`).
//!
//! An agent's configuration holds the size it hands over at, and it is
//! saved with the session: a resumed conversation keeps it, and its prompt
//! cache with it. So the size the user sets goes to the framework instead,
//! as the main agent's `HandoverThreshold` and the workers' one, which the
//! workflow gives each run of the agent (`WorkflowBuilder::
//! agent_handover_threshold`). The runtime hands over at it in place of the
//! configured size, larger or smaller, from its next check on: a worker
//! running too, and one started, continued or resumed later. Each worker
//! goes by it up to what its own model leaves room for, since workers can
//! be on different models (`HandoverThreshold::with_most`). The session
//! keeps the sizes in its settings (`main_handover_at`,
//! `workers_handover_at`), and a resumed session sets them again. At the size,
//! the agent writes its report and goes on from it in a fresh
//! conversation. An agent in Claude Code compacts instead (see
//! `agents::compacts`): Claude Code's `/compact` summarises the
//! conversation, and the agent goes on from the summary in the same
//! conversation. For `/handover`, Arko asks with
//! `UserCommand::RequestHandover`, through the command channel of the run,
//! which it reaches through the tools the run is bound to
//! (`Tool::bind_runtime`): the shell tool keeps them in [`Runs`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arko_agents::runtime::event::AgentEvent;
use arko_agents::task::TaskState;
use arko_agents::{HandoverThreshold, UserCommand};
use arko_workflows::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::{self, MAIN};
use crate::providers::OpenAiAuth;
use crate::remote::{self, Level};
use crate::ui::Output;

/// The smallest handover size `/handover-at` takes. A fresh conversation
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
/// `/handover-at` under the agent's context) that hasn't happened yet: the
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
            "Claude Code compacted the conversation from {} to {} tokens",
            about(input_tokens),
            about(after)
        ),
        None => format!(
            "Claude Code compacted the conversation at {} tokens",
            about(input_tokens)
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

/// The main agent's handover size and the workers', and the watch that
/// asks for the main agent's handovers.
pub struct Handover {
    runs: Arc<Runs>,
    /// The size the user set, which the main agent's runs hand over at in
    /// place of the configured one. Unset, the configured one applies.
    threshold: HandoverThreshold,
    /// The size the user set for the workers, which every worker run hands
    /// over at in place of its configured one, each up to what its own
    /// model leaves room for (see `agents::max_handover`), the configured
    /// size too. Unset, each goes by its configured size.
    workers: HandoverThreshold,
    /// How the session reaches OpenAI models, which decides what their
    /// models leave room for (on the ChatGPT login, less).
    openai_auth: Arc<Mutex<Option<OpenAiAuth>>>,
    /// The size the main agent's configuration hands over at.
    configured: AtomicUsize,
    /// The largest size the user can set: what the main agent's model
    /// leaves room for (see `agents::max_handover`).
    most: AtomicUsize,
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
    /// A handover was asked for (by Arko, or by the runtime at the size)
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
    /// With `at` and `workers_at`, the sizes the user set for the main
    /// agent and for the workers (kept with the session), if any.
    pub fn new(
        runs: Arc<Runs>,
        at: Option<usize>,
        workers_at: Option<usize>,
        output: Arc<Output>,
        feed: remote::Feed,
    ) -> Self {
        let openai_auth = Arc::new(Mutex::new(None));
        // Each run goes by at most what its model leaves room for.
        let most = |threshold: HandoverThreshold| {
            let auth = openai_auth.clone();
            threshold.with_most(move |config| {
                agents::max_handover(&config.llm_config.model, *crate::lock(&auth))
            })
        };
        Self {
            runs,
            threshold: most(HandoverThreshold::new(at)),
            workers: most(HandoverThreshold::new(workers_at)),
            openai_auth,
            configured: AtomicUsize::new(usize::MAX),
            most: AtomicUsize::new(usize::MAX),
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

    /// The size the main agent hands over at: the one the user set, else
    /// the configured one, up to what its model leaves room for.
    pub fn at(&self) -> usize {
        self.threshold
            .get()
            .unwrap_or_else(|| self.configured.load(Ordering::SeqCst))
            .min(self.most())
    }

    /// The largest size the user can set.
    pub fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    /// The size the user set, as the workflow gives it to the main agent's
    /// runs.
    pub fn threshold(&self) -> HandoverThreshold {
        self.threshold.clone()
    }

    /// The size the user set for the workers, as the workflow gives it to
    /// their runs (each capped by its own model).
    pub fn workers_threshold(&self) -> HandoverThreshold {
        self.workers.clone()
    }

    /// The size the user set for the workers, if any: each worker hands
    /// over at it, or at less where its model leaves less room.
    pub fn workers_at(&self) -> Option<usize> {
        self.workers.get()
    }

    /// Sets the size the workers hand over at, larger or smaller: the
    /// running ones from their next check on, and the ones started,
    /// continued or resumed later. Each goes by it up to what its own
    /// model leaves room for ([`worker_most`](Self::worker_most)).
    pub fn set_workers_at(&self, tokens: usize) {
        self.workers.set(Some(tokens));
    }

    /// The largest size a worker on `model` hands over at, when its limit
    /// is known.
    pub fn worker_most(&self, model: &arko_ai::Model) -> Option<usize> {
        agents::max_handover(model, *crate::lock(&self.openai_auth))
    }

    /// The size a worker on `model` whose configuration hands over at
    /// `configured` goes by: the workers' size the user set, else its
    /// configured one, up to what its model leaves room for.
    pub fn worker_at(&self, model: &arko_ai::Model, configured: Option<usize>) -> Option<usize> {
        let at = self.workers.get().or(configured)?;
        Some(self.worker_most(model).map_or(at, |most| at.min(most)))
    }

    /// Whether the main agent compacts in Claude Code at the size, instead
    /// of handing over.
    pub fn compacts(&self) -> bool {
        self.compacts.load(Ordering::SeqCst)
    }

    /// The main agent's configuration, as the workflow runs it (the saved
    /// one in a resumed session), hands over at `configured`, and its model
    /// leaves room up to `most` (`None` when its limit isn't known: then
    /// up to the configured size). The size the user set stays, up to
    /// that, and so does a configured size over it (a session saved before
    /// sizes were held under the model's room): the runs go by `most`.
    fn configure(&self, configured: Option<usize>, most: Option<usize>) {
        let configured = configured.unwrap_or(usize::MAX);
        let most = most.unwrap_or(configured);
        self.configured.store(configured, Ordering::SeqCst);
        self.most.store(most, Ordering::SeqCst);
        if self.threshold.get().is_some_and(|at| at > most) {
            self.threshold.set(Some(most));
        }
    }

    /// The input of the main agent's latest request.
    pub fn input(&self) -> Option<usize> {
        crate::lock(&self.watch).input
    }

    /// Sets the size the main agent hands over at, up to the largest it
    /// can be. Its runs go by it from their next check on. Returns the size
    /// set.
    pub fn set_at(&self, tokens: usize) -> usize {
        let tokens = tokens.min(self.most());
        self.threshold.set(Some(tokens));
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

    /// The main agent's conversation is now run `run`'s, whose latest
    /// request was `input` tokens (`/resume`), or a new one the next
    /// message starts (`None`, `/new`). A handover asked for the next turn
    /// of the conversation left stays asked of its run, for when it goes on.
    pub fn switch(&self, run: Option<Uuid>, input: Option<usize>) {
        let due = *crate::lock(&self.runs.handover_on_bind);
        let mut watch = crate::lock(&self.watch);
        if let Some(old) = watch.run.filter(|old| Some(*old) != due) {
            user_asked(old, false);
        }
        *watch = Watch {
            run,
            input,
            ..Watch::default()
        };
    }

    /// Watches the main agent's events in `workflow` (in place of the
    /// workflow watched before), whose main agent has configuration `main`,
    /// in a session that reaches OpenAI models by `openai_auth`.
    pub fn watch(
        self: &Arc<Self>,
        workflow: &Workflow,
        main: &arko_agents::AgentConfig,
        openai_auth: Option<OpenAiAuth>,
    ) {
        self.configure_for(main, openai_auth);
        let mut events = workflow.subscribe_events_lossless();
        let handover = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                let Some(event) = events.recv().await else {
                    break;
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

    /// The main agent's conversation runs on `main`, in a session that
    /// reaches OpenAI models by `openai_auth`: its handover size, the most
    /// the user can set, and whether it compacts in Claude Code go by it
    /// from now on (the conversation the session opens in, one it goes back
    /// to, a new one).
    pub fn configure_for(&self, main: &arko_agents::AgentConfig, openai_auth: Option<OpenAiAuth>) {
        *crate::lock(&self.openai_auth) = openai_auth;
        self.configure(
            main.handover
                .as_ref()
                .and_then(|handover| handover.at_input_tokens),
            agents::max_handover(&main.llm_config.model, openai_auth),
        );
        self.compacts
            .store(agents::compacts(main), Ordering::SeqCst);
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
            // made: Arko asks again in the next turn when it is still due.
            AgentEvent::TaskStateChanged(TaskState::Completed { .. }) => {
                user_asked(event.agent_run_id, false);
                watch.asked = false;
            }
            _ => {}
        }
        self.ask_if_due(&mut watch);
    }

    /// Asks the running main agent for the handover the user asked for as
    /// its turn started, and tells the user when a fresh conversation
    /// starts over the handover size. The runtime hands over at the size
    /// by itself.
    fn ask_if_due(&self, watch: &mut Watch) {
        let Some(run) = watch.run else {
            return;
        };
        if watch.pending {
            if self.runs.send(run, UserCommand::RequestHandover) {
                user_asked(run, true);
                watch.pending = false;
                watch.asked = true;
            }
            return;
        }
        // A conversation that starts over the size would start over it
        // again after another handover, so the runtime doesn't hand it over.
        let at = self.at();
        let Some(first) = watch.fresh_input.filter(|first| *first >= at) else {
            return;
        };
        if std::mem::replace(&mut watch.warned, true) {
            return;
        }
        let text = if self.compacts() {
            format!(
                "The main agent's compacted conversation started at {}k tokens, over the handover size ({}k), so Claude Code doesn't compact it again. /handover-at sets a larger size.",
                first / 1000,
                at / 1000
            )
        } else {
            format!(
                "The main agent's fresh conversation started at {}k tokens, over the handover size ({}k), so it doesn't hand over again in it. /handover-at sets a larger size.",
                first / 1000,
                at / 1000
            )
        };
        self.output.line(&self.output.warn(&text));
        self.feed.notice(Level::Warn, &text);
    }
}

/// Whose handover size `/handover-at` says or sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// The main agent's and the workers' (`/handover-at 950k`).
    Both,
    /// The main agent's (`/handover-at main 950k`).
    Main,
    /// The workers' (`/handover-at workers 950k`).
    Workers,
}

/// Reads `/handover-at`'s argument: whose size (`main`, `workers`, else
/// both), and the rest (the size, if any).
pub fn whose(argument: &str) -> (Whose, &str) {
    let argument = argument.trim();
    let (first, rest) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(first, rest)| (first, rest.trim()));
    match first.to_ascii_lowercase().as_str() {
        "main" => (Whose::Main, rest),
        "workers" | "worker" => (Whose::Workers, rest),
        _ => (Whose::Both, argument),
    }
}

/// Reads a handover size, a number of tokens, as `/handover-at` takes it,
/// such as `200000`, `200,000`, `200_000`, `200k`, `950k`, `1.2M` or `1M`
/// (`k` and `M` in either case, with a decimal point before them). The
/// size is kept as given, to the token, and a part of a token is refused.
/// It is at least [`MIN_TOKENS`] (20,000). There is no largest here, as a
/// size that is set is held under what the agent's model leaves room for.
pub fn parse_tokens(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let invalid = || format!("`{text}` isn't a number of tokens, such as 200000, 200k or 1.2M.");
    let (number, places): (&str, usize) = match text.strip_suffix(['k', 'K']) {
        Some(number) => (number, 3),
        None => match text.strip_suffix(['m', 'M']) {
            Some(number) => (number, 6),
            None => (text, 0),
        },
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    // `,` and `_` only between groups of three digits of the whole part.
    let groups: Vec<&str> = whole.split([',', '_']).collect();
    if !groups.iter().all(|group| digits(group)) || !(fraction.is_empty() || digits(fraction)) {
        return Err(invalid());
    }
    if groups.len() > 1 && (groups[0].len() > 3 || groups[1..].iter().any(|group| group.len() != 3))
    {
        return Err(format!(
            "`{text}` isn't a number of tokens. Use , or _ only between groups of three digits, and a point for decimals, as in 1.2M."
        ));
    }
    // In whole tokens: `1.2M` is 1,200,000, `1.2345k` isn't whole.
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > places {
        return Err(format!("`{text}` isn't a whole number of tokens."));
    }
    let number = format!("{}{fraction:0<places$}", groups.concat())
        .parse::<u128>()
        .map_err(|_| invalid())?;
    let tokens = usize::try_from(number).map_err(|_| invalid())?;
    if tokens < MIN_TOKENS {
        return Err(format!(
            "{tokens} tokens is too small, because an agent's fresh conversation would start near it. Give at least {MIN_TOKENS}."
        ));
    }
    Ok(tokens)
}

/// A handover size as the user reads it: `250k` for whole thousands, else
/// all of it (`250,500`), never rounded.
pub fn shown(tokens: usize) -> String {
    if tokens.is_multiple_of(1000) {
        return format!("{}k", tokens / 1000);
    }
    let digits = tokens.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A measured size, such as a request's input, in whole thousands:
/// `642k` for 642,400.
pub fn about(tokens: usize) -> String {
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
        // Millions, and a decimal point before k or M.
        assert_eq!(parse_tokens("1M"), Ok(1_000_000));
        assert_eq!(parse_tokens("1m"), Ok(1_000_000));
        assert_eq!(parse_tokens("1.2M"), Ok(1_200_000));
        assert_eq!(parse_tokens("950k"), Ok(950_000));
        assert_eq!(parse_tokens("1,000,000"), Ok(1_000_000));
        assert_eq!(parse_tokens("250.5k"), Ok(250_500));
        assert!(
            parse_tokens("1.2345k")
                .unwrap_err()
                .contains("whole number")
        );
        assert!(parse_tokens("1.5").unwrap_err().contains("whole number"));
        assert!(
            parse_tokens("1.0000000001M")
                .unwrap_err()
                .contains("whole number")
        );
        assert_eq!(parse_tokens("1.0000000000M"), Ok(1_000_000));
        assert_eq!(parse_tokens("20000.0"), Ok(20_000));
        assert_eq!(parse_tokens("1.M"), Ok(1_000_000));
        // Separators only between groups of three digits.
        for text in ["1,2M", "1_2M", "9,50k", "1,0000k"] {
            assert!(
                parse_tokens(text)
                    .unwrap_err()
                    .contains("a point for decimals"),
                "{text}"
            );
        }
        for text in [",950k", "950k,", "1,,000,000", "1.2,5M", "950 k"] {
            assert!(parse_tokens(text).is_err(), "{text}");
        }
        assert_eq!(parse_tokens("1_000_000"), Ok(1_000_000));
        assert_eq!(parse_tokens("1,234.5k"), Ok(1_234_500));
        assert!(parse_tokens(".5M").is_err());
        assert!(parse_tokens("1.2.3M").is_err());
        assert!(parse_tokens("M").is_err());
        // Shown as it is, never rounded.
        assert_eq!(shown(250_000), "250k");
        assert_eq!(shown(250_500), "250,500");
        assert_eq!(shown(1_234_567), "1,234,567");
    }

    #[test]
    fn whose_size_is_the_first_word() {
        assert_eq!(whose("950k"), (Whose::Both, "950k"));
        assert_eq!(whose(" main 950k "), (Whose::Main, "950k"));
        assert_eq!(whose("workers 950,000"), (Whose::Workers, "950,000"));
        assert_eq!(whose("Worker"), (Whose::Workers, ""));
        assert_eq!(whose(""), (Whose::Both, ""));
    }

    /// Unset, a worker goes by its configured size. Set, every worker goes
    /// by the workers' size, each up to what its own model leaves room for.
    #[test]
    fn each_worker_goes_by_the_workers_size_up_to_its_models_room() {
        let runs = Arc::new(Runs::default());
        let (handover, _) = handover_at(&runs, None);
        let opus = agents::resolve_model("cc:opus").unwrap();
        let luna = agents::resolve_model("luna").unwrap();
        assert_eq!(handover.worker_at(&opus, Some(500_000)), Some(500_000));
        assert_eq!(handover.workers_threshold().get(), None);
        handover.set_workers_at(2_000_000);
        assert_eq!(handover.workers_at(), Some(2_000_000));
        assert_eq!(handover.worker_at(&opus, Some(500_000)), Some(950_000));
        let most = agents::max_handover(&luna, None).unwrap();
        assert!(most < 950_000);
        assert_eq!(handover.worker_at(&luna, Some(200_000)), Some(most));
        handover.set_workers_at(300_000);
        assert_eq!(handover.worker_at(&opus, Some(500_000)), Some(300_000));
        assert_eq!(handover.workers_threshold().get(), Some(300_000));
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

    fn handover_at(runs: &Arc<Runs>, at: Option<usize>) -> (Handover, remote::Feed) {
        let feed = remote::Feed::new(MAIN);
        let handover = Handover::new(
            runs.clone(),
            at,
            None,
            Arc::new(Output::new(None, true)),
            feed.clone(),
        );
        (handover, feed)
    }

    /// The feed's notices.
    fn notices(feed: &remote::Feed) -> Vec<String> {
        feed.subscribe()
            .0
            .items
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .filter(|item| item["kind"] == "notice")
            .map(|item| item["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// The size the user set is the threshold the main agent's runs go by,
    /// larger or smaller than the configured one, up to what the model
    /// leaves room for. Unset (as a session starts without one), the
    /// configured one applies, and a resumed session's size stays.
    #[test]
    fn the_size_set_is_the_runs_threshold_up_to_the_models_room() {
        let runs = Arc::new(Runs::default());
        let (handover, _) = handover_at(&runs, None);
        handover.configure(Some(250_000), Some(950_000));
        assert_eq!(handover.at(), 250_000);
        assert_eq!(handover.threshold().get(), None);
        assert_eq!(handover.set_at(2_000_000), 950_000);
        assert_eq!(handover.set_at(300_000), 300_000);
        assert_eq!(handover.threshold().get(), Some(300_000));
        assert_eq!(handover.set_at(100_000), 100_000);
        assert_eq!(handover.at(), 100_000);

        // A resumed session's size, and a model with less room.
        let (handover, _) = handover_at(&runs, Some(600_000));
        handover.configure(Some(250_000), Some(950_000));
        assert_eq!(handover.at(), 600_000);
        handover.configure(Some(250_000), Some(400_000));
        assert_eq!(handover.threshold().get(), Some(400_000));
        // A model whose limit isn't known: up to the configured size.
        handover.configure(Some(250_000), None);
        assert_eq!(handover.set_at(300_000), 250_000);
        // A session saved at a size over the model's room goes by the room.
        let (handover, _) = handover_at(&runs, None);
        handover.configure(Some(240_000), Some(190_000));
        assert_eq!(handover.at(), 190_000);
        assert_eq!(handover.set_at(1_000_000), 190_000);
    }

    /// The watch leaves the size to the runtime: it asks for no handover
    /// there. It warns once when a fresh conversation starts over the
    /// size, which the runtime then doesn't hand over.
    #[tokio::test]
    async fn the_watch_warns_of_a_fresh_conversation_over_the_size() {
        let runs = Arc::new(Runs::default());
        let (handover, feed) = handover_at(&runs, Some(100_000));
        handover.configure(Some(250_000), Some(950_000));
        let run = Uuid::new_v4();
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(run, sender.downgrade());
        handover.event(&event(run, usage(90_000)));
        handover.event(&event(run, usage(120_000)));
        assert!(commands.try_recv().is_err(), "the runtime hands over");
        handover.event(&event(
            run,
            AgentEvent::HandoverRequested {
                input_tokens: 120_000,
            },
        ));
        assert_eq!(handover.ask_now(), Asked::Already);
        handover.event(&event(
            run,
            AgentEvent::ConversationHandedOver {
                report: "R".into(),
                input_tokens: 120_000,
                handovers: 1,
            },
        ));
        handover.event(&event(run, usage(105_000)));
        handover.event(&event(run, usage(130_000)));
        assert!(commands.try_recv().is_err());
        assert_eq!(
            notices(&feed),
            [
                "The main agent's fresh conversation started at 105k tokens, over the handover size (100k), so it doesn't hand over again in it. /handover-at sets a larger size."
            ]
        );
        drop(sender);
    }

    /// A compaction resets the watch as a handover does, and a turn that
    /// ends drops a request it didn't act on: `/handover` asks again.
    #[tokio::test]
    async fn the_watch_starts_over_after_a_compaction_or_a_turn() {
        let runs = Arc::new(Runs::default());
        let (handover, _) = handover_at(&runs, Some(100_000));
        handover.configure(Some(250_000), Some(950_000));
        let run = Uuid::new_v4();
        let (sender, mut commands) = mpsc::channel(8);
        runs.bind(run, sender.downgrade());
        let mut asked = || {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter(|command| matches!(command, UserCommand::RequestHandover))
                .count()
        };
        handover.event(&event(run, usage(110_000)));
        assert_eq!(handover.ask_now(), Asked::Now);
        assert_eq!(handover.ask_now(), Asked::Already);
        assert_eq!(asked(), 1);
        handover.event(&event(
            run,
            AgentEvent::TaskStateChanged(TaskState::Completed {
                summary: "Done.".into(),
            }),
        ));
        assert_eq!(
            handover.ask_now(),
            Asked::Now,
            "asked again in the next turn"
        );
        assert_eq!(asked(), 1);
        handover.event(&event(
            run,
            AgentEvent::ConversationCompacted {
                input_tokens: 120_000,
                compacted_tokens: Some(20_000),
            },
        ));
        assert_eq!(handover.input(), None);
        handover.event(&event(run, usage(25_000)));
        assert_eq!(handover.input(), Some(25_000));
        drop(sender);
    }

    #[test]
    fn a_compaction_is_told_with_its_sizes() {
        assert_eq!(
            compacted_text(642_400, Some(38_100)),
            "Claude Code compacted the conversation from 642k to 38k tokens"
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
        let (handover, _) = handover_at(&runs, None);
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
