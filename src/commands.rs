//! The commands typed at the prompt (`/name`), in one registry: what is a
//! command, the line prompt's `/help`, and the full-screen view's command
//! menu and help all read it.

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
        "start a new conversation (this session keeps the old one)",
    ),
    Command::session("/cost", "what this session has cost"),
    Command::session("/workers", "the workers: what each is doing, or reported"),
    Command::session(
        "/stop-workers",
        "stop the workers running (the agent can resume them)",
    ),
    Command::screen(
        "/watch",
        "WORKER|main",
        "follow one worker's own work, or the main agent's again",
    ),
    Command::screen(
        "/view",
        "full|quiet|messages",
        "how much of the work the transcript shows",
    ),
    Command::screen("/copy", "", "copy the agent's last reply"),
    Command::screen(
        "/settings",
        "",
        "the view, theme, mouse, outside access, handover size and web page",
    ),
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
        about: "outside access: whether the agents may change paths outside the working directory (/outside on, /outside off)",
        aliases: &[],
        runs: Runs::Session,
    },
    Command::session(
        "/handover",
        "the main agent hands over now: it writes a report and goes on from it in a fresh conversation",
    ),
    Command {
        name: "/handover-at",
        args: "SIZE",
        about: "the input size the main agent hands over at (/handover-at 200k)",
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

    #[test]
    fn only_whole_commands_are_commands() {
        assert_eq!(find("/cost").map(|c| c.name), Some("/cost"));
        assert_eq!(find(" /quit ").map(|c| c.name), Some("/exit"));
        assert_eq!(find("/remote off").map(|c| c.name), Some("/remote"));
        assert_eq!(find("/view quiet").map(|c| c.runs), Some(Runs::Screen));
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
}
