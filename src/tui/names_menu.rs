//! The session names menu (`/session-names`). Sessions are named by the
//! first words of the first message, or by a model. For a model, the menu
//! lists the four ways to ask one, each on or off, in the order they are
//! tried, and says under each what it needs that is missing now. Space
//! turns a way on or off, Shift+↑ and Shift+↓ move it, and Enter saves.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, footer, frame_box, put};
use super::theme::Theme;
use crate::glyphs;
use crate::session_names::{self, Way};

/// The menu, open.
#[derive(Debug, Clone, PartialEq)]
pub struct NamesMenu {
    /// Whether a model names sessions.
    pub by_model: bool,
    /// The four ways in the order they are tried, with whether each is on.
    pub ways: Vec<(Way, bool)>,
    /// What each way needs that is missing now, once it has been checked
    /// (the check runs in the background, so the menu opens at once).
    pub missing: Option<Vec<(Way, Option<String>)>>,
    /// The row chosen, 0 and 1 for the two ways to name, then the ways.
    pub selected: usize,
    /// What Enter couldn't do.
    pub said: Option<String>,
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Stay,
    Close,
    /// Save these ways, none for the first words.
    Save(Vec<Way>),
}

impl NamesMenu {
    /// The menu for the `saved` ways (none for the first words), without
    /// what they lack, which [`NamesMenu::checked`] adds. A choice of a
    /// model starts with the two ways that use a plan on.
    pub fn open(saved: &[Way]) -> Self {
        let on = if saved.is_empty() {
            session_names::default_ways()
        } else {
            saved.to_vec()
        };
        Self {
            by_model: !saved.is_empty(),
            ways: session_names::arranged(&on),
            missing: None,
            selected: 0,
            said: None,
        }
    }

    /// How many rows there are.
    fn rows(&self) -> usize {
        if self.by_model {
            2 + self.ways.len()
        } else {
            2
        }
    }

    /// The way the chosen row is, if it is one.
    fn way(&self) -> Option<usize> {
        self.selected.checked_sub(2)
    }

    fn reason(&self, way: Way) -> Option<&str> {
        self.missing
            .iter()
            .flatten()
            .find(|(missing, _)| *missing == way)
            .and_then(|(_, reason)| reason.as_deref())
    }

    /// What the check of the ways found, which says under each what it lacks.
    pub fn checked(&mut self, missing: Vec<(Way, Option<String>)>) {
        self.missing = Some(missing);
    }

    /// A key; `close` is Esc or Ctrl+C.
    pub fn key(&mut self, key: KeyEvent, close: bool) -> Action {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        self.said = None;
        match key.code {
            _ if close => return Action::Close,
            KeyCode::Up | KeyCode::Down if shift => self.shift(key.code == KeyCode::Up),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(self.rows() - 1)
            }
            KeyCode::Home | KeyCode::PageUp => self.selected = 0,
            KeyCode::End | KeyCode::PageDown => self.selected = self.rows() - 1,
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Enter => return self.save(),
            _ => {}
        }
        Action::Stay
    }

    /// Chooses how to name, or turns the chosen way on or off.
    fn toggle(&mut self) {
        match self.way() {
            None => self.by_model = self.selected == 1,
            Some(at) => self.ways[at].1 = !self.ways[at].1,
        }
    }

    /// Moves the chosen way up or down, past the others.
    fn shift(&mut self, up: bool) {
        let Some(at) = self.way() else {
            return;
        };
        let to = if up { at.checked_sub(1) } else { Some(at + 1) };
        if let Some(to) = to.filter(|to| *to < self.ways.len()) {
            self.ways.swap(at, to);
            self.selected = to + 2;
        }
    }

    fn save(&mut self) -> Action {
        if !self.by_model {
            return Action::Save(Vec::new());
        }
        let on: Vec<Way> = self
            .ways
            .iter()
            .filter(|(_, on)| *on)
            .map(|(way, _)| *way)
            .collect();
        if on.is_empty() {
            self.said = Some("Turn on a way, or choose First words.".to_string());
            return Action::Stay;
        }
        Action::Save(on)
    }
}

