//! The commands typed at the prompt (`/name`), in one registry: what is a
//! command, the line prompt's `/help`, and the full-screen view's command
//! menu and help all read it. With the readers of the values some commands
//! take, for a front end that checks them as they are typed.

/// A handover size (`/handover-at`): see its rules there.
pub use crate::handover::{MIN_TOKENS as MIN_HANDOVER_SIZE, parse_tokens as parse_handover_size};
/// When commands move to the background (`/background-after`): see its
/// rules there.
pub use crate::shell::{MAX_TIMEOUT_SECONDS as MAX_BACKGROUND_AFTER, parse_background_after};

/// Who runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// The session: typed at the line prompt, in the full-screen view or on
    /// the web page.
    Session,
    /// The full-screen view itself: it has no meaning anywhere else.
    Screen,
}

/// A command: its name, what may follow it, and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    /// What may follow the name, as the menu hints it (`ADDR|off|new-key`);
    /// empty when nothing may.
    pub args: &'static str,
    /// What it does, in a line.
    pub about: &'static str,
    /// Other names it answers to; the menu and the help leave them out.
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
        about: "go back to an earlier conversation of this session. /resume lists them, and /resume 2 goes back to the second",
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
        about: "send a worker a message directly, which it reads at its next step (a finished worker goes on with it). In a worker's own view, what you send goes to that worker",
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
        "the view, theme, mouse, outside access, handover size, background commands, text replies, extra rules, Claude Code compaction, project instructions, web page and API keys",
    ),
    Command {
        name: "/model",
        args: "[MODEL] [EFFORT] | worker [MODEL|main] [EFFORT]",
        about: "the main agent's model and effort before the conversation starts (/model sol high), or the model of workers started from now on, at any time (/model worker fable). /model worker main makes the workers follow the main agent's model again",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/keys",
        args: "[add|remove PROVIDER]",
        about: "add, replace or remove an API key for Anthropic, OpenAI, Gemini, OpenRouter or Voyage, used at once (/keys add openai)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session("/mcp", "the MCP servers and their tools"),
    Command::session("/skills", "the skills the agents can load"),
    Command::session("/session", "this session's ID, folder and models"),
    Command {
        name: "/remote",
        args: "ADDR|qr [INTERFACE]|off|new-key",
        about: "the web page, for a phone or browser (/remote ADDR, /remote qr [INTERFACE], /remote off, /remote new-key)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/outside",
        args: "on|off",
        about: "whether the agents may change paths outside the working directory (/outside on, /outside off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/background-after",
        args: "DURATION|off",
        about: "when a shell command still running moves to the background, where it goes on as a job and the agent goes on with its work (/background-after 10m, /background-after off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/text-replies",
        args: "on|off",
        about: "whether the main agent ends its turns with a plain reply (on) or by calling complete_task (off). It changes before the conversation starts (/text-replies off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/cc-compact",
        args: "on|off",
        about: "whether Claude Code compacts the conversation of agents on cc: models at the handover size (on), or they hand over as on the other providers (off). It changes before the conversation starts (/cc-compact off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/extra-rules",
        args: "small|all|off",
        about: "which agents get the extra rules for small models in their instructions, those on small models (small), every agent (all) or none (off). It changes before the conversation starts (/extra-rules off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/project-instructions",
        args: "one|both|off | workers on|off",
        about: "which of the project's AGENTS.md and CLAUDE.md the agents get, AGENTS.md else CLAUDE.md (one), both, or neither (off), and whether workers get them (workers on|off). The main agent gets them once per conversation, in its first message, and each worker once, in its first message. A change reaches the conversations and workers that start after it (/project-instructions both)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session(
        "/handover",
        "the main agent hands over now, writing a report and going on from it in a fresh conversation. On cc: models, Claude Code compacts the conversation instead",
    ),
    Command {
        name: "/handover-at",
        args: "[main|workers] SIZE",
        about: "the input size the main agent and the workers hand over at, or Claude Code compacts at on cc: models, each up to 95% of what its model accepts. main or workers sets one of them. Running workers go by it from their next step (/handover-at 950k)",
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

/// The command `text` (a whole line) is, if it is one: a command's name
/// alone, or followed by arguments when it takes them. Other text that
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

/// The commands whose names start with `prefix` (which starts with `/`),
/// for the menu: those of the session, and the screen's when `screen`.
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
];

/// The words that may follow `before` in the arguments of the command
/// `name`, for a front end's completion: its fixed ones, or what the session
/// has (its workers, the keys, the web page's interfaces, the models), as
/// its status `reading` says.
pub fn dynamic_choices(
    reading: &crate::remote::Reading,
    name: &str,
    before: &[&str],
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
        ("/stop-workers", []) => reading.workers_running.clone(),
        ("/keys", ["add" | "remove"]) => crate::credentials::KEYS
            .iter()
            .map(|key| key.id.to_string())
            .collect(),
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
        ("/model", words) => model_choices(&reading.models, words),
        (name, words) => choices(name, words)
            .iter()
            .map(|word| word.to_string())
            .collect(),
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
/// them: each name, padded, then what it does.
pub fn help_lines() -> Vec<String> {
    COMMANDS
        .iter()
        .filter(|command| command.runs == Runs::Session)
        .map(|command| format!("{:<14} {}", command.name, command.about))
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
            workers_running: vec!["r2".into()],
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
            ["/stop-workers", "/settings", "/skills", "/session"]
        );
        assert_eq!(
            names(matching("/s", false)),
            ["/stop-workers", "/skills", "/session"]
        );
        assert!(matching("/x", true).is_empty());
        let help = help_lines();
        assert!(
            help.iter()
                .any(|line| line.starts_with("/new           start"))
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
                .split(|c: char| !c.is_ascii_lowercase() && c != '-')
                .collect();
            for word in before.iter().chain(words.iter()) {
                assert!(hint.contains(word), "{name} {word}");
            }
        }
        assert_eq!(choices("/project-instructions", &["workers"]), ON_OFF);
        assert!(choices("/view", &["full"]).is_empty());
    }
}
