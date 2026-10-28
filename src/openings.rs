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
    /// The user answered or dismissed the agent's questions, or wrote a
    /// message that answers them: the note that says so follows.
    Answers,
}

/// The message of a turn for the user's answers, whose notes follow.
const ANSWERS: &str =
    "The user replied to your questions. What they said is in the notes that follow.";

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
            // (The user did write, in the notes.)
            Self::Answers => return ANSWERS.to_string(),
        };
        format!("{lead} {STARTED}")
    }

    /// The transcript's line as the turn starts, naming who and what it is
    /// for (`about`). A reopened session's transcript shows the same line.
    /// The user's answers have none, since answering already says where
    /// they go.
    pub(crate) fn notice(self, about: &Subjects) -> Option<String> {
        let commands = &about.commands;
        Some(match self {
            Self::News | Self::ResentNews => workers_line(&about.workers),
            Self::Job => counted(commands, ("Job", "jobs"), ("finished", "finished")),
            Self::Alert => counted(commands, ("Monitor", "monitors"), ("alerted", "alerted")),
            Self::EndsSoon(left) => {
                let when = ends(left);
                counted(
                    commands,
                    ("Monitor", "monitors"),
                    (&format!("ends {when}"), &format!("end {when}")),
                )
            }
            Self::Answers => return None,
        })
    }

    /// The opening of `text`, a message Arko composed. It's a paragraph of
    /// its own, after the sections that go before it.
    pub(crate) fn of(text: &str) -> Option<Self> {
        text.split("\n\n").find_map(|paragraph| {
            let paragraph = paragraph.trim();
            if paragraph == ANSWERS {
                return Some(Self::Answers);
            }
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

/// What a worker sent in the news of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Heard {
    Report,
    Failed,
    Stopped,
    Asked,
    Message,
}

/// Who and what the news of a turn Arko started is from, which the line in
/// the transcript names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Subjects {
    /// The workers it carries news from, each once, in the order it came.
    pub(crate) workers: Vec<(String, Heard)>,
    /// What each job that finished, or monitor that alerted or ends, runs
    /// (`None` when that isn't known). Each is listed once.
    pub(crate) commands: Vec<Option<String>>,
}

impl Subjects {
    /// The workers that sent `mail`, the news waiting for the main agent.
    /// `handle` gives a run's ID as the agent list shows it (`r4` for a
    /// worker of a worker too). A copy of a message the user sent a worker,
    /// and news from the host, name no worker.
    pub(crate) fn of_mail(
        mail: &[arko_workflows::subagents::MailItem],
        handle: impl Fn(uuid::Uuid) -> Option<String>,
    ) -> Self {
        use arko_workflows::subagents::{MailKind, MailSender};
        let mut about = Self::default();
        for item in mail {
            let MailSender::Run(from) = item.from else {
                continue;
            };
            let heard = match item.kind {
                MailKind::Report => Heard::Report,
                MailKind::Failed => Heard::Failed,
                MailKind::Stopped => Heard::Stopped,
                MailKind::Question => Heard::Asked,
                MailKind::Message => Heard::Message,
                MailKind::Copy => continue,
            };
            if let Some(who) = handle(from) {
                about.heard(&who, heard);
            }
        }
        about
    }

    /// `who` sent `heard`. A worker is listed once, as what it did last
    /// besides sending messages (it reported, then a message came: it
    /// reported).
    fn heard(&mut self, who: &str, heard: Heard) {
        match self.workers.iter_mut().find(|(known, _)| known == who) {
            Some((_, kept)) => {
                if heard != Heard::Message {
                    *kept = heard;
                }
            }
            None => self.workers.push((who.to_string(), heard)),
        }
    }

    /// What the news of the turn `opening` started names, from the text
    /// that went to the agent with it (the context updates that follow its
    /// message). `job_name` gives what a job ran, by its ID (the monitors
    /// the news names have their commands in it). What the text doesn't say
    /// stays out.
    pub(crate) fn in_news(
        opening: Opening,
        news: &str,
        job_name: impl Fn(uuid::Uuid) -> Option<String>,
    ) -> Self {
        let mut about = Self::default();
        let lines: Vec<&str> = news.lines().collect();
        let mut seen: Vec<uuid::Uuid> = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            match opening {
                Opening::News | Opening::ResentNews => {
                    if let Some((who, heard)) = mail_in(line) {
                        about.heard(who, heard);
                    }
                }
                Opening::Job => {
                    let job = if *line == "Terminal update:" {
                        lines.get(at + 1).and_then(|json| job_in_update(json))
                    } else {
                        line.strip_prefix("Job ").and_then(job_at_start)
                    };
                    if let Some(job) = job.filter(|job| !seen.contains(job)) {
                        seen.push(job);
                        about.commands.push(job_name(job));
                    }
                }
                Opening::Alert | Opening::EndsSoon(_) => {
                    let named = if opening == Opening::Alert {
                        line.strip_prefix("Monitor alert. ")
                            .and_then(|alert| monitor_in(alert, "`, "))
                    } else {
                        line.strip_prefix("Monitor ")
                            .and_then(|warning| monitor_in(warning, "`) ends in "))
                    };
                    if let Some((job, command)) = named.filter(|(job, _)| !seen.contains(job)) {
                        seen.push(job);
                        // (The news has the command too, if the call that
                        // started the monitor isn't there to read.)
                        about
                            .commands
                            .push(job_name(job).or_else(|| Some(crate::jobs::label(&command))));
                    }
                }
                Opening::Answers => {}
            }
        }
        about
    }
}

