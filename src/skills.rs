//! Skills: instructions for particular kinds of work, each a folder with a
//! `SKILL.md` (a name and description in YAML front matter, then the
//! instructions) and the files it uses. The agents see each skill's name
//! and description, and load a skill with arko_agents' `load_skill` when a
//! task calls for it. One marked `auto_load: true` is always in their
//! instructions.
//!
//! Arko reads skills from the project's `.agents/skills`, `.claude/skills`,
//! `.skills` and `.arko/skills`, then from `skills` in Arko's folder,
//! `~/.agents/skills` and `~/.claude/skills`. The two folders agents save
//! skills in, the project's `.arko/skills` and Arko's `skills`, are read
//! whether or not they exist yet, and the others when they exist as the
//! session starts. Skills are read again at the start of each turn, and
//! `load_skill` reads them at each call, so a skill saved or edited takes
//! effect at once, and a new one is listed from the next conversation.
//! After those come Arko's built-in skills (see
//! [`builtin_skills`](crate::builtin_skills)), which a skill of the same
//! name in the folders replaces.
//!
//! A skill's `models` lists the models whose agents see it in their skill
//! list. Each agent gets the names its own model goes by ([`model_names`]):
//! its registry ID and alias, `cc:` names in Claude Code, and its
//! `openrouter:` slug, which gets the plain names too when
//! [`OPENROUTER_EQUIVALENTS`](crate::providers::OPENROUTER_EQUIVALENTS)
//! has it.

use std::path::{Path, PathBuf};

use arko_agents::{EmbeddedSkill, InvalidSkillPolicy, SkillSource, SkillsConfig};
use arko_ai::{Model, Provider};

use crate::builtin_skills::BuiltinSkills;
use crate::providers::{self, ALIASES, CLAUDE_CODE, OPENROUTER};

/// The project's skill folders users share, in the working directory.
const PROJECT_FOLDERS: [&str; 3] = [".agents/skills", ".claude/skills", ".skills"];
/// The user's skill folders, in the home directory.
const HOME_FOLDERS: [&str; 2] = [".agents/skills", ".claude/skills"];

/// The folder agents save the project's skills in, `.arko/skills`, whether
/// it exists or not.
pub fn project_folder(workspace: &Path) -> PathBuf {
    crate::arko_dir::dir(workspace).join("skills")
}

/// The folder agents save skills for every project in, `skills` in Arko's
/// folder `root`, whether it exists or not.
pub fn user_folder(root: &Path) -> PathBuf {
    root.join("skills")
}

/// The skill folders, the project's first: a skill's name in more than one
/// is kept from the first, and the others are renamed (`name-2`). The two
/// folders agents save in are there whether they exist or not, and the
/// others when they exist.
pub fn folders(workspace: &Path, root: &Path) -> Vec<PathBuf> {
    folders_for(Some(workspace), root)
}

pub(crate) fn folders_for(workspace: Option<&Path>, root: &Path) -> Vec<PathBuf> {
    folders_in(workspace, root, home().as_deref())
}

/// The user's home directory, whose skill folders Arko reads.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// [`folders`] with the home directory `home`.
pub(crate) fn folders_in(
    workspace: Option<&Path>,
    root: &Path,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    // (Each with whether it is one agents save in.)
    let candidates = workspace
        .into_iter()
        .flat_map(|workspace| {
            PROJECT_FOLDERS
                .iter()
                .map(move |folder| (workspace.join(folder), false))
                .chain(std::iter::once((project_folder(workspace), true)))
        })
        .chain(std::iter::once((user_folder(root), true)))
        .chain(home.into_iter().flat_map(|home| {
            HOME_FOLDERS
                .iter()
                .map(move |folder| (home.join(folder), false))
        }));
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen = Vec::new();
    for (folder, target) in candidates {
        let normal =
            arko_agents::utils::normalize_path(&std::path::absolute(&folder).unwrap_or(folder));
        let real = match std::fs::canonicalize(&normal) {
            Ok(real) if real.is_dir() => real,
            // (One that is there but isn't a folder is no folder of skills.)
            Ok(_) => continue,
            Err(_) if target => normal.clone(),
            Err(_) => continue,
        };
        if !seen.contains(&real) && !found.contains(&normal) {
            seen.push(real);
            found.push(normal);
        }
    }
    found
}

