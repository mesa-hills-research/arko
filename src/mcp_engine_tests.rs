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

/// Rewrites the session in `dir` as an Arko from before `mcp` and
/// `add_skill` saved it: each configuration that offers `mcp` offers
/// `direct` (a tool of a server's own) in its place, and none offers the
/// tools that save skills.
fn to_direct_tools(dir: &std::path::Path, direct: &str) {
    let mut replaced = 0;
    rewrite(dir, &mut |key, value| {
        let (true, serde_json::Value::Array(names)) = (key == "available_tool_names", value) else {
            return false;
        };
        let before = names.clone();
        names.retain(|name| {
            !crate::skill_install::TOOL_NAMES.contains(&name.as_str().unwrap_or_default())
        });
        for name in names.iter_mut() {
            if name == "mcp" {
                *name = direct.into();
                replaced += 1;
            }
        }
        *names != before
    });
    assert!(replaced > 0, "nothing in {} offers mcp", dir.display());
}

/// Changes each JSON file in `dir` with `change`, which is given each field
/// of each object, by key, and says whether it changed it. Says how many
/// files changed.
fn rewrite(
    dir: &std::path::Path,
    change: &mut dyn FnMut(&str, &mut serde_json::Value) -> bool,
) -> usize {
    fn visit(
        value: &mut serde_json::Value,
        change: &mut dyn FnMut(&str, &mut serde_json::Value) -> bool,
    ) -> bool {
        let mut changed = false;
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    changed |= change(key, value) | visit(value, change);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    changed |= visit(item, change);
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
        if visit(&mut value, change) {
            std::fs::write(&entry, value.to_string()).unwrap();
            changed += 1;
        }
    }
    changed
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

/// A conversation saved with a tool for each MCP tool and without
/// `add_skill`, as an Arko before `mcp` saved it, resumes with its tools and
/// instructions byte for byte, its calls going through the hub, and the
/// session says nothing of MCP is missing. A new session's conversation gets
/// `mcp` alone, and so does the next conversation (`/new`) of the resumed
/// session and of a branch of it, with `add_skill`.
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

    for session in [id.clone(), branched] {
        let (seam, ended) = open(SessionChoice::Resume(session.clone())).await;
        // (A branch's conversation is saved once it opens.)
        let note = seam.details.mcp().unoffered.unwrap_or_default();
        assert!(!note.contains("MCP"), "{session}: {note}");
        if session == id {
            assert!(note.contains("saving skills (add_skill)"), "{note}");
        }
        let again = ask(&seam, "Again.").await;
        let tools = offered(&again);
        assert!(tools.contains(&ping.to_string()), "{tools:?}");
        assert!(!tools.contains(&"mcp".to_string()), "{tools:?}");
        assert!(!tools.contains(&"add_skill".to_string()), "{tools:?}");
        // Its call goes through the hub.
        let called = ask(&seam, "Call ping.").await;
        assert!(called.contains("pong"), "{called}");
        assert_eq!(cached_parts(&called), cached_parts(&again));
        // The next conversation reaches the server through `mcp`, and saves
        // skills.
        seam.input
            .send(Input::Message(app_message("/new")))
            .unwrap();
        let fresh = offered(&ask(&seam, "Fresh.").await);
        for tool in ["mcp", "add_skill", "remove_skill"] {
            assert!(fresh.contains(&tool.to_string()), "{session}: {fresh:?}");
        }
        assert!(!fresh.contains(&ping.to_string()), "{fresh:?}");
        quit(seam, ended).await;
    }
}

