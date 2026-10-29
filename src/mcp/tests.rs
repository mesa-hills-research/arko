use super::*;

fn lookup(name: &str) -> Option<String> {
    match name {
        "TOKEN" => Some("secret".into()),
        "EMPTY" => Some(String::new()),
        _ => None,
    }
}

fn parsed(text: &str) -> Parsed {
    parse(text, Path::new("/x/.mcp.json"), &lookup).unwrap()
}

#[test]
fn both_formats_are_read() {
    let claude = parsed(
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["-y", "server-github"], "env": {"GITHUB_TOKEN": "${TOKEN}"}}}}"#,
    );
    assert!(claude.skipped.is_empty());
    let server = &claude.servers[0];
    assert_eq!(server.name, "github");
    assert_eq!(server.command, "npx");
    assert_eq!(server.args, ["-y", "server-github"]);
    assert_eq!(server.env["GITHUB_TOKEN"], "secret");
    assert_eq!(server.shown, "npx -y server-github");
    assert_eq!(server.file, Path::new("/x/.mcp.json"));

    let arko = parsed(
        r#"{"mcpServers": [{"name": "files", "command": "fs-server", "args": ["/tmp", 8080]}, {"command": "nameless"}]}"#,
    );
    assert_eq!(arko.servers.len(), 1);
    assert_eq!(arko.servers[0].name, "files");
    assert_eq!(arko.servers[0].args, ["/tmp", "8080"]);
    assert_eq!(arko.skipped, ["server 2 has no name"]);
}

#[test]
fn http_disabled_and_broken_servers_are_skipped() {
    let parsed = parsed(
        r#"{"mcpServers": {
            "remote": {"type": "http", "url": "https://example.com/mcp"},
            "bare": {"url": "https://example.com/sse"},
            "off": {"command": "x", "disabled": true},
            "unset": {"command": "x", "args": ["--token=${MISSING}"]},
            "empty": {"command": " "},
            "ok": {"type": "stdio", "command": "run", "args": ["${MISSING:-fallback}", "${EMPTY:-used}", "a${TOKEN}b"]}
        }}"#,
    );
    assert_eq!(parsed.servers.len(), 1);
    assert_eq!(parsed.servers[0].args, ["fallback", "used", "asecretb"]);
    let skipped = parsed.skipped.join("\n");
    assert!(skipped.contains("`remote` is a `http` server"), "{skipped}");
    assert!(skipped.contains("`bare` is an HTTP server"), "{skipped}");
    assert!(
        skipped.contains("`unset` uses ${MISSING}, which isn't set"),
        "{skipped}"
    );
    assert!(skipped.contains("`empty` has no `command`"), "{skipped}");
    assert!(!skipped.contains("off"), "{skipped}");
    assert!(parse("{}", Path::new("f"), &lookup).is_err());
    assert!(parse("not json", Path::new("f"), &lookup).is_err());
}

#[test]
fn variables_expand_and_text_without_them_stays() {
    assert_eq!(
        expand("plain $HOME {x}", &lookup).unwrap(),
        "plain $HOME {x}"
    );
    assert_eq!(expand("${TOKEN}${TOKEN}", &lookup).unwrap(), "secretsecret");
    assert_eq!(expand("${EMPTY}", &lookup).unwrap(), "");
    assert_eq!(expand("open ${TOKEN", &lookup).unwrap(), "open ${TOKEN");
}

