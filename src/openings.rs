//! The messages that open the turns Arko starts for the main agent while it
//! waits for the user, and the transcript's line for each. A reopened
//! session's transcript finds them in the conversation (see
//! `remote::history`), so both read them from here.

/// How the message of a turn Arko started ends.
const STARTED: &str = "(Arko started this turn for it. The user hasn't written.)";

/// How it ended before it was reworded.
const EARLIER_STARTED: &str = "(Arko started this turn for it; the user hasn't written.)";

/// A turn Arko starts without a message from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opening {
    /// The workers sent news.
    News,
    /// A background job of the agent's finished.
    Job,
    /// A monitor of the agent's alerted.
    Alert,
    /// A monitor of the agent's ends soon, and the agent is asked whether to
    /// keep it.
    EndsSoon,
}

impl Opening {
    const ALL: [Self; 4] = [Self::News, Self::Job, Self::Alert, Self::EndsSoon];

    /// The message, which the news follows.
    pub(crate) fn text(self) -> String {
        let lead = match self {
            Self::News => "Your workers sent you news as your last turn ended. It follows.",
            Self::Job => {
                "A background job of yours finished after your last turn ended. Its update follows."
            }
            Self::Alert => {
                "A monitor of yours alerted after your last turn ended. The alert follows."
            }
            Self::EndsSoon => "A monitor of yours is about to end. The notice follows.",
        };
        format!("{lead} {STARTED}")
    }

    /// How the message begins, as worded now and before.
    fn start(self) -> &'static str {
        match self {
            Self::News => "Your workers sent you news",
            Self::Job => "A background job of yours finished",
            Self::Alert => "A monitor of yours alerted",
            Self::EndsSoon => "A monitor of yours is about to end",
        }
    }

    /// The transcript's line as the turn starts, which a reopened session's
    /// transcript shows too.
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::News => "⇠ the workers' news goes to the agent",
            Self::Job => "⇠ a background job finished, and its update goes to the agent",
            Self::Alert => "⇠ a monitor alerted, and its alert goes to the agent",
            Self::EndsSoon => {
                "⇠ a monitor is about to end, and the agent is asked whether to keep it"
            }
        }
    }

    /// The opening of `text`, a message Arko composed. It's a paragraph of
    /// its own, after the sections that go before it.
    pub(crate) fn of(text: &str) -> Option<Self> {
        text.split("\n\n").find_map(|paragraph| {
            let paragraph = paragraph.trim();
            if !paragraph.ends_with(STARTED) && !paragraph.ends_with(EARLIER_STARTED) {
                return None;
            }
            Self::ALL
                .into_iter()
                .find(|opening| paragraph.starts_with(opening.start()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each opening is found in its message, also after other sections and
    /// as worded before, and the user's words aren't taken for one.
    #[test]
    fn each_opening_is_found_in_its_message() {
        for opening in Opening::ALL {
            assert_eq!(Opening::of(&opening.text()), Some(opening));
            let after = format!("# Session capabilities\n\nNo images.\n\n{}", opening.text());
            assert_eq!(Opening::of(&after), Some(opening));
        }
        assert_eq!(
            Opening::of(
                "Your workers sent you news as your last turn ended: it follows. (Arko started this turn for it; the user hasn't written.)"
            ),
            Some(Opening::News)
        );
        assert_eq!(
            Opening::of("A monitor of yours alerted after your last turn ended. Why?"),
            None
        );
    }
}
