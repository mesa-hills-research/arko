//! How long after the user's last message a monitor's warning before it
//! ends still wakes the agent (`/monitor-wakes`, `--monitor-wakes`).

use serde::{Deserialize, Serialize};

/// How long after the user's last message a monitor's warning still wakes
/// an idle agent: for that many seconds, `Some(0)` for never, or `None`
/// for always. Two hours by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wakes(pub Option<u64>);

impl Default for Wakes {
    fn default() -> Self {
        Self(Some(DEFAULT_SECONDS))
    }
}

/// Two hours.
const DEFAULT_SECONDS: u64 = 2 * 3_600;

/// The longest time a warning wakes the agent for, short of always: a day.
const MOST_SECONDS: u64 = 24 * 3_600;

impl Wakes {
    /// The time for the terminal manager
    /// (`TerminalManager::set_warning_wakes_within`).
    pub fn within(self) -> Option<std::time::Duration> {
        self.0.map(std::time::Duration::from_secs)
    }

    /// As `/monitor-wakes` takes it: `2h`, `30m`, `always` or `off`.
    pub fn shown(self) -> String {
        match self.0 {
            None => "always".into(),
            Some(0) => "off".into(),
            Some(seconds) => crate::shell::shown_background_after(seconds),
        }
    }

    /// What it means, in a sentence.
    pub fn sentence(self) -> String {
        match self.0 {
            None => "A monitor's warning before it ends always wakes the agent, so it can keep the monitor.".into(),
            Some(0) => "A monitor's warning before it ends never wakes the agent. It goes with your next message.".into(),
            Some(seconds) => format!(
                "A monitor's warning before it ends wakes the agent for {} after your last message, so it can keep the monitor. Later it goes with your next message.",
                crate::shell::background_after_words(seconds)
            ),
        }
    }
}

/// `text` as `/monitor-wakes` takes it: a whole number of seconds, minutes
/// or hours (`90m`, `2h`), up to a day, `always`, or `off` (never).
pub fn parse_wakes(text: &str) -> Result<Wakes, String> {
    let text = text.trim().to_ascii_lowercase();
    match text.as_str() {
        "always" => return Ok(Wakes(None)),
        "off" | "never" => return Ok(Wakes(Some(0))),
        _ => {}
    }
    let invalid = || {
        format!("`{text}` isn't a time. Give one such as 30m or 2h (at most 24h), always, or off")
    };
    let (number, scale) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 3_600),
        _ => return Err(invalid()),
    };
    let seconds = number
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(scale))
        .filter(|seconds| (1..=MOST_SECONDS).contains(seconds))
        .ok_or_else(invalid)?;
    Ok(Wakes(Some(seconds)))
}

/// The steps ← and → go through in the settings, from never to always.
pub const STEPS: [Wakes; 7] = [
    Wakes(Some(0)),
    Wakes(Some(1_800)),
    Wakes(Some(3_600)),
    Wakes(Some(7_200)),
    Wakes(Some(4 * 3_600)),
    Wakes(Some(8 * 3_600)),
    Wakes(None),
];

impl Wakes {
    /// Where it stands among the [`STEPS`], as a number that grows with
    /// how long warnings wake the agent.
    fn rank(self) -> u64 {
        self.0.unwrap_or(u64::MAX)
    }

    /// The next step up (`up`) or down from it, if there is one.
    pub fn step(self, up: bool) -> Option<Wakes> {
        if up {
            STEPS.into_iter().find(|step| step.rank() > self.rank())
        } else {
            STEPS
                .into_iter()
                .rev()
                .find(|step| step.rank() < self.rank())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_always_and_off_are_read_and_shown() {
        assert_eq!(parse_wakes("2h"), Ok(Wakes(Some(7_200))));
        assert_eq!(parse_wakes(" 90M "), Ok(Wakes(Some(5_400))));
        assert_eq!(parse_wakes("always"), Ok(Wakes(None)));
        assert_eq!(parse_wakes("off"), Ok(Wakes(Some(0))));
        assert_eq!(parse_wakes("never"), Ok(Wakes(Some(0))));
        for wrong in ["", "soon", "0m", "25h", "2d", "h"] {
            assert!(parse_wakes(wrong).is_err(), "{wrong}");
        }
        assert_eq!(Wakes::default().shown(), "2h");
        assert_eq!(Wakes(None).shown(), "always");
        assert_eq!(Wakes(Some(0)).shown(), "off");
        assert_eq!(
            Wakes::default().sentence(),
            "A monitor's warning before it ends wakes the agent for 2 hours after your last message, so it can keep the monitor. Later it goes with your next message."
        );
        assert_eq!(
            Wakes::default().within(),
            Some(std::time::Duration::from_secs(7_200))
        );
        assert_eq!(Wakes(None).within(), None);
    }

    #[test]
    fn steps_go_from_never_to_always() {
        assert_eq!(Wakes::default().step(true), Some(Wakes(Some(4 * 3_600))));
        assert_eq!(Wakes::default().step(false), Some(Wakes(Some(3_600))));
        assert_eq!(Wakes(Some(8 * 3_600)).step(true), Some(Wakes(None)));
        assert_eq!(Wakes(None).step(true), None);
        assert_eq!(Wakes(Some(0)).step(false), None);
        // A time between the steps goes to the nearest one either way.
        assert_eq!(Wakes(Some(5_400)).step(true), Some(Wakes(Some(7_200))));
        assert_eq!(Wakes(Some(5_400)).step(false), Some(Wakes(Some(3_600))));
    }
}
