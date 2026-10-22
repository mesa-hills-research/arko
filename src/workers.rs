//! The session settings for who chooses the workers' models and how deep
//! workers go, with the words and commands every front end uses for them
//! (`/worker-models`, `/worker-depth`, `--worker-models`, `--worker-depth`).
//! The terminal's settings, the web page and an app's settings page name and
//! set them alike.

pub use crate::agents::ModelChoice;

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

/// The line under the depth setting, saying what it does. `nested` is whether
/// workers can start workers yet (`LiveSettings::nested_workers`).
pub fn depth_about(nested: bool) -> &'static str {
    if nested {
        "How many levels of workers there can be, from 1 to 4. 1 means only the main agent starts workers. 2 lets a worker the main agent starts begin workers of its own, when the main agent allows it."
    } else {
        "How many levels of workers there can be, from 1 to 4. It takes effect once workers can start workers of their own, so for now only the main agent starts workers."
    }
}

/// What a worker depth of `depth` means for the session now, as a sentence
/// to follow "Worker depth is 2."
pub fn depth_effect(depth: u32, nested: bool) -> String {
    if !nested {
        "It takes effect once workers can start workers of their own, so for now only the main agent starts workers.".to_string()
    } else if depth <= 1 {
        "Only the main agent starts workers.".to_string()
    } else {
        format!(
            "Workers can go {depth} levels deep. A worker the main agent starts can start workers of its own when the main agent allows it."
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
        // The sentences an app shows say the value once, and that nothing
        // nests yet when that is so.
        assert!(
            depth_effect(2, false).starts_with("It takes effect once workers can start workers")
        );
        assert_eq!(depth_effect(1, true), "Only the main agent starts workers.");
        assert!(depth_effect(3, true).starts_with("Workers can go 3 levels deep."));
        assert!(depth_about(false).contains("takes effect once workers can start workers"));
        assert!(depth_about(true).contains("1 means only the main agent starts workers"));
        assert_eq!(parse_depth(" 4 "), Ok(4));
        assert_eq!(check_depth(1), Ok(1));
        assert_eq!(check_depth(4), Ok(4));
        for bad in [0, 5, u32::MAX] {
            assert!(check_depth(bad).unwrap_err().contains("from 1 to 4"));
        }
        assert_eq!(DEFAULT_DEPTH, 2);
        for bad in ["0", "5", "two", "", "-1", "2.5"] {
            let error = parse_depth(bad).unwrap_err();
            assert!(error.contains("from 1 to 4"), "{bad}: {error}");
        }
    }
}