/// The tools each worker's first request offers, by its handle, in a
/// session whose servers `config` sets up (none without it). The main agent
/// starts r1 with no `mcp`, and r2 with `mcp: none`.
async fn workers_offered(
    temp: &std::path::Path,
    config: Option<serde_json::Value>,
) -> std::collections::HashMap<String, Vec<String>> {
    std::fs::create_dir_all(temp).unwrap();
    let script = temp.join("ping.py");
    std::fs::write(&script, PING_STUB).unwrap();
    let files: Vec<PathBuf> = config
        .map(|mut config| {
            for entry in config["mcpServers"].as_object_mut().unwrap().values_mut() {
                entry["command"] = crate::mcp::tests::python().into();
                entry["args"] = serde_json::json!([script]);
            }
            let file = temp.join("mcp.json");
            std::fs::write(&file, config.to_string()).unwrap();
            file
        })
        .into_iter()
        .collect();
    let workers: Arc<Mutex<std::collections::HashMap<String, Vec<String>>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let workers = workers.clone();
        move |_, body: String| {
            let worker = ["r1", "r2"]
                .into_iter()
                .find(|handle| body.contains(&format!("You are agent run {handle}")));
            if let Some(handle) = worker {
                workers
                    .lock()
                    .unwrap()
                    .entry(handle.to_string())
                    .or_insert_with(|| offered(&body));
            }
            Box::pin(async move {
                if worker.is_some() {
                    complete("w", "Done.")
                } else if body.contains("call_m1") {
                    reply("m2", "Started them.")
                } else {
                    crate::mock_model::tool_calls(
                        "m1",
                        &[
                            (
                                crate::crew::START_AGENT,
                                serde_json::json!({"agent": crate::agents::WORKER, "task": "Look."}),
                            ),
                            (
                                crate::crew::START_AGENT,
                                serde_json::json!({"agent": crate::agents::WORKER, "task": "Look.", "mcp": "none"}),
                            ),
                        ],
                    )
                }
            })
        }
    }))
    .await;
    let (root, project) = (temp.join("arko"), temp.join("project"));
    std::fs::create_dir_all(&project).unwrap();
    let running = spawn(
        Options {
            root: Some(root),
            workspace: Some(project),
            mcp_config: files,
            model: Some("luna".into()),
            openai_auth: Some(OpenAiAuth::ApiKey),
            cache_1h: Some(false),
            text_replies: Some(true),
            worker_billing: Some(crate::billing::WorkerBilling::Api),
            client: Some(GivenClient(Arc::new(server.client()))),
            ..Options::default()
        },
        Arc::new(NoPrompter),
    );
    let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    seam.input
        .send(Input::Message(app_message("Start the workers.")))
        .unwrap();
    // Both workers report, and the main agent hears of it.
    let finished = |seam: &Seam| {
        seam.feed
            .subscribe()
            .0
            .workers
            .iter()
            .filter(|worker| worker.state == crate::remote::WorkerState::Finished)
            .count()
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        while workers.lock().unwrap().len() < 2 || finished(&seam) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    idle(&seam).await;
    quit(seam, running.ended).await;
    workers.lock().unwrap().clone()
}

/// A worker is offered `mcp` only when its set holds a server at its
/// start: one that workers may reach, and that its start doesn't leave out.
#[tokio::test]
async fn a_worker_gets_mcp_only_with_a_server_to_reach() {
    let has_mcp = |tools: &Vec<String>| tools.iter().any(|tool| tool == "mcp");
    let temp = tempfile::tempdir().unwrap();
    let with = workers_offered(
        &temp.path().join("with"),
        Some(serde_json::json!({"mcpServers": {"ping": {}}})),
    )
    .await;
    assert!(has_mcp(&with["r1"]), "{:?}", with["r1"]);
    assert!(!has_mcp(&with["r2"]), "{:?}", with["r2"]);
    let none = workers_offered(&temp.path().join("none"), None).await;
    assert!(!has_mcp(&none["r1"]) && !has_mcp(&none["r2"]), "{none:?}");
    let lead_only = workers_offered(
        &temp.path().join("lead"),
        Some(serde_json::json!({"mcpServers": {"ping": {"workers": false}}})),
    )
    .await;
    assert!(!has_mcp(&lead_only["r1"]), "{lead_only:?}");
    for offered in [&with, &none, &lead_only] {
        assert!(
            offered
                .values()
                .flatten()
                .all(|tool| !tool.starts_with("mcp__"))
        );
    }
}