/// The worker and what it sent, from a line of a context update that starts
/// an item of mail (`- [mail 3] r1 (agent `worker`) finished. Its report:`).
fn mail_in(line: &str) -> Option<(&str, Heard)> {
    let heading = line.strip_prefix("- [mail ")?.split_once("] ")?.1;
    let who = heading.split_whitespace().next()?;
    let heard = if heading.contains(" finished. Its report:") {
        Heard::Report
    } else if heading.contains(" stopped with an error:") {
        Heard::Failed
    } else if heading.contains(" stopped before it finished:") {
        Heard::Stopped
    } else if heading.contains(" asks you (") {
        Heard::Asked
    } else if heading.contains(" sent you a message") {
        Heard::Message
    } else {
        return None;
    };
    Some((who, heard))
}

/// The job a terminal update's JSON says it is for.
fn job_in_update(json: &str) -> Option<uuid::Uuid> {
    let update: serde_json::Value = serde_json::from_str(json).ok()?;
    update.get("job_id")?.as_str()?.parse().ok()
}

/// The job ID at the start of `text` (`4d2c… ended.`).
fn job_at_start(text: &str) -> Option<uuid::Uuid> {
    let id = text.get(..36)?;
    text[36..]
        .starts_with(' ')
        .then(|| id.parse().ok())
        .flatten()
}

/// The monitor a line of news names, as its job and command. The command
/// is in backticks after the job's ID, and `closing` follows its closing
/// backtick.
fn monitor_in(text: &str, closing: &str) -> Option<(uuid::Uuid, String)> {
    let start = text.find(" (`")?;
    let job = text[..start].get(start.checked_sub(36)?..)?.parse().ok()?;
    let rest = &text[start + 3..];
    let end = rest.find(closing)?;
    Some((job, rest[..end].to_string()))
}

/// The line for news from workers: "r1 reported", "r1, r2 and r3
/// reported", "Message from r2", "r1 reported · r2 sent a message".
fn workers_line(workers: &[(String, Heard)]) -> String {
    let only_messages = workers.iter().all(|(_, heard)| *heard == Heard::Message);
    let parts: Vec<String> = [
        Heard::Report,
        Heard::Failed,
        Heard::Stopped,
        Heard::Asked,
        Heard::Message,
    ]
    .into_iter()
    .filter_map(|kind| {
        let names: Vec<&str> = workers
            .iter()
            .filter(|(_, heard)| *heard == kind)
            .map(|(who, _)| who.as_str())
            .collect();
        if names.is_empty() {
            return None;
        }
        let many = names.len() > 1;
        let who = list(&names, 3, "others");
        Some(match kind {
            Heard::Report => format!("{who} reported"),
            Heard::Failed => format!("{who} failed"),
            Heard::Stopped => format!("{who} stopped"),
            Heard::Asked if many => format!("{who} asked questions"),
            Heard::Asked => format!("{who} asked a question"),
            Heard::Message if only_messages && many => format!("Messages from {who}"),
            Heard::Message if only_messages => format!("Message from {who}"),
            Heard::Message if many => format!("{who} sent messages"),
            Heard::Message => format!("{who} sent a message"),
        })
    })
    .collect();
    if parts.is_empty() {
        "News from workers".into()
    } else {
        parts.join(" · ")
    }
}

