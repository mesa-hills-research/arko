//! The API keys menu (`/keys`, and API keys in the settings): each
//! provider's key, masked, with where it comes from, or what it is for when
//! none is set. Enter adds or replaces the key chosen: it is pasted into a
//! field that shows only its start and last 4 characters (enough to tell it
//! is the key meant), then saved and used by the session (`keys`). Delete
//! removes a key Arko saved. A key never reaches the input box, its history or the clipboard:
//! the menu keeps it as a [`Secret`] until it goes to the session.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, footer, frame_box, put};
use super::theme::Theme;
use super::wrap;
use crate::credentials::{self, Key, KeySource, KeyState};
use crate::glyphs;
use crate::keys::{self, Secret};

/// Where the menu is.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The keys.
    List,
    /// Entering the chosen key: what is pasted or typed, and why the last
    /// one wasn't taken.
    Entering {
        value: Secret,
        error: Option<String>,
    },
    /// Removing the chosen key, once Enter confirms it.
    Removing,
}

/// What the last change did, under the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    pub text: String,
    pub warnings: Vec<String>,
    /// It failed: shown as an error.
    pub failed: bool,
}

/// The menu, open.
#[derive(Debug, Clone, PartialEq)]
pub struct KeysMenu {
    pub states: Vec<KeyState>,
    pub selected: usize,
    pub step: Step,
    /// The settings' row it was opened from: Esc goes back there.
    pub back_to: Option<usize>,
    pub said: Option<Said>,
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Stay,
    /// Close the menu (back to the settings when it came from there).
    Close,
    /// The session saves `value` as `key`, and uses it.
    Add {
        key: &'static Key,
        value: Secret,
    },
    /// The session removes `key`.
    Remove(&'static Key),
}

impl KeysMenu {
    /// The menu on `states`. `words` are what followed `/keys`: `add
    /// PROVIDER` opens that key's field, and `remove PROVIDER` asks to
    /// remove it.
    pub fn open(states: Vec<KeyState>, words: &str, back_to: Option<usize>) -> Self {
        let mut menu = Self {
            states,
            selected: 0,
            step: Step::List,
            back_to,
            said: None,
        };
        let words: Vec<&str> = words.split_whitespace().collect();
        let chosen = match words.as_slice() {
            [_, word] => credentials::key_named(word)
                .and_then(|key| menu.states.iter().position(|state| state.key == key)),
            _ => None,
        };
        match (words.as_slice(), chosen) {
            ([], _) => {}
            ([verb @ ("add" | "remove"), _], Some(chosen)) => {
                menu.selected = chosen;
                if *verb == "add" {
                    menu.step = Step::Entering {
                        value: Secret::default(),
                        error: None,
                    };
                } else {
                    menu.remove();
                }
            }
            _ => {
                menu.said = Some(Said {
                    text: keys::USAGE.to_string(),
                    warnings: Vec::new(),
                    failed: true,
                })
            }
        }
        menu
    }

    fn state(&self) -> &KeyState {
        &self.states[self.selected.min(self.states.len() - 1)]
    }

    /// Asks to remove the chosen key, when Arko saved it, else says where it
    /// comes from.
    fn remove(&mut self) {
        let state = self.state().clone();
        match &state.source {
            _ if state.removable() => self.step = Step::Removing,
            None => {}
            Some(source) => {
                self.said = Some(Said {
                    text: format!(
                        "Arko didn't save this key. It comes from {}, so change it there.",
                        credentials::source_text(source, state.variable)
                    ),
                    warnings: Vec::new(),
                    failed: true,
                })
            }
        }
    }

