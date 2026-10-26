//! OpenRouter's settings menu (`/openrouter`): the seven choices with their
//! values, and the models OpenRouter lists. Enter changes the one chosen. A
//! switch toggles and a choice among a few goes to the next. The two lists
//! of providers open a line to edit. A change is saved at once, and the open
//! sessions use it from their next request (`openrouter`). The models row
//! fetches the list again. A change is made on the settings as the file has
//! them, on a thread of its own ([`Worker`]), so that a wait for another
//! change never stops the keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, footer, frame_box, put};
use super::theme::Theme;
use super::wrap;
use crate::glyphs;
use crate::openrouter::{self, Catalog, Control, Precision, Region, Settings, Sort};

/// Where the menu is.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The choices.
    List,
    /// Editing the providers of `control` (`Only` or `Ignore`), as text.
    Editing { control: Control, text: String },
}

/// What the last change did, under the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    pub text: String,
    /// It failed: shown as an error.
    pub failed: bool,
}

/// The menu, open. Its rows are the seven choices, then the models.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenRouterMenu {
    pub root: std::path::PathBuf,
    pub settings: Settings,
    pub catalog: Catalog,
    pub selected: usize,
    pub step: Step,
    pub said: Option<Said>,
}

/// The row of the models, after the choices.
const MODELS: usize = Control::ALL.len();

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Stay,
    /// Close the menu.
    Close,
    /// Make this change to the settings, away from the keys (see
    /// [`make`]), and tell the menu ([`OpenRouterMenu::done`]).
    Edit(Edit),
}

/// A change the menu makes. It is made on the settings as the file has them
/// when it runs, so what another session saved meanwhile stays, and a choice
/// that cycles goes on from the one then.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Zdr,
    DataCollection,
    Region,
    Sort,
    Precision,
    Only(Vec<String>),
    Ignore(Vec<String>),
}

impl Edit {
    /// Makes the change in `settings`, and says what it did.
    pub fn apply(&self, settings: &mut Settings) -> String {
        let on_off = |on: bool| if on { "on" } else { "off" };
        match self {
            Edit::Zdr => {
                let on = !settings.zdr();
                settings.set_zdr(on);
                format!("Zero data retention is {}.", on_off(on))
            }
            Edit::DataCollection if settings.data_collection_implied() => {
                "Zero data retention covers it, so it stays on while that is on.".to_string()
            }
            Edit::DataCollection => {
                let on = !settings.deny_data_collection();
                settings.set_deny_data_collection(on);
                format!("No data collection is {}.", on_off(on))
            }
            Edit::Region => {
                let at = Region::ALL
                    .iter()
                    .position(|region| *region == settings.region())
                    .unwrap_or(0);
                let next = Region::ALL[(at + 1) % Region::ALL.len()];
                settings.set_region(next);
                format!("Region is {}.", next.label())
            }
            Edit::Sort => {
                let at = Sort::ALL
                    .iter()
                    .position(|sort| *sort == settings.sort())
                    .unwrap_or(0);
                let next = Sort::ALL[(at + 1) % Sort::ALL.len()];
                settings.set_sort(next);
                format!("Providers go by {}.", next.label().to_lowercase())
            }
            Edit::Precision => {
                // A list written by hand goes to the first choice.
                let at = Precision::ALL
                    .iter()
                    .position(|precision| *precision == settings.precision());
                let next = Precision::ALL[at.map_or(0, |at| (at + 1) % Precision::ALL.len())];
                settings.set_precision(next);
                format!("Precision is {}.", next.label().to_lowercase())
            }
            Edit::Only(slugs) => {
                settings.set_only(slugs.clone());
                match slugs.as_slice() {
                    [] => "Any provider may serve requests.".to_string(),
                    slugs => format!("Only {} serve requests.", crate::and_list(slugs)),
                }
            }
            Edit::Ignore(slugs) => {
                settings.set_ignore(slugs.clone());
                match slugs.as_slice() {
                    [] => "No provider is left out.".to_string(),
                    slugs => format!("{} never serve requests.", crate::and_list(slugs)),
                }
            }
        }
    }
}

/// Makes `edit` in the settings of Arko's folder `root`, under the lock every
/// change takes, and installs them in the sessions open here. It may wait
/// for another change, so it runs on a [`Worker`].
pub fn make(root: &std::path::Path, edit: &Edit) -> Result<(Settings, String), String> {
    Settings::edit(root, 0, |settings| edit.apply(settings))
}

