use super::*;

fn added() -> agent_added::ServerRecord {
    agent_added::ServerRecord {
        file: PathBuf::from("/app/.arko/mcp.json"),
        folder: None,
        name: "github".into(),
        session: "2026-10-28-a1b2".into(),
        session_title: Some("Release prep".into()),
        added: "2026-10-28T12:00:00Z".parse().unwrap(),
        why: "Read and file this repository's issues.".into(),
    }
}

fn facts() -> McpFacts {
    McpFacts {
        servers: vec![
            McpServerFacts {
                name: "github".into(),
                shown: "npx -y @modelcontextprotocol/server-github".into(),
                file: PathBuf::from("/app/.arko/mcp.json"),
                tools: Ok((0..26).map(|n| format!("tool_number_{n:02}")).collect()),
                state: McpState::Running,
                env: vec![("GITHUB_TOKEN".into(), true), ("SENTRY_DSN".into(), false)],
                added: Some(added()),
                ..Default::default()
            },
            McpServerFacts {
                name: "figma".into(),
                shown: "npx -y figma-developer-mcp".into(),
                file: PathBuf::from("/arko/mcp.json"),
                state: McpState::StoppedByAgent,
                workers: false,
                shared: true,
                ..Default::default()
            },
            McpServerFacts {
                name: "broken".into(),
                shown: "npx broken".into(),
                file: PathBuf::from("/arko/mcp.json"),
                tools: Err("npx exited with code 1".into()),
                state: McpState::Failed("npx exited with code 1".into()),
                ..Default::default()
            },
        ],
        user_file: PathBuf::from("/arko/mcp.json"),
        ..Default::default()
    }
}

fn texts(lines: &[(Look, String)]) -> Vec<String> {
    lines.iter().map(|(_, text)| text.clone()).collect()
}

/// `/mcp` gives each server's command, file and what's unusual about it
/// (main agent only, shared, added by the agent), then its tools or state,
/// and ends with the warning while agents add servers without asking.
#[test]
fn mcp_lists_each_server_with_its_provenance_and_state() {
    let day = added()
        .added
        .with_timezone(&chrono::Local)
        .format("%-d %b")
        .to_string();
    let lines = list_lines(&facts(), false);
    let text = texts(&lines);
    assert_eq!(lines[0].0, Look::Server("github".into()));
    assert_eq!(
        text[0],
        format!(
            "npx -y @modelcontextprotocol/server-github · /app/.arko/mcp.json · added by the agent {day}"
        )
    );
    assert!(
        text[1].starts_with("  tool_number_00, tool_number_01"),
        "{}",
        text[1]
    );
    assert!(text[1].ends_with("… (26 tools)"), "{}", text[1]);
    assert_eq!(
        text[2],
        "npx -y figma-developer-mcp · /arko/mcp.json · main agent only · shared"
    );
    assert_eq!(
        (lines[3].0.clone(), text[3].as_str()),
        (Look::Dim, "  stopped by the agent")
    );
    assert_eq!(
        (lines[5].0.clone(), text[5].as_str()),
        (Look::Error, "  didn't start (npx exited with code 1)")
    );
    assert_eq!(text.len(), 6);
    let mut off = facts();
    off.adds_without_asking = true;
    let lines = list_lines(&off, false);
    assert_eq!(
        lines.last().unwrap(),
        &(
            Look::Warn,
            "Agents add MCP servers without asking.".to_string()
        )
    );
    // None at all.
    let none = McpFacts {
        user_file: PathBuf::from("/arko/mcp.json"),
        ..Default::default()
    };
    let text = texts(&list_lines(&none, false));
    assert_eq!(
        text,
        [
            "No MCP servers. Arko starts those set up in /arko/mcp.json, in the project's .arko/mcp.json and .mcp.json (once you approve them) and in --mcp-config files. The main agent can add one with your approval."
        ]
    );
    assert!(list_lines(&none, true).is_empty());
}

/// `/mcp NAME` shows one server whole: its command, file, environment
/// names set or not, workers, state, tools, and who added it, when and why.
#[test]
fn mcp_name_shows_one_server_whole() {
    let facts = facts();
    let date = added()
        .added
        .with_timezone(&chrono::Local)
        .format("%-d %b %Y")
        .to_string();
    let text = texts(&server_lines(&facts.servers[0]));
    assert_eq!(text[0], "");
    assert_eq!(text[1], "npx -y @modelcontextprotocol/server-github");
    assert_eq!(text[2], "/app/.arko/mcp.json");
    assert_eq!(text[3], "env      GITHUB_TOKEN set, SENTRY_DSN not set");
    assert_eq!(text[4], "workers  can use it");
    assert_eq!(text[5], "state    running, 26 tools");
    assert!(text[6].starts_with("tools    tool_number_00, "));
    assert_eq!(
        text[7],
        format!("Added by the agent in Release prep on {date}")
    );
    assert_eq!(text[8], "why      Read and file this repository's issues.");
    assert_eq!(text[9], "/mcp remove github removes it.");
    let text = texts(&server_lines(&facts.servers[1]));
    assert_eq!(
        text[3..],
        [
            "workers  can't use it",
            "shared   sessions open at the same time share one copy",
            "state    stopped by the agent",
        ]
    );
    assert_eq!(
        no_server("gihub", &facts),
        "There's no MCP server named gihub. The servers are github, figma, and broken."
    );
}

/// `/mcp` and `/mcp remove NAME` typed on the web page are answered there,
/// as in the terminal.
#[tokio::test]
async fn mcp_answers_on_the_web_page() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) =
        crate::harness::tests::harness(temp.path(), arko_ai::LLMClient::new(), false);
    // The page's items after the command, by kind and text.
    let after = |harness: &Harness, command: &str| -> Vec<(String, String)> {
        let items: Vec<serde_json::Value> = harness
            .feed
            .subscribe()
            .0
            .items
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let at = items
            .iter()
            .rposition(|item| item["text"] == command)
            .unwrap();
        items[at + 1..]
            .iter()
            .map(|item| {
                (
                    item["kind"].as_str().unwrap_or_default().to_string(),
                    item["text"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    assert!(harness.live_command("/mcp", crate::remote::Source::Web));
    let said = after(&harness, "/mcp");
    assert!(
        said.iter()
            .any(|(kind, text)| kind == "output" && text.starts_with("No MCP servers.")),
        "{said:?}"
    );
    assert!(harness.live_command("/mcp remove nosuch", crate::remote::Source::Web));
    assert_eq!(
        after(&harness, "/mcp remove nosuch"),
        [(
            "output".to_string(),
            "There's no MCP server named nosuch.".to_string()
        )]
    );
}
