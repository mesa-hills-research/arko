//! What the user sends, read the same way whatever the session is doing: a
//! line typed at the prompt, an app's message or answer, or the web page's.
//! An idle session, a turn and the old model's report for a switch read it
//! with [`Harness::read_sent`], check it once with [`Harness::admit`] before
//! anything happens, and run a command with [`Harness::run_command`].
//!
//! A client that sent it with [`Request::Send`](crate::Request::Send) gets
//! its [`Verdict`] through its [`Reply`]: refused by a check, or accepted
//! once it went. A send without one is told in the transcript, as it always
//! was.

use tokio::sync::oneshot;

use crate::conversations;
use crate::harness::{Harness, command, is_remote_command, openrouter_words, split_live_command};
use crate::inputs::{Event, Inputs};
use crate::remote::{
    Action, Confirm, Delivery, Level, Partial, Refusal, RefusalKind, Source, Verdict,
};
use crate::ui::{self, Input};

/// What the session is doing as a send comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No turn runs.
    Idle,
    /// The main agent's turn runs.
    Turn,
    /// The old model writes its report for a switch of the main agent's
    /// model. Commands run, and messages wait.
    Reporting,
}

/// Where a send's verdict goes: to the client that sent it, or, for a send
/// without a verdict to come, nowhere, and the caller tells the user in the
/// transcript as it always has. It answers once, and whatever reads a send
/// answers it, accepted or refused. One dropped unanswered is a mistake in
/// Arko: its client hears nothing, and says it doesn't know whether the
/// send went.
#[derive(Debug, Default)]
pub(crate) struct Reply(Option<oneshot::Sender<Verdict>>);

impl Drop for Reply {
    fn drop(&mut self) {
        if self.0.is_some() && !std::thread::panicking() {
            log::error!("a send's verdict was dropped unanswered");
            debug_assert!(false, "a send's verdict was dropped unanswered");
        }
    }
}

impl Reply {
    pub(crate) fn to(sender: Option<oneshot::Sender<Verdict>>) -> Self {
        Self(sender)
    }

    /// Whether a client waits for the verdict.
    pub(crate) fn tracked(&self) -> bool {
        self.0.is_some()
    }

    /// The send went as `delivery`, as the feed item `item`.
    pub(crate) fn accept(&mut self, delivery: Delivery, item: Option<u64>) {
        self.answer(Verdict::accepted(delivery, item));
    }

    /// The send went as `delivery`, as the feed item `item`, but for what
    /// `partial` says.
    pub(crate) fn accept_partly(
        &mut self,
        delivery: Delivery,
        item: Option<u64>,
        partial: Option<Partial>,
    ) {
        self.answer(Verdict::Accepted {
            delivery,
            item,
            partial,
        });
    }

    /// Refuses the send for `kind`, with `words` to show. Returns whether a
    /// client was told. When none was, the caller tells the user.
    pub(crate) fn refuse(&mut self, kind: RefusalKind, words: impl Into<String>) -> bool {
        self.refuse_unless(kind, words, None)
    }

    /// [`refuse`](Self::refuse), saying what sending it again needs.
    pub(crate) fn refuse_unless(
        &mut self,
        kind: RefusalKind,
        words: impl Into<String>,
        confirm: Option<Confirm>,
    ) -> bool {
        let tracked = self.tracked();
        self.answer(Verdict::Refused(Refusal {
            kind,
            words: words.into(),
            confirm,
        }));
        tracked
    }

    fn answer(&mut self, verdict: Verdict) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(verdict);
        }
    }
}

/// What the user sent, as its event carried it.
pub(crate) struct Sent {
    /// The line as typed, its leading spaces kept.
    pub(crate) line: String,
    pub(crate) source: Source,
    /// The open questions it answers.
    pub(crate) attached: Vec<String>,
    /// An app's message's images, which [`Harness::admit`] attaches. `None`
    /// for what isn't an app's message.
    images: Option<Vec<ui::Attachment>>,
    /// The user agreed, as they sent it, to continue a worker whose prompt
    /// cache expired. It holds for this send alone.
    confirmed: bool,
    pub(crate) reply: Reply,
}

/// A send that passed the checks. It is accepted where it goes.
pub(crate) enum Admitted {
    /// A command, trimmed.
    Command {
        command: String,
        source: Source,
        /// As [`Sent::confirmed`].
        confirmed: bool,
        reply: Reply,
    },
    /// A message, trimmed, with the open questions it answers.
    Message {
        text: String,
        source: Source,
        attached: Vec<String>,
        reply: Reply,
    },
}

