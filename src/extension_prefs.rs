//! What the user chose about the skills and MCP servers agents add, in
//! `extensions.json` in Arko's folder. Whether agents ask before they add an
//! MCP server and whether they ask before they save a skill in the project
//! are read each time an agent adds one, so a change reaches every session at
//! its next add, with no restart. The file is written under a lock, as
//! `mcp-approvals.json` is, so two sessions changing different switches keep
//! both.
//!
//! Where agents' project files stay out of git is the
//! [`profiles::Preferences::keep_out_of_git`](crate::profiles::Preferences)
//! setting, and [`Switches`] reads the three together for the settings.
//! `/extensions` says and changes them, for the line prompt and the web page.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Harness;
use crate::remote::Source;

/// The file in Arko's folder that holds the [`ExtensionPrefs`].
const FILE: &str = "extensions.json";

/// "Ask before adding MCP servers" and what it covers.
pub const ASK_MCP_NAME: &str = "Ask before adding MCP servers";
pub const ASK_MCP_ABOUT: &str =
    "Agents ask before they add a server, for this project or every project.";
/// What turning it off warns of, before it asks to be sure.
pub const ASK_MCP_OFF: &str = "Agents will add MCP servers without asking. A server runs a program the agent chose, with your permissions and outside the command guard.";
/// The buttons of that question.
pub const TURN_OFF: &str = "Turn off";
pub const CANCEL: &str = "Cancel";
/// The line `/mcp` ends with while the switch is off, and what the command
/// adds when it turns it off.
pub const ASK_MCP_OFF_LINE: &str = "Agents add MCP servers without asking.";

pub const REVIEW_SKILLS_NAME: &str = "Review project skills";
pub const REVIEW_SKILLS_ABOUT: &str = "Agents ask before they save a skill in the project too. Skills for every project and downloaded skills always ask. A skill folder an agent writes with its file tools or the shell loads as before, without asking.";

pub const KEEP_OUT_OF_GIT_NAME: &str = "Keep agents' project files out of git";
pub const KEEP_OUT_OF_GIT_ABOUT: &str = "Skills, MCP servers and worker profiles that agents save in the project's .arko folder stay out of git. Off removes Arko's .gitignore from that folder.";

/// The user's choices about what agents add, in `extensions.json`. Without
/// the file, or when it can't be read, the defaults apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPrefs {
    /// An agent asks before it adds an MCP server, for the project or for
    /// every project. On by default. Off still records an approved add, so
    /// the next session starts the server without a question.
    pub ask_before_adding_mcp: bool,
    /// An agent also asks before it saves a skill in the project. Off by
    /// default. A skill for every project and a downloaded skill always ask.
    pub review_project_skills: bool,
}

impl Default for ExtensionPrefs {
    fn default() -> Self {
        Self {
            ask_before_adding_mcp: true,
            review_project_skills: false,
        }
    }
}

/// The preferences in Arko's folder `root`.
pub fn load(root: &Path) -> ExtensionPrefs {
    std::fs::read_to_string(root.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Saves `prefs` in Arko's folder `root`, whole.
pub fn save(root: &Path, prefs: &ExtensionPrefs) -> Result<(), String> {
    update(root, |now| *now = *prefs).map(|_| ())
}

/// Reads the preferences, applies `change` and saves them, all under the
/// lock, so a switch another session changes meanwhile isn't lost. Returns
/// the preferences saved.
pub fn update(
    root: &Path,
    change: impl FnOnce(&mut ExtensionPrefs),
) -> Result<ExtensionPrefs, String> {
    let _lock = lock(root)?;
    let mut prefs = load(root);
    change(&mut prefs);
    let file = root.join(FILE);
    let text = serde_json::to_string_pretty(&prefs).map_err(|error| error.to_string())?;
    crate::session::write_whole(&file, (text + "\n").as_bytes())
        .map_err(|error| format!("Couldn't write {} ({error})", file.display()))?;
    Ok(prefs)
}

fn lock(root: &Path) -> Result<std::fs::File, String> {
    let path = root.join(format!("{FILE}.lock"));
    let file = std::fs::create_dir_all(root)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
        })
        .map_err(|error| format!("Couldn't open {} ({error})", path.display()))?;
    file.lock()
        .map_err(|error| format!("Couldn't lock {} ({error})", path.display()))?;
    Ok(file)
}

