//! What the user sends, read the same way whatever the session is doing: a
//! line typed at the prompt, an app's message or answer, or the web page's.
//! An idle session, a turn and the old model's report for a switch read it
//! with [`Harness::read_sent`], check it once with [`Harness::admit`] before
//! anything happens, and run a command with [`Harness::run_command`].

use crate::conversations;
use crate::harness::{Harness, command, is_remote_command, openrouter_words};
use crate::inputs::{Event, Inputs};
use crate::remote::{Action, Delivery, Level, Source};
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
}

/// A send that passed the checks.
pub(crate) enum Admitted {
    /// A command, trimmed.
    Command { command: String, source: Source },
    /// A message, trimmed, with the open questions it answers.
    Message {
        text: String,
        source: Source,
        attached: Vec<String>,
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
        input: &mut Inputs,
        phase: Phase,
    ) -> Read {
        // (While the report is written, nothing an answer says goes, so its
        // card isn't looked at.)
        let reporting = phase == Phase::Reporting;
        let sent = |line: String, source: Source, attached: Vec<String>| {
            Read::Sent(Sent {
                line,
                source,
                attached,
                images: None,
            })
        };
        match event {
            Event::Input(Some(Input::Line(line))) => {
                let line = input.coalesce(line).await;
                sent(line, self.input_source, Vec::new())
            }
            Event::Input(Some(Input::Message(message))) => {
                if message.continue_uncached {
                    self.confirm_continue(&message.text);
                }
                Read::Sent(Sent {
                    line: message.text,
                    source: self.input_source,
                    attached: message.answers,
                    images: Some(message.images),
                })
            }
            Event::Input(Some(Input::Answer { question, text })) => {
                let attached = match reporting {
                    true => Vec::new(),
                    false => match self.card_answered(question) {
                        Some(attached) => attached,
                        None => return Read::Done,
                    },
                };
                sent(text, self.input_source, attached)
            }
            Event::Remote(Action::Message(text)) => {
                self.echo_web(&text);
                sent(text, Source::Web, Vec::new())
            }
            Event::Remote(Action::Continue(text)) => {
                self.echo_web(&text);
                self.confirm_continue(&text);
                sent(text, Source::Web, Vec::new())
            }
            Event::Remote(Action::Reply { text, answers }) => {
                self.echo_web(&text);
                sent(text, Source::Web, answers)
            }
            Event::Remote(Action::Answer { question, text }) => {
                let attached = match reporting {
                    true => Vec::new(),
                    false => match self.card_answered(question) {
                        Some(attached) => attached,
                        None => return Read::Done,
                    },
                };
                self.echo_web(&text);
                sent(text, Source::Web, attached)
            }
            _ => Read::Other,
        }
    }

    /// Checks what the user sent before anything of it happens, and tells
    /// the user why when it can't go: an app's images the model can't take,
    /// a command the web page can't run or one that doesn't exist, a
    /// message while the old model writes its report, and answers too long
    /// to send. An app's images are attached for its message, and a
    /// command drops them.
    pub(crate) fn admit(&mut self, sent: Sent, phase: Phase) -> Option<Admitted> {
        let Sent {
            line,
            source,
            attached,
            images,
        } = sent;
        // (While the report is written, an app's message doesn't go, so its
        // images aren't attached.)
        let reported_images = match images {
            Some(images) if phase != Phase::Reporting => {
                if let Err(why) = self.attach(images) {
                    self.say(Level::Error, &why);
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
            return None;
        }
        self.user_present();
        if let Some(command) = command(text) {
            // (Images go with messages only.)
            self.drop_images(phase);
            if self.refused(command, source) {
                return None;
            }
            return Some(Admitted::Command {
                command: command.to_string(),
                source,
            });
        }
        if self.unknown_command(&line, source) {
            self.drop_images(phase);
            return None;
        }
        if phase == Phase::Reporting {
            self.not_sent_while_reporting(text);
            return None;
        }
        // An answer too long to send isn't sent, and its questions stay
        // open.
        if let Some(refusal) = self.reply_refused(text, &attached) {
            self.say(Level::Warn, &refusal);
            return None;
        }
        Some(Admitted::Message {
            text: text.to_string(),
            source,
            attached,
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

    /// Runs the `command` the user sent, as the session runs it in `phase`.
    /// Returns whether the session is to end: the user quit, or a switch
    /// ended it.
    pub(crate) async fn run_command(
        &mut self,
        command: &str,
        source: Source,
        phase: Phase,
        input: &mut Inputs,
    ) -> bool {
        if is_remote_command(command) {
            self.remote_command(command, source).await;
            return false;
        }
        if let Some(argument) = openrouter_words(command) {
            self.openrouter_live(command, argument, source).await;
            return false;
        }
        // A new conversation that carries this one over.
        if phase == Phase::Idle
            && let Some(switch) = self.switch_of(command)
        {
            self.feed.user(command, source, Delivery::Command);
            return Box::pin(self.switch(switch, source, input)).await;
        }
        if self.live_command(command, source) {
            return false;
        }
        self.feed.user(command, source, Delivery::Command);
        if phase == Phase::Idle
            && let Some(argument) = conversations::resume_argument(command)
        {
            self.resume_command(argument).await;
            return false;
        }
        self.mirrored(|harness| match command {
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
        })
    }
}
