//! The full-screen view's state, and what each key, click and update does
//! to it. Drawing is `screen`'s; what reaches the session (a message, a
//! stop) and the terminal (a copy) comes out as [`Effect`]s, so all of it
//! runs in tests without a terminal.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::input::Editor;
use super::model_picker::{self, ModelPicker, Outcome};
use super::overlay::{Overlay, Picker, SettingsRow};
use super::prefs::Prefs;
use super::theme::{self, Catalog, Depth, Theme};
use super::transcript::{Filter, Transcript, View};
use crate::commands::{self, Runs};
use crate::model_command::Target;
use crate::remote::{Activity, Reading, Snapshot, Update};
use crate::ui::Input;

/// How long a second Ctrl+C (or a hint in the status line) counts.
const AGAIN: Duration = Duration::from_secs(3);
/// Lines a wheel notch scrolls.
const WHEEL: i64 = 3;
/// The theme picker previews the theme it is on once the selection has
/// rested this long: holding a key repaints nothing, and quick steps
/// repaint the screen less than 3 times a second (WCAG 2.3.1's threshold
/// for flashes).
pub const PREVIEW_REST: Duration = Duration::from_millis(350);
/// Clicks this close together, on one cell, are a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// The left button pressed in the transcript: where the press began in the
/// text, where the mouse is, and whether it moved (a drag selects text).
#[derive(Debug, Clone, Copy)]
struct Press {
    anchor: super::transcript::Point,
    at: (u16, u16),
    dragged: bool,
    /// A double or triple click chose what is highlighted (a word, a
    /// line): it stays so unless the mouse drags on.
    chosen: bool,
    /// The view followed the bottom when the button went down: a click
    /// (no drag) leaves it following.
    following: bool,
}

/// What the session is, for the status line, the title and the overlays.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// The working directory's name.
    pub workspace: String,
    /// The handover size the session started with, until the status says.
    pub handover_at: usize,
    /// The session's folder: the web page's key is there.
    pub session_dir: PathBuf,
    /// Where the view's choices are remembered.
    pub prefs: Option<PathBuf>,
}

/// What the view asks of the session and the terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// The session gets this, as the terminal's input.
    Input(Input),
    /// Copy `text`, which is `what` ("14 characters", "the last
    /// reply"): the status line says what became of it.
    Copy { text: String, what: String },
    /// Turn mouse capture on or off.
    Mouse(bool),
    /// Draw the whole screen again.
    Clear,
}

/// The command menu's state.
#[derive(Debug, Clone, Default)]
pub struct Menu {
    pub selected: usize,
    /// Esc closed it: it stays closed until the text changes.
    pub dismissed: Option<String>,
}

/// The view's state.
pub struct App {
    pub transcript: Transcript,
    pub editor: Editor,
    pub info: Info,
    pub catalog: Catalog,
    pub depth: Depth,
    pub theme: Theme,
    pub prefs: Prefs,
    pub view: View,
    /// The worker whose own transcript shows (the worker view).
    pub watching: Option<String>,
    pub activity: Activity,
    pub reading: Reading,
    /// When the turn running began.
    pub working_since: Option<Instant>,
    /// A stop was asked for the turn running.
    pub stopping: bool,
    /// The question put off with Esc (by its ID).
    pub put_off: Option<u64>,
    pub menu: Menu,
    pub overlay: Option<Overlay>,
    /// A word in the status line for a moment: "Copied", ...
    pub hint: Option<(String, Instant)>,
    last_interrupt: Option<Instant>,
    /// The terminal window has the focus (as far as it says).
    pub focused: bool,
    /// The terminal reports Shift+Enter apart from Enter.
    pub shift_enter: bool,
    pub mouse: bool,
    /// What the session got, waiting to be taken.
    effects: Vec<Effect>,
    /// The input box's width when last drawn, for ↑ and ↓.
    pub input_width: usize,
    /// The question answered from here (by its ID).
    answered: Option<u64>,
    /// The agent list has the focus, on this row (0 is the main agent).
    pub list_focus: Option<usize>,
    /// When the view started: the agent list shows the workers since.
    started_ms: i64,
    /// The left button, while it is down over the transcript.
    press: Option<Press>,
    /// The last click: when, where, and how many in a row.
    last_click: Option<(Instant, u16, u16, u8)>,
    /// Where the agent list was drawn, and the row of its first line: for
    /// the mouse.
    pub list_drawn: (ratatui::layout::Rect, usize),
    /// What a model needs that the session doesn't have, for `/model`'s
    /// picker (from the providers the session reaches, and Claude Code's
    /// login).
    pub model_needs: fn(&[String], &str) -> Option<String>,
}

impl App {
    pub fn new(
        snapshot: Snapshot,
        editor: Editor,
        info: Info,
        catalog: Catalog,
        depth: Depth,
    ) -> Self {
        let prefs = Prefs::load(info.prefs.as_deref());
        // FIN_THEME, else the theme chosen last, else the default.
        let theme_name = crate::credentials::var(theme::SETTING)
            .or_else(|| prefs.theme.clone())
            .unwrap_or_else(|| theme::DEFAULT.to_string());
        let theme = catalog.theme(&theme_name, depth);
        let view = prefs
            .view
            .as_deref()
            .and_then(View::parse)
            .unwrap_or(View::Full);
        let mouse = prefs.mouse.unwrap_or(true);
        let mut transcript = Transcript::new();
        let activity = snapshot.activity.clone();
        transcript.reset(snapshot);
        Self {
            transcript,
            editor,
            info,
            catalog,
            depth,
            theme,
            prefs,
            view,
            watching: None,
            working_since: activity.working.then(Instant::now),
            activity,
            reading: Reading::default(),
            stopping: false,
            put_off: None,
            menu: Menu::default(),
            overlay: None,
            hint: None,
            last_interrupt: None,
            focused: true,
            shift_enter: false,
            mouse,
            effects: Vec::new(),
            input_width: 80,
            answered: None,
            list_focus: None,
            started_ms: chrono::Utc::now().timestamp_millis(),
            list_drawn: Default::default(),
            press: None,
            last_click: None,
            model_needs: model_picker::needs,
        }
    }

    /// The session's figures now: the banner follows its models.
    pub fn set_reading(&mut self, reading: Reading) {
        if let Some(banner) = &mut self.transcript.banner
            && !reading.models.banner_model.is_empty()
        {
            banner.model = reading.models.banner_model.clone();
            banner.workers = reading.models.banner_workers.clone();
        }
        self.reading = reading;
    }

