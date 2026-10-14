//! `/model`'s picker: the model of the main agent (before the conversation
//! starts) or of new workers, in two steps. The first lists the models by
//! the way they are reached, the ways that can be used first; a way that
//! can't is one dim row saying what it needs, so the list shows what fin
//! can reach without a menu that changes shape from one machine to the
//! next. The second lists the efforts that model takes. Enter applies:
//! the picker sends the session the `/model` line a user would type, so
//! the line prompt, the page and the picker change models one way.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, footer, frame_box, put};
use super::theme::Theme;
use super::wrap::{self, width as text_width};
use crate::agents::Effort;
use crate::glyphs;
use crate::model_command::Target;
use crate::providers;
use crate::remote::Models;

/// A row of the first step.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A way the models are reached, as a heading.
    Group(String),
    /// A way that can't be used, and what it needs.
    Unavailable { group: String, needs: String },
    /// A model: its name as `/model` takes it, and its alias.
    Model { name: String, alias: &'static str },
    /// New workers follow the main agent's model (the workers' picker).
    FollowMain { model: String },
    /// Typing another model's name.
    Other,
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(
            self,
            Row::Model { .. } | Row::FollowMain { .. } | Row::Other
        )
    }
}

/// Where the picker is.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The models.
    Models,
    /// Typing a model's name: what is typed, and why the last one wasn't
    /// taken.
    Typing { text: String, error: Option<String> },
    /// The efforts `model` takes; `selected` past them is Back.
    Efforts {
        model: String,
        efforts: Vec<Effort>,
        selected: usize,
    },
}

/// The picker, open.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPicker {
    pub target: Target,
    pub rows: Vec<Row>,
    /// The first step's row selected.
    pub selected: usize,
    pub step: Step,
    /// The agent's model and effort now.
    pub model: String,
    pub effort: String,
    /// The workers follow the main agent's model now.
    pub follows: bool,
    /// The main agent's model, which following means.
    pub main: String,
    /// The providers the session reaches, for a typed model.
    pub reachable: Vec<String>,
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Stay,
    Close,
    /// Send the session `line`, and say `said` in the status line.
    Apply {
        line: String,
        said: String,
    },
}

impl ModelPicker {
    /// The picker for `target`, from the session's `models`; `needs` says
    /// what a model needs that isn't there.
    pub fn open(target: Target, models: &Models, needs: impl Fn(&str) -> Option<String>) -> Self {
        let (model, effort) = match target {
            Target::Main => (models.main.clone(), models.main_effort.clone()),
            Target::Workers => (models.workers.clone(), models.workers_effort.clone()),
        };
        let auth = match models.openai_auth.as_deref() {
            Some("OpenAI API key") => Some(providers::OpenAiAuth::ApiKey),
            Some(_) => Some(providers::OpenAiAuth::ChatGpt),
            None => None,
        };
        let mut rows = Vec::new();
        if target == Target::Workers {
            rows.push(Row::FollowMain {
                model: models.main.clone(),
            });
        }
        let current = [models.main.as_str(), models.workers.as_str()];
        for group in providers::model_menu(needs, auth, &current) {
            match group.needs {
                Some(needs) => rows.push(Row::Unavailable {
                    group: group.name,
                    needs,
                }),
                None => {
                    rows.push(Row::Group(group.name));
                    rows.extend(
                        group
                            .models
                            .into_iter()
                            .map(|(name, alias)| Row::Model { name, alias }),
                    );
                }
            }
        }
        rows.push(Row::Other);
        let follows = target == Target::Workers && models.workers_follow;
        // On the setting now.
        let selected = rows
            .iter()
            .position(|row| match row {
                Row::FollowMain { .. } => follows,
                Row::Model { name, .. } => !follows && *name == model,
                _ => false,
            })
            .or_else(|| rows.iter().position(Row::selectable))
            .unwrap_or(0);
        Self {
            target,
            rows,
            selected,
            step: Step::Models,
            model,
            effort,
            follows,
            main: models.main.clone(),
            reachable: models.reachable.clone(),
        }
    }

    /// The box's title: whose model it is.
    pub fn title(&self) -> String {
        match (&self.target, &self.step) {
            (Target::Main, Step::Efforts { model, .. }) => {
                format!("Main agent's effort on {model}")
            }
            (Target::Workers, Step::Efforts { model, .. }) => {
                format!("New workers' effort on {model}")
            }
            (Target::Main, _) => "Main agent's model".to_string(),
            (Target::Workers, _) => "Model of workers started from now on".to_string(),
        }
    }

