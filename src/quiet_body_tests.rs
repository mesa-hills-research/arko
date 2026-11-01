// Every parameter of every tool Arko offers shows in its call's Quiet
// body or in its row's words, end to end with the mock model (r127). (Included
// in `main`'s tests, beside the worker choice tests, whose setup it uses.)

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::worker_choice::{setup, start_worker};
use crate::remote::{Shown, quiet};

/// The tools a request offers, by name, with their parameters' schemas.
fn offered(body: &str) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let tool = if tool["function"].is_object() {
                        &tool["function"]
                    } else {
                        tool
                    };
                    let properties = tool["parameters"]["properties"].as_object().cloned();
                    Some((tool["name"].as_str()?.to_string(), properties.unwrap_or_default()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The values a test gives the parameter `name` of `schema`, each marked
/// with the name. Text comes long, with a folder and a second line, and
/// short, as a word. A choice comes as each of its values.
fn samples(name: &str, schema: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(choices) = schema["enum"].as_array() {
        return choices.clone();
    }
    match schema["type"].as_str() {
        Some("integer" | "number") => vec![serde_json::json!(7041)],
        Some("boolean") => vec![serde_json::json!(true), serde_json::json!(false)],
        Some("array") => vec![serde_json::json!([format!("{name}-item")])],
        Some("object") => vec![serde_json::json!({ format!("{name}-key"): format!("{name}-value") })],
        _ => vec![
            serde_json::json!(format!(
                "{name}-whole in deep/folder/{name}.rs, past what a row says\nand a second line {}",
                "with a long tail ".repeat(12)
            )),
            serde_json::json!(format!("{name}7")),
            // Trailing spaces, blank lines and only spaces are a value's too.
            serde_json::json!(format!("{name} ends in spaces  ")),
            serde_json::json!(format!("{name} ends in blank lines\n\n")),
            serde_json::json!("   "),
        ],
    }
}

/// The text in `value` that a body or a row's words hold.
fn leaves(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items.iter().flat_map(leaves).collect(),
        serde_json::Value::Object(map) => map
            .iter()
            .flat_map(|(key, value)| std::iter::once(key.clone()).chain(leaves(value)))
            .collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![other.to_string()],
    }
}

/// The calls whose items aren't a call's (a reply, a question, a message to
/// the user), which have no row.
const NOT_CALLS: [&str; 3] = ["complete_task", "ask_user", "message_user"];

/// The choices a call's words may state by the words they make, which the
/// test knows apart from the code that makes them: the tool, the parameter,
/// the value, and a phrase of the words in each form (while it runs, once
/// it ended, after "Couldn’t" when it failed). A branch that says a choice
/// not listed here fails.
const CHOICES: &[(&str, &str, &str, [&str; 3])] = &[
    ("terminal_control", "action", "wait", ["Checking job", "Checked job", "check job"]),
    ("terminal_control", "action", "list", ["Listing jobs", "Listed jobs", "list jobs"]),
    ("terminal_control", "action", "cancel", ["Stopping job", "Stopped job", "stop job"]),
    ("terminal_control", "action", "forget", ["Forgetting job", "Forgot job", "forget job"]),
    ("monitor_control", "action", "list", ["Listing monitors", "Listed monitors", "list monitors"]),
    (
        "monitor_control",
        "action",
        "rearm",
        ["Keeping a monitor going", "Kept a monitor going", "keep a monitor going"],
    ),
    ("monitor_control", "action", "stop", ["Stopping a monitor", "Stopped a monitor", "stop a monitor"]),
    ("mcp", "action", "call", ["Calling", "Called", "call"]),
    (
        "mcp",
        "action",
        "servers",
        ["Listing the MCP servers", "Listed the MCP servers", "list the MCP servers"],
    ),
    ("mcp", "action", "tools", ["’s tools", "’s tools", "’s tools"]),
    ("mcp", "action", "start", ["Starting", "Started", "start"]),
    ("mcp", "action", "stop", ["Stopping", "Stopped", "stop"]),
    ("mcp", "action", "add", ["Adding the", "Added the", "add the"]),
    ("mcp", "action", "remove", ["Removing the", "Removed the", "remove the"]),
    ("index_manage", "action", "rebuild", ["Rebuilding", "Rebuilt", "rebuild"]),
    ("index_manage", "action", "embed", ["Embedding", "Embedded", "embed"]),
    (
        "index_manage",
        "action",
        "estimate",
        ["Estimating embedding", "Estimated embedding", "estimate embedding"],
    ),
    (
        "index_manage",
        "action",
        "monitor_on",
        ["Turning on watching", "Turned on watching", "turn on watching"],
    ),
    (
        "index_manage",
        "action",
        "monitor_off",
        ["Turning off watching", "Turned off watching", "turn off watching"],
    ),
    ("index_manage", "action", "remove", ["Removing", "Removed", "remove"]),
    (
        "stop_agent",
        "remove_workspace",
        "true",
        ["removing its worktree", "removed its worktree", "remove its worktree"],
    ),
];

/// The forms a row's words take: while the call runs, once it ended, when
/// it failed or was refused (after "Couldn’t"), held for a compaction, and
/// stopped. Each with its status, its outcome, and which phrase of a
/// choice it holds (0 while it runs, 1 once it ended, 2 when it failed).
const FORMS: [(remote::CallStatus, Option<&str>, usize); 6] = [
    (remote::CallStatus::Running, None, 0),
    (remote::CallStatus::Ok, Some("done"), 1),
    (remote::CallStatus::Failed, Some("it broke"), 2),
    (remote::CallStatus::Failed, Some("refused"), 2),
    (remote::CallStatus::Failed, Some(quiet::HELD), 0),
    (remote::CallStatus::Failed, Some(quiet::STOPPED), 0),
];

/// The forms of `value` a row's words may show for the parameter `name` of
/// a call of `tool`, as text: the value itself, a number written out, each
/// run of a wait's list, and "lead" by the lead's name ("main" here).
fn forms_of(tool: &str, name: &str, value: &serde_json::Value) -> Vec<String> {
    let mut forms = match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Number(number) => vec![number.to_string()],
        _ => Vec::new(),
    };
    if tool == "wait_for_agents" && name == "runs" {
        let parameters: HashMap<String, serde_json::Value> =
            [(name.to_string(), value.clone())].into_iter().collect();
        forms.extend(crate::ui::waited_runs(&parameters, &[]));
    }
    if tool == "message_agent" && name == "to" && value == "lead" {
        forms.push("main".into());
    }
    forms
}

