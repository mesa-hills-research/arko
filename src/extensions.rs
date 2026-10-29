//! What an agent session would get of MCP servers and skills, read as a
//! session reads them, without starting anything, for a front end that
//! lists them. (A chat gets neither.)

use std::path::{Path, PathBuf};

use crate::builtin_skills::BuiltinSkills;
use crate::{mcp, skills};

/// An MCP server a session would start, or would leave out and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerEntry {
    /// Its name ("server 3" for an entry without one).
    pub name: String,
    /// Its command line as the file writes it (`${VAR}`s as written),
    /// empty for an entry without one.
    pub command: String,
    /// The file that sets it up.
    pub file: PathBuf,
    /// Why a session doesn't start it, if it doesn't. That may be its entry
    /// (no name, not a server Arko starts, a `${VAR}` that isn't set), its
    /// being disabled, a name an earlier file sets up, or a project's
    /// servers that wait for the user's answer or that the user chose not
    /// to start.
    pub skipped: Option<String>,
    /// When, in which session and why an agent added it, if one did. Only
    /// a server an agent added can be removed with [`remove_added`].
    pub added: Option<crate::agent_added::ServerRecord>,
    /// Workers may use it (Arko's `workers` field, true unless the entry
    /// says false).
    pub workers: bool,
    /// Sessions open at the same time share one copy of it (Arko's `shared`
    /// field).
    pub shared: bool,
}

/// A skill the agents get, or a skill file or folder they don't get and
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// Where it is, a skill's folder or its file (a `.md`, or a `.skill`
    /// bundle). Empty for a built-in skill, which is held in the program.
    pub path: PathBuf,
    /// Its name as the agents load it (a name an earlier skill has is
    /// renamed `name-2`). One that can't be read goes by its folder's or
    /// file's name.
    pub name: String,
    /// Its description (empty for one that can't be read).
    pub description: String,
    /// It is always in the agents' instructions, not loaded on demand.
    pub auto_load: bool,
    /// Why the agents don't get it, if they don't. That may be why it can't
    /// be read (such as its front matter), a skill with the same content
    /// before it, or, for a built-in skill, a skill of the folders with its
    /// name.
    pub skipped: Option<String>,
    /// It is one of Arko's built-in skills.
    pub builtin: bool,
}

/// A config file that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionNote {
    /// The file the note is about.
    pub file: PathBuf,
    /// What happened, suitable for display.
    pub message: String,
}

/// What an agent session would get.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    /// The MCP servers, in the order a session reads their files (the files
    /// given, the project's `.arko/mcp.json` and `.mcp.json`, Arko's
    /// `mcp.json`).
    pub servers: Vec<McpServerEntry>,
    /// The skills, in the order the agents get them. The skill folders come
    /// in the order `arko --help` gives them, and each folder's skills in
    /// the order of their names.
    pub skills: Vec<SkillEntry>,
    /// The MCP config files that can't be read, and why.
    pub notes: Vec<ExtensionNote>,
}

/// What an agent session in Arko's folder `root`, working in `workspace`,
/// with the MCP config files `mcp_config` (an app's, as
/// [`Options::mcp_config`](crate::Options::mcp_config) gives them), would
/// get. With no workspace it lists the given MCP files, Arko's `mcp.json`,
/// and the shared skill folders only. The built-in skills come last, those
/// that `ARKO_BUILTIN_SKILLS` and `ARKO_BUILTIN_SKILLS_OFF` leave on. It
/// reads the files, so call it off a UI thread.
pub fn list(root: &Path, workspace: Option<&Path>, mcp_config: &[PathBuf]) -> Extensions {
    list_with(root, workspace, mcp_config, None, None)
}