/// The three switches the settings show for what agents add, as the
/// session last read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Switches {
    pub ask_before_adding_mcp: bool,
    pub review_project_skills: bool,
    pub keep_out_of_git: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Self::of(
            ExtensionPrefs::default(),
            crate::profiles::Preferences::default().keep_out_of_git,
        )
    }
}

impl Switches {
    fn of(prefs: ExtensionPrefs, keep_out_of_git: bool) -> Self {
        Self {
            ask_before_adding_mcp: prefs.ask_before_adding_mcp,
            review_project_skills: prefs.review_project_skills,
            keep_out_of_git,
        }
    }

    /// The switches saved in Arko's folder `root`.
    pub(crate) fn load(root: &Path) -> Self {
        Self::of(
            load(root),
            crate::profiles::Preferences::load(root).keep_out_of_git,
        )
    }
}

/// A switch `/extensions` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    AskMcp,
    ReviewSkills,
    KeepOutOfGit,
}

impl Switch {
    const ALL: [Switch; 3] = [Switch::AskMcp, Switch::ReviewSkills, Switch::KeepOutOfGit];

    fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|switch| switch.word() == word)
    }

    /// The word that names it in the command.
    fn word(self) -> &'static str {
        match self {
            Switch::AskMcp => "ask-mcp",
            Switch::ReviewSkills => "review-skills",
            Switch::KeepOutOfGit => "keep-out-of-git",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Switch::AskMcp => ASK_MCP_NAME,
            Switch::ReviewSkills => REVIEW_SKILLS_NAME,
            Switch::KeepOutOfGit => KEEP_OUT_OF_GIT_NAME,
        }
    }

    fn of(self, switches: &Switches) -> bool {
        match self {
            Switch::AskMcp => switches.ask_before_adding_mcp,
            Switch::ReviewSkills => switches.review_project_skills,
            Switch::KeepOutOfGit => switches.keep_out_of_git,
        }
    }

    /// Saves the switch. The git switch also adds or removes Arko's
    /// `.gitignore` in the project `workspace`'s `.arko` folder.
    fn set(self, root: &Path, workspace: &Path, on: bool) -> Result<(), String> {
        match self {
            Switch::AskMcp => update(root, |prefs| prefs.ask_before_adding_mcp = on).map(|_| ()),
            Switch::ReviewSkills => {
                update(root, |prefs| prefs.review_project_skills = on).map(|_| ())
            }
            Switch::KeepOutOfGit => {
                let mut prefs = crate::profiles::Preferences::load(root);
                prefs.keep_out_of_git = on;
                prefs.save_in(root, workspace)
            }
        }
    }
}