/// A session saved before `mcp`, whose worker r1 was given only the server
/// `other`, resumes with r1's tools as they were. r1 goes on, and its call of
/// `stub`'s own tool is refused, since `stub` isn't one of its servers. Nor
/// can it load a skill kept to the main agent, although its configuration
/// was saved without saying it is a worker's. A new worker gets the
/// session's configuration now, with `mcp` and no tool of a server's own,
/// and can't load that skill either.
#[tokio::test]
async fn resumed_workers_keep_their_tools_and_their_limits() {
    let temp = tempfile::tempdir().unwrap();
    let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
    std::fs::create_dir_all(&project).unwrap();
    let script = temp.path().join("ping.py");
    std::fs::write(&script, PING_STUB).unwrap();
    let config = temp.path().join("mcp.json");
    let entry = serde_json::json!({"command": crate::mcp::tests::python(), "args": [script]});
    std::fs::write(
        &config,
        serde_json::json!({"mcpServers": {"stub": entry, "other": entry}}).to_string(),
    )
    .unwrap();
    // A skill kept to the main agent, and one for every agent.
    for (name, extra) in [("secret", "workers: false\n"), ("open", "")] {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} skill.\n{extra}---\n\nDo {name} things.\n"),
        )
        .unwrap();
    }
    let (ping, load) = ("mcp__stub__ping", arko_agents::LOAD_SKILL_TOOL_NAME);
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            let secret = serde_json::json!({"name": "secret"});
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    if body.contains("call_w2_1") {
                        complete("w3", "Checked.")
                    } else if body.contains("Check again.") {
                        crate::mock_model::tool_calls(
                            "w2",
                            &[(ping, serde_json::json!({})), (load, secret)],
                        )
                    } else {
                        complete("w1", "Counted.")
                    }
                } else if body.contains("You are agent run r2") {
                    if body.contains("call_n1") {
                        complete("n2", "Looked.")
                    } else {
                        tool_call("n1", load, secret)
                    }
                } else if body.contains("call_m3_1") {
                    reply("m4", "Asked them.")
                } else if body.contains("Go on.") {
                    crate::mock_model::tool_calls(
                        "m3",
                        &[
                            (
                                crate::crew::MESSAGE_AGENT,
                                serde_json::json!({"to": "r1", "message": "Check again."}),
                            ),
                            (
                                crate::crew::START_AGENT,
                                serde_json::json!({"agent": crate::agents::WORKER, "task": "Look."}),
                            ),
                        ],
                    )
                } else if body.contains("call_m1") {
                    reply("m2", "Started it.")
                } else {
                    tool_call(
                        "m1",
                        crate::crew::START_AGENT,
                        serde_json::json!({"agent": crate::agents::WORKER, "task": "Count.", "mcp": "other"}),
                    )
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
                worker_billing: Some(crate::billing::WorkerBilling::Api),
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
    // The body of the request of `run` with `text`, once there is one.
    let request = |run: &str, text: &str| {
        let (bodies, run, text) = (bodies.clone(), format!("You are agent run {run}"), text.to_string());
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let found = bodies
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|body| body.contains(&run) && body.contains(&text))
                        .cloned();
                    if let Some(found) = found {
                        return found;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap()
        }
    };
    let finished = |seam: &Seam, count: usize| {
        let seam_feed = seam.feed.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while seam_feed
                    .subscribe()
                    .0
                    .workers
                    .iter()
                    .filter(|worker| worker.state == crate::remote::WorkerState::Finished)
                    .count()
                    < count
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    let (seam, ended) = open(SessionChoice::New).await;
    let id = seam.session.id.clone();
    seam.input
        .send(Input::Message(app_message("Start a worker.")))
        .unwrap();
    let first = request("r1", "Count.").await;
    assert!(offered(&first).contains(&"mcp".to_string()), "{:?}", offered(&first));
    finished(&seam, 1).await;
    idle(&seam).await;
    quit(seam, ended).await;
    // As an Arko from before `mcp`, and before a worker's skills config said
    // it is a worker's.
    let dir = Session::open(&root, &id).unwrap().dir;
    to_direct_tools(&dir, ping);
    let changed = rewrite(&dir, &mut |key, value| {
        let worker = key == "subagent" && *value == serde_json::Value::Bool(true);
        if worker {
            *value = false.into();
        }
        worker
    });
    assert!(changed > 0);

    let (seam, ended) = open(SessionChoice::Resume(id)).await;
    seam.input.send(Input::Message(app_message("Go on."))).unwrap();
    let checked = request("r1", "call_w2_1").await;
    let tools = offered(&checked);
    assert!(tools.contains(&ping.to_string()) && !tools.contains(&"mcp".to_string()), "{tools:?}");
    assert!(checked.contains("stub isn't one of your MCP servers."), "{checked}");
    assert!(checked.contains("Skill 'secret' not found"), "{checked}");
    let fresh = request("r2", "Look.").await;
    let tools = offered(&fresh);
    assert!(tools.contains(&"mcp".to_string()), "{tools:?}");
    assert!(!tools.iter().any(|tool| tool.starts_with("mcp__")), "{tools:?}");
    let looked = request("r2", "call_n1").await;
    assert!(
        looked.contains("Skill 'secret' not found") && looked.contains("- open"),
        "{looked}"
    );
    // Both reports reach the main agent.
    tokio::time::timeout(Duration::from_secs(30), async {
        let reached = |text: &str| {
            bodies.lock().unwrap().iter().any(|body| {
                !body.contains("You are agent run") && body.contains(text)
            })
        };
        while !reached("Checked.") || !reached("Looked.") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    idle(&seam).await;
    quit(seam, ended).await;
}