/// A thread that makes the changes asked for one after another, in the order
/// they were asked, so that a wait for another change doesn't stop the keys
/// and two quick changes don't swap.
pub struct Worker(std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>);

impl Worker {
    pub fn start() -> Self {
        let (jobs, queue) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::spawn(move || {
            for job in queue {
                job();
            }
        });
        Self(jobs)
    }

    /// Has `job` run after those asked before it, and returns at once.
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.0.send(Box::new(job));
    }
}

impl OpenRouterMenu {
    /// The menu on the settings in Arko's folder `root`. A file that can't
    /// be read is said, and the menu shows none set.
    pub fn open(root: &std::path::Path) -> Self {
        let (settings, said) = match Settings::read(root) {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings::default(),
                Some(Said {
                    text: format!(
                        "{error}. Fix the file or remove it to change the settings here."
                    ),
                    failed: true,
                }),
            ),
        };
        // The list of models, when it is due.
        openrouter::refresh_soon(root);
        Self {
            root: root.to_path_buf(),
            catalog: openrouter::catalog(root),
            settings,
            selected: 0,
            step: Step::List,
            said,
        }
    }

    /// Takes the models as they are now (they were fetched).
    pub fn refresh_catalog(&mut self) {
        self.catalog = openrouter::catalog(&self.root);
    }

    fn control(&self) -> Option<Control> {
        Control::ALL.get(self.selected).copied()
    }

    /// Enter on the choice selected.
    fn change(&mut self, control: Control) -> Action {
        match control {
            Control::Zdr => self.ask(Edit::Zdr),
            Control::DataCollection => self.ask(Edit::DataCollection),
            Control::Region => self.ask(Edit::Region),
            Control::Sort => self.ask(Edit::Sort),
            Control::Precision => self.ask(Edit::Precision),
            Control::Only | Control::Ignore => {
                let current = match control {
                    Control::Only => self.settings.only(),
                    _ => self.settings.ignore(),
                };
                self.said = None;
                self.step = Step::Editing {
                    control,
                    text: current.join(", "),
                };
                Action::Stay
            }
        }
    }

    /// Asks for `edit`, to be made away from the keys. The menu says so
    /// until [`done`](Self::done).
    fn ask(&mut self, edit: Edit) -> Action {
        self.said = Some(Said {
            text: "Saving…".to_string(),
            failed: false,
        });
        Action::Edit(edit)
    }

    /// What the change asked for did: the settings as they are now with what
    /// it said, or why it wasn't made.
    pub fn done(&mut self, result: Result<(Settings, String), String>) {
        match result {
            Ok((settings, said)) => {
                self.settings = settings;
                // The list of models follows the filters.
                openrouter::refresh_soon(&self.root);
                self.refresh_catalog();
                self.said = Some(Said {
                    text: said,
                    failed: false,
                });
            }
            Err(error) => {
                self.said = Some(Said {
                    text: error,
                    failed: true,
                });
            }
        }
    }

    /// A key, in the step open; `close` is Esc or Ctrl+C.
    pub fn key(&mut self, key: KeyEvent, close: bool) -> Action {
        let ctrl = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match &mut self.step {
            Step::List => match key.code {
                _ if close => return Action::Close,
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => self.selected = (self.selected + 1).min(MODELS),
                KeyCode::Home | KeyCode::PageUp => self.selected = 0,
                KeyCode::End | KeyCode::PageDown => self.selected = MODELS,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => match self.control() {
                    Some(control) => return self.change(control),
                    None => {
                        // The models row: fetch them again.
                        self.said = Some(Said {
                            text: "Fetching OpenRouter's models.".to_string(),
                            failed: false,
                        });
                        let root = self.root.clone();
                        std::thread::spawn(move || {
                            openrouter::refresh_blocking(&root, true);
                        });
                    }
                },
                _ => {}
            },
            Step::Editing { control, text } => match key.code {
                _ if close => self.step = Step::List,
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !ctrl && !c.is_control() => text.push(c),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Enter => {
                    let (control, slugs) = (*control, openrouter::slugs(text));
                    self.step = Step::List;
                    return self.ask(match control {
                        Control::Only => Edit::Only(slugs),
                        _ => Edit::Ignore(slugs),
                    });
                }
                _ => {}
            },
        }
        Action::Stay
    }

    /// A paste, into the line being edited.
    pub fn paste(&mut self, pasted: &str) {
        if let Step::Editing { text, .. } = &mut self.step {
            text.extend(pasted.chars().filter(|c| !c.is_control()));
        }
    }

    /// The models row's value.
    fn models_value(&self) -> String {
        if self.catalog.models.is_empty() && openrouter::loading_for(&self.root) {
            return "loading".to_string();
        }
        self.catalog.summary()
    }

    /// The lines under the row selected: what it does.
    fn detail(&self) -> String {
        match self.control() {
            Some(Control::DataCollection) if self.settings.data_collection_implied() => {
                "Zero data retention covers it, so it stays on while that is on.".to_string()
            }
            Some(control) => control.about().to_string(),
            None => match &self.catalog.problem {
                Some(problem) => format!("{problem}. Enter tries again."),
                None => "The models that take tool calls, which Arko's agents work through, and that the settings allow. Enter fetches them again.".to_string(),
            },
        }
    }
}