/// [`list`] with the app's choice of built-in skills, as
/// [`Options::builtin_skills`](crate::Options::builtin_skills) and
/// [`Options::builtin_skills_off`](crate::Options::builtin_skills_off) give
/// it, where `None` leaves it to the environment. A name that isn't a
/// built-in skill is skipped, as a session skips it. If
/// `ARKO_BUILTIN_SKILLS` holds a value a session wouldn't open with, every
/// built-in skill is listed as skipped, with the reason.
pub fn list_with(
    root: &Path,
    workspace: Option<&Path>,
    mcp_config: &[PathBuf],
    builtin_skills: Option<bool>,
    builtin_skills_off: Option<&[String]>,
) -> Extensions {
    let mut extensions = Extensions::default();
    let normal = |file: &Path| {
        arko_agents::utils::normalize_path(
            &std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf()),
        )
    };
    let workspace = workspace.map(normal);
    let workspace = workspace.as_deref();
    let local = workspace.map(mcp::local_file);
    let project = workspace.map(|workspace| workspace.join(mcp::PROJECT_FILE));
    let user = normal(&root.join(mcp::USER_FILE));
    let added = crate::agent_added::servers(root);
    let mut files: Vec<PathBuf> = Vec::new();
    for file in mcp_config.iter().map(|file| normal(file)) {
        if !files.contains(&file) {
            files.push(file);
        }
    }
    let given = files.len();
    for file in local
        .iter()
        .chain(project.iter())
        .cloned()
        .chain(std::iter::once(user))
    {
        if file.is_file() && !files.contains(&normal(&file)) {
            files.push(file);
        }
    }
    for (index, file) in files.iter().enumerate() {
        let parsed = match mcp::parse_file(file, &mcp::environment) {
            Ok(parsed) => parsed,
            Err(error) => {
                extensions.notes.push(ExtensionNote {
                    file: file.clone(),
                    message: format!("MCP servers not started. {}", crate::ui::sentence(&error)),
                });
                continue;
            }
        };
        // A project's servers start once the user agrees.
        let answer = workspace.filter(|_| index >= given).and_then(|workspace| {
            if Some(file) == project.as_ref() {
                Some(mcp::remembered(root, workspace, &parsed))
            } else if Some(file) == local.as_ref() {
                Some(mcp::remembered_local(root, workspace, &parsed))
            } else {
                None
            }
        });
        let held = match answer {
            None | Some(Some(true)) => None,
            Some(Some(false)) => Some("the user chose not to start the project's servers".into()),
            Some(None) => {
                Some("a session asks the user before it starts the project's servers".into())
            }
        };
        let added_here = |name: &str| {
            added
                .iter()
                .find(|record| record.name == name && record.file == *file)
                .cloned()
        };
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
                added: added_here(&server.name),
                workers: server.workers,
                shared: server.shared,
            });
        }
        for (name, command, why) in &parsed.left_out {
            extensions.servers.push(McpServerEntry {
                name: name.clone(),
                command: command.clone(),
                file: file.clone(),
                skipped: Some(why.clone()),
                added: added_here(name),
                workers: true,
                shared: false,
            });
        }
    }
    let folders = skills::folders_for(workspace, root);
    let (builtin, refused) = builtin_choice(builtin_skills, builtin_skills_off);
    extensions.skills = skills::scan(&folders, &builtin, workspace.unwrap_or(root))
        .into_iter()
        .map(|skill| SkillEntry {
            path: skill.path,
            name: skill.name,
            description: skill.description,
            auto_load: skill.auto_load,
            skipped: skill
                .skipped
                .or_else(|| refused.clone().filter(|_| skill.embedded)),
            builtin: skill.embedded,
        })
        .collect();
    extensions
}

/// The built-in skills a new session would give, as the app's choice says,
/// else as `ARKO_BUILTIN_SKILLS` and `ARKO_BUILTIN_SKILLS_OFF` say. With a
/// setting a session wouldn't open with, all of them and why.
fn builtin_choice(
    builtin_skills: Option<bool>,
    builtin_skills_off: Option<&[String]>,
) -> (BuiltinSkills, Option<String>) {
    let options = crate::Options {
        builtin_skills,
        builtin_skills_off: builtin_skills_off.map(<[String]>::to_vec),
        ..crate::Options::default()
    };
    match crate::start::builtin_skills_for(&options) {
        Ok((builtin, _)) => (builtin, None),
        Err(error) => (BuiltinSkills::default(), Some(error)),
    }
}

/// The answer remembered for the project's current `.mcp.json` servers.
/// `Some(true)` approves them, `Some(false)` declines them, and `None`
/// means no answer is remembered for this config (also when it is absent).
/// Changes to `mcpServers` require a new answer. Unreadable files return an
/// error. Call this off a UI thread.
pub fn project_mcp_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String> {
    mcp::project_approval(root, workspace)
}

