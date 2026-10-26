//! The commands typed at the prompt (`/name`), in one registry, which
//! decides what is a command and which the line prompt's `/help` and the
//! full-screen view's command menu and help read. With the readers of the
//! values some commands take, for a front end that checks them as they
//! are typed.

/// The reader of a handover size (`/handover-at`), whose doc gives its
/// rules.
pub use crate::handover::{MIN_TOKENS as MIN_HANDOVER_SIZE, parse_tokens as parse_handover_size};
/// The reader of when commands move to the background
/// (`/background-after`), whose doc gives its rules.
pub use crate::shell::{MAX_TIMEOUT_SECONDS as MAX_BACKGROUND_AFTER, parse_background_after};

/// Who runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// The session, whether it's typed at the line prompt, in the
    /// full-screen view or on the web page.
    Session,
    /// The full-screen view itself, since it means nothing anywhere else.
    Screen,
}

/// A command, with its name, what may follow it, and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    /// What may follow the name, as the menu hints it (`ADDR|off|new-key`),
    /// or empty when nothing may.
    pub args: &'static str,
    /// What it does, in a line.
    pub about: &'static str,
    /// Other names it answers to, which the menu and the help leave out.
    pub aliases: &'static [&'static str],
    pub runs: Runs,
}

impl Command {
    const fn session(name: &'static str, about: &'static str) -> Self {
        Self {
            name,
            args: "",
            about,
            aliases: &[],
            runs: Runs::Session,
        }
    }

    const fn screen(name: &'static str, args: &'static str, about: &'static str) -> Self {
        Self {
            name,
            args,
            about,
            aliases: &[],
            runs: Runs::Screen,
        }
    }

    /// Whether `name` (a command's first word) names this command.
    fn named(&self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }

    /// Whether only the terminal runs it: the web page can't quit Arko or
    /// turn itself off.
    pub fn terminal_only(&self) -> bool {
        matches!(self.name, "/exit" | "/remote")
    }
}

