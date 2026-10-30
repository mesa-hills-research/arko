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
    Command {
        name: "/new",
        args: "[continue]",
        about: "start a new conversation (/resume goes back to the old one). /new continue starts it from a brief of this one and the agent's report",
        aliases: &[],
        runs: Runs::Session,
    },
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
        "the view, theme, mouse, outside access, handover size, background jobs, monitor wakes, models for workers, worker depth, worker retry cap, message copies, whether agents ask before adding MCP servers, whether they ask before saving project skills, whether agents' project files stay out of git, whether the repository root's profiles are read, text replies, extra rules, Claude Code compaction, project instructions, built-in skills, web page, API keys and ChatGPT logins",
    ),
    Command {
        name: "/model",
        args: "[MODEL] [EFFORT] | worker [MODEL|main] [EFFORT]",
        about: "the main agent's model and effort, in a new conversation that carries this one over once it has started, or the model of workers started from now on. /model worker main has workers follow the main agent's model again",
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
    Command {
        name: "/mcp",
        args: "[NAME|remove NAME]",
        about: "the MCP servers and their tools, one server in full, or remove a server an agent added",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/skills",
        args: "[NAME|remove NAME]",
        about: "the skills the agents can load, one skill in full, or remove a skill an agent saved",
        aliases: &[],
        runs: Runs::Session,
    },
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
        name: "/approve",
        args: "[N…] [WORDS]",
        about: "approve the requests in the approval tray, all of them or those numbered, and reject the rest. Words after go to the agent",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/reject",
        args: "[WORDS]",
        about: "reject the requests in the approval tray. Words after go to the agent",
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
        name: "/worker-billing",
        args: "plans|ask|api",
        about: "whether workers may run on models billed per token, with plans (only on your plans), ask (you allow each) or api (without asking)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command {
        name: "/api-budget",
        args: "DOLLARS|on|off",
        about: "a daily budget for what is billed per token, across your sessions. Once it is reached, new workers on API models don't start. on means $10",
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
        name: "/extensions",
        args: "[ask-mcp|review-skills|keep-out-of-git] [on|off]",
        about: "whether agents ask before they add an MCP server, review the skills they save in the project, and keep their project files out of git",
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

/// What to tell the user when `text` (a message as typed, its leading
/// spaces kept) reads as a command that isn't one: one line that starts
/// with a slash and a word (`/quiet`) that names no command, or names one
/// that takes nothing after it (`/cost please`). It names the closest of
/// `offered` (the commands where it was typed) when one is near, and how to
/// send the line as a message instead, which a space before the slash does.
/// A path (`/usr/bin is missing`), a lone slash and text of several lines
/// are messages.
pub fn unknown<'a>(text: &str, offered: impl IntoIterator<Item = &'a Command>) -> Option<String> {
    let text = text.trim_end();
    let rest = text.strip_prefix('/')?;
    if text.contains('\n') || find(text).is_some() {
        return None;
    }
    let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let mut chars = word.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let name = format!("/{word}");
    const AS_MESSAGE: &str = "Start with a space to send it as a message.";
    if !after.trim().is_empty() && find(&name).is_some() {
        return Some(format!("{name} takes nothing after it. {AS_MESSAGE}"));
    }
    Some(match closest(&word.to_ascii_lowercase(), offered) {
        Some(near) => format!("{name} isn't a command. Did you mean {near}? {AS_MESSAGE}"),
        None => format!("{name} isn't a command. {AS_MESSAGE}"),
    })
}

/// The command of `offered` nearest to `word` (a name typed without its
/// slash): the one whose name it starts, else the one that takes it as its
/// first word (`quiet` is `/view quiet`), else the one it misspells, if any
/// is near enough.
fn closest<'a>(word: &str, offered: impl IntoIterator<Item = &'a Command>) -> Option<String> {
    let offered: Vec<&Command> = offered.into_iter().collect();
    let name = |command: &Command| command.name.trim_start_matches('/').to_string();
    let started: Vec<&&Command> = offered
        .iter()
        .filter(|command| name(command).starts_with(word))
        .collect();
    if let [command] = started[..] {
        return Some(command.name.to_string());
    }
    let taking: Vec<&str> = CHOICES
        .iter()
        .filter(|(command, before, words)| {
            before.is_empty()
                && words.contains(&word)
                && offered.iter().any(|offer| offer.name == *command)
        })
        .map(|(command, _, _)| *command)
        .collect();
    if let [command] = taking[..] {
        return Some(format!("{command} {word}"));
    }
    // A typo or a few, by the word's length.
    let most = (word.len() / 3).clamp(1, 3);
    offered
        .iter()
        .filter_map(|command| Some((edits(word, &name(command), most)?, command.name)))
        .min_by_key(|(edits, _)| *edits)
        .map(|(_, name)| name.to_string())
}