/// An input event, read.
pub(crate) enum Read {
    /// The user sent this.
    Sent(Sent),
    /// The user sent it, and it goes no further. The user was told why.
    Done,
    /// It isn't something the user sent, and the caller has seen to it.
    Other,
}

impl Harness {
    /// Reads what the user sent in `event`: a line (with the lines pasted
    /// with it), an app's message, an answer to a question card, or the
    /// web page's message, which the terminal shows. Anything else is the
    /// caller's to see to before it asks.
    pub(crate) async fn read_sent(
        &mut self,
        event: Event,
        mut reply: Reply,
        input: &mut Inputs,
        phase: Phase,
    ) -> Read {
        // (While the report is written, nothing an answer says goes, so its
        // card isn't looked at.)
        let reporting = phase == Phase::Reporting;
        let sent = |line: String, source: Source, attached: Vec<String>, reply: Reply| {
            Read::Sent(Sent {
                line,
                source,
                attached,
                images: None,
                confirmed: false,
                reply,
            })
        };
        match event {
            Event::Input(Some(Input::Line(line))) => {
                let line = input.coalesce(line).await;
                sent(line, self.input_source, Vec::new(), reply)
            }
            Event::Input(Some(Input::Message(message))) => Read::Sent(Sent {
                line: message.text,
                source: self.input_source,
                attached: message.answers,
                images: Some(message.images),
                confirmed: message.continue_uncached,
                reply,
            }),
            Event::Input(Some(Input::Answer { question, text })) => {
                let attached = match reporting {
                    true => Vec::new(),
                    false => match self.card_answered(question) {
                        Some(attached) => attached,
                        None => return self.card_closed(&mut reply),
                    },
                };
                sent(text, self.input_source, attached, reply)
            }
            Event::Remote(Action::Message(text)) => {
                self.echo_web(&text);
                sent(text, Source::Web, Vec::new(), reply)
            }
            Event::Remote(Action::Continue(line)) => {
                self.echo_web(&line);
                Read::Sent(Sent {
                    line,
                    source: Source::Web,
                    attached: Vec::new(),
                    images: None,
                    confirmed: true,
                    reply,
                })
            }
            Event::Remote(Action::Reply { text, answers }) => {
                self.echo_web(&text);
                sent(text, Source::Web, answers, reply)
            }
            Event::Remote(Action::Answer { question, text }) => {
                let attached = match reporting {
                    true => Vec::new(),
                    false => match self.card_answered(question) {
                        Some(attached) => attached,
                        None => return self.card_closed(&mut reply),
                    },
                };
                self.echo_web(&text);
                sent(text, Source::Web, attached, reply)
            }
            _ => {
                // (Not a send: a stop or the like, accepted as it was read.)
                reply.accept(Delivery::Command, None);
                Read::Other
            }
        }
    }

    /// An answer to a question card that has no question open any more.
    fn card_closed(&self, reply: &mut Reply) -> Read {
        const WORDS: &str = "That question is no longer open.";
        if !reply.refuse(RefusalKind::NotOpen, WORDS) {
            self.feed.notice(Level::Warn, WORDS);
        }
        Read::Done
    }