/// Every command, in the order the help and the menu list them.
pub const COMMANDS: &[Command] = &[
    Command::session(
        "/new",
        "start a new conversation (/resume goes back to the old one)",
    ),
    Command {
        name: "/resume",
        args: "[NUMBER]",
        about: "go back to an earlier conversation of this session, from a list or by its number",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session("/cost", "what this session has cost"),
    Command::session("/workers", "what each worker is doing, or reported"),
    Command {
        name: "/stop-workers",
        args: "[WORKER]",
        about: "stop the workers running, or one of them (the agent can resume them)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::screen(
        "/watch",
        "WORKER|main",
        "follow one worker's own work, or the main agent's again",
    ),
    Command {
        name: "/message",
        args: "WORKER MESSAGE",
        about: "send a worker a message directly, which it reads at its next step or goes on with if it finished. What you type in a worker's own view goes to it too",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::screen(
        "/view",
        "full|quiet|messages",
        "how much of the work the transcript shows",
    ),
    Command::screen("/copy", "", "copy the agent's last reply"),
    Command::screen(
        "/settings",
        "",
        "the view, theme, mouse, outside access, handover size, background jobs, monitor wakes, models for workers, worker depth, worker retry cap, message copies, worker profile folders, text replies, extra rules, Claude Code compaction, project instructions, built-in skills, web page, API keys and ChatGPT logins",
    ),
    Command {
        name: "/model",
        args: "[MODEL] [EFFORT] | worker [MODEL|main] [EFFORT]",
        about: "the main agent's model and effort before the conversation starts, or the model of workers started from now on at any time. /model worker main has workers follow the main agent's model again",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/keys",
        args: "[add|remove PROVIDER]",
        about: "add, replace or remove an API key for Anthropic, OpenAI, Gemini, OpenRouter or Voyage, used at once",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/openrouter",
        args: "zdr on|off | data-collection allow|deny | region any|us|eu|us-or-eu | sort default|price|throughput|latency | precision any|fp8|bf16 | only SLUGS|none | ignore SLUGS|none | refresh",
        about: "how OpenRouter routes requests, which providers may serve them (zero data retention, no data collection, a region, providers to use or leave out, their order and precision), and its list of models. It changes with the next request",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/chatgpt",
        args: "[add PATH|remove N|first N]",
        about: "the ChatGPT logins, in the order they are used. The next one takes over when one reaches its usage limit",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session("/mcp", "the MCP servers and their tools"),
    Command::session("/skills", "the skills the agents can load"),
    Command::session("/session", "this session's ID, folder and models"),
    Command {
        name: "/session-names",
        args: "words|WAY...",
        about: "name sessions by the first words of their first message, or by a model asked in the background. A WAY is claude-plan, chatgpt-plan, claude-key or openai-key, and the ways are tried in the order given",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/remote",
        args: "ADDR|qr [INTERFACE]|off|new-key",
        about: "the web page, for a phone or browser",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/outside",
        args: "on|off",
        about: "whether the agents may change paths outside the working directory",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/jobs",
        args: "[stop N|all]",
        about: "the background jobs and monitors, and stopping them",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/notes",
        args: "[remove N]",
        about: "the notes the main agent keeps, which come back after a handover or compaction, and removing one",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/background-after",
        args: "DURATION|off",
        about: "how long a shell command or another long call runs before it moves to the background, where it goes on as a job and the agent goes on with its work",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/monitor-wakes",
        args: "DURATION|auto|always|off",
        about: "how long after your last message a monitor's warning still wakes the agent, so it can keep the monitor",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/profiles",
        args: "[NAME | add NAME MODEL [EFFORT] [DESCRIPTION] | remove NAME]",
        about: "list, show, add and remove worker profiles, the named setups the main agent starts workers with",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/worker-models",
        args: "agent|profiles|session",
        about: "who picks the workers' models, with agent (the main agent chooses), profiles (profiles only) or session (every worker runs the session's worker model)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/worker-depth",
        args: "1-4",
        about: "how deep workers go, from 1 to 4. 1 means only the main agent starts workers",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/worker-retry-cap",
        args: "off|on|N",
        about: "how many failed model requests in a row stop a worker. on means 10 and off means no cap",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/message-copies",
        args: "on|off",
        about: "copies of your messages to a worker for the agent that started it",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/text-replies",
        args: "on|off",
        about: "whether the main agent ends its turns with a plain reply (on) or by calling complete_task (off). It changes before the conversation starts",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/cc-compact",
        args: "on|off",
        about: "whether agents on cc: models have Claude Code compact the conversation at the handover size (on) or hand over as on other providers (off). It changes before the conversation starts",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/extra-rules",
        args: "small|all|off",
        about: "which agents get the extra rules for small models in their instructions, those on small models (small), every agent (all) or none (off). It changes before the conversation starts",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/project-instructions",
        args: "one|both|off | workers on|off",
        about: "which of the project's AGENTS.md and CLAUDE.md the agents get (one is AGENTS.md, else CLAUDE.md), and whether workers get them. Each conversation and worker gets them once, in its first message, and a change reaches those that start after it",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/builtin-skills",
        args: "on|off | NAME on|off",
        about: "whether the agents get Arko's built-in skills, all or one by name. It changes the agents before the conversation starts, and after it a resumed session gets the change",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session(
        "/handover",
        "the main agent hands over now, writing a report and going on from it in a fresh conversation (on cc: models, Claude Code compacts the conversation instead)",
    ),
    Command {
        name: "/handover-at",
        args: "[main|workers] SIZE",
        about: "the input size the main agent and the workers hand over at (or Claude Code compacts at, on cc: models), each up to 95% of what its model accepts. main or workers sets one of them, and running workers go by it from their next step",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session("/help", "the commands (and keys)"),
    Command {
        name: "/exit",
        args: "",
        about: "quit, with the session saved (Ctrl-D too)",
        aliases: &["/quit"],
        runs: Runs::Session,
    },
];

/// The command `text` (a whole line) is, if it is one. That is a command's
/// name alone, or followed by arguments when it takes them. Other text that
/// starts with a slash (`/usr/bin is missing`) is a message.
pub fn find(text: &str) -> Option<&'static Command> {
    let text = text.trim();
    let (name, rest) = match text.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (text, ""),
    };
    COMMANDS
        .iter()
        .find(|command| command.named(name) && (rest.is_empty() || !command.args.is_empty()))
}

/// The commands the web page runs, for its menu: the session's, except
/// those only the terminal runs.
pub fn on_page() -> impl Iterator<Item = &'static Command> {
    COMMANDS
        .iter()
        .filter(|command| command.runs == Runs::Session && !command.terminal_only())
}

/// The commands whose names start with `prefix` (which starts with `/`),
/// for the menu. They are the session's, and the screen's too when
/// `screen` is set.
pub fn matching(prefix: &str, screen: bool) -> Vec<&'static Command> {
    COMMANDS
        .iter()
        .filter(|command| screen || command.runs == Runs::Session)
        .filter(|command| command.name.starts_with(prefix))
        .collect()
}

const ON_OFF: &[&str] = &["on", "off"];

/// The words an argument may be, where they are fixed: the command, the
/// words before the argument, and its words. The full-screen view
/// completes them (and the arguments that depend on the session).
const CHOICES: &[(&str, &[&str], &[&str])] = &[
    ("/view", &[], &["full", "quiet", "messages"]),
    ("/outside", &[], ON_OFF),
    ("/background-after", &[], &["off"]),
    ("/monitor-wakes", &[], &["always", "off"]),
    ("/worker-models", &[], &["agent", "profiles", "session"]),
    ("/worker-retry-cap", &[], &["off", "on"]),
    ("/message-copies", &[], ON_OFF),
    ("/profiles", &[], &["add", "remove"]),
    ("/jobs", &[], &["stop"]),
    ("/notes", &[], &["remove"]),
    ("/jobs", &["stop"], &["all"]),
    ("/text-replies", &[], ON_OFF),
    ("/cc-compact", &[], ON_OFF),
    ("/extra-rules", &[], &["small", "all", "off"]),
    (
        "/project-instructions",
        &[],
        &["one", "both", "off", "workers"],
    ),
    ("/project-instructions", &["workers"], ON_OFF),
    ("/remote", &[], &["qr", "off", "new-key"]),
    ("/handover-at", &[], &["main", "workers"]),
    ("/keys", &[], &["add", "remove"]),
    (
        "/openrouter",
        &[],
        &[
            "zdr",
            "data-collection",
            "region",
            "sort",
            "precision",
            "only",
            "ignore",
            "refresh",
        ],
    ),
    ("/openrouter", &["zdr"], ON_OFF),
    ("/openrouter", &["data-collection"], &["allow", "deny"]),
    ("/openrouter", &["region"], &["any", "us", "eu", "us-or-eu"]),
    (
        "/openrouter",
        &["sort"],
        &["default", "price", "throughput", "latency"],
    ),
    ("/openrouter", &["precision"], &["any", "fp8", "bf16"]),
    ("/openrouter", &["only"], &["none"]),
    ("/openrouter", &["ignore"], &["none"]),
    ("/chatgpt", &[], &["add", "remove", "first"]),
];

/// The words that may follow `before` in the arguments of the command
/// `name`, for a front end's completion. They are the command's fixed
/// words, or what the session has (its workers, the keys, the web page's
/// interfaces, the models) as its status `reading` says.
pub fn dynamic_choices(
    reading: &crate::remote::Reading,
    name: &str,
    before: &[&str],
) -> Vec<String> {
    dynamic_choices_typed(reading, name, before, "")
}

/// [`dynamic_choices`] for the word being typed (`typed`, the start of the
/// word after `before`). `/model` completes an OpenRouter model typed as
/// `openrouter:` and more from OpenRouter's list, whichever models were
/// picked before, up to forty. That list is read from memory and fetched in
/// the background when it is due, so the first answer may lack it, and a
/// later one has it.
pub fn dynamic_choices_typed(
    reading: &crate::remote::Reading,
    name: &str,
    before: &[&str],
    typed: &str,
) -> Vec<String> {
    let workers = || -> Vec<String> {
        reading
            .agents
            .iter()
            .map(|agent| agent.id.clone())
            .collect()
    };
    match (name, before) {
        ("/watch", []) => workers().into_iter().chain(["main".to_string()]).collect(),
        ("/message", []) => workers(),
        ("/stop-workers", []) => reading
            .workers_running
            .iter()
            .map(|(id, _)| id.clone())
            .collect(),
        ("/keys", ["add" | "remove"]) => crate::credentials::KEYS
            .iter()
            .map(|key| key.id.to_string())
            .collect(),
        ("/session-names", words) => {
            use crate::session_names::Way;
            let ways = Way::ALL
                .iter()
                .map(|way| way.id())
                .filter(|id| !words.contains(id));
            match words {
                [] => std::iter::once("words")
                    .chain(ways)
                    .map(String::from)
                    .collect(),
                ["words"] => Vec::new(),
                _ => ways.map(String::from).collect(),
            }
        }
        ("/remote", ["qr"]) => {
            let mut names: Vec<String> = reading
                .page
                .iter()
                .flat_map(|page| page.links.iter())
                .filter_map(|link| link.interface.clone())
                .collect();
            names.dedup();
            names
        }
        ("/model", words) => {
            let mut choices = model_choices(&reading.models, words);
            if model_comes_next(&reading.models, words)
                && let Some(root) = &reading.models.root
            {
                for name in crate::openrouter::completions(root, typed) {
                    if !choices.contains(&name) {
                        choices.push(name);
                    }
                }
            }
            choices
        }
        ("/builtin-skills", []) => ON_OFF
            .iter()
            .copied()
            .chain(crate::builtin_skills::names())
            .map(String::from)
            .collect(),
        ("/builtin-skills", [_]) => ON_OFF.iter().map(|word| word.to_string()).collect(),
        (name, words) => choices(name, words)
            .iter()
            .map(|word| word.to_string())
            .collect(),
    }
}

/// Whether the next word of `/model` (after `words`) may be a model.
/// The main agent keeps its model once the conversation has started.
pub(crate) fn model_comes_next(models: &crate::remote::Models, words: &[&str]) -> bool {
    match words {
        [] => !models.started,
        ["worker" | "workers"] => true,
        _ => false,
    }
}

/// `/model`'s words: `worker`, the models the picker offers, `main` for the
/// workers, then the efforts the model takes.
fn model_choices(models: &crate::remote::Models, words: &[&str]) -> Vec<String> {
    let (workers, words) = match words {
        ["worker" | "workers", rest @ ..] => (true, rest),
        _ => (false, words),
    };
    let names =
        || crate::models::names(models, |name| crate::models::needs(&models.reachable, name));
    match words {
        // The main agent keeps its model once the conversation starts.
        [] if !workers && models.started => {
            if models.has_workers {
                vec!["worker".into()]
            } else {
                Vec::new()
            }
        }
        [] if workers => std::iter::once("main".to_string()).chain(names()).collect(),
        [] => names()
            .into_iter()
            .chain(models.has_workers.then(|| "worker".to_string()))
            .collect(),
        [model] if *model != "main" => crate::providers::resolve_model(model)
            .map(|model| {
                crate::providers::efforts(&model)
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The fixed words that may follow `before` in the arguments of the
/// command `name`.
pub fn choices(name: &str, before: &[&str]) -> &'static [&'static str] {
    CHOICES
        .iter()
        .find(|(command, words, _)| *command == name && *words == before)
        .map_or(&[], |(_, _, choices)| choices)
}

/// The help's lines for the session's commands, as the line prompt shows
/// them, with each name, padded, then what it does and what may follow it.
pub fn help_lines() -> Vec<String> {
    COMMANDS
        .iter()
        .filter(|command| command.runs == Runs::Session)
        .map(|command| {
            let line = format!("{:<14} {}", command.name, command.about);
            match command.args {
                "" => line,
                args => format!("{line}  {args}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app completes a command's arguments from the session's status:
    /// its workers, those running, the web page's interfaces, the models.
    #[test]
    fn choices_come_from_the_status() {
        use crate::remote::{AgentStatus, Link, LinkKind, Page, Reading};
        let agent = |id: &str| AgentStatus {
            id: id.into(),
            context_tokens: None,
            context_window: None,
            handover_at: None,
            compacts: false,
        };
        let mut reading = Reading {
            agents: vec![agent("r1"), agent("r2")],
            workers_running: vec![("r2".into(), None)],
            ..Reading::default()
        };
        reading.page = Some(Page {
            addr: "0.0.0.0:8765".parse().unwrap(),
            links: vec![Link {
                url: "http://192.168.1.2:8765/#key=k".into(),
                interface: Some("eth0".into()),
                kind: LinkKind::Lan,
                loopback: false,
            }],
            note: None,
        });
        reading.models.has_workers = true;
        assert_eq!(
            dynamic_choices(&reading, "/watch", &[]),
            ["r1", "r2", "main"]
        );
        assert_eq!(dynamic_choices(&reading, "/message", &[]), ["r1", "r2"]);
        assert_eq!(dynamic_choices(&reading, "/stop-workers", &[]), ["r2"]);
        assert_eq!(dynamic_choices(&reading, "/remote", &["qr"]), ["eth0"]);
        assert!(dynamic_choices(&reading, "/keys", &["add"]).contains(&"anthropic".to_string()));
        let workers = dynamic_choices(&reading, "/model", &["worker"]);
        assert_eq!(workers.first().map(String::as_str), Some("main"));
        assert!(dynamic_choices(&reading, "/model", &["opus"]).contains(&"high".to_string()));
        assert_eq!(dynamic_choices(&reading, "/outside", &[]), ["on", "off"]);
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &[]),
            [
                "zdr",
                "data-collection",
                "region",
                "sort",
                "precision",
                "only",
                "ignore",
                "refresh"
            ]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["zdr"]),
            ["on", "off"]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["region"]),
            ["any", "us", "eu", "us-or-eu"]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["sort"]),
            ["default", "price", "throughput", "latency"]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["precision"]),
            ["any", "fp8", "bf16"]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["data-collection"]),
            ["allow", "deny"]
        );
        assert_eq!(
            dynamic_choices(&reading, "/openrouter", &["only"]),
            ["none"]
        );
        assert!(dynamic_choices(&reading, "/openrouter", &["refresh"]).is_empty());
        assert_eq!(
            dynamic_choices(&reading, "/session-names", &[]),
            [
                "words",
                "claude-plan",
                "chatgpt-plan",
                "claude-key",
                "openai-key"
            ]
        );
        assert_eq!(
            dynamic_choices(&reading, "/session-names", &["claude-key", "claude-plan"]),
            ["chatgpt-plan", "openai-key"]
        );
        assert!(dynamic_choices(&reading, "/session-names", &["words"]).is_empty());
        assert_eq!(
            dynamic_choices(&reading, "/builtin-skills", &[]),
            [
                "on",
                "off",
                "user-facing-prose",
                "repo-docs-style",
                "ponytail"
            ]
        );
        assert_eq!(
            dynamic_choices(&reading, "/builtin-skills", &["repo-docs-style"]),
            ["on", "off"]
        );
    }

    /// `/model` completes an OpenRouter model typed as `openrouter:` and
    /// more from the models in memory, where a model may come next.
    #[test]
    fn a_model_on_openrouter_completes_from_its_list() {
        use crate::openrouter::{Region, Settings, seed_for_tests};
        use crate::remote::{Models, Reading};
        let temp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.set_zdr(true);
        settings.set_region(Region::UsOrEu);
        settings.save(temp.path()).unwrap();
        seed_for_tests(&settings, &["moonshotai/kimi-k3", "z-ai/glm-5"]);
        let reading = |started| Reading {
            models: Models {
                root: Some(temp.path().to_path_buf()),
                started,
                has_workers: true,
                ..Models::default()
            },
            ..Reading::default()
        };
        let typed = |reading: &Reading, before: &[&str], typed: &str| {
            dynamic_choices_typed(reading, "/model", before, typed)
        };
        let before_start = reading(false);
        let found = typed(&before_start, &[], "openrouter:moon");
        assert!(
            found.contains(&"openrouter:moonshotai/kimi-k3".to_string()),
            "{found:?}"
        );
        assert!(
            found.contains(&"cc:opus".to_string()),
            "the picker's words stay"
        );
        assert!(
            !found.contains(&"openrouter:z-ai/glm-5".to_string()),
            "{found:?}"
        );
        // The workers' model too, and not the words after a model.
        assert!(
            typed(&before_start, &["worker"], "openrouter:z")
                .contains(&"openrouter:z-ai/glm-5".to_string())
        );
        assert!(
            !typed(&before_start, &["opus"], "openrouter:z")
                .contains(&"openrouter:z-ai/glm-5".to_string())
        );
        // Without a start of an OpenRouter model, nothing from the list.
        assert!(!typed(&before_start, &[], "").contains(&"openrouter:z-ai/glm-5".to_string()));
        assert!(!typed(&before_start, &[], "o").contains(&"openrouter:z-ai/glm-5".to_string()));
        // The main agent keeps its model once the conversation starts.
        let started = reading(true);
        assert!(
            !typed(&started, &[], "openrouter:z").contains(&"openrouter:z-ai/glm-5".to_string())
        );
        assert!(
            typed(&started, &["worker"], "openrouter:z")
                .contains(&"openrouter:z-ai/glm-5".to_string())
        );
        // `dynamic_choices` is the same without a typed start.
        assert_eq!(
            dynamic_choices(&before_start, "/model", &[]),
            typed(&before_start, &[], "")
        );
    }

    #[test]
    fn only_whole_commands_are_commands() {
        assert_eq!(find("/cost").map(|c| c.name), Some("/cost"));
        assert_eq!(find(" /quit ").map(|c| c.name), Some("/exit"));
        assert_eq!(find("/remote off").map(|c| c.name), Some("/remote"));
        assert_eq!(find("/view quiet").map(|c| c.runs), Some(Runs::Screen));
        assert_eq!(find("/resume").map(|c| c.name), Some("/resume"));
        assert_eq!(find("/resume 2").map(|c| c.name), Some("/resume"));
        // Commands without arguments take none, and other slashes are text.
        assert!(find("/cost please").is_none());
        assert!(find("/usr/bin is missing").is_none());
        assert!(find("/remotely").is_none());
    }

    #[test]
    fn the_menu_filters_by_prefix_and_the_line_prompt_lists_the_session() {
        let names = |commands: Vec<&Command>| commands.iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(
            names(matching("/s", true)),
            [
                "/stop-workers",
                "/settings",
                "/skills",
                "/session",
                "/session-names"
            ]
        );
        assert_eq!(
            names(matching("/s", false)),
            ["/stop-workers", "/skills", "/session", "/session-names"]
        );
        assert!(matching("/x", true).is_empty());
        let help = help_lines();
        assert!(
            help.iter()
                .any(|line| line.starts_with("/new           start"))
        );
        // What a command takes follows what it does.
        assert!(
            help.iter()
                .any(|line| line.starts_with("/worker-depth ") && line.ends_with("  1-4"))
        );
        assert!(!help.iter().any(|line| line.starts_with("/copy")));
        // Every name is unique, including the aliases.
        let mut all: Vec<&str> = COMMANDS
            .iter()
            .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
            .collect();
        let count = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), count);
    }

    /// Each fixed choice is a word its command's hint shows, so the two
    /// don't drift apart.
    #[test]
    fn the_choices_are_the_hints_words() {
        for (name, before, words) in CHOICES {
            let command = find(name).unwrap();
            let hint: Vec<&str> = command
                .args
                .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-')
                .collect();
            for word in before.iter().chain(words.iter()) {
                assert!(hint.contains(word), "{name} {word}");
            }
        }
        assert_eq!(choices("/project-instructions", &["workers"]), ON_OFF);
        assert!(choices("/view", &["full"]).is_empty());
    }
}