fn state(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

impl Harness {
    /// `/extensions`, typed at the prompt, sent from the page or chosen in
    /// the settings: says the switches for what agents add, or with a
    /// switch's word and `on` or `off` changes it. It applies to the next
    /// thing an agent adds, and the agents aren't told.
    pub(crate) fn extensions_command(&mut self, argument: &str, source: Source) {
        let before = Switches::load(&self.root);
        let result = self.extensions_request(argument);
        let changed = before != Switches::load(&self.root);
        self.publish_extensions();
        self.setting_reply(result, source, changed);
    }

    /// Tells the clients the switches as saved now.
    pub(crate) fn publish_extensions(&self) {
        *crate::lock(&self.gauges.extensions) = Switches::load(&self.root);
    }

    fn extensions_request(&mut self, argument: &str) -> Result<String, String> {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let switches = Switches::load(&self.root);
        let (switch, on) = match words.as_slice() {
            [] => {
                let mut lines: Vec<String> = Switch::ALL
                    .into_iter()
                    .map(|switch| format!("{} is {}.", switch.name(), state(switch.of(&switches))))
                    .collect();
                lines.push("/extensions ask-mcp, review-skills or keep-out-of-git, then on or off, changes one.".to_string());
                return Ok(lines.join("\n"));
            }
            [word, value @ ("on" | "off")] => match Switch::parse(word) {
                Some(switch) => (switch, *value == "on"),
                None => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        if switch.of(&switches) == on {
            return Ok(format!("{} is {} already.", switch.name(), state(on)));
        }
        switch.set(&self.root, &self.settings.workspace, on)?;
        let mut text = format!("{} is now {}.", switch.name(), state(on));
        if switch == Switch::AskMcp && !on {
            text.push(' ');
            text.push_str(ASK_MCP_OFF_LINE);
        }
        Ok(text)
    }
}

fn usage() -> String {
    "/extensions takes ask-mcp, review-skills or keep-out-of-git, then on or off, such as /extensions ask-mcp off.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_file_agents_ask_about_servers_and_skip_the_skill_review() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            load(temp.path()),
            ExtensionPrefs {
                ask_before_adding_mcp: true,
                review_project_skills: false
            }
        );
        assert_eq!(
            Switches::load(temp.path()),
            Switches {
                ask_before_adding_mcp: true,
                review_project_skills: false,
                keep_out_of_git: true
            }
        );
        assert_eq!(Switches::default(), Switches::load(temp.path()));
    }

    /// A client that hasn't got a status yet never shows the two switches
    /// that guard what agents add as off.
    #[test]
    fn before_a_status_the_clients_show_the_defaults() {
        let live = crate::remote::LiveSettings::default();
        assert!(live.ask_before_adding_mcp);
        assert!(!live.review_project_skills);
        assert!(live.keep_agents_files_out_of_git);
        // The page's copy of the settings, from a status with none of them
        // and from one that has them.
        let page = |json: serde_json::Value| -> crate::remote::protocol::SettingsData {
            serde_json::from_value(json).unwrap()
        };
        let none = page(serde_json::json!({}));
        assert_ne!(none.ask_before_adding_mcp, Some(false));
        assert_ne!(none.keep_agents_files_out_of_git, Some(false));
        assert!(!none.review_project_skills);
        let sent = page(serde_json::to_value(&live).unwrap());
        assert_eq!(sent.ask_before_adding_mcp, Some(true));
        assert_eq!(sent.keep_agents_files_out_of_git, Some(true));
        let off = page(serde_json::json!({
            "ask_before_adding_mcp": false,
            "keep_agents_files_out_of_git": false,
        }));
        assert_eq!(off.ask_before_adding_mcp, Some(false));
        assert_eq!(off.keep_agents_files_out_of_git, Some(false));
    }

    #[test]
    fn a_new_session_reads_what_was_saved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        // (Arko's folder doesn't exist yet.)
        let saved = ExtensionPrefs {
            ask_before_adding_mcp: false,
            review_project_skills: true,
        };
        save(&root, &saved).unwrap();
        assert!(root.join("extensions.json").is_file());
        assert_eq!(load(&root), saved);
        let text = std::fs::read_to_string(root.join("extensions.json")).unwrap();
        assert!(text.contains("\"ask_before_adding_mcp\": false"), "{text}");
        assert!(text.contains("\"review_project_skills\": true"), "{text}");
        save(&root, &ExtensionPrefs::default()).unwrap();
        assert_eq!(load(&root), ExtensionPrefs::default());
    }

    #[test]
    fn a_file_without_a_switch_leaves_it_at_its_default() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("extensions.json");
        std::fs::write(&file, r#"{"review_project_skills": true}"#).unwrap();
        assert!(load(temp.path()).ask_before_adding_mcp);
        assert!(load(temp.path()).review_project_skills);
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(load(temp.path()), ExtensionPrefs::default());
    }

    #[test]
    fn two_sessions_changing_different_switches_keep_both() {
        let temp = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..16)
            .map(|n| {
                let root = temp.path().to_path_buf();
                std::thread::spawn(move || {
                    update(&root, |prefs| {
                        if n % 2 == 0 {
                            prefs.ask_before_adding_mcp = false;
                        } else {
                            prefs.review_project_skills = true;
                        }
                    })
                    .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            load(temp.path()),
            ExtensionPrefs {
                ask_before_adding_mcp: false,
                review_project_skills: true
            }
        );
        // The lock file is all that is beside it.
        let mut names: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["extensions.json", "extensions.json.lock"]);
    }

    #[test]
    fn the_words_follow_the_settings() {
        // The sentences the settings show hold no colon or semicolon.
        for text in [
            ASK_MCP_NAME,
            ASK_MCP_ABOUT,
            ASK_MCP_OFF,
            ASK_MCP_OFF_LINE,
            REVIEW_SKILLS_NAME,
            REVIEW_SKILLS_ABOUT,
            KEEP_OUT_OF_GIT_NAME,
            KEEP_OUT_OF_GIT_ABOUT,
        ] {
            assert!(!text.contains([':', ';']), "{text}");
        }
        assert_eq!(
            KEEP_OUT_OF_GIT_NAME,
            "Keep agents' project files out of git"
        );
    }

    /// The help names the switches and the command, and no longer the old
    /// label or the old profile folders as the only place.
    #[cfg(feature = "tui")]
    #[test]
    fn the_help_names_the_switches() {
        let help = <crate::cli::Args as clap::CommandFactory>::command()
            .get_after_help()
            .map(|help| help.to_string())
            .unwrap_or_default();
        assert!(help.contains("Approvals\nAgents ask you before they add an MCP server"));
        for word in ["/extensions", "ask-mcp", "review-skills", "keep-out-of-git"] {
            assert!(help.contains(word), "{word}");
        }
        for old in [
            "new profile folders",
            "in .agents/profiles in the project",
            "Arko makes that folder with a .gitignore",
        ] {
            assert!(!help.contains(old), "{old}");
        }
        assert!(help.contains("in .arko/profiles (or the older .agents/profiles) in the project"));
        assert!(
            help.contains("whether agents' project files stay out of git"),
            "the settings and the page"
        );
        // The command is one the menus offer, with its words.
        let command = crate::commands::find("/extensions ask-mcp on").unwrap();
        assert_eq!(command.name, "/extensions");
        assert!(
            command
                .about
                .contains("keep their project files out of git")
        );
        let settings = crate::commands::find("/settings").unwrap();
        assert!(
            settings
                .about
                .contains("whether agents' project files stay out of git")
        );
        let reading = crate::remote::Reading::default();
        let words =
            |before: &[&str]| crate::commands::dynamic_choices(&reading, "/extensions", before);
        assert_eq!(words(&[]), ["ask-mcp", "review-skills", "keep-out-of-git"]);
        for switch in Switch::ALL {
            assert_eq!(words(&[switch.word()]), ["on", "off"]);
        }
    }

    mod command {
        use super::super::*;
        use crate::harness::tests::harness;
        use arko_ai::LLMClient;

        /// What the last command said.
        fn last_said(harness: &Harness) -> String {
            harness
                .feed
                .subscribe()
                .0
                .items
                .iter()
                .rev()
                .map(|item| serde_json::to_value(item).unwrap())
                .find(|item| matches!(item["kind"].as_str(), Some("output" | "notice")))
                .and_then(|item| item["text"].as_str().map(str::to_string))
                .unwrap_or_default()
        }

        /// An app that saves the switches without the session has the
        /// session send them to its clients, with nothing in the
        /// transcript.
        #[tokio::test]
        async fn an_app_refreshes_the_switches_without_a_transcript_line() {
            let temp = tempfile::tempdir().unwrap();
            let (harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
            let root = harness.root.clone();
            let items = || harness.feed.subscribe().0.items.len();
            let before = items();
            let live = || crate::remote::Meter::read(harness.gauges.as_ref()).settings;
            assert!(live().ask_before_adding_mcp);
            update(&root, |prefs| {
                prefs.ask_before_adding_mcp = false;
                prefs.review_project_skills = true;
            })
            .unwrap();
            let mut git = crate::profiles::Preferences::load(&root);
            git.keep_out_of_git = false;
            git.save(&root).unwrap();
            // Until it is told, the session shows what it read last.
            assert!(live().ask_before_adding_mcp);
            harness.details.refresh_settings();
            let settings = live();
            assert!(!settings.ask_before_adding_mcp);
            assert!(settings.review_project_skills);
            assert!(!settings.keep_agents_files_out_of_git);
            assert_eq!(items(), before, "nothing in the transcript");
        }

        /// `/extensions` says the switches, changes each one and saves it
        /// where a new session reads it, and the clients read it from the
        /// status.
        #[tokio::test]
        async fn extensions_says_and_changes_the_switches() {
            let temp = tempfile::tempdir().unwrap();
            let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
            let root = harness.root.clone();
            let ignore = crate::arko_dir::ensure(&harness.settings.workspace, &root)
                .unwrap()
                .join(".gitignore");
            assert!(ignore.is_file());
            assert!(harness.live_command("/extensions", Source::Terminal));
            assert_eq!(
                last_said(&harness),
                "Ask before adding MCP servers is on.\nReview project skills is off.\nKeep agents' project files out of git is on.\n/extensions ask-mcp, review-skills or keep-out-of-git, then on or off, changes one."
            );
            assert!(harness.live_command("/extensions ask-mcp off", Source::Terminal));
            assert_eq!(
                last_said(&harness),
                "Ask before adding MCP servers is now off. Agents add MCP servers without asking."
            );
            assert!(!load(&root).ask_before_adding_mcp);
            assert!(harness.live_command("/extensions ask-mcp off", Source::Terminal));
            assert_eq!(
                last_said(&harness),
                "Ask before adding MCP servers is off already."
            );
            assert!(harness.live_command("/extensions review-skills on", Source::Terminal));
            assert_eq!(last_said(&harness), "Review project skills is now on.");
            assert!(harness.live_command("/extensions keep-out-of-git off", Source::Web));
            assert!(
                last_said(&harness).starts_with(
                    "Changed from the web page. Keep agents' project files out of git is now off."
                ),
                "{}",
                last_said(&harness)
            );
            // A new session reads them.
            assert_eq!(
                Switches::load(&root),
                Switches {
                    ask_before_adding_mcp: false,
                    review_project_skills: true,
                    keep_out_of_git: false
                }
            );
            assert!(!crate::profiles::Preferences::load(&root).keep_out_of_git);
            // Off took Arko's `.gitignore` out of the project's `.arko` folder.
            assert!(!ignore.exists());
            let live = crate::remote::Meter::read(harness.gauges.as_ref()).settings;
            assert!(!live.ask_before_adding_mcp);
            assert!(live.review_project_skills);
            assert!(!live.keep_agents_files_out_of_git);
            assert!(harness.live_command("/extensions keep-out-of-git on", Source::Terminal));
            assert!(ignore.is_file(), "on writes it again");
            assert!(harness.live_command("/extensions ask-mcp on", Source::Terminal));
            assert_eq!(
                last_said(&harness),
                "Ask before adding MCP servers is now on."
            );
            assert!(
                crate::remote::Meter::read(harness.gauges.as_ref())
                    .settings
                    .ask_before_adding_mcp
            );
            for bad in [
                "/extensions ask-mcp",
                "/extensions ask-mcp maybe",
                "/extensions ask-mcps on",
                "/extensions ask-mcp on now",
            ] {
                assert!(harness.live_command(bad, Source::Terminal));
                assert_eq!(
                    last_said(&harness),
                    "/extensions takes ask-mcp, review-skills or keep-out-of-git, then on or off, such as /extensions ask-mcp off.",
                    "{bad}"
                );
            }
            assert!(load(&root).ask_before_adding_mcp);
        }
    }
}
