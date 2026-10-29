// End-to-end tests of the MCP servers behind `mcp`: an add through the
// engine with the mock model, the tool list and the prompt staying the same
// across it, sessions sharing a server, and conversations saved with a tool
// for each MCP tool. (Included in `engine`'s tests, whose helpers they use.)

/// A stub MCP server with a `ping` tool that answers "pong", which needs no
/// environment.
const PING_STUB: &str = r#"
import json, sys
def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        reply(message["id"], {"protocolVersion": message["params"]["protocolVersion"],
              "capabilities": {"tools": {}}, "serverInfo": {"name": "ping", "version": "1"}})
    elif method == "tools/list":
        reply(message["id"], {"tools": [{"name": "ping", "description": "Answers pong.",
              "inputSchema": {"type": "object", "properties": {}}}]})
    else:
        reply(message["id"], {"content": [{"type": "text", "text": "pong"}]})
"#;

/// The names of the tools a request offers.
fn offered(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The parts of a request the prompt cache keeps: its tools and its
/// instructions, as sent.
fn cached_parts(body: &str) -> (String, String) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    (body["tools"].to_string(), body["instructions"].to_string())
}

/// Rewrites the session in `dir` as an Arko from before `mcp` saved it:
/// each configuration that offers `mcp` offers `direct` (a tool of a
/// server's own) in its place.
fn to_direct_tools(dir: &std::path::Path, direct: &str) {
    fn visit(value: &mut serde_json::Value, direct: &str) -> bool {
        let mut changed = false;
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    if key == "available_tool_names"
                        && let serde_json::Value::Array(names) = value
                    {
                        for name in names.iter_mut() {
                            if name == "mcp" {
                                *name = direct.into();
                                changed = true;
                            }
                        }
                    } else {
                        changed |= visit(value, direct);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    changed |= visit(item, direct);
                }
            }
            _ => {}
        }
        changed
    }
    let mut changed = 0;
    for entry in walk(dir) {
        if entry.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if visit(&mut value, direct) {
            std::fs::write(&entry, value.to_string()).unwrap();
            changed += 1;
        }
    }
    assert!(changed > 0, "nothing in {} offers mcp", dir.display());
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// The main agent adds a server in the middle of a conversation (with Ask
/// before adding MCP servers off, so nothing is asked), and calls it in the
/// same turn. Every request of the conversation, before and after the add,
/// sends the same tools and instructions byte for byte. The next session
/// starts the server without asking, and its instructions name it.
#[tokio::test]
async fn the_tool_list_stays_the_same_across_an_add() {
    let temp = tempfile::tempdir().unwrap();
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    crate::extension_prefs::update(&root, |prefs| prefs.ask_before_adding_mcp = false).unwrap();
    let script = temp.path().join("ping.py");
    std::fs::write(&script, PING_STUB).unwrap();
    let python = crate::mcp::tests::python();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        let (python, script) = (python.clone(), script.display().to_string());
        move |_, body: String| {
            let count = {
                let mut bodies = bodies.lock().unwrap();
                bodies.push(body);
                bodies.len()
            };
            let (python, script) = (python.clone(), script.clone());
            Box::pin(async move {
                match count {
                    1 => tool_call(
                        "c1",
                        "mcp",
                        serde_json::json!({
                            "action": "add",
                            "server": "pinger",
                            "command": python,
                            "args": [script],
                            "why": "Answers pong for the test.",
                        }),
                    ),
                    2 => tool_call(
                        "c2",
                        "mcp",
                        serde_json::json!({"action": "call", "server": "pinger", "tool": "ping"}),
                    ),
                    n => reply("a", &format!("Reply {n}.")),
                }
            })
        }
    }))
    .await;
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    assert!(seam.details.mcp().servers.is_empty());
    seam.input
        .send(Input::Message(app_message("Add the ping server.")))
        .unwrap();
    answered(&seam, &bodies, 3).await;
    seam.input.send(Input::Message(app_message("Again."))).unwrap();
    answered(&seam, &bodies, 4).await;
    let bodies = bodies.lock().unwrap().clone();
    assert!(
        bodies[1].contains("Added pinger for this project. It started with 1 tool."),
        "{}",
        bodies[1]
    );
    assert!(bodies[2].contains("pong"), "{}", bodies[2]);
    let first = cached_parts(&bodies[0]);
    for body in &bodies[1..] {
        assert_eq!(cached_parts(body), first);
    }
    let tools = offered(&bodies[0]);
    assert!(tools.contains(&"mcp".to_string()), "{tools:?}");
    assert!(!tools.iter().any(|tool| tool.starts_with("mcp__")), "{tools:?}");
    // The session's facts: added by the agent, running, and the warning.
    let mcp = seam.details.mcp();
    assert_eq!(mcp.servers.len(), 1);
    assert_eq!(mcp.servers[0].state, crate::details::McpState::Running);
    assert_eq!(
        mcp.servers[0].added.as_ref().unwrap().why,
        "Answers pong for the test."
    );
    assert!(mcp.adds_without_asking);
    assert!(project.join(".arko/mcp.json").is_file());
    assert!(!project.join(".agents").exists());
    quit(seam, ended).await;
    // The next session starts it without asking (no one could answer).
    let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
    let mcp = seam.details.mcp();
    assert_eq!(mcp.servers.len(), 1);
    assert_eq!(mcp.servers[0].state, crate::details::McpState::Running);
    quit(seam, ended).await;
}