/// Draws the menu over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, menu: &NamesMenu) {
    let width = 72.min(area.width.saturating_sub(2));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let marker = |chosen: bool| {
        if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        }
    };
    let label = |text: &str, chosen: bool| {
        Span::styled(
            text.to_string(),
            if chosen {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.text()
            },
        )
    };
    for (n, (text, on)) in [
        ("First words", !menu.by_model),
        ("Named by a model", menu.by_model),
    ]
    .into_iter()
    .enumerate()
    {
        let dot = if on {
            glyphs::render("•").into_owned()
        } else {
            " ".to_string()
        };
        lines.push(Line::from(vec![
            marker(menu.selected == n),
            Span::styled(format!("({dot}) "), theme.dim()),
            label(text, menu.selected == n),
        ]));
    }
    if menu.by_model {
        lines.push(Line::default());
        for (at, (way, on)) in menu.ways.iter().enumerate() {
            let chosen = menu.selected == at + 2;
            lines.push(Line::from(vec![
                marker(chosen),
                Span::styled(if *on { "[x] " } else { "[ ] " }, theme.dim()),
                label(way.label(), chosen),
            ]));
            if let Some(reason) = menu.reason(*way) {
                lines.push(Line::from(Span::styled(
                    format!("      {reason}"),
                    theme.dim(),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            "  Then the first words.",
            theme.dim(),
        )));
    }
    if let Some(said) = &menu.said {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(said.clone(), theme.error())));
    }
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Session names");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    let mut keys = Vec::new();
    if menu.way().is_some() {
        keys.push(("Space", "toggle"));
        keys.push(("Shift+↑ ↓", "move"));
    } else {
        keys.push(("Space", "choose"));
    }
    keys.push(("Enter", "save"));
    keys.push(("Esc", "close"));
    footer(frame, rect, theme, &keys);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// What the ways need on a machine with Claude Code but no login or
    /// keys.
    pub fn missing() -> Vec<(Way, Option<String>)> {
        vec![
            (Way::ClaudePlan, None),
            (Way::ChatGptPlan, Some("No ChatGPT login".into())),
            (Way::ClaudeKey, Some("No Anthropic API key".into())),
            (Way::OpenAiKey, Some("No OpenAI API key".into())),
        ]
    }

    /// The menu for the ways `saved`, once its check is done.
    pub fn menu_with(saved: &[Way]) -> NamesMenu {
        let mut menu = NamesMenu::open(saved);
        menu.checked(missing());
        menu
    }

    /// The menu as an 80-column screen shows it, row by row.
    pub fn screen(menu: &NamesMenu, theme: &Theme) -> Vec<String> {
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

    fn press(menu: &mut NamesMenu, code: KeyCode) -> Action {
        menu.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            code == KeyCode::Esc,
        )
    }

    fn shifted(menu: &mut NamesMenu, code: KeyCode) -> Action {
        menu.key(KeyEvent::new(code, KeyModifiers::SHIFT), false)
    }

    /// With the first words, the menu is two rows. Choosing a model shows
    /// the four ways with the two plans on, and what each lacks.
    #[test]
    fn choosing_a_model_shows_the_ways_with_the_plans_on() {
        let mut menu = menu_with(&[]);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        println!("{text}");
        assert!(
            text.contains("▸ ( • ) First words") || text.contains("▸ (•) First words"),
            "{text}"
        );
        assert!(text.contains("Named by a model"), "{text}");
        assert!(!text.contains("Haiku"), "no ways yet: {text}");
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        assert!(menu.by_model);
        let rows = screen(&menu, &Theme::terminal());
        let text = rows.join("\n");
        println!("{text}");
        for row in [
            "[x] Haiku 5.5 on the Claude plan",
            "[x] GPT-6 Luna on the ChatGPT plan",
            "[ ] Haiku 5.5 with the Anthropic API key",
            "[ ] GPT-6 Luna with the OpenAI API key",
        ] {
            assert!(text.contains(row), "{row}: {text}");
        }
        for reason in [
            "No ChatGPT login",
            "No Anthropic API key",
            "No OpenAI API key",
        ] {
            assert!(text.contains(reason), "{reason}: {text}");
        }
        assert!(text.contains("Then the first words."), "{text}");
        assert!(text.contains("Enter  save"), "{text}");
        assert!(rows.iter().all(|row| row.chars().count() <= 80));
    }

    /// The saved ways open in their order, and Enter saves the ones on.
    #[test]
    fn the_saved_order_opens_and_enter_saves_the_ways_on() {
        let saved = [Way::OpenAiKey, Way::ClaudePlan];
        let mut menu = menu_with(&saved);
        assert!(menu.by_model);
        assert_eq!(
            menu.ways,
            [
                (Way::OpenAiKey, true),
                (Way::ClaudePlan, true),
                (Way::ChatGptPlan, false),
                (Way::ClaudeKey, false),
            ]
        );
        assert_eq!(
            press(&mut menu, KeyCode::Enter),
            Action::Save(saved.to_vec())
        );
        // Turn the ChatGPT plan on, with the cursor on it.
        for _ in 0..4 {
            press(&mut menu, KeyCode::Down);
        }
        press(&mut menu, KeyCode::Char(' '));
        assert_eq!(
            press(&mut menu, KeyCode::Enter),
            Action::Save(vec![Way::OpenAiKey, Way::ClaudePlan, Way::ChatGptPlan])
        );
    }

    /// Shift+Up and Shift+Down move the way chosen, and the cursor goes
    /// with it. It stops at the ends.
    #[test]
    fn shift_with_the_arrows_moves_a_way() {
        let mut menu = menu_with(&default_ways_all());
        // On the first way: moving up does nothing.
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Down);
        shifted(&mut menu, KeyCode::Up);
        assert_eq!(menu.ways[0].0, Way::ClaudePlan);
        shifted(&mut menu, KeyCode::Down);
        assert_eq!(menu.ways[1].0, Way::ClaudePlan);
        assert_eq!(menu.selected, 3, "the cursor stays on the way");
        shifted(&mut menu, KeyCode::Down);
        shifted(&mut menu, KeyCode::Down);
        shifted(&mut menu, KeyCode::Down);
        assert_eq!(menu.ways[3].0, Way::ClaudePlan);
        assert_eq!(menu.selected, 5);
        assert_eq!(
            press(&mut menu, KeyCode::Enter),
            Action::Save(vec![
                Way::ChatGptPlan,
                Way::ClaudeKey,
                Way::OpenAiKey,
                Way::ClaudePlan
            ])
        );
        // On a row for how to name, it moves nothing.
        let mut menu = menu_with(&[Way::ClaudePlan]);
        let before = menu.ways.clone();
        shifted(&mut menu, KeyCode::Down);
        assert_eq!(menu.ways, before);
    }

    fn default_ways_all() -> Vec<Way> {
        Way::ALL.to_vec()
    }

    /// The first words save as none, and a model with nothing on isn't
    /// saved.
    #[test]
    fn the_first_words_save_as_none_and_a_model_needs_a_way() {
        let mut menu = menu_with(&[Way::ClaudePlan]);
        press(&mut menu, KeyCode::Char(' '));
        assert!(!menu.by_model, "the first row is First words");
        assert_eq!(press(&mut menu, KeyCode::Enter), Action::Save(Vec::new()));
        // Back to a model, with no way on.
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        assert_eq!(menu.ways[0], (Way::ClaudePlan, false));
        assert_eq!(press(&mut menu, KeyCode::Enter), Action::Stay);
        let text = screen(&menu, &Theme::terminal()).join("\n");
        assert!(
            text.contains("Turn on a way, or choose First words."),
            "{text}"
        );
        assert_eq!(press(&mut menu, KeyCode::Esc), Action::Close);
    }
}
