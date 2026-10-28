//! The session settings for who chooses the workers' models, how deep
//! workers go, how many failed requests stop a worker and whether the agent
//! that started a worker gets a copy of the user's messages to it, with the
//! words and commands every front end uses for them (`/worker-models`,
//! `/worker-depth`, `/worker-retry-cap`, `/message-copies`, and the flags
//! `--worker-models`, `--worker-depth`, `--worker-retry-cap`,
//! `--message-copies`). The
//! terminal's settings, the web page and an app's settings page name and set
//! them alike.

pub use crate::agents::ModelChoice;
/// Whether workers may run on models billed per token (`/worker-billing`,
/// `--worker-billing`, `ARKO_WORKER_BILLING`), and the daily API budget
/// (`/api-budget`, `--api-budget`, `ARKO_API_BUDGET`), with their names,
/// commands and sentences.
pub use crate::billing::{
    ABOUT as BILLING_ABOUT, BUDGET_COMMAND, BUDGET_NAME, COMMAND as BILLING_COMMAND,
    DEFAULT_BUDGET, NAME as BILLING_NAME, WorkerBilling, budget_about, budget_command,
    check_budget, money, parse_budget,
};

/// The name of the setting that says who chooses the workers' models, as
/// /settings and the web page call it. An app that lists the setting uses it
/// as the label.
pub const MODELS_NAME: &str = "Models for workers";

/// What the setting does, before what each value does ([`ModelChoice::about`]).
pub const MODELS_ABOUT: &str = "Who picks a worker's model.";

/// What happens to a worker already started when the setting changes.
pub const MODELS_APPLIES: &str = "A change applies to the next worker started.";

/// The command that sets it. It takes `agent`, `profiles` or `session`
/// ([`ModelChoice::command`] writes it, [`ModelChoice::parse`] reads the
/// word).
pub const MODELS_COMMAND: &str = "/worker-models";

/// The name of the setting for how many levels of workers there can be.
pub const DEPTH_NAME: &str = "Worker depth";

/// The command that sets it, which takes a number from 1 to
/// [`MAX_DEPTH`] ([`parse_depth`] reads it, [`depth_command`] writes it).
pub const DEPTH_COMMAND: &str = "/worker-depth";

/// The name of the setting for how many failed model requests in a row stop
/// a worker.
pub const RETRY_CAP_NAME: &str = "Worker retry cap";

/// The command that sets it, which takes `off`, `on` or a number from 1
/// ([`parse_retry_cap`] reads it, [`retry_cap_command`] writes it).
pub const RETRY_CAP_COMMAND: &str = "/worker-retry-cap";

/// The cap `on` gives, and `--worker-retry-cap` without a number.
pub const DEFAULT_RETRY_CAP: u32 = 10;

/// The name of the setting for whether the agent that started a worker gets
/// a copy of the messages the user sends the worker.
pub const COPIES_NAME: &str = "Message copies";

/// The command that sets it, which takes `on` or `off` ([`copies_command`]
/// writes it).
pub const COPIES_COMMAND: &str = "/message-copies";

/// The command that turns message copies on or off, such as
/// `/message-copies off`.
pub fn copies_command(on: bool) -> String {
    format!("{COPIES_COMMAND} {}", copies_state(on))
}

/// `on` or `off`, as the command takes it.
pub fn copies_state(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The line under the message copies setting, saying what it does with
/// `on` as the setting is.
pub fn copies_about(on: bool) -> &'static str {
    if on {
        "The agent that started a worker gets a copy of each message you send the worker, with its next news."
    } else {
        "The agent that started a worker doesn’t see the messages you send the worker."
    }
}

/// The command that turns the retry cap on (at [`DEFAULT_RETRY_CAP`]) or off,
/// such as `/worker-retry-cap off`.
pub fn retry_cap_command(on: bool) -> String {
    format!("{RETRY_CAP_COMMAND} {}", copies_state(on))
}

