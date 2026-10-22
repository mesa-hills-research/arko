//! The skills built into Arko: general advice on writing, held in the
//! program so no file is needed. They are listed with the skills of the
//! skill folders, and a skill of the same name in those folders replaces one.
//! A session can leave some or all of them out (see [`BuiltinSkills`]).
//!
//! Each is the `SKILL.md` in its folder here, and `BUILTIN` below lists them.
//! A skill is loaded on demand unless its `pre_loaded` is true, which puts
//! its whole text in the instructions of every session.

use serde::{Deserialize, Serialize};

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
pub(crate) const BUILTIN: [Builtin; 2] = [
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
];

impl Builtin {
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

/// Which built-in skills a session gives its agents: all but those turned
/// off. It is a session setting, kept with the session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuiltinSkills {
    /// None of them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub none: bool,
    /// These by name. A name no skill has, such as one a later version
    /// dropped, is passed over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub off: Vec<String>,
}

impl BuiltinSkills {
    /// Every built-in skill on.
    pub fn is_all(&self) -> bool {
        *self == Self::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `name` and `description` lines of a skill's front matter.
    fn front_matter(text: &str) -> (Option<String>, Option<String>) {
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("---"), "front matter opens the file");
        let (mut name, mut description) = (None, None);
        for line in lines {
            if line == "---" {
                return (name, description);
            }
            if let Some(value) = line.strip_prefix("name: ") {
                name = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("description: ") {
                description = Some(value.to_string());
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
            let (name, description) = front_matter(skill.text);
            assert_eq!(name.as_deref(), Some(skill.name));
            let description = description.expect("a description");
            assert!(
                description.contains("Load it before"),
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
        let none = BuiltinSkills {
            none: true,
            off: Vec::new(),
        };
        assert!(!none.is_all());
        assert_eq!(none.left_out(), ["user-facing-prose", "repo-docs-style"]);
        assert!(!none.gives("repo-docs-style"));
        let one = BuiltinSkills {
            none: false,
            off: vec!["repo-docs-style".into()],
        };
        assert_eq!(one.left_out(), ["repo-docs-style"]);
        assert!(one.gives("user-facing-prose") && !one.gives("repo-docs-style"));
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