/// Forgets the project's remembered answer, including an answer for an
/// earlier config. Returns whether an answer was removed. Other projects
/// keep their answers. The next session asks again before starting these
/// servers, and servers in sessions already open keep running.
/// Call this off a UI thread.
pub fn forget_project_mcp_approval(root: &Path, workspace: &Path) -> Result<bool, String> {
    mcp::forget_project_approval(root, workspace)
}

/// The answer remembered for the project's current `.arko/mcp.json`
/// servers, where agents add the project's servers, as
/// [`project_mcp_approval`] gives it for `.mcp.json`. A server an agent adds
/// there is remembered as approved.
pub fn local_mcp_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String> {
    mcp::local_approval(root, workspace)
}

/// Forgets the answer remembered for the project's `.arko/mcp.json`, as
/// [`forget_project_mcp_approval`] does for `.mcp.json`.
pub fn forget_local_mcp_approval(root: &Path, workspace: &Path) -> Result<bool, String> {
    mcp::forget_local_approval(root, workspace)
}

/// Something an agent added, which [`remove_added`] removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    /// A skill an agent saved, by its folder.
    Skill { path: PathBuf },
    /// An MCP server an agent added, by the file its entry is in and its
    /// name.
    Server { file: PathBuf, name: String },
}

/// Removes what an agent added, without a session: a skill's folder, or a
/// server's entry in its file, and its line in the record of what agents
/// added. Something an agent didn't add is refused. Sessions open now keep a
/// removed server running until they close. The text says what happened,
/// or why it didn't. Call it off a UI thread.
pub fn remove_added(root: &Path, added: Added) -> Result<String, String> {
    match added {
        Added::Server { file, name } => match mcp::changes::take_out(root, &file, &name) {
            Ok(()) => Ok(format!("Removed the MCP server {name}.")),
            Err(mcp::changes::NotRemoved::NotAdded(file)) => Err(format!(
                "{name} wasn't added by an agent. Edit {} to remove it.",
                file.display()
            )),
            Err(mcp::changes::NotRemoved::Failed(why)) => Err(why),
        },
        Added::Skill { path } => remove_saved_skill(root, &path),
    }
}

