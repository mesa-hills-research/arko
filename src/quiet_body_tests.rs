// Every parameter of every tool Arko offers shows in its call's Quiet
// body or in its row's words, end to end with the mock model (r127). (Included
// in `main`'s tests, beside the worker choice tests, whose setup it uses.)

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::worker_choice::{setup, start_worker};
use crate::remote::quiet;

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

/// Checks that each of `parameters` of a call of `tool` shows whole in the
/// call's body, or in its row's words, as the branch that made them says
/// they show it. Returns how many values it checked.
fn check(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> usize {
    let made = crate::remote::call_said(tool, parameters, "main");
    let Some((act, said)) = made.and_then(|(item, said)| Some((item.act?, said))) else {
        assert!(NOT_CALLS.contains(&tool), "{tool} made no call: {parameters:?}");
        return 0;
    };
    assert!(!NOT_CALLS.contains(&tool), "{tool} made a call");
    let steps = [quiet::Step::Call(quiet::Call {
        id: 1,
        act: Some(&act),
        status: Some(remote::CallStatus::Ok),
        turn: Some(1),
        ..quiet::Call::default()
    })];
    let words = match quiet::rows(&steps).as_slice() {
        [quiet::Piece::Row(row)] => row.words.text.clone(),
        other => panic!("{tool}: {other:?}"),
    };
    let body: Vec<&str> = act.body.iter().map(|fact| fact.text.as_str()).collect();
    let body = body.join("\n");
    let mut checked = 0;
    for (name, value) in parameters {
        // What the branch says its words show for it, each part of which
        // they must hold.
        let claimed = said.all().iter().find(|(said, _)| said == name);
        if let Some((_, shown)) = claimed {
            for part in shown {
                assert!(
                    words.contains(part.as_str()),
                    "{tool}.{name}: the words {words:?} don't show {part:?}, as their branch says ({parameters:?})"
                );
            }
            assert!(
                act.body.iter().all(|fact| fact.label != *name),
                "{tool}.{name} is both said and in the body"
            );
        }
        for leaf in leaves(value) {
            let shown = body.contains(&leaf) || claimed.is_some();
            assert!(
                shown,
                "{tool}.{name} = {leaf:?} is in neither its body {:?} nor what its words {words:?} say ({parameters:?})",
                act.body
            );
            checked += 1;
        }
    }
    checked
}

/// Every parameter of every tool the main agent and a worker are offered,
/// and an MCP server's own tool, shows whole in the body of the call's
/// Quiet row, or in the row's words, which the branch that made them says
/// (and the words then hold what it says they show). Each tool's calls come
/// with all its parameters at once, each varied in turn, so parameters that
/// change each other's words are covered (a `cd` in the command and a
/// `cwd`, a note that replaces another and its audience). A body too long
/// for what a body keeps says how much it left out.
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
            for value in samples(name, schema) {
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
