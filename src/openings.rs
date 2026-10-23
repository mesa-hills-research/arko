//! The messages that open the turns Arko starts for the main agent while it
//! waits for the user, and the transcript's line for each. A reopened
//! session's transcript finds them in the conversation (see
//! `remote::history`), so both read them from here.

/// How the message of a turn Arko started ends.
const STARTED: &str = "(Arko started this turn for it. The user hasn't written.)";

/// How it ended before it was reworded.
const EARLIER_STARTED: &str = "(Arko started this turn for it; the user hasn't written.)";

/// How the message of a monitor's warning begins, before the time left.
const ENDS: &str = "A monitor of yours ends ";

/// A turn Arko starts without a message from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opening {
    /// The workers sent news.
    News,
    /// The workers sent news, and part of it reached the agent earlier but
    /// not whole.
    ResentNews,
    /// A background job of the agent's finished.
    Job,
    /// A monitor of the agent's alerted.
    Alert,
    /// A monitor of the agent's ends in that many minutes (0 for less than
    /// one, `None` when that isn't known), and the agent is asked whether
    /// to keep it.
    EndsSoon(Option<u64>),
}

impl Opening {
    /// The message, which the news follows.
    pub(crate) fn text(self) -> String {
        let lead = match self {
            Self::News => "Your workers sent you news as your last turn ended. It follows.".into(),
            Self::ResentNews => "Your workers sent you news as your last turn ended. Some of it reached you earlier but not whole, so that part follows again.".into(),
            Self::Job => {
                "A background job of yours finished after your last turn ended. Its update follows."
                    .into()
            }
            Self::Alert => {
                "A monitor of yours alerted after your last turn ended. The alert follows.".into()
            }
            Self::EndsSoon(left) => format!(
                "{ENDS}{}. Its notice follows and asks whether to keep it.",
                ends(left)
            ),
        };
        format!("{lead} {STARTED}")
    }

    /// The transcript's line as the turn starts, which a reopened session's
    /// transcript shows too.
    pub(crate) fn notice(self) -> String {
        match self {
            Self::News | Self::ResentNews => "⇠ the workers' news goes to the agent".into(),
            Self::Job => "⇠ a background job finished, and its update goes to the agent".into(),
            Self::Alert => "⇠ a monitor alerted, and its alert goes to the agent".into(),
            Self::EndsSoon(left) => format!(
                "⇠ a monitor ends {}, and the agent is asked whether to keep it",
                ends(left)
            ),
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
            if paragraph.starts_with("Your workers sent you news") {
                Some(if paragraph.contains("follows again.") {
                    Self::ResentNews
                } else {
                    Self::News
                })
            } else if paragraph.starts_with("A background job of yours finished") {
                Some(Self::Job)
            } else if paragraph.starts_with("A monitor of yours alerted") {
                Some(Self::Alert)
            } else {
                let rest = paragraph.strip_prefix(ENDS)?;
                let left = if rest.starts_with("in less than a minute.") {
                    Some(0)
                } else {
                    rest.strip_prefix("in ")
                        .and_then(|rest| rest.split_once(" min."))
                        .and_then(|(minutes, _)| minutes.parse().ok())
                };
                Some(Self::EndsSoon(left))
            }
        })
    }
}

/// When a monitor ends, `left` minutes from now: "in 25 min", "in less
/// than a minute", or "soon" when that isn't known.
fn ends(left: Option<u64>) -> String {
    match left {
        None => "soon".into(),
        Some(0) => "in less than a minute".into(),
        Some(minutes) => format!("in {minutes} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each opening is found in its message, also after other sections and
    /// as worded before, with the time a warning gives, and the user's words
    /// aren't taken for one.
    #[test]
    fn each_opening_is_found_in_its_message() {
        for opening in [
            Opening::News,
            Opening::ResentNews,
            Opening::Job,
            Opening::Alert,
            Opening::EndsSoon(Some(25)),
            Opening::EndsSoon(Some(0)),
            Opening::EndsSoon(None),
        ] {
            assert_eq!(Opening::of(&opening.text()), Some(opening));
            let after = format!("# Session capabilities\n\nNo images.\n\n{}", opening.text());
            assert_eq!(Opening::of(&after), Some(opening));
        }
        // News that goes out again says so, and has the same line in the
        // transcript.
        assert_eq!(
            Opening::ResentNews.text(),
            "Your workers sent you news as your last turn ended. Some of it reached you earlier but not whole, so that part follows again. (Arko started this turn for it. The user hasn't written.)"
        );
        assert_ne!(Opening::ResentNews.text(), Opening::News.text());
        assert_eq!(Opening::ResentNews.notice(), Opening::News.notice());
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
        assert_eq!(
            Opening::EndsSoon(Some(25)).text(),
            "A monitor of yours ends in 25 min. Its notice follows and asks whether to keep it. (Arko started this turn for it. The user hasn't written.)"
        );
        assert_eq!(
            Opening::EndsSoon(Some(25)).notice(),
            "⇠ a monitor ends in 25 min, and the agent is asked whether to keep it"
        );
    }
}