/// How many letters to add, remove, change or swap with the next to turn
/// `a` into `b` (ASCII words), when that is at most `most`. Words whose
/// lengths differ by more are refused before anything is counted, and the
/// count keeps three rows of its table.
fn edits(a: &str, b: &str, most: usize) -> Option<usize> {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > most {
        return None;
    }
    // The rows for a's letters before the last two, the last one, and this
    // one: how many edits turn a's start into each start of b.
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut last: Vec<usize> = (0..=b.len()).collect();
    let mut row: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        row[0] = i;
        for j in 1..=b.len() {
            let change = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (last[j] + 1).min(row[j - 1] + 1).min(last[j - 1] + change);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before[j - 2] + 1);
            }
            row[j] = best;
        }
        std::mem::swap(&mut before, &mut last);
        std::mem::swap(&mut last, &mut row);
    }
    let count = last[b.len()];
    (count <= most).then_some(count)
}

/// The commands the session runs wherever it was typed, the line prompt's.
pub fn of_session() -> impl Iterator<Item = &'static Command> {
    COMMANDS
        .iter()
        .filter(|command| command.runs == Runs::Session)
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
    ("/worker-billing", &[], &["plans", "ask", "api"]),
    ("/api-budget", &[], &["off", "on"]),
    ("/worker-retry-cap", &[], &["off", "on"]),
    ("/message-copies", &[], ON_OFF),
    (
        "/extensions",
        &[],
        &["ask-mcp", "review-skills", "keep-out-of-git"],
    ),
    ("/extensions", &["ask-mcp"], ON_OFF),
    ("/extensions", &["review-skills"], ON_OFF),
    ("/extensions", &["keep-out-of-git"], ON_OFF),
    ("/profiles", &[], &["add", "remove"]),
    ("/jobs", &[], &["stop"]),
    ("/new", &[], &["continue"]),
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
/// (Once the conversation has started, the main agent's starts a new
/// conversation on it.)
pub(crate) fn model_comes_next(_models: &crate::remote::Models, words: &[&str]) -> bool {
    matches!(words, [] | ["worker" | "workers"])
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
        // Once the conversation starts, the main agent's model chosen starts
        // a new conversation on it.
        let started = reading(true);
        assert!(
            typed(&started, &[], "openrouter:z").contains(&"openrouter:z-ai/glm-5".to_string())
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

    /// A slash and a word that names no command is refused with the
    /// nearest command where it was typed, and a space first sends it.
    /// Paths, a lone slash and longer texts are messages.
    #[test]
    fn a_word_after_a_slash_that_names_no_command_is_unknown() {
        let everywhere = || COMMANDS.iter();
        let space = "Start with a space to send it as a message.";
        assert_eq!(
            unknown("/quiet", everywhere()).unwrap(),
            format!("/quiet isn't a command. Did you mean /view quiet? {space}")
        );
        // The line prompt has no views, and an alias isn't offered.
        assert_eq!(
            unknown("/quiet", of_session()).unwrap(),
            format!("/quiet isn't a command. {space}")
        );
        let near = |text: &str| {
            let said = unknown(text, everywhere()).unwrap();
            let start = said.find("Did you mean ")? + "Did you mean ".len();
            Some(said[start..said.find('?').unwrap()].to_string())
        };
        assert_eq!(near("/setings").as_deref(), Some("/settings"));
        assert_eq!(near("/modle").as_deref(), Some("/model"));
        assert_eq!(near("/Worker").as_deref(), Some("/workers"));
        assert_eq!(near("/stop").as_deref(), Some("/stop-workers"));
        assert_eq!(near("/zdr on").as_deref(), Some("/openrouter zdr"));
        assert_eq!(near("/tmp is full"), None);
        // A command that takes nothing more says so.
        assert_eq!(
            unknown("/cost please", everywhere()).unwrap(),
            format!("/cost takes nothing after it. {space}")
        );
        for message in [
            "/home/me/x",
            "/usr/bin is missing",
            "/",
            "/ there",
            "/2 of them",
            " /quiet",
            "/quiet\nand more",
            "quiet",
            "/cost",
            "/view quiet",
            "/remote off",
        ] {
            assert_eq!(unknown(message, everywhere()), None, "{message:?}");
        }
        assert_eq!(edits("modle", "model", 3), Some(1));
        assert_eq!(edits("quiet", "quit", 3), Some(1));
        assert_eq!(edits("", "new", 3), Some(3));
        assert_eq!(edits("abc", "xyz", 2), None);
    }

    /// A word of any length is answered at once, without a table for its
    /// length per command.
    #[test]
    fn a_huge_word_after_a_slash_is_answered_at_once() {
        let word = "a".repeat(512 * 1024);
        let started = std::time::Instant::now();
        let said = unknown(&format!("/{word}"), COMMANDS.iter()).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(said.ends_with(" isn't a command. Start with a space to send it as a message."));
        assert_eq!(edits(&word, "new", 3), None);
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