    /// Checks what the user sent before anything of it happens, and says
    /// why when it can't go: an app's images the model can't take, a
    /// command the web page can't run or one that doesn't exist, images
    /// with a message to a worker, a message while the old model writes its
    /// report, and answers too long to send. An app's images are attached
    /// for its message, and a command drops them.
    pub(crate) fn admit(&mut self, sent: Sent, phase: Phase) -> Option<Admitted> {
        let Sent {
            line,
            source,
            attached,
            images,
            confirmed,
            mut reply,
        } = sent;
        // (While the report is written, an app's message doesn't go, so its
        // images aren't attached.)
        let reported_images = match images {
            Some(images) if phase != Phase::Reporting => {
                if let Err(why) = self.attach(images) {
                    if !reply.refuse(RefusalKind::Images, &why) {
                        self.say(Level::Error, &why);
                    }
                    return None;
                }
                0
            }
            Some(images) => images.len(),
            None => 0,
        };
        let has_images = match phase {
            Phase::Reporting => reported_images > 0,
            Phase::Idle | Phase::Turn => !self.pending_images.is_empty(),
        };
        let text = line.trim();
        if text.is_empty() && !has_images {
            reply.refuse(RefusalKind::Empty, "There is nothing to send.");
            return None;
        }
        self.user_present();
        if let Some(command) = command(text) {
            if has_images && split_live_command(command).is_some_and(|(name, _)| name == "/message")
            {
                self.drop_images(phase);
                let words = "A message to a worker takes text only. Remove the images, or send them to Arko.";
                if !reply.refuse(RefusalKind::WorkerTextOnly, words) {
                    self.say(Level::Warn, words);
                }
                return None;
            }
            // (Images go with messages only.)
            self.drop_images(phase);
            if let Some(why) = self.terminal_only(command, source) {
                if !reply.refuse(RefusalKind::TerminalOnly, &why) {
                    self.say(Level::Warn, &format!("  {why}"));
                }
                return None;
            }
            return Some(Admitted::Command {
                command: command.to_string(),
                source,
                confirmed,
                reply,
            });
        }
        if let Some(why) = self.unknown_command(&line, source) {
            self.drop_images(phase);
            if !reply.refuse(RefusalKind::UnknownCommand, &why) {
                self.say(Level::Warn, &why);
            }
            return None;
        }
        if phase == Phase::Reporting {
            if !reply.refuse(RefusalKind::Reporting, crate::switch::REPORTING) {
                self.not_sent_while_reporting(text);
            }
            return None;
        }
        // An answer too long to send isn't sent, and its questions stay
        // open.
        if let Some(refusal) = self.reply_refused(text, &attached) {
            if !reply.refuse(RefusalKind::TooLong, &refusal) {
                self.say(Level::Warn, &refusal);
            }
            return None;
        }
        Some(Admitted::Message {
            text: text.to_string(),
            source,
            attached,
            reply,
        })
    }

    /// Forgets the images an app's message attached, when it isn't one
    /// that sends them.
    fn drop_images(&mut self, phase: Phase) {
        if phase != Phase::Reporting {
            self.pending_images.clear();
            self.pending_refs.clear();
        }
    }

    /// Runs the `command` the user sent, as the session runs it in `phase`,
    /// and accepts it once it ran. Returns whether the session is to end:
    /// the user quit, or a switch ended it.
    pub(crate) async fn run_command(
        &mut self,
        command: &str,
        source: Source,
        phase: Phase,
        input: &mut Inputs,
        mut reply: Reply,
        confirmed: bool,
    ) -> bool {
        if is_remote_command(command) {
            self.remote_command(command, source).await;
            reply.accept(Delivery::Command, None);
            return false;
        }
        if let Some(argument) = openrouter_words(command) {
            self.openrouter_live(command, argument, source).await;
            reply.accept(Delivery::Command, None);
            return false;
        }
        // A new conversation that carries this one over: it went once the
        // old model was asked for its report.
        if phase == Phase::Idle
            && let Some(switch) = self.switch_of(command)
        {
            let item = self.feed.user(command, source, Delivery::Command);
            reply.accept(Delivery::Command, Some(item));
            return Box::pin(self.switch(switch, source, input)).await;
        }
        // A message to a worker went once the worker has it.
        if let Some(("/message", argument)) = split_live_command(command) {
            self.message_command(argument, source, &mut reply, confirmed);
            return false;
        }
        if self.live_command(command, source) {
            reply.accept(Delivery::Command, None);
            return false;
        }
        let item = self.feed.user(command, source, Delivery::Command);
        if phase == Phase::Idle
            && let Some(argument) = conversations::resume_argument(command)
        {
            self.resume_command(argument).await;
            reply.accept(Delivery::Command, Some(item));
            return false;
        }
        let quit = self.mirrored(|harness| match command {
            "/exit" | "/quit" => harness.may_exit(Some(harness.jobs_shown.clone())),
            "/new" if phase == Phase::Idle => {
                harness.new_conversation();
                false
            }
            other if phase == Phase::Idle => {
                match crate::switch::new_argument(other) {
                    Some(Err(error)) => harness.output.line(&harness.output.error(&error)),
                    _ => harness.command(other),
                }
                false
            }
            other => {
                harness.command(other);
                false
            }
        });
        reply.accept(Delivery::Command, Some(item));
        quit
    }
}