/// The label column's width.
const LABEL: usize = 23;

/// Draws the menu over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, menu: &OpenRouterMenu) {
    let width = 76.min(area.width.saturating_sub(2));
    // Inside the borders and a column of space on each side.
    let room = (width as usize).saturating_sub(4);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let wrapped = |lines: &mut Vec<Line<'static>>, text: &str, style, indent: usize| {
        for line in wrap::wrap(
            &[Span::styled(glyphs::render(text).into_owned(), style)],
            room.saturating_sub(indent).max(10),
            0,
        ) {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(line);
            lines.push(Line::from(spans));
        }
    };
    let marker = |chosen: bool| {
        if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        }
    };
    let rows: Vec<(String, String)> = Control::ALL
        .iter()
        .map(|control| {
            let mut value = control.value(&menu.settings);
            // On because zero data retention covers it, not because it was set.
            if *control == Control::DataCollection
                && menu.settings.data_collection_implied()
                && !menu.settings.deny_data_collection()
            {
                value = "On, covered by zero data retention".to_string();
            }
            (control.label().to_string(), value)
        })
        .chain(std::iter::once(("Models".to_string(), menu.models_value())))
        .collect();
    let keys: Vec<(&str, &str)> = match &menu.step {
        Step::List => {
            for (n, (label, value)) in rows.iter().enumerate() {
                let chosen = n == menu.selected;
                let label_style = if chosen {
                    theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    theme.text()
                };
                lines.push(Line::from(vec![
                    marker(chosen),
                    Span::styled(format!("{label:<LABEL$}"), label_style),
                    Span::styled(glyphs::render(value).into_owned(), theme.text()),
                ]));
                if chosen {
                    wrapped(&mut lines, &menu.detail(), theme.dim(), LABEL + 2);
                }
            }
            if let Some(said) = &menu.said {
                lines.push(Line::default());
                let style = if said.failed {
                    theme.error()
                } else {
                    theme.text()
                };
                wrapped(&mut lines, &said.text, style, 0);
            }
            lines.push(Line::default());
            wrapped(
                &mut lines,
                "Your OpenRouter account's own privacy settings at openrouter.ai apply too. Open sessions use a change from their next request.",
                theme.dim(),
                0,
            );
            let act = match menu.control() {
                Some(Control::Only | Control::Ignore) => "edit",
                Some(Control::Zdr | Control::DataCollection) => "switch",
                Some(_) => "next",
                None => "fetch",
            };
            vec![("↑ ↓", "choose"), ("Enter", act), ("Esc", "close")]
        }
        Step::Editing { control, text } => {
            wrapped(
                &mut lines,
                &format!(
                    "{}. Provider slugs separated by commas, such as anthropic, google-vertex. Empty clears the list.",
                    control.label()
                ),
                theme.text(),
                0,
            );
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", glyphs::render("›")), theme.accent()),
                Span::styled(text.clone(), theme.text()),
                Span::styled(" ", theme.chosen()),
            ]));
            vec![("Enter", "save"), ("Esc", "back")]
        }
    };
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "OpenRouter");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    footer(frame, rect, theme, &keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    /// A key, with the change it asks for made at once (the screen has a
    /// thread make it). Returns whether the menu closes.
    fn press(menu: &mut OpenRouterMenu, code: KeyCode) -> bool {
        match menu.key(KeyEvent::new(code, KeyModifiers::NONE), false) {
            Action::Close => true,
            Action::Stay => false,
            Action::Edit(edit) => {
                let result = make(&menu.root, &edit);
                menu.done(result);
                false
            }
        }
    }

    fn type_in(menu: &mut OpenRouterMenu, text: &str) {
        for c in text.chars() {
            press(menu, KeyCode::Char(c));
        }
    }

    /// The menu as an 80-column screen shows it, inside its box.
    fn screen(menu: &OpenRouterMenu) -> Vec<String> {
        let (width, height) = (80, 30);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, &Theme::terminal(), frame.area(), menu))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .replace('│', " ")
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn on_row(menu: &mut OpenRouterMenu, control: Control) {
        menu.selected = Control::ALL.iter().position(|c| *c == control).unwrap();
    }

    fn file(root: &std::path::Path) -> serde_json::Value {
        let text = std::fs::read_to_string(root.join(openrouter::FILE)).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// Enter changes the choice selected and saves it at once: switches
    /// toggle, the others go to the next, and what is written is the
    /// settings' file.
    #[test]
    fn enter_changes_a_choice_and_saves_it() {
        let root = tempfile::tempdir().unwrap();
        let mut menu = OpenRouterMenu::open(root.path());
        assert!(menu.said.is_none());
        let rows = screen(&menu);
        for (label, value) in [
            ("Zero data retention", "Off"),
            ("Region", "Any"),
            ("Providers by", "Default"),
            ("Precision", "Any"),
            ("Only these providers", "Any"),
            ("Never these providers", "None"),
            ("Models", "not loaded"),
        ] {
            let row = format!("{label:<LABEL$}{value}");
            assert!(
                rows.iter().any(|r| r.contains(&row)),
                "{row:?} in {rows:#?}"
            );
        }
        press(&mut menu, KeyCode::Enter);
        assert!(menu.settings.zdr());
        assert_eq!(file(root.path())["provider"]["zdr"], true);
        assert_eq!(
            menu.said.as_ref().unwrap().text,
            "Zero data retention is on."
        );
        // The row under it is on, and covered by zero data retention.
        assert_eq!(Control::DataCollection.value(&menu.settings), "On");
        assert!(
            screen(&menu)
                .iter()
                .any(|r| r.contains("No data collection")
                    && r.contains("On, covered by zero data retention")),
            "{:#?}",
            screen(&menu)
        );
        on_row(&mut menu, Control::DataCollection);
        press(&mut menu, KeyCode::Enter);
        assert!(!menu.settings.deny_data_collection());
        assert!(menu.said.as_ref().unwrap().text.contains("covers it"));
        assert!(
            screen(&menu)
                .iter()
                .any(|r| r.contains("Zero data retention covers it, so it stays on")),
            "its detail says so"
        );
        on_row(&mut menu, Control::Zdr);
        press(&mut menu, KeyCode::Enter);
        on_row(&mut menu, Control::DataCollection);
        press(&mut menu, KeyCode::Enter);
        assert_eq!(file(root.path())["provider"]["data_collection"], "deny");
        assert!(file(root.path())["provider"].get("zdr").is_none());

        // The choices among a few go round.
        on_row(&mut menu, Control::Region);
        for expected in ["us", "eu", "us-or-eu"] {
            press(&mut menu, KeyCode::Enter);
            assert_eq!(file(root.path())["region"], expected);
        }
        press(&mut menu, KeyCode::Enter);
        assert!(
            file(root.path()).get("region").is_none(),
            "any is not written"
        );
        on_row(&mut menu, Control::Sort);
        press(&mut menu, KeyCode::Enter);
        assert_eq!(file(root.path())["provider"]["sort"], "price");
        on_row(&mut menu, Control::Precision);
        press(&mut menu, KeyCode::Enter);
        assert_eq!(file(root.path())["provider"]["quantizations"][0], "fp8");
        press(&mut menu, KeyCode::Enter);
        assert_eq!(file(root.path())["provider"]["quantizations"][0], "fp16");
        press(&mut menu, KeyCode::Enter);
        assert!(file(root.path())["provider"].get("quantizations").is_none());
    }

    /// The two lists of providers open a line to edit, which saves as slugs.
    #[test]
    fn the_lists_of_providers_are_edited_in_a_line() {
        let root = tempfile::tempdir().unwrap();
        let mut menu = OpenRouterMenu::open(root.path());
        on_row(&mut menu, Control::Only);
        press(&mut menu, KeyCode::Enter);
        assert!(
            matches!(&menu.step, Step::Editing { control: Control::Only, text } if text.is_empty())
        );
        type_in(&mut menu, "Anthropic, google-vertex");
        menu.paste(" azure\n");
        press(&mut menu, KeyCode::Backspace);
        press(&mut menu, KeyCode::Enter);
        assert_eq!(menu.step, Step::List);
        assert_eq!(
            file(root.path())["provider"]["only"],
            serde_json::json!(["anthropic", "google-vertex", "azur"])
        );
        assert_eq!(
            menu.said.as_ref().unwrap().text,
            "Only anthropic, google-vertex, and azur serve requests."
        );
        // It starts from what is there, Esc leaves it as it was, and an
        // empty line clears it.
        press(&mut menu, KeyCode::Enter);
        assert!(
            matches!(&menu.step, Step::Editing { text, .. } if text == "anthropic, google-vertex, azur")
        );
        menu.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true);
        assert_eq!(menu.step, Step::List);
        assert_eq!(menu.settings.only().len(), 3);
        press(&mut menu, KeyCode::Enter);
        menu.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            false,
        );
        press(&mut menu, KeyCode::Enter);
        assert!(file(root.path()).get("provider").is_none());
        on_row(&mut menu, Control::Ignore);
        press(&mut menu, KeyCode::Enter);
        type_in(&mut menu, "some-provider");
        press(&mut menu, KeyCode::Enter);
        assert_eq!(
            file(root.path())["provider"]["ignore"],
            serde_json::json!(["some-provider"])
        );
        assert_eq!(
            menu.said.as_ref().unwrap().text,
            "some-provider never serve requests."
        );
    }

    /// A change goes into the file as it is now, so the models another
    /// session picked since the menu opened stay, and a file that can't be
    /// read isn't written over.
    #[test]
    fn a_change_keeps_what_was_saved_since_and_leaves_an_unreadable_file() {
        let root = tempfile::tempdir().unwrap();
        let mut menu = OpenRouterMenu::open(root.path());
        openrouter::remember(root.path(), "openrouter:acme/picked");
        press(&mut menu, KeyCode::Enter);
        assert_eq!(
            file(root.path())["recent"],
            serde_json::json!(["acme/picked"])
        );
        assert_eq!(file(root.path())["provider"]["zdr"], true);
        // A field written by hand stays too.
        let mut text = file(root.path());
        text["provider"]["max_price"] = serde_json::json!({"prompt": 2.0});
        std::fs::write(root.path().join(openrouter::FILE), text.to_string()).unwrap();
        press(&mut menu, KeyCode::Enter);
        assert_eq!(
            file(root.path())["provider"]["max_price"],
            serde_json::json!({"prompt": 2.0})
        );

        std::fs::write(root.path().join(openrouter::FILE), "{ oops").unwrap();
        press(&mut menu, KeyCode::Enter);
        assert!(menu.said.as_ref().unwrap().failed);
        assert!(
            menu.said
                .as_ref()
                .unwrap()
                .text
                .contains("openrouter.json can't be read")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(openrouter::FILE)).unwrap(),
            "{ oops"
        );
        let broken = OpenRouterMenu::open(root.path());
        assert!(broken.said.as_ref().unwrap().failed);
    }

    /// A precision written by hand shows as custom, and Enter moves it to the
    /// first choice. The models row fetches again.
    #[test]
    fn a_custom_precision_goes_to_any_and_the_models_row_fetches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(openrouter::FILE),
            r#"{"provider": {"quantizations": ["int8"]}}"#,
        )
        .unwrap();
        let mut menu = OpenRouterMenu::open(root.path());
        assert!(
            screen(&menu)
                .iter()
                .any(|r| r.contains("Precision") && r.contains("Custom"))
        );
        on_row(&mut menu, Control::Precision);
        press(&mut menu, KeyCode::Enter);
        assert!(file(root.path())["provider"].get("quantizations").is_none());
        press(&mut menu, KeyCode::End);
        assert_eq!(menu.selected, Control::ALL.len());
        press(&mut menu, KeyCode::Enter);
        assert_eq!(
            menu.said.as_ref().unwrap().text,
            "Fetching OpenRouter's models."
        );
        assert!(!menu.said.as_ref().unwrap().failed);
        assert_eq!(
            menu.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true),
            Action::Close
        );
    }

    /// The models row says how many and how old, or why not.
    #[test]
    fn the_models_row_says_how_many_and_how_old_or_why_not() {
        let root = tempfile::tempdir().unwrap();
        // (Settings no other test has: the models in memory go by them.)
        let mut settings = openrouter::Settings::default();
        settings.set_zdr(true);
        settings.save(root.path()).unwrap();
        openrouter::seed_failure_for_tests(&settings, "it couldn't be reached");
        let mut menu = OpenRouterMenu::open(root.path());
        menu.selected = Control::ALL.len();
        let rows = screen(&menu);
        // (The detail wraps under the row.)
        let text = rows.join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            text.contains("Couldn't load the models (it couldn't be reached). Enter tries again."),
            "{rows:#?}"
        );
        // The menu, open, takes the models once they are fetched, with no key
        // pressed, as the view finds out they have come.
        let mut app = crate::tui::app::tests::app();
        app.info.root = Some(root.path().to_path_buf());
        app.overlay = Some(crate::tui::overlay::Overlay::OpenRouter(menu));
        let _ = app.openrouter_moved();
        openrouter::seed_for_tests(&settings, &["a/b", "c/d"]);
        assert!(app.openrouter_moved());
        let Some(crate::tui::overlay::Overlay::OpenRouter(menu)) = &app.overlay else {
            panic!("the menu is open");
        };
        let rows = screen(menu);
        assert!(
            rows.iter()
                .any(|r| r.contains("Models") && r.contains("2, updated just now")),
            "{rows:#?}"
        );
    }

    /// A change made by the menu goes into the settings as the file has them
    /// when it runs: one made by another session just before stays, and the
    /// menu's choice goes on from the one then. (The menu once read the file,
    /// changed one choice and saved all of the routing.)
    #[test]
    fn a_change_from_the_menu_keeps_what_another_session_changed_meanwhile() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let mut menu = OpenRouterMenu::open(&root);
        assert!(!menu.settings.zdr(), "the menu has zero data retention off");
        // Another session turns zero data retention on, and holds the lock
        // after it read the file.
        let (reached, go) = openrouter::pause_next_change(&root);
        let other = {
            let root = root.clone();
            std::thread::spawn(move || Settings::update(&root, |s| s.set_zdr(true)))
        };
        reached.recv().unwrap();
        // The menu's change, on the sort, waits for it, and then goes on.
        on_row(&mut menu, Control::Sort);
        let Action::Edit(edit) = menu.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), false)
        else {
            panic!("Enter asks for a change");
        };
        assert_eq!(edit, Edit::Sort);
        assert_eq!(menu.said.as_ref().unwrap().text, "Saving…");
        let made = {
            let root = root.clone();
            std::thread::spawn(move || make(&root, &edit))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!made.is_finished(), "it waits for the other change");
        go.send(()).unwrap();
        other.join().unwrap().unwrap();
        let result = made.join().unwrap();
        menu.done(result);
        let file: serde_json::Value = file(&root);
        assert_eq!(file["provider"]["zdr"], true, "{file}");
        assert_eq!(file["provider"]["sort"], "price", "{file}");
        assert!(menu.settings.zdr(), "the menu shows what is saved now");
        assert_eq!(menu.said.as_ref().unwrap().text, "Providers go by price.");
        // A choice that cycles goes on from where the file has it.
        Settings::update(&root, |s| s.set_sort(Sort::Latency)).unwrap();
        let (settings, said) = make(&root, &Edit::Sort).unwrap();
        assert_eq!(
            (settings.sort(), said.as_str()),
            (Sort::Default, "Providers go by default.")
        );
    }

    /// The keys aren't held by a wait for the settings' lock: Enter returns
    /// at once with the change to make, and a thread made for waiting makes
    /// them in the order they were asked, however long the lock is held.
    #[test]
    fn a_held_settings_lock_stops_no_key_and_the_changes_keep_their_order() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let mut menu = OpenRouterMenu::open(&root);
        // Another process holds the file's lock for a while.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(format!("{}.lock", openrouter::FILE)))
            .unwrap();
        lock.lock().unwrap();
        let (results, received) = std::sync::mpsc::channel();
        let worker = Worker::start();
        let started = Instant::now();
        for control in [Control::Region, Control::Zdr, Control::Region] {
            on_row(&mut menu, control);
            let Action::Edit(edit) =
                menu.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), false)
            else {
                panic!("Enter asks for a change");
            };
            let (root, results) = (root.clone(), results.clone());
            worker.submit(move || {
                let _ = results.send(make(&root, &edit));
            });
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the keys went through: {:?}",
            started.elapsed()
        );
        assert!(
            received.try_recv().is_err(),
            "nothing is made while it is held"
        );
        std::thread::sleep(Duration::from_millis(200));
        drop(lock);
        let said: Vec<String> = (0..3)
            .map(|_| {
                received
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(
            said,
            [
                "Region is US.",
                "Zero data retention is on.",
                "Region is EU."
            ],
            "in the order asked"
        );
    }
}
