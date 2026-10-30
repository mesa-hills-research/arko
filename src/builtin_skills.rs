//! The skills built into Arko: general advice on writing, held in the
//! program so no file is needed. They are listed with the skills of the
//! skill folders, a skill of the same name in those folders replaces one, and
//! `ARKO_BUILTIN_SKILLS`, `ARKO_BUILTIN_SKILLS_OFF`, their flags and
//! `/builtin-skills` leave them out (see `BuiltinSkills`). An app reads their
//! names with [`names`].
//!
//! Each is the `SKILL.md` in its folder here, and `BUILTIN` below lists them.
//! A skill is loaded on demand unless its `pre_loaded` is true, which puts
//! its whole text in the instructions of every session.

use serde::{Deserialize, Serialize};

use crate::Harness;
use crate::agents::{MAIN, WORKER};
use crate::remote::Source;

/// A skill built into Arko.
pub(crate) struct Builtin {
    /// Its name, also the name of its folder here.
    pub name: &'static str,
    /// Its `SKILL.md`, the name and description in front matter, then the
    /// instructions.
    pub text: &'static str,
    /// Its text is always in the agents' instructions, which costs its
    /// tokens in every session. Else the agents load it with `load_skill`
    /// when a task calls for it, and see only its description.
    pub pre_loaded: bool,
}

/// The built-in skills, in the order they are listed.
pub(crate) const BUILTIN: [Builtin; 3] = [
    Builtin {
        name: "user-facing-prose",
        text: include_str!("builtin_skills/user-facing-prose/SKILL.md"),
        pre_loaded: false,
    },
    Builtin {
        name: "repo-docs-style",
        text: include_str!("builtin_skills/repo-docs-style/SKILL.md"),
        pre_loaded: false,
    },
    Builtin {
        name: "ponytail",
        text: include_str!("builtin_skills/ponytail/SKILL.md"),
        pre_loaded: false,
    },
];

impl Builtin {
    /// What the skill is for, the `description` in its front matter (on one
    /// line, or on the indented lines after `description: >`).
    #[cfg(any(test, feature = "tui"))]
    pub fn description(&self) -> String {
        let mut lines = self
            .text
            .lines()
            .skip_while(|line| !line.starts_with("description:"));
        let first = lines.next().unwrap_or_default()["description:".len()..].trim();
        if first != ">" {
            return first.to_string();
        }
        let folded: Vec<&str> = lines
            .take_while(|line| line.starts_with(' '))
            .map(str::trim)
            .collect();
        folded.join(" ")
    }

    /// The `SKILL.md` the agents' skill system loads: `text`, with
    /// `auto_load: true` in its front matter when the skill is pre-loaded.
    pub fn skill_md(&self) -> std::borrow::Cow<'static, str> {
        // The front matter closes at the first line of three dashes after
        // the one that opens it.
        match self.text.find("\n---\n") {
            Some(close) if self.pre_loaded => format!(
                "{}\nauto_load: true{}",
                &self.text[..close],
                &self.text[close..]
            )
            .into(),
            _ => self.text.into(),
        }
    }
}

/// The name of the setting for whether the agents get the built-in skills
/// at all, and the line under it. The terminal and the web page show both.
pub const SETTING_NAME: &str = "Built-in skills";
pub const SETTING_ABOUT: &str = "Off leaves out all of them, whatever the rows below say.";

/// What a change does once the conversation has started.
pub const AFTER_START: &str =
    "The conversation going on keeps its skills. New workers and new conversations get the change.";

/// The built-in skills' names, in the order they are listed. These are the
/// names `Options::builtin_skills_off` takes.
pub fn names() -> [&'static str; BUILTIN.len()] {
    BUILTIN.map(|skill| skill.name)
}

/// The name of the built-in skill `name`, if there is one.
pub(crate) fn find(name: &str) -> Option<&'static Builtin> {
    BUILTIN.iter().find(|skill| skill.name == name)
}

/// Which built-in skills a session gives its agents: all but those turned
/// off. It is a session setting, kept with the session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuiltinSkills {
    /// None of them (`ARKO_BUILTIN_SKILLS=0`, `--no-builtin-skills`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub none: bool,
    /// These by name (`ARKO_BUILTIN_SKILLS_OFF`, `--no-builtin-skill`). A
    /// name no skill has, such as one a later version dropped, is passed
    /// over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub off: Vec<String>,
}