#[test]
fn tool_names_are_ones_the_providers_take() {
    assert_eq!(
        tool_name("github", "create_issue"),
        "mcp__github__create_issue"
    );
    assert_eq!(
        tool_name("my server", "files.read"),
        "mcp__my_server__files_read"
    );
    let long = tool_name(
        "a-server-with-a-long-name",
        "a_tool_with_an_even_longer_name_than_that",
    );
    let other = tool_name(
        "a-server-with-a-long-name",
        "a_tool_with_an_even_longer_name_than_this",
    );
    assert_eq!(long.len(), 64);
    assert_ne!(long, other);
    // The same name every time: sessions keep tool names.
    assert_eq!(
        long,
        tool_name(
            "a-server-with-a-long-name",
            "a_tool_with_an_even_longer_name_than_that"
        )
    );
    let valid = |name: &str| {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    assert!(valid(&long) && valid(&tool_name("ü s", "ö.t")));
    assert_eq!(
        split_tool_name("mcp__github__create_issue"),
        Some(("github", "create_issue"))
    );
    assert_eq!(split_tool_name("shell"), None);
}

/// The files are read in order, a name is taken from the first file
/// that has it, and a project's servers start only when approved.
#[test]
fn servers_come_from_the_flag_the_project_and_fins_folder() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let workspace = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        root.join(USER_FILE),
        r#"{"mcpServers": {"shared": {"command": "user-shared"}, "mine": {"command": "user-mine"}}}"#,
    )
    .unwrap();
    let flag = temp.path().join("extra.json");
    std::fs::write(
        &flag,
        r#"{"mcpServers": {"shared": {"command": "flag-shared"}}}"#,
    )
    .unwrap();
    let (servers, notes) = servers_of(&root, &workspace, std::slice::from_ref(&flag));
    assert_eq!(servers, ["shared=flag-shared", "mine=user-mine"]);
    assert!(notes[0].contains("`shared`"), "{notes:?}");

    // A project's servers wait for an answer, which needs a terminal.
    let project = r#"{"mcpServers": {"proj": {"command": "project-server"}}}"#;
    std::fs::write(workspace.join(PROJECT_FILE), project).unwrap();
    let (servers, notes) = servers_of(&root, &workspace, &[]);
    // A file's servers by name come in the order of their names.
    assert_eq!(servers, ["mine=user-mine", "shared=user-shared"]);
    assert!(
        notes.iter().any(|note| note.contains("aren't started")),
        "{notes:?}"
    );

    // Approved, they start; changed, they wait again.
    let mut approvals = Approvals::default();
    approvals.projects.insert(
        workspace.display().to_string(),
        Approval {
            start: true,
            servers: serde_json::from_str::<Value>(project).unwrap()["mcpServers"].clone(),
        },
    );
    std::fs::write(
        root.join(APPROVALS_FILE),
        serde_json::to_string(&approvals).unwrap(),
    )
    .unwrap();
    let (servers, _) = servers_of(&root, &workspace, &[]);
    assert_eq!(
        servers,
        [
            "proj=project-server",
            "mine=user-mine",
            "shared=user-shared"
        ]
    );
    std::fs::write(
        workspace.join(PROJECT_FILE),
        r#"{"mcpServers": {"proj": {"command": "something-else"}}}"#,
    )
    .unwrap();
    let (servers, _) = servers_of(&root, &workspace, &[]);
    assert_eq!(servers, ["mine=user-mine", "shared=user-shared"]);
    // Named with --mcp-config, they start without an answer.
    let (servers, _) = servers_of(&root, &workspace, &[workspace.join(PROJECT_FILE)]);
    assert_eq!(
        servers,
        [
            "proj=something-else",
            "mine=user-mine",
            "shared=user-shared"
        ]
    );
    // A file --mcp-config names must be readable.
    let missing = temp.path().join("missing.json");
    assert!(plan(&root, &workspace, std::slice::from_ref(&missing), true).is_err());
    // An app's: its servers aren't started, and a note names it; the
    // others start.
    let (servers, notes) = plan(&root, &workspace, std::slice::from_ref(&missing), false)
        .unwrap()
        .decide(&[])
        .unwrap();
    assert_eq!(servers.len(), 2);
    assert!(
        notes
            .iter()
            .any(|note| note.contains(&missing.display().to_string())),
        "{notes:?}"
    );
}

