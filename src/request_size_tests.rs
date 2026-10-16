// What the model is sent: the main agent's first request, with text
// replies on and off, and a worker's, with every tool Arko offers (the
// shell, the terminal and the search index tools too, with one index
// named). The test is ignored: run it to measure the system prompt and
// the tool definitions, and to see the requests whole.
//
//     ARKO_REQUESTS=/some/dir cargo test --locked request_sizes -- --ignored --nocapture
//
// It writes `main-text.json`, `main-tool.json` and `worker.json` there,
// and prints their sizes. (Included in `main`'s tests, whose helpers it
// uses.)

use crate::mock_model::{ModelServer, complete, reply, tool_call};

/// The sizes of a request, in characters: its system prompt, its tool
/// definitions (as the JSON sent), and their text alone (the tools' and
/// their parameters' descriptions). And its tools' names.
fn sizes(body: &str) -> (usize, usize, usize, Vec<String>) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let prompt = body["instructions"].as_str().unwrap_or_default();
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
        .collect();
    let chars = |value: &serde_json::Value| value.as_str().map_or(0, |text| text.chars().count());
    let text = tools
        .iter()
        .map(|tool| {
            chars(&tool["description"])
                + tool["parameters"]["properties"]
                    .as_object()
                    .map_or(0, |properties| {
                        properties
                            .values()
                            .map(|property| chars(&property["description"]))
                            .sum()
                    })
        })
        .sum();
    let tools = serde_json::to_string(&tools).unwrap();
    (prompt.chars().count(), tools.chars().count(), text, names)
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "writes the requests to $ARKO_REQUESTS"]
async fn request_sizes() {
    let out = std::path::PathBuf::from(std::env::var("ARKO_REQUESTS").expect("ARKO_REQUESTS"));
    std::fs::create_dir_all(&out).unwrap();
    let mut table = Vec::new();
    for text_replies in [true, false] {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("You are agent run r1") {
                        complete("w1", "There are 3 files.")
                    } else if body.contains("Started r1") {
                        if text_replies {
                            reply("m2", "Started a worker.")
                        } else {
                            complete("m2", "Started a worker.")
                        }
                    } else {
                        tool_call(
                            "m1",
                            crew::START_AGENT,
                            serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                        )
                    }
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) =
            harness(temp.path(), server.chatgpt_client(), true);
        harness.settings.openai_auth = Some(providers::OpenAiAuth::ChatGpt);
        harness.settings.text_replies = text_replies;
        // The session's shell and terminal tools, and the search index
        // tools, with an index of the working directory.
        let shell = Shell::detect(WindowsShell::Cmd);
        let managers = Managers::open(&harness.session, &shell).unwrap();
        harness.tools = managers.tools(&shell, harness.handover.runs());
        let indexes =
            index::Indexes::open(&temp.path().join("indexes"), &harness.settings.workspace, None, false)
                .unwrap();
        harness.tools.extend(indexes.tools());
        let known = [index::Known {
            name: "work".into(),
            kind: "code",
            root: harness.settings.workspace.clone(),
            place: Some("the working directory"),
        }];
        let setup = providers::Setup::new(harness.settings.openai_auth);
        let settings = AgentSettings {
            main: &harness.settings.main,
            worker: &harness.settings.worker,
            providers: &setup,
            max_workers: harness.settings.max_workers,
            handover_at: harness.settings.handover_at,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: index::TOOL_NAMES,
            indexes: &known,
            text_replies,
            extra_rules: Default::default(),
        };
        let main_config = agents::main_config(&settings).unwrap();
        let worker_config = agents::worker_config(&settings).unwrap();
        harness.main_config = main_config;
        harness.worker_config = worker_config;
        harness.open(false).unwrap();
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("How many files?".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        let worker = async {
            loop {
                if let Some(body) = bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|body| body.contains("You are agent run r1"))
                {
                    return body.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let worker = tokio::time::timeout(Duration::from_secs(30), worker)
            .await
            .unwrap();
        let main = bodies.lock().unwrap()[0].clone();
        let name = if text_replies {
            "main-text"
        } else {
            "main-tool"
        };
        let mut requests = vec![(name, main)];
        if text_replies {
            requests.push(("worker", worker));
        }
        for (name, body) in requests {
            let pretty: serde_json::Value = serde_json::from_str(&body).unwrap();
            std::fs::write(
                out.join(format!("{name}.json")),
                serde_json::to_string_pretty(&pretty).unwrap(),
            )
            .unwrap();
            let (prompt, tools, text, names) = sizes(&body);
            table.push(format!(
                "{name}: system prompt {prompt}, tools {tools} (their text {text}), total {} ({} tools: {})",
                prompt + tools,
                names.len(),
                names.join(", ")
            ));
        }
        harness.crew.as_ref().unwrap().shutdown().await;
        indexes.shutdown().await;
    }
    println!("{}", table.join("\n"));
}
