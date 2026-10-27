//! How long after the user's last message a monitor's warning before it
//! ends still wakes the agent (`/monitor-wakes`, `--monitor-wakes`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long after the user's last message a monitor's warning still wakes
/// an idle agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Saved", into = "String")]
pub enum Wakes {
    /// While the main agent's prompt cache from the user's last message
    /// lasts, which is 90% of its lifetime (an hour where the provider gives
    /// none). It follows the cache of the model in use.
    #[default]
    Auto,
    /// However long ago the user wrote.
    Always,
    /// Never, so the warning goes with the user's next message.
    Never,
    /// This many seconds, up to a day.
    For(u64),
}

/// The setting's name, as /settings, the web page and what Arko says of it
/// call it. An app that lists the setting uses it as the label, so every
/// front end names it the same.
pub const NAME: &str = "Monitor wakes";

/// The command that sets it, which takes a time such as `30m`, `auto`,
/// `always` or `off` ([`parse_wakes`] reads it, [`Wakes::command`] writes
/// it).
pub const COMMAND: &str = "/monitor-wakes";

/// How long `auto` lasts where the provider gives no prompt cache lifetime.
const AUTO_WITHOUT_CACHE: Duration = Duration::from_secs(3_600);

/// The longest time a warning wakes the agent for, short of always, which
/// is a day.
const MOST_SECONDS: u64 = 24 * 3_600;

/// What sessions saved before `auto` hold for the default (two hours), which
/// they can't tell from a choice of two hours. They follow `auto`.
const SAVED_DEFAULT_SECONDS: u64 = 7_200;

impl Wakes {
    /// How long warnings wake the agent for, given how long the main
    /// agent's prompt cache lasts (`None` where the provider gives none), for
    /// the terminal manager (`TerminalManager::set_warning_wakes_within`).
    pub fn within(self, cache: Option<Duration>) -> Option<Duration> {
        match self {
            Wakes::Auto => Some(cache.map_or(AUTO_WITHOUT_CACHE, |cache| cache * 9 / 10)),
            Wakes::Always => None,
            Wakes::Never => Some(Duration::ZERO),
            Wakes::For(seconds) => Some(Duration::from_secs(seconds)),
        }
    }

    /// As `/monitor-wakes` takes it, such as `2h`, `30m`, `auto`, `always`
    /// or `off`.
    pub fn shown(self) -> String {
        match self {
            Wakes::Auto => "auto".into(),
            Wakes::Always => "always".into(),
            Wakes::Never => "off".into(),
            Wakes::For(seconds) => crate::shell::shown_background_after(seconds),
        }
    }

    /// [`shown`](Self::shown), with the time `auto` is now (`within`'s):
    /// `auto (54m)`.
    pub fn shown_now(self, now: Option<Duration>) -> String {
        match (self, now) {
            (Wakes::Auto, Some(now)) => format!(
                "auto ({})",
                crate::shell::shown_background_after(now.as_secs())
            ),
            _ => self.shown(),
        }
    }

    /// The command that sets it, such as `/monitor-wakes 2h`. An app sends
    /// it to the session as the user's input.
    pub fn command(self) -> String {
        format!("{COMMAND} {}", self.shown())
    }

    /// What it means, in a sentence. `now` is the time `auto` is now
    /// (`within`'s).
    pub fn sentence(self, now: Option<Duration>) -> String {
        match self {
            Wakes::Always => {
                "A monitor's warning always wakes the agent, so it can keep the monitor.".into()
            }
            Wakes::Never => {
                "A monitor's warning never wakes the agent. It goes with your next message.".into()
            }
            Wakes::For(seconds) => format!(
                "A monitor's warning wakes the agent for {} after your last message, so it can keep the monitor. Later it goes with your next message.",
                crate::shell::background_after_words(seconds)
            ),
            Wakes::Auto => {
                let now = now.map(|now| {
                    format!(
                        " (now {})",
                        crate::shell::background_after_words(now.as_secs())
                    )
                });
                format!(
                    "A monitor's warning wakes the agent while the prompt cache from your last message lasts{}, so it can keep the monitor. Later it goes with your next message.",
                    now.unwrap_or_default()
                )
            }
        }
    }
}