/// The names the agent on `model` goes by, which a skill's `models` are
/// matched against: its registry ID and its aliases, those with `cc:` in
/// front in Claude Code, and on OpenRouter its `openrouter:` slug, with the
/// ID and aliases of the model the slug is in
/// [`OPENROUTER_EQUIVALENTS`](crate::providers::OPENROUTER_EQUIVALENTS) for
/// (also with a routing suffix). Another slug has only its own name.
pub fn model_names(model: &Model) -> Vec<String> {
    let plain = |id: &str| -> Vec<String> {
        std::iter::once(id.to_string())
            .chain(
                ALIASES
                    .iter()
                    .filter(|(_, registry)| *registry == id)
                    .map(|(alias, _)| alias.to_string()),
            )
            .collect()
    };
    match model.provider {
        Provider::ClaudeCode => {
            let names = plain(&model.id);
            let in_claude_code: Vec<String> = names
                .iter()
                .map(|name| format!("{CLAUDE_CODE}{name}"))
                .collect();
            names.into_iter().chain(in_claude_code).collect()
        }
        Provider::OpenRouter => {
            let mut names = providers::openrouter_equivalent(&model.id)
                .map(plain)
                .unwrap_or_default();
            names.push(format!("{OPENROUTER}{}", model.id));
            names
        }
        _ => plain(&model.id),
    }
}

/// The built-in skills `builtin` gives, as the skill system takes them.
fn embedded(builtin: &BuiltinSkills) -> Vec<EmbeddedSkill> {
    builtin
        .given()
        .map(|skill| EmbeddedSkill::new(skill.name, skill.skill_md()))
        .collect()
}

/// The skills of `folders` and the built-in skills `builtin` gives, for no
/// agent in particular (every skill listed); `None` without skill folders or
/// built-in skills. A folder that isn't there has no skills, without a
/// warning, so one made later is read.
pub fn config(folders: &[PathBuf], builtin: &BuiltinSkills) -> Option<SkillsConfig> {
    let embedded = embedded(builtin);
    if folders.is_empty() && embedded.is_empty() {
        return None;
    }
    Some(SkillsConfig {
        global_sources: folders
            .iter()
            .map(|folder| SkillSource {
                path: folder.display().to_string(),
                include: None,
                exclude: None,
                optional: true,
            })
            .collect(),
        embedded,
        on_invalid_skill: InvalidSkillPolicy::Warn,
        ..Default::default()
    })
}

/// [`config`] for an agent on `model` (a model as a session saves it): the
/// main agent's or, with `worker`, a worker's, which leaves out the skills
/// marked `workers: false`. Its skill list holds the skills for its model
/// ([`model_names`]). `load_skill` is offered even without a skill to load,
/// so that the tool list stays the same when one is saved.
pub fn agent_config(
    folders: &[PathBuf],
    builtin: &BuiltinSkills,
    model: &str,
    worker: bool,
) -> Option<SkillsConfig> {
    let mut config = config(folders, builtin)?;
    config.model_names = providers::resolve_model(model)
        .map(|model| model_names(&model))
        .unwrap_or_default();
    config.subagent = worker;
    config.offer_when_empty = true;
    Some(config)
}

/// The skill files and folders in `folders` now, then the built-in skills
/// `builtin` gives, each with what loading makes of it: the name the agents
/// load it under, or why it is skipped. Read as `list` reads them, by the
/// same code.
pub fn scan(
    folders: &[PathBuf],
    builtin: &BuiltinSkills,
    workspace: &Path,
) -> Vec<arko_agents::skill::SkillScan> {
    match config(folders, builtin) {
        None => Vec::new(),
        Some(config) => arko_agents::skill::scan(&config, workspace),
    }
}