/// Deletes the folder of a skill an agent saved, and drops its record.
fn remove_saved_skill(root: &Path, path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if crate::agent_added::skill_at(root, path).is_none() {
        return Err(format!("{name} wasn't added by an agent."));
    }
    std::fs::remove_dir_all(path).map_err(|error| {
        format!(
            "Couldn't delete {} ({}).",
            path.display(),
            crate::profiles::reason_of(&error)
        )
    })?;
    crate::agent_added::forget_skill(root, path)?;
    Ok(format!("Removed the skill {name}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_extensions_can_be_listed_without_a_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let servers = r#"{"mcpServers":{"shared":{"command":"shared-server"}}}"#;
        std::fs::write(root.join(mcp::USER_FILE), servers).unwrap();
        std::fs::write(root.join(mcp::PROJECT_FILE), servers).unwrap();
        for folder in ["skills", ".agents/skills"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(
                root.join(folder).join("local.md"),
                "---\nname: local\ndescription: A local skill.\n---\nInstructions.\n",
            )
            .unwrap();
        }
        let given = root.join("given.json");
        std::fs::write(
            &given,
            r#"{"mcpServers":{"given":{"command":"given-server"}}}"#,
        )
        .unwrap();
        let listed = list(root, None, &[given]);
        assert_eq!(listed.servers.len(), 2);
        assert_eq!(listed.servers[0].name, "given");
        assert_eq!(listed.servers[1].file, root.join(mcp::USER_FILE));
        assert!(
            listed
                .skills
                .iter()
                .any(|skill| skill.path == root.join("skills/local.md"))
        );
        assert!(
            !listed
                .skills
                .iter()
                .any(|skill| skill.path.starts_with(root.join(".agents")))
        );
        assert!(listed.notes.is_empty());
    }

    /// The app's choice decides which built-in skills are listed, and a name
    /// no built-in skill has is skipped, as a session skips it.
    #[test]
    fn the_apps_choice_of_built_in_skills_is_listed() {
        let temp = tempfile::tempdir().unwrap();
        let builtin = |on: Option<bool>, off: &[&str]| -> Vec<String> {
            let off: Vec<String> = off.iter().map(|name| name.to_string()).collect();
            list_with(temp.path(), None, &[], on, Some(&off))
                .skills
                .into_iter()
                .filter(|skill| skill.builtin)
                .map(|skill| skill.name)
                .collect()
        };
        assert_eq!(
            builtin(Some(true), &[]),
            ["user-facing-prose", "repo-docs-style", "ponytail"]
        );
        assert_eq!(
            builtin(Some(true), &["repo-docs-style", "repo-doc-style"]),
            ["user-facing-prose", "ponytail"]
        );
        assert!(builtin(Some(false), &[]).is_empty());
    }

    /// A setting a session wouldn't open with doesn't list the built-in
    /// skills as given.
    #[test]
    fn a_setting_a_session_would_refuse_lists_every_built_in_skill_as_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let (root, workspace) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".env"), "ARKO_BUILTIN_SKILLS=maybe\n").unwrap();
        let env = crate::credentials::Env::for_folder_in(Some(&workspace), false, Some(&root));
        let (builtin, why) = crate::context::with_env(env, || builtin_choice(None, None));
        assert!(builtin.is_all());
        let why = why.expect("a reason");
        assert!(
            why.contains("ARKO_BUILTIN_SKILLS") && why.contains("`maybe`"),
            "{why}"
        );
        // The app's own choice doesn't read it.
        let env = crate::credentials::Env::for_folder_in(Some(&workspace), false, Some(&root));
        let (builtin, why) =
            crate::context::with_env(env, || builtin_choice(Some(false), Some(&[])));
        assert!(builtin.none && why.is_none());
    }

    #[test]
    fn project_approvals_can_be_read_and_forgotten_without_starting_servers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("project");
        let other = temp.path().join("other");
        for project in [&workspace, &other] {
            std::fs::create_dir_all(project).unwrap();
            std::fs::write(
                project.join(mcp::PROJECT_FILE),
                r#"{"mcpServers":{"lint":{"command":"lint-server"}}}"#,
            )
            .unwrap();
        }
        assert_eq!(project_mcp_approval(&root, &workspace), Ok(None));
        assert_eq!(forget_project_mcp_approval(&root, &workspace), Ok(false));
        for answer in [true, false] {
            mcp::plan(&root, &workspace, &[], false)
                .unwrap()
                .decide(&[Some(answer)])
                .unwrap();
            mcp::plan(&root, &other, &[], false)
                .unwrap()
                .decide(&[Some(true)])
                .unwrap();
            assert_eq!(project_mcp_approval(&root, &workspace), Ok(Some(answer)));
            assert_eq!(
                project_mcp_approval(&root, &workspace.join(".")),
                Ok(Some(answer))
            );
            let listed = list(&root, Some(&workspace), &[]);
            assert_eq!(listed.servers[0].skipped.is_none(), answer);
            assert_eq!(forget_project_mcp_approval(&root, &workspace), Ok(true));
            assert_eq!(project_mcp_approval(&root, &workspace), Ok(None));
            assert_eq!(project_mcp_approval(&root, &other), Ok(Some(true)));
            assert!(
                mcp::plan(&root, &workspace, &[], false)
                    .unwrap()
                    .approvals
                    .len()
                    == 1
            );
        }
        mcp::plan(&root, &workspace, &[], false)
            .unwrap()
            .decide(&[Some(true)])
            .unwrap();
        std::fs::write(
            workspace.join(mcp::PROJECT_FILE),
            r#"{"mcpServers":{"changed":{"command":"another-server"}}}"#,
        )
        .unwrap();
        assert_eq!(project_mcp_approval(&root, &workspace), Ok(None));
        assert_eq!(forget_project_mcp_approval(&root, &workspace), Ok(true));
        std::fs::write(workspace.join(mcp::PROJECT_FILE), "broken config").unwrap();
        assert!(project_mcp_approval(&root, &workspace).is_err());
        std::fs::remove_file(workspace.join(mcp::PROJECT_FILE)).unwrap();
        assert_eq!(project_mcp_approval(&root, &workspace), Ok(None));
        assert_eq!(forget_project_mcp_approval(&root, &workspace), Ok(false));
        std::fs::write(root.join("mcp-approvals.json"), "broken approvals").unwrap();
        assert!(project_mcp_approval(&root, &other).is_err());
        assert!(forget_project_mcp_approval(&root, &other).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("mcp-approvals.json")).unwrap(),
            "broken approvals"
        );
    }

    #[test]
    fn concurrent_approval_updates_keep_other_projects_answers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let projects: Vec<_> = (0..12)
            .map(|index| temp.path().join(format!("p{index}")))
            .collect();
        for (index, project) in projects.iter().enumerate() {
            std::fs::create_dir_all(project).unwrap();
            std::fs::write(
                project.join(mcp::PROJECT_FILE),
                r#"{"mcpServers":{"lint":{"command":"lint-server"}}}"#,
            )
            .unwrap();
            if index % 2 == 0 {
                mcp::plan(&root, project, &[], false)
                    .unwrap()
                    .decide(&[Some(true)])
                    .unwrap();
            }
        }
        let barrier = std::sync::Barrier::new(projects.len());
        std::thread::scope(|scope| {
            for (index, project) in projects.iter().enumerate() {
                let root = &root;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    if index % 2 == 0 {
                        assert_eq!(forget_project_mcp_approval(root, project), Ok(true));
                    } else {
                        mcp::plan(root, project, &[], false)
                            .unwrap()
                            .decide(&[Some(false)])
                            .unwrap();
                    }
                });
            }
        });
        for (index, project) in projects.iter().enumerate() {
            assert_eq!(
                project_mcp_approval(&root, project),
                Ok((index % 2 != 0).then_some(false))
            );
        }
    }

    /// The servers a session would start, and those it leaves out with
    /// why. The skills, with where each is, and those the agents don't
    /// get with why, but not the files that aren't skills.
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
            (
                "ok",
                "---\nname: ok\ndescription: A skill.\n---\nDo the ok thing.\n",
            ),
            (".hidden", "---\nname: .hidden\ndescription: d\n---\nx\n"),
            ("notes", "No front matter.\n"),
            (
                "unread",
                "---\nname: unread\ndescription: d\n---\nIts reference isn't text.\n",
            ),
        ] {
            std::fs::create_dir_all(skills.join(folder)).unwrap();
            std::fs::write(skills.join(folder).join("SKILL.md"), text).unwrap();
        }
        // A resource that can't be read (not UTF-8).
        std::fs::write(skills.join("unread/ref.md"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(
            skills.join("flat.md"),
            "---\nname: flat\ndescription: A flat skill.\n---\nDo the flat thing.\n",
        )
        .unwrap();

        let listed = list(&root, Some(&workspace), &[given.clone(), missing.clone()]);
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
        // A server left out keeps its command line as written.
        let command = |name: &str| {
            listed
                .servers
                .iter()
                .find(|server| server.name == name)
                .map(|server| server.command.clone())
                .unwrap_or_default()
        };
        assert_eq!((command("env"), command("off")), ("x".into(), "y".into()));
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
        assert!(
            listed
                .notes
                .iter()
                .any(|note| note.file == missing && note.message.contains("gone.json"))
        );

        // (The user's own skill folders may add theirs.)
        let ours: Vec<(&str, Option<&str>)> = listed
            .skills
            .iter()
            .filter(|skill| skill.path.starts_with(&skills))
            .map(|skill| (skill.name.as_str(), skill.skipped.as_deref()))
            .collect();
        assert_eq!(ours.len(), 4, "{ours:?}");
        assert_eq!(ours[0], ("flat", None));
        assert_eq!(ours[1].0, "notes");
        assert!(
            ours[1].1.is_some_and(|why| why.contains("frontmatter")),
            "{ours:?}"
        );
        assert_eq!(ours[2], ("ok", None));
        assert_eq!(ours[3].0, "unread");
        assert!(ours[3].1.is_some(), "{ours:?}");
        // As a session loads them: the one that can't be read is skipped.
        let loaded = skills::list(
            &skills::folders(&workspace, &root),
            &Default::default(),
            &workspace,
        )
        .unwrap();
        assert!(
            loaded
                .on_demand_skills
                .iter()
                .any(|skill| skill.name == "ok")
        );
        assert!(
            !loaded
                .on_demand_skills
                .iter()
                .any(|skill| skill.name == "unread")
        );
        let ok = listed
            .skills
            .iter()
            .find(|skill| skill.name == "ok")
            .unwrap();
        assert_eq!((ok.description.as_str(), ok.auto_load), ("A skill.", false));
        assert_eq!(ok.path, skills.join("ok"));
        assert_eq!(listed.skills[0].path, skills.join("flat.md"));
        // Only the MCP file that can't be read is a note.
        assert_eq!(listed.notes.len(), 1, "{:?}", listed.notes);
    }
}