/// The project's servers wait for the front end's answer: the plan
/// says what it would ask, the answer starts them or not, and is
/// remembered until the file's servers change. With no answer (no one
/// can ask), they don't start.
#[test]
fn project_mcp_servers_wait_for_the_prompters_answer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let workspace = temp.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join(PROJECT_FILE),
        r#"{"mcpServers": {"proj": {"command": "project-server", "args": ["--fast"]}, "web": {"url": "https://example.com"}}}"#,
    )
    .unwrap();
    let names = |servers: &[Server]| {
        servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>()
    };
    let asked = plan(&root, &workspace, &[], true).unwrap();
    assert_eq!(asked.approvals.len(), 1);
    let approval = asked.approvals[0].clone();
    assert_eq!(approval.file, workspace.join(PROJECT_FILE));
    assert_eq!(
        approval.servers,
        [("proj".to_string(), "project-server --fast".to_string())]
    );
    assert_eq!(approval.skipped.len(), 1, "{approval:?}");
    // No one to ask: they don't start, and nothing is remembered.
    let (servers, notes) = asked.decide(&[None]).unwrap();
    assert!(servers.is_empty());
    assert!(
        notes.iter().any(|note| note.contains("aren't started")),
        "{notes:?}"
    );
    assert!(plan(&root, &workspace, &[], true).unwrap().approvals.len() == 1);
    // Yes: they start, and the next session doesn't ask.
    let (servers, _) = plan(&root, &workspace, &[], true)
        .unwrap()
        .decide(&[Some(true)])
        .unwrap();
    assert_eq!(names(&servers), ["proj"]);
    let remembered = plan(&root, &workspace, &[], true).unwrap();
    assert!(remembered.approvals.is_empty());
    assert_eq!(names(&remembered.decide(&[]).unwrap().0), ["proj"]);
    // Changed, it asks again, and no is remembered too.
    std::fs::write(
        workspace.join(PROJECT_FILE),
        r#"{"mcpServers": {"proj": {"command": "other-server"}}}"#,
    )
    .unwrap();
    let changed = plan(&root, &workspace, &[], true).unwrap();
    assert_eq!(changed.approvals.len(), 1);
    assert!(changed.decide(&[Some(false)]).unwrap().0.is_empty());
    let refused = plan(&root, &workspace, &[], true).unwrap();
    assert!(refused.approvals.is_empty());
    assert!(refused.decide(&[]).unwrap().0.is_empty());
}

fn servers_of(root: &Path, workspace: &Path, files: &[PathBuf]) -> (Vec<String>, Vec<String>) {
    let (servers, notes) = plan(root, workspace, files, true)
        .unwrap()
        .decide(&[])
        .unwrap();
    (
        servers
            .into_iter()
            .map(|server| format!("{}={}", server.name, server.command))
            .collect(),
        notes,
    )
}