/// Checks that each of `parameters` of a call of `tool` shows whole in the
/// call's body, or in its row's words in every form the row takes. A text
/// the words show must be the value itself or a defined form of it
/// ([`forms_of`]). A choice must be one [`CHOICES`] lists, with its phrase
/// in each form. Returns how many values it checked.
fn check(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> usize {
    let made = crate::remote::call_said(tool, parameters, "main");
    let Some((act, said)) = made.and_then(|(item, said)| Some((item.act?, said))) else {
        assert!(NOT_CALLS.contains(&tool), "{tool} made no call: {parameters:?}");
        return 0;
    };
    assert!(!NOT_CALLS.contains(&tool), "{tool} made a call");
    // The row's words in each form.
    let words: Vec<(usize, String)> = FORMS
        .iter()
        .map(|(status, outcome, phrase)| {
            let steps = [quiet::Step::Call(quiet::Call {
                id: 1,
                act: Some(&act),
                status: Some(*status),
                outcome: *outcome,
                turn: Some(1),
                ..quiet::Call::default()
            })];
            match quiet::rows(&steps).as_slice() {
                [quiet::Piece::Row(row)] => (*phrase, row.words.text.clone()),
                other => panic!("{tool}: {other:?}"),
            }
        })
        .collect();
    let body: Vec<&str> = act.body.iter().map(|fact| fact.text.as_str()).collect();
    let body = body.join("\n");
    let mut checked = 0;
    for (name, value) in parameters {
        let claimed = said.all().iter().find(|(said, _)| said == name);
        match claimed.map(|(_, shown)| shown) {
            Some(Shown::Text(parts)) => {
                let forms = forms_of(tool, name, value);
                for part in parts {
                    assert!(
                        forms.contains(part),
                        "{tool}.{name}: the words are said to show {part:?}, which isn't {value:?} or a form of it"
                    );
                    for (_, words) in &words {
                        assert!(
                            words.contains(part.as_str()),
                            "{tool}.{name}: {words:?} doesn't show {part:?} ({parameters:?})"
                        );
                    }
                }
                // (Every run of a list, not some.)
                if tool == "wait_for_agents" && name == "runs" {
                    let runs = crate::ui::waited_runs(parameters, &[]);
                    assert!(runs.iter().all(|run| parts.contains(run)), "{parts:?}");
                }
            }
            Some(Shown::Choice) => {
                let value = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let Some((.., phrases)) = CHOICES
                    .iter()
                    .find(|(t, n, v, _)| *t == tool && n == name && *v == value)
                else {
                    panic!("{tool}.{name} = {value:?} is said as a choice the test doesn't know");
                };
                for (phrase, words) in &words {
                    assert!(
                        words.to_lowercase().contains(&phrases[*phrase].to_lowercase()),
                        "{tool}.{name} = {value:?}: {words:?} doesn't say {:?}",
                        phrases[*phrase]
                    );
                }
            }
            None => {}
        }
        if claimed.is_some() {
            assert!(
                act.body.iter().all(|fact| fact.label != *name),
                "{tool}.{name} is both said and in the body"
            );
        }
        for leaf in leaves(value) {
            assert!(
                body.contains(&leaf) || claimed.is_some(),
                "{tool}.{name} = {leaf:?} is in neither its body {:?} nor its row's words {words:?} ({parameters:?})",
                act.body
            );
            checked += 1;
        }
    }
    checked
}

/// Every parameter of every tool the main agent and a worker are offered,
/// and an MCP server's own tool, shows whole in the body of the call's
/// Quiet row, or in the row's words in every form the row takes (while it
/// runs, once it ended, failed, refused, held or stopped). What the branch
/// that made the words says they show is checked against the value (a text
/// must be the value or a defined form of it) and against the words, and a
/// choice against the test's own list of the words each value makes
/// ([`CHOICES`]). Each tool's calls come with all its parameters at once,
/// each varied in turn, so parameters that change each other's words are
/// covered (a `cd` in the command and a `cwd`, a note that replaces another
/// and its audience). A body too long for what a body keeps says how much
/// it left out.
#[tokio::test]
async fn every_parameter_shows_in_its_body_or_its_words() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;
    // The session's shell, terminal and monitor tools, and the search index
    // tools, as a session has them.
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    let indexes = index::Indexes::open(
        &temp.path().join("indexes"),
        &harness.settings.workspace,
        None,
        false,
    )
    .unwrap();
    harness.tools.extend(indexes.tools());
    let known = [index::Known {
        name: "work".into(),
        kind: "code",
        root: harness.settings.workspace.clone(),
        place: Some("the working directory"),
    }];
    let provider = providers::Setup::new(harness.settings.openai_auth);
    let settings = AgentSettings {
        index_tools: index::TOOL_NAMES,
        indexes: &known,
        ..AgentSettings::for_session(&harness.settings, &provider)
    };
    harness.main_config = agents::main_config(&settings).unwrap();
    harness.worker_config = agents::worker_config(&settings).unwrap();
    harness.open(false).unwrap();
    harness.start_new_workers();
    start_worker(&mut harness, &mut inputs, "Start plainly.").await;
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
    // Every definition offered, the main agent's and a worker's both when
    // they differ.
    let mut tools: BTreeSet<(String, String)> = BTreeSet::new();
    let mut agents = BTreeSet::new();
    for (who, _, _, body) in seen.lock().unwrap().iter() {
        agents.insert(who.clone());
        for (name, properties) in offered(body) {
            tools.insert((name, serde_json::Value::Object(properties).to_string()));
        }
    }
    assert!(agents.contains("main") && agents.len() > 1, "{agents:?}");
    // An MCP server's own tool, named `mcp__<server>__<tool>`, as a session
    // saved with one offers it. (Arko offers servers' tools through `mcp`,
    // which is among the tools above.)
    let mcp = serde_json::json!({
        "title": {"type": "string"},
        "labels": {"type": "array"},
        "draft": {"type": "boolean"},
        "number": {"type": "integer"},
    });
    tools.insert(("mcp__github__create_issue".into(), mcp.to_string()));
    let names: BTreeSet<&str> = tools.iter().map(|(name, _)| name.as_str()).collect();
    for tool in [
        "shell",
        "read_file",
        "write_file",
        "edit_file",
        "start_agent",
        "wait_for_agents",
        "message_agent",
        "keep_note",
        "mcp",
        "terminal_start",
        "terminal_status",
        "monitor_start",
        "index_search",
    ] {
        assert!(names.contains(tool), "{tool} isn't offered: {names:?}");
    }
    let mut checked = 0;
    for (tool, properties) in &tools {
        let properties: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(properties).unwrap();
        // All its parameters at once, then each in turn through its values,
        // the others held.
        let first: HashMap<String, serde_json::Value> = properties
            .iter()
            .map(|(name, schema)| (name.clone(), samples(name, schema).remove(0)))
            .collect();
        checked += check(tool, &first);
        for (name, schema) in &properties {
            // (And each choice the test knows, which a text's samples miss.)
            let mut values = samples(name, schema);
            values.extend(
                CHOICES
                    .iter()
                    .filter(|(t, n, v, _)| t == tool && n == name && *v != "true")
                    .map(|(.., value, _)| serde_json::json!(value)),
            );
            for value in values {
                let mut parameters = first.clone();
                parameters.insert(name.clone(), value.clone());
                checked += check(tool, &parameters);
                // And alone.
                checked += check(tool, &[(name.clone(), value)].into_iter().collect());
            }
        }
    }
    assert!(checked > 1000, "{checked}");
    // Parameters that change each other's words.
    let calls = [
        (
            "shell",
            serde_json::json!({"command": "cd child && pwd", "cwd": "parent"}),
        ),
        (
            "shell",
            serde_json::json!({"command": "pwd", "cwd": "parent"}),
        ),
        (
            "keep_note",
            serde_json::json!({"text": "Replacement note text", "replaces": 7, "audience": "r3"}),
        ),
        (
            "keep_note",
            serde_json::json!({"text": "Use tabs.", "audience": "lead"}),
        ),
        (
            "mcp",
            serde_json::json!({"action": "servers", "server": "github", "tool": "create_issue"}),
        ),
        (
            "message_agent",
            serde_json::json!({"to": "lead", "message": "Done."}),
        ),
    ];
    for (tool, parameters) in calls {
        check(tool, &serde_json::from_value(parameters).unwrap());
    }

    // A body too long for what a body keeps says how much it left out.
    let lines: Vec<String> = (1..=1000).map(|n| format!("line {n}")).collect();
    let write = |content: String| {
        let parameters: HashMap<String, serde_json::Value> = [
            ("path".to_string(), serde_json::json!("src/big.rs")),
            ("content".to_string(), serde_json::json!(content)),
        ]
        .into_iter()
        .collect();
        let (item, _) = crate::remote::call_said("write_file", &parameters, "main").unwrap();
        item.act
            .unwrap()
            .body
            .into_iter()
            .find(|fact| fact.label == "content")
            .unwrap()
            .text
    };
    let kept = write(lines.join("\n"));
    assert!(kept.starts_with("line 1\nline 2\n"), "{kept:.40}");
    assert!(kept.contains("\nline 400\n"), "{kept:.40}");
    assert!(!kept.contains("line 401"));
    assert!(kept.ends_with("\n… 600 more lines"), "{}", &kept[kept.len() - 40..]);
    let kept = write("x".repeat(51_240));
    assert!(kept.ends_with("\n… 11,240 more characters"), "{}", &kept[kept.len() - 40..]);
}