    /// `/model`'s picker for `target`. Once the conversation has started,
    /// `/model` (the main agent's) opens a note instead: the main agent
    /// keeps its model, and the workers' picker is a key away.
    pub fn open_model(&mut self, target: Target) {
        let models = self.reading.models.clone();
        if models.main.is_empty() {
            // (The session hasn't said its models yet: it answers.)
            self.send(match target {
                Target::Main => "/model",
                Target::Workers => "/model worker",
            });
            return;
        }
        if target == Target::Main && models.started {
            self.overlay = Some(Overlay::ModelNotice {
                text: format!(
                    "The main agent keeps {} ({} effort) for this conversation. /model worker sets the model for workers started from now on.",
                    models.main, models.main_effort
                ),
                workers: models.has_workers,
                choice: 0,
            });
            return;
        }
        if target == Target::Workers && !models.has_workers {
            self.say("This session has no workers");
            return;
        }
        let needs = self.model_needs;
        let reachable = models.reachable.clone();
        self.overlay = Some(Overlay::Model(ModelPicker::open(target, &models, |name| {
            needs(&reachable, name)
        })));
    }

    /// What the view asked for since last time.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn send(&mut self, text: &str) {
        self.effects
            .push(Effect::Input(Input::Line(text.to_string())));
    }

    /// Shows `text` in the status line for a moment.
    pub fn say(&mut self, text: impl Into<String>) {
        self.hint = Some((text.into(), Instant::now()));
    }

    /// The view's filter: the main transcript in its view, or a worker's.
    pub fn filter(&self) -> Filter {
        match &self.watching {
            Some(worker) => Filter::Worker(worker.clone()),
            None => Filter::Main(self.view),
        }
    }

    /// The question waiting for an answer, unless it was put off.
    pub fn question_shown(&self) -> Option<&crate::remote::Question> {
        let question = self.activity.question.as_ref()?;
        (self.put_off != Some(question.id)).then_some(question)
    }

    /// The commands the menu offers for what is typed: the box holds a
    /// command's name being typed.
    pub fn menu_items(&self) -> Vec<&'static commands::Command> {
        if self.overlay.is_some() {
            return Vec::new();
        }
        let Some(text) = self.editor.command_typed() else {
            return Vec::new();
        };
        if self.menu.dismissed.as_deref() == Some(text.as_str()) {
            return Vec::new();
        }
        commands::matching(&text, true)
    }

    // ----- updates from the session -----

    /// Takes the feed's `update`; `activity` is the feed's activity now.
    /// Returns whether the bell should sound.
    pub fn update(&mut self, update: Update, activity: impl FnOnce() -> Activity) -> bool {
        let bell = matches!(update, Update::Activity) && self.set_activity(activity());
        self.transcript.apply(update);
        bell
    }

    /// Starts over from the feed's `snapshot` (the view fell behind it).
    pub fn reset(&mut self, snapshot: Snapshot) {
        self.set_activity(snapshot.activity.clone());
        self.transcript.reset(snapshot);
    }

    /// The activity changed: returns whether the bell should sound (the
    /// agent asks, or finished, while the window isn't focused).
    pub fn set_activity(&mut self, activity: Activity) -> bool {
        let asked = activity.question.is_some()
            && activity.question.as_ref().map(|q| q.id)
                != self.activity.question.as_ref().map(|q| q.id);
        let finished = self.activity.working && !activity.working;
        if activity.working && !self.activity.working {
            self.working_since = Some(Instant::now());
        }
        if !activity.working {
            self.working_since = None;
            self.stopping = false;
        }
        if asked {
            self.put_off = None;
        }
        // Answered elsewhere (the web page) while an answer was typed here:
        // the text stays, as a message now.
        if let Some(question) = &self.activity.question
            && activity.question.is_none()
            && self.answered != Some(question.id)
            && !self.editor.is_empty()
        {
            self.say("The question was answered elsewhere. Your text is a message now");
        }
        self.activity = activity;
        !self.focused && (asked || finished)
    }

    // ----- keys -----

    /// A key. Enter's line breaks in a paste are the caller's (it knows
    /// when keys come together).
    pub fn key(&mut self, key: KeyEvent) {
        // Windows reports releases too.
        if key.kind == KeyEventKind::Release {
            return;
        }
        // A key press clears what the mouse highlighted.
        self.transcript.selection = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.overlay.is_some() {
            self.overlay_key(key);
            return;
        }
        if self.list_focus.is_some() && self.list_key(key) {
            return;
        }
        // Keys that work whatever is typed.
        match key.code {
            KeyCode::Char('c') if ctrl => return self.interrupt(),
            KeyCode::Char('o') if ctrl => return self.cycle_view(),
            KeyCode::Char('t') if ctrl => return self.focus_list(),
            KeyCode::Char('l') if ctrl => return self.effects.push(Effect::Clear),
            KeyCode::PageUp => return self.transcript.scroll(-self.page()),
            KeyCode::PageDown => return self.transcript.scroll(self.page()),
            KeyCode::Up if shift && !ctrl && !alt => return self.transcript.scroll(-WHEEL),
            KeyCode::Down if shift && !ctrl && !alt => return self.transcript.scroll(WHEEL),
            KeyCode::Home if ctrl => return self.transcript.top(),
            KeyCode::End if ctrl => return self.transcript.follow(),
            KeyCode::Home if self.editor.is_empty() => return self.transcript.top(),
            KeyCode::End if self.editor.is_empty() => return self.transcript.follow(),
            KeyCode::Esc => return self.escape(),
            _ => {}
        }
        let menu = self.menu_items();
        if !menu.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.menu.selected = (self.menu.selected + menu.len() - 1) % menu.len();
                    return;
                }
                KeyCode::Down => {
                    self.menu.selected = (self.menu.selected + 1) % menu.len();
                    return;
                }
                KeyCode::Tab => {
                    let command = menu[self.menu.selected.min(menu.len() - 1)];
                    self.complete(command);
                    return;
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let command = menu[self.menu.selected.min(menu.len() - 1)];
                    let typed = self.editor.shown();
                    // A command that takes arguments is completed first,
                    // unless it was typed whole.
                    if command.args.is_empty() || typed == command.name {
                        self.editor.set(command.name);
                        self.submit();
                    } else {
                        self.complete(command);
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter if shift || alt || ctrl => self.editor.newline(),
            KeyCode::Enter => self.submit(),
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('b') if ctrl => self.editor.left(),
            KeyCode::Char('f') if ctrl => self.editor.right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Char('d') if alt => self.editor.kill_word_forward(),
            KeyCode::Char('k') if ctrl => self.editor.kill_end(),
            KeyCode::Char('u') if ctrl => self.editor.kill_start(),
            KeyCode::Char('w') if ctrl => self.editor.kill_word(),
            KeyCode::Char('y') if ctrl => self.editor.yank(),
            KeyCode::Char('h') if ctrl => self.editor.backspace(),
            KeyCode::Char('d') if ctrl => {
                // As at the line prompt: an empty box quits (through /exit,
                // which warns about running workers).
                if self.editor.is_empty() {
                    self.send("/exit");
                } else {
                    self.editor.delete();
                }
            }
            KeyCode::Backspace if ctrl || alt => self.editor.kill_word(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => {
                self.editor.delete();
            }
            KeyCode::Left if ctrl || alt => self.editor.word_left(),
            KeyCode::Right if ctrl || alt => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Up => {
                if !self.editor.vertical(true, self.input_width) {
                    self.editor.history_back();
                }
            }
            KeyCode::Down => {
                // Past the last line: the history, else the agent list.
                if !self.editor.vertical(false, self.input_width)
                    && !self.editor.history_forward()
                    && !self.list().is_empty()
                {
                    self.list_focus = Some(0);
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => self.typed(c),
            _ => {}
        }
    }

    /// A character typed (or pasted as keys).
    pub fn typed(&mut self, c: char) {
        self.editor.insert(c);
        self.typing();
    }

    /// Text was typed or pasted: the question put off comes back to be
    /// answered, and the menu opens again.
    fn typing(&mut self) {
        self.menu.selected = 0;
        self.menu.dismissed = None;
        if !self.editor.starts_with('/') {
            self.put_off = None;
        }
    }

    /// A paste the terminal marked.
    pub fn paste(&mut self, text: &str) {
        if self.overlay.is_some() {
            return;
        }
        self.editor.paste(text);
        self.typing();
    }

    /// A line break typed into the box (Enter in a paste, Shift+Enter).
    pub fn newline(&mut self) {
        self.editor.newline();
        self.typing();
    }

    fn page(&self) -> i64 {
        (self.transcript.drawn.area.height as i64 - 2).max(1)
    }

    fn complete(&mut self, command: &commands::Command) {
        let text = if command.args.is_empty() {
            command.name.to_string()
        } else {
            format!("{} ", command.name)
        };
        self.editor.set(&text);
        self.menu.selected = 0;
    }

    /// Esc: closes what is open, nearest first.
    fn escape(&mut self) {
        if !self.menu_items().is_empty() {
            self.menu.dismissed = Some(self.editor.shown());
        } else if self.watching.is_some() {
            self.watching = None;
            self.transcript.follow();
        } else if let Some(question) = &self.activity.question
            && self.put_off != Some(question.id)
        {
            self.put_off = Some(question.id);
        } else if !self.transcript.following() {
            self.transcript.follow();
        }
    }

    /// Ctrl+C: stops the turn; else clears the box; else, pressed twice,
    /// quits. (It never copies: letting go of a drag does.)
    fn interrupt(&mut self) {
        if self.activity.working {
            if self.stopping {
                self.say("Stopping: the agents get up to 10 s to end");
            } else {
                self.stopping = true;
            }
            self.effects.push(Effect::Input(Input::Interrupt));
            return;
        }
        if !self.editor.is_empty() {
            self.editor.clear();
            return;
        }
        let now = Instant::now();
        if self
            .last_interrupt
            .is_some_and(|at| now.duration_since(at) < AGAIN)
        {
            self.last_interrupt = None;
            self.send("/exit");
        } else {
            self.last_interrupt = Some(now);
            self.say("Ctrl+C again quits");
        }
    }

    fn cycle_view(&mut self) {
        if self.watching.is_some() {
            self.say("Esc goes back to the main view first");
            return;
        }
        self.set_view(self.view.next());
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.prefs.view = Some(view.name().to_string());
        self.prefs.save(self.info.prefs.as_deref());
    }

    /// The agent list below the input: the main agent, then the workers
    /// of this run (running, or ended since the view started). Empty when
    /// there are none, so it takes no room.
    pub fn list(&self) -> Vec<Option<&crate::remote::WorkerSummary>> {
        let workers: Vec<Option<&crate::remote::WorkerSummary>> = self
            .transcript
            .workers()
            .iter()
            .filter(|worker| {
                worker.state.is_active()
                    || worker.ended_at.is_some_and(|at| at >= self.started_ms)
                    || worker.started_at.is_some_and(|at| at >= self.started_ms)
            })
            .map(Some)
            .collect();
        if workers.is_empty() {
            return Vec::new();
        }
        std::iter::once(None).chain(workers).collect()
    }

    /// Ctrl+T: the focus goes to the agent list, on the agent shown.
    fn focus_list(&mut self) {
        let list = self.list();
        if list.is_empty() {
            self.say("No workers yet");
            return;
        }
        let row = list
            .iter()
            .position(|row| row.map(|w| w.id.as_str()) == self.watching.as_deref())
            .unwrap_or(0);
        self.list_focus = Some(row);
    }

    /// A key while the agent list has the focus; false when it isn't the
    /// list's (it goes on as a key of the input box's, or everywhere's).
    fn list_key(&mut self, key: KeyEvent) -> bool {
        let list = self.list();
        let Some(row) = self.list_focus.filter(|_| !list.is_empty()) else {
            self.list_focus = None;
            return false;
        };
        let row = row.min(list.len() - 1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Above the main agent is the input box.
            KeyCode::Up if row == 0 => self.list_focus = None,
            KeyCode::Up => self.list_focus = Some(row - 1),
            KeyCode::Down => self.list_focus = Some((row + 1).min(list.len() - 1)),
            KeyCode::Char('t') if ctrl => self.list_focus = Some((row + 1) % list.len()),
            KeyCode::Enter => {
                let id = list[row].map(|w| w.id.clone());
                self.watch(id);
                self.list_focus = Some(row);
            }
            KeyCode::Esc => self.list_focus = None,
            // Keys that work everywhere (scrolling, Ctrl+C, the views) keep
            // the focus here; others (typing, editing) go to the input box.
            KeyCode::PageUp | KeyCode::PageDown => return false,
            KeyCode::Char('c' | 'o' | 'l') if ctrl => return false,
            _ => {
                self.list_focus = None;
                return false;
            }
        }
        true
    }

    /// Shows the worker `id`'s own transcript (`None`: the main one).
    pub fn watch(&mut self, id: Option<String>) {
        self.watching = id;
        self.transcript.selection = None;
        self.transcript.follow();
    }

    /// Enter: sends what is typed. The view's own commands run here; the
    /// rest goes to the session, which takes it as a line typed at the
    /// prompt (a note, a new turn, an answer, a command).
    fn submit(&mut self) {
        let text = self.editor.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Debug builds: what a crash of the view does (the terminal comes
        // back, and the session ends saved).
        #[cfg(debug_assertions)]
        if trimmed == "/__panic" {
            panic!("the full-screen view's test panic");
        }
        if let Some(command) = commands::find(trimmed) {
            let rest = trimmed
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest.trim());
            let handled = match (command.name, command.runs) {
                (_, Runs::Screen) => {
                    self.screen_command(command.name, rest);
                    true
                }
                ("/help", _) => {
                    self.overlay = Some(Overlay::Help { scroll: 0 });
                    true
                }
                // Without a model, the picker; with one, the session.
                ("/model", _) => match rest {
                    "" => {
                        self.open_model(Target::Main);
                        true
                    }
                    "worker" | "workers" => {
                        self.open_model(Target::Workers);
                        true
                    }
                    _ => false,
                },
                ("/remote", _) => {
                    // The page starts (or stops) in the session; its links
                    // show in an overlay that stays (`qr IFACE`: that one's).
                    if !matches!(rest, "off" | "new-key") {
                        let interface = rest.strip_prefix("qr").map(str::trim);
                        let link = interface
                            .filter(|name| !name.is_empty())
                            .and_then(|name| {
                                super::overlay::page_links(self)?
                                    .iter()
                                    .position(|link| link.interface.as_deref() == Some(name))
                            })
                            .unwrap_or(0);
                        self.overlay = Some(Overlay::Remote {
                            link,
                            since: Instant::now(),
                        });
                    }
                    false
                }
                _ => false,
            };
            self.editor.take();
            self.menu = Menu::default();
            if !handled {
                self.send(trimmed);
            }
            return;
        }
        let text = self.editor.take();
        self.menu = Menu::default();
        self.put_off = None;
        self.answered = self.activity.question.as_ref().map(|q| q.id);
        self.transcript.follow();
        self.send(text.trim_end());
    }

    fn screen_command(&mut self, name: &str, rest: &str) {
        match name {
            "/copy" => match self.transcript.last_reply() {
                Some(reply) => {
                    let text = reply.to_string();
                    self.effects.push(Effect::Copy {
                        text,
                        what: "the last reply".into(),
                    });
                }
                None => self.say("No reply to copy yet"),
            },
            "/view" => match View::parse(rest) {
                Some(view) => {
                    self.watching = None;
                    self.set_view(view);
                }
                None if rest.is_empty() => self.cycle_view(),
                None => self.say("Views: full, quiet, messages"),
            },
            "/watch" => match rest {
                "" => self.focus_list(),
                "main" => self.watch(None),
                id if self.transcript.worker(id).is_some() => self.watch(Some(id.to_string())),
                id => self.say(format!("No worker {id}")),
            },
            "/settings" => {
                self.overlay = Some(Overlay::Settings {
                    row: 0,
                    picker: None,
                })
            }
            _ => {}
        }
    }

    // ----- overlays -----

    fn overlay_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        let close = matches!(key.code, KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c'));
        self.overlay = match overlay {
            Overlay::Help { scroll } => match key.code {
                _ if close => None,
                KeyCode::Char('q') => None,
                KeyCode::Up => Some(Overlay::Help {
                    scroll: scroll.saturating_sub(1),
                }),
                KeyCode::Down => Some(Overlay::Help { scroll: scroll + 1 }),
                KeyCode::PageUp => Some(Overlay::Help {
                    scroll: scroll.saturating_sub(10),
                }),
                KeyCode::PageDown => Some(Overlay::Help {
                    scroll: scroll + 10,
                }),
                _ => Some(Overlay::Help { scroll }),
            },
            Overlay::Remote { link, since } => match key.code {
                _ if close => None,
                KeyCode::Left => Some(Overlay::Remote {
                    link: link.saturating_sub(1),
                    since,
                }),
                KeyCode::Right => Some(Overlay::Remote {
                    link: link + 1,
                    since,
                }),
                _ => Some(Overlay::Remote { link, since }),
            },
            Overlay::Settings { row, picker } => self.settings_key(key, row, picker, close),
            Overlay::Model(mut picker) => match picker.key(key, close) {
                Outcome::Stay => Some(Overlay::Model(picker)),
                Outcome::Close => None,
                Outcome::Apply { line, said } => {
                    self.send(&line);
                    self.say(said);
                    None
                }
            },
            Overlay::ModelNotice {
                text,
                workers,
                choice,
            } => match key.code {
                _ if close => None,
                KeyCode::Left | KeyCode::Right | KeyCode::Tab if workers => {
                    Some(Overlay::ModelNotice {
                        text,
                        workers,
                        choice: 1 - choice,
                    })
                }
                KeyCode::Enter if workers && choice == 0 => {
                    self.open_model(Target::Workers);
                    return;
                }
                KeyCode::Enter => None,
                _ => Some(Overlay::ModelNotice {
                    text,
                    workers,
                    choice,
                }),
            },
        };
    }

    /// The settings' rows, as this session has them.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        let mut rows = vec![
            SettingsRow::View,
            SettingsRow::Theme,
            SettingsRow::Previews,
            SettingsRow::Mouse,
        ];
        if commands::find("/outside on").is_some() {
            rows.push(SettingsRow::Outside);
        }
        if commands::find("/handover-at 200k").is_some() {
            rows.push(SettingsRow::Handover);
        }
        rows.push(SettingsRow::Page);
        rows
    }

    fn settings_key(
        &mut self,
        key: KeyEvent,
        row: usize,
        picker: Option<Picker>,
        close: bool,
    ) -> Option<Overlay> {
        if let Some(mut picker) = picker {
            if let Some(choice) = picker.notice {
                // The notice about flashing, over the picker: it is
                // answered (and so never shown again) with Enter.
                match key.code {
                    _ if close => return Some(Overlay::Settings { row, picker: None }),
                    KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                        picker.notice = Some(1 - choice);
                    }
                    KeyCode::Enter => {
                        picker.notice = None;
                        self.prefs.flash_notice = true;
                        self.prefs.theme_previews = Some(choice == 0);
                        self.prefs.save(self.info.prefs.as_deref());
                    }
                    _ => {}
                }
                return Some(Overlay::Settings {
                    row,
                    picker: Some(picker),
                });
            }
            // The theme picker: the theme it rests on is previewed (see
            // `preview`). Enter keeps it at once, Esc goes back at once.
            let before = picker.selected;
            match key.code {
                _ if close => {
                    self.theme = self.catalog.theme(&picker.before, self.depth);
                    return Some(Overlay::Settings { row, picker: None });
                }
                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1).min(picker.names.len() - 1)
                }
                KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
                KeyCode::PageDown => {
                    picker.selected = (picker.selected + 10).min(picker.names.len() - 1)
                }
                KeyCode::Enter => {
                    let name = picker.names[picker.selected].1.clone();
                    self.theme = self.catalog.theme(&name, self.depth);
                    self.prefs.theme = Some(name);
                    self.prefs.save(self.info.prefs.as_deref());
                    return Some(Overlay::Settings { row, picker: None });
                }
                _ => {}
            }
            // Only a move starts the wait: keys that don't move it (at the
            // list's end) leave the selection resting.
            if picker.selected != before && self.previews() {
                picker.moved = Some(Instant::now());
            }
            return Some(Overlay::Settings {
                row,
                picker: Some(picker),
            });
        }
        let rows = self.settings_rows();
        let row = row.min(rows.len() - 1);
        let step: i32 = match key.code {
            _ if close => return None,
            KeyCode::Up => {
                return Some(Overlay::Settings {
                    row: row.saturating_sub(1),
                    picker: None,
                });
            }
            KeyCode::Down | KeyCode::Tab => {
                return Some(Overlay::Settings {
                    row: (row + 1).min(rows.len() - 1),
                    picker: None,
                });
            }
            KeyCode::Left => -1,
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => 1,
            _ => 0,
        };
        if step != 0 {
            match rows[row] {
                SettingsRow::View => {
                    let index = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
                    let next = (index as i32 + step).rem_euclid(View::ALL.len() as i32);
                    self.set_view(View::ALL[next as usize]);
                }
                SettingsRow::Theme => {
                    if self.depth == Depth::Ansi16 {
                        self.say("This terminal shows 16 colours: themes need more");
                    } else {
                        let names = self.catalog.names();
                        let selected = names
                            .iter()
                            .position(|(_, name)| name.eq_ignore_ascii_case(&self.theme.name))
                            .unwrap_or(0);
                        return Some(Overlay::Settings {
                            row,
                            picker: Some(Picker {
                                names,
                                selected,
                                before: self.theme.name.clone(),
                                // The first time: the notice about
                                // flashing, before any preview.
                                notice: (!self.prefs.flash_notice).then_some(0),
                                moved: None,
                            }),
                        });
                    }
                }
                SettingsRow::Previews => {
                    self.prefs.theme_previews = Some(!self.previews());
                    self.prefs.flash_notice = true;
                    self.prefs.save(self.info.prefs.as_deref());
                }
                SettingsRow::Mouse => {
                    self.mouse = !self.mouse;
                    self.prefs.mouse = Some(self.mouse);
                    self.prefs.save(self.info.prefs.as_deref());
                    self.effects.push(Effect::Mouse(self.mouse));
                }
                SettingsRow::Outside => {
                    let on = !self.outside();
                    self.send(if on { "/outside on" } else { "/outside off" });
                }
                SettingsRow::Handover => {
                    // The next round size down or up (the session may hold
                    // it under the model's limit; the status says).
                    let now = self.handover_at();
                    let steps = super::overlay::HANDOVER_STEPS;
                    let next = if step < 0 {
                        steps.iter().rev().find(|size| **size < now)
                    } else {
                        steps.iter().find(|size| **size > now)
                    };
                    if let Some(size) = next {
                        self.send(&format!("/handover-at {}k", size / 1000));
                    }
                }
                SettingsRow::Page => {
                    if super::page().is_some() {
                        self.send("/remote off");
                    } else {
                        self.send("/remote");
                    }
                }
            }
        }
        Some(Overlay::Settings { row, picker: None })
    }

    /// Whether the theme picker previews the theme it rests on.
    pub fn previews(&self) -> bool {
        self.prefs.theme_previews.unwrap_or(true)
    }

    /// When the picker's waiting preview is due: the runner wakes then.
    pub fn preview_due(&self) -> Option<Instant> {
        match &self.overlay {
            Some(Overlay::Settings {
                picker: Some(picker),
                ..
            }) => picker.moved.map(|moved| moved + PREVIEW_REST),
            _ => None,
        }
    }

    /// Shows the theme the picker rests on, once it has rested (`now`
    /// past its due time): returns whether the screen changed.
    pub fn preview(&mut self, now: Instant) -> bool {
        let Some(Overlay::Settings {
            picker: Some(picker),
            ..
        }) = &mut self.overlay
        else {
            return false;
        };
        match picker.moved {
            Some(moved) if now >= moved + PREVIEW_REST => {
                picker.moved = None;
                let name = picker.names[picker.selected].1.clone();
                self.theme = self.catalog.theme(&name, self.depth);
                true
            }
            _ => false,
        }
    }

    /// Whether the agents may work outside the workspace now.
    pub fn outside(&self) -> bool {
        self.reading.settings.outside
    }

    /// The handover size now.
    pub fn handover_at(&self) -> usize {
        match self.reading.settings.handover_at {
            0 => self.info.handover_at,
            size => size,
        }
    }

    // ----- the mouse -----

    /// The mouse: the wheel scrolls; a drag over the transcript highlights
    /// text, copied when the button is let go (a double click a word, a
    /// triple click a line); a click (no drag) opens what it is on.
    pub fn mouse(&mut self, event: MouseEvent) {
        let (x, y) = (event.column, event.row);
        let at = ratatui::layout::Position { x, y };
        match event.kind {
            // Over the help, the wheel scrolls it.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if matches!(self.overlay, Some(Overlay::Help { .. })) =>
            {
                if let Some(Overlay::Help { scroll }) = &mut self.overlay {
                    *scroll = if event.kind == MouseEventKind::ScrollUp {
                        scroll.saturating_sub(WHEEL as usize)
                    } else {
                        *scroll + WHEEL as usize
                    };
                }
            }
            // (While a drag selects, the text under the mouse is read again
            // once the screen is drawn: the selection grows as it scrolls.)
            MouseEventKind::ScrollUp => self.transcript.scroll(-WHEEL),
            MouseEventKind::ScrollDown => self.transcript.scroll(WHEEL),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.overlay.is_some() {
                    return;
                }
                // A click clears what was highlighted.
                self.transcript.selection = None;
                let now = Instant::now();
                let count = match self.last_click {
                    Some((when, cx, cy, count))
                        if now.duration_since(when) < MULTI_CLICK && (cx, cy) == (x, y) =>
                    {
                        count % 3 + 1
                    }
                    _ => 1,
                };
                self.last_click = Some((now, x, y, count));
                let on_marker = self
                    .transcript
                    .drawn
                    .marker
                    .is_some_and(|rect| rect.contains(at));
                if on_marker || !self.transcript.drawn.area.contains(at) {
                    return;
                }
                let Some(point) = self.transcript.point_at(x, y) else {
                    return;
                };
                let filter = self.filter();
                let theme = self.theme.clone();
                let span = match count {
                    2 => self.transcript.word_at(point, &filter, &theme),
                    3 => self.transcript.line_at(point, &filter, &theme),
                    _ => None,
                };
                // The text under the mouse holds still from the press: what
                // the press found is what a drag starts from.
                let following = self.transcript.following();
                self.transcript.hold();
                self.press = Some(match span {
                    Some((from, to)) => {
                        self.transcript.select(from, to, &filter);
                        Press {
                            anchor: from,
                            at: (x, y),
                            dragged: true,
                            chosen: true,
                            following,
                        }
                    }
                    None => Press {
                        anchor: point,
                        at: (x, y),
                        dragged: false,
                        chosen: false,
                        following,
                    },
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let filter = self.filter();
                let Some(press) = &mut self.press else {
                    return;
                };
                if press.at == (x, y) {
                    return;
                }
                press.dragged = true;
                press.chosen = false;
                press.at = (x, y);
                let anchor = press.anchor;
                if let Some(head) = self.transcript.point_at(x, y) {
                    self.transcript.select(anchor, head, &filter);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.transcript.release();
                match self.press.take() {
                    Some(press) if press.dragged => self.copy_selection(),
                    // A click (no drag) never copies: it opens what it is on.
                    press => {
                        if press.is_some_and(|press| press.following) {
                            self.transcript.follow();
                        }
                        self.click(at);
                    }
                }
            }
            _ => {}
        }
    }

    /// After a frame: a drag's end follows the text now under the mouse
    /// (the wheel may have scrolled it).
    pub fn after_draw(&mut self) {
        let filter = self.filter();
        if let Some(press) = &self.press
            && press.dragged
            && !press.chosen
            && self.transcript.selection.is_some()
            && let Some(head) = self.transcript.point_at(press.at.0, press.at.1)
        {
            let anchor = press.anchor;
            self.transcript.select(anchor, head, &filter);
        }
    }

    /// Copies what is highlighted, and says how much.
    fn copy_selection(&mut self) {
        let filter = self.filter();
        let theme = self.theme.clone();
        let Some(text) = self.transcript.selected_text(&filter, &theme) else {
            return;
        };
        let lines = text.lines().count();
        let what = if lines > 1 {
            format!("{lines} lines")
        } else {
            let count = text.chars().count();
            format!("{count} character{}", if count == 1 { "" } else { "s" })
        };
        self.effects.push(Effect::Copy { text, what });
    }

    /// A click (no drag) at `at`: a row of the agent list shows that
    /// agent, the "new" marker goes to the end, a turn's counted calls
    /// open in the Full view, and a worker's line opens its transcript.
    /// Elsewhere it does nothing.
    fn click(&mut self, at: ratatui::layout::Position) {
        if self.overlay.is_some() {
            return;
        }
        let (list, first) = self.list_drawn;
        if list.contains(at) {
            let row = first + (at.y - list.y) as usize;
            if let Some(agent) = self.list().get(row) {
                let id = agent.map(|w| w.id.clone());
                self.watch(id);
            }
            return;
        }
        if self
            .transcript
            .drawn
            .marker
            .is_some_and(|rect| rect.contains(at))
        {
            self.transcript.follow();
            return;
        }
        let Some(key) = self.transcript.drawn.key_at(at.x, at.y) else {
            return;
        };
        if self.transcript.is_counter(key) {
            self.set_view(View::Full);
            self.transcript.show(key);
            return;
        }
        if let Some(item) = self.transcript.item(key)
            && let Some(worker) = item.worker.clone()
            && self.watching.is_none()
        {
            self.watch(Some(worker));
        }
    }

    /// Whether the status line's hint is still fresh.
    pub fn hint(&self) -> Option<&str> {
        self.hint
            .as_ref()
            .filter(|(_, at)| at.elapsed() < AGAIN)
            .map(|(text, _)| text.as_str())
    }

    /// The terminal's title: the state and the folder.
    pub fn title(&self) -> String {
        let state = if self.activity.question.is_some() {
            "asks you"
        } else if self.activity.working {
            "working"
        } else {
            "idle"
        };
        format!("fin · {state} · {}", self.info.workspace)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::remote::{Item, Kind, Question};
    use crate::tui::input::History;

    pub fn app() -> App {
        let info = Info {
            workspace: "work".into(),
            handover_at: 250_000,
            ..Info::default()
        };
        App::new(
            Snapshot::default(),
            Editor::new(History::load(None)),
            info,
            Catalog::load(None),
            Depth::Rgb,
        )
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.key(KeyEvent::new(code, modifiers));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    fn sent(app: &mut App) -> Vec<String> {
        app.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Input(Input::Line(line)) => Some(line),
                Effect::Input(Input::Interrupt) => Some("^C".into()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn enter_sends_and_the_other_enters_break_the_line() {
        let mut app = app();
        type_text(&mut app, "one");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_text(&mut app, "two");
        press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        type_text(&mut app, "three");
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["one\ntwo\nthree"]);
        assert!(app.editor.is_empty());
    }

    #[test]
    fn ctrl_c_stops_clears_and_quits_twice() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            question: None,
        });
        type_text(&mut app, "keep");
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(sent(&mut app), ["^C"]);
        assert_eq!(app.editor.text(), "keep");
        app.set_activity(Activity::default());
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.editor.is_empty());
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(sent(&mut app).is_empty());
        assert_eq!(app.hint(), Some("Ctrl+C again quits"));
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(sent(&mut app), ["/exit"]);
        // Ctrl+D on an empty box quits the same way.
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(sent(&mut app), ["/exit"]);
    }

    #[test]
    fn the_command_menu_filters_completes_and_runs() {
        let mut app = app();
        type_text(&mut app, "/se");
        let names: Vec<&str> = app.menu_items().iter().map(|c| c.name).collect();
        assert_eq!(names, ["/settings", "/session"]);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/session");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/session"]);
        // A command with arguments is completed, then sent as typed.
        type_text(&mut app, "/rem");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/remote ");
        type_text(&mut app, "off");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/remote off"]);
        // Esc closes the menu; typing opens it again.
        type_text(&mut app, "/c");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.menu_items().is_empty());
        type_text(&mut app, "o");
        assert_eq!(app.menu_items()[0].name, "/cost");
        // The view's own commands don't reach the session.
        app.editor.clear();
        type_text(&mut app, "/view quiet");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(sent(&mut app).is_empty());
        assert_eq!(app.view, View::Quiet);
    }

    #[test]
    fn a_question_docks_esc_puts_it_off_and_typing_brings_it_back() {
        let mut app = app();
        let question = Question {
            id: 4,
            text: "Tabs or spaces?".into(),
        };
        app.focused = false;
        assert!(app.set_activity(Activity {
            working: true,
            question: Some(question.clone()),
        }));
        assert_eq!(app.question_shown(), Some(&question));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.question_shown(), None);
        assert_eq!(app.title(), "fin · asks you · work");
        type_text(&mut app, "spaces");
        assert_eq!(app.question_shown(), Some(&question));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["spaces"]);
    }

    #[test]
    fn views_cycle_and_workers_open_their_transcripts() {
        let mut app = app();
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.view, View::Quiet);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.list_focus, None);
        assert_eq!(app.hint(), Some("No workers yet"));
        let mut line = Item::new(Kind::WorkerLine, "Test it");
        line.id = 1;
        line.worker = Some("r1".into());
        app.update(Update::Item(line), Activity::default);
        app.update(
            Update::Worker(crate::remote::WorkerSummary {
                id: "r1".into(),
                title: "Test it".into(),
                state: crate::remote::WorkerState::Running,
                tool_calls: 0,
                started_at: Some(chrono::Utc::now().timestamp_millis()),
                ended_at: None,
                last: None,
                report: None,
            }),
            Activity::default,
        );
        // ↓ from the box goes into the list, Enter shows the worker's
        // transcript, typing goes back to the box, ↑ on main leaves.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.list_focus, Some(0));
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.watching.as_deref(), Some("r1"));
        assert_eq!(app.filter(), Filter::Worker("r1".into()));
        assert_eq!(app.list_focus, Some(1));
        type_text(&mut app, "x");
        assert_eq!(app.list_focus, None);
        assert_eq!(app.editor.text(), "x");
        app.editor.clear();
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.list_focus, Some(1));
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.list_focus, None);
        // Esc in the box leaves the worker's view.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.watching, None);
        // A click on a row of the list (pressed and let go) shows that
        // agent, and copies nothing.
        app.list_drawn = (ratatui::layout::Rect::new(0, 20, 80, 2), 0);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.mouse(MouseEvent {
                kind,
                column: 5,
                row: 21,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(app.watching.as_deref(), Some("r1"));
        assert!(app.take_effects().is_empty());
    }

    #[test]
    fn the_flash_notice_is_answered_once_and_no_previews_holds() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.json");
        let session = || {
            let info = Info {
                prefs: Some(path.clone()),
                ..Info::default()
            };
            App::new(
                Snapshot::default(),
                Editor::new(History::load(None)),
                info,
                Catalog::load(None),
                Depth::Rgb,
            )
        };
        let open = |app: &mut App| {
            app.editor.set("/settings");
            press(app, KeyCode::Enter, KeyModifiers::NONE);
            press(app, KeyCode::Down, KeyModifiers::NONE);
            press(app, KeyCode::Enter, KeyModifiers::NONE);
        };
        let notice = |app: &App| match &app.overlay {
            Some(Overlay::Settings {
                picker: Some(picker),
                ..
            }) => picker.notice,
            _ => None,
        };
        let mut app = session();
        open(&mut app);
        assert_eq!(notice(&app), Some(0));
        // Esc backs out of the notice and the picker, and the notice
        // comes again next time.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Settings { picker: None, .. })
        ));
        assert!(!Prefs::load(Some(&path)).flash_notice);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(notice(&app), Some(0));
        // No previews: kept in tui.json with the notice answered.
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(notice(&app), Some(1));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let saved = Prefs::load(Some(&path));
        assert!(saved.flash_notice);
        assert_eq!(saved.theme_previews, Some(false));
        // A later session opens the picker without the notice, and moving
        // changes nothing.
        let mut app = session();
        let before = app.theme.name.clone();
        open(&mut app);
        assert_eq!(notice(&app), None);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.preview_due(), None);
        assert_eq!(app.theme.name, before);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.theme.name, before);
        // The Previews row turns them back on.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.previews());
        assert_eq!(Prefs::load(Some(&path)).theme_previews, Some(true));
    }

    #[test]
    fn the_theme_picker_previews_and_esc_goes_back() {
        let mut app = app();
        let dark = "macOS Classic Dark";
        assert_eq!(app.theme.name, dark);
        let notice = |app: &App| match &app.overlay {
            Some(Overlay::Settings {
                picker: Some(picker),
                ..
            }) => picker.notice,
            _ => None,
        };
        app.editor.set("/settings");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        // The first time, the notice about flashing comes first, and keys
        // choose in it: nothing is previewed.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(notice(&app), Some(0));
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(notice(&app), Some(0));
        assert_eq!(app.theme.name, dark);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(notice(&app), None);
        assert!(app.prefs.flash_notice && app.previews());
        // A move previews once the selection has rested, not before.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let now = Instant::now();
        assert!(!app.preview(now));
        assert_eq!(app.theme.name, dark);
        assert!(app.preview(now + PREVIEW_REST));
        assert_ne!(app.theme.name, dark);
        // Esc goes back at once.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.theme.name, dark);
        // The second time there is no notice. Enter keeps a theme at once.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(notice(&app), None);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let chosen = app.theme.name.clone();
        assert_ne!(chosen, dark);
        assert_eq!(app.prefs.theme.as_deref(), Some(chosen.as_str()));
        // With previews off, moving changes nothing until Enter.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert!(!app.previews());
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.preview(Instant::now() + PREVIEW_REST * 4));
        assert_eq!(app.theme.name, chosen);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_ne!(app.theme.name, chosen);
        // The handover size steps to the next round size, as a command.
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::Handover)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/handover-at 300k", "/handover-at 200k"]);
    }

    /// The session's models, as it publishes them: luna for both agents,
    /// the conversation not started, OpenAI reachable with an API key.
    fn models(started: bool) -> crate::remote::Models {
        crate::remote::Models {
            main: "gpt-6-luna".into(),
            main_effort: "high".into(),
            workers: "gpt-6-luna".into(),
            workers_effort: "high".into(),
            workers_follow: true,
            started,
            banner_model: "gpt-6-luna (high effort) via OpenAI API key".into(),
            banner_workers: "parallel workers".into(),
            reachable: vec!["openai".into()],
            openai_auth: Some("OpenAI API key".into()),
            has_workers: true,
        }
    }

    /// A picker that doesn't ask Claude Code (whose login differs from one
    /// machine to the next): it needs signing in.
    fn hermetic(app: &mut App) {
        app.model_needs = |reachable, name| {
            if name.starts_with("cc:") {
                Some("Claude Code signed in".into())
            } else {
                model_picker::needs(reachable, name)
            }
        };
    }

    fn picker(app: &App) -> &ModelPicker {
        match &app.overlay {
            Some(Overlay::Model(picker)) => picker,
            other => panic!("no picker: {other:?}"),
        }
    }

    /// `/model` before the start: the models, on the one now, then its
    /// efforts with Back, and Enter sends the line and says it.
    #[test]
    fn the_model_picker_chooses_a_model_then_an_effort() {
        let mut app = app();
        hermetic(&mut app);
        app.set_reading(Reading {
            models: models(false),
            ..Reading::default()
        });
        type_text(&mut app, "/model");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let open = picker(&app);
        assert_eq!(open.title(), "Main agent's model");
        assert!(
            matches!(&open.rows[open.selected], model_picker::Row::Model { name, .. } if name == "gpt-6-luna")
        );
        // The ways that can't be used are one row each, not chosen.
        assert!(open.rows.iter().any(|row| matches!(row, model_picker::Row::Unavailable { needs, .. } if needs == "GEMINI_API_KEY")));
        assert!(sent(&mut app).is_empty());
        // Up to sol, then its efforts, on the effort now.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let efforts = picker(&app);
        assert_eq!(efforts.title(), "Main agent's effort on gpt-6.1-sol");
        assert!(
            matches!(&efforts.step, model_picker::Step::Efforts { efforts, selected, .. } if efforts.len() == 5 && *selected == 2)
        );
        // Back, and Esc, go to the models.
        for _ in 0..3 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(picker(&app).step, model_picker::Step::Models);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(picker(&app).step, model_picker::Step::Models);
        // Astra at xhigh.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        assert_eq!(sent(&mut app), ["/model gpt-6-astra xhigh"]);
        assert_eq!(
            app.hint.as_ref().map(|(text, _)| text.as_str()),
            Some("The main agent runs gpt-6-astra at xhigh effort")
        );
        // With a model, the line goes to the session as typed.
        type_text(&mut app, "/model sol high");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/model sol high"]);
    }

    /// Once the conversation has started, `/model` says the main agent
    /// keeps its model and offers the workers' picker, which can make them
    /// follow the main agent or take a model typed.
    #[test]
    fn after_the_start_model_offers_the_workers_picker() {
        let mut app = app();
        hermetic(&mut app);
        app.set_reading(Reading {
            models: models(true),
            ..Reading::default()
        });
        type_text(&mut app, "/model");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(&app.overlay, Some(Overlay::ModelNotice { text, workers: true, choice: 0 }) if text.starts_with("The main agent keeps gpt-6-luna (high effort) for this conversation."))
        );
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let workers = picker(&app);
        assert_eq!(workers.title(), "Model of workers started from now on");
        assert!(matches!(
            workers.rows[workers.selected],
            model_picker::Row::FollowMain { .. }
        ));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/model worker main"]);

        // A model typed: one fin doesn't know says what was meant, one it
        // can't reach what it needs.
        type_text(&mut app, "/model worker");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "opsu");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(&picker(&app).step, model_picker::Step::Typing { error: Some(error), .. } if error.contains("Did you mean opus?"))
        );
        for _ in 0..4 {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
        type_text(&mut app, "fable");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(&picker(&app).step, model_picker::Step::Typing { error: Some(error), .. } if error == "claude-fable-5-1 needs ANTHROPIC_API_KEY, which fin reads as it starts, from the environment or a .env file.")
        );
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
        type_text(&mut app, "gpt-6-astra");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/model worker gpt-6-astra high"]);
        assert_eq!(
            app.hint.as_ref().map(|(text, _)| text.as_str()),
            Some("Workers started from now on run gpt-6-astra at high effort")
        );
    }

    /// The banner's Model row follows the session's.
    #[test]
    fn the_banner_follows_the_models() {
        let mut app = app();
        app.transcript.banner = Some(crate::tui::Banner {
            model: "gpt-6-luna (high effort)".into(),
            workers: "parallel workers".into(),
            workspace: "/work".into(),
            session: "s".into(),
            project: None,
            tools: Vec::new(),
            notes: Vec::new(),
        });
        let mut reading = Reading::default();
        app.set_reading(reading.clone());
        assert_eq!(
            app.transcript.banner.as_ref().unwrap().model,
            "gpt-6-luna (high effort)"
        );
        reading.models.banner_model = "gpt-6.1-sol (high effort)".into();
        reading.models.banner_workers = "parallel workers on gpt-6-astra (high effort)".into();
        app.set_reading(reading);
        let banner = app.transcript.banner.as_ref().unwrap();
        assert_eq!(banner.model, "gpt-6.1-sol (high effort)");
        assert_eq!(
            banner.workers,
            "parallel workers on gpt-6-astra (high effort)"
        );
    }
}