    /// A key, in the step open; `close` is Esc or Ctrl+C.
    pub fn key(&mut self, key: KeyEvent, close: bool) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let chosen = self.state().key;
        match &mut self.step {
            Step::List => match key.code {
                _ if close => return Action::Close,
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    self.selected = (self.selected + 1).min(self.states.len() - 1)
                }
                KeyCode::Home | KeyCode::PageUp => self.selected = 0,
                KeyCode::End | KeyCode::PageDown => self.selected = self.states.len() - 1,
                KeyCode::Enter | KeyCode::Right => {
                    self.said = None;
                    self.step = Step::Entering {
                        value: Secret::default(),
                        error: None,
                    }
                }
                KeyCode::Delete | KeyCode::Backspace => {
                    self.said = None;
                    self.remove();
                }
                _ => {}
            },
            Step::Entering { value, error } => match key.code {
                _ if close => self.step = Step::List,
                KeyCode::Char('u') if ctrl => value.clear(),
                KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                    value.push(c);
                    *error = None;
                }
                KeyCode::Backspace => value.pop(),
                KeyCode::Enter if !value.is_empty() => {
                    let key = chosen;
                    let typed = value.reveal().trim().to_string();
                    if let Err(problem) = credentials::check_pasted(&typed, key) {
                        value.clear();
                        *error = Some(problem);
                        return Action::Stay;
                    }
                    return Action::Add {
                        key,
                        value: Secret::new(typed),
                    };
                }
                _ => {}
            },
            Step::Removing => match key.code {
                _ if close => self.step = Step::List,
                KeyCode::Enter => {
                    self.step = Step::List;
                    return Action::Remove(chosen);
                }
                _ => {}
            },
        }
        Action::Stay
    }

    /// A paste the terminal marked: into the field, without the line break
    /// or spaces a copy may have around it.
    pub fn paste(&mut self, text: &str) {
        if let Step::Entering { value, error } = &mut self.step {
            for c in text
                .trim()
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
            {
                value.push(c);
            }
            *error = None;
        }
    }

    /// What the session did with `key`; `states` are the keys after it.
    pub fn done(&mut self, key: &'static Key, outcome: keys::Outcome, states: Vec<KeyState>) {
        self.states = states;
        let on_it = self.state().key == key;
        match outcome {
            keys::Outcome::Done { said, warnings } => {
                if on_it && matches!(self.step, Step::Entering { .. }) {
                    self.step = Step::List;
                }
                self.said = Some(Said {
                    text: said,
                    warnings,
                    failed: false,
                });
            }
            keys::Outcome::Failed(why) if on_it && matches!(self.step, Step::Entering { .. }) => {
                // Not saved: the field again, empty, with why.
                self.step = Step::Entering {
                    value: Secret::default(),
                    error: Some(why),
                };
            }
            keys::Outcome::Failed(why) => {
                self.said = Some(Said {
                    text: why,
                    warnings: Vec::new(),
                    failed: true,
                })
            }
        }
    }

    /// The box's title.
    fn title(&self) -> String {
        match self.step {
            Step::Entering { .. } => {
                let what = self.state().key.what;
                let what = what.split_once(' ').map_or(what, |(_, rest)| rest);
                what.to_string()
            }
            _ => "API keys".to_string(),
        }
    }
}

/// Where a key comes from, in a word or two for its row.
fn source_word(source: &KeySource) -> &'static str {
    match source {
        KeySource::Arko(_) => "saved by Arko",
        KeySource::Environment => "environment",
        KeySource::DotEnv(_) => ".env file",
    }
}

/// The lines under the chosen key: where it comes from and where to change
/// it, or what it is for.
fn detail(state: &KeyState) -> String {
    match &state.source {
        None => state.key.purpose.to_string(),
        Some(KeySource::Arko(path)) => {
            format!("Saved by Arko in {}.", path.display())
        }
        Some(KeySource::Environment) => format!(
            "From {} in Arko's environment. Change it where it is set.",
            state.variable
        ),
        Some(KeySource::DotEnv(path)) => format!(
            "From {} in {}. Change it there.",
            state.variable,
            path.display()
        ),
    }
}

/// The label column's width.
const LABEL: usize = 15;
/// The value column's width.
const VALUE: usize = 16;

