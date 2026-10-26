//! The command menu above the message box: the commands that start with
//! what's typed, then the words their arguments may be.

use crate::protocol::CommandData;

/// An entry of the menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub label: String,
    pub args: String,
    /// What a command does (empty for a word).
    pub about: String,
    /// What a pick puts in the box from `Menu::from`.
    pub insert: String,
}

/// The menu for the box's text.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    pub entries: Vec<Entry>,
    /// Where in the text a pick goes.
    pub from: usize,
    /// The entry it starts on, if any.
    pub selected: Option<usize>,
    /// The command whose hint shows (its first argument is being typed).
    pub command: Option<CommandData>,
}

/// The menu for `text`, or none when it isn't a command line.
/// `choices(name, before, typed)` gives the words an argument may be after
/// the words `before`, or none while they aren't known yet.
pub fn menu(commands: &[CommandData], text: &str, choices: &mut dyn FnMut(&str, &[String], &str) -> Option<Vec<String>>) -> Option<Menu> {
    if !text.starts_with('/') || text.contains('\n') {
        return None;
    }
    let Some(space) = text.find(char::is_whitespace) else {
        let typed = text.to_lowercase();
        let entries: Vec<Entry> = commands
            .iter()
            .filter(|command| command.name.starts_with(&typed))
            .map(|command| Entry {
                label: command.name.clone(),
                args: command.args.clone(),
                about: command.about.clone(),
                insert: if command.args.is_empty() { command.name.clone() } else { format!("{} ", command.name) },
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        let exact = entries.iter().position(|entry| entry.label == text).unwrap_or(0);
        return Some(Menu { entries, from: 0, selected: Some(exact), command: None });
    };
    let command = commands.iter().find(|command| command.name == text[..space])?;
    if command.args.is_empty() {
        return None;
    }
    let rest = &text[space..];
    let mut words: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
    let partial = if rest.ends_with(char::is_whitespace) { String::new() } else { words.pop().unwrap_or_default() };
    let typed = partial.to_lowercase();
    // Past the second word nothing is completed, so free text asks for nothing.
    let known = if words.len() <= 2 { choices(&command.name, &words, &partial).unwrap_or_default() } else { Vec::new() };
    let entries: Vec<Entry> = known
        .iter()
        .filter(|word| word.to_lowercase().starts_with(&typed) && **word != partial)
        .map(|word| Entry { label: word.clone(), args: String::new(), about: String::new(), insert: format!("{word} ") })
        .collect();
    // Past the first argument the menu shows only words to pick.
    if !words.is_empty() && entries.is_empty() {
        return None;
    }
    // A word typed whole (r1, with r10 too) is sent as it is.
    let selected = (!partial.is_empty() && !entries.is_empty() && !known.contains(&partial)).then_some(0);
    Some(Menu {
        from: text.len() - partial.len(),
        selected,
        command: words.is_empty().then(|| command.clone()),
        entries,
    })
}

/// What a key does while the menu is open.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Select(usize),
    Pick(usize),
    Close,
}

/// What `key` does with `selected`, and the word typed so far, or none when
/// the menu leaves it alone. Enter takes the selected entry unless the word
/// is already whole, which sends it as it is.
pub fn key(menu: &Menu, selected: Option<usize>, key: &str, typed: &str) -> Option<Key> {
    let count = menu.entries.len();
    if key == "Escape" {
        return Some(Key::Close);
    }
    if count == 0 {
        return None;
    }
    match key {
        "ArrowDown" => Some(Key::Select(selected.map_or(0, |at| (at + 1) % count))),
        "ArrowUp" => Some(Key::Select(selected.map_or(count - 1, |at| (at + count - 1) % count))),
        "Tab" => Some(Key::Pick(selected.unwrap_or(0))),
        "Enter" => selected.filter(|&at| menu.entries[at].label != typed).map(Key::Pick),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<CommandData> {
        let command = |name: &str, args: &str, about: &str| CommandData {
            name: name.into(),
            args: args.into(),
            about: about.into(),
        };
        vec![
            command("/model", "[worker] MODEL [EFFORT]", "Changes the model"),
            command("/message", "WORKER TEXT", "Messages a worker"),
            command("/new", "", "Starts a new conversation"),
        ]
    }

    fn no_choices(_: &str, _: &[String], _: &str) -> Option<Vec<String>> {
        None
    }

    #[test]
    fn the_menu_filters_the_commands_by_what_is_typed_and_shows_each_ones_arguments() {
        let list = commands();
        let menu = menu(&list, "/m", &mut no_choices).unwrap();
        let labels: Vec<&str> = menu.entries.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(labels, ["/model", "/message"]);
        assert_eq!(menu.entries[0].insert, "/model ");
        assert_eq!(menu.selected, Some(0));
        assert_eq!(super::menu(&list, "/new", &mut no_choices).unwrap().entries[0].insert, "/new");
        assert!(super::menu(&list, "/x", &mut no_choices).is_none());
        assert!(super::menu(&list, "hello", &mut no_choices).is_none());
    }

    #[test]
    fn after_a_command_and_a_space_the_menu_shows_the_words_its_arguments_may_be() {
        let list = commands();
        let mut asked = Vec::new();
        let mut choices = |name: &str, before: &[String], typed: &str| {
            asked.push((name.to_string(), before.to_vec(), typed.to_string()));
            Some(vec!["r1".to_string(), "r10".to_string(), "r2".to_string()])
        };
        let menu = menu(&list, "/message r", &mut choices).unwrap();
        assert_eq!(menu.entries.len(), 3);
        assert_eq!(menu.from, 9);
        assert_eq!(menu.selected, Some(0));
        assert!(menu.command.is_some());
        // A word typed whole is sent as it is.
        let menu = super::menu(&list, "/message r1", &mut choices).unwrap();
        assert_eq!(menu.selected, None);
        assert_eq!(asked[0], ("/message".to_string(), vec![], "r".to_string()));
        // Past the words, free text asks for nothing.
        assert!(super::menu(&list, "/message r1 hello there", &mut choices).is_none());
    }

    #[test]
    fn the_menus_keys_move_pick_and_close_and_enter_sends_a_word_that_is_whole() {
        let list = commands();
        let menu = menu(&list, "/m", &mut no_choices).unwrap();
        assert_eq!(key(&menu, Some(0), "ArrowDown", "/m"), Some(Key::Select(1)));
        assert_eq!(key(&menu, Some(0), "ArrowUp", "/m"), Some(Key::Select(1)));
        assert_eq!(key(&menu, None, "Tab", "/m"), Some(Key::Pick(0)));
        assert_eq!(key(&menu, Some(1), "Enter", "/m"), Some(Key::Pick(1)));
        assert_eq!(key(&menu, Some(0), "Enter", "/model"), None);
        assert_eq!(key(&menu, Some(0), "Escape", "/m"), Some(Key::Close));
        assert_eq!(key(&menu, Some(0), "a", "/m"), None);
    }
}
