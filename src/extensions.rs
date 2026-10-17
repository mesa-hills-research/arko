//! What an agent session would get of MCP servers and skills, read as a
//! session reads them, without starting anything: for a front end that
//! lists them. (A chat gets neither.)

use std::path::{Path, PathBuf};

use crate::{mcp, skills};

/// An MCP server a session would start, or would leave out and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    /// Its name ("server 3" for an entry without one).
    pub name: String,
    /// Its command line as the file writes it (`${VAR}`s as written).
    /// Empty for an entry that is left out before it has one.
    pub command: String,
    /// The file that sets it up.
    pub file: PathBuf,
    /// Why a session doesn't start it, when it doesn't: its entry (no name,
    /// not a server Arko starts, a `${VAR}` that isn't set), disabled, a name
    /// an earlier file sets up, or a project's servers that wait for the
    /// user's answer or that the user chose not to start.
    pub skipped: Option<String>,
}

/// A skill the agents get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// Its name, as the agents load it (a name an earlier folder has is
    /// renamed: `name-2`).
    pub name: String,
    /// Its description (empty for one always in the instructions).
    pub description: String,
    /// It is always in the agents' instructions, not loaded on demand.
    pub auto_load: bool,
}

/// What an agent session would get.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    /// The MCP servers, in the order a session reads their files: the
    /// files given, the project's `.mcp.json`, Arko's `mcp.json`.
    pub servers: Vec<McpServerEntry>,
    /// The skills, those always in the instructions first.
    pub skills: Vec<SkillEntry>,
    /// What can't be read: a file, a skill folder.
    pub notes: Vec<String>,
}