impl BuiltinSkills {
    /// Every built-in skill on.
    pub fn is_all(&self) -> bool {
        *self == Self::default()
    }

    /// The setting `none` and `off` make of `from`'s words, with a warning
    /// for each name no built-in skill has. A session skips such a name and
    /// goes on, since one that a later version dropped is no reason to
    /// refuse a session.
    pub fn new(none: bool, off: &[String], from: &str) -> (Self, Vec<String>) {
        let mut kept: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for name in off
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
        {
            if find(name).is_none() {
                warnings.push(format!(
                    "{from} names `{name}`, which isn't a built-in skill (they are {}), so Arko skips it",
                    crate::and_list(&names())
                ));
            } else if !kept.iter().any(|seen| seen == name) {
                kept.push(name.to_string());
            }
        }
        (Self { none, off: kept }, warnings)
    }

    /// Whether the agents get the built-in skill `name`.
    pub fn gives(&self, name: &str) -> bool {
        !self.none && !self.off.iter().any(|off| off == name)
    }

    /// The built-in skills the agents get.
    pub fn given(&self) -> impl Iterator<Item = &'static Builtin> + '_ {
        BUILTIN.iter().filter(|skill| self.gives(skill.name))
    }

    /// The names of the built-in skills turned off.
    pub fn left_out(&self) -> Vec<&'static str> {
        BUILTIN
            .iter()
            .filter(|skill| !self.gives(skill.name))
            .map(|skill| skill.name)
            .collect()
    }
}

