// End-to-end tests of workers that start workers, with the mock model: the
// main agent starts r1 with `nested`, r1 starts r2, and r2's report reaches
// r1. Also what a stop, a message, a resume and a cut-back do to the tree.
// (Included in `main`'s tests, whose helpers they use.)

use crate::mock_model::{ModelServer, complete, tool_call, tool_calls};

/// The requests the model server got: (the agent run that sent it, its
/// body).
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// The agent run that sent a request: `r1`, `r2`, or `main`.
fn sender(body: &str) -> String {
    let marker = "You are agent run ";
    match body.find(marker) {
        Some(at) => body[at + marker.len()..]
            .split(',')
            .next()
            .unwrap_or_default()
            .to_string(),
        None => "main".to_string(),
    }
}

/// How many tool results a request carries, which says how far its run has
/// got in the mock's script.
fn results(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The names of the tools a request offers.
fn tool_names(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// What a request says of the parameter `parameter` of its tool `tool`.
fn parameter_text(body: &str, tool: &str, parameter: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|offered| offered["name"] == tool)
        .and_then(|offered| offered["parameters"]["properties"][parameter]["description"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// A request's first user message.
fn first_message(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let item = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["role"] == "user")
        .unwrap();
    match &item["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// All the text of the user messages and tool results a request carries.
fn read(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| match (&item["content"], &item["output"]) {
            (serde_json::Value::String(text), _) => text.clone(),
            (serde_json::Value::Array(parts), _) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            (_, serde_json::Value::String(text)) => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the three agents of a tree do, and when. The main agent starts r1
/// (with `nested` when `r1_nests`), which starts r2, asks to report at once,
/// waits for r2, and reports with what r2 found. r2 works until `r2_may_go`
/// is opened, or never when it is not.
struct Script {
    /// r2 reports once this is opened (by r1's request after its refused
    /// report), or never.
    r2_may_go: Arc<tokio::sync::Notify>,
    /// Opened by r1's second request, so the test knows the refusal came.
    refused: Arc<tokio::sync::Notify>,
    /// r2 never reports: it stays on its request. (A test lets it go.)
    r2_stays: Arc<std::sync::atomic::AtomicBool>,
    /// What r1's start call carries besides the agent and the task.
    r1_start: serde_json::Value,
    /// What r1 asks of r2's start besides the agent and the task.
    r2_start: serde_json::Value,
    /// r1 reports at once and starts nothing.
    r1_plain: bool,
    /// r2 starts a monitor of a command that runs on, before it waits.
    r2_monitors: bool,
    /// r1's first response starts this many workers at once, in worktrees
    /// of their own when the flag is set, and every worker stays on its
    /// request.
    r1_together: Option<(usize, bool)>,
    /// The main agent's turn goes on after it started r1, until the user
    /// stops it, and says when it is there.
    main_waits: Option<Arc<tokio::sync::Notify>>,
    seen: Seen,
}

impl Script {
    fn new(r2_stays: bool) -> Self {
        Self {
            r2_may_go: Arc::default(),
            refused: Arc::default(),
            r2_stays: Arc::new(std::sync::atomic::AtomicBool::new(r2_stays)),
            r1_start: serde_json::json!({"nested": true}),
            r2_start: serde_json::json!({}),
            r1_plain: false,
            r2_monitors: false,
            r1_together: None,
            main_waits: None,
            seen: Arc::default(),
        }
    }

    async fn serve(&self) -> ModelServer {
        let (seen, gate, refused) = (
            self.seen.clone(),
            self.r2_may_go.clone(),
            self.refused.clone(),
        );
        let (stays, r1_start, r2_start) = (
            self.r2_stays.clone(),
            self.r1_start.clone(),
            self.r2_start.clone(),
        );
        let plain = self.r1_plain;
        let main_waits = self.main_waits.clone();
        let monitors = self.r2_monitors;
        let together = self.r1_together;
        ModelServer::start(Arc::new(move |index, body: String| {
            let who = sender(&body);
            seen.lock().unwrap().push((who.clone(), body.clone()));
            let (gate, refused, stays) = (gate.clone(), refused.clone(), stays.clone());
            let (r1_start, r2_start) = (r1_start.clone(), r2_start.clone());
            let main_waits = main_waits.clone();
            Box::pin(async move {
                let done = results(&body);
                let response = match who.as_str() {
                    // The main agent's turn goes on until the user stops it.
                    "main"
                        if !done.is_empty()
                            && main_waits.is_some()
                            && !body.contains("Fresh lead") =>
                    {
                        main_waits.unwrap().notify_one();
                        return std::future::pending::<String>().await;
                    }
                    "main" if body.contains("Fresh lead") && done.is_empty() => tool_call(
                        &format!("m{index}"),
                        crew::LIST_AGENTS,
                        serde_json::json!({}),
                    ),
                    "main" if body.contains("Fresh lead") => {
                        complete(&format!("m{index}"), "Listed.")
                    }
                    "main" if done.is_empty() => {
                        let mut call = serde_json::json!({
                            "agent": WORKER,
                            "task": "Coordinate the review of the files.",
                        });
                        for (name, value) in r1_start.as_object().unwrap() {
                            call[name] = value.clone();
                        }
                        tool_call(&format!("m{index}"), crew::START_AGENT, call)
                    }
                    "main" => complete(&format!("m{index}"), "Started."),
                    "r1" if together.is_some() && done.is_empty() => {
                        let (count, isolated) = together.unwrap();
                        let calls: Vec<(&str, serde_json::Value)> = (1..=count)
                            .map(|part| {
                                (
                                    crew::START_AGENT,
                                    serde_json::json!({
                                        "agent": WORKER,
                                        "task": format!("Part {part}."),
                                        "isolated": isolated,
                                    }),
                                )
                            })
                            .collect();
                        tool_calls(&format!("a{index}"), &calls)
                    }
                    // Everyone waits, so the workers stay running.
                    _ if together.is_some() && who != "main" => {
                        return std::future::pending::<String>().await;
                    }
                    "r1" if plain => complete(&format!("a{index}"), "Nothing to split."),
                    "r1" => match done.len() {
                        0 => {
                            let mut call = serde_json::json!({
                                "agent": WORKER,
                                "task": "Count the files in src.",
                            });
                            for (name, value) in r2_start.as_object().unwrap() {
                                call[name] = value.clone();
                            }
                            tool_call(&format!("a{index}"), crew::START_AGENT, call)
                        }
                        // Reports at once, while r2 works.
                        1 => complete(&format!("a{index}"), "Done early."),
                        2 => {
                            refused.notify_one();
                            gate.notify_one();
                            tool_call(
                                &format!("a{index}"),
                                crew::WAIT_FOR_AGENTS,
                                serde_json::json!({}),
                            )
                        }
                        // Waits again until r2's report is there (a wait
                        // that was cut short by Arko closing says so).
                        _ if !done.last().unwrap().contains("Counted 41 files.") => tool_call(
                            &format!("a{index}"),
                            crew::WAIT_FOR_AGENTS,
                            serde_json::json!({}),
                        ),
                        _ => complete(&format!("a{index}"), "Reviewed. r2 said: Counted 41 files."),
                    },
                    "r2" if monitors && done.is_empty() => tool_call(
                        &format!("b{index}"),
                        "monitor_start",
                        serde_json::json!({"command": "sleep 60", "pattern": "ERROR"}),
                    ),
                    "r2" => {
                        if stays.load(std::sync::atomic::Ordering::SeqCst) {
                            return std::future::pending::<String>().await;
                        }
                        gate.notified().await;
                        complete(&format!("b{index}"), "Counted 41 files.")
                    }
                    other => panic!("an unexpected agent {other}"),
                };
                // The workers' requests are big enough to cost something.
                match who.as_str() {
                    "main" => response,
                    _ => crate::mock_model::with_input(response, 50_000),
                }
            })
        }))
        .await
    }

    /// The requests an agent run sent.
    fn requests(&self, who: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(sender, _)| sender == who)
            .map(|(_, body)| body.clone())
            .collect()
    }
}

/// A session over `server`, as one is once it is open.
fn open_session(
    temp: &Path,
    server: &ModelServer,
) -> (Harness, Inputs, mpsc::UnboundedSender<Input>) {
    let opened = harness(temp, server.client(), true);
    opened.0.start_new_workers();
    opened
}

async fn turn(harness: &mut Harness, inputs: &mut Inputs, message: &str) {
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User(message.into()), inputs),
    )
    .await
    .expect("the turn ended in time");
    assert!(matches!(end, TurnEnd::Answered));
}

/// The main agent starts r1 with `nested`, r1 starts r2, and r2's report
/// reaches r1 and not the main agent. r1 can't report while r2 runs, and
/// is told what to do. The feed knows the family.
#[tokio::test]
async fn a_worker_started_to_nest_starts_a_worker_whose_report_reaches_it() {
    let script = Script::new(false);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    // r1 is waiting for r2 when its report is refused and r2 is let go.
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;

    // r1 was told about its own workers in its first message, and offered
    // the tools to start them.
    let r1 = script.requests("r1");
    assert!(
        first_message(&r1[0])
            .contains("# Workers of your own\nYou can start workers with `start_agent`.")
    );
    let offered = tool_names(&r1[0]);
    for tool in crew::NESTED_TOOL_NAMES
        .iter()
        .chain(crew::WORKER_TOOL_NAMES)
    {
        assert!(offered.iter().any(|name| name == tool), "r1 lacks {tool}");
    }
    assert!(!offered.iter().any(|name| name == crew::SAVE_WORKER_PROFILE));
    // The report it gave too early was refused with what to do.
    let refusal = &results(&r1[2])[1];
    assert_eq!(
        refusal,
        "Your agent run r2 (running) hasn't finished. Wait for it with wait_for_agents, or stop it with stop_agent, then report again."
    );
    // r2's report reached r1, and r1's reached the main agent, which never
    // read r2's.
    assert!(read(r1.last().unwrap()).contains("Counted 41 files."));
    let main = script.requests("main");
    assert!(
        main.iter()
            .any(|body| read(body).contains("Reviewed. r2 said:")),
        "the main agent read r1's report"
    );
    for body in &main {
        let text = read(body).replace("Reviewed. r2 said: Counted 41 files.", "");
        assert!(
            !text.contains("Counted 41 files."),
            "the main agent read r2's report: {text}"
        );
    }

    // r1's wait names the run it waits for, though the main agent has
    // others going. (The feed names the runs of the agent that waits.)
    assert!(
        shown(&harness.feed)
            .iter()
            .any(|(kind, text, _)| kind == "call" && text == "waiting for r2"),
        "{:?}",
        shown(&harness.feed)
    );
    // r2's line is in r1's transcript, and r1's in the main one.
    let lines: Vec<(String, Option<String>)> = harness
        .feed
        .subscribe()
        .0
        .items
        .iter()
        .filter(|item| item.kind == remote::Kind::WorkerLine)
        .map(|item| (item.agent.clone(), item.worker.clone()))
        .collect();
    assert_eq!(
        lines,
        [
            ("main".to_string(), Some("r1".to_string())),
            ("r1".to_string(), Some("r2".to_string()))
        ]
    );
    // /workers lists r2 after r1 and says who started it.
    let described = harness.crew.as_ref().unwrap().describe();
    assert!(
        described.contains("r2  finished  Count the files in src.\n  started by r1\n"),
        "{described}"
    );

    // The clients know who started whom.
    let summaries = harness.feed.worker_summaries();
    let summary = |id: &str| summaries.iter().find(|summary| summary.id == id).unwrap();
    assert_eq!(
        (summary("r1").parent.as_deref(), summary("r1").depth),
        (None, 1)
    );
    assert_eq!(
        (summary("r2").parent.as_deref(), summary("r2").depth),
        (Some("r1"), 2)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker is offered the start tools only when its start said `nested`
/// and the user's depth lets it start workers, so r2 (at the last level)
/// and a worker started plainly are not.
#[tokio::test]
async fn only_a_worker_started_to_nest_below_the_depth_is_offered_the_start_tools() {
    let tools = |script: &Script, who: &str| tool_names(&script.requests(who)[0]);
    let starts = |tools: &[String]| {
        crew::NESTED_TOOL_NAMES
            .iter()
            .filter(|name| tools.iter().any(|tool| tool == *name))
            .count()
    };

    // A worker started with no `nested` has `message_agent` and
    // `wait_for_agents` and none of the start tools.
    let mut plain = Script::new(false);
    plain.r1_start = serde_json::json!({});
    plain.r1_plain = true;
    let server = plain.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    let r1 = tools(&plain, "r1");
    assert_eq!(starts(&r1), 0, "{r1:?}");
    for tool in crew::WORKER_TOOL_NAMES {
        assert!(r1.iter().any(|name| name == tool), "{tool}");
    }
    assert!(!first_message(&plain.requests("r1")[0]).contains("Workers of your own"));
    // Its `message_agent` reaches its lead and its peers, not its workers.
    assert_eq!(
        parameter_text(&plain.requests("r1")[0], crew::MESSAGE_AGENT, "to"),
        "`lead` for the agent that started you, or the run id of another agent run your lead linked you with."
    );
    harness.crew.as_ref().unwrap().shutdown().await;

    // One started with `nested` has them, and the worker it starts, at
    // the second level, has none.
    let nesting = Script::new(false);
    let server = nesting.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    assert_eq!(
        starts(&tools(&nesting, "r1")),
        crew::NESTED_TOOL_NAMES.len()
    );
    assert!(
        parameter_text(&nesting.requests("r1")[0], crew::MESSAGE_AGENT, "to")
            .starts_with("The run id of one of your agent runs"),
    );
    assert!(
        parameter_text(&nesting.requests("r2")[0], crew::MESSAGE_AGENT, "to")
            .starts_with("`lead` for the agent that started you"),
    );
    let r2 = tools(&nesting, "r2");
    assert_eq!(starts(&r2), 0, "{r2:?}");
    assert!(!first_message(&nesting.requests("r2")[0]).contains("Workers of your own"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `nested` is refused, with the reason and what to do, when every worker
/// runs the session's model and when the user allows one level.
#[tokio::test]
async fn nested_is_refused_when_the_session_does_not_let_workers_nest() {
    let mut session = Script::new(false);
    session.r1_plain = true;
    let server = session.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    harness.settings.worker_models = crate::agents::ModelChoice::Session;
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let refusal = &results(&session.requests("main")[1])[0];
    assert!(
        refusal.ends_with("The user has every worker run the session's worker model (/worker-models session), and no worker starts workers then. Start the worker without nested."),
        "{refusal}"
    );
    assert!(session.requests("r1").is_empty());
    harness.crew.as_ref().unwrap().shutdown().await;

    // One level: the main agent's `nested` is refused with the reason.
    let one = Script::new(false);
    let server = one.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    harness.settings.worker_depth = 1;
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let refusal = &results(&one.requests("main")[1])[0];
    assert!(
        refusal.ends_with("A worker started here is at level 1 and can't start workers of its own, because the user allows 1 level of workers (/worker-depth). Start it without nested, or ask the user to allow more levels."),
        "{refusal}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker at the second level that asks for `nested` for its own worker
/// is refused, with the reason and what to do, when the depth is 2.
#[tokio::test]
async fn a_worker_cant_start_a_worker_that_nests_past_the_depth() {
    let mut script = Script::new(false);
    script.r2_start = serde_json::json!({"nested": true});
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    // (r1 read the refusal as the result of its start, then reported.)
    let refusal = &results(&script.requests("r1")[1])[0];
    assert!(
        refusal.ends_with("A worker started here is at level 2 and can't start workers of its own, because the user allows 2 levels of workers (/worker-depth). Start it without nested, or ask the user to allow more levels."),
        "{refusal}"
    );
    assert!(script.requests("r2").is_empty(), "no r2 started");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// Stopping r1 stops r2 with it, the command says so, and the message the
/// user sends r2 reaches r2 and is copied to r1, which started it.
#[tokio::test]
async fn stopping_a_worker_stops_its_workers_and_a_message_to_one_is_copied_to_its_starter() {
    #[derive(Clone, Default)]
    struct Printed(Arc<Mutex<Vec<String>>>);
    impl ui::Printer for Printed {
        fn print(&mut self, text: String) -> Result<(), ()> {
            self.0
                .lock()
                .unwrap()
                .extend(text.lines().map(str::to_string));
            Ok(())
        }
    }
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"] && script.requests("r2").len() == 1).await;

    // The user's message to r2 reaches r2, and r1 (which started it) gets
    // a copy it knows is its own.
    assert_eq!(crew.message("r2", "Skip the tests folder.").unwrap(), "r2");
    let workflow = harness.workflow().clone();
    let r1 = workflow
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap();
    let news = workflow.pending_news(r1.run_id);
    assert!(
        news.iter().any(|item| item.text().contains(
            "The user sent r2 a message directly (a copy for you, since you started r2):\n\nSkip the tests folder."
        )),
        "{:?}",
        news.iter().map(|item| item.text()).collect::<Vec<_>>()
    );
    // A message to r1 is copied to the main agent as before.
    assert_eq!(crew.message("r1", "Hurry.").unwrap(), "r1");
    let main = workflow
        .subagents()
        .into_iter()
        .find(|run| run.handle == "r1")
        .unwrap()
        .caller_run_id;
    assert!(workflow.pending_news(main).iter().any(|item| {
        item.text()
            .contains("The user sent r1 a message directly (a copy for you):")
    }));

    // The command stops r1 and names r2.
    let printed = Printed::default();
    harness.output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
    harness.command("/stop-workers r1");
    assert_eq!(
        printed.0.lock().unwrap().last().unwrap(),
        "Stopping r1. Its own worker r2 stops with it. A message from the agent resumes r1."
    );
    until(|| crew.running().is_empty()).await;
    let states: Vec<(String, remote::WorkerState)> = harness
        .feed
        .worker_summaries()
        .into_iter()
        .map(|summary| (summary.id, summary.state))
        .collect();
    assert_eq!(
        states,
        [
            ("r1".to_string(), remote::WorkerState::Stopped),
            ("r2".to_string(), remote::WorkerState::Stopped)
        ]
    );
    crew.shutdown().await;
}

/// A session saved while a tree runs resumes: the workflow's check of the
/// saved runs passes though the profile that made r1 a worker that nests
/// was edited and then deleted, both workers go on where they were with
/// the setup they started with, and r2's report reaches r1 and r1's the
/// main agent.
#[tokio::test]
async fn a_tree_that_was_running_resumes_with_the_setups_it_started_with() {
    let mut script = Script::new(true);
    script.r1_start = serde_json::json!({"profile": "coordinator"});
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    let folder = harness.session.dir.join("profiles");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("coordinator.md");
    std::fs::write(
        &file,
        "---\ndescription: Splits a review\nmodel: sol\neffort: medium\nnested: true\n---\nCoordinate it, version one.\n",
    )
    .unwrap();
    turn(&mut harness, &mut inputs, "Review the files.").await;
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"] && script.requests("r2").len() == 1).await;
    let before = (
        script.requests("r1")[0].clone(),
        script.requests("r2")[0].clone(),
    );
    assert!(
        first_message(&before.0)
            .contains("# Your profile (coordinator)\nCoordinate it, version one.")
    );
    assert!(first_message(&before.0).contains("# Workers of your own"));

    // What each run started with is kept with the session.
    let workflow = harness.workflow().clone();
    let runs = workflow.subagents();
    let (r1, r2) = (runs[0].clone(), runs[1].clone());
    assert_eq!((r1.depth, r2.depth, r2.caller_run_id), (1, 2, r1.run_id));
    let kept = crew.run_model(r1.run_id).unwrap();
    assert_eq!(kept.tools_added, crew::NESTED_TOOL_NAMES);
    assert!(
        kept.context_added
            .unwrap()
            .contains("Coordinate it, version one.")
    );
    assert!(crew.run_model(r2.run_id).unwrap().tools_added.is_empty());
    let record = std::fs::read_to_string(harness.session.dir.join("worker-models.json")).unwrap();
    assert!(record.contains("start_agent") && record.contains("Workers of your own"));

    // Arko stops. The profile is edited, then deleted.
    crew.shutdown().await;
    std::fs::write(
        &file,
        "---\ndescription: Splits a review\nmodel: opus\neffort: low\n---\nCoordinate it, version two.\n",
    )
    .unwrap();
    std::fs::remove_file(&file).unwrap();
    script
        .r2_stays
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // The session resumes, and passes the check of its saved runs.
    harness.open(true).unwrap();
    harness.start_new_workers();
    until(|| script.requests("r2").len() >= 2).await;
    let after = script.requests("r2")[1].clone();
    assert_eq!(first_message(&after), first_message(&before.1));
    assert_eq!(tool_names(&after), tool_names(&before.1));
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;

    // r2's report reached r1, which was waiting, and r1's reached the main
    // agent. The family is as it was.
    let r1_last = script.requests("r1").last().unwrap().clone();
    assert!(read(&r1_last).contains("Counted 41 files."));
    assert_eq!(first_message(&r1_last), first_message(&before.0));
    assert_eq!(tool_names(&r1_last), tool_names(&before.0));
    let main = script.requests("main");
    assert!(
        main.iter()
            .any(|body| read(body).contains("Reviewed. r2 said:"))
    );
    let summaries = harness.feed.worker_summaries();
    let family: Vec<_> = summaries
        .iter()
        .map(|summary| {
            (
                summary.id.as_str(),
                summary.parent.as_deref(),
                summary.depth,
                summary.state,
            )
        })
        .collect();
    assert_eq!(
        family,
        [
            ("r1", None, 1, remote::WorkerState::Finished),
            ("r2", Some("r1"), 2, remote::WorkerState::Finished)
        ]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker whose lead won't run again when the session resumes is stopped
/// and not relaunched, and the clients list it as stopped under its lead.
#[tokio::test]
async fn a_worker_whose_lead_failed_is_stopped_when_the_session_resumes() {
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"] && script.requests("r2").len() == 1).await;
    crew.shutdown().await;

    // As if r1 had failed and Arko had ended with r2 running.
    let run_dir = harness.session.run_dir();
    let latest = harness.session.latest_checkpoint().unwrap();
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
    saved["subagents"]["runs"][0]["state"] = "failed".into();
    saved["subagents"]["runs"][1]["state"] = "interrupted".into();
    std::fs::write(run_dir.join("edited-checkpoint.json"), saved.to_string()).unwrap();
    std::fs::write(run_dir.join("latest-checkpoint"), "edited-checkpoint.json").unwrap();

    harness.open(true).unwrap();
    harness.start_new_workers();
    let facts = harness.crew.as_ref().unwrap().facts();
    let state = |id: &str| {
        facts
            .iter()
            .find(|fact| fact.id == id)
            .map(|fact| fact.state.clone())
            .unwrap()
    };
    assert_eq!(
        (state("r1").as_str(), state("r2").as_str()),
        ("failed", "stopped")
    );
    let workers = harness.workflow().subagents();
    assert_eq!(
        workers[1].error.as_deref(),
        Some("its lead r1 isn't running")
    );
    // It was not relaunched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(script.requests("r2").len(), 1);
    assert!(harness.crew.as_ref().unwrap().running().is_empty());
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The conversation can't be edited while a worker of the tree runs, so a
/// cut back never reaches a running tree. The refusal names a worker.
#[tokio::test]
async fn an_edit_is_refused_while_a_nested_worker_runs() {
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"]).await;
    // (A message of the conversation; the refusal comes before it is looked up.)
    let item = 1;
    assert_eq!(
        harness
            .edit(item, "Review fewer files.".into())
            .unwrap_err(),
        "r1 is running. Wait for the workers or stop them first."
    );
    // With the tree stopped, the refusal is gone.
    assert_eq!(crew.stop_all("test"), 2);
    until(|| crew.running().is_empty()).await;
    assert_eq!(
        harness
            .edit(item, "Review fewer files.".into())
            .unwrap_err(),
        "This message can't be edited."
    );
    crew.shutdown().await;
}

/// Quitting while a tree runs counts every worker in it, and the workers
/// the agent list, the status line and the quit popup read are all of them.
#[tokio::test]
async fn quitting_counts_the_workers_of_the_whole_tree() {
    #[derive(Clone, Default)]
    struct Printed(Arc<Mutex<Vec<String>>>);
    impl ui::Printer for Printed {
        fn print(&mut self, text: String) -> Result<(), ()> {
            self.0
                .lock()
                .unwrap()
                .extend(text.lines().map(str::to_string));
            Ok(())
        }
    }
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    tokio::time::timeout(Duration::from_secs(20), script.refused.notified())
        .await
        .expect("r1 asked to report early");
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"]).await;
    let printed = Printed::default();
    harness.output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
    assert!(!harness.may_exit(Some(Vec::new())));
    assert_eq!(
        printed.0.lock().unwrap().last().unwrap(),
        "2 workers are running. /exit again stops them and quits (resuming the session lets the agent continue them)."
    );
    // The status counts both too.
    let live = remote::Meter::read(harness.gauges.as_ref());
    assert_eq!(live.workers_running, ["r1", "r2"]);
    crew.shutdown().await;
}

/// Esc on the main agent stops its turn and leaves the whole tree going. A
/// fresh conversation then owns r1 and r1 keeps r2.
#[tokio::test]
async fn stopping_the_main_agents_turn_leaves_the_tree_going() {
    let mut script = Script::new(true);
    script.main_waits = Some(Arc::default());
    let at_the_wait = script.main_waits.clone().unwrap();
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) = open_session(temp.path(), &server);
    let interrupt = tokio::spawn(async move {
        at_the_wait.notified().await;
        sender.send(Input::Interrupt).unwrap();
        sender
    });
    let end = tokio::time::timeout(
        Duration::from_secs(25),
        harness.turn(Prompt::User("Review the files.".into()), &mut inputs),
    )
    .await
    .unwrap();
    let _sender = interrupt.await.unwrap();
    assert!(matches!(end, TurnEnd::Stopped));
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"] && script.requests("r2").len() == 1).await;

    // A fresh conversation owns r1, which keeps r2.
    let workflow = harness.workflow().clone();
    let before = workflow.subagents();
    harness.conversation = Conversation::New;
    let end = tokio::time::timeout(
        Duration::from_secs(15),
        harness.turn(Prompt::User("Fresh lead".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let after = workflow.subagents();
    assert_ne!(after[0].caller_run_id, before[0].caller_run_id);
    assert_eq!(after[1].caller_run_id, after[0].run_id);
    assert_eq!(crew.running(), ["r1", "r2"]);
    // It lists r1 and not r2, which is not its worker.
    let listed = results(script.requests("main").last().unwrap())
        .pop()
        .unwrap();
    assert!(
        listed.contains("r1 · agent `worker` · running")
            && listed.contains("waiting for r2")
            && !listed.contains("r2 ·"),
        "{listed}"
    );
    crew.stop_all("done");
    crew.shutdown().await;
}

/// What the tree cost is counted once: the workers' figure is r1's plus
/// r2's, and the family total of r1 is the same.
#[tokio::test]
async fn a_nested_workers_cost_counts_once() {
    let script = Script::new(false);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let _ =
        tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs)).await;
    let summaries = harness.feed.worker_summaries();
    let own = |id: &str| summaries.iter().find(|summary| summary.id == id).unwrap();
    let (r1, r2) = (own("r1"), own("r2"));
    assert!(
        r1.cost_billed > 0.0 && r2.cost_billed > 0.0,
        "{r1:?} {r2:?}"
    );
    // The session's figures split the main agent from the workers.
    let reading = remote::Meter::read(harness.gauges.as_ref());
    let workers = reading.costs.workers.billed;
    assert!(
        (workers - (r1.cost_billed + r2.cost_billed)).abs() < 1e-9,
        "{workers} against {} and {}",
        r1.cost_billed,
        r2.cost_billed
    );
    // r1's family is r1 and r2 once each, and r2's is its own.
    assert!((r1.family_cost_billed - workers).abs() < 1e-9, "{r1:?}");
    assert!(
        (r2.family_cost_billed - r2.cost_billed).abs() < 1e-9,
        "{r2:?}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A monitor of a nested worker's stops when the worker is stopped, with
/// the worker above it, as the workers under it do.
#[cfg(unix)]
#[tokio::test]
async fn a_nested_workers_monitor_stops_with_it() {
    // r2 stays on its second request, after its monitor started.
    let mut script = Script::new(true);
    script.r2_monitors = true;
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let shell = Shell::detect(WindowsShell::Cmd);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.background = managers.background.clone();
    harness.open(false).unwrap();
    harness.start_new_workers();
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let crew = harness.crew.clone().unwrap();
    until(|| crew.running() == ["r1", "r2"] && harness.background.jobs().len() == 1).await;
    let jobs = harness.background.jobs();
    assert!(
        jobs[0].command.contains("sleep 60"),
        "{:?}",
        jobs[0].command
    );

    // Stopping r1 stops r2, which stops its monitor.
    assert_eq!(crew.stop("r1", "test").unwrap(), ["r2"]);
    until(|| crew.running().is_empty() && harness.background.jobs().is_empty()).await;
    crew.shutdown().await;
    managers.shutdown().await;
}

/// `/worker-depth` and `/worker-models` apply to the next start at once:
/// the same start is refused and then goes through as the user changes
/// them.
#[tokio::test]
async fn the_depth_and_the_models_setting_apply_to_the_next_start() {
    let script = Script::new(false);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = open_session(temp.path(), &server);
    let hook = harness.crew.as_ref().unwrap().config().hook.unwrap();
    let registered = harness.worker_config.clone();
    let start = |nested: bool| {
        let mut request = StartRequest {
            workflow_run_id: uuid::Uuid::nil(),
            run_id: uuid::Uuid::new_v4(),
            handle: "r1".into(),
            agent: WORKER.into(),
            caller_run_id: uuid::Uuid::new_v4(),
            caller_agent: MAIN.into(),
            task: "Count".into(),
            parameters: Default::default(),
        };
        request.parameters.insert("nested".into(), nested.into());
        let mut config = registered.clone();
        hook.prepare(&request, &mut config).map(|()| config)
    };
    assert!(start(true).is_ok());
    for (command, nested_allowed) in [
        ("/worker-depth 1", false),
        ("/worker-depth 3", true),
        ("/worker-models session", false),
        ("/worker-models agent", true),
        ("/worker-depth 2", true),
    ] {
        assert!(harness.live_command(command, Source::Terminal), "{command}");
        assert_eq!(start(true).is_ok(), nested_allowed, "{command}");
        assert!(start(false).is_ok(), "{command}");
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker that starts many workers at once, each in a worktree of its own,
/// stays within its eight. The starts pass the hook together, before any
/// worktree exists, so each has to find its place taken.
#[tokio::test]
async fn a_worker_starting_many_isolated_workers_together_stays_within_its_limit() {
    let mut script = Script::new(true);
    script.r1_together = Some((12, true));
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    let work = harness.settings.workspace.clone();
    let git = |args: &[&str]| {
        let done = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&work)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .status()
            .unwrap();
        assert!(done.success(), "git {args:?}");
    };
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    git(&["init", "-q"]);
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "one"]);
    turn(&mut harness, &mut inputs, "Review the files.").await;
    until(|| script.requests("r1").len() >= 2).await;
    let answers = results(&script.requests("r1")[1]);
    assert_eq!(answers.len(), 12, "{answers:?}");
    let started = answers
        .iter()
        .filter(|answer| answer.contains("in an isolated workspace"))
        .count();
    let refused = answers
        .iter()
        .filter(|answer| {
            answer.contains(
                "You already have 8 workers running or queued, the most one worker can have.",
            )
        })
        .count();
    assert_eq!((started, refused), (8, 4), "{answers:?}");
    let own = harness
        .workflow()
        .subagents()
        .into_iter()
        .filter(|run| run.handle != "r1")
        .count();
    assert_eq!(own, 8);
    let crew = harness.crew.clone().unwrap();
    crew.stop_all("done");
    crew.shutdown().await;
}

/// The main agent is told that a worker can start workers only when the
/// depth lets it. The instructions are those of the conversation's start, so
/// the depth before it starts changes them, and after it starts a note tells
/// the agent and the start hook holds each start to the depth.
#[tokio::test]
async fn the_main_agent_hears_of_nested_workers_only_when_the_depth_allows_them() {
    let sentence = crate::agents::NESTING;
    let instructions = |body: &str| -> String {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        body["instructions"].as_str().unwrap().to_string()
    };

    // One level, from before the conversation: no sentence, and `nested`
    // is refused.
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    assert!(harness.live_command("/worker-depth 1", Source::Terminal));
    turn(&mut harness, &mut inputs, "Review the files.").await;
    let first = script.requests("main")[0].clone();
    assert!(!instructions(&first).contains(sentence));
    assert!(results(&script.requests("main")[1])[0].contains("(/worker-depth)"));
    // The conversation has started: the depth changes now come as a note
    // and the hook, and the instructions stay as they were.
    assert!(harness.live_command("/worker-depth 3", Source::Terminal));
    assert!(!harness.main_config.instructions.contains(sentence));
    let hook = harness.crew.as_ref().unwrap().config().hook.unwrap();
    let mut config = harness.worker_config.clone();
    let mut request = StartRequest {
        workflow_run_id: uuid::Uuid::nil(),
        run_id: uuid::Uuid::new_v4(),
        handle: "r9".into(),
        agent: WORKER.into(),
        caller_run_id: uuid::Uuid::new_v4(),
        caller_agent: MAIN.into(),
        task: "Count".into(),
        parameters: Default::default(),
    };
    request.parameters.insert("nested".into(), true.into());
    assert!(hook.prepare(&request, &mut config).is_ok());
    harness.crew.as_ref().unwrap().stop_all("done");
    harness.crew.as_ref().unwrap().shutdown().await;

    // Two levels, and the instructions follow a change before the
    // conversation starts, up and down.
    let script = Script::new(true);
    let server = script.serve().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = open_session(temp.path(), &server);
    assert!(harness.main_config.instructions.contains(sentence));
    for (command, told) in [
        ("/worker-depth 1", false),
        ("/worker-depth 4", true),
        ("/worker-models session", false),
        ("/worker-models agent", true),
    ] {
        assert!(harness.live_command(command, Source::Terminal), "{command}");
        assert_eq!(
            harness.main_config.instructions.contains(sentence),
            told,
            "{command}"
        );
    }
    turn(&mut harness, &mut inputs, "Review the files.").await;
    assert!(instructions(&script.requests("main")[0]).contains(sentence));
    harness.crew.as_ref().unwrap().stop_all("done");
    harness.crew.as_ref().unwrap().shutdown().await;
}