/// What an agent session in Arko's folder `root`, working in `workspace`,
/// with the MCP config files `mcp_config` (an app's, as
/// [`Options::mcp_config`](crate::Options::mcp_config) gives them), would
/// get. Reads the files: call it off a UI thread.
pub fn list(root: &Path, workspace: &Path, mcp_config: &[PathBuf]) -> Extensions {
    let mut extensions = Extensions::default();
    let project = workspace.join(mcp::PROJECT_FILE);
    let user = root.join(mcp::USER_FILE);
    let normal = |file: &Path| {
        arko_agents::utils::normalize_path(
            &std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf()),
        )
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for file in mcp_config.iter().map(|file| normal(file)) {
        if !files.contains(&file) {
            files.push(file);
        }
    }
    let given = files.len();
    for file in [project.clone(), user] {
        if file.is_file() && !files.contains(&normal(&file)) {
            files.push(file);
        }
    }
    for (index, file) in files.iter().enumerate() {
        let parsed = match mcp::parse_file(file, &mcp::environment) {
            Ok(parsed) => parsed,
            Err(error) => {
                extensions
                    .notes
                    .push(format!("MCP servers not started: {error}"));
                continue;
            }
        };
        // A project's servers start once the user agrees.
        let held = (index >= given && *file == project)
            .then(|| match mcp::remembered(root, workspace, &parsed) {
                Some(true) => None,
                Some(false) => Some("the user chose not to start the project's servers".into()),
                None => {
                    Some("a session asks the user before it starts the project's servers".into())
                }
            })
            .flatten();
        for server in &parsed.servers {
            let taken = extensions
                .servers
                .iter()
                .find(|other| other.name == server.name && other.skipped.is_none());
            let skipped = match taken {
                Some(other) => Some(format!(
                    "{} sets up a server of that name first",
                    other.file.display()
                )),
                None => held.clone(),
            };
            extensions.servers.push(McpServerEntry {
                name: server.name.clone(),
                command: server.shown.clone(),
                file: file.clone(),
                skipped,
            });
        }
        for (name, why) in &parsed.left_out {
            extensions.servers.push(McpServerEntry {
                name: name.clone(),
                command: String::new(),
                file: file.clone(),
                skipped: Some(why.clone()),
            });
        }
    }
    let folders = skills::folders(workspace, root);
    match skills::list(&folders, workspace) {
        Ok(found) => {
            extensions
                .skills
                .extend(found.auto_loaded_skills.iter().map(|skill| SkillEntry {
                    name: skill.name.clone(),
                    description: String::new(),
                    auto_load: true,
                }));
            extensions
                .skills
                .extend(found.on_demand_skills.iter().map(|skill| SkillEntry {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    auto_load: false,
                }));
        }
        Err(error) => extensions.notes.push(format!("skills not read: {error}")),
    }
    extensions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The servers a session would start, and those it leaves out with
    /// why; the skills the agents get, and not the files that aren't
    /// skills.
    #[test]
    fn what_a_session_would_get_is_listed_with_what_it_leaves_out() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let given = temp.path().join("servers.json");
        std::fs::write(
            &given,
            r#"{"mcpServers": [
                {"name": "gh", "command": "npx", "args": ["github"]},
                {"command": "unnamed"},
                "notobj",
                {"name": "env", "command": "x", "env": {"TOKEN": "${ARKO_SURELY_UNSET_VARIABLE}"}},
                {"name": "off", "command": "y", "disabled": true}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(mcp::PROJECT_FILE),
            r#"{"mcpServers": {"gh": {"command": "other"}, "lint": {"command": "lint"}, "bad": "text"}}"#,
        )
        .unwrap();
        let missing = temp.path().join("gone.json");
        let skills = workspace.join(".agents/skills");
        for (folder, text) in [
            ("ok", "---\nname: ok\ndescription: A skill.\n---\nDo the ok thing.\n"),
            (".hidden", "---\nname: .hidden\ndescription: d\n---\nx\n"),
            ("notes", "No front matter.\n"),
        ] {
            std::fs::create_dir_all(skills.join(folder)).unwrap();
            std::fs::write(skills.join(folder).join("SKILL.md"), text).unwrap();
        }
        std::fs::write(
            skills.join("flat.md"),
            "---\nname: flat\ndescription: A flat skill.\n---\nDo the flat thing.\n",
        )
        .unwrap();

        let listed = list(&root, &workspace, &[given.clone(), missing]);
        let servers: Vec<(&str, Option<&str>)> = listed
            .servers
            .iter()
            .map(|server| (server.name.as_str(), server.skipped.as_deref()))
            .collect();
        assert_eq!(servers[0], ("gh", None));
        assert_eq!(listed.servers[0].command, "npx github");
        let skipped = |name: &str| {
            listed
                .servers
                .iter()
                .find(|server| server.name == name)
                .and_then(|server| server.skipped.clone())
                .unwrap_or_default()
        };
        assert!(skipped("server 2").contains("no name"), "{servers:?}");
        assert!(skipped("server 3").contains("no name"));
        assert!(skipped("bad").contains("isn't an object"));
        assert!(
            skipped("env").contains("ARKO_SURELY_UNSET_VARIABLE"),
            "{}",
            skipped("env")
        );
        assert!(skipped("off").contains("disabled"));
        assert!(skipped("lint").contains("asks the user"));
        let project_gh = listed
            .servers
            .iter()
            .find(|server| server.name == "gh" && server.command == "other")
            .unwrap();
        assert!(
            project_gh
                .skipped
                .as_deref()
                .unwrap()
                .contains("servers.json")
        );
        assert!(listed.notes.iter().any(|note| note.contains("gone.json")));

        // (The user's own skill folders may add theirs.)
        let names: Vec<&str> = listed
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert!(
            names.contains(&"flat") && names.contains(&"ok"),
            "{names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|name| *name == ".hidden" || *name == "notes")
        );
        let ok = listed
            .skills
            .iter()
            .find(|skill| skill.name == "ok")
            .unwrap();
        assert_eq!((ok.description.as_str(), ok.auto_load), ("A skill.", false));
    }
}
