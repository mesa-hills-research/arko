//! `/model`'s picker: the model of the main agent (before the conversation
//! starts) or of new workers, in two steps. The first lists the models by
//! the way they are reached, the ways that can be used first; a way that
//! can't is one dim row saying what it needs, so the list shows what Arko
//! can reach without a menu that changes shape from one machine to the
//! next. OpenRouter's group lists the models picked, with their context and
//! prices, and a row that opens all of OpenRouter's models to filter and
//! choose from. The second step lists the efforts that model takes, on the
//! one it ran at last. Enter applies: the picker sends the session the
//! `/model` line a user would type, so the line prompt, the page and the
//! picker change models one way.

pub use crate::models::{menu, names, needs};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
use crate::openrouter::{self, Catalog, Listed};
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
    /// All of OpenRouter's models, in a step of their own.
    AllOpenRouter,
    /// New workers follow the main agent's model (the workers' picker).
    FollowMain { model: String },
    /// Typing another model's name.
    Other,
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(
            self,
            Row::Model { .. } | Row::AllOpenRouter | Row::FollowMain { .. } | Row::Other
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
    /// OpenRouter's models, filtered by what is typed (by name and slug),
    /// with the one selected among those that match.
    Browse { filter: String, selected: usize },
    /// The efforts `model` takes; `selected` past them is Back. `last` is
    /// the one it ran at last (found as the step opens, since drawing reads
    /// no file).
    Efforts {
        model: String,
        efforts: Vec<Effort>,
        selected: usize,
        last: Option<Effort>,
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
    /// Arko's folder, where the efforts models ran at and OpenRouter's
    /// settings are kept (none in a picker without them).
    pub root: Option<std::path::PathBuf>,
    /// OpenRouter's models as they are in memory, and those picked before
    /// with what was known of them.
    pub catalog: Catalog,
    pub recent: Vec<Listed>,
    /// What the OpenRouter settings rule out or set, in a few words.
    pub summary: Option<String>,
    /// How each model billed per token is billed (`API, $5/$25`), by name.
    /// The plans' models have none.
    pub billing: std::collections::BTreeMap<String, String>,
    /// The filter and the model the browse step was on, while the efforts
    /// of a model chosen there are open: Back goes there.
    via_browse: Option<(String, usize)>,
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
    #[cfg(test)]
    pub fn open(target: Target, models: &Models, needs: impl Fn(&str) -> Option<String>) -> Self {
        Self::open_in(target, models, needs, None)
    }

    /// [`open`](Self::open) with Arko's folder `root`, which holds the
    /// OpenRouter settings and the efforts models ran at. It starts fetching
    /// OpenRouter's models when they are due.
    pub fn open_in(
        target: Target,
        models: &Models,
        needs: impl Fn(&str) -> Option<String>,
        root: Option<&std::path::Path>,
    ) -> Self {
        let (catalog, recent, summary) = match root {
            Some(root) => {
                // (Reading the settings first, so that the fetch knows them.)
                let found = (
                    openrouter::catalog(root),
                    openrouter::recent(root),
                    openrouter::Settings::load(root).summary(),
                );
                openrouter::refresh_soon(root);
                found
            }
            None => (Catalog::default(), Vec::new(), None),
        };
        let (model, effort) = match target {
            Target::Main => (models.main.clone(), models.main_effort.clone()),
            Target::Workers => (models.workers.clone(), models.workers_effort.clone()),
        };
        let mut rows = Vec::new();
        if target == Target::Workers {
            rows.push(Row::FollowMain {
                model: models.main.clone(),
            });
        }
        for group in menu(models, needs) {
            match group.needs {
                Some(needs) => rows.push(Row::Unavailable {
                    group: group.name,
                    needs,
                }),
                None => {
                    let openrouter = group.name.starts_with("OpenRouter");
                    rows.push(Row::Group(group.name));
                    rows.extend(
                        group
                            .models
                            .into_iter()
                            .map(|(name, alias)| Row::Model { name, alias }),
                    );
                    if openrouter && root.is_some() {
                        rows.push(Row::AllOpenRouter);
                    }
                }
            }
        }
        rows.push(Row::Other);
        let billing = rows
            .iter()
            .filter_map(|row| match row {
                Row::Model { name, .. } => {
                    crate::models::billing(models, name).map(|label| (name.clone(), label))
                }
                _ => None,
            })
            .collect();
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
            root: root.map(std::path::Path::to_path_buf),
            catalog,
            recent,
            summary,
            billing,
            via_browse: None,
        }
    }

    /// Takes OpenRouter's models and the settings as they are now (they
    /// were fetched, or changed).
    pub fn refresh_openrouter(&mut self) {
        if let Some(root) = &self.root {
            self.catalog = openrouter::catalog(root);
            self.recent = openrouter::recent(root);
            self.summary = openrouter::Settings::load(root).summary();
        }
    }

    /// Whether the models the settings need are being fetched.
    fn loading(&self) -> bool {
        self.root.as_deref().is_some_and(openrouter::loading_for)
    }

    /// What OpenRouter says of the model `name`, in few characters, from
    /// its list in memory or else from when it was picked. Another model
    /// billed per token says how (`API, $5/$25`).
    fn details(&self, name: &str) -> String {
        self.catalog
            .get(name)
            .or_else(|| self.recent.iter().find(|model| model.name == name))
            .map(Listed::short)
            .or_else(|| self.billing.get(name).cloned())
            .unwrap_or_default()
    }

    /// The box's title: whose model it is.
    pub fn title(&self) -> String {
        match (&self.target, &self.step) {
            (Target::Main, Step::Efforts { model, .. }) => {
                format!("Main agent's effort on {model}")
            }
            (Target::Workers, Step::Efforts { model, .. }) => {
                format!("Workers' effort on {model}")
            }
            (Target::Main, Step::Browse { .. }) => "Main agent's model on OpenRouter".to_string(),
            (Target::Workers, Step::Browse { .. }) => "Workers' model on OpenRouter".to_string(),
            (Target::Main, _) => "Main agent's model".to_string(),
            (Target::Workers, _) => "Workers' model".to_string(),
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
        // On the effort it ran at last, else the effort now or the nearest
        // the model takes.
        let now = providers::parse_effort(&self.effort).unwrap_or(Effort::Xhigh);
        let last = self
            .root
            .as_deref()
            .and_then(|root| providers::LastModels::effort_of(root, &name));
        let fit = last.unwrap_or_else(|| providers::fit_effort(&model, now));
        let selected = efforts
            .iter()
            .position(|effort| *effort == fit)
            .unwrap_or(0);
        self.step = Step::Efforts {
            model: name,
            efforts,
            selected,
            last,
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
                said: format!(
                    "Workers that get no profile or model of their own now run {model}{effort_said}"
                ),
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
                        Row::AllOpenRouter => {
                            self.step = Step::Browse {
                                filter: String::new(),
                                selected: 0,
                            }
                        }
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
            Step::Browse { filter, selected } => {
                let ctrl = key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                let count = self.catalog.search(filter).len();
                let last = count.saturating_sub(1);
                match key.code {
                    _ if close => self.step = Step::Models,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(last),
                    KeyCode::PageUp => *selected = selected.saturating_sub(10),
                    KeyCode::PageDown => *selected = (*selected + 10).min(last),
                    KeyCode::Home => *selected = 0,
                    KeyCode::End => *selected = last,
                    KeyCode::Char(c) if !ctrl && !c.is_control() => {
                        filter.push(c);
                        *selected = 0;
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                        *selected = 0;
                    }
                    KeyCode::Enter => {
                        let found = self.catalog.search(filter);
                        match found.get(*selected) {
                            Some(model) => {
                                let name = model.name.clone();
                                self.via_browse = Some((filter.clone(), *selected));
                                return self.efforts(name);
                            }
                            // Nothing listed, or the list can't be had: the
                            // slug typed is taken as it is, by typing it.
                            None => {
                                let typed = filter.split_whitespace().collect::<String>();
                                self.step = Step::Typing {
                                    text: format!("{}{typed}", providers::OPENROUTER),
                                    error: None,
                                };
                            }
                        }
                    }
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
                ..
            } => {
                let back = efforts.len();
                // Back goes to the models, or to the list they were chosen
                // from.
                let before = |via: Option<(String, usize)>| match via {
                    Some((filter, selected)) => Step::Browse { filter, selected },
                    None => Step::Models,
                };
                match key.code {
                    _ if close => self.step = before(self.via_browse.take()),
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(back),
                    KeyCode::Enter if *selected == back => {
                        self.step = before(self.via_browse.take())
                    }
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

/// A word on each effort, to choose by.
fn about(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "fastest, thinks least",
        Effort::Medium => "quicker, thinks less",
        Effort::High => "thinks more",
        Effort::Xhigh => "thinks hard, Arko's default",
        Effort::Max => "thinks most, slowest",
    }
}

/// Draws the picker over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, picker: &ModelPicker) {
    // As wide as the banner when it is about as wide, so their right edges
    // meet instead of showing side by side.
    let banner = area.width.saturating_sub(4);
    let width = if banner < 95 + 6 { banner } else { 95 };
    let inner_width = width.saturating_sub(4) as usize;
    let marker = |chosen: bool| {
        if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        }
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Lines that stay where they are while `lines` scroll.
    let mut header: Vec<Line<'static>> = Vec::new();
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
            // What OpenRouter says of its models, in a column of its own.
            let details_width = picker
                .rows
                .iter()
                .map(|row| match row {
                    Row::Model { name, .. } => text_width(&picker.details(name)),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
                + 2;
            if picker.target == Target::Workers {
                lines.push(Line::from(Span::styled(
                    "For workers started from now on. Running workers keep their model.",
                    theme.dim(),
                )));
                lines.push(Line::default());
            }
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
                        let mut spans = vec![Span::styled(
                            name.clone(),
                            theme.dim().add_modifier(Modifier::BOLD),
                        )];
                        // What the OpenRouter settings do to its models.
                        if name.starts_with("OpenRouter")
                            && let Some(summary) = &picker.summary
                        {
                            spans.push(Span::styled(format!("  {summary}"), theme.dim()));
                        }
                        lines.push(Line::from(spans));
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
                        if details_width > 2 {
                            let details = picker.details(name);
                            spans.push(Span::styled(
                                format!("{:<details_width$}", glyphs::render(&details)),
                                theme.dim(),
                            ));
                        }
                        if current {
                            spans.push(Span::styled(
                                format!("now, {} effort", picker.effort),
                                theme.accent(),
                            ));
                        }
                        lines.push(Line::from(spans));
                        // What to know before choosing it, under its name.
                        if let Some(note) = providers::model_note(name) {
                            for line in wrap::wrap(
                                &[Span::styled(note, theme.dim())],
                                inner_width.saturating_sub(4),
                                0,
                            ) {
                                let mut spans = vec![Span::raw("    ")];
                                spans.extend(line);
                                lines.push(Line::from(spans));
                            }
                        }
                    }
                    Row::AllOpenRouter => {
                        let count = picker.catalog.models.len();
                        let text = match count {
                            0 => "All OpenRouter models…".to_string(),
                            count => format!("All OpenRouter models ({count})…"),
                        };
                        let mut spans = vec![
                            marker(chosen),
                            Span::styled(glyphs::render(&text).into_owned(), style),
                        ];
                        if count == 0 && picker.loading() {
                            spans.push(Span::styled("  loading", theme.dim()));
                        }
                        lines.push(Line::from(spans));
                    }
                    Row::FollowMain { model } => {
                        let mut spans = vec![
                            marker(chosen),
                            Span::styled(format!("Same as the main agent ({model})  "), style),
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
                        // In line with its heading: chosen, it shows by its
                        // colour alone.
                        lines.push(Line::from(Span::styled(
                            glyphs::render(
                                "Type a model ID, an alias, cc:<model> or openrouter:<slug>…",
                            )
                            .into_owned(),
                            style,
                        )));
                        if chosen {
                            selected_line = lines.len() - 1;
                        }
                    }
                }
            }
        }
        Step::Browse { filter, selected } => {
            keys = &[
                ("type", "filter"),
                ("↑ ↓", "choose"),
                ("Enter", "next"),
                ("Esc", "back"),
            ];
            header.push(Line::from(vec![
                Span::styled(format!("{} ", glyphs::render("›")), theme.accent()),
                Span::styled(filter.clone(), theme.text()),
                Span::styled(" ", theme.chosen()),
            ]));
            let total = picker.catalog.models.len();
            let found = picker.catalog.search(filter);
            let mut counts = vec![Span::styled(
                match total {
                    0 => String::new(),
                    total => format!("  {} of {total} models", found.len()),
                },
                theme.dim(),
            )];
            if total > 0
                && let Some(problem) = &picker.catalog.problem
            {
                counts.push(Span::styled(format!("  {problem}"), theme.warning()));
            }
            header.push(Line::from(counts));
            let say = |lines: &mut Vec<Line<'static>>, text: &str, style| {
                for line in wrap::wrap(
                    &[Span::styled(text.to_string(), style)],
                    inner_width.saturating_sub(2),
                    0,
                ) {
                    lines.push(Line::from(line));
                }
            };
            if found.is_empty() {
                let none = match (&picker.catalog.problem, total, picker.loading()) {
                    (_, 0, true) => {
                        say(&mut lines, "Loading OpenRouter's models…", theme.dim());
                        None
                    }
                    (Some(problem), 0, false) => {
                        say(&mut lines, &format!("{problem}."), theme.error());
                        Some("Enter types a slug.")
                    }
                    (None, 0, false) => {
                        say(
                            &mut lines,
                            "OpenRouter's models aren't loaded.",
                            theme.dim(),
                        );
                        Some("Enter types a slug.")
                    }
                    _ => {
                        say(&mut lines, "No model matches.", theme.dim());
                        Some("Enter takes what is typed as a slug.")
                    }
                };
                if let Some(none) = none {
                    say(&mut lines, none, theme.dim());
                }
            }
            let label_width = found
                .iter()
                .map(|model| text_width(&model.label))
                .max()
                .unwrap_or(0)
                .min(28)
                + 2;
            let slug_width = found
                .iter()
                .map(|model| text_width(model.slug()))
                .max()
                .unwrap_or(0)
                .min(44)
                + 2;
            for (n, model) in found.iter().enumerate() {
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
                    Span::styled(fit(&model.label, label_width), style),
                    Span::styled(fit(model.slug(), slug_width), theme.dim()),
                    Span::styled(glyphs::render(&model.short()).into_owned(), theme.dim()),
                ];
                if !picker.follows && model.name == picker.model {
                    spans.push(Span::styled(
                        format!("  now, {} effort", picker.effort),
                        theme.accent(),
                    ));
                }
                lines.push(Line::from(spans));
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
            last,
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
                } else if *last == Some(*effort) {
                    spans.push(Span::styled("  last used", theme.dim()));
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
    // (The list of models doesn't change the box's height as it filters.)
    let wanted = match picker.step {
        Step::Browse { .. } => (lines.len() + header.len() + 4).max(14),
        _ => lines.len() + 4,
    };
    let height = (wanted as u16).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, &picker.title());
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let head = (header.len() as u16).min(body.height);
    put(
        frame,
        Rect::new(body.x, body.y, body.width, head),
        &header,
        0,
    );
    let list = Rect::new(body.x, body.y + head, body.width, body.height - head);
    let visible = list.height as usize;
    let scroll = selected_line
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    put(frame, list, &lines, scroll);
    footer(frame, rect, theme, keys);
}

/// `text` as wide as `width` columns, cut short with an ellipsis or padded.
fn fit(text: &str, width: usize) -> String {
    let mut shown = glyphs::render(text).into_owned();
    if text_width(&shown) + 1 > width {
        while text_width(&shown) + 3 > width && shown.pop().is_some() {}
        shown.push_str(&glyphs::render("…"));
    }
    let pad = width.saturating_sub(text_width(&shown));
    shown + &" ".repeat(pad)
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
        // The headings start in one column, and the models under them in
        // another. The row to type a model lines up with its heading.
        assert_eq!(
            column("Another model"),
            column("OpenAI, on your ChatGPT plan")
        );
        assert_eq!(column("Type a model ID"), column("Another model"));
    }

    /// A model billed per token says so with its price beside its name, as
    /// worker_options and the web page say it. A plan's model needs nothing
    /// beside it, its group names the plan.
    #[test]
    fn the_models_billed_per_token_show_it_beside_their_names() {
        let models = Models {
            main: "cc:claude-opus-5-5".into(),
            main_effort: "high".into(),
            workers: "cc:claude-opus-5-5".into(),
            workers_effort: "high".into(),
            openai_auth: Some("ChatGPT".into()),
            ..Models::default()
        };
        let picker = ModelPicker::open(Target::Workers, &models, |_| None);
        let rows = screen(&picker);
        let row = |name: &str| {
            rows.iter()
                .find(|row| row.split_whitespace().any(|word| word == name))
                .unwrap_or_else(|| panic!("no {name} in {rows:#?}"))
                .clone()
        };
        let opus = crate::models::billing(&models, "opus").unwrap();
        assert!(opus.starts_with("API, $"), "{opus}");
        assert!(row("opus").contains(&opus), "{rows:#?}");
        assert!(!row("cc:opus").contains("API"), "{rows:#?}");
        assert!(
            picker
                .details("gemini-3.1-pro-preview")
                .starts_with("API, $")
        );
        for plan in ["cc:claude-sonnet-5-5", "gpt-6.1-sol"] {
            assert_eq!(picker.details(plan), "", "{plan}");
        }
        // On an API key OpenAI's models are billed per token too.
        let keyed = Models {
            openai_auth: Some("OpenAI API key".into()),
            ..models
        };
        assert!(
            crate::models::billing(&keyed, "sol").is_some_and(|label| label.starts_with("API"))
        );
        assert_eq!(crate::models::billing(&keyed, "cc:opus"), None);
    }

    /// Claude Mythos 5.1 shows the access it needs on the row under its
    /// name, and the models without a note follow one another.
    #[test]
    fn mythos_shows_the_access_it_needs_under_its_name() {
        let models = Models {
            main: "cc:claude-opus-5-5".into(),
            main_effort: "high".into(),
            workers: "cc:claude-opus-5-5".into(),
            workers_effort: "high".into(),
            ..Models::default()
        };
        let picker = ModelPicker::open(Target::Main, &models, |name| {
            (!name.starts_with("cc:")).then(|| "a key".to_string())
        });
        let rows = screen(&picker);
        let row = |text: &str| {
            rows.iter()
                .position(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"))
        };
        let mythos = row("cc:claude-mythos-5-1");
        let note = &rows[mythos + 1];
        assert!(note.contains(providers::MYTHOS_ACCESS), "{rows:#?}");
        assert!(
            note.find(providers::MYTHOS_ACCESS).unwrap()
                > rows[mythos].find("cc:claude-mythos-5-1").unwrap(),
            "{rows:#?}"
        );
        assert_eq!(row("cc:claude-haiku-5-5") + 1, mythos, "{rows:#?}");
    }

    /// The picker's rows inside its box.
    fn inside(picker: &ModelPicker) -> Vec<String> {
        screen(picker)
            .into_iter()
            .map(|row| row.trim_end().trim_end_matches('│').trim_end().to_string())
            .collect()
    }

    /// What a session with only OpenRouter's key can use.
    fn only_openrouter(name: &str) -> Option<String> {
        (!name.starts_with("openrouter:")).then(|| "a key".to_string())
    }

    fn key(picker: &mut ModelPicker, code: KeyCode) -> Outcome {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE), false)
    }

    fn type_in(picker: &mut ModelPicker, text: &str) {
        for c in text.chars() {
            key(picker, KeyCode::Char(c));
        }
    }

    /// A root whose OpenRouter settings are `zdr` and `region` (so that
    /// no other test shares its list), with `models` listed and `recent`
    /// picked before.
    fn openrouter_root(
        region: openrouter::Region,
        models: &[(&str, &str, u64, &str, &str)],
        recent: &str,
    ) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = openrouter::Settings::default();
        settings.set_region(region);
        settings.save(temp.path()).unwrap();
        // (The file as the settings write it, with the models picked.)
        let file = std::fs::read_to_string(temp.path().join(openrouter::FILE)).unwrap();
        let mut file: serde_json::Value = serde_json::from_str(&file).unwrap();
        file["recent"] = serde_json::from_str(recent).unwrap();
        std::fs::write(temp.path().join(openrouter::FILE), file.to_string()).unwrap();
        let entries = models
            .iter()
            .map(|(slug, name, context, input, output)| {
                serde_json::json!({
                    "id": slug, "name": name, "context_length": context,
                    "pricing": {"prompt": input, "completion": output},
                    "supported_parameters": ["tools"],
                })
            })
            .collect();
        openrouter::seed_entries_for_tests(&openrouter::Settings::load(temp.path()), entries);
        temp
    }

    fn openrouter_models(recent: &[&str]) -> Models {
        Models {
            main: "gpt-6-luna".into(),
            main_effort: "high".into(),
            workers: "gpt-6-luna".into(),
            workers_effort: "high".into(),
            reachable: vec!["openai".into(), "openrouter".into()],
            openai_auth: Some("ChatGPT".into()),
            recent: recent.iter().map(|name| name.to_string()).collect(),
            ..Models::default()
        }
    }

    const LISTED: &[(&str, &str, u64, &str, &str)] = &[
        (
            "moonshotai/kimi-k3",
            "MoonshotAI: Kimi K3",
            262_144,
            "0.0000006",
            "0.0000025",
        ),
        (
            "z-ai/glm-5",
            "Z.ai: GLM 5",
            202_752,
            "0.0000004",
            "0.0000015",
        ),
        (
            "acme/big-kimi-clone",
            "Acme: Big Kimi Clone",
            1_048_576,
            "0.000002",
            "0.00001",
        ),
    ];

    /// OpenRouter's group shows what is known of each model beside it, the
    /// settings beside its heading, and a row for all its models.
    #[test]
    fn openrouter_rows_show_the_details_the_settings_and_a_row_for_all_models() {
        let root = openrouter_root(
            openrouter::Region::Eu,
            LISTED,
            r#"["moonshotai/kimi-k3", {"slug": "acme/kept", "context": 131072, "input_price": 0.1, "output_price": 0.5}, "acme/unknown"]"#,
        );
        let mut settings = openrouter::Settings::load(root.path());
        settings.set_sort(openrouter::Sort::Price);
        settings.save(root.path()).unwrap();
        let models = openrouter_models(&[
            "openrouter:moonshotai/kimi-k3",
            "openrouter:acme/kept",
            "openrouter:acme/unknown",
        ]);
        let picker =
            ModelPicker::open_in(Target::Main, &models, only_openrouter, Some(root.path()));
        let rows = inside(&picker);
        let row = |text: &str| {
            rows.iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"))
                .clone()
        };
        // From the list in memory, and from what was kept at the pick.
        assert!(
            row("openrouter:moonshotai/kimi-k3").ends_with("262k · $0.60/$2.50"),
            "{rows:#?}"
        );
        assert!(
            row("openrouter:acme/kept").ends_with("131k · $0.10/$0.50"),
            "{rows:#?}"
        );
        assert!(
            row("openrouter:acme/unknown")
                .trim_end()
                .ends_with("openrouter:acme/unknown"),
            "{rows:#?}"
        );
        assert!(
            row("OpenRouter, billed per token").ends_with("EU only, price first"),
            "{rows:#?}"
        );
        // The row for all the models is last in the group, with how many.
        let all = rows
            .iter()
            .position(|row| row.contains("All OpenRouter models (3)…"))
            .unwrap();
        let unknown = rows
            .iter()
            .position(|row| row.contains("openrouter:acme/unknown"))
            .unwrap();
        assert_eq!(all, unknown + 1, "{rows:#?}");
        // The columns of the details line up.
        let column = |row: String, text: &str| row.find(text).map(|at| row[..at].chars().count());
        assert_eq!(
            column(row("openrouter:moonshotai/kimi-k3"), "262k"),
            column(row("openrouter:acme/kept"), "131k"),
            "{rows:#?}"
        );
        // A picker without Arko's folder has no such row.
        let bare = ModelPicker::open(Target::Main, &models, only_openrouter);
        assert!(!bare.rows.contains(&Row::AllOpenRouter));
    }

    /// The row for all OpenRouter's models opens a list to filter by what
    /// is typed. Enter chooses, and the efforts follow, with Back and Esc
    /// returning to the list as it was.
    #[test]
    fn all_of_openrouters_models_are_browsed_by_a_filter() {
        let root = openrouter_root(openrouter::Region::Us, LISTED, "[]");
        let models = openrouter_models(&[]);
        let mut picker =
            ModelPicker::open_in(Target::Main, &models, only_openrouter, Some(root.path()));
        while picker.rows[picker.selected] != Row::AllOpenRouter {
            key(&mut picker, KeyCode::Down);
        }
        key(&mut picker, KeyCode::Enter);
        assert!(matches!(&picker.step, Step::Browse { filter, selected: 0 } if filter.is_empty()));
        assert_eq!(picker.title(), "Main agent's model on OpenRouter");
        let rows = screen(&picker);
        for text in ["Kimi K3", "GLM 5", "Big Kimi Clone", "› ", "3 of 3 models"] {
            assert!(
                rows.iter().any(|row| row.contains(text)),
                "{text}: {rows:#?}"
            );
        }
        let kimi = rows.iter().find(|row| row.contains("Kimi K3")).unwrap();
        assert!(
            kimi.contains("moonshotai/kimi-k3") && kimi.contains("262k · $0.60/$2.50"),
            "{kimi}"
        );

        // Typing filters by name and slug, those that start with it first.
        type_in(&mut picker, "kimi");
        let rows = screen(&picker);
        assert!(
            rows.iter().any(|row| row.contains("2 of 3 models")),
            "{rows:#?}"
        );
        assert!(!rows.iter().any(|row| row.contains("GLM 5")), "{rows:#?}");
        let first = rows.iter().position(|row| row.contains("Kimi K3")).unwrap();
        let second = rows
            .iter()
            .position(|row| row.contains("Big Kimi Clone"))
            .unwrap();
        assert!(first < second, "{rows:#?}");
        key(&mut picker, KeyCode::Backspace);
        type_in(&mut picker, "i z-ai");
        assert!(
            screen(&picker)
                .iter()
                .any(|row| row.contains("No model matches"))
        );
        type_in(&mut picker, "x");
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        key(&mut picker, KeyCode::Backspace);
        assert!(
            matches!(&picker.step, Step::Browse { filter, .. } if filter == "kim"),
            "{:?}",
            picker.step
        );

        // Enter chooses the second, and its efforts follow.
        key(&mut picker, KeyCode::Down);
        key(&mut picker, KeyCode::Enter);
        match &picker.step {
            Step::Efforts { model, .. } => assert_eq!(model, "openrouter:acme/big-kimi-clone"),
            other => panic!("{other:?}"),
        }
        // Esc goes back to the list, as it was, and again to the models.
        key(&mut picker, KeyCode::Down);
        let back = picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true);
        assert_eq!(back, Outcome::Stay);
        assert!(
            matches!(&picker.step, Step::Browse { filter, selected: 1 } if filter == "kim"),
            "{:?}",
            picker.step
        );
        picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true);
        assert_eq!(picker.step, Step::Models);

        // Choosing again and an effort applies the line a user would type.
        while picker.rows[picker.selected] != Row::AllOpenRouter {
            key(&mut picker, KeyCode::Down);
        }
        key(&mut picker, KeyCode::Enter);
        type_in(&mut picker, "glm");
        key(&mut picker, KeyCode::Enter);
        key(&mut picker, KeyCode::Enter);
        // (The effort the model starts on is the effort now, high.)
        match key(&mut picker, KeyCode::Enter) {
            Outcome::Apply { line, .. } => assert_eq!(line, "/model openrouter:z-ai/glm-5 high"),
            other => panic!("{other:?}"),
        }
    }

    /// While the models load the list says so, and when they can't be had
    /// it says why and takes a slug typed.
    #[test]
    fn the_list_says_when_it_loads_and_why_it_cant() {
        let root = tempfile::tempdir().unwrap();
        let mut settings = openrouter::Settings::default();
        settings.set_region(openrouter::Region::UsOrEu);
        settings.save(root.path()).unwrap();
        openrouter::seed_failure_for_tests(&settings, "it couldn't be reached");
        let models = openrouter_models(&[]);
        let mut picker =
            ModelPicker::open_in(Target::Main, &models, only_openrouter, Some(root.path()));
        while picker.rows[picker.selected] != Row::AllOpenRouter {
            key(&mut picker, KeyCode::Down);
        }
        let rows = inside(&picker);
        assert!(
            rows.iter()
                .any(|row| row.trim_end().ends_with("All OpenRouter models…")),
            "{rows:#?}"
        );
        key(&mut picker, KeyCode::Enter);
        let rows = screen(&picker);
        assert!(
            rows.iter()
                .any(|row| row.contains("Couldn't load the models (it couldn't be reached).")),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Enter types a slug.")),
            "{rows:#?}"
        );
        // Typing a filter and Enter takes it as a slug, to type further.
        type_in(&mut picker, "acme/x");
        key(&mut picker, KeyCode::Enter);
        assert!(
            matches!(&picker.step, Step::Typing { text, .. } if text == "openrouter:acme/x"),
            "{:?}",
            picker.step
        );
        key(&mut picker, KeyCode::Enter);
        assert!(
            matches!(&picker.step, Step::Efforts { model, .. } if model == "openrouter:acme/x")
        );
    }

    /// The efforts step starts on the effort the model ran at last, and
    /// says so, else on the effort now.
    #[test]
    fn the_efforts_start_on_the_one_the_model_last_ran_at() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("last-model.json"),
            r#"{"main": "gpt-6-luna", "efforts": {"gpt-6.1-sol": "medium", "gpt-6-luna": "high", "gpt-6-astra": "extreme"}}"#,
        )
        .unwrap();
        let models = openrouter_models(&[]);
        let efforts = |name: &str| {
            let mut picker =
                ModelPicker::open_in(Target::Main, &models, |_| None, Some(root.path()));
            picker.efforts(name.to_string());
            match &picker.step {
                Step::Efforts {
                    efforts, selected, ..
                } => (efforts[*selected].to_string(), inside(&picker)),
                other => panic!("{other:?}"),
            }
        };
        let (selected, rows) = efforts("gpt-6.1-sol");
        assert_eq!(selected, "medium");
        assert!(
            rows.iter()
                .any(|row| row.contains("medium") && row.contains("last used")),
            "{rows:#?}"
        );
        // The model now, at its effort now: that says "now" and not more.
        let (selected, rows) = efforts("gpt-6-luna");
        assert_eq!(selected, "high");
        assert!(
            rows.iter()
                .any(|row| row.contains("high") && row.trim_end().ends_with("now")),
            "{rows:#?}"
        );
        // An effort the model doesn't take, or none kept: the effort now.
        assert_eq!(efforts("gpt-6-astra").0, "high");
        assert_eq!(efforts("gemini").0, "high");
        // A picker without Arko's folder starts on the effort now.
        let mut bare = ModelPicker::open(Target::Main, &models, |_| None);
        bare.efforts("gpt-6.1-sol".to_string());
        assert!(
            matches!(&bare.step, Step::Efforts { efforts, selected, .. } if efforts[*selected].to_string() == "high")
        );
    }
}