/// The Python interpreter the stub servers run on (`ARKO_TEST_PYTHON`).
pub(crate) fn python() -> String {
    std::env::var("ARKO_TEST_PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into())
}

/// The stub server in `dir` as `name`, set up in no file.
pub(crate) fn stub_server(dir: &Path, name: &str) -> Server {
    let script = dir.join("stub.py");
    std::fs::write(&script, STUB_SERVER).unwrap();
    Server {
        name: name.into(),
        command: python(),
        args: vec![script.display().to_string()],
        env: BTreeMap::from([("STUB_PREFIX".into(), "echo".into())]),
        written_env: BTreeMap::from([("STUB_PREFIX".into(), "echo".into())]),
        shown: String::new(),
        file: PathBuf::new(),
        workers: true,
        shared: false,
    }
}

/// A stub server over standard input and output (Python's standard
/// library) starts, and the agents call its tools through `mcp`. One that
/// fails is reported with why. The main agent's stop shows in the
/// transcript and in the server's state, and the tool's definition doesn't
/// change while the servers do.
#[tokio::test]
async fn a_server_starts_and_its_tools_are_called_and_one_that_fails_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let stub = stub_server(temp.path(), "stub server");
    let missing = Server {
        name: "missing".into(),
        command: "arko-no-such-mcp-server".into(),
        ..stub.clone()
    };
    let servers = Servers::start(vec![stub, missing], None).await;
    assert_eq!(servers.running(), ["stub server"]);
    assert_eq!(
        servers.tool_names("stub server").unwrap(),
        ["echo", "write.note"]
    );
    match servers.state("missing") {
        Some(McpState::Failed(reason)) => {
            assert!(reason.contains("arko-no-such-mcp-server"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        servers.summary().unwrap(),
        "MCP servers stub server (2 tools), missing (failed)"
    );
    let feed = crate::remote::Feed::new(crate::agents::MAIN);
    let notices = changes::Notices::new(
        Arc::new(crate::ui::Output::new(None, true)),
        Some(feed.clone()),
    );
    let access: Arc<dyn McpAccess> = Arc::new(arko_agents::mcp::AllServers);
    let lead = Bridge::lead(
        &servers,
        access.clone(),
        Arc::new(changes::NoChanges),
        Some(notices),
    );
    let definition = lead.definition();
    assert_eq!(definition.name, "mcp");
    assert!(definition.description.ends_with(NOTE));
    assert!(!lead.uses_host_timeout(), "an add waits for the user");
    assert_eq!(lead.background_mode(), arko_agents::ToolBackground::Detach);
    let context = TaskContext::new(uuid::Uuid::new_v4(), "main", "test", temp.path());
    let call = |params: Value| {
        let params: std::collections::HashMap<String, Value> =
            serde_json::from_value(params).unwrap();
        params
    };
    let output = lead
        .execute(
            &context,
            call(serde_json::json!({"action": "call", "server": "stub server", "tool": "echo", "arguments": {"text": "hi"}})),
        )
        .await
        .unwrap();
    assert_eq!(output, "echo: hi");
    // A worker's `mcp` has no add.
    let worker = Bridge::worker(&servers, access);
    assert!(
        !worker
            .definition()
            .raw_json_schema_override
            .unwrap()
            .to_string()
            .contains("\"add\"")
    );
    // The main agent's stop is said and kept.
    lead.execute(
        &context,
        call(serde_json::json!({"action": "stop", "server": "stub server"})),
    )
    .await
    .unwrap();
    assert_eq!(servers.state("stub server"), Some(McpState::StoppedByAgent));
    let said = |feed: &crate::remote::Feed| {
        feed.subscribe()
            .0
            .items
            .iter()
            .map(|item| item.text.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        said(&feed).contains(&"The main agent stopped the MCP server stub server.".to_string())
    );
    lead.execute(
        &context,
        call(serde_json::json!({"action": "start", "server": "stub server"})),
    )
    .await
    .unwrap();
    assert_eq!(servers.state("stub server"), Some(McpState::Running));
    assert!(
        said(&feed).contains(&"The main agent started the MCP server stub server.".to_string())
    );
    let json = |definition: ToolDefinition| {
        serde_json::json!({
            "name": definition.name,
            "description": definition.description,
            "schema": definition.raw_json_schema_override,
        })
    };
    assert_eq!(json(lead.definition()), json(definition.clone()));
    assert_eq!(json(Bridge::definition_of(Role::Lead)), json(definition));
    servers.shutdown().await;
}

/// A tool kept for a saved conversation answers a call by saying its
/// server isn't running, and why.
#[tokio::test]
async fn a_tool_of_a_server_not_started_says_so() {
    let context = TaskContext::new(uuid::Uuid::new_v4(), "main", "test", Path::new("."));
    let said = async |tool: UnstartedTool| {
        assert_eq!(tool.definition().name, "mcp__stub__ping");
        let error = tool.execute(&context, Default::default()).await;
        error.unwrap_err().to_string()
    };
    let chat = said(UnstartedTool::in_chat("mcp__stub__ping".into())).await;
    assert!(chat.contains("a chat starts none"), "{chat}");
    let gone = said(UnstartedTool::not_running("mcp__stub__ping".into())).await;
    assert!(
        gone.contains(
            "The MCP server stub isn't running. It's no longer set up, or it failed to start."
        ),
        "{gone}"
    );
}

/// The files are read in this order: the given ones, the project's
/// `.arko/mcp.json`, its `.mcp.json`, then Arko's `mcp.json`. The first to
/// name a server wins, and the two project files are asked about in that
/// order.
#[test]
fn the_files_are_read_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let workspace = temp.path().join("project");
    std::fs::create_dir_all(crate::arko_dir::dir(&workspace)).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let file = |path: &Path, unique: &str, command: &str| {
        std::fs::write(
            path,
            serde_json::json!({"mcpServers": {
                "same": {"command": command},
                unique: {"command": command},
            }})
            .to_string(),
        )
        .unwrap();
    };
    let flag = temp.path().join("flag.json");
    file(&flag, "f", "flag");
    file(&local_file(&workspace), "l", "local");
    file(&workspace.join(PROJECT_FILE), "p", "project");
    file(&root.join(USER_FILE), "u", "user");
    let asked = plan(&root, &workspace, std::slice::from_ref(&flag), true).unwrap();
    let files: Vec<PathBuf> = asked
        .approvals
        .iter()
        .map(|approval| approval.file.clone())
        .collect();
    assert_eq!(
        files,
        [local_file(&workspace), workspace.join(PROJECT_FILE)]
    );
    let (servers, notes) = asked.decide(&[Some(true), Some(true)]).unwrap();
    let started: Vec<String> = servers
        .iter()
        .map(|server| format!("{}={}", server.name, server.command))
        .collect();
    assert_eq!(
        started,
        ["f=flag", "same=flag", "l=local", "p=project", "u=user"]
    );
    assert_eq!(
        notes.iter().filter(|note| note.contains("`same`")).count(),
        3,
        "{notes:?}"
    );
    // Remembered, neither asks again.
    let (servers, _) = servers_of(&root, &workspace, std::slice::from_ref(&flag));
    assert_eq!(servers.len(), 5);
}

/// A project's `.arko/mcp.json` waits for an answer when Arko doesn't
/// remember approving it, as `.mcp.json` does, in its own `local` map. Once
/// approved it starts without a question, until its servers change.
#[test]
fn a_projects_arko_file_is_asked_about_when_unknown_and_started_when_remembered() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let workspace = temp.path().join("project");
    std::fs::create_dir_all(crate::arko_dir::dir(&workspace)).unwrap();
    let local = local_file(&workspace);
    std::fs::write(
        &local,
        r#"{"mcpServers": {"committed": {"command": "someone-elses-server"}}}"#,
    )
    .unwrap();
    assert_eq!(
        crate::extensions::local_mcp_approval(&root, &workspace),
        Ok(None)
    );
    let asked = plan(&root, &workspace, &[], true).unwrap();
    assert_eq!(asked.approvals.len(), 1);
    assert_eq!(asked.approvals[0].file, local);
    assert_eq!(
        asked.approvals[0].servers,
        [("committed".to_string(), "someone-elses-server".to_string())]
    );
    // No one to ask: it doesn't start.
    let (servers, notes) = asked.decide(&[None]).unwrap();
    assert!(servers.is_empty());
    assert!(
        notes
            .iter()
            .any(|note| note.contains(".arko") && note.contains("aren't started")),
        "{notes:?}"
    );
    // Approved: it starts, and the next session doesn't ask.
    let (servers, _) = plan(&root, &workspace, &[], true)
        .unwrap()
        .decide(&[Some(true)])
        .unwrap();
    assert_eq!(servers[0].name, "committed");
    let approvals: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(APPROVALS_FILE)).unwrap()).unwrap();
    assert_eq!(approvals["local"].as_object().unwrap().len(), 1);
    assert!(approvals["projects"].as_object().unwrap().is_empty());
    let remembered = plan(&root, &workspace, &[], true).unwrap();
    assert!(remembered.approvals.is_empty());
    assert_eq!(remembered.decide(&[]).unwrap().0.len(), 1);
    assert_eq!(
        crate::extensions::local_mcp_approval(&root, &workspace),
        Ok(Some(true))
    );
    // The project's `.mcp.json` answer is its own.
    assert_eq!(
        crate::extensions::project_mcp_approval(&root, &workspace),
        Ok(None)
    );
    // Changed, it asks again. Forgotten, too.
    std::fs::write(
        &local,
        r#"{"mcpServers": {"committed": {"command": "changed-server"}}}"#,
    )
    .unwrap();
    assert_eq!(
        plan(&root, &workspace, &[], true).unwrap().approvals.len(),
        1
    );
    assert_eq!(
        crate::extensions::forget_local_mcp_approval(&root, &workspace),
        Ok(true)
    );
    assert_eq!(
        crate::extensions::forget_local_mcp_approval(&root, &workspace),
        Ok(false)
    );
}

