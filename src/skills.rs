//! Skills: instructions for particular kinds of work, each a folder with a
//! `SKILL.md` (a name and description in YAML front matter, then the
//! instructions) and the files it uses. The agents see each skill's name
//! and description, and load a skill with arko_agents' `load_skill` when a
//! task calls for it. One marked `auto_load: true` is always in their
//! instructions.
//!
//! Arko reads skills from the project's `.agents/skills`, `.claude/skills`
//! and `.skills`, then from `skills` in Arko's folder, `~/.agents/skills`
//! and `~/.claude/skills`, the folders of these that exist when a session
//! starts. Skills are read again at the start of each turn, so an edited
//! skill takes effect at once, and a new one is listed from the next
//! conversation. After those come Arko's built-in skills (see
//! [`builtin_skills`](crate::builtin_skills)), which a skill of the same name
//! in the folders replaces.

use std::path::{Path, PathBuf};

use arko_agents::{EmbeddedSkill, InvalidSkillPolicy, SkillSource, SkillsConfig};

use crate::builtin_skills::BuiltinSkills;

/// The project's skill folders, in the working directory.
const PROJECT_FOLDERS: [&str; 3] = [".agents/skills", ".claude/skills", ".skills"];
/// The user's skill folders, in the home directory.
const HOME_FOLDERS: [&str; 2] = [".agents/skills", ".claude/skills"];

/// The skill folders that exist, the project's first: a skill's name in
/// more than one is kept from the first, and the others are renamed
/// (`name-2`).
pub fn folders(workspace: &Path, root: &Path) -> Vec<PathBuf> {
    folders_for(Some(workspace), root)
}

pub(crate) fn folders_for(workspace: Option<&Path>, root: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    folders_in(workspace, root, home.as_deref())
}

fn folders_in(workspace: Option<&Path>, root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let candidates = workspace
        .into_iter()
        .flat_map(|workspace| {
            PROJECT_FOLDERS
                .iter()
                .map(move |folder| workspace.join(folder))
        })
        .chain(std::iter::once(root.join("skills")))
        .chain(
            home.into_iter()
                .flat_map(|home| HOME_FOLDERS.iter().map(move |folder| home.join(folder))),
        );
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen = Vec::new();
    for folder in candidates {
        let Ok(real) = std::fs::canonicalize(&folder) else {
            continue;
        };
        if real.is_dir() && !seen.contains(&real) {
            seen.push(real);
            found.push(arko_agents::utils::normalize_path(
                &std::path::absolute(&folder).unwrap_or(folder),
            ));
        }
    }
    found
}

/// The built-in skills `builtin` gives, as the skill system takes them.
fn embedded(builtin: &BuiltinSkills) -> Vec<EmbeddedSkill> {
    builtin
        .given()
        .map(|skill| EmbeddedSkill::new(skill.name, skill.skill_md()))
        .collect()
}

/// The agents' skills configuration; `None` without skill folders or
/// built-in skills.
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
                optional: false,
            })
            .collect(),
        embedded,
        on_invalid_skill: InvalidSkillPolicy::Warn,
        ..Default::default()
    })
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
        assert!(folders_in(Some(&workspace), &root, Some(&home)).is_empty());
        assert!(config(&[], &none()).is_none());
        skill(&workspace.join(".claude/skills"), "review", "");
        skill(&home.join(".claude/skills"), "style", "auto_load: true\n");
        skill(&root.join("skills"), "deploy", "");
        let found = folders_in(Some(&workspace), &root, Some(&home));
        assert_eq!(
            found,
            [
                workspace.join(".claude/skills"),
                root.join("skills"),
                home.join(".claude/skills")
            ]
        );
        assert_eq!(
            folders_in(None, &root, Some(&home)),
            [root.join("skills"), home.join(".claude/skills")]
        );
        // The home directory as the working directory: each folder once.
        assert_eq!(folders_in(Some(&home), &root, Some(&home)).len(), 2);

        let skills = list(&found, &none(), &workspace).unwrap();
        let names: Vec<&str> = skills
            .on_demand_skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["review", "deploy"]);
        assert_eq!(skills.auto_loaded_skills[0].name, "style");
        let config = config(&found, &none()).unwrap();
        assert_eq!(config.global_sources.len(), 3);
        assert!(config.embedded.is_empty());
        assert_eq!(config.on_invalid_skill, InvalidSkillPolicy::Warn);
    }
}
