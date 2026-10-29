//! `/skills NAME`, one skill in full, and `/skills remove NAME`, which
//! removes a skill an agent saved. (`/skills` alone lists them, in
//! `harness`.)

use std::path::Path;
use std::sync::Arc;

use crate::Harness;
use crate::details::SkillFacts;
use crate::remote::Level;
use crate::skill_install;

/// What `/skills` adds to a skill's place: who saved it, the models it is
/// listed for, and that only the main agent gets it, as in
/// ", added by the agent 28 Oct, for haiku, main agent only".
pub(crate) fn place_notes(skill: &SkillFacts) -> String {
    let mut notes = String::new();
    if let Some(added) = &skill.added {
        notes.push_str(&format!(
            ", added by the agent {}",
            skill_install::day(&added.added)
        ));
    }
    if !skill.models.is_empty() {
        notes.push_str(&format!(", for {}", crate::and_list(&skill.models)));
    }
    if !skill.workers {
        notes.push_str(", main agent only");
    }
    notes
}

impl Harness {
    /// What the main agent's skill tools need of the session, whose main
    /// agent runs with `main_config`.
    pub(crate) fn skill_context(
        &self,
        main_config: &arko_agents::AgentConfig,
    ) -> Arc<skill_install::Context> {
        let gauges = self.gauges.clone();
        let output = self.output.clone();
        let feed = self.feed.clone();
        Arc::new(skill_install::Context {
            root: self.root.clone(),
            workspace: self.settings.workspace.clone(),
            home: crate::skills::home(),
            session: self.session.id.clone(),
            title: Arc::new(move || {
                let title = gauges.title.lock().unwrap().clone();
                // (Not the placeholder of a session with no name yet.)
                let named = !["", "New session", "New chat"].contains(&title.as_str());
                named.then_some(title)
            }),
            model_names: main_config
                .skills_config
                .as_ref()
                .map(|skills| skills.model_names.clone())
                .unwrap_or_default(),
            approver: self.asker.clone(),
            fetch: Arc::new(skill_install::Https),
            notice: Arc::new(move |level, text| {
                output.echo(&match level {
                    Level::Info => output.dim(text),
                    Level::Warn => output.warn(text),
                    Level::Error => output.error(text),
                });
                feed.notice(level, text);
            }),
            writing: Default::default(),
        })
    }

    /// `/skills` with `argument`: `remove NAME`, or a skill's name.
    pub(crate) fn skills_command(&self, argument: &str) {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let lines = match words.as_slice() {
            ["remove", name] => vec![self.remove_skill(name)],
            ["remove", ..] => {
                vec!["/skills remove takes a skill's name, such as /skills remove release-notes.".to_string()]
            }
            [name] => self.skill_lines(name),
            _ => vec![
                "/skills takes a skill's name, or remove and a name, such as /skills release-notes.".to_string(),
            ],
        };
        for line in lines {
            self.output.line(&line);
        }
    }

    /// `/skills remove NAME`'s reply.
    fn remove_skill(&self, name: &str) -> String {
        let workspace = &self.settings.workspace;
        if let Some(record) = skill_install::saved_skill(&self.root, workspace, name) {
            return skill_install::remove_saved(&self.root, &record.path)
                .unwrap_or_else(|error| error);
        }
        let home = crate::skills::home();
        match skill_install::skill_path(&self.root, workspace, home.as_deref(), name) {
            Some(path) => format!(
                "{name} wasn't added by an agent. Delete {} to remove it.",
                skill_install::with_tilde(&path, home.as_deref())
            ),
            None if crate::builtin_skills::find(name).is_some() => {
                format!("{name} is built into Arko. /builtin-skills {name} off leaves it out.")
            }
            None => format!("There's no skill named {name}."),
        }
    }

    /// `/skills NAME`'s lines: the skill's name, description and place, who
    /// saved it and why, who gets it, its models and its files.
    fn skill_lines(&self, name: &str) -> Vec<String> {
        let out = &self.output;
        self.publish_details();
        let skills = match self.details.skills() {
            Ok(skills) => skills,
            Err(error) => {
                return vec![out.error(&format!("The skills couldn't be read ({error})."))];
            }
        };
        let Some(skill) = skills.skills.iter().find(|skill| skill.name == name) else {
            return vec![format!("There's no skill named {name}.")];
        };
        let home = crate::skills::home();
        let mut lines = vec![out.bold(&skill.name), skill.description.clone()];
        lines.push(out.dim(&match &skill.path {
            None => "built in".to_string(),
            Some(path) => {
                let mut place = skill_install::with_tilde(path, home.as_deref());
                let project = crate::skills::project_folder(&self.settings.workspace);
                if path.starts_with(&project)
                    && crate::arko_dir::has_own_ignore(&self.settings.workspace)
                {
                    place.push_str(", out of git");
                }
                place
            }
        }));
        if let Some(added) = &skill.added {
            let mut text = "Added by the agent".to_string();
            if let Some(title) = &added.session_title {
                text.push_str(&format!(" in {title}"));
            }
            text.push_str(&format!(
                " on {}",
                added
                    .added
                    .with_timezone(&chrono::Local)
                    .format("%-d %b %Y")
            ));
            if let Some(source) = &added.source {
                text.push_str(&format!(", from {source}"));
            }
            lines.push(text);
            lines.push(format!("{}  {}", out.dim("why    "), added.why));
        }
        lines.push(format!(
            "{}  {}",
            out.dim("workers"),
            if skill.workers {
                "get it"
            } else {
                "main agent only"
            }
        ));
        lines.push(format!(
            "{}  {}",
            out.dim("models "),
            if skill.models.is_empty() {
                "all".to_string()
            } else {
                skill.models.join(", ")
            }
        ));
        if let Some(path) = &skill.path {
            lines.push(format!("{}  {}", out.dim("files  "), files_line(path)));
        }
        if skill.added.is_some() {
            lines.push(out.dim(&format!("/skills remove {name} removes it.")));
        }
        lines
    }
}

/// A skill's files, `SKILL.md` first with its length: "SKILL.md (3,210
/// characters), checklist.md, scripts/release.sh". A skill that is a file
/// (a `.md`, or a bundle) is that file.
fn files_line(path: &Path) -> String {
    if !path.is_dir() {
        return path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
    }
    let mut files = Vec::new();
    collect(path, path, &mut files);
    files.sort_by_key(|file| (file != "SKILL.md", file.clone()));
    files
        .into_iter()
        .map(|file| {
            if file == "SKILL.md" {
                let characters = std::fs::read_to_string(path.join("SKILL.md"))
                    .map(|text| text.chars().count())
                    .unwrap_or_default();
                format!("SKILL.md ({} characters)", thousands(characters))
            } else {
                file
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The files under `folder`, by their paths in `root`, without hidden ones.
fn collect(root: &Path, folder: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(inside) = path.strip_prefix(root) {
            files.push(
                inside
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

/// `4000` as `4,000`.
fn thousands(number: usize) -> String {
    let digits: Vec<char> = number.to_string().chars().rev().collect();
    let groups: Vec<String> = digits
        .chunks(3)
        .map(|group| group.iter().rev().collect())
        .collect();
    groups.into_iter().rev().collect::<Vec<_>>().join(",")
}