/// The line under the retry cap setting, saying what it does with `cap` as
/// the setting is.
pub fn retry_cap_about(cap: Option<u32>) -> String {
    match cap {
        None => format!(
            "Workers keep retrying failed model requests. On, a worker fails after {DEFAULT_RETRY_CAP} in a row, and the workers it started stop with it."
        ),
        Some(cap) => format!(
            "A worker fails after {cap} failed model {} in a row, and the workers it started stop with it. A model response starts the count again.",
            if cap == 1 { "request" } else { "requests" }
        ),
    }
}

/// `cap` if it is a cap a session can have, 1 or more. An app that sets
/// `Options::worker_retry_cap` is held to the same range as the command and
/// the flag.
pub fn check_retry_cap(cap: u32) -> Result<u32, String> {
    if cap >= 1 {
        Ok(cap)
    } else {
        Err("the worker retry cap is a number from 1, not 0".to_string())
    }
}

/// Reads `text` as `/worker-retry-cap` takes it: `off` for no cap, `on` for
/// [`DEFAULT_RETRY_CAP`], or a whole number from 1.
pub fn parse_retry_cap(text: &str) -> Result<Option<u32>, String> {
    match text.trim() {
        "off" => Ok(None),
        "on" => Ok(Some(DEFAULT_RETRY_CAP)),
        number => number
            .parse::<u32>()
            .ok()
            .and_then(|cap| check_retry_cap(cap).ok())
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "`{number}` isn't off, on or a number from 1, such as {RETRY_CAP_COMMAND} {DEFAULT_RETRY_CAP}."
                )
            }),
    }
}

/// A model as the terminal writes it in as few characters as still name it,
/// such as `sol` or `cc:opus`, from its ID as [`WorkerSummary::model`] has
/// it (`gpt-6.1-sol`, `cc:claude-opus-5-5`).
///
/// [`WorkerSummary::model`]: crate::WorkerSummary
pub use crate::providers::short_model_name;

/// The levels of workers a new session allows.
pub use crate::agents::DEFAULT_WORKER_DEPTH as DEFAULT_DEPTH;
/// The most levels of workers a session can allow.
pub use crate::agents::MAX_WORKER_DEPTH as MAX_DEPTH;

/// The command that sets the depth to `depth`, such as `/worker-depth 2`.
pub fn depth_command(depth: u32) -> String {
    format!("{DEPTH_COMMAND} {depth}")
}

/// The line under the depth setting, saying what it does.
pub fn depth_about() -> &'static str {
    "How many levels of workers there can be, from 1 to 4. With 1 only the main agent starts workers. With more, a worker can start workers of its own when the agent that starts it says so, down to that level."
}

/// What a worker depth of `depth` means for the session now, with `choice`
/// as the workers' models are chosen, as a sentence to follow "Worker depth
/// is 2." No worker starts workers when every one runs the session's model.
pub fn depth_effect(depth: u32, choice: ModelChoice) -> String {
    if depth <= 1 {
        "Only the main agent starts workers.".to_string()
    } else if choice == ModelChoice::Session {
        "No worker starts workers, because every worker runs the session’s worker model."
            .to_string()
    } else {
        format!(
            "Workers can go {depth} levels deep, and a worker can start workers of its own when the agent that starts it says so."
        )
    }
}

/// `depth` if it is a depth a session can have, 1 to [`MAX_DEPTH`]. An app
/// that sets `Options::worker_depth` is held to the same range as the
/// command and the flag, so a saved depth is always one the settings show.
pub fn check_depth(depth: u32) -> Result<u32, String> {
    if (1..=MAX_DEPTH).contains(&depth) {
        Ok(depth)
    } else {
        Err(format!(
            "the worker depth is a number from 1 to {MAX_DEPTH}, not {depth}"
        ))
    }
}