/// Two sessions open in one process, as an app opens them, share one copy
/// of a server marked shared, and closing one keeps it running for the
/// other.
#[tokio::test]
async fn two_sessions_share_a_shared_server_and_closing_one_keeps_it() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("stub.py");
    std::fs::write(&script, MCP_STUB).unwrap();
    let log = temp.path().join("stub.log");
    let config = temp.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({"mcpServers": {"stub": {
            "command": crate::mcp::tests::python(),
            "args": [script],
            "env": {"STUB_LOG": log},
            "shared": true,
        }}})
        .to_string(),
    )
    .unwrap();
    let events = |event: &str| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == event)
            .count()
    };
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("Call ping.") && !body.contains("pong") {
                    tool_call(
                        "c",
                        "mcp",
                        serde_json::json!({"action": "call", "server": "stub", "tool": "ping"}),
                    )
                } else {
                    reply("a", "Done.")
                }
            })
        }
    }))
    .await;
    let open = |name: &str| {
        let running = spawn(
            Options {
                root: Some(temp.path().join(name)),
                workspace: Some(temp.path().join(format!("{name}-project"))),
                mcp_config: vec![config.clone()],
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                text_replies: Some(true),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        async move {
            let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            (seam, running.ended)
        }
    };
    std::fs::create_dir_all(temp.path().join("one-project")).unwrap();
    std::fs::create_dir_all(temp.path().join("two-project")).unwrap();
    let (one, one_ended) = open("one").await;
    let (two, two_ended) = open("two").await;
    let running = |seam: &Seam| seam.details.mcp().servers[0].state.clone();
    assert_eq!(running(&one), crate::details::McpState::Running);
    assert_eq!(running(&two), crate::details::McpState::Running);
    assert_eq!(events("start"), 1, "one copy for both sessions");
    quit(one, one_ended).await;
    assert_eq!(events("stop"), 0);
    assert_eq!(running(&two), crate::details::McpState::Running);
    let before = bodies.lock().unwrap().len();
    two.input.send(Input::Message(app_message("Call ping."))).unwrap();
    let called = answered(&two, &bodies, before + 2).await;
    assert!(called.contains("pong"), "{called}");
    quit(two, two_ended).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while events("stop") < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(events("start"), 1);
}

/// A conversation saved with a tool for each MCP tool, as an Arko before
/// `mcp` saved it, resumes with those tools, called through the hub, and
/// the session says nothing is missing. A new session's conversation gets
/// `mcp` alone, and so does a new conversation of a session that keeps each
/// conversation's configuration (a branch).
#[tokio::test]
async fn a_resumed_conversation_keeps_its_direct_tools() {
    let temp = tempfile::tempdir().unwrap();
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    std::fs::create_dir_all(&project).unwrap();
    let script = temp.path().join("ping.py");
    std::fs::write(&script, PING_STUB).unwrap();
    let config = temp.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({"mcpServers": {"stub": {
            "command": crate::mcp::tests::python(),
            "args": [script],
        }}})
        .to_string(),
    )
    .unwrap();
    let ping = "mcp__stub__ping";
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("Call ping.") && !body.contains("pong") {
                    tool_call("c", "mcp__stub__ping", serde_json::json!({}))
                } else {
                    reply("a", "Done.")
                }
            })
        }
    }))
    .await;
    let open = |session: SessionChoice| {
        let running = spawn(
            Options {
                root: Some(root.clone()),
                session,
                workspace: Some(project.clone()),
                mcp_config: vec![config.clone()],
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                text_replies: Some(true),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        async move {
            let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            (seam, running.ended)
        }
    };
    let ask = |seam: &Seam, text: &str| {
        let count = bodies.lock().unwrap().len();
        seam.input.send(Input::Message(app_message(text))).unwrap();
        let bodies = bodies.clone();
        let feed = seam.feed.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while bodies.lock().unwrap().len() <= count || feed.activity().working {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            bodies.lock().unwrap().last().unwrap().clone()
        }
    };
    // A new session's conversation reaches the server through `mcp` alone.
    let (seam, ended) = open(SessionChoice::New).await;
    let id = seam.session.id.clone();
    let tools = offered(&ask(&seam, "First.").await);
    assert!(tools.contains(&"mcp".to_string()) && !tools.contains(&ping.to_string()));
    quit(seam, ended).await;
    to_direct_tools(&Session::open(&root, &id).unwrap().dir, ping);
    let branched = crate::sessions::branch(&root, &id).unwrap();

    for session in [id, branched] {
        let (seam, ended) = open(SessionChoice::Resume(session.clone())).await;
        assert_eq!(seam.details.mcp().unoffered, None, "{session}");
        let again = ask(&seam, "Again.").await;
        let tools = offered(&again);
        assert!(tools.contains(&ping.to_string()), "{tools:?}");
        assert!(!tools.contains(&"mcp".to_string()), "{tools:?}");
        // Its call goes through the hub.
        let called = ask(&seam, "Call ping.").await;
        assert!(called.contains("pong"), "{called}");
        assert_eq!(cached_parts(&called).0, cached_parts(&again).0);
        quit(seam, ended).await;
    }
}