/// The line for jobs or monitors: "Job finished · cargo build", "2 jobs
/// finished · cargo build and npm test". `noun` is the singular and plural,
/// and `did` what happened, for one and for several.
fn counted(commands: &[Option<String>], noun: (&str, &str), did: (&str, &str)) -> String {
    let head = match commands.len() {
        0 | 1 => format!("{} {}", noun.0, did.0),
        count => format!("{count} {} {}", noun.1, did.1),
    };
    let mut names: Vec<&str> = Vec::new();
    for name in commands.iter().flatten().filter(|name| !name.is_empty()) {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    if names.is_empty() {
        head
    } else {
        format!("{head} · {}", list(&names, 2, "more"))
    }
}

/// `names` in words: "a", "a and b", "a, b and c". More than `whole` of
/// them show the first two and how many are left (`rest`): "a, b and 4
/// others".
fn list(names: &[&str], whole: usize, rest: &str) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [init @ .., last] if names.len() <= whole => format!("{} and {last}", init.join(", ")),
        [first, second, left @ ..] => format!("{first}, {second} and {} {rest}", left.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_workflows::subagents::{MailItem, MailKind, MailSender};
    use uuid::Uuid;

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
            Opening::Answers,
        ] {
            assert_eq!(Opening::of(&opening.text()), Some(opening));
            let after = format!("# Session capabilities\n\nNo images.\n\n{}", opening.text());
            assert_eq!(Opening::of(&after), Some(opening));
        }
        // News that goes out again says so to the agent, and the user sees
        // the same line.
        assert_eq!(
            Opening::ResentNews.text(),
            "Your workers sent you news as your last turn ended. Some of it reached you earlier but not whole, so that part follows again. (Arko started this turn for it. The user hasn't written.)"
        );
        assert_ne!(Opening::ResentNews.text(), Opening::News.text());
        let about = workers(&[("r1", Heard::Report)]);
        assert_eq!(
            Opening::ResentNews.notice(&about).as_deref(),
            Some("r1 reported")
        );
        assert_eq!(
            Opening::ResentNews.notice(&about),
            Opening::News.notice(&about)
        );
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
        // The user's answers go to the agent without a line saying so.
        assert_eq!(Opening::Answers.notice(&Subjects::default()), None);
    }

    fn workers(sent: &[(&str, Heard)]) -> Subjects {
        let mut about = Subjects::default();
        for (who, heard) in sent {
            about.heard(who, *heard);
        }
        about
    }

    fn commands(names: &[Option<&str>]) -> Subjects {
        Subjects {
            commands: names.iter().map(|name| name.map(str::to_string)).collect(),
            ..Subjects::default()
        }
    }

    fn news(sent: &[(&str, Heard)]) -> String {
        Opening::News.notice(&workers(sent)).unwrap()
    }

    /// News from workers says who reported, and what else they sent.
    #[test]
    fn news_names_the_workers_and_what_they_sent() {
        use Heard::*;
        assert_eq!(news(&[("r1", Report)]), "r1 reported");
        assert_eq!(
            news(&[("r1", Report), ("r3", Report)]),
            "r1 and r3 reported"
        );
        assert_eq!(
            news(&[("r1", Report), ("r2", Report), ("r3", Report)]),
            "r1, r2 and r3 reported"
        );
        assert_eq!(news(&[("r2", Message)]), "Message from r2");
        assert_eq!(
            news(&[("r2", Message), ("r5", Message)]),
            "Messages from r2 and r5"
        );
        assert_eq!(
            news(&[("r1", Report), ("r2", Message)]),
            "r1 reported · r2 sent a message"
        );
        assert_eq!(
            news(&[
                ("r1", Report),
                ("r2", Message),
                ("r3", Message),
                ("r4", Report)
            ]),
            "r1 and r4 reported · r2 and r3 sent messages"
        );
        assert_eq!(news(&[("r1", Failed)]), "r1 failed");
        assert_eq!(
            news(&[("r1", Stopped), ("r2", Stopped)]),
            "r1 and r2 stopped"
        );
        assert_eq!(news(&[("r1", Asked)]), "r1 asked a question");
        assert_eq!(
            news(&[("r1", Asked), ("r2", Asked)]),
            "r1 and r2 asked questions"
        );
        // Nothing is known of who sent it.
        assert_eq!(
            Opening::News.notice(&Subjects::default()).unwrap(),
            "News from workers"
        );
    }

    /// A long list shows the first two workers and how many are left.
    #[test]
    fn a_long_list_of_workers_is_cut() {
        let line = |count: usize, heard| {
            let mut about = Subjects::default();
            for n in 1..=count {
                about.heard(&format!("r{n}"), heard);
            }
            Opening::News.notice(&about).unwrap()
        };
        assert_eq!(line(3, Heard::Report), "r1, r2 and r3 reported");
        assert_eq!(line(4, Heard::Report), "r1, r2 and 2 others reported");
        assert_eq!(line(6, Heard::Report), "r1, r2 and 4 others reported");
        assert_eq!(line(6, Heard::Message), "Messages from r1, r2 and 4 others");
        let mut about = workers(&[("r9", Heard::Report)]);
        for n in 1..=5 {
            about.heard(&format!("r{n}"), Heard::Message);
        }
        assert_eq!(
            Opening::News.notice(&about).unwrap(),
            "r9 reported · r1, r2 and 3 others sent messages"
        );
    }

    /// A worker shows once, as what it did besides sending messages.
    #[test]
    fn a_worker_shows_once() {
        use Heard::*;
        assert_eq!(
            news(&[("r1", Message), ("r1", Report), ("r1", Message)]),
            "r1 reported"
        );
        assert_eq!(news(&[("r1", Report), ("r1", Failed)]), "r1 failed");
        assert_eq!(
            news(&[("r1", Message), ("r2", Message), ("r1", Message)]),
            "Messages from r1 and r2"
        );
    }

    fn mail(from: MailSender, kind: MailKind) -> MailItem {
        MailItem {
            id: 1,
            to: Uuid::nil(),
            from,
            kind,
            heading: String::new(),
            body: String::new(),
            at: chrono::Utc::now(),
            delivered_at: None,
            file: None,
            resent: false,
            withheld: false,
        }
    }

    /// The workers of the waiting mail are named by the IDs the agent list
    /// shows, a worker of a worker too. A copy of the user's message to a
    /// worker, and the host's news, name none.
    #[test]
    fn mail_names_its_senders_by_their_agent_list_ids() {
        let (one, four, gone) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let handle = |id: Uuid| {
            [(one, "r1"), (four, "r4")]
                .into_iter()
                .find(|(known, _)| *known == id)
                .map(|(_, handle)| handle.to_string())
        };
        let from = MailSender::Run;
        let about = Subjects::of_mail(
            &[
                mail(from(one), MailKind::Message),
                mail(from(four), MailKind::Report),
                mail(from(one), MailKind::Copy),
                mail(MailSender::Host, MailKind::Copy),
                mail(MailSender::Host, MailKind::Message),
                mail(from(gone), MailKind::Report),
                mail(from(one), MailKind::Message),
            ],
            handle,
        );
        assert_eq!(
            about.workers,
            [
                ("r1".to_string(), Heard::Message),
                ("r4".to_string(), Heard::Report)
            ]
        );
        assert_eq!(
            Opening::News.notice(&about).unwrap(),
            "r4 reported · r1 sent a message"
        );
        let kinds = [
            (MailKind::Report, "r1 reported"),
            (MailKind::Failed, "r1 failed"),
            (MailKind::Stopped, "r1 stopped"),
            (MailKind::Question, "r1 asked a question"),
            (MailKind::Message, "Message from r1"),
        ];
        for (kind, line) in kinds {
            let about = Subjects::of_mail(&[mail(from(one), kind)], handle);
            assert_eq!(Opening::News.notice(&about).unwrap(), line);
        }
    }

    /// A job's line names what it ran, and says how many when several did.
    #[test]
    fn a_job_is_named_by_its_command() {
        let job = |names: &[Option<&str>]| Opening::Job.notice(&commands(names)).unwrap();
        assert_eq!(
            job(&[Some("cargo build --release")]),
            "Job finished · cargo build --release"
        );
        // Without a command the line says only that.
        assert_eq!(job(&[None]), "Job finished");
        assert_eq!(job(&[]), "Job finished");
        assert_eq!(
            job(&[Some("cargo build"), Some("npm test")]),
            "2 jobs finished · cargo build and npm test"
        );
        assert_eq!(
            job(&[Some("cargo build"), Some("npm test"), Some("npm run lint")]),
            "3 jobs finished · cargo build, npm test and 1 more"
        );
        assert_eq!(
            job(&[Some("a"), Some("b"), Some("c"), Some("d"), Some("e")]),
            "5 jobs finished · a, b and 3 more"
        );
        // A command that ran twice shows once, and one without a name only
        // counts.
        assert_eq!(
            job(&[Some("npm test"), Some("npm test"), None]),
            "3 jobs finished · npm test"
        );
        assert_eq!(job(&[None, None]), "2 jobs finished");
    }

    /// A monitor's alert and its warning name the monitor by its command.
    #[test]
    fn a_monitor_is_named_by_its_command() {
        let alert = |names: &[Option<&str>]| Opening::Alert.notice(&commands(names)).unwrap();
        assert_eq!(
            alert(&[Some("npm run dev")]),
            "Monitor alerted · npm run dev"
        );
        assert_eq!(alert(&[None]), "Monitor alerted");
        assert_eq!(
            alert(&[Some("npm run dev"), Some("cargo watch")]),
            "2 monitors alerted · npm run dev and cargo watch"
        );
        let ends = |left, names: &[Option<&str>]| {
            Opening::EndsSoon(left).notice(&commands(names)).unwrap()
        };
        assert_eq!(
            ends(Some(2), &[Some("npm run dev")]),
            "Monitor ends in 2 min · npm run dev"
        );
        assert_eq!(
            ends(Some(0), &[Some("npm run dev")]),
            "Monitor ends in less than a minute · npm run dev"
        );
        assert_eq!(
            ends(None, &[Some("npm run dev")]),
            "Monitor ends soon · npm run dev"
        );
        assert_eq!(ends(Some(2), &[]), "Monitor ends in 2 min");
        assert_eq!(
            ends(Some(2), &[Some("a"), Some("b")]),
            "2 monitors end in 2 min · a and b"
        );
    }

    const JOB: &str = "4d2c1a9b-0f3e-4c7a-9b1e-5a6d7c8e9f00";
    const OTHER: &str = "b57d52ff-2c3e-4c4b-9a55-0d1f6a1e2b3c";

    fn id(text: &str) -> Uuid {
        text.parse().unwrap()
    }

    /// The news that went with a turn names the workers (the lines of mail in
    /// a context update).
    #[test]
    fn the_workers_are_read_from_the_mail_that_went_with_the_turn() {
        let update = "# Workflow context update\n\nNew mail from the agent runs you work with, oldest first. Reply with message_agent; wait_for_agents waits for more.\n- [mail 3] r1 (agent `worker`) finished. Its report:\n  Done.\n  [end of mail 3]\n- [mail 4] r2 (agent `worker`) sent you a message (it goes on working):\n  Which branch?\n  [end of mail 4]\n- [mail 5] r3 (agent `worker`) stopped with an error:\n  - [mail 9] r7 (agent `worker`) finished. Its report:\n  [end of mail 5]\n- [mail 6] r4 (agent `worker`) asks you (it waits for your answer: message_agent with to \"r4\"):\n  Which?\n  [end of mail 6]\n- [mail 7] r5 (agent `worker`) stopped before it finished: you stopped it.\n  [end of mail 7]\n- [mail 8] The user sent r1 a message directly (a copy for you):\n  Hurry.\n  [end of mail 8]";
        let about = Subjects::in_news(Opening::News, update, |_| None);
        assert_eq!(
            about.workers,
            [
                ("r1".to_string(), Heard::Report),
                ("r2".to_string(), Heard::Message),
                ("r3".to_string(), Heard::Failed),
                ("r4".to_string(), Heard::Asked),
                ("r5".to_string(), Heard::Stopped),
            ]
        );
        // Nothing of the turn's news in the text.
        let none = Subjects::in_news(Opening::News, "Done.", |_| None);
        assert_eq!(none, Subjects::default());
    }

    /// A job's turn names the jobs its updates are for, once each.
    #[test]
    fn the_jobs_are_read_from_their_updates() {
        let update = format!(
            "# Workflow context update\n\nTerminal update:\n{{\"job_id\":\"{JOB}\",\"revision\":3,\"state\":\"succeeded\"}}\n\nTerminal update:\n{{\"job_id\":\"{JOB}\",\"revision\":4}}\n\nJob {OTHER} ended. It ran your search call, which moved to the background 1 min ago. Its result follows.\n\nfound"
        );
        let name = |job: Uuid| (job == id(JOB)).then(|| "cargo build".to_string());
        let about = Subjects::in_news(Opening::Job, &update, name);
        assert_eq!(about.commands, [Some("cargo build".to_string()), None]);
        assert_eq!(
            Opening::Job.notice(&about).unwrap(),
            "2 jobs finished · cargo build"
        );
    }

    /// A monitor's turn names the monitors whose alerts or warnings went with
    /// it, by their commands, once each.
    #[test]
    fn the_monitors_are_read_from_their_news() {
        let alert = format!(
            "# Workflow context update\n\nMonitor alert. Job {JOB} (`npm run dev`, lines that match `error|warn`) printed 5 matching lines since 10:41:05 UTC.\n  Monitor alert. Job {OTHER} (`not this`, a check every 15 s) changed.\nIt ends in 22 min.\n\nMonitor alert. The check of job {OTHER} (`curl -sf localhost:3000/health`, every 15 s) started succeeding at 10:44:30 UTC, after failing for 3 min.\n\nMonitor alert. Job {JOB} (`npm run dev`, lines that match `error`) printed 1 matching line since 10:50:00 UTC."
        );
        let about = Subjects::in_news(Opening::Alert, &alert, |_| None);
        assert_eq!(
            about.commands,
            [
                Some("npm run dev".to_string()),
                Some("curl -sf localhost:3000/health".to_string())
            ]
        );
        let warning = format!(
            "# Workflow context update\n\nMonitor {JOB} (`npm run dev`) ends in 2 min, at 11:00 UTC. monitor_control rearm keeps it going for up to 30 min from now. If you no longer need it, let it end.\n\nMonitor {OTHER} (`npm test`) ended at 10:59 UTC and its command was stopped."
        );
        let about = Subjects::in_news(Opening::EndsSoon(Some(2)), &warning, |_| None);
        assert_eq!(about.commands, [Some("npm run dev".to_string())]);
        // An alert's text names no monitor for a warning, nor the reverse.
        assert_eq!(
            Subjects::in_news(Opening::EndsSoon(None), &alert, |_| None),
            Subjects::default()
        );
        assert_eq!(
            Subjects::in_news(Opening::Alert, &warning, |_| None),
            Subjects::default()
        );
    }

    /// A long command is cut as the job rows cut it.
    #[test]
    fn a_long_command_is_cut_as_job_rows_cut_it() {
        let command = "x".repeat(100);
        let alert = format!(
            "Monitor alert. Job {JOB} (`{command}`, lines that match `e`) printed 1 matching line."
        );
        let about = Subjects::in_news(Opening::Alert, &alert, |_| None);
        assert_eq!(about.commands, [Some(format!("{}…", "x".repeat(80)))]);
    }
}