/// Reads `text` as `/worker-depth` takes it: a whole number from 1 to
/// [`MAX_DEPTH`]. 1 allows only the main agent to start workers.
pub fn parse_depth(text: &str) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|depth| (1..=MAX_DEPTH).contains(depth))
        .ok_or_else(|| {
            format!(
                "`{}` isn't a depth. Give a number from 1 to {MAX_DEPTH}, such as {DEPTH_COMMAND} {DEFAULT_DEPTH}.",
                text.trim()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_names_and_sets_the_settings_with_the_librarys_words() {
        assert_eq!(MODELS_NAME, "Models for workers");
        assert_eq!(DEPTH_NAME, "Worker depth");
        assert_eq!(ModelChoice::Profiles.command(), "/worker-models profiles");
        assert_eq!(
            ModelChoice::ALL.map(ModelChoice::label),
            [
                "Main agent chooses",
                "Profiles only",
                "Session's worker model"
            ]
        );
        assert_eq!(depth_command(3), "/worker-depth 3");
        assert_eq!(COPIES_NAME, "Message copies");
        assert_eq!(copies_command(true), "/message-copies on");
        assert_eq!(copies_command(false), "/message-copies off");
        assert_ne!(copies_about(true), copies_about(false));
        assert!(copies_about(false).contains("doesn’t"));
        // Choosing the session's model says that no worker starts workers.
        assert_eq!(
            ModelChoice::Session.about(),
            "Every worker runs the session's worker model, and none starts workers."
        );
        // The sentences an app shows say what the value means.
        let agent = ModelChoice::Agent;
        assert_eq!(
            depth_effect(1, agent),
            "Only the main agent starts workers."
        );
        assert_eq!(
            depth_effect(3, agent),
            "Workers can go 3 levels deep, and a worker can start workers of its own when the agent that starts it says so."
        );
        assert_ne!(depth_effect(3, agent), depth_effect(4, agent));
        assert!(depth_effect(4, agent).starts_with("Workers can go 4 levels deep"));
        // With every worker on the session's model none starts workers, at
        // any depth that would let them.
        assert_eq!(
            depth_effect(3, ModelChoice::Session),
            "No worker starts workers, because every worker runs the session’s worker model."
        );
        assert_eq!(
            depth_effect(1, ModelChoice::Session),
            depth_effect(1, agent)
        );
        assert_eq!(
            depth_effect(3, ModelChoice::Profiles),
            depth_effect(3, agent)
        );
        assert!(depth_about().contains("With 1 only the main agent starts workers"));
        assert!(!depth_about().contains("takes effect"));
        assert_eq!(parse_depth(" 4 "), Ok(4));
        assert_eq!(check_depth(1), Ok(1));
        assert_eq!(check_depth(4), Ok(4));
        for bad in [0, 5, u32::MAX] {
            assert!(check_depth(bad).unwrap_err().contains("from 1 to 4"));
        }
        assert_eq!(DEFAULT_DEPTH, 2);
        assert_eq!(RETRY_CAP_NAME, "Worker retry cap");
        assert_eq!(retry_cap_command(true), "/worker-retry-cap on");
        assert_eq!(retry_cap_command(false), "/worker-retry-cap off");
        assert_eq!(
            retry_cap_about(Some(1)),
            "A worker fails after 1 failed model request in a row, and the workers it started stop with it. A model response starts the count again."
        );
        assert!(
            retry_cap_about(Some(5))
                .starts_with("A worker fails after 5 failed model requests in a row")
        );
        assert_eq!(parse_retry_cap(" off "), Ok(None));
        assert_eq!(parse_retry_cap("on"), Ok(Some(DEFAULT_RETRY_CAP)));
        assert_eq!(parse_retry_cap("7"), Ok(Some(7)));
        assert_eq!(check_retry_cap(1), Ok(1));
        for bad in ["0", "-1", "2.5", "many", ""] {
            let error = parse_retry_cap(bad).unwrap_err();
            assert!(
                error.contains("off, on or a number from 1"),
                "{bad}: {error}"
            );
        }
        assert!(check_retry_cap(0).is_err());
        for bad in ["0", "5", "two", "", "-1", "2.5"] {
            let error = parse_depth(bad).unwrap_err();
            assert!(error.contains("from 1 to 4"), "{bad}: {error}");
        }
    }
}
