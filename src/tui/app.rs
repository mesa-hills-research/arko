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
use super::keys_menu::{self, KeysMenu};
use super::model_picker::{self, ModelPicker, Outcome};
use super::names_menu::{self, NamesMenu};
use super::openrouter_menu::OpenRouterMenu;
use super::overlay::{Overlay, Picker, SettingsRow};
use super::prefs::Prefs;
use super::resume_picker::{self, ResumePicker};
use super::select::{Docked, Spot};
use super::theme::{self, Catalog, Depth, Theme};
use super::transcript::{Filter, Transcript, View};
use crate::agents::ExtraRules;
use crate::commands::{self, Runs};
use crate::credentials::{Key, KeyState};
use crate::keys::Secret;
use crate::model_command::Target;
use crate::project;
use crate::remote::{Activity, Reading, Snapshot, Update};
use crate::session_names::Way;
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

/// The left button pressed on the answer panel's text: where the press
/// began in it, where the mouse is, and whether it highlighted text (a
/// drag, or a double or triple click), which is copied when the button is
/// let go.
#[derive(Debug, Clone, Copy)]
struct DockPress {
    anchor: Spot,
    at: (u16, u16),
    dragged: bool,
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
    /// Arko's folder, where the settings for worker profiles are kept.
    pub root: Option<PathBuf>,
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
    /// The session saves `value` as `key` and uses it (the result comes to
    /// [`App::keys_done`]).
    AddKey { key: &'static Key, value: Secret },
    /// The session removes `key`, which Arko saved.
    RemoveKey(&'static Key),
    /// A change to OpenRouter's settings in Arko's folder `root` is made on
    /// a thread of its own (it may wait for another change), and the result
    /// comes to [`App::openrouter_done`].
    OpenRouter {
        root: PathBuf,
        edit: super::openrouter_menu::Edit,
    },
    /// Check, off the view's thread, what each way of naming sessions by a
    /// model lacks (the result comes to [`App::names_checked`]), until
    /// asked to stop.
    CheckNames(Cancel),
}

/// A check that its asker can end: set to stop it.
#[derive(Debug, Clone, Default)]
pub struct Cancel(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl PartialEq for Cancel {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The command menu's state.
#[derive(Debug, Clone, Default)]
pub struct Menu {
    pub selected: usize,
    /// Esc closed it: it stays closed until the text changes.
    pub dismissed: Option<String>,
    /// The argument's choice ↑ ↓ moved to.
    pub choice: Option<String>,
}

/// The agent list shows this many workers at most, with a row above them
/// counting the workers above them and one under them counting those
/// below, when there are.
pub const LIST_WORKERS: usize = 5;

/// How long a worker that finished, failed or was stopped stays in the
/// agent list, in milliseconds. Those that ended earlier leave it, so a long
/// session's list holds the workers that matter now.
pub const LIST_ENDED_MS: i64 = 5 * 60 * 1000;

/// Whether `worker` ended in the last [`LIST_ENDED_MS`] before `now`.
fn ended_lately(worker: &crate::remote::WorkerSummary, now: i64) -> bool {
    !worker.state.is_active() && worker.ended_at.is_some_and(|at| now - at < LIST_ENDED_MS)
}

/// A row of the agent list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
    /// The agent at this index of [`App::list`].
    Agent(usize),
    /// "… N more" on top: the workers above those shown.
    Above(usize),
    /// "… N more" at the bottom: the workers below those shown.
    More(usize),
    /// "… N more above, M more below": both, in a list with room for one
    /// count.
    Both(usize, usize),
}

/// The agent list as drawn: its rows, and how far it is scrolled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListWindow {
    pub rows: Vec<ListRow>,
    /// How many workers are above those shown.
    pub scroll: usize,
    /// The farthest it scrolls: the last worker is shown.
    pub most: usize,
}

/// The rows of a list of `count` agents (the main agent, then the workers)
/// in `height` rows at most: the main agent, then at most
/// [`LIST_WORKERS`] workers from the `scroll`th on, with a row counting
/// those above them and one counting those below, when there are. With
/// `follow`, the workers shown move to the agent `focus` is on. A list too
/// short for both counts counts both on one row, and one too short for a
/// worker beside the main agent and a count shows the agent the focus is
/// on and counts the others.
pub fn list_rows(
    count: usize,
    scroll: usize,
    focus: Option<usize>,
    follow: bool,
    height: usize,
) -> ListWindow {
    if count == 0 || height == 0 {
        return ListWindow::default();
    }
    let workers = count - 1;
    if workers <= LIST_WORKERS && count <= height {
        return ListWindow {
            rows: (0..count).map(ListRow::Agent).collect(),
            ..ListWindow::default()
        };
    }
    let focus = focus.map(|row| row.min(count - 1));
    // The rows under the main agent's.
    let room = (height - 1).min(LIST_WORKERS + 1);
    if room < 2 {
        // No room for a worker beside the main agent and the count: the
        // agent the focus is on.
        let agent = focus.unwrap_or(0);
        let mut rows = vec![ListRow::Agent(agent)];
        if height > 1 {
            rows.push(ListRow::More(workers - usize::from(agent > 0)));
        }
        return ListWindow {
            rows,
            ..ListWindow::default()
        };
    }
    // Scrolled past `scroll` workers: whether the count above is shown, how
    // many workers are, and how many are below them.
    let layout = |scroll: usize| {
        let rest = workers - scroll;
        // The rest fit, under the count above when there are workers above.
        if rest + usize::from(scroll > 0) <= room && rest <= LIST_WORKERS {
            return (scroll > 0, rest, 0);
        }
        // The count below, and the count above where a worker fits besides.
        let above = scroll > 0 && room >= 3;
        let shown = (room - 1 - usize::from(above)).min(LIST_WORKERS);
        (above, shown, rest - shown)
    };
    let most = (0..workers)
        .find(|scroll| layout(*scroll).2 == 0)
        .unwrap_or(workers);
    let mut scroll = scroll.min(most);
    if follow && let Some(row) = focus.filter(|row| *row > 0) {
        let worker = row - 1;
        scroll = scroll.min(worker);
        while scroll < most && worker >= scroll + layout(scroll).1 {
            scroll += 1;
        }
    }
    let (above, shown, below) = layout(scroll);
    let mut rows = vec![ListRow::Agent(0)];
    if above {
        rows.push(ListRow::Above(scroll));
    }
    rows.extend((scroll + 1..=scroll + shown).map(ListRow::Agent));
    match below {
        0 => {}
        // (No room for the count above: one row counts both.)
        below if scroll > 0 && !above => rows.push(ListRow::Both(scroll, below)),
        below => rows.push(ListRow::More(below)),
    }
    ListWindow { rows, scroll, most }
}

/// Since when the main agent works (Unix milliseconds), unless it waits
/// for the user's answer (`waits`: not on its model).
fn agent_works_since(activity: &Activity, waits: bool) -> Option<i64> {
    (activity.working && !waits).then(|| activity.since.unwrap_or(i64::MIN))
}

/// How soon after the warning that a worker's cache expired Enter again
/// continues it.
const CONTINUE_CONFIRM_WITHIN: Duration = Duration::from_secs(30);

/// The view's state.
pub struct App {
    pub transcript: Transcript,
    pub editor: Editor,
    pub info: Info,
    pub catalog: Catalog,
    pub depth: Depth,
    pub theme: Theme,
    pub prefs: Prefs,
    /// The settings for worker profiles, which are the user's for every
    /// project.
    pub profile_prefs: crate::profiles::Preferences,
    pub view: View,
    /// The worker whose own transcript shows (the worker view).
    pub watching: Option<String>,
    pub activity: Activity,
    pub reading: Reading,
    /// When the turn running began.
    pub working_since: Option<Instant>,
    /// How long the last turn ran, for the status line once it ends.
    pub last_turn: Option<Duration>,
    /// A stop was asked for the turn running.
    pub stopping: bool,
    /// The answer panel for the main agent's questions, and the questions
    /// attached to the message box.
    pub panel: super::questions::Panel,
    /// The approval tray.
    pub tray: super::approvals::Tray,
    pub menu: Menu,
    pub overlay: Option<Overlay>,
    /// A word in the status line for a moment: "Copied", ...
    pub hint: Option<(String, Instant)>,
    /// The `/message` to a worker whose prompt cache expired that the
    /// status line just said the cost of, and when: Enter again soon after
    /// continues the worker. Esc, or other text, asks again.
    continue_asked: Option<(String, Instant)>,
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
    /// The agent list has the focus, on this agent of [`App::list`] (0 is
    /// the main agent).
    pub list_focus: Option<usize>,
    /// The left button, while it is down over the transcript.
    press: Option<Press>,
    /// The answer panel's text, as last drawn, for the mouse.
    pub docked: Option<Docked>,
    /// The left button, while it is down over the answer panel's text.
    docked_press: Option<DockPress>,
    /// The last click: when, where, and how many in a row.
    last_click: Option<(Instant, u16, u16, u8)>,
    /// The block whose fold the last click opened or closed.
    folded: Option<u64>,
    /// Where the agent list was drawn, and its rows: for the mouse.
    pub list_drawn: (ratatui::layout::Rect, Vec<ListRow>),
    /// How many workers the agent list is scrolled past, and the farthest
    /// it scrolls, as last drawn.
    list_scroll: (usize, usize),
    /// The agent the list's rows last moved to show (see `list_focus`).
    list_followed: Option<usize>,
    /// The wheel scrolled the list since the focus last moved, so its rows
    /// stay where the wheel left them.
    list_wheeled: bool,
    /// The time the agent list goes by (Unix milliseconds), that of the
    /// frame last drawn, for the workers that ended in the last five
    /// minutes.
    list_now: i64,
    /// What a model needs that the session doesn't have, for `/model`'s
    /// picker (from the providers the session reaches, and Claude Code's
    /// login).
    pub model_needs: fn(&[String], &str) -> Option<String>,
    /// The API keys as they are now, without their values, for the keys
    /// menu.
    pub key_states: fn() -> Vec<KeyState>,
    /// Ends the check of what the ways of naming lack, which the session
    /// names menu asked for, when the menu closes before it is done.
    names_check: Option<Cancel>,
    /// The Codex auth.json files of the ChatGPT logins, in order, for the
    /// settings.
    pub login_paths: std::sync::Arc<dyn Fn() -> Vec<std::path::PathBuf> + Send + Sync>,
    /// The models `/model` completes, found once for the session's models
    /// (finding them can ask Claude Code for its login).
    model_names: std::cell::OnceCell<Vec<String>>,
    /// How far OpenRouter's models in memory had changed when the view last
    /// looked.
    openrouter_seen: u64,
}

/// The argument being typed on a command line, and what it may be.
struct Argument {
    /// The command's name, and the words before the argument.
    name: &'static str,
    before: Vec<String>,
    /// The line, and the length of the argument's part typed (in bytes).
    line: String,
    typed: usize,
    /// The choices that start with what is typed, and go further.
    choices: Vec<String>,
    /// What the command takes, on its first argument.
    hint: Option<&'static str>,
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
        let profile_prefs = info
            .root
            .as_deref()
            .map(crate::profiles::Preferences::load)
            .unwrap_or_default();
        // ARKO_THEME, else the theme chosen last, else the default.
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
        transcript.working = agent_works_since(&activity, false);
        transcript.reset(snapshot);
        Self {
            transcript,
            editor,
            info,
            catalog,
            depth,
            theme,
            prefs,
            profile_prefs,
            view,
            watching: None,
            working_since: activity.working.then(Instant::now),
            last_turn: None,
            activity,
            reading: Reading::default(),
            stopping: false,
            panel: Default::default(),
            tray: Default::default(),
            menu: Menu::default(),
            overlay: None,
            hint: None,
            continue_asked: None,
            last_interrupt: None,
            focused: true,
            shift_enter: false,
            mouse,
            effects: Vec::new(),
            input_width: 80,
            list_focus: None,
            list_drawn: Default::default(),
            list_scroll: (0, 0),
            list_followed: None,
            list_wheeled: false,
            list_now: chrono::Utc::now().timestamp_millis(),
            press: None,
            docked: None,
            docked_press: None,
            last_click: None,
            folded: None,
            model_needs: model_picker::needs,
            key_states: crate::credentials::key_states,
            names_check: None,
            login_paths: std::sync::Arc::new(|| {
                crate::logins::paths_in_use_of(&crate::credentials::codex_auths(None))
            }),
            model_names: Default::default(),
            openrouter_seen: crate::openrouter::generation(),
        }
    }

    /// The web page's overlay waits for the page to start (its links come
    /// with the status).
    pub fn waits_for_page(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Remote { .. })) && self.reading.page.is_none()
    }

    /// The session's figures now: the banner follows its models.
    pub fn set_reading(&mut self, reading: Reading) {
        if let Some(banner) = &mut self.transcript.banner
            && !reading.models.banner_model.is_empty()
        {
            if banner.model != reading.models.banner_model {
                // (Why Arko chose the model it had.)
                banner.model_note = None;
            }
            banner.model = reading.models.banner_model.clone();
            banner.workers = reading.models.banner_workers.clone();
        }
        if reading.models != self.reading.models {
            self.model_names.take();
        }
        if reading.models.workers != self.transcript.usual.model
            || reading.models.workers_effort != self.transcript.usual.effort
        {
            self.transcript.usual = super::transcript::Usual {
                model: reading.models.workers.clone(),
                effort: reading.models.workers_effort.clone(),
            };
        }
        let questions = reading.questions != self.reading.questions;
        let approvals = reading.approvals != self.reading.approvals;
        self.reading = reading;
        if approvals {
            self.approvals_changed();
        }
        if questions {
            self.questions_changed();
        }
        if questions || approvals {
            self.transcript.working = agent_works_since(&self.activity, self.waits_for_user());
        }
    }

    /// `/model`'s picker for `target`. Once the conversation has started,
    /// the main agent's model chosen starts a new conversation on it.
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
        if target == Target::Workers && !models.has_workers {
            self.say("This session has no workers");
            return;
        }
        let needs = self.model_needs;
        let reachable = models.reachable.clone();
        self.overlay = Some(Overlay::Model(ModelPicker::open_in(
            target,
            &models,
            |name| needs(&reachable, name),
            self.info.root.as_deref(),
        )));
    }

    /// `/openrouter`'s menu.
    pub fn open_openrouter(&mut self) {
        match self.info.root.as_deref() {
            Some(root) => self.overlay = Some(Overlay::OpenRouter(OpenRouterMenu::open(root))),
            None => self.say("OpenRouter's settings need Arko's folder"),
        }
    }

    /// Whether OpenRouter's models changed in memory since the last call
    /// (fetched), which an open picker or menu takes up, so the view draws
    /// again.
    pub fn openrouter_moved(&mut self) -> bool {
        let now = crate::openrouter::generation();
        if now == self.openrouter_seen {
            return false;
        }
        self.openrouter_seen = now;
        match &mut self.overlay {
            Some(Overlay::Model(picker)) => picker.refresh_openrouter(),
            Some(Overlay::OpenRouter(menu)) => menu.refresh_catalog(),
            _ => {}
        }
        // A fetch that ended may leave the models the settings need now
        // unfetched (they changed meanwhile): they are asked for again.
        if let (Some(root), Some(Overlay::Model(_) | Overlay::OpenRouter(_))) =
            (&self.info.root, &self.overlay)
        {
            crate::openrouter::refresh_soon(root);
        }
        true
    }

    /// `/resume`'s picker, from the session's conversations. While a turn
    /// runs, or when there is no other conversation to go back to, the
    /// session answers `/resume` instead (it says why not).
    pub fn open_resume(&mut self) {
        let entries = self.reading.conversations.clone();
        if self.activity.working || entries.iter().all(|entry| entry.current) {
            self.send("/resume");
            return;
        }
        self.overlay = Some(Overlay::Resume(ResumePicker::open(entries)));
    }

    /// The API keys menu; `words` are what followed `/keys`, and `back_to`
    /// the settings' row it was opened from.
    pub fn open_keys(&mut self, words: &str, back_to: Option<usize>) {
        self.overlay = Some(Overlay::Keys(KeysMenu::open(
            (self.key_states)(),
            words,
            back_to,
        )));
    }

    /// A change the OpenRouter menu asked for was made, or wasn't: the menu
    /// shows it, or, closed, the status line.
    pub fn openrouter_done(
        &mut self,
        result: Result<(crate::openrouter::Settings, String), String>,
    ) {
        if let Some(Overlay::OpenRouter(menu)) = &mut self.overlay {
            menu.done(result);
            return;
        }
        self.say(match result {
            Ok((_, said)) | Err(said) => said,
        });
    }

    /// The session names menu. Without Arko's folder (no session), the
    /// session answers `/session-names`.
    pub fn open_names(&mut self) {
        let Some(root) = self.info.root.clone() else {
            self.send("/session-names");
            return;
        };
        // It opens on the ways saved at once. What each lacks takes a
        // check of Claude Code, which runs in the background and comes to
        // [`App::names_checked`].
        self.overlay = Some(Overlay::Names(NamesMenu::open(
            &crate::session_names::load(&root),
        )));
        self.end_names_check();
        let cancel = Cancel::default();
        self.names_check = Some(cancel.clone());
        self.effects.push(Effect::CheckNames(cancel));
    }

    /// What `check`, a check of the ways of naming, found: the menu shows it,
    /// if that check is the one it waits for.
    pub fn names_checked(&mut self, check: &Cancel, missing: Vec<(Way, Option<String>)>) {
        // Only the check the menu open now asked for: one made for a menu
        // that closed, and reached the loop before the new one opened, isn't
        // this one's.
        if self.names_check.as_ref() != Some(check) {
            return;
        }
        if let Some(Overlay::Names(menu)) = &mut self.overlay {
            menu.checked(missing);
        }
    }

    /// The check the menu open now waits for (tests).
    #[cfg(test)]
    pub fn names_check(&self) -> Option<Cancel> {
        self.names_check.clone()
    }

    /// The menu is closed or open again: a check that is running is no
    /// longer wanted.
    fn end_names_check(&mut self) {
        if let Some(cancel) = self.names_check.take() {
            cancel.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// What the session did with a key the menu sent: the menu shows it,
    /// or, closed, the status line.
    pub fn keys_done(&mut self, key: &'static Key, outcome: crate::keys::Outcome) {
        if let Some(Overlay::Keys(menu)) = &mut self.overlay {
            menu.done(key, outcome, (self.key_states)());
            return;
        }
        self.say(match outcome {
            crate::keys::Outcome::Done { said, .. } => said,
            crate::keys::Outcome::Failed(why) => why,
        });
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

    pub(super) fn effects_push(&mut self, effect: Effect) {
        self.effects.push(effect);
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

    /// The argument typed at the end of a command line, unless Esc closed
    /// its completion.
    fn argument(&self) -> Option<Argument> {
        if self.overlay.is_some() {
            return None;
        }
        let line = self.editor.command_line()?;
        if self.menu.dismissed.as_deref() == Some(line.as_str()) {
            return None;
        }
        let (name, rest) = line.split_once(' ')?;
        let command = commands::find(name).filter(|command| !command.args.is_empty())?;
        let (before, typed) = rest.rsplit_once(' ').unwrap_or(("", rest));
        let before: Vec<String> = before.split_whitespace().map(String::from).collect();
        let choices = self
            .choices(command.name, &before, typed)
            .into_iter()
            .filter(|choice| choice.starts_with(typed) && choice != typed)
            .collect();
        Some(Argument {
            name: command.name,
            hint: before.is_empty().then_some(command.args),
            before,
            typed: typed.len(),
            choices,
            line,
        })
    }

    /// The words that may follow `before` in the arguments of the command
    /// `name`: its fixed ones, or the session's workers, models and so on.
    /// `typed` is the start of the word being typed, which `/model` completes
    /// from OpenRouter's list once it starts with `openrouter:`.
    fn choices(&self, name: &str, before: &[String], typed: &str) -> Vec<String> {
        let before: Vec<&str> = before.iter().map(String::as_str).collect();
        match (name, before.as_slice()) {
            ("/watch", []) => self
                .transcript
                .workers()
                .iter()
                .map(|worker| worker.id.clone())
                .chain(["main".to_string()])
                .collect(),
            ("/message", []) => self
                .transcript
                .workers()
                .iter()
                .map(|worker| worker.id.clone())
                .collect(),
            // The workers running.
            ("/stop-workers", []) => self
                .reading
                .workers_running
                .iter()
                .map(|(id, _)| id.clone())
                .collect(),
            ("/keys", ["add" | "remove"]) => crate::credentials::KEYS
                .iter()
                .map(|key| key.id.to_string())
                .collect(),
            ("/remote", ["qr"]) => {
                let mut names: Vec<String> = super::overlay::page_links(self)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|link| link.interface)
                    .collect();
                names.dedup();
                names
            }
            ("/model", words) => self.model_choices(words, typed),
            ("/builtin-skills", []) => ["on", "off"]
                .into_iter()
                .chain(crate::builtin_skills::names())
                .map(String::from)
                .collect(),
            ("/builtin-skills", [_]) => vec!["on".into(), "off".into()],
            (name, words) => commands::choices(name, words)
                .iter()
                .map(|word| word.to_string())
                .collect(),
        }
    }

    /// `/model`'s words: `worker`, the models the picker offers, `main`
    /// for the workers, then the efforts the model takes.
    fn model_choices(&self, words: &[&str], typed: &str) -> Vec<String> {
        let models = &self.reading.models;
        let model_next = commands::model_comes_next(models, words);
        let (workers, words) = match words {
            ["worker" | "workers", rest @ ..] => (true, rest),
            _ => (false, words),
        };
        let names = || {
            self.model_names.get_or_init(|| {
                let needs = self.model_needs;
                model_picker::names(models, |name| needs(&models.reachable, name))
            })
        };
        let mut found = match words {
            // The main agent keeps its model once the conversation starts.
            [] if !workers && models.started => {
                if models.has_workers {
                    vec!["worker".into()]
                } else {
                    Vec::new()
                }
            }
            [] if workers => std::iter::once("main".to_string())
                .chain(names().iter().cloned())
                .collect(),
            [] => names()
                .iter()
                .cloned()
                .chain(models.has_workers.then(|| "worker".to_string()))
                .collect(),
            [model] if *model != "main" => crate::providers::resolve_model(model)
                .map(|model| {
                    crate::providers::efforts(&model)
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // Any model on OpenRouter, from its list (see `openrouter::completions`).
        if model_next && let Some(root) = &self.info.root {
            for name in crate::openrouter::completions(root, typed) {
                if !found.contains(&name) {
                    found.push(name);
                }
            }
        }
        found
    }

    /// The choice ↑ ↓ show: the one moved to, else the first once a
    /// letter is typed.
    fn chosen(&self, argument: &Argument) -> Option<usize> {
        let moved = self
            .menu
            .choice
            .as_ref()
            .and_then(|choice| argument.choices.iter().position(|c| c == choice));
        moved.or((argument.typed > 0 && !argument.choices.is_empty()).then_some(0))
    }

    /// The dim text after the cursor on a command line: the rest of the
    /// choice shown (and which of how many), else what the argument may be.
    pub fn ghost(&self) -> Option<String> {
        let argument = self.argument()?;
        if let Some(index) = self.chosen(&argument) {
            let rest = &argument.choices[index][argument.typed..];
            let count = argument.choices.len();
            return Some(if count > 1 {
                let keys = crate::glyphs::render("↑↓");
                format!("{rest}  {keys} {} of {count}", index + 1)
            } else {
                rest.to_string()
            });
        }
        if argument.typed > 0 {
            return None;
        }
        match argument.hint {
            Some(hint) => Some(hint.to_string()),
            None => (!argument.choices.is_empty()).then(|| argument.choices.join("|")),
        }
    }

    /// Tab: the choice shown, else the first, in place of what is typed,
    /// then a space when another word may follow.
    fn accept(&mut self, argument: &Argument, index: usize) {
        let choice = &argument.choices[index];
        let start = &argument.line[..argument.line.len() - argument.typed];
        let mut text = format!("{start}{choice}");
        let mut words = argument.before.clone();
        words.push(choice.clone());
        if !self.choices(argument.name, &words, "").is_empty() {
            text.push(' ');
        }
        self.editor.set(&text);
        self.menu.choice = None;
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
    /// agent finished while the window isn't focused).
    pub fn set_activity(&mut self, activity: Activity) -> bool {
        let finished = self.activity.working && !activity.working;
        if activity.working && !self.activity.working {
            self.working_since = Some(Instant::now());
        }
        if finished {
            self.last_turn = self.working_since.map(|since| since.elapsed());
        }
        if !activity.working {
            self.working_since = None;
            self.stopping = false;
        }
        self.transcript.working = agent_works_since(&activity, self.waits_for_user());
        self.activity = activity;
        // (New questions ring when they come, see `questions_changed`.)
        !self.focused && finished
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
        if let Some(docked) = &mut self.docked {
            docked.selection = None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        self.panel.last_key = Some(Instant::now());
        // The agent's questions, from anywhere: a window open comes back
        // when the panel closes.
        if let KeyCode::Char('q') = key.code
            && alt
            && !ctrl
        {
            self.list_focus = None;
            return self.open_panel();
        }
        if self.overlay.is_some() {
            self.overlay_key(key);
            return;
        }
        // The approval tray takes the keys, and gives them back.
        if let KeyCode::Char('a') = key.code
            && alt
            && !ctrl
        {
            return self.focus_tray();
        }
        if self.tray.focused && self.tray_key(key) {
            return;
        }
        // The panel has the keys before the command menu, whose place it
        // takes on the screen.
        if self.panel.open && self.panel_key(key) {
            return;
        }
        if let KeyCode::Char('n') = key.code
            && ctrl
            && !self.open_questions().is_empty()
        {
            return self.not_now();
        }
        if self.list_focus.is_some() && self.list_key(key) {
            return;
        }
        // Keys that work whatever is typed.
        match key.code {
            // (Else back a character, as in readline.)
            KeyCode::Char('b') if ctrl && self.command_running() => return self.move_command(),
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
        // An argument's choices: ↑ ↓ show each, Tab takes it. Enter sends
        // only what is typed.
        if let Some(argument) = self.argument()
            && !argument.choices.is_empty()
        {
            let count = argument.choices.len();
            let chosen = self.chosen(&argument);
            // → takes the choice shown, as in fish and PowerShell: the
            // cursor is at the end, where → did nothing.
            let right = (key.code == KeyCode::Right && !ctrl && !alt)
                || (key.code == KeyCode::Char('f') && ctrl);
            if let Some(at) = chosen.filter(|_| right) {
                return self.accept(&argument, at);
            }
            let moved = match key.code {
                KeyCode::Up => Some(chosen.map_or(count - 1, |at| (at + count - 1) % count)),
                KeyCode::Down => Some(chosen.map_or(0, |at| (at + 1) % count)),
                KeyCode::Tab => return self.accept(&argument, chosen.unwrap_or(0)),
                _ => None,
            };
            if let Some(at) = moved {
                self.menu.choice = Some(argument.choices[at].clone());
                return;
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
                // As at the line prompt: an empty box quits, as /exit does
                // (which warns of what quitting stops).
                if self.editor.is_empty() {
                    self.quit();
                } else {
                    self.editor.delete();
                }
            }
            KeyCode::Backspace if ctrl || alt => self.editor.kill_word(),
            // In an empty box it takes off the questions attached, as it
            // would an attachment.
            KeyCode::Backspace if self.editor.is_empty() && self.detach() => {}
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
            // With nothing to complete, Tab goes on to the agent list, and
            // Shift+Tab to its last agent.
            KeyCode::Tab | KeyCode::BackTab if !self.list().is_empty() => {
                let last = self.list().len() - 1;
                self.list_focus = Some(if key.code == KeyCode::Tab { 0 } else { last });
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

    /// Text was typed or pasted: the menu opens again.
    fn typing(&mut self) {
        self.menu.selected = 0;
        self.menu.dismissed = None;
        self.menu.choice = None;
    }

    /// A paste the terminal marked: into the box, or the keys menu's field.
    pub fn paste(&mut self, text: &str) {
        if let Some(Overlay::Keys(menu)) = &mut self.overlay {
            menu.paste(text);
            return;
        }
        if let Some(Overlay::OpenRouter(menu)) = &mut self.overlay {
            menu.paste(text);
            return;
        }
        if self.overlay.is_some() {
            return;
        }
        if self.panel.open {
            return self.panel_paste(text);
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
        self.continue_asked = None;
        if !self.menu_items().is_empty() || self.ghost().is_some() {
            self.menu.dismissed = Some(self.editor.shown());
        } else if self.watching.is_some() {
            self.watching = None;
            self.transcript.follow();
        } else if !self.transcript.following() {
            self.transcript.follow();
        }
    }

    /// Whether the agent whose transcript is shown (the main agent, or the
    /// worker watched) runs a call that can move to the background now (a
    /// shell command, an MCP tool's call).
    pub fn command_running(&self) -> bool {
        let shown = self
            .watching
            .as_deref()
            .unwrap_or(crate::remote::MAIN_AGENT);
        self.reading.commands.iter().any(|agent| agent == shown)
    }

    /// Ctrl+B while the agent shown runs a call that can move: the call
    /// moves to the background, and the agent goes on (what is typed
    /// meanwhile reaches it as a note, the call no longer in the way).
    fn move_command(&mut self) {
        let shown = self
            .watching
            .clone()
            .unwrap_or_else(|| crate::remote::MAIN_AGENT.to_string());
        // Until the status says again: a second Ctrl+B is a key again.
        self.reading.commands.retain(|agent| *agent != shown);
        self.effects.push(Effect::Input(Input::Background(shown)));
        self.say("Moved to the background");
    }

    /// Ctrl+C: stops the turn; else clears the box; else, pressed twice,
    /// quits. (It never copies: letting go of a drag does.)
    fn interrupt(&mut self) {
        if self.activity.working {
            if self.stopping {
                self.say("Stopping (the agents get up to 10 s to end)");
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
            self.quit();
        } else {
            self.last_interrupt = Some(now);
            self.say("Ctrl+C again quits");
        }
    }

    /// Quits (Ctrl+D, Ctrl+C twice, `/exit`). The session decides, as it
    /// knows the jobs running now: background jobs stop when Arko exits
    /// and never come back, so with any running it asks first
    /// ([`ask_quit`](Self::ask_quit)), unless the user turned that off.
    fn quit(&mut self) {
        let agreed = self.confirms_quit().then(Vec::new);
        self.effects.push(Effect::Input(Input::Quit(agreed)));
    }

    /// The session asks before it quits: `jobs` run in the background, which
    /// quitting would stop. Yes agrees to stop those the popup shows.
    pub fn ask_quit(&mut self, jobs: Vec<arko_agents::terminal::BackgroundJob>) {
        self.reading.jobs = jobs;
        self.overlay = Some(Overlay::Quit {
            choice: 0,
            dont_ask: false,
        });
    }

    /// Whether quitting with background jobs running asks first.
    pub fn confirms_quit(&self) -> bool {
        self.prefs.confirm_quit.unwrap_or(true)
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
    /// running or waiting to run, those that ended in the last five minutes
    /// ([`LIST_ENDED_MS`]), and the one shown when it ended before. A
    /// worker's workers follow it, so Tab goes through a family. Empty when
    /// there are none, so it takes no room. (The workers that ended are in
    /// `/workers`, and `/watch` still shows them.)
    pub fn list(&self) -> Vec<Option<&crate::remote::WorkerSummary>> {
        let all = self.transcript.workers();
        let workers: Vec<Option<&crate::remote::WorkerSummary>> = crate::remote::family_order(all)
            .into_iter()
            .map(|at| &all[at])
            .filter(|worker| {
                worker.state.is_active()
                    || ended_lately(worker, self.list_now)
                    || self.watching.as_deref() == Some(worker.id.as_str())
            })
            .map(Some)
            .collect();
        if workers.is_empty() {
            return Vec::new();
        }
        std::iter::once(None).chain(workers).collect()
    }

    /// Whether the agent list changes with the time: a worker runs (its
    /// time goes on), or one that ended is still in it (it leaves the list
    /// when its five minutes are over).
    pub fn list_ticks(&self) -> bool {
        self.transcript
            .workers()
            .iter()
            .any(|worker| worker.state.is_active() || ended_lately(worker, self.list_now))
    }

    /// The agent list's rows in `height` rows at most, for the frame being
    /// drawn. They show the agent the focus is on, as workers come and go
    /// and the area changes, unless the wheel scrolled the list since the
    /// focus last moved. The list goes by the time of this frame until the
    /// next one, so a worker's five minutes don't end halfway through it.
    pub fn list_rows(&mut self, height: usize) -> Vec<ListRow> {
        self.list_now = chrono::Utc::now().timestamp_millis();
        if self.list_focus != self.list_followed {
            self.list_wheeled = false;
        }
        let follow = !self.list_wheeled;
        self.list_followed = self.list_focus;
        let window = list_rows(
            self.list().len(),
            self.list_scroll.0,
            self.list_focus,
            follow,
            height,
        );
        self.list_scroll = (window.scroll, window.most);
        window.rows
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
            // Tab moves down the list and Shift+Tab up it. Past either end
            // the focus goes back to the input box.
            KeyCode::Tab if row + 1 < list.len() => self.list_focus = Some(row + 1),
            KeyCode::BackTab if row > 0 => self.list_focus = Some(row - 1),
            KeyCode::Tab | KeyCode::BackTab => self.list_focus = None,
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
                ("/exit", _) => {
                    self.quit();
                    true
                }
                // Without a number, the picker. With one, the session.
                ("/resume", _) if rest.is_empty() => {
                    self.open_resume();
                    true
                }
                // A key is entered in the menu, never typed as a word.
                ("/keys", _) => {
                    self.open_keys(rest, None);
                    true
                }
                // Alone, OpenRouter's settings menu. With words, the session.
                ("/openrouter", _) if rest.is_empty() => {
                    self.open_openrouter();
                    true
                }
                // Without words, the menu. With them, the session.
                ("/session-names", _) if rest.is_empty() => {
                    self.open_names();
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
            if !handled && command.name == "/message" && self.asks_to_continue(trimmed) {
                return;
            }
            self.editor.take();
            self.menu = Menu::default();
            if !handled {
                self.send(trimmed);
            }
            return;
        }
        // A mistyped command stays in the box to put right, in a worker's
        // view too, where the rest goes to the worker.
        if let Some(why) = commands::unknown(text.trim_end(), commands::COMMANDS) {
            self.say(why);
            return;
        }
        // While the old model writes its report for a switch, a message to
        // the main agent stays in the box. (One to a worker, in its view,
        // goes.)
        if self.activity.reporting && (self.watching.is_none() || self.agent_waits()) {
            self.say(crate::switch::REPORTING);
            return;
        }
        // An answer too long to send stays in the box, with how short to
        // make it: the questions attached, or those the agent waits for.
        let replied: Vec<String> = if self.panel.attached.is_empty() {
            self.open_questions()
                .iter()
                .filter(|question| question.waits)
                .map(|question| question.id.clone())
                .collect()
        } else {
            self.panel.attached.clone()
        };
        if let Some(refusal) =
            crate::questions::reply_refusal(self.open_questions(), &replied, trimmed)
        {
            self.say(refusal);
            return;
        }
        // In a worker's own view, continuing it when its cache expired
        // asks first.
        if self.panel.attached.is_empty()
            && let Some(id) = self.watching.clone().filter(|_| !self.agent_waits())
            && self.asks_to_continue(&format!("/message {id} {}", trimmed.trim()))
        {
            return;
        }
        let text = self.editor.take();
        self.menu = Menu::default();
        self.transcript.follow();
        // The questions open now were seen.
        let open: Vec<String> = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
        self.panel.seen.extend(open);
        // With questions attached, it answers them, in any view.
        let answers = std::mem::take(&mut self.panel.attached);
        if !answers.is_empty() {
            self.effects
                .push(Effect::Input(Input::Message(crate::ui::Message {
                    text: text.trim_end().to_string(),
                    answers,
                    ..Default::default()
                })));
            return;
        }
        // In a worker's own view it goes to that worker, unless the main
        // agent waits for an answer: then it is the answer, as anywhere.
        match &self.watching {
            Some(id) if !self.agent_waits() => {
                let line = format!("/message {id} {}", text.trim());
                self.send(&line);
            }
            _ => self.send(text.trim_end()),
        }
    }

    /// Whether the `/message` in `line` waits for Enter again: it continues
    /// a worker whose prompt cache expired, and the status line says what
    /// that costs, with the text left in the box. Enter again soon after
    /// (the same text) continues the worker.
    fn asks_to_continue(&mut self, line: &str) -> bool {
        let now = chrono::Utc::now();
        let cache = crate::worker_cache::message_parts(line)
            .and_then(|(to, _)| self.transcript.worker_named(to))
            .and_then(|worker| worker.cache())
            .filter(|cache| cache.asks(now));
        let Some(cache) = cache else {
            return false;
        };
        let confirmed = self
            .continue_asked
            .take()
            .is_some_and(|(asked, at)| asked == line && at.elapsed() < CONTINUE_CONFIRM_WITHIN);
        if confirmed {
            self.editor.take();
            self.menu = Menu::default();
            self.effects
                .push(Effect::Input(Input::Message(crate::ui::Message {
                    text: line.to_string(),
                    continue_uncached: true,
                    ..Default::default()
                })));
            return true;
        }
        self.continue_asked = Some((line.to_string(), Instant::now()));
        self.say(format!("{} Enter again continues it.", cache.warning(now)));
        true
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
                None => self.say("The views are full, quiet and messages"),
            },
            "/watch" => match rest {
                "" => self.focus_list(),
                "main" => self.watch(None),
                id if self.transcript.worker(id).is_some() => self.watch(Some(id.to_string())),
                id => self.say(format!("No worker {id}")),
            },
            "/settings" => {
                // (Another front end may have changed the file.)
                if let Some(root) = self.info.root.as_deref() {
                    self.profile_prefs = crate::profiles::Preferences::load(root);
                }
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
            Overlay::Resume(mut picker) => match picker.key(key, close) {
                resume_picker::Outcome::Stay => Some(Overlay::Resume(picker)),
                resume_picker::Outcome::Close => None,
                resume_picker::Outcome::Current => {
                    self.say("You are in this conversation");
                    None
                }
                resume_picker::Outcome::Resume(number) => {
                    self.send(&format!("/resume {number}"));
                    None
                }
            },
            Overlay::OpenRouter(mut menu) => match menu.key(key, close) {
                super::openrouter_menu::Action::Close => None,
                super::openrouter_menu::Action::Stay => Some(Overlay::OpenRouter(menu)),
                super::openrouter_menu::Action::Edit(edit) => {
                    self.effects.push(Effect::OpenRouter {
                        root: menu.root.clone(),
                        edit,
                    });
                    Some(Overlay::OpenRouter(menu))
                }
            },
            Overlay::Keys(mut menu) => match menu.key(key, close) {
                keys_menu::Action::Stay => Some(Overlay::Keys(menu)),
                keys_menu::Action::Close => menu
                    .back_to
                    .map(|row| Overlay::Settings { row, picker: None }),
                keys_menu::Action::Add { key, value } => {
                    self.effects.push(Effect::AddKey { key, value });
                    Some(Overlay::Keys(menu))
                }
                keys_menu::Action::Remove(key) => {
                    self.effects.push(Effect::RemoveKey(key));
                    Some(Overlay::Keys(menu))
                }
            },
            Overlay::Names(mut menu) => match menu.key(key, close) {
                names_menu::Action::Stay => Some(Overlay::Names(menu)),
                names_menu::Action::Close => {
                    self.end_names_check();
                    None
                }
                names_menu::Action::Save(ways) => {
                    self.end_names_check();
                    let ids: Vec<&str> = ways.iter().map(|way| way.id()).collect();
                    let line = if ids.is_empty() {
                        "/session-names words".to_string()
                    } else {
                        format!("/session-names {}", ids.join(" "))
                    };
                    self.send(&line);
                    None
                }
            },
            // Esc is No. Yes saves a ticked box and quits, stopping the jobs
            // shown (drawn before any key is read) and the workers: should
            // another have started, the session asks again. No leaves the
            // choice unsaved, so the next quit asks again.
            // Esc is Cancel, and either button goes back to the settings.
            Overlay::AskOff { row, choice } => match key.code {
                _ if close => Some(Overlay::Settings { row, picker: None }),
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Tab
                | KeyCode::BackTab => Some(Overlay::AskOff {
                    row,
                    choice: 1 - choice,
                }),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if choice == 0 {
                        self.send("/extensions ask-mcp off");
                    }
                    Some(Overlay::Settings { row, picker: None })
                }
                _ => Some(Overlay::AskOff { row, choice }),
            },
            Overlay::Quit { choice, dont_ask } => match key.code {
                _ if close => None,
                KeyCode::Left | KeyCode::Up | KeyCode::BackTab => Some(Overlay::Quit {
                    choice: (choice + 2) % 3,
                    dont_ask,
                }),
                KeyCode::Right | KeyCode::Down | KeyCode::Tab => Some(Overlay::Quit {
                    choice: (choice + 1) % 3,
                    dont_ask,
                }),
                KeyCode::Enter | KeyCode::Char(' ') => match choice {
                    0 => {
                        if dont_ask {
                            self.prefs.confirm_quit = Some(false);
                            self.prefs.save(self.info.prefs.as_deref());
                        }
                        let shown = self.reading.jobs.iter().map(|job| job.job_id).collect();
                        self.effects.push(Effect::Input(Input::Quit(Some(shown))));
                        None
                    }
                    1 => None,
                    _ => Some(Overlay::Quit {
                        choice,
                        dont_ask: !dont_ask,
                    }),
                },
                _ => Some(Overlay::Quit { choice, dont_ask }),
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
            SettingsRow::ConfirmQuit,
            SettingsRow::ApiPrices,
        ];
        if commands::find("/outside on").is_some() {
            rows.push(SettingsRow::Outside);
        }
        if commands::find("/background-after 10m").is_some() {
            rows.push(SettingsRow::Background);
        }
        if commands::find("/monitor-wakes 2h").is_some() {
            rows.push(SettingsRow::MonitorWakes);
        }
        if commands::find("/text-replies on").is_some() {
            rows.push(SettingsRow::Replies);
        }
        if commands::find("/extra-rules small").is_some() {
            rows.push(SettingsRow::Rules);
        }
        rows.push(SettingsRow::Page);
        if commands::find("/handover-at 200k").is_some() {
            rows.push(SettingsRow::Handover);
            if self.reading.models.has_workers {
                rows.push(SettingsRow::WorkersHandover);
            }
        }
        if commands::find("/cc-compact on").is_some() && self.in_claude_code() {
            rows.push(SettingsRow::Compact);
        }
        if commands::find("/project-instructions one").is_some() {
            rows.push(SettingsRow::ProjectFiles);
            if self.reading.models.has_workers {
                rows.push(SettingsRow::ProjectWorkers);
            }
        }
        if commands::find("/builtin-skills on").is_some() {
            rows.push(SettingsRow::BuiltinSkills);
            rows.extend(
                (0..self.reading.settings.builtin_skill_list.len()).map(SettingsRow::BuiltinSkill),
            );
        }
        if commands::find("/extensions ask-mcp on").is_some() {
            rows.push(SettingsRow::AskBeforeMcp);
            rows.push(SettingsRow::ReviewProjectSkills);
        }
        rows.push(SettingsRow::KeepProfilesOutOfGit);
        if self.reading.models.has_workers {
            if commands::find("/worker-models agent").is_some() {
                rows.push(SettingsRow::WorkerModels);
            }
            if commands::find("/worker-billing ask").is_some() {
                rows.push(SettingsRow::WorkerBilling);
                rows.push(SettingsRow::ApiBudget);
            }
            if commands::find("/worker-depth 2").is_some() {
                rows.push(SettingsRow::WorkerDepth);
            }
            if commands::find("/worker-retry-cap on").is_some() {
                rows.push(SettingsRow::WorkerRetryCap);
            }
            if commands::find("/message-copies on").is_some() {
                rows.push(SettingsRow::MessageCopies);
            }
            rows.push(SettingsRow::RepoRootProfiles);
        }
        rows.push(SettingsRow::Keys);
        if commands::find("/chatgpt add").is_some() {
            rows.push(SettingsRow::Logins);
        }
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
                        self.say("This terminal shows 16 colours, and themes need more");
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
                SettingsRow::ConfirmQuit => {
                    self.prefs.confirm_quit = Some(!self.confirms_quit());
                    self.prefs.save(self.info.prefs.as_deref());
                }
                SettingsRow::ApiPrices => {
                    self.prefs.api_prices = !self.prefs.api_prices;
                    self.prefs.save(self.info.prefs.as_deref());
                }
                SettingsRow::Outside => {
                    let on = !self.outside();
                    self.send(if on { "/outside on" } else { "/outside off" });
                }
                SettingsRow::Handover | SettingsRow::WorkersHandover => {
                    // The next round size down or up (the session may hold
                    // it under the model's limit; the status says).
                    let (now, whose) = if rows[row] == SettingsRow::Handover {
                        (self.handover_at(), "main")
                    } else {
                        (self.workers_handover_at(), "workers")
                    };
                    let steps = super::overlay::HANDOVER_STEPS;
                    let next = if step < 0 {
                        steps.iter().rev().find(|size| **size < now)
                    } else {
                        steps.iter().find(|size| **size > now)
                    };
                    if let Some(size) = next {
                        self.send(&format!("/handover-at {whose} {}k", size / 1000));
                    }
                }
                SettingsRow::Background => {
                    // The next step down (to off) or up.
                    let now = self.background_after();
                    let steps = super::overlay::BACKGROUND_STEPS;
                    let next = if step < 0 {
                        steps.iter().rev().find(|after| **after < now)
                    } else {
                        steps.iter().find(|after| **after > now)
                    };
                    if let Some(after) = next {
                        self.send(&format!(
                            "/background-after {}",
                            crate::shell::shown_background_after(*after)
                        ));
                    }
                }
                SettingsRow::MonitorWakes => {
                    // The next step down (to off) or up (to always).
                    if let Some(wakes) = self.monitor_wakes().step(step >= 0) {
                        self.send(&wakes.command());
                    }
                }
                SettingsRow::Replies => {
                    let on = !self.text_replies();
                    self.send(if on {
                        "/text-replies on"
                    } else {
                        "/text-replies off"
                    });
                }
                SettingsRow::Compact => {
                    let on = !self.cc_compact();
                    self.send(if on {
                        "/cc-compact on"
                    } else {
                        "/cc-compact off"
                    });
                }
                SettingsRow::Rules => {
                    let all = ExtraRules::ALL;
                    let index = all.iter().position(|rules| *rules == self.extra_rules());
                    let next = (index.unwrap_or(0) as i32 + step).rem_euclid(all.len() as i32);
                    self.send(&format!("/extra-rules {}", all[next as usize].name()));
                }
                SettingsRow::ProjectFiles => {
                    let all = project::Files::ALL;
                    let index = all
                        .iter()
                        .position(|files| *files == self.project_instructions());
                    let next = (index.unwrap_or(0) as i32 + step).rem_euclid(all.len() as i32);
                    self.send(&format!(
                        "/project-instructions {}",
                        all[next as usize].name()
                    ));
                }
                SettingsRow::ProjectWorkers => {
                    self.send(if self.worker_project_instructions() {
                        "/project-instructions workers off"
                    } else {
                        "/project-instructions workers on"
                    });
                }
                SettingsRow::WorkerModels => {
                    let all = crate::agents::ModelChoice::ALL;
                    let index = all
                        .iter()
                        .position(|choice| *choice == self.worker_models());
                    let next = (index.unwrap_or(0) as i32 + step).rem_euclid(all.len() as i32);
                    self.send(&all[next as usize].command());
                }
                SettingsRow::WorkerBilling => {
                    let all = crate::workers::WorkerBilling::ALL;
                    let index = all
                        .iter()
                        .position(|billing| *billing == self.worker_billing());
                    let next = (index.unwrap_or(0) as i32 + step).rem_euclid(all.len() as i32);
                    self.send(&all[next as usize].command());
                }
                SettingsRow::ApiBudget => {
                    self.send(&crate::workers::budget_command(self.api_budget().is_none()));
                }
                SettingsRow::WorkerDepth => {
                    let most = crate::workers::MAX_DEPTH as i32;
                    let next = (self.worker_depth() as i32 + step).clamp(1, most) as u32;
                    if next != self.worker_depth() {
                        self.send(&crate::workers::depth_command(next));
                    }
                }
                SettingsRow::WorkerRetryCap => {
                    self.send(&crate::workers::retry_cap_command(
                        self.worker_retry_cap().is_none(),
                    ));
                }
                SettingsRow::MessageCopies => {
                    self.send(&crate::workers::copies_command(!self.message_copies()));
                }
                SettingsRow::KeepProfilesOutOfGit => {
                    // The session saves it and follows it in the project's
                    // `.arko/.gitignore`, and tells the web page.
                    self.send(if self.reading.settings.keep_agents_files_out_of_git {
                        "/extensions keep-out-of-git off"
                    } else {
                        "/extensions keep-out-of-git on"
                    });
                }
                SettingsRow::RepoRootProfiles => {
                    // (Read again, as the git switch is the session's to save.)
                    let mut now = self
                        .info
                        .root
                        .as_deref()
                        .map_or(self.profile_prefs, crate::profiles::Preferences::load);
                    now.repo_root = !now.repo_root;
                    match self
                        .info
                        .root
                        .as_deref()
                        .map_or(Ok(()), |root| now.save(root))
                    {
                        Ok(()) => self.profile_prefs = now,
                        Err(error) => self.say(error),
                    }
                }
                SettingsRow::AskBeforeMcp => {
                    if self.reading.settings.ask_before_adding_mcp {
                        // The safe choice is on, so turning it off asks.
                        return Some(Overlay::AskOff { row, choice: 1 });
                    }
                    self.send("/extensions ask-mcp on");
                }
                SettingsRow::ReviewProjectSkills => {
                    self.send(if self.reading.settings.review_project_skills {
                        "/extensions review-skills off"
                    } else {
                        "/extensions review-skills on"
                    });
                }
                SettingsRow::BuiltinSkills => {
                    self.send(if self.builtin_skills() {
                        "/builtin-skills off"
                    } else {
                        "/builtin-skills on"
                    });
                }
                SettingsRow::BuiltinSkill(index) => {
                    if let Some((name, on)) = self
                        .builtin_skill(index)
                        .map(|skill| (skill.name.clone(), skill.on))
                    {
                        let state = if on { "off" } else { "on" };
                        self.send(&format!("/builtin-skills {name} {state}"));
                    }
                }
                SettingsRow::Page => {
                    if self.reading.page.is_some() {
                        self.send("/remote off");
                    } else {
                        self.send("/remote");
                    }
                }
                // Opened forward only: ← has nothing to step back to.
                SettingsRow::Keys if step > 0 => {
                    self.open_keys("", Some(row));
                    return self.overlay.take();
                }
                SettingsRow::Keys | SettingsRow::Logins => {}
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

    /// Whether the main agent ends its turns with a reply in plain text.
    pub fn text_replies(&self) -> bool {
        self.reading.settings.text_replies
    }

    /// Whether the agents on `cc:` models compact at the handover size
    /// (else they hand over).
    pub fn cc_compact(&self) -> bool {
        self.reading.settings.cc_compact
    }

    /// Whether an agent of the session runs in Claude Code: the main agent,
    /// or the workers it starts.
    fn in_claude_code(&self) -> bool {
        let models = &self.reading.models;
        models.main.starts_with("cc:") || models.workers.starts_with("cc:")
    }

    /// Which agents get the extra rules for small models.
    pub fn extra_rules(&self) -> ExtraRules {
        self.reading.settings.extra_rules
    }

    /// Who chooses the workers' models.
    pub fn worker_models(&self) -> crate::agents::ModelChoice {
        self.reading.settings.worker_models
    }

    /// Whether workers may run on models billed per token.
    pub fn worker_billing(&self) -> crate::workers::WorkerBilling {
        self.reading.settings.worker_billing
    }

    /// The daily API budget in dollars, if any.
    pub fn api_budget(&self) -> Option<f64> {
        self.reading.settings.api_budget
    }

    /// How many levels of workers the session allows.
    pub fn worker_depth(&self) -> u32 {
        self.reading.settings.worker_depth
    }

    /// After how many failed model requests in a row a worker fails, if it
    /// does.
    pub fn worker_retry_cap(&self) -> Option<u32> {
        self.reading.settings.worker_retry_cap
    }

    /// Whether the agent that started a worker gets a copy of the user's
    /// messages to it.
    pub fn message_copies(&self) -> bool {
        self.reading.settings.message_copies
    }

    /// Whether the agents get the built-in skills at all.
    pub fn builtin_skills(&self) -> bool {
        self.reading.settings.builtin_skills
    }

    /// The `index`th built-in skill and whether it is left out by name.
    pub fn builtin_skill(&self, index: usize) -> Option<&crate::remote::BuiltinSkillSetting> {
        self.reading.settings.builtin_skill_list.get(index)
    }

    /// Which of the project's instruction files the agents get.
    pub fn project_instructions(&self) -> project::Files {
        self.reading.settings.project_instructions
    }

    /// Whether workers started from now on get the project's instructions.
    pub fn worker_project_instructions(&self) -> bool {
        self.reading.settings.worker_project_instructions
    }

    /// Whether the main agent's conversation has started: its text replies
    /// setting (like its model) stays as it is then.
    pub fn conversation_started(&self) -> bool {
        self.reading.models.started
    }

    /// After how many seconds a shell command still running moves to the
    /// background; 0: never.
    pub fn background_after(&self) -> u64 {
        self.reading.settings.background_after
    }

    /// How long after the user's last message a monitor's warning still
    /// wakes the agent.
    pub fn monitor_wakes(&self) -> crate::monitors::Wakes {
        self.reading.settings.monitor_wakes
    }

    /// For how long a warning wakes the agent now, which `auto` takes from
    /// its prompt cache (`None`: always).
    pub fn monitor_wakes_within(&self) -> Option<std::time::Duration> {
        self.reading
            .settings
            .monitor_wakes_within
            .map(std::time::Duration::from_secs)
    }

    /// The handover size now.
    pub fn handover_at(&self) -> usize {
        match self.reading.settings.handover_at {
            0 => self.info.handover_at,
            size => size,
        }
    }

    /// The size new workers hand over at now.
    pub fn workers_handover_at(&self) -> usize {
        self.reading.settings.workers_handover_at
    }

    // ----- the mouse -----

    /// The mouse: the wheel scrolls; a drag over the transcript, or over
    /// the text of the answer panel, highlights
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
            // Over the agent list, the wheel scrolls it, a worker at a time,
            // when it has workers to scroll to.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.overlay.is_none()
                    && self.list_drawn.0.contains(at)
                    && self.list_scroll.1 > 0 =>
            {
                self.list_wheeled = true;
                let (scroll, most) = self.list_scroll;
                self.list_scroll.0 = if event.kind == MouseEventKind::ScrollUp {
                    scroll.saturating_sub(1)
                } else {
                    (scroll + 1).min(most)
                };
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
                // On the answer panel's text, it selects that text.
                if let Some(docked) = &mut self.docked {
                    if docked.contains(x, y) {
                        let Some(spot) = docked.spot_at(x, y) else {
                            return;
                        };
                        docked.selection = match count {
                            2 => docked.word_at(spot),
                            3 => docked.line_at(spot),
                            _ => None,
                        };
                        self.docked_press = Some(DockPress {
                            anchor: docked.selection.map_or(spot, |(from, _)| from),
                            at: (x, y),
                            dragged: docked.selection.is_some(),
                        });
                        return;
                    }
                    docked.selection = None;
                }
                let folded = self.folded.take();
                let on_marker = self
                    .transcript
                    .drawn
                    .marker
                    .is_some_and(|rect| rect.contains(at));
                if on_marker || !self.transcript.drawn.area.contains(at) {
                    return;
                }
                let Some(mut point) = self.transcript.point_at(x, y) else {
                    return;
                };
                let filter = self.filter();
                let theme = self.theme.clone();
                // A double click on a fold's line selects a word of it: the
                // fold its first click opened or closed goes back.
                if count == 2
                    && let Some(key) = folded
                    && let Some(line) = self.transcript.toggle(key, y, &filter, &theme)
                {
                    point = super::transcript::Point { key, line, ..point };
                }
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
            MouseEventKind::Drag(MouseButton::Left) if self.docked_press.is_some() => {
                let Some(press) = &mut self.docked_press else {
                    return;
                };
                if press.at == (x, y) {
                    return;
                }
                press.dragged = true;
                press.at = (x, y);
                let anchor = press.anchor;
                if let Some(docked) = &mut self.docked
                    && let Some(head) = docked.spot_at(x, y)
                {
                    docked.selection = Some((anchor, head));
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.docked_press.is_some() => {
                if self.docked_press.take().is_some_and(|press| press.dragged)
                    && let Some(text) = self.docked.as_ref().and_then(Docked::selected_text)
                {
                    self.copy(text);
                }
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
        if let Some(text) = self.transcript.selected_text(&filter, &theme) {
            self.copy(text);
        }
    }

    /// Copies `text`, and says how much.
    fn copy(&mut self, text: String) {
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
    /// open in the Full view, a command's fold line opens or closes its
    /// whole output, and a worker's line opens its transcript. Elsewhere it
    /// does nothing.
    fn click(&mut self, at: ratatui::layout::Position) {
        if self.overlay.is_some() {
            return;
        }
        let (list, rows) = &self.list_drawn;
        if list.contains(at) {
            let row = rows.get((at.y - list.y) as usize).copied();
            // (The first and the last worker shown, for the counts of the
            // others.)
            let shown = || {
                rows.iter().filter_map(|row| match row {
                    ListRow::Agent(index) => Some(*index),
                    _ => None,
                })
            };
            let first = shown().filter(|index| *index > 0).min().unwrap_or(1);
            let last = shown().max().unwrap_or(0);
            match row {
                Some(ListRow::Agent(row)) => {
                    if let Some(agent) = self.list().get(row) {
                        let id = agent.map(|w| w.id.clone());
                        self.watch(id);
                    }
                }
                // The count of the workers below: the list moves on to the
                // next of them (after the last, back to the first).
                Some(ListRow::More(_) | ListRow::Both(..)) => {
                    let count = self.list().len();
                    self.list_focus = Some(if last + 1 < count { last + 1 } else { 1 });
                }
                // The count of those above: the list moves back to the one
                // before the first shown.
                Some(ListRow::Above(_)) => {
                    self.list_focus = Some(first.saturating_sub(1).max(1));
                }
                None => {}
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
        let (filter, theme) = (self.filter(), self.theme.clone());
        if let Some(key) = self.transcript.fold_at(at.x, at.y, &filter, &theme) {
            self.transcript.toggle(key, at.y, &filter, &theme);
            self.folded = Some(key);
            return;
        }
        if let Some(item) = self.transcript.item(key)
            && let Some(worker) = item.worker.clone()
            && self.watching.is_none()
        {
            self.watch(Some(worker));
            return;
        }
        // The line of questions still open opens the answer panel on them.
        if self
            .transcript
            .item(key)
            .is_some_and(|item| item.kind == crate::remote::Kind::Question)
        {
            self.open_card(key);
        }
    }

    /// Whether the status line's hint is still fresh: for 3 s, or for as
    /// long as a longer one takes to read.
    pub fn hint(&self) -> Option<&str> {
        self.hint
            .as_ref()
            .filter(|(text, at)| {
                at.elapsed() < AGAIN.max(Duration::from_millis(60) * text.chars().count() as u32)
            })
            .map(|(text, _)| text.as_str())
    }

    /// The terminal's title: the state and the folder.
    pub fn title(&self) -> String {
        let state = if self.agent_waits() {
            "asks you"
        } else if self.activity.working {
            "working"
        } else if !self.open_questions().is_empty() {
            "asks you"
        } else {
            "idle"
        };
        crate::glyphs::render(&format!("Arko · {state} · {}", self.info.workspace)).into_owned()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::remote::{Item, Kind};
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
    fn a_message_stays_in_the_box_while_the_report_is_written() {
        let mut app = app();
        app.set_activity(Activity {
            reporting: true,
            working: true,
            since: None,
            question: None,
        });
        type_text(&mut app, "Hello.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(sent(&mut app).is_empty(), "not sent");
        assert_eq!(app.editor.text(), "Hello.");
        assert_eq!(
            app.hint.as_ref().map(|(text, _)| text.as_str()),
            Some("The report is being written. Stopping skips it.")
        );
        // A command still goes.
        app.editor.take();
        type_text(&mut app, "/workers");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/workers"]);
        // Once it is in, the message goes.
        app.set_activity(Activity::default());
        type_text(&mut app, "Hello.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["Hello."]);
    }

    #[test]
    fn ctrl_c_stops_clears_and_quits_twice() {
        let mut app = app();
        app.set_activity(Activity {
            reporting: false,
            working: true,
            since: None,
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
        let quit = [Effect::Input(Input::Quit(Some(Vec::new())))];
        assert_eq!(app.take_effects(), quit);
        // Ctrl+D on an empty box quits the same way.
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.take_effects(), quit);
    }

    /// A job in the background, started `ago`.
    pub fn job(command: &str, ago: Duration) -> arko_agents::terminal::BackgroundJob {
        arko_agents::terminal::BackgroundJob {
            owner: uuid::Uuid::nil(),
            job_id: uuid::Uuid::new_v4(),
            command: command.into(),
            started: Instant::now() - ago,
        }
    }

    /// Quitting asks the session, which knows the jobs running now: with
    /// any in the background, it opens the popup. Esc and No stay, and the
    /// box ticked with No isn't kept. Yes quits, agreeing to stop the jobs
    /// shown, and with the box ticked it is kept, so quitting doesn't ask
    /// again until the settings' row turns the question back on.
    #[test]
    fn quitting_with_background_jobs_asks_first() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.json");
        let mut app = App::new(
            Snapshot::default(),
            Editor::new(History::load(None)),
            Info {
                prefs: Some(path.clone()),
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        );
        let popup = |app: &App| match app.overlay {
            Some(Overlay::Quit { choice, dont_ask }) => Some((choice, dont_ask)),
            _ => None,
        };
        let quits = |app: &mut App| -> Vec<Option<Vec<uuid::Uuid>>> {
            app.take_effects()
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Input(Input::Quit(agreed)) => Some(agreed),
                    _ => None,
                })
                .collect()
        };
        let asking = || vec![Some(Vec::new())];
        let ctrl = |app: &mut App, c: char| press(app, KeyCode::Char(c), KeyModifiers::CONTROL);
        let key = |app: &mut App, code: KeyCode| press(app, code, KeyModifiers::NONE);
        let jobs = vec![job("sleep 300", Duration::from_secs(90))];
        ctrl(&mut app, 'd');
        assert_eq!(quits(&mut app), asking());
        assert_eq!(app.overlay, None);

        app.ask_quit(jobs.clone());
        assert_eq!(popup(&app), Some((0, false)));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, None);
        assert!(app.take_effects().is_empty());
        // `/quit` asks too, and No stays.
        type_text(&mut app, "/quit");
        key(&mut app, KeyCode::Enter);
        assert_eq!(quits(&mut app), asking());
        app.ask_quit(jobs.clone());
        key(&mut app, KeyCode::Right);
        assert_eq!(popup(&app), Some((1, false)));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, None);
        assert!(app.take_effects().is_empty());
        // Ctrl+C twice asks too. The box ticked, then No: nothing is kept.
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        assert_eq!(quits(&mut app), asking());
        app.ask_quit(jobs.clone());
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(popup(&app), Some((2, true)));
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, None);
        assert!(app.take_effects().is_empty());
        assert!(app.confirms_quit());
        assert_eq!(Prefs::load(Some(&path)).confirm_quit, None);

        // The box ticked, then Yes: Arko quits, stopping the jobs shown,
        // and the choice is kept.
        ctrl(&mut app, 'd');
        assert_eq!(quits(&mut app), asking());
        app.ask_quit(jobs.clone());
        assert_eq!(popup(&app), Some((0, false)), "the box starts clear");
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Tab);
        assert_eq!(popup(&app), Some((0, true)));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, None);
        assert_eq!(quits(&mut app), [Some(vec![jobs[0].job_id])]);
        assert_eq!(Prefs::load(Some(&path)).confirm_quit, Some(false));
        // From then on quitting doesn't ask about jobs (the session still
        // warns of running workers).
        ctrl(&mut app, 'd');
        assert_eq!(quits(&mut app), [None]);

        // The settings' row turns the question back on.
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::ConfirmQuit)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        key(&mut app, KeyCode::Enter);
        assert!(app.confirms_quit());
        assert_eq!(Prefs::load(Some(&path)).confirm_quit, Some(true));
        key(&mut app, KeyCode::Esc);
        ctrl(&mut app, 'd');
        assert_eq!(quits(&mut app), asking());
    }

    #[test]
    fn the_command_menu_filters_completes_and_runs() {
        let mut app = app();
        type_text(&mut app, "/se");
        let names: Vec<&str> = app.menu_items().iter().map(|c| c.name).collect();
        assert_eq!(names, ["/settings", "/session", "/session-names"]);
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

    /// In a worker's own view, what is sent goes to that worker (as
    /// `/message`), but for commands and the main agent's answer.
    #[test]
    fn a_workers_view_sends_to_that_worker() {
        let mut app = app();
        app.watch(Some("r1".into()));
        type_text(&mut app, "Use tabs.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/message r1 Use tabs."]);
        // A command still goes to the session as it is.
        type_text(&mut app, "/workers");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/workers"]);
        // The main agent's question it waits for takes the answer, in any
        // view. (Keys came just now: the panel stays closed.)
        let mut waiting = asking();
        waiting.questions.truncate(1);
        waiting.questions[0].waits = true;
        app.set_reading(waiting);
        assert!(!app.panel.open);
        type_text(&mut app, "tabs");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["tabs"]);
        // Back on the main agent, a message is the main agent's.
        app.watch(None);
        app.set_reading(Reading::default());
        type_text(&mut app, "Thanks.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["Thanks."]);
    }

    /// A mistyped command isn't sent, from the main view or a worker's: it
    /// stays in the box, and the status line says what it may be. A space
    /// first sends it as a message.
    #[test]
    fn a_mistyped_command_stays_in_the_box() {
        let mut app = app();
        for watching in [None, Some("r1".to_string())] {
            app.watch(watching);
            type_text(&mut app, "/quiet");
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert!(sent(&mut app).is_empty());
            assert_eq!(app.editor.text(), "/quiet");
            assert_eq!(
                app.hint(),
                Some(
                    "/quiet isn't a command. Did you mean /view quiet? Start with a space to send it as a message."
                )
            );
            app.editor.clear();
        }
        type_text(&mut app, " /quiet");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/message r1 /quiet"]);
    }

    /// Open questions as the status has them: q1 with choices and a
    /// default, q2 without.
    pub fn asking() -> Reading {
        use crate::questions::{AgentQuestion, QuestionState};
        Reading {
            questions: vec![
                AgentQuestion {
                    id: "q1".into(),
                    question: "Tabs or spaces?".into(),
                    choices: vec!["Tabs".into(), "Spaces".into()],
                    multiple: false,
                    default: Some("Spaces".into()),
                    state: QuestionState::Open,
                    waits: false,
                    item: 7,
                    number: Some(1),
                },
                AgentQuestion {
                    id: "q2".into(),
                    question: "Which branch?".into(),
                    choices: Vec::new(),
                    multiple: false,
                    default: None,
                    state: QuestionState::Open,
                    waits: false,
                    item: 7,
                    number: Some(2),
                },
            ],
            ..Reading::default()
        }
    }

    fn inputs(app: &mut App) -> Vec<Input> {
        app.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Input(input) => Some(input),
                _ => None,
            })
            .collect()
    }

    /// New questions open the answer panel when the box is empty and no key
    /// came lately. While the user types, a chip says so instead, and Alt+Q
    /// (Option+Q on a Mac) opens the panel.
    #[test]
    fn questions_open_the_panel_unless_the_user_is_typing() {
        let mut app = app();
        app.set_reading(asking());
        assert!(app.panel.open);
        assert!(app.chips().is_empty());

        let mut app = super::tests::app();
        type_text(&mut app, "Hel");
        app.set_reading(asking());
        assert!(!app.panel.open);
        assert_eq!(app.chips(), [super::super::questions::Chip::Asks(2)]);
        assert_eq!(app.chips()[0].text(), "Arko asks 2 questions");
        press(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        assert!(app.panel.open);
        assert!(app.chips().is_empty());
        // What was typed stays in the box.
        assert_eq!(app.editor.shown(), "Hel");
        // Without questions, Alt+Q says so.
        app.set_reading(Reading::default());
        assert!(!app.panel.open);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        assert_eq!(app.hint(), Some("No open questions"));
    }

    /// A single choice advances to the next question. The summary lets
    /// an answer change before Send. Drafts stay until the questions settle.
    #[test]
    fn the_panel_picks_choices_and_sends_what_is_filled_in() {
        use crate::questions::{Answer, Answers, QuestionAnswer};
        let mut app = app();
        app.set_reading(asking());
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.panel.drafts["q1"].chosen, [0]);
        assert_eq!(app.focus_index(), 1);
        type_text(&mut app, "2 hours");
        assert_eq!(app.panel.drafts["q2"].other, "2 hours");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(app.panel.drafts["q2"].you_decide);
        assert_eq!(app.panel.review, Some(2));
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus_index(), 1);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut app, "main 2");
        // The words go with "You decide" until Space takes it back.
        assert!(app.panel.drafts["q2"].you_decide);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!app.panel.drafts["q2"].you_decide);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.panel.review, Some(2));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            inputs(&mut app),
            [Input::AnswerQuestions(Answers {
                answers: vec![
                    QuestionAnswer::new(
                        "q1",
                        Answer {
                            choices: vec!["Tabs".into()],
                            ..Answer::default()
                        }
                    ),
                    QuestionAnswer::new(
                        "q2",
                        Answer {
                            text: Some("main 2".into()),
                            ..Answer::default()
                        }
                    ),
                ],
            })]
        );
        assert!(!app.panel.open);
        // The drafts stay until their questions settle (answers too long to
        // send settle nothing, and can be shortened).
        assert_eq!(app.panel.drafts["q2"].other, "main 2");
        let mut reading = asking();
        reading.questions.clear();
        app.set_reading(reading);
        assert!(app.panel.drafts.is_empty());
        // Enter with nothing filled in sends nothing.
        app.set_reading(asking());
        app.open_panel();
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(inputs(&mut app).is_empty());
        assert!(app.panel.open);
    }

    /// An answer that is a number, or starts with one, is typed whole: in
    /// a question without choices at once, and in one with choices in its
    /// Other field, or straight from its chips when no chip has the number.
    #[test]
    fn answers_starting_with_a_digit_are_typed_whole() {
        let mut app = app();
        app.set_reading(asking());
        app.focus_on(1);
        type_text(&mut app, "1 hour");
        assert_eq!(app.panel.drafts["q2"].other, "1 hour");
        assert!(!app.panel.drafts["q2"].you_decide);
        app.focus_on(0);
        // q1 has three numbered choices, so 4 starts a written answer.
        type_text(&mut app, "42");
        assert_eq!(app.panel.drafts["q1"].other, "42");
        assert!(app.panel.drafts["q1"].chosen.is_empty());
        app.focus_on(0);
        type_text(&mut app, "1");
        assert_eq!(app.panel.drafts["q1"].other, "421");
    }

    /// The panel's keys stay with the question being answered while others
    /// are answered elsewhere (the web page), and when that question itself
    /// is answered elsewhere, the panel closes rather than give the keys to
    /// another one.
    #[test]
    fn the_panel_keeps_its_question_as_others_are_answered_elsewhere() {
        let mut app = app();
        let mut reading = asking();
        let mut third = reading.questions[1].clone();
        third.id = "q3".into();
        third.question = "Push now?".into();
        third.number = Some(3);
        reading.questions.push(third);
        app.set_reading(reading.clone());
        app.focus_on(1);
        type_text(&mut app, "ab");
        // The page answers q1.
        reading.questions.remove(0);
        app.set_reading(reading.clone());
        type_text(&mut app, "c");
        assert_eq!(app.panel.drafts["q2"].other, "abc");
        assert!(!app.panel.drafts.contains_key("q3"));
        // Then q2, the one being answered here.
        reading.questions.remove(0);
        app.set_reading(reading);
        assert!(!app.panel.open);
        assert_eq!(app.hint(), Some("That question is no longer open"));
        type_text(&mut app, "d");
        assert!(!app.panel.drafts.contains_key("q3"));
        assert_eq!(app.editor.shown(), "d");
    }

    /// Alt+Q opens the panel over the command menu, and the panel has the
    /// keys as it shows, the text typed staying in the box. Over a window,
    /// the window goes, and comes back when the panel closes.
    #[test]
    fn the_panel_takes_the_keys_where_it_shows() {
        let mut app = app();
        type_text(&mut app, "/m");
        app.set_reading(asking());
        assert!(!app.panel.open && !app.menu_items().is_empty());
        press(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        assert!(app.panel.open);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.panel.drafts["q1"].chosen, [0]);
        assert_eq!(app.editor.shown(), "/m");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        let mut app = super::tests::app();
        app.overlay = Some(Overlay::Help { scroll: 0 });
        app.set_reading(asking());
        assert!(!app.panel.open, "a window is open");
        press(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        assert!(app.panel.open && app.overlay.is_none());
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.panel.drafts["q1"].chosen, [0]);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.panel.open);
        assert_eq!(app.overlay, Some(Overlay::Help { scroll: 0 }));
    }

    /// Esc ("answer in chat") attaches the open questions to the message
    /// box, whose next message answers them. Backspace in the empty box
    /// takes them off, and Ctrl+N dismisses them ("Not now").
    #[test]
    fn esc_attaches_the_questions_and_backspace_takes_them_off() {
        use super::super::questions::Chip;
        let mut app = app();
        app.set_reading(asking());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.panel.open);
        assert_eq!(app.chips(), [Chip::Replying(2)]);
        assert_eq!(app.chips()[0].text(), "Replying to 2 questions ×");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.chips(), [Chip::Open(2)]);
        assert_eq!(app.chips()[0].text(), "2 open questions");
        // Attached again, the message answers them.
        press(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        type_text(&mut app, "Spaces, on main.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            inputs(&mut app),
            [Input::Message(crate::ui::Message {
                text: "Spaces, on main.".into(),
                answers: vec!["q1".into(), "q2".into()],
                ..Default::default()
            })]
        );
        assert_eq!(app.chips(), [Chip::Open(2)]);
        // A message without them is a plain one.
        type_text(&mut app, "Hello.");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["Hello."]);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(
            inputs(&mut app),
            [Input::DismissQuestions(vec!["q1".into(), "q2".into()])]
        );
    }

    /// A message that answers questions, or answers in the panel, too long
    /// to send aren't sent: the text stays in the box (and the drafts in
    /// the open panel), and the status line says how short to make it.
    /// Once short enough, they go.
    #[test]
    fn answers_too_long_to_send_stay_where_they_were_typed() {
        let mut app = app();
        let mut reading = asking();
        reading.questions[1].waits = true;
        app.set_reading(reading);
        let long = "x".repeat(crate::questions::MAX_ANSWER_NOTE_BYTES);
        // In the panel, Send on the summary checks the whole answer.
        app.paste(&long);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(inputs(&mut app).is_empty());
        assert!(app.panel.open);
        assert_eq!(app.panel.drafts["q1"].other, long);
        assert_eq!(
            app.hint(),
            Some("Your answer is too long to send. Shorten it to 15 KB.")
        );
        app.panel.drafts.clear();
        // Attached to a message.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.paste(&long);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let refusal = "Your message is too long to send as an answer. Shorten it to 15 KB.";
        assert!(app.take_effects().is_empty());
        assert_eq!(app.editor.text(), long);
        assert_eq!(app.panel.attached, ["q1", "q2"]);
        assert_eq!(app.hint(), Some(refusal));
        // Not attached, while the agent waits for q2: the answer too.
        app.panel.attached.clear();
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_effects().is_empty());
        assert_eq!(app.editor.text(), long);
        // Shortened, it goes.
        app.editor.take();
        type_text(&mut app, "main");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["main"]);
        assert!(app.editor.text().is_empty());
    }

    /// New questions ring the bell once while the window isn't focused.
    #[test]
    fn new_questions_ring_the_bell_when_unfocused() {
        let mut app = app();
        app.set_reading(asking());
        assert!(!app.take_ring(), "focused");
        let mut app = super::tests::app();
        app.focused = false;
        app.set_reading(asking());
        assert!(app.take_ring());
        assert!(!app.take_ring());
        // The same questions again, or fewer, ring no more.
        let mut fewer = asking();
        fewer.questions.truncate(1);
        app.set_reading(fewer);
        assert!(!app.take_ring());
        app.set_reading(asking());
        assert!(app.take_ring(), "q2 is new again");
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
                title: "Test it".into(),
                state: crate::remote::WorkerState::Running,
                started_at: Some(chrono::Utc::now().timestamp_millis()),
                ..crate::remote::WorkerSummary::new("r1")
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
        app.list_drawn = (
            ratatui::layout::Rect::new(0, 20, 80, 2),
            vec![ListRow::Agent(0), ListRow::Agent(1)],
        );
        click(&mut app, 5, 21);
        assert_eq!(app.watching.as_deref(), Some("r1"));
        assert!(app.take_effects().is_empty());
    }

    /// Continuing a worker whose cache expired, with `/message` or from its
    /// view, keeps the text in the box and says what it costs. Enter again
    /// sends it, marked confirmed. Esc, or other text, asks again, and a
    /// warm cache or a small conversation sends at once.
    #[test]
    fn enter_twice_continues_a_worker_whose_cache_expired() {
        use crate::remote::WorkerState::Finished;
        let mut app = app();
        let now = chrono::Utc::now().timestamp_millis();
        let ended = |expires_at: i64, tokens: u64| crate::remote::WorkerSummary {
            cache_expires_at: Some(expires_at),
            reread_tokens: Some(tokens),
            reread_dollars: Some(1.36),
            ..worker("r3", Finished)
        };
        let two_hours_ago = now - 2 * 3_600_000;
        app.update(
            Update::Worker(ended(two_hours_ago, 340_000)),
            Activity::default,
        );
        let messages = |app: &mut App| -> Vec<(String, bool)> {
            app.take_effects()
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Input(Input::Message(message)) => {
                        Some((message.text, message.continue_uncached))
                    }
                    Effect::Input(Input::Line(line)) => Some((line, false)),
                    _ => None,
                })
                .collect()
        };
        let warning = "r3's cache expired 2 hours ago. Continuing re-reads about 340k tokens without it (≈ $1.36 at API prices). Enter again continues it.";
        let enter = |app: &mut App| press(app, KeyCode::Enter, KeyModifiers::NONE);

        // `/message`: the first Enter asks, the second sends.
        type_text(&mut app, "/message r3 Go on.");
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        assert_eq!(app.editor.text(), "/message r3 Go on.");
        assert_eq!(app.hint(), Some(warning));
        enter(&mut app);
        assert_eq!(
            messages(&mut app),
            [("/message r3 Go on.".to_string(), true)]
        );
        assert!(app.editor.is_empty());

        // The worker in capitals is the same worker. (The box takes no
        // tabs.)
        type_text(&mut app, "/message R3 Go on.");
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        assert_eq!(app.hint(), Some(warning));
        enter(&mut app);
        assert_eq!(
            messages(&mut app),
            [("/message R3 Go on.".to_string(), true)]
        );

        // Enter again after the 30 seconds asks again.
        type_text(&mut app, "/message r3 Go on.");
        enter(&mut app);
        let (line, _) = app.continue_asked.take().unwrap();
        let long_ago = Instant::now()
            .checked_sub(CONTINUE_CONFIRM_WITHIN + Duration::from_secs(1))
            .expect("a clock that goes back 31 seconds");
        app.continue_asked = Some((line, long_ago));
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        enter(&mut app);
        assert_eq!(
            messages(&mut app),
            [("/message r3 Go on.".to_string(), true)]
        );

        // Esc in between, or other text, asks again.
        type_text(&mut app, "/message r3 Go on.");
        enter(&mut app);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        type_text(&mut app, " Now.");
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        assert_eq!(app.editor.text(), "/message r3 Go on. Now.");
        app.editor.clear();

        // In r3's own view, the text goes to r3 the same way.
        app.watch(Some("r3".into()));
        type_text(&mut app, "Go on.");
        enter(&mut app);
        assert!(messages(&mut app).is_empty());
        assert_eq!(app.editor.text(), "Go on.");
        assert_eq!(app.hint(), Some(warning));
        enter(&mut app);
        assert_eq!(
            messages(&mut app),
            [("/message r3 Go on.".to_string(), true)]
        );

        // A warm cache, or a small conversation, sends at once.
        for (expires_at, tokens) in [(now + 300_000, 340_000), (two_hours_ago, 20_000)] {
            app.update(Update::Worker(ended(expires_at, tokens)), Activity::default);
            type_text(&mut app, "Go on.");
            enter(&mut app);
            assert_eq!(
                messages(&mut app),
                [("/message r3 Go on.".to_string(), false)]
            );
        }
    }

    /// A click (pressed and let go) at column `x`, row `y`.
    fn click(app: &mut App, x: u16, y: u16) {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
        }
    }

    /// A worker `id` in `state`, started a minute ago.
    pub fn worker(id: &str, state: crate::remote::WorkerState) -> crate::remote::WorkerSummary {
        let now = chrono::Utc::now().timestamp_millis();
        crate::remote::WorkerSummary {
            title: format!("Task of {id}"),
            state,
            tool_calls: 3,
            started_at: Some(now - 60_000),
            ended_at: (!state.is_active()).then_some(now),
            ..crate::remote::WorkerSummary::new(id)
        }
    }

    /// A worker `id` that ended in `state` six minutes ago, so it has left
    /// the agent list.
    pub fn ended_earlier(
        id: &str,
        state: crate::remote::WorkerState,
    ) -> crate::remote::WorkerSummary {
        let mut summary = worker(id, state);
        summary.ended_at = summary.ended_at.map(|at| at - LIST_ENDED_MS - 60_000);
        summary
    }

    /// The list's rows: the main agent and up to five workers, with a count
    /// of the workers above them and one of those below, when there are.
    /// The workers shown follow the focus, and a short screen shows fewer.
    #[test]
    fn the_list_shows_five_workers_and_counts_those_above_and_below() {
        use ListRow::{Above, Agent, Both, More};
        let rows = |count, focus, height| list_rows(count, 0, focus, true, height).rows;
        assert!(rows(0, None, 7).is_empty());
        assert_eq!(rows(4, None, 7), [Agent(0), Agent(1), Agent(2), Agent(3)]);
        assert_eq!(rows(6, None, 7), (0..6).map(Agent).collect::<Vec<_>>());
        // Seven workers: five, and "… 2 more".
        let top = [
            Agent(0),
            Agent(1),
            Agent(2),
            Agent(3),
            Agent(4),
            Agent(5),
            More(2),
        ];
        assert_eq!(rows(8, None, 7), top);
        assert_eq!(rows(8, Some(5), 7), top);
        // At the bottom, nothing is below: "… 2 more" on top.
        let bottom = [
            Agent(0),
            Above(2),
            Agent(3),
            Agent(4),
            Agent(5),
            Agent(6),
            Agent(7),
        ];
        assert_eq!(rows(8, Some(6), 7), bottom);
        assert_eq!(rows(8, Some(7), 7), bottom);
        // In between, both.
        assert_eq!(
            list_rows(8, 1, None, false, 7),
            ListWindow {
                rows: vec![
                    Agent(0),
                    Above(1),
                    Agent(2),
                    Agent(3),
                    Agent(4),
                    Agent(5),
                    More(2)
                ],
                scroll: 1,
                most: 2,
            }
        );
        // One more worker than fits: at the bottom, none is below.
        assert_eq!(
            rows(7, Some(6), 7),
            [
                Agent(0),
                Above(1),
                Agent(2),
                Agent(3),
                Agent(4),
                Agent(5),
                Agent(6)
            ]
        );
        // Back up, the workers shown follow the focus to the top.
        let window = list_rows(8, 2, Some(2), true, 7);
        assert_eq!(
            (window.scroll, &window.rows[1..3]),
            (1, &[Above(1), Agent(2)][..])
        );
        assert_eq!(list_rows(8, 2, Some(1), true, 7).rows, top);
        // Scrolled past the end, it stops at the bottom.
        assert_eq!(list_rows(8, 9, None, false, 7).rows, bottom);
        // On a short screen: fewer workers, and the counts.
        assert_eq!(rows(8, None, 4), [Agent(0), Agent(1), Agent(2), More(5)]);
        assert_eq!(rows(8, Some(4), 4), [Agent(0), Above(3), Agent(4), More(3)]);
        // Too short for both counts: both on one row.
        assert_eq!(rows(4, Some(2), 3), [Agent(0), Agent(2), Both(1, 1)]);
        assert_eq!(rows(4, Some(3), 3), [Agent(0), Above(2), Agent(3)]);
        assert_eq!(rows(11, Some(9), 3), [Agent(0), Agent(9), Both(8, 1)]);
        assert_eq!(rows(3, None, 2), [Agent(0), More(2)]);
        assert_eq!(rows(3, Some(2), 1), [Agent(2)]);
    }

    /// The worker selected stays in the list as workers arrive and the area
    /// shrinks, unless the wheel moved the list since.
    #[test]
    fn the_list_keeps_the_selected_worker_in_view() {
        use crate::remote::WorkerState::Running;
        let mut app = app();
        let add = |app: &mut App, n: usize| {
            app.update(
                Update::Worker(worker(&format!("r{n}"), Running)),
                Activity::default,
            )
        };
        for n in 1..=6 {
            add(&mut app, n);
        }
        app.list_focus = Some(6);
        assert!(app.list_rows(7).contains(&ListRow::Agent(6)));
        // A worker arrives: the selected one stays.
        add(&mut app, 7);
        assert!(app.list_rows(7).contains(&ListRow::Agent(6)));
        // The area shrinks: it stays too.
        assert!(app.list_rows(4).contains(&ListRow::Agent(6)));
        assert!(app.list_rows(3).contains(&ListRow::Agent(6)));
        // The wheel moves the list away from it, and it stays where the
        // wheel left it, until the selection moves.
        let rows = app.list_rows(7);
        app.list_drawn = (ratatui::layout::Rect::new(0, 30, 80, 7), rows);
        for _ in 0..5 {
            app.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 32,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(!app.list_rows(7).contains(&ListRow::Agent(6)));
        add(&mut app, 8);
        assert!(!app.list_rows(7).contains(&ListRow::Agent(6)));
        app.list_focus = Some(7);
        assert!(app.list_rows(7).contains(&ListRow::Agent(7)));
    }

    /// The list has the workers running or waiting, not those that ended
    /// more than five minutes ago (unless one is shown). Past five, ↓ and a
    /// click on the count reach the others.
    #[test]
    fn the_list_drops_ended_workers_and_reaches_the_others() {
        use crate::remote::WorkerState::{Failed, Finished, Queued, Running, Stopped};
        let mut app = app();
        let states = [
            Running, Finished, Running, Failed, Queued, Running, Stopped, Running, Running, Running,
        ];
        for (n, state) in states.into_iter().enumerate() {
            app.update(
                Update::Worker(ended_earlier(&format!("r{}", n + 1), state)),
                Activity::default,
            );
        }
        let ids = |app: &App| -> Vec<String> {
            app.list()
                .iter()
                .map(|row| row.map_or("main".to_string(), |w| w.id.clone()))
                .collect()
        };
        assert_eq!(
            ids(&app),
            ["main", "r1", "r3", "r5", "r6", "r8", "r9", "r10"]
        );
        use ListRow::{Above, Agent, More};
        assert_eq!(
            app.list_rows(7),
            [
                Agent(0),
                Agent(1),
                Agent(2),
                Agent(3),
                Agent(4),
                Agent(5),
                More(2)
            ]
        );
        // A worker that ended stays while it is shown.
        app.watch(Some("r2".into()));
        assert_eq!(ids(&app)[..3], ["main", "r1", "r2"]);
        app.watch(None);
        assert!(!ids(&app).contains(&"r2".to_string()));
        // ↓ past the fifth worker brings the next into the list, under the
        // count of those above it.
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        for _ in 0..6 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(app.list_focus, Some(6));
        let bottom = [
            Agent(0),
            Above(2),
            Agent(3),
            Agent(4),
            Agent(5),
            Agent(6),
            Agent(7),
        ];
        assert_eq!(app.list_rows(7), bottom);
        // A click on the count above moves back to the worker before those
        // shown, and one on the count below on to the next after them.
        let area = ratatui::layout::Rect::new(0, 30, 80, 7);
        app.list_drawn = (area, app.list_rows(7));
        click(&mut app, 5, 31);
        assert_eq!(app.list_focus, Some(2));
        assert_eq!(
            app.list_rows(7),
            [
                Agent(0),
                Above(1),
                Agent(2),
                Agent(3),
                Agent(4),
                Agent(5),
                More(2)
            ]
        );
        app.list_drawn = (area, app.list_rows(7));
        click(&mut app, 5, 36);
        assert_eq!(app.list_focus, Some(6));
        assert_eq!(app.watching, None);
        assert_eq!(app.list_rows(7), bottom);
        // The wheel over the list scrolls it, as far as it goes.
        let wheel = |app: &mut App, kind| {
            app.mouse(MouseEvent {
                kind,
                column: 5,
                row: 32,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.list_drawn = (area, app.list_rows(7));
        for _ in 0..3 {
            wheel(&mut app, MouseEventKind::ScrollUp);
        }
        let top = [
            Agent(0),
            Agent(1),
            Agent(2),
            Agent(3),
            Agent(4),
            Agent(5),
            More(2),
        ];
        assert_eq!(app.list_rows(7), top);
        for _ in 0..5 {
            wheel(&mut app, MouseEventKind::ScrollDown);
        }
        assert_eq!(app.list_rows(7), bottom);
        // A worker that ends stays in its place for five minutes, and the
        // clock redraws the list until it leaves.
        app.update(Update::Worker(worker("r1", Finished)), Activity::default);
        assert_eq!(ids(&app)[..3], ["main", "r1", "r3"]);
        app.update(Update::Worker(worker("r3", Failed)), Activity::default);
        assert_eq!(ids(&app)[..3], ["main", "r1", "r3"]);
        for id in ["r5", "r6", "r8", "r9", "r10"] {
            app.update(Update::Worker(worker(id, Stopped)), Activity::default);
        }
        assert_eq!(ids(&app).len(), 8);
        assert!(app.list_ticks());
        app.list_now += LIST_ENDED_MS + 1000;
        assert!(ids(&app).is_empty());
        assert!(!app.list_ticks());
    }

    /// Tab from the input box goes on to the agent list, moves down it, and
    /// past its last agent goes back to the box. Shift+Tab goes the other
    /// way. A command's completion in the box keeps Tab.
    #[test]
    fn tab_moves_through_the_agent_list_and_back_to_the_box() {
        use crate::remote::WorkerState::Running;
        let mut app = app();
        // No workers, so no list to go to.
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.list_focus, None);
        for id in ["r1", "r2"] {
            app.update(Update::Worker(worker(id, Running)), Activity::default);
        }
        let focus = |app: &mut App, code, modifiers| {
            (0..4)
                .map(|_| {
                    press(app, code, modifiers);
                    app.list_focus
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            focus(&mut app, KeyCode::Tab, KeyModifiers::NONE),
            [Some(0), Some(1), Some(2), None]
        );
        assert_eq!(
            focus(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT),
            [Some(2), Some(1), Some(0), None]
        );
        assert!(app.editor.is_empty());
        // In the box, Tab still takes an argument's choice.
        type_text(&mut app, "/view q");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/view quiet");
        assert_eq!(app.list_focus, None);
    }

    #[test]
    fn the_api_prices_setting_is_saved_without_a_session_command() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.json");
        let mut app = app();
        app.info.prefs = Some(path.clone());
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::ApiPrices)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        assert!(!app.prefs.api_prices);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(Prefs::load(Some(&path)).api_prices);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert!(!Prefs::load(Some(&path)).api_prices);
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
        assert_eq!(
            sent(&mut app),
            ["/handover-at main 300k", "/handover-at main 200k"]
        );
        // The workers' size, in a session with workers, the same way.
        assert!(!app.settings_rows().contains(&SettingsRow::WorkersHandover));
        let mut reading = app.reading.clone();
        reading.models.has_workers = true;
        reading.settings.workers_handover_at = 500_000;
        app.set_reading(reading);
        let rows = app.settings_rows();
        let workers = rows
            .iter()
            .position(|row| *row == SettingsRow::WorkersHandover)
            .unwrap();
        assert_eq!(workers, row + 1, "under the main agent's");
        app.overlay = Some(Overlay::Settings {
            row: workers,
            picker: None,
        });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/handover-at workers 750k", "/handover-at workers 400k"]
        );
    }

    /// Ctrl+B moves the shell command of the agent shown to the background,
    /// the main agent's or a watched worker's. Without one it moves back a
    /// character, as in readline.
    #[test]
    fn ctrl_b_moves_the_command_of_the_agent_shown() {
        let moved = |app: &mut App| -> Vec<String> {
            app.take_effects()
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Input(Input::Background(agent)) => Some(agent),
                    _ => None,
                })
                .collect()
        };
        let mut app = app();
        type_text(&mut app, "ab");
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        type_text(&mut app, "X");
        assert_eq!(app.editor.text(), "aXb");
        assert!(moved(&mut app).is_empty());

        app.reading.commands = vec!["main".into()];
        assert!(app.command_running());
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(moved(&mut app), ["main"]);
        assert_eq!(app.editor.text(), "aXb", "the box is left as it was");
        // Until the status says again, a second press is a key again.
        assert!(!app.command_running());

        app.reading.commands = vec!["main".into()];
        app.watching = Some("r1".into());
        assert!(!app.command_running(), "the main agent's isn't shown");
        app.reading.commands = vec!["main".into(), "r1".into()];
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(moved(&mut app), ["r1"]);
    }

    /// When commands move to the background is a session setting: the row
    /// steps through round times, down to off, with `/background-after`.
    #[test]
    fn the_background_row_sends_the_command() {
        let mut app = app();
        let rows = app.settings_rows();
        let row = rows
            .iter()
            .position(|row| *row == SettingsRow::Background)
            .unwrap();
        let outside = rows
            .iter()
            .position(|row| *row == SettingsRow::Outside)
            .unwrap();
        assert_eq!(row, outside + 1);
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/background-after 1m"]);
        app.reading.settings.background_after = 600;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/background-after 15m", "/background-after 5m"]
        );
        app.reading.settings.background_after = 60;
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/background-after off"]);
    }

    /// How long monitors' warnings wake the agent is a session setting, after
    /// when commands move to the background: the row steps from off to
    /// always with `/monitor-wakes`.
    #[test]
    fn the_monitor_wakes_row_sends_the_command() {
        let mut app = app();
        let rows = app.settings_rows();
        let row = rows
            .iter()
            .position(|row| *row == SettingsRow::MonitorWakes)
            .unwrap();
        let background = rows
            .iter()
            .position(|row| *row == SettingsRow::Background)
            .unwrap();
        assert_eq!(row, background + 1);
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/monitor-wakes 1h", "/monitor-wakes 30m"]);
        app.reading.settings.monitor_wakes = crate::monitors::Wakes::For(8 * 3600);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/monitor-wakes always"]);
        app.reading.settings.monitor_wakes = crate::monitors::Wakes::For(1800);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/monitor-wakes off"]);
    }

    /// Who chooses the workers' models, how deep workers go, how many failed
    /// requests stop a worker and whether the starter of a worker gets a copy
    /// of the user's messages to it are the last session settings, with the
    /// workers. The rows step with `/worker-models`, `/worker-depth`,
    /// `/worker-retry-cap` and `/message-copies`, and a session with no
    /// workers has none of them.
    #[test]
    fn the_worker_rows_send_the_commands() {
        let mut app = app();
        assert!(
            !app.settings_rows().contains(&SettingsRow::WorkerModels),
            "no workers, no rows"
        );
        app.reading.models.has_workers = true;
        let rows = app.settings_rows();
        let models = rows
            .iter()
            .position(|row| *row == SettingsRow::WorkerModels)
            .unwrap();
        assert_eq!(rows[models + 1], SettingsRow::WorkerBilling);
        assert_eq!(rows[models + 2], SettingsRow::ApiBudget);
        let models = models + 2;
        assert_eq!(rows[models + 1], SettingsRow::WorkerDepth);
        assert_eq!(rows[models + 2], SettingsRow::WorkerRetryCap);
        assert_eq!(rows[models + 3], SettingsRow::MessageCopies);
        assert_eq!(rows[models + 4], SettingsRow::RepoRootProfiles);
        assert_eq!(rows[models + 5], SettingsRow::Keys);
        // Worker billing steps through plans, ask and api, ask by default.
        app.overlay = Some(Overlay::Settings {
            row: models - 1,
            picker: None,
        });
        assert_eq!(app.worker_billing(), crate::workers::WorkerBilling::Ask);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/worker-billing api", "/worker-billing plans"]
        );
        // The budget turns on (at $10) and off.
        app.overlay = Some(Overlay::Settings {
            row: models,
            picker: None,
        });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        app.reading.settings.api_budget = Some(25.0);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/api-budget on", "/api-budget off"]);
        app.overlay = Some(Overlay::Settings {
            row: models - 2,
            picker: None,
        });
        // The main agent chooses by default: right goes to profiles, left
        // round to the session's model.
        assert_eq!(app.worker_models(), crate::agents::ModelChoice::Agent);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/worker-models profiles", "/worker-models session"]
        );
        app.reading.settings.worker_models = crate::agents::ModelChoice::Session;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/worker-models agent"]);
        // The depth steps by one, from 1 to 4.
        app.overlay = Some(Overlay::Settings {
            row: models + 1,
            picker: None,
        });
        app.reading.settings.worker_depth = 2;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/worker-depth 3", "/worker-depth 1"]);
        app.reading.settings.worker_depth = 4;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert!(sent(&mut app).is_empty(), "4 is the most");
        app.reading.settings.worker_depth = 1;
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert!(sent(&mut app).is_empty(), "1 is the least");
        // The cap turns on (at 10) and off, either way the arrow goes.
        app.overlay = Some(Overlay::Settings {
            row: models + 2,
            picker: None,
        });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        app.reading.settings.worker_retry_cap = Some(5);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/worker-retry-cap on", "/worker-retry-cap off"]
        );
        // The copies turn off and on, either way the arrow goes.
        app.overlay = Some(Overlay::Settings {
            row: models + 3,
            picker: None,
        });
        app.reading.settings.message_copies = true;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        app.reading.settings.message_copies = false;
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/message-copies off", "/message-copies on"]
        );
    }

    /// The repo root row is the user's for every project, so it is saved in
    /// Arko's folder and not sent to the session, and it leaves the git switch
    /// as the session last saved it.
    #[test]
    fn the_repo_root_row_is_saved_in_arkos_folder() {
        use crate::profiles::Preferences;
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Snapshot::default(),
            Editor::new(History::load(None)),
            Info {
                root: Some(temp.path().to_path_buf()),
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        );
        app.reading.models.has_workers = true;
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::RepoRootProfiles)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        assert!(app.profile_prefs.repo_root, "on at first");
        // (The session turned the git switch off meanwhile.)
        Preferences {
            keep_out_of_git: false,
            ..Preferences::default()
        }
        .save(temp.path())
        .unwrap();
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert!(!Preferences::load(temp.path()).repo_root, "off in the file");
        assert!(!app.profile_prefs.repo_root);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert!(Preferences::load(temp.path()).repo_root, "on again");
        assert!(
            !Preferences::load(temp.path()).keep_out_of_git,
            "the git switch stays as it was saved"
        );
        assert!(sent(&mut app).is_empty(), "nothing goes to the session");
    }

    /// The git row is the session's to save, so it sends `/extensions` and
    /// shows what the session reports.
    #[test]
    fn the_git_row_sends_the_command() {
        let mut app = app();
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::KeepProfilesOutOfGit)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        assert!(
            app.reading.settings.keep_agents_files_out_of_git,
            "on at first"
        );
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        app.reading.settings.keep_agents_files_out_of_git = false;
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            [
                "/extensions keep-out-of-git off",
                "/extensions keep-out-of-git on"
            ]
        );
    }

    /// The approval rows send `/extensions`. Turning asking about MCP servers
    /// off asks first, and Cancel or Esc sends nothing. Turning it on and the
    /// other row need no question.
    #[test]
    fn the_approval_rows_send_the_commands_and_turning_asking_off_asks_first() {
        let mut app = app();
        let rows = app.settings_rows();
        let ask = rows
            .iter()
            .position(|row| *row == SettingsRow::AskBeforeMcp)
            .unwrap();
        assert_eq!(rows[ask + 1], SettingsRow::ReviewProjectSkills);
        assert_eq!(rows[ask + 2], SettingsRow::KeepProfilesOutOfGit);
        app.reading.settings.ask_before_adding_mcp = true;
        app.overlay = Some(Overlay::Settings {
            row: ask,
            picker: None,
        });
        let settings = Some(Overlay::Settings {
            row: ask,
            picker: None,
        });
        // The question opens on Cancel, and Enter there changes nothing.
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            app.overlay,
            Some(Overlay::AskOff {
                row: ask,
                choice: 1
            })
        );
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.overlay, settings);
        // Esc is Cancel too.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            app.overlay,
            Some(Overlay::AskOff {
                row: ask,
                choice: 0
            })
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.overlay, settings);
        assert!(sent(&mut app).is_empty(), "nothing is sent until Turn off");
        // Turn off sends the command and goes back to the settings.
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            app.overlay,
            Some(Overlay::AskOff {
                row: ask,
                choice: 0
            })
        );
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.overlay, settings);
        assert_eq!(sent(&mut app), ["/extensions ask-mcp off"]);
        // Turning it on asks nothing.
        app.reading.settings.ask_before_adding_mcp = false;
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.overlay, settings);
        assert_eq!(sent(&mut app), ["/extensions ask-mcp on"]);
        // The skills review turns on and off.
        app.overlay = Some(Overlay::Settings {
            row: ask + 1,
            picker: None,
        });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        app.reading.settings.review_project_skills = true;
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            [
                "/extensions review-skills on",
                "/extensions review-skills off"
            ]
        );
    }

    /// Opening the settings reads the repo root row again, as another
    /// program may have changed it.
    #[test]
    fn the_settings_read_the_repo_root_row_when_they_open() {
        use crate::profiles::Preferences;
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Snapshot::default(),
            Editor::new(History::load(None)),
            Info {
                root: Some(temp.path().to_path_buf()),
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        );
        assert!(app.profile_prefs.repo_root);
        Preferences {
            repo_root: false,
            ..Preferences::default()
        }
        .save(temp.path())
        .unwrap();
        app.editor.set("/settings");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.profile_prefs.repo_root);
    }

    /// Text replies are a session setting: the row turns them on or off with
    /// `/text-replies`, and says what they do and whether they can change.
    #[test]
    fn the_text_replies_row_sends_the_command() {
        let mut app = app();
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::Replies)
            .unwrap();
        let wakes = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::MonitorWakes)
            .unwrap();
        assert_eq!(row, wakes + 1, "beside the other session settings");
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/text-replies on"]);
        let mut reading = app.reading.clone();
        reading.settings.text_replies = true;
        app.set_reading(reading);
        assert!(app.text_replies());
        assert!(!app.conversation_started());
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/text-replies off"]);
    }

    /// The extra rules are a session setting: the row steps through small,
    /// all and off with `/extra-rules`, beside text replies.
    #[test]
    fn the_extra_rules_row_sends_the_command() {
        let mut app = app();
        let rows = app.settings_rows();
        let row = rows
            .iter()
            .position(|row| *row == SettingsRow::Rules)
            .unwrap();
        let replies = rows
            .iter()
            .position(|row| *row == SettingsRow::Replies)
            .unwrap();
        assert_eq!(row, replies + 1, "beside the other session settings");
        app.overlay = Some(Overlay::Settings { row, picker: None });
        // Off, as a session saved before the rules has them, steps round to
        // small.
        assert_eq!(app.extra_rules(), ExtraRules::Off);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/extra-rules small"]);
        let mut reading = app.reading.clone();
        reading.settings.extra_rules = ExtraRules::Small;
        app.set_reading(reading);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/extra-rules all", "/extra-rules off"]);
    }

    /// What agents in Claude Code do at the handover size is a session
    /// setting, shown only when an agent of the session is on a cc: model:
    /// the row has them compact or hand over with `/cc-compact`.
    #[test]
    fn the_claude_code_row_sends_the_command() {
        let mut app = app();
        assert!(!app.settings_rows().contains(&SettingsRow::Compact));
        let mut reading = app.reading.clone();
        reading.models.workers = "cc:opus".into();
        reading.settings.cc_compact = true;
        app.set_reading(reading);
        let rows = app.settings_rows();
        let row = rows
            .iter()
            .position(|row| *row == SettingsRow::Compact)
            .unwrap();
        let handover = rows
            .iter()
            .position(|row| *row == SettingsRow::Handover)
            .unwrap();
        assert_eq!(row, handover + 1, "beside the handover size");
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/cc-compact off"]);
        let mut reading = app.reading.clone();
        reading.settings.cc_compact = false;
        app.set_reading(reading);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/cc-compact on"]);
    }

    /// The project's instructions are a group of their own, after the
    /// session's settings: which files, stepping through one, both and off,
    /// and (in a session with workers) whether workers get them.
    #[test]
    fn the_project_instructions_rows_send_the_command() {
        let mut app = app();
        let rows = app.settings_rows();
        let files = rows
            .iter()
            .position(|row| *row == SettingsRow::ProjectFiles)
            .unwrap();
        let handover = rows
            .iter()
            .position(|row| *row == SettingsRow::Handover)
            .unwrap();
        assert_eq!(files, handover + 1, "after the session's settings");
        assert!(!rows.contains(&SettingsRow::ProjectWorkers), "no workers");
        app.overlay = Some(Overlay::Settings {
            row: files,
            picker: None,
        });
        assert_eq!(app.project_instructions(), project::Files::One);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/project-instructions both", "/project-instructions off"]
        );

        let mut reading = app.reading.clone();
        reading.models.has_workers = true;
        reading.settings.worker_project_instructions = true;
        app.set_reading(reading);
        let rows = app.settings_rows();
        let workers = rows
            .iter()
            .position(|row| *row == SettingsRow::ProjectWorkers)
            .unwrap();
        let files = rows
            .iter()
            .position(|row| *row == SettingsRow::ProjectFiles)
            .unwrap();
        assert_eq!(workers, files + 1);
        app.overlay = Some(Overlay::Settings {
            row: workers,
            picker: None,
        });
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/project-instructions workers off"]);
    }

    /// The built-in skills are a group of their own after the project's
    /// instructions: a row for all of them, then one for each, which send
    /// `/builtin-skills`.
    #[test]
    fn the_built_in_skills_rows_send_the_command() {
        use crate::remote::BuiltinSkillSetting;
        let mut app = app();
        let skill = |name: &str, on| BuiltinSkillSetting {
            name: name.into(),
            on,
        };
        let mut reading = app.reading.clone();
        reading.settings.builtin_skills = true;
        reading.settings.builtin_skill_list = vec![
            skill("user-facing-prose", true),
            skill("repo-docs-style", false),
        ];
        app.set_reading(reading);
        let rows = app.settings_rows();
        let all = rows
            .iter()
            .position(|row| *row == SettingsRow::BuiltinSkills)
            .unwrap();
        assert_eq!(rows[all + 1], SettingsRow::BuiltinSkill(0));
        assert_eq!(rows[all + 2], SettingsRow::BuiltinSkill(1));
        let files = rows
            .iter()
            .position(|row| *row == SettingsRow::ProjectFiles)
            .unwrap();
        assert_eq!(all, files + 1, "after the project's instructions");
        for (row, expected) in [
            (all, "/builtin-skills off"),
            (all + 1, "/builtin-skills user-facing-prose off"),
            (all + 2, "/builtin-skills repo-docs-style on"),
        ] {
            app.overlay = Some(Overlay::Settings { row, picker: None });
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(sent(&mut app), [expected]);
        }
    }

    /// The web page's overlay waits for the page until its status has it:
    /// the view reads the status again as the page's links come in the feed.
    #[test]
    fn the_page_overlay_waits_for_the_page() {
        let mut app = app();
        assert!(!app.waits_for_page(), "no overlay");
        app.overlay = Some(Overlay::Remote {
            link: 0,
            since: Instant::now(),
        });
        assert!(app.waits_for_page());
        let mut reading = app.reading.clone();
        reading.page = Some(crate::remote::Page {
            addr: "127.0.0.1:8080".parse().unwrap(),
            links: Vec::new(),
            note: None,
        });
        app.set_reading(reading);
        assert!(!app.waits_for_page());
    }

    /// The keys menu opens from the settings and goes back there, and from
    /// `/keys add PROVIDER` on that key's field. A paste goes into the
    /// field, never into the box, and the key reaches only the session's
    /// keys.
    #[test]
    fn the_keys_menu_opens_from_the_settings_and_takes_a_paste() {
        use crate::credentials;
        use crate::keys::Outcome as KeyOutcome;
        let mut app = app();
        app.key_states = keys_menu::tests::states;
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::Keys)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(&app.overlay, Some(Overlay::Keys(menu)) if menu.back_to == Some(row)));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.overlay, Some(Overlay::Settings { row, picker: None }));
        app.overlay = None;

        type_text(&mut app, "/keys add openai");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(sent(&mut app).is_empty(), "the view handles /keys");
        assert!(matches!(&app.overlay, Some(Overlay::Keys(menu))
            if matches!(menu.step, keys_menu::Step::Entering { .. })));
        let key = "sk-proj-0123456789abcdefwxyz";
        app.paste(&format!("{key}\n"));
        assert!(app.editor.is_empty());
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.take_effects(),
            [Effect::AddKey {
                key: &credentials::OPENAI,
                value: Secret::new(key)
            }]
        );
        app.keys_done(
            &credentials::OPENAI,
            KeyOutcome::Done {
                said: "Arko saved the OpenAI API key.".into(),
                warnings: Vec::new(),
            },
        );
        assert!(matches!(&app.overlay, Some(Overlay::Keys(menu))
            if menu.step == keys_menu::Step::List && menu.said.is_some()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        // Closed, the status line says what became of it.
        app.keys_done(
            &credentials::OPENAI,
            KeyOutcome::Failed("Could not save /home/me/.env: Permission denied".into()),
        );
        assert_eq!(
            app.hint(),
            Some("Could not save /home/me/.env: Permission denied")
        );
        // The box's history has the command, not the key.
        app.editor.history_back();
        assert_eq!(app.editor.text(), "/keys add openai");
    }

    /// `/session-names` alone opens the menu at once, on the ways saved,
    /// and what each lacks comes from a check that runs in the background.
    /// Enter saves as the line the session takes, and words after the
    /// command go to the session as they are.
    #[test]
    fn the_session_names_menu_opens_and_sends_the_line_that_saves() {
        use crate::session_names::Way;
        let temp = tempfile::tempdir().unwrap();
        crate::session_names::save(temp.path(), &[Way::ClaudePlan, Way::ChatGptPlan]).unwrap();
        let mut app = app();
        // Without Arko's folder, the session answers.
        type_text(&mut app, "/session-names");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/session-names"]);
        assert!(app.overlay.is_none());
        app.info.root = Some(temp.path().to_path_buf());
        type_text(&mut app, "/session-names");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        // The view opens the menu, and asks for the check, which the menu
        // doesn't wait for.
        let effects = app.take_effects();
        assert!(
            matches!(effects.as_slice(), [Effect::CheckNames(_)]),
            "{effects:?}"
        );
        let Some(Overlay::Names(menu)) = &app.overlay else {
            panic!("no menu")
        };
        assert!(menu.by_model);
        assert_eq!(menu.ways[0], (Way::ClaudePlan, true));
        assert_eq!(menu.missing, None, "not checked yet");
        // Move the ChatGPT plan up, turn the Anthropic key on, and save.
        for _ in 0..3 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        // What the check finds joins the menu while it is open.
        let check = app.names_check().unwrap();
        app.names_checked(&check, names_menu::tests::missing());
        let Some(Overlay::Names(menu)) = &app.overlay else {
            panic!("no menu")
        };
        assert_eq!(menu.missing, Some(names_menu::tests::missing()));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            sent(&mut app),
            ["/session-names chatgpt-plan claude-plan claude-key"]
        );
        assert!(app.overlay.is_none());
        // The first words are saved as `words`, and Esc saves nothing.
        type_text(&mut app, "/session-names");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/session-names words"]);
        type_text(&mut app, "/session-names");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none() && sent(&mut app).is_empty());
        // With words, the session takes the line.
        type_text(&mut app, "/session-names claude-key");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/session-names claude-key"]);
        assert!(app.overlay.is_none());
    }

    /// A check of what the ways lack that is slow doesn't hold the menu:
    /// it opens and closes without it, closing ends the check, and what it
    /// finds after the menu closed is dropped, also by a menu opened again.
    #[test]
    fn a_slow_check_of_the_ways_does_not_hold_the_menu() {
        use crate::session_names::Way;
        use std::sync::atomic::Ordering;
        let temp = tempfile::tempdir().unwrap();
        let mut app = app();
        app.info.root = Some(temp.path().to_path_buf());
        app.open_names();
        let effects = app.take_effects();
        let [Effect::CheckNames(first)] = effects.as_slice() else {
            panic!("no check")
        };
        let first = first.clone();
        assert!(!first.0.load(Ordering::Relaxed));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none(), "closed before the check was done");
        assert!(first.0.load(Ordering::Relaxed), "closing ends the check");
        // What it finds later reaches no one.
        app.names_checked(&first, names_menu::tests::missing());
        assert!(app.overlay.is_none());
        // Opened again, the old check is over and the new one's answer shows.
        app.open_names();
        let effects = app.take_effects();
        let [Effect::CheckNames(second)] = effects.as_slice() else {
            panic!("no check")
        };
        let second = second.clone();
        assert_ne!(second, first);
        app.open_names();
        assert!(
            second.0.load(Ordering::Relaxed),
            "opening again ends it too"
        );
        // What the second check found, already on its way to the loop when
        // the menu opened again, isn't for the menu that waits for the third.
        app.names_checked(&second, vec![(Way::ClaudeKey, Some("Stale".into()))]);
        let Some(Overlay::Names(menu)) = &app.overlay else {
            panic!("no menu")
        };
        assert_eq!(menu.missing, None);
        // The third one's is.
        let third = app.names_check().unwrap();
        assert_ne!(third, second);
        app.names_checked(
            &third,
            vec![(Way::ClaudeKey, Some("No Anthropic API key".into()))],
        );
        let Some(Overlay::Names(menu)) = &app.overlay else {
            panic!("no menu")
        };
        assert_eq!(menu.missing.as_ref().map(Vec::len), Some(1));
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
            main_fast: false,
            ..Default::default()
        }
    }

    /// A command's argument completes from its choices: the rest shows
    /// dim, ↑ ↓ show the others, Tab takes one, and Enter sends only what
    /// is typed.
    #[test]
    fn arguments_complete_from_their_choices() {
        let mut app = app();
        let keys = crate::glyphs::render("↑↓");
        type_text(&mut app, "/view ");
        assert_eq!(app.ghost().as_deref(), Some("full|quiet|messages"));
        type_text(&mut app, "q");
        assert_eq!(app.ghost().as_deref(), Some("uiet"));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/view quiet");
        assert_eq!(app.ghost(), None);
        app.editor.clear();
        // Several match: the first, then ↓ the next. Enter takes neither.
        type_text(&mut app, "/outside o");
        assert_eq!(app.ghost(), Some(format!("n  {keys} 1 of 2")));
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.ghost(), Some(format!("ff  {keys} 2 of 2")));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/outside o"]);
        // → takes the choice shown, and nothing over the hint.
        type_text(&mut app, "/outside ");
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/outside ");
        type_text(&mut app, "of");
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/outside off");
        app.editor.clear();
        // ↑ before a letter, then the next word's choices, and Esc.
        type_text(&mut app, "/project-instructions ");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.ghost(), Some(format!("workers  {keys} 4 of 4")));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/project-instructions workers ");
        assert_eq!(app.ghost().as_deref(), Some("on|off"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.ghost(), None);
        type_text(&mut app, "o");
        assert_eq!(app.ghost(), Some(format!("n  {keys} 1 of 2")));
        app.editor.clear();
        // Free text: the hint until a letter, then nothing.
        type_text(&mut app, "/handover-at ");
        assert_eq!(app.ghost().as_deref(), Some("[main|workers] SIZE"));
        type_text(&mut app, "2");
        assert_eq!(app.ghost(), None);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/handover-at 2");
    }

    /// `/model` completes the models the picker offers, then their efforts.
    #[test]
    fn model_arguments_complete_from_the_picker() {
        let mut app = app();
        hermetic(&mut app);
        app.set_reading(Reading {
            models: models(false),
            ..Reading::default()
        });
        type_text(&mut app, "/model l");
        assert_eq!(app.ghost().as_deref(), Some("una"));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/model luna ");
        assert!(app.ghost().unwrap().starts_with("low|medium|high"));
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/model luna high");
        app.editor.clear();
        // Models the session can't reach aren't offered.
        type_text(&mut app, "/model op");
        assert_eq!(app.ghost(), None);
        app.editor.clear();
        type_text(&mut app, "/model w");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.ghost().unwrap().starts_with("main|sol|gpt-6.1-sol"));
        app.editor.clear();
        // Once the conversation starts, only the workers' model changes.
        app.set_reading(Reading {
            models: models(true),
            ..Reading::default()
        });
        type_text(&mut app, "/model ");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.ghost().as_deref(), Some("worker"));
    }

    /// `/model` completes a model on OpenRouter from its list once
    /// `openrouter:` is typed, for the main agent's model and the workers'.
    #[test]
    fn an_openrouter_model_completes_from_its_list() {
        use crate::openrouter::{Region, Settings, seed_for_tests};
        let temp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.set_zdr(true);
        settings.set_region(Region::Us);
        settings.save(temp.path()).unwrap();
        seed_for_tests(
            &settings,
            &[
                "moonshotai/kimi-k3",
                "moonshotai/kimi-k3-fast",
                "z-ai/glm-5",
            ],
        );
        let mut app = app();
        hermetic(&mut app);
        app.info.root = Some(temp.path().to_path_buf());
        app.set_reading(Reading {
            models: models(false),
            ..Reading::default()
        });
        let keys = crate::glyphs::render("↑↓");
        type_text(&mut app, "/model openrouter:z");
        assert_eq!(app.ghost().as_deref(), Some("-ai/glm-5"));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/model openrouter:z-ai/glm-5 ");
        assert!(app.ghost().unwrap().starts_with("low|medium|high"));
        app.editor.clear();
        type_text(&mut app, "/model openrouter:moon");
        assert_eq!(app.ghost(), Some(format!("shotai/kimi-k3  {keys} 1 of 2")));
        app.editor.clear();
        type_text(&mut app, "/model worker openrouter:z");
        assert_eq!(app.ghost().as_deref(), Some("-ai/glm-5"));
        app.editor.clear();
        // Drawing asks for the completions at every frame, which takes no
        // file: the settings are as this process has them.
        std::fs::remove_dir_all(temp.path()).unwrap();
        type_text(&mut app, "/model openrouter:z");
        for _ in 0..3 {
            assert_eq!(app.ghost().as_deref(), Some("-ai/glm-5"));
        }
        assert!(!temp.path().exists(), "no file was made either");
        app.editor.clear();
        // A letter or two doesn't bring the list in.
        type_text(&mut app, "/model o");
        assert!(
            !app.ghost().unwrap_or_default().contains("kimi"),
            "{:?}",
            app.ghost()
        );
    }

    /// `/openrouter` alone opens the settings menu, and with words goes to
    /// the session, which changes them. The words complete.
    #[test]
    fn openrouter_opens_a_menu_and_its_words_go_to_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app();
        app.info.root = Some(temp.path().to_path_buf());
        type_text(&mut app, "/openrouter");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(&app.overlay, Some(Overlay::OpenRouter(_))));
        assert!(sent(&mut app).is_empty());
        // A toggle in the menu is made away from the keys, and saves. (The
        // screen has a thread make it, and sends the result back.)
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let effects = app.take_effects();
        let [Effect::OpenRouter { root, edit }] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        assert!(!crate::openrouter::Settings::load(temp.path()).zdr());
        app.openrouter_done(crate::tui::openrouter_menu::make(root, edit));
        assert!(crate::openrouter::Settings::load(temp.path()).zdr());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        type_text(&mut app, "/openrouter region eu");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        assert_eq!(sent(&mut app), ["/openrouter region eu"]);
        // The words complete, a word after a word.
        type_text(&mut app, "/openrouter ");
        assert!(
            app.ghost()
                .unwrap()
                .starts_with("zdr on|off | data-collection")
        );
        type_text(&mut app, "r");
        assert_eq!(app.ghost().unwrap().split("  ").next(), Some("egion"));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "/openrouter region ");
        assert_eq!(app.ghost().as_deref(), Some("any|us|eu|us-or-eu"));
        type_text(&mut app, "us-");
        assert_eq!(app.ghost().as_deref(), Some("or-eu"));
        app.editor.clear();
        // Without Arko's folder there is no menu to open.
        let mut bare = super::tests::app();
        type_text(&mut bare, "/openrouter");
        press(&mut bare, KeyCode::Enter, KeyModifiers::NONE);
        assert!(bare.overlay.is_none());
        assert!(bare.hint().unwrap().contains("Arko's folder"));
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
        for _ in 0..3 {
            press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        }
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

    /// Once the conversation has started, `/model` opens the picker of a
    /// new conversation's model, and `/model worker` the workers' picker,
    /// which can make them follow the main agent or take a model typed.
    #[test]
    fn after_the_start_model_picks_a_new_conversations_model() {
        let mut app = app();
        hermetic(&mut app);
        app.set_reading(Reading {
            models: models(true),
            ..Reading::default()
        });
        type_text(&mut app, "/model");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let main = picker(&app);
        assert_eq!(main.title(), "New conversation's model");
        assert!(main.new_conversation);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        type_text(&mut app, "/model worker");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let workers = picker(&app);
        assert_eq!(workers.title(), "Workers' model");
        assert!(matches!(
            workers.rows[workers.selected],
            model_picker::Row::FollowMain { .. }
        ));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(&mut app), ["/model worker main"]);

        // A model typed: one Arko doesn't know says what was meant, one it
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
            matches!(&picker(&app).step, model_picker::Step::Typing { error: Some(error), .. } if error == "claude-fable-5-1 needs ANTHROPIC_API_KEY. /keys adds API keys.")
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
            Some(
                "Workers that get no profile or model of their own now run gpt-6-astra at high effort"
            )
        );
    }

    /// The banner's Model row follows the session's, and the note on why
    /// Arko chose the model goes once another is chosen.
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
            model_note: Some(
                "This session runs gpt-6-luna, as your last one did (-m picks another).".into(),
            ),
            notes: Vec::new(),
        });
        let mut reading = Reading::default();
        app.set_reading(reading.clone());
        assert_eq!(
            app.transcript.banner.as_ref().unwrap().model,
            "gpt-6-luna (high effort)"
        );
        reading.models.banner_model = "gpt-6-luna (high effort)".into();
        app.set_reading(reading.clone());
        assert!(app.transcript.banner.as_ref().unwrap().model_note.is_some());
        reading.models.banner_model = "gpt-6.1-sol (high effort)".into();
        reading.models.banner_workers = "parallel workers on gpt-6-astra (high effort)".into();
        app.set_reading(reading);
        let banner = app.transcript.banner.as_ref().unwrap();
        assert_eq!(banner.model, "gpt-6.1-sol (high effort)");
        assert_eq!(banner.model_note, None);
        assert_eq!(
            banner.workers,
            "parallel workers on gpt-6-astra (high effort)"
        );
    }
}