/// Draws the menu over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, menu: &KeysMenu) {
    let width = 72.min(area.width.saturating_sub(2));
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
    let state = menu.state();
    let keys: Vec<(&str, &str)> = match &menu.step {
        Step::List | Step::Removing => {
            for (n, row) in menu.states.iter().enumerate() {
                let chosen = n == menu.selected;
                let marker = if chosen {
                    Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
                } else {
                    Span::raw("  ")
                };
                let label = if chosen {
                    theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    theme.text()
                };
                let value = match &row.masked {
                    Some(masked) => {
                        Span::styled(format!("{:<VALUE$}", glyphs::render(masked)), theme.text())
                    }
                    None => Span::styled(format!("{:<VALUE$}", "not set"), theme.dim()),
                };
                let mut spans = vec![
                    marker,
                    Span::styled(format!("{:<LABEL$}", row.key.provider), label),
                    value,
                ];
                if let Some(source) = &row.source {
                    spans.push(Span::styled(source_word(source), theme.dim()));
                }
                lines.push(Line::from(spans));
                if chosen {
                    if menu.step == Step::Removing {
                        wrapped(
                            &mut lines,
                            "Remove the key Arko saved? Enter removes it, and Esc keeps it.",
                            theme.warning(),
                            LABEL + 2,
                        );
                    } else {
                        wrapped(&mut lines, &detail(row), theme.dim(), LABEL + 2);
                    }
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
                for warning in &said.warnings {
                    wrapped(&mut lines, warning, theme.warning(), 0);
                }
            }
            match (&menu.step, menu.back_to.is_some()) {
                (Step::Removing, _) => vec![("Enter", "remove"), ("Esc", "keep")],
                (_, back) => {
                    let mut keys = vec![
                        ("↑ ↓", "choose"),
                        (
                            "Enter",
                            if state.masked.is_some() {
                                "replace"
                            } else {
                                "add"
                            },
                        ),
                    ];
                    if state.removable() {
                        keys.push(("Delete", "remove"));
                    }
                    keys.push(("Esc", if back { "back" } else { "close" }));
                    keys
                }
            }
        }
        Step::Entering { value, error } => {
            let key = state.key;
            wrapped(
                &mut lines,
                &format!(
                    "Paste {} (from {}). Arko saves it in its .env (readable only by you) and uses it at once.",
                    key.what, key.from
                ),
                theme.text(),
                0,
            );
            lines.push(Line::default());
            let shown = if value.is_empty() {
                String::new()
            } else {
                glyphs::render(&credentials::masked(value.reveal(), key.prefix)).into_owned()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", glyphs::render("›")), theme.accent()),
                Span::styled(shown, theme.text()),
                Span::styled(" ", theme.chosen()),
            ]));
            if !value.is_empty() {
                let count = value.len();
                lines.push(Line::from(Span::styled(
                    format!("  {count} character{}", if count == 1 { "" } else { "s" }),
                    theme.dim(),
                )));
            }
            if let Some(error) = error {
                lines.push(Line::default());
                wrapped(&mut lines, error, theme.error(), 0);
            }
            vec![("Enter", "save"), ("Esc", "back")]
        }
    };
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, &menu.title());
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
pub(super) mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Keys as a machine might have them: one Arko saved, one from the
    /// environment under Gemini's older name, one from a project's `.env`.
    pub fn states() -> Vec<KeyState> {
        credentials::KEYS
            .into_iter()
            .map(|key| {
                let (masked, variable, source) = match key.id {
                    "anthropic" => (
                        Some("sk-ant-…a1b2"),
                        key.name,
                        Some(KeySource::Arko("/home/me/.local/share/arko/.env".into())),
                    ),
                    "gemini" => (
                        Some("AIza…x9Yz"),
                        credentials::GOOGLE_KEY,
                        Some(KeySource::Environment),
                    ),
                    "openrouter" => (
                        Some("sk-or-…77aa"),
                        key.name,
                        Some(KeySource::DotEnv("/home/me/project/.env".into())),
                    ),
                    _ => (None, key.name, None),
                };
                KeyState {
                    key,
                    masked: masked.map(str::to_string),
                    variable,
                    source,
                }
            })
            .collect()
    }

    /// The menu as an 80-column screen shows it, row by row.
    pub fn screen(menu: &KeysMenu, theme: &Theme) -> Vec<String> {
        let (width, height) = (80, 24);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, theme, frame.area(), menu))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn press(menu: &mut KeysMenu, code: KeyCode) -> Action {
        menu.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            code == KeyCode::Esc,
        )
    }

    /// Each key shows masked, with where it comes from, and the one chosen
    /// says more under it; the footer offers what the chosen key allows.
    #[test]
    fn the_keys_show_masked_with_where_they_come_from() {
        let mut menu = KeysMenu::open(states(), "", None);
        let rows = screen(&menu, &Theme::terminal());
        let text = rows.join("\n");
        println!("{text}");
        for (row, label) in [
            (
                "▸ Anthropic      sk-ant-…a1b2    saved by Arko",
                "Anthropic",
            ),
            ("  OpenAI         not set", "OpenAI"),
            ("  Google Gemini  AIza…x9Yz       environment", "Gemini"),
            ("  OpenRouter     sk-or-…77aa     .env file", "OpenRouter"),
            ("  Voyage         not set", "Voyage"),
        ] {
            assert!(text.contains(row), "{label}: {text}");
        }
        assert!(text.contains("Saved by Arko in /home/me/.local/share/arko/.env."));
        assert!(text.contains("Enter  replace"), "{text}");
        assert!(text.contains("Delete  remove"), "{text}");
        assert!(text.contains("Esc  close"), "{text}");
        // The rows fit in 80 columns.
        assert!(rows.iter().all(|row| row.chars().count() <= 80));
        // Not set: what it is for, and only Enter to add.
        press(&mut menu, KeyCode::Down);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(
            text.contains("For OpenAI models, billed per token."),
            "{text}"
        );
        assert!(
            text.contains("Enter  add") && !text.contains("Delete"),
            "{text}"
        );
        // From elsewhere: where to change it, and Delete says so.
        press(&mut menu, KeyCode::Down);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(
            text.contains("From GOOGLE_API_KEY in Arko's environment."),
            "{text}"
        );
        assert_eq!(press(&mut menu, KeyCode::Delete), Action::Stay);
        assert_eq!(menu.step, Step::List);
        assert!(
            menu.said
                .as_ref()
                .unwrap()
                .text
                .contains("Arko didn't save this key")
        );
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Down);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(text.contains("The search indexes embed with it"), "{text}");
    }

    /// A key is pasted into a field that shows its start and last 4
    /// characters at once. Enter sends it to the session to save, and a key
    /// that isn't saved brings the field back, empty, with why.
    #[test]
    fn a_pasted_key_shows_masked_and_goes_to_the_session() {
        let mut menu = KeysMenu::open(states(), "add openai", None);
        assert!(matches!(menu.step, Step::Entering { .. }));
        let key = "sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789wxyz";
        menu.paste(&format!("  {key}\n"));
        let rows = screen(&menu, &Theme::terminal());
        println!("{}", rows.join("\n"));
        let text = rows.join("\n");
        assert!(text.contains("OpenAI API key"), "{text}");
        assert!(
            text.contains("saves it in its .env (readable only by you)"),
            "{text}"
        );
        assert!(text.contains("› sk-…wxyz"), "{text}");
        assert!(
            text.contains(&format!("{} characters", key.len())),
            "{text}"
        );
        assert!(!text.contains("AbCd"), "the key is never shown: {text}");
        assert!(text.contains("Enter  save"));
        let Action::Add { key: added, value } = press(&mut menu, KeyCode::Enter) else {
            panic!("not sent")
        };
        assert_eq!(added.name, credentials::OPENAI_KEY);
        assert_eq!(value.reveal(), key);
        let failed = "Could not save /home/me/.local/share/arko/.env: Permission denied";
        menu.done(
            &credentials::OPENAI,
            keys::Outcome::Failed(failed.into()),
            states(),
        );
        assert_eq!(
            menu.step,
            Step::Entering {
                value: Secret::default(),
                error: Some(failed.into())
            }
        );
        // Another provider's key is caught before it is sent.
        for c in "sk-ant-api03-0123456789".chars() {
            press(&mut menu, KeyCode::Char(c));
        }
        assert_eq!(press(&mut menu, KeyCode::Enter), Action::Stay);
        assert!(
            matches!(&menu.step, Step::Entering { value, error: Some(error) }
            if value.is_empty() && error.starts_with("That looks like an Anthropic API key"))
        );
        // Saved: the list, with what was done.
        menu.paste(key);
        press(&mut menu, KeyCode::Enter);
        menu.done(
            &credentials::OPENAI,
            keys::Outcome::Done {
                said: "Arko saved the OpenAI API key. OpenAI models can be used now.".into(),
                warnings: vec!["Note: OPENAI_API_KEY in your environment comes first in later runs. This run uses what you entered.".into()],
            },
            states(),
        );
        assert_eq!(menu.step, Step::List);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(text.contains("Arko saved the OpenAI API key."), "{text}");
        assert!(
            text.contains("Note: OPENAI_API_KEY in your environment"),
            "{text}"
        );
        assert_eq!(press(&mut menu, KeyCode::Esc), Action::Close);
    }

    /// Delete asks before it removes a key Arko saved.
    #[test]
    fn removing_a_saved_key_asks_first() {
        let mut menu = KeysMenu::open(states(), "", None);
        assert_eq!(press(&mut menu, KeyCode::Backspace), Action::Stay);
        assert_eq!(menu.step, Step::Removing);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(
            text.contains("Remove the key Arko saved? Enter removes it"),
            "{text}"
        );
        assert!(text.contains("Esc  keep"), "{text}");
        assert_eq!(press(&mut menu, KeyCode::Esc), Action::Stay);
        assert_eq!(menu.step, Step::List);
        press(&mut menu, KeyCode::Delete);
        assert_eq!(
            press(&mut menu, KeyCode::Enter),
            Action::Remove(&credentials::ANTHROPIC)
        );
        let menu = KeysMenu::open(states(), "remove anthropic", Some(7));
        assert_eq!(menu.step, Step::Removing);
        let menu = KeysMenu::open(states(), "add nothing", None);
        assert_eq!(menu.step, Step::List);
        assert!(menu.said.unwrap().failed);
    }
}