/// The skills in `folders` now and the built-in skills `builtin` gives:
/// those loaded on demand, and those always in the instructions.
#[cfg(test)]
pub fn list(
    folders: &[PathBuf],
    builtin: &BuiltinSkills,
    workspace: &Path,
) -> Result<arko_agents::SkillPromptContext, String> {
    match config(folders, builtin) {
        None => Ok(arko_agents::SkillPromptContext::default()),
        Some(config) => arko_agents::SkillManager::load(&config, workspace)
            .map(|manager| manager.prompt_context().clone())
            .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(folder: &Path, name: &str, extra: &str) {
        let dir = folder.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} skill.\n{extra}---\n\nDo {name} things.\n"),
        )
        .unwrap();
    }

    /// No built-in skill.
    fn none() -> BuiltinSkills {
        BuiltinSkills {
            none: true,
            off: Vec::new(),
        }
    }

    #[test]
    fn the_folders_that_exist_are_found_the_projects_first() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        let root = temp.path().join("arko");
        let home = temp.path().join("home");
        // The two folders agents save in are there before they exist.
        let targets = [workspace.join(".arko/skills"), root.join("skills")];
        assert_eq!(folders_in(Some(&workspace), &root, Some(&home)), targets);
        assert!(config(&[], &none()).is_none());
        skill(&workspace.join(".claude/skills"), "review", "");
        skill(&home.join(".claude/skills"), "style", "auto_load: true\n");
        skill(&root.join("skills"), "deploy", "");
        let found = folders_in(Some(&workspace), &root, Some(&home));
        assert_eq!(
            found,
            [
                workspace.join(".claude/skills"),
                workspace.join(".arko/skills"),
                root.join("skills"),
                home.join(".claude/skills")
            ]
        );
        assert_eq!(
            folders_in(None, &root, Some(&home)),
            [root.join("skills"), home.join(".claude/skills")]
        );
        // The home directory as the working directory: each folder once.
        assert_eq!(
            folders_in(Some(&home), &root, Some(&home)),
            [
                home.join(".claude/skills"),
                home.join(".arko/skills"),
                root.join("skills")
            ]
        );

        let skills = list(&found, &none(), &workspace).unwrap();
        let names: Vec<&str> = skills
            .on_demand_skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["review", "deploy"]);
        assert_eq!(skills.auto_loaded_skills[0].name, "style");
        let config = config(&found, &none()).unwrap();
        assert_eq!(config.global_sources.len(), 4);
        // A folder that goes away, or isn't there yet, is no warning.
        assert!(config.global_sources.iter().all(|source| source.optional));
        assert!(config.embedded.is_empty());
        assert_eq!(config.on_invalid_skill, InvalidSkillPolicy::Warn);
        // A skill saved in a folder that wasn't there loads.
        skill(&workspace.join(".arko/skills"), "saved", "");
        let skills = list(&found, &none(), &workspace).unwrap();
        assert!(
            skills
                .on_demand_skills
                .iter()
                .any(|skill| skill.name == "saved")
        );
    }

    fn names(name: &str) -> Vec<String> {
        model_names(&providers::resolve_model(name).unwrap())
    }

    /// A model's names: its ID and alias through the API, those and their
    /// `cc:` forms in Claude Code, and on OpenRouter its slug, with the ID
    /// and alias for a tabled slug, also with a routing suffix.
    #[test]
    fn each_model_goes_by_the_names_of_its_route() {
        assert_eq!(names("haiku"), ["claude-haiku-5-5", "haiku"]);
        assert_eq!(names("gpt-6.1-sol"), ["gpt-6.1-sol", "sol"]);
        assert_eq!(names("claude-opus-4-6"), ["claude-opus-4-6"]);
        assert_eq!(
            names("cc:haiku"),
            [
                "claude-haiku-5-5",
                "haiku",
                "cc:claude-haiku-5-5",
                "cc:haiku"
            ]
        );
        assert_eq!(
            names("openrouter:anthropic/claude-haiku-5.5"),
            [
                "claude-haiku-5-5",
                "haiku",
                "openrouter:anthropic/claude-haiku-5.5"
            ]
        );
        assert_eq!(
            names("openrouter:anthropic/claude-haiku-5.5:floor"),
            [
                "claude-haiku-5-5",
                "haiku",
                "openrouter:anthropic/claude-haiku-5.5:floor"
            ]
        );
        assert_eq!(
            names("openrouter:anthropic/claude-haiku-5.5:batch"),
            ["openrouter:anthropic/claude-haiku-5.5:batch"]
        );
    }

    fn lists(patterns: &[&str], model: &str) -> bool {
        arko_agents::skill::models_match(patterns, &names(model))
    }

    /// For every row of the table, the model's ID, its alias and the full
    /// slug each list a skill for `openrouter:<slug>`. A lookalike slug has
    /// only its own name, so the short names and plain patterns list nothing
    /// for it, and a pattern on the full slug does.
    #[test]
    fn short_names_reach_an_openrouter_model_only_by_the_table() {
        for (id, slug) in providers::OPENROUTER_EQUIVALENTS {
            let model = format!("openrouter:{slug}");
            assert!(lists(&[id], &model), "{id} for {model}");
            assert!(lists(&[&model], &model), "{model}");
            for (alias, _) in ALIASES.iter().filter(|(_, aliased)| aliased == id) {
                assert!(lists(&[alias], &model), "{alias} for {model}");
            }
        }
        for slug in [
            "anthropic/claude-haiku-5.5-v2",
            "anthropic/claude-haiku-5.5:batch",
            "someorg/claude-haiku-5.5",
            "anthropic/claude-haiku-5.5-ft-acme",
        ] {
            let model = format!("openrouter:{slug}");
            assert_eq!(names(&model), [model.clone()]);
            for pattern in ["haiku", "claude-haiku-*", "*haiku*", "claude-haiku-5-5"] {
                assert!(!lists(&[pattern], &model), "{pattern} for {model}");
            }
            if slug.starts_with("anthropic/") {
                assert!(lists(&["openrouter:anthropic/claude-haiku-5.5*"], &model));
            }
        }
        // The routing suffixes name the same model, as for the extra rules.
        for suffix in providers::OPENROUTER_ROUTING_SUFFIXES {
            let model = format!("openrouter:anthropic/claude-haiku-5.5{suffix}");
            assert!(lists(&["haiku"], &model), "{model}");
            assert!(lists(&["claude-haiku-*"], &model), "{model}");
        }
        // The kinds of the table in DESIGN: a family by its pattern, and
        // Claude Code only by a cc: pattern.
        assert!(lists(&["gpt-6*"], "luna"));
        assert!(lists(&["gpt-6*"], "openrouter:openai/gpt-6.1-terra"));
        assert!(lists(
            &["openrouter:openai/gpt-6*"],
            "openrouter:openai/gpt-6-luna-pro"
        ));
        assert!(!lists(&["gpt-6*"], "openrouter:openai/gpt-6-luna-pro"));
        assert!(lists(
            &["claude-haiku-*"],
            "openrouter:anthropic/claude-haiku-4.5"
        ));
        assert!(lists(&["cc:haiku"], "cc:haiku"));
        assert!(!lists(&["cc:haiku"], "haiku"));
        assert!(!lists(
            &["cc:haiku"],
            "openrouter:anthropic/claude-haiku-5.5"
        ));
        assert!(lists(&["haiku", "luna"], "cc:haiku"));
    }

    /// Each agent's configuration has its own model's names, a worker's
    /// leaves out what is marked for the main agent, and both offer
    /// `load_skill` with no skill to load.
    #[test]
    fn each_agent_gets_the_skills_of_its_model() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("skills");
        skill(&folder, "quirks", "models: haiku\n");
        skill(&folder, "lead", "workers: false\n");
        skill(&folder, "common", "");
        let folders = [folder];
        let main = agent_config(&folders, &none(), "opus", false).unwrap();
        assert_eq!(main.model_names, ["claude-opus-5-5", "opus"]);
        assert!(!main.subagent && main.offer_when_empty);
        assert_eq!(main.only, None);
        let worker = agent_config(&folders, &none(), "cc:haiku", true).unwrap();
        assert!(worker.subagent && worker.offer_when_empty);
        let listed = |config: &SkillsConfig| -> Vec<String> {
            arko_agents::SkillManager::load(config, temp.path())
                .unwrap()
                .prompt_context()
                .on_demand_skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect()
        };
        assert_eq!(listed(&main), ["common", "lead"]);
        assert_eq!(listed(&worker), ["common", "quirks"]);
        // With no skill at all, load_skill is still offered.
        let empty = agent_config(&[temp.path().join("none")], &none(), "opus", false).unwrap();
        let manager = arko_agents::SkillManager::load(&empty, temp.path()).unwrap();
        let mut registry = arko_agents::ToolRegistry::new();
        let mut agent = arko_agents::AgentConfig::builder(
            "main".into(),
            "Main".into(),
            arko_agents::AgentLLMConfig {
                model: providers::resolve_model("opus").unwrap(),
                generation_config: Default::default(),
            },
        )
        .role_description("A test.".into())
        .instructions("Test.".into())
        .build()
        .unwrap();
        manager.setup(&mut registry, &mut agent);
        assert!(
            agent
                .tool_config
                .available_tool_names
                .iter()
                .any(|name| name == "load_skill")
        );
    }
}