/// Arko's `workers` and `shared` fields are read in any file, and a value
/// that isn't true or false skips the server.
#[test]
fn workers_and_shared_are_read_in_any_file() {
    let parsed = parsed(
        r#"{"mcpServers": {
            "lead": {"command": "a", "workers": false},
            "both": {"command": "b", "shared": true},
            "plain": {"command": "c"},
            "odd": {"command": "d", "workers": "no"}
        }}"#,
    );
    let flags: Vec<(&str, bool, bool)> = parsed
        .servers
        .iter()
        .map(|server| (server.name.as_str(), server.workers, server.shared))
        .collect();
    assert_eq!(
        flags,
        [
            ("both", true, true),
            ("lead", false, false),
            ("plain", true, false)
        ]
    );
    assert!(
        parsed.skipped[0].contains("`odd` has a `workers` that isn't true or false"),
        "{:?}",
        parsed.skipped
    );
    let config = parsed.servers[1].config(Some("Why.".into()));
    assert!(!config.workers && !config.shared);
    assert_eq!(config.description.as_deref(), Some("Why."));
}

/// An entry is written in Claude Code's form, keeping the file's other
/// servers and fields, and a file that lists its servers keeps its list.
/// Taking it out keeps the rest too.
#[test]
fn an_entry_is_written_and_taken_out_keeping_the_rest_of_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mcp.json");
    let entry = serde_json::json!({"command": "npx", "env": {"TOKEN": "${TOKEN}"}});
    // A new file.
    let (before, after) = write_entry(&path, "github", entry.clone()).unwrap();
    assert_eq!(before, serde_json::json!({}));
    assert_eq!(after, serde_json::json!({"github": entry}));
    // A file with other servers and fields of its own.
    std::fs::write(
        &path,
        r#"{"theme": "dark", "mcpServers": {"mine": {"command": "mine", "custom": 1}}}"#,
    )
    .unwrap();
    write_entry(&path, "github", entry.clone()).unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::json!({"theme": "dark", "mcpServers": {
            "mine": {"command": "mine", "custom": 1},
            "github": entry,
        }})
    );
    assert!(sets_up(&path, "github") && sets_up(&path, "mine"));
    assert_eq!(
        remove_entry(&path, "github")
            .unwrap()
            .map(|(_, after)| after),
        Some(serde_json::json!({"mine": {"command": "mine", "custom": 1}}))
    );
    assert_eq!(remove_entry(&path, "github").unwrap(), None);
    // A list stays a list, the entry named in it.
    std::fs::write(
        &path,
        r#"{"mcpServers": [{"name": "mine", "command": "mine"}]}"#,
    )
    .unwrap();
    write_entry(&path, "github", entry.clone()).unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        written["mcpServers"],
        serde_json::json!([
            {"name": "mine", "command": "mine"},
            {"name": "github", "command": "npx", "env": {"TOKEN": "${TOKEN}"}},
        ])
    );
    // A file that isn't JSON is left alone.
    std::fs::write(&path, "not json").unwrap();
    assert!(write_entry(&path, "github", entry).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
}

const STUB_SERVER: &str = r#"
import json, os, sys
TOOLS = [
    {"name": "echo", "description": "Echoes text.", "annotations": {"readOnlyHint": True},
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "write.note", "description": "Writes a note.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
]
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
              "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "1"}})
    elif method == "tools/list":
        reply(message["id"], {"tools": TOOLS})
    elif method == "tools/call":
        text = message["params"].get("arguments", {}).get("text", "")
        reply(message["id"], {"content": [{"type": "text", "text": os.environ["STUB_PREFIX"] + ": " + text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                         "error": {"code": -32601, "message": "unknown"}}) + "\n")
        sys.stdout.flush()
"#;