/// What a session's settings hold: a text such as `auto`, `2h` or `off`, or
/// (in sessions saved before `auto`) a number of seconds, 0 for never, or
/// null for always.
#[derive(Deserialize)]
#[serde(untagged)]
enum Saved {
    Seconds(Option<u64>),
    Text(String),
}

impl TryFrom<Saved> for Wakes {
    type Error = String;

    fn try_from(saved: Saved) -> Result<Self, String> {
        match saved {
            Saved::Seconds(None) => Ok(Wakes::Always),
            Saved::Seconds(Some(0)) => Ok(Wakes::Never),
            Saved::Seconds(Some(SAVED_DEFAULT_SECONDS)) => Ok(Wakes::Auto),
            Saved::Seconds(Some(seconds)) => Ok(Wakes::For(seconds)),
            Saved::Text(text) => parse_wakes(&text),
        }
    }
}

impl From<Wakes> for String {
    fn from(wakes: Wakes) -> String {
        wakes.shown()
    }
}

/// Reads `text` as `/monitor-wakes` takes it, which is a whole number of
/// seconds, minutes or hours (`90m`, `2h`) up to a day, `auto`, `always`, or
/// `off` (never).
pub fn parse_wakes(text: &str) -> Result<Wakes, String> {
    let text = text.trim().to_ascii_lowercase();
    match text.as_str() {
        "auto" => return Ok(Wakes::Auto),
        "always" => return Ok(Wakes::Always),
        "off" | "never" => return Ok(Wakes::Never),
        _ => {}
    }
    let invalid = || {
        format!(
            "`{text}` isn't a time. Give one such as 30m or 2h (at most 24h), auto, always, or off"
        )
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
    Ok(Wakes::For(seconds))
}

/// The steps ← and → go through in the settings, from never to always.
pub const STEPS: [Wakes; 8] = [
    Wakes::Never,
    Wakes::For(1_800),
    Wakes::Auto,
    Wakes::For(3_600),
    Wakes::For(7_200),
    Wakes::For(4 * 3_600),
    Wakes::For(8 * 3_600),
    Wakes::Always,
];

impl Wakes {
    /// Where it stands among the [`STEPS`], as a number that grows with
    /// how long warnings wake the agent. `auto` is just under an hour.
    fn rank(self) -> u64 {
        match self {
            Wakes::Never => 0,
            Wakes::Auto => 3_599,
            Wakes::For(seconds) => seconds,
            Wakes::Always => u64::MAX,
        }
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

    /// The web page and /settings' list in the commands' help call the
    /// setting by the one name.
    #[test]
    fn the_setting_has_one_name() {
        let page: &[u8] = include_bytes!("remote/page/arko-web_bg.wasm");
        assert!(
            page.windows(NAME.len())
                .any(|bytes| bytes == NAME.as_bytes()),
            "the page's row"
        );
        let settings = crate::commands::find("/settings").unwrap();
        assert!(settings.about.contains(&NAME.to_lowercase()));
        #[cfg(feature = "tui")]
        {
            let help = <crate::cli::Args as clap::CommandFactory>::command()
                .get_after_help()
                .map(|help| help.to_string())
                .unwrap_or_default();
            assert_eq!(
                help.matches(&NAME.to_lowercase()).count(),
                2,
                "/settings and the page"
            );
        }
    }

    /// An app that depends on the library names the setting and sets it
    /// with what the library gives it.
    #[test]
    fn the_name_and_the_command_are_public() {
        assert_eq!(crate::monitors::NAME, "Monitor wakes");
        assert_eq!(crate::monitors::COMMAND, "/monitor-wakes");
        assert!(crate::commands::find(COMMAND).is_some());
        assert_eq!(Wakes::default().command(), "/monitor-wakes auto");
        assert_eq!(Wakes::Always.command(), "/monitor-wakes always");
        assert_eq!(Wakes::Never.command(), "/monitor-wakes off");
        // What the command says is what `parse_wakes` reads back.
        for wakes in STEPS {
            let said = wakes.command();
            let argument = said.strip_prefix(COMMAND).unwrap();
            assert_eq!(parse_wakes(argument), Ok(wakes), "{said}");
        }
    }

    #[test]
    fn times_auto_always_and_off_are_read_and_shown() {
        assert_eq!(parse_wakes("2h"), Ok(Wakes::For(7_200)));
        assert_eq!(parse_wakes(" 90M "), Ok(Wakes::For(5_400)));
        assert_eq!(parse_wakes("Auto"), Ok(Wakes::Auto));
        assert_eq!(parse_wakes("always"), Ok(Wakes::Always));
        assert_eq!(parse_wakes("off"), Ok(Wakes::Never));
        assert_eq!(parse_wakes("never"), Ok(Wakes::Never));
        for wrong in ["", "soon", "0m", "25h", "2d", "h"] {
            assert!(parse_wakes(wrong).is_err(), "{wrong}");
        }
        assert_eq!(Wakes::default().shown(), "auto");
        assert_eq!(Wakes::For(7_200).shown(), "2h");
        assert_eq!(Wakes::Always.shown(), "always");
        assert_eq!(Wakes::Never.shown(), "off");
        assert_eq!(
            Wakes::For(7_200).sentence(None),
            "A monitor's warning wakes the agent for 2 hours after your last message, so it can keep the monitor. Later it goes with your next message."
        );
        assert_eq!(
            Wakes::Auto.sentence(Wakes::Auto.within(Some(HOUR))),
            "A monitor's warning wakes the agent while the prompt cache from your last message lasts (now 54 minutes), so it can keep the monitor. Later it goes with your next message."
        );
        assert_eq!(
            Wakes::Auto.shown_now(Wakes::Auto.within(Some(HOUR))),
            "auto (54m)"
        );
        assert_eq!(Wakes::For(7_200).shown_now(None), "2h");
    }

    const HOUR: Duration = Duration::from_secs(3_600);

    /// `auto` lasts 90% of the main agent's prompt cache, and an hour where
    /// the provider gives none, so a change of cache moves it.
    #[test]
    fn auto_follows_the_prompt_cache() {
        let secs = |cache: Option<u64>| {
            Wakes::Auto
                .within(cache.map(Duration::from_secs))
                .map(|within| within.as_secs())
        };
        assert_eq!(secs(Some(300)), Some(270));
        assert_eq!(secs(Some(1_800)), Some(1_620));
        assert_eq!(secs(Some(3_600)), Some(3_240));
        assert_eq!(secs(None), Some(3_600));
        // The others ignore it.
        assert_eq!(Wakes::For(7_200).within(Some(HOUR)), Some(2 * HOUR));
        assert_eq!(Wakes::Never.within(Some(HOUR)), Some(Duration::ZERO));
        assert_eq!(Wakes::Always.within(Some(HOUR)), None);
    }

    /// A session keeps the setting as saved. Auto and the times are texts
    /// such as the command takes them, and the numbers of sessions saved
    /// before auto read as they did, except the old default, which follows
    /// auto.
    #[test]
    fn a_saved_setting_reads_back() {
        let read = |json: &str| serde_json::from_str::<Wakes>(json);
        for wakes in STEPS {
            let saved = serde_json::to_string(&wakes).unwrap();
            assert_eq!(read(&saved).unwrap(), wakes, "{saved}");
        }
        assert_eq!(serde_json::to_string(&Wakes::For(7_200)).unwrap(), "\"2h\"");
        assert_eq!(read("7200").unwrap(), Wakes::Auto);
        assert_eq!(read("5400").unwrap(), Wakes::For(5_400));
        assert_eq!(read("0").unwrap(), Wakes::Never);
        assert_eq!(read("null").unwrap(), Wakes::Always);
        assert!(read("\"soon\"").is_err());
    }

    #[test]
    fn steps_go_from_never_to_always() {
        assert_eq!(Wakes::default().step(true), Some(Wakes::For(3_600)));
        assert_eq!(Wakes::default().step(false), Some(Wakes::For(1_800)));
        assert_eq!(Wakes::For(3_600).step(false), Some(Wakes::Auto));
        assert_eq!(Wakes::For(8 * 3_600).step(true), Some(Wakes::Always));
        assert_eq!(Wakes::Always.step(true), None);
        assert_eq!(Wakes::Never.step(false), None);
        // A time between the steps goes to the nearest one either way.
        assert_eq!(Wakes::For(5_400).step(true), Some(Wakes::For(7_200)));
        assert_eq!(Wakes::For(5_400).step(false), Some(Wakes::For(3_600)));
    }
}