impl Harness {
    /// `/builtin-skills`, typed at the prompt, sent from the page or chosen
    /// in the settings: says which built-in skills the agents get, or with
    /// `on` or `off` (all of them) or a name and `on` or `off` (one) changes
    /// that. Before the conversation starts the agents are built again.
    /// After it the running workflow gives the main agent and the workers
    /// the new skills for the runs that start from now on, and the
    /// conversation going on keeps its own.
    pub(crate) fn builtin_skills_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.builtin_skills.clone();
        let result = self.builtin_skills_request(argument);
        let changed = before != self.settings.builtin_skills;
        self.setting_reply(result, source, changed);
    }

    fn builtin_skills_request(&mut self, argument: &str) -> Result<String, String> {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let mut choice = self.settings.builtin_skills.clone();
        let (subject, is, on) = match words.as_slice() {
            [] => return Ok(self.builtin_skills_line()),
            [state @ ("on" | "off")] => {
                choice.none = *state == "off";
                ("Built-in skills", "are", *state == "on")
            }
            [name, state @ ("on" | "off")] => {
                if find(name).is_none() {
                    return Err(format!(
                        "{name} isn't a built-in skill. They are {}.",
                        crate::and_list(&names())
                    ));
                }
                let listed = choice.off.iter().any(|off| off == name);
                if *state == "off" && !listed {
                    choice.off.push(name.to_string());
                } else if *state == "on" {
                    choice.off.retain(|off| off != name);
                }
                (*name, "is", *state == "on")
            }
            _ => {
                return Err(
                    "/builtin-skills takes on or off, or a built-in skill's name and on or off, such as /builtin-skills repo-docs-style off."
                        .to_string(),
                );
            }
        };
        let state = if on { "on" } else { "off" };
        if choice == self.settings.builtin_skills {
            return Ok(format!("{subject} {is} {state} already."));
        }
        let mut settings = self.settings.clone();
        settings.builtin_skills = choice;
        let mut text = format!("{subject} {is} now {state}.");
        if settings.builtin_skills.none && words.len() == 2 {
            text.push_str(
                " Built-in skills are off, so the agents get none until you turn them on.",
            );
        }
        if self.conversation_started() {
            self.settings = settings;
            self.save_session();
            self.publish_models();
            // Each agent's own, for the skills of its model.
            let skills = |model: &str, worker: bool| {
                crate::skills::agent_config(
                    &self.skill_folders,
                    &self.settings.builtin_skills,
                    model,
                    worker,
                )
            };
            let main = (MAIN, skills(&self.settings.main.model, false));
            let workers = (self.settings.max_workers > 0)
                .then(|| (WORKER, skills(&self.settings.worker.model, true)));
            for (agent, skills) in [main].into_iter().chain(workers) {
                if let Err(error) = self.workflow().set_agent_skills(agent, skills) {
                    log::warn!("could not give the {agent} agent its new skills: {error}");
                }
            }
            text.push(' ');
            text.push_str(AFTER_START);
        } else {
            self.rebuild_agents(settings)?;
        }
        Ok(text)
    }

    /// Which built-in skills the agents get, as `/builtin-skills` says it.
    fn builtin_skills_line(&self) -> String {
        let choice = &self.settings.builtin_skills;
        let given: Vec<&str> = choice.given().map(|skill| skill.name).collect();
        let mut text = if choice.none {
            "Built-in skills are off, so the agents get none of them.".to_string()
        } else if given.is_empty() {
            "Built-in skills are on, but every one is left out.".to_string()
        } else {
            let mut text = format!(
                "Built-in skills are on, so the agents get {}.",
                crate::and_list(&given)
            );
            let left_out = choice.left_out();
            if !left_out.is_empty() {
                text.push_str(&format!(
                    " {} {} left out.",
                    crate::and_list(&left_out),
                    if left_out.len() == 1 { "is" } else { "are" }
                ));
            }
            text
        };
        text.push_str(
            " /builtin-skills on or off changes all of them, and /builtin-skills NAME on or off one.",
        );
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `name` line of a skill's front matter.
    fn front_matter_name(text: &str) -> Option<String> {
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("---"), "front matter opens the file");
        for line in lines {
            if line == "---" {
                return None;
            }
            if let Some(value) = line.strip_prefix("name: ") {
                return Some(value.to_string());
            }
        }
        panic!("front matter closes");
    }

    /// Each text names itself as its folder does and describes when to
    /// load it. The table, not the text, says whether it is pre-loaded.
    #[test]
    fn each_text_has_its_folders_name_and_a_description() {
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/builtin_skills");
        let mut folders: Vec<String> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        folders.sort();
        let mut names: Vec<&str> = BUILTIN.iter().map(|skill| skill.name).collect();
        names.sort();
        assert_eq!(folders, names, "every folder is in the table");
        for skill in &BUILTIN {
            let on_disk = std::fs::read_to_string(folder.join(skill.name).join("SKILL.md"));
            assert_eq!(on_disk.unwrap(), skill.text, "{}", skill.name);
            assert_eq!(front_matter_name(skill.text).as_deref(), Some(skill.name));
            let description = skill.description();
            assert!(
                description.contains("Load it before") || description.contains("Use it for"),
                "{} says when to load it",
                skill.name
            );
            assert!(!skill.text.contains("auto_load"), "the table decides");
        }
        assert!(
            BUILTIN[0].text.contains("\n# User-facing prose\n")
                && BUILTIN[1]
                    .text
                    .contains("\n# Repository documentation style\n")
        );
    }

    /// A description on the lines after `description: >` reads as one.
    #[test]
    fn a_description_on_folded_lines_is_read_as_one() {
        let ponytail = find("ponytail").unwrap().description();
        assert!(ponytail.starts_with(
            "Finish the whole task with the best change, kept as small as that allows, split"
        ));
        assert!(ponytail.ends_with("complains about over-engineering."));
        assert!(!ponytail.contains('\n'));
        let one_line = find("repo-docs-style").unwrap().description();
        assert!(one_line.starts_with("Rules for repository documentation"));
    }

    /// The texts teach plain prose, and are written in it. Their examples
    /// of what to avoid sit in quotes, in tables and in code.
    #[test]
    fn the_texts_follow_their_own_rules() {
        for skill in &BUILTIN {
            assert!(!skill.text.contains("; "), "{} has a semicolon", skill.name);
            for line in skill.text.lines() {
                for prefix in ["Note:", "Tip:", "Warning:", "Important:"] {
                    assert!(!line.starts_with(prefix), "{}: {line}", skill.name);
                }
                // A heading never ends in a colon, nor does a list item.
                if line.starts_with('#') || line.starts_with("- ") {
                    assert!(!line.ends_with(':'), "{}: {line}", skill.name);
                }
            }
            // It is general advice, with no one person, project or date.
            for word in ["owner", "Arko", "arko_", "mhr_"] {
                assert!(!skill.text.contains(word), "{} has {word}", skill.name);
            }
            assert!(
                !skill
                    .text
                    .split(|c: char| !c.is_ascii_digit())
                    .any(|number| number.len() == 4 && number.starts_with("20")),
                "{} has a date",
                skill.name
            );
        }
    }

    /// Both skills load on demand now. A skill with `pre_loaded` set has its
    /// whole text in the instructions, as the skill system does for
    /// `auto_load: true`.
    #[test]
    fn a_pre_loaded_skill_is_in_the_instructions_and_the_others_load_on_demand() {
        use arko_agents::{EmbeddedSkill, SkillManager, SkillsConfig};
        assert!(BUILTIN.iter().all(|skill| !skill.pre_loaded));
        let text = "---\nname: x\ndescription: An x.\n---\n\nDo x.\n";
        let skill = |name: &'static str, pre_loaded| Builtin {
            name,
            text: text.trim_end(),
            pre_loaded,
        };
        let (pre, later) = (skill("x", true), skill("x", false));
        assert_eq!(later.skill_md(), text.trim_end());
        assert!(
            pre.skill_md()
                .starts_with("---\nname: x\ndescription: An x.\nauto_load: true\n---\n")
        );
        let load = |skill: &Builtin| {
            let config = SkillsConfig {
                embedded: vec![EmbeddedSkill::new(skill.name, skill.skill_md())],
                ..Default::default()
            };
            SkillManager::load(&config, std::path::Path::new(".")).unwrap()
        };
        let (pre, later) = (load(&pre), load(&later));
        let context = pre.prompt_context();
        assert_eq!(context.auto_loaded_skills.len(), 1);
        assert!(context.auto_loaded_skills[0].content.contains("Do x."));
        assert!(context.on_demand_skills.is_empty());
        let context = later.prompt_context();
        assert!(context.auto_loaded_skills.is_empty());
        assert_eq!(context.on_demand_skills[0].description, "An x.");
    }

    #[test]
    fn the_choice_leaves_out_all_or_some_by_name() {
        let all = BuiltinSkills::default();
        assert!(all.is_all());
        assert!(all.left_out().is_empty());
        assert_eq!(all.given().count(), BUILTIN.len());
        let (none, warnings) = BuiltinSkills::new(true, &[], "x");
        assert!(warnings.is_empty());
        assert!(!none.is_all());
        assert_eq!(
            none.left_out(),
            ["user-facing-prose", "repo-docs-style", "ponytail"]
        );
        assert!(!none.gives("repo-docs-style"));
        let (one, _) = BuiltinSkills::new(
            false,
            &[
                " repo-docs-style".into(),
                "repo-docs-style".into(),
                "".into(),
            ],
            "x",
        );
        assert_eq!(one.off, ["repo-docs-style"]);
        assert_eq!(one.left_out(), ["repo-docs-style"]);
        assert!(one.gives("user-facing-prose") && !one.gives("repo-docs-style"));
    }

    /// A name no built-in skill has is skipped, with a warning that says
    /// where it came from, and the names around it still count.
    #[test]
    fn a_name_that_is_not_a_built_in_skill_is_skipped_with_a_warning() {
        let (typo, warnings) = BuiltinSkills::new(
            false,
            &["repo-doc-style".into(), "user-facing-prose".into()],
            "--no-builtin-skill",
        );
        assert_eq!(typo.off, ["user-facing-prose"]);
        assert_eq!(
            warnings,
            [
                "--no-builtin-skill names `repo-doc-style`, which isn't a built-in skill (they are user-facing-prose, repo-docs-style, and ponytail), so Arko skips it"
            ]
        );
    }

    #[test]
    fn the_choice_is_saved_only_when_it_differs() {
        assert_eq!(
            serde_json::to_string(&BuiltinSkills::default()).unwrap(),
            "{}"
        );
        let kept: BuiltinSkills =
            serde_json::from_str(r#"{"off":["repo-docs-style","gone"]}"#).unwrap();
        assert!(kept.gives("user-facing-prose") && !kept.gives("repo-docs-style"));
        // A name no skill has now is passed over.
        assert_eq!(kept.left_out(), ["repo-docs-style"]);
        let none: BuiltinSkills = serde_json::from_str(r#"{"none":true}"#).unwrap();
        assert!(none.given().next().is_none());
    }
}