    /// The second step, for `name`; a model that doesn't reason is applied
    /// at once.
    fn efforts(&mut self, name: String) -> Outcome {
        let Ok(model) = providers::resolve_model(&name) else {
            return Outcome::Stay;
        };
        let efforts = providers::efforts(&model);
        if efforts.is_empty() {
            return self.apply(&name, None);
        }
        // On the effort now, else the nearest the model takes.
        let now = providers::parse_effort(&self.effort).unwrap_or(Effort::Xhigh);
        let fit = providers::fit_effort(&model, now);
        let selected = efforts
            .iter()
            .position(|effort| *effort == fit)
            .unwrap_or(0);
        self.step = Step::Efforts {
            model: name,
            efforts,
            selected,
        };
        Outcome::Stay
    }

    fn apply(&self, model: &str, effort: Option<Effort>) -> Outcome {
        let effort_words = effort.map(|e| format!(" {e}")).unwrap_or_default();
        let effort_said = effort
            .map(|e| format!(" at {e} effort"))
            .unwrap_or_default();
        match self.target {
            Target::Main => Outcome::Apply {
                line: format!("/model {model}{effort_words}"),
                said: format!("The main agent runs {model}{effort_said}"),
            },
            Target::Workers => Outcome::Apply {
                line: format!("/model worker {model}{effort_words}"),
                said: format!("Workers started from now on run {model}{effort_said}"),
            },
        }
    }

    /// A key, in the step open.
    pub fn key(&mut self, key: KeyEvent, close: bool) -> Outcome {
        match &mut self.step {
            Step::Models => {
                let selectable: Vec<usize> = (0..self.rows.len())
                    .filter(|n| self.rows[*n].selectable())
                    .collect();
                let at = selectable
                    .iter()
                    .position(|n| *n == self.selected)
                    .unwrap_or(0);
                let to = |at: usize| selectable.get(at).copied().unwrap_or(0);
                match key.code {
                    _ if close => return Outcome::Close,
                    KeyCode::Up => self.selected = to(at.saturating_sub(1)),
                    KeyCode::Down => self.selected = to((at + 1).min(selectable.len() - 1)),
                    KeyCode::PageUp | KeyCode::Home => self.selected = to(0),
                    KeyCode::PageDown | KeyCode::End => self.selected = to(selectable.len() - 1),
                    KeyCode::Enter => match self.rows[self.selected].clone() {
                        Row::Model { name, .. } => return self.efforts(name),
                        Row::FollowMain { .. } => {
                            return Outcome::Apply {
                                line: "/model worker main".to_string(),
                                said: format!(
                                    "Workers started from now on follow the main agent's model, {}",
                                    self.main
                                ),
                            };
                        }
                        Row::Other => {
                            self.step = Step::Typing {
                                text: String::new(),
                                error: None,
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
                Outcome::Stay
            }
            Step::Typing { text, error } => {
                match key.code {
                    _ if close => self.step = Step::Models,
                    KeyCode::Char(c) if !c.is_whitespace() => {
                        text.push(c);
                        *error = None;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                        *error = None;
                    }
                    KeyCode::Enter if !text.is_empty() => {
                        let name = text.clone();
                        match providers::resolve_model(&name) {
                            Err(problem) => *error = Some(problem),
                            Ok(model) => {
                                match needs(&self.reachable, &providers::model_name(&model)) {
                                    Some(needs) => {
                                        *error = Some(providers::needs_text(
                                            &providers::model_name(&model),
                                            &needs,
                                        ))
                                    }
                                    None => return self.efforts(providers::model_name(&model)),
                                }
                            }
                        }
                    }
                    _ => {}
                }
                Outcome::Stay
            }
            Step::Efforts {
                model,
                efforts,
                selected,
            } => {
                let back = efforts.len();
                match key.code {
                    _ if close => self.step = Step::Models,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(back),
                    KeyCode::Enter if *selected == back => self.step = Step::Models,
                    KeyCode::Enter => {
                        let (model, effort) = (model.clone(), efforts[*selected]);
                        return self.apply(&model, Some(effort));
                    }
                    _ => {}
                }
                Outcome::Stay
            }
        }
    }
}

/// What the model `name` needs that the session doesn't have: Claude Code
/// installed and signed in, or a provider the session's client reaches
/// (`reachable`, as the session publishes it).
pub fn needs(reachable: &[String], name: &str) -> Option<String> {
    let model = providers::resolve_model(name).ok()?;
    match &model.provider {
        arko_ai::Provider::ClaudeCode => providers::missing_credential(name, None),
        provider
            if reachable
                .iter()
                .any(|key| key == providers::provider_key(provider)) =>
        {
            None
        }
        provider => Some(providers::credential_name(provider).to_string()),
    }
}

/// A word on each effort, to choose by.
fn about(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "fastest, thinks least",
        Effort::Medium => "quicker, thinks less",
        Effort::High => "thinks more",
        Effort::Xhigh => "thinks hard, fin's default",
        Effort::Max => "thinks most, slowest",
    }
}

/// Draws the picker over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, picker: &ModelPicker) {
    let width = 95.min(area.width.saturating_sub(2));
    let inner_width = width.saturating_sub(4) as usize;
    let marker = |chosen: bool| {
        if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        }
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0;
    let keys: &[(&str, &str)];
    match &picker.step {
        Step::Models => {
            keys = &[("↑ ↓", "choose"), ("Enter", "next"), ("Esc", "close")];
            // The columns: the name, the alias, and a note.
            let name_width = picker
                .rows
                .iter()
                .map(|row| match row {
                    Row::Model { name, .. } => text_width(name),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
                + 2;
            let alias_width = picker
                .rows
                .iter()
                .map(|row| match row {
                    Row::Model { alias, .. } => text_width(alias),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
                + 2;
            for (n, row) in picker.rows.iter().enumerate() {
                let chosen = n == picker.selected;
                if chosen {
                    selected_line = lines.len();
                }
                let style = if chosen {
                    theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    theme.text()
                };
                match row {
                    Row::Group(name) => {
                        if !lines.is_empty() {
                            lines.push(Line::default());
                        }
                        lines.push(Line::from(Span::styled(
                            name.clone(),
                            theme.dim().add_modifier(Modifier::BOLD),
                        )));
                    }
                    Row::Unavailable { group, needs } => {
                        if !lines.is_empty() {
                            lines.push(Line::default());
                        }
                        lines.push(Line::from(vec![
                            Span::styled(group.clone(), theme.dim().add_modifier(Modifier::BOLD)),
                            Span::styled(format!("  needs {needs}"), theme.dim()),
                        ]));
                    }
                    Row::Model { name, alias } => {
                        let current = !picker.follows && *name == picker.model;
                        let mut spans = vec![
                            marker(chosen),
                            Span::styled(format!("{name:<name_width$}"), style),
                            Span::styled(format!("{alias:<alias_width$}"), theme.dim()),
                        ];
                        if current {
                            spans.push(Span::styled(
                                format!("now, {} effort", picker.effort),
                                theme.accent(),
                            ));
                        }
                        lines.push(Line::from(spans));
                    }
                    Row::FollowMain { model } => {
                        let mut spans = vec![
                            marker(chosen),
                            Span::styled(format!("The main agent's model, {model}  "), style),
                        ];
                        if picker.follows {
                            spans.push(Span::styled("now", theme.accent()));
                        }
                        lines.push(Line::from(spans));
                    }
                    Row::Other => {
                        lines.push(Line::default());
                        lines.push(Line::from(Span::styled(
                            "Another model",
                            theme.dim().add_modifier(Modifier::BOLD),
                        )));
                        lines.push(Line::from(vec![
                            marker(chosen),
                            Span::styled(
                                glyphs::render(
                                    "Type a model ID, an alias, cc:<model> or openrouter:<slug>…",
                                )
                                .into_owned(),
                                style,
                            ),
                        ]));
                        if chosen {
                            selected_line = lines.len() - 1;
                        }
                    }
                }
            }
        }
        Step::Typing { text, error } => {
            keys = &[("Enter", "next"), ("Esc", "back")];
            for line in wrap::wrap(
                &[Span::styled(
                    "Type a model ID, an alias, cc:<model> for Claude Code, or openrouter:<slug> for a model on OpenRouter.",
                    theme.text(),
                )],
                inner_width.saturating_sub(2),
                0,
            ) {
                lines.push(Line::from(line));
            }
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", glyphs::render("›")), theme.accent()),
                Span::styled(text.clone(), theme.text()),
                Span::styled(" ", theme.chosen()),
            ]));
            if let Some(error) = error {
                lines.push(Line::default());
                for line in wrap::wrap(
                    &[Span::styled(error.clone(), theme.error())],
                    inner_width.saturating_sub(2),
                    0,
                ) {
                    lines.push(Line::from(line));
                }
            }
        }
        Step::Efforts {
            model,
            efforts,
            selected,
        } => {
            keys = &[("↑ ↓", "choose"), ("Enter", "apply"), ("Esc", "back")];
            let same_model = *model == picker.model && !picker.follows;
            for (n, effort) in efforts.iter().enumerate() {
                let chosen = n == *selected;
                if chosen {
                    selected_line = lines.len();
                }
                let style = if chosen {
                    theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    theme.text()
                };
                let mut spans = vec![
                    marker(chosen),
                    Span::styled(format!("{:<8}", effort.to_string()), style),
                    Span::styled(about(*effort).to_string(), theme.dim()),
                ];
                if same_model && effort.to_string() == picker.effort {
                    spans.push(Span::styled("  now", theme.accent()));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::default());
            let chosen = *selected == efforts.len();
            if chosen {
                selected_line = lines.len();
            }
            lines.push(Line::from(vec![
                marker(chosen),
                Span::styled(
                    format!("{} Back to the models", glyphs::render("←")),
                    if chosen {
                        theme.accent().add_modifier(Modifier::BOLD)
                    } else {
                        theme.text()
                    },
                ),
            ]));
        }
    }
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, &picker.title());
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let visible = body.height as usize;
    let scroll = selected_line
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    put(frame, body, &lines, scroll);
    footer(frame, rect, theme, keys);
}

/// The note `/model` opens with once the conversation has started: the
/// main agent keeps its model, and the workers' picker is one key away.
pub fn draw_notice(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    text: &str,
    choice: usize,
    workers: bool,
) {
    let width = 60.min(area.width.saturating_sub(4)).max(20);
    let inner = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> =
        wrap::wrap(&[Span::styled(text.to_string(), theme.text())], inner, 0)
            .into_iter()
            .map(Line::from)
            .collect();
    lines.push(Line::default());
    let options: &[&str] = if workers {
        &["Choose the workers' model", "Close"]
    } else {
        &["Close"]
    };
    let mut row = Vec::new();
    for (n, option) in options.iter().enumerate() {
        if n > 0 {
            row.push(Span::raw("  "));
        }
        let style = if n == choice {
            theme.chosen()
        } else {
            theme.key()
        };
        row.push(Span::styled(format!(" {option} "), style));
    }
    lines.push(Line::from(row));
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner_rect = frame_box(frame, rect, theme, "Main agent's model");
    let body = Rect::new(
        inner_rect.x + 1,
        inner_rect.y + 1,
        inner_rect.width.saturating_sub(2),
        inner_rect.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    let keys: &[(&str, &str)] = if workers {
        &[("← →", "choose"), ("Enter", "go on"), ("Esc", "close")]
    } else {
        &[("Enter", "close")]
    };
    footer(frame, rect, theme, keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The picker as an 80-column screen shows it, row by row.
    fn screen(picker: &ModelPicker) -> Vec<String> {
        let (width, height) = (80, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, &Theme::terminal(), frame.area(), picker))
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

    #[test]
    fn every_section_lines_up_and_its_note_fits_an_80_column_screen() {
        let models = Models {
            main: "gpt-6-luna".into(),
            main_effort: "high".into(),
            workers: "gpt-6-luna".into(),
            workers_effort: "high".into(),
            openai_auth: Some("ChatGPT".into()),
            ..Models::default()
        };
        let picker = ModelPicker::open(Target::Main, &models, |name| {
            name.starts_with("cc:")
                .then(|| "Claude Code installed".to_string())
        });
        let rows = screen(&picker);
        let column = |text: &str| {
            let row = rows
                .iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            row.find(text).unwrap()
        };
        assert!(
            rows.iter()
                .any(|row| row
                    .contains("Claude Code, on your Claude plan  needs Claude Code installed")),
            "{rows:#?}"
        );
        // The headings start in one column, and the rows under them in another.
        assert_eq!(
            column("Another model"),
            column("OpenAI, on your ChatGPT plan")
        );
        assert_eq!(column("Type a model ID"), column("gpt-6.1-sol"));
    }
}
