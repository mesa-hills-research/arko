// End-to-end tests of the project's instructions (AGENTS.md and CLAUDE.md):
// the flags and defaults, the session that keeps the choice,
// `/project-instructions`, and what reaches the main agent's and the
// workers' first messages, with the mock model. (Included in `main`'s
// tests, whose helpers they use.) Which files each choice gives, and the
// cap, are tested in `project`.

use crate::mock_model::{ModelServer, complete, tool_call};

/// The heading of AGENTS.md in the main agent's first message, and in a
/// worker's.
const MAIN_AGENTS: &str = "# Project instructions (AGENTS.md in the working directory)";
const WORKER_AGENTS: &str = "# Project instructions (AGENTS.md in your lead's working directory)";
const MAIN_CLAUDE: &str = "# Project instructions (CLAUDE.md in the working directory)";
const WORKER_CLAUDE: &str = "# Project instructions (CLAUDE.md in your lead's working directory)";

fn args(words: &[&str]) -> Args {
    Args::parse_from(std::iter::once("fin").chain(words.iter().copied()))
}

/// The user messages of a request, in order.
fn user_messages(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "user")
        .map(|item| match &item["content"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .collect()
}

/// The first message of the first request of worker run `handle`.
fn worker_opening(bodies: &[String], handle: &str) -> String {
    let marker = format!("You are agent run {handle},");
    let body = bodies
        .iter()
        .find(|body| body.contains(&marker))
        .unwrap_or_else(|| panic!("no request of {handle}"));
    user_messages(body).remove(0)
}

/// The feed's items of `kind`, oldest first.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

/// `harness`'s working directory with these instruction files, read as
/// fin reads them when it starts.
fn with_files(harness: &mut Harness, agents: Option<&str>, claude: Option<&str>) {
    let workspace = harness.settings.workspace.clone();
    for (name, text) in [("AGENTS.md", agents), ("CLAUDE.md", claude)] {
        if let Some(text) = text {
            std::fs::write(workspace.join(name), text).unwrap();
        }
    }
    harness.project = project::Found::read(&workspace);
    harness.give_workers_project();
    harness.publish_models();
}

/// A new session gives the agents AGENTS.md, else CLAUDE.md, and gives it
/// to workers too. --project-instructions chooses the files and
/// --no-worker-project-instructions keeps them from the workers, for a new
/// session and for a resumed one, which otherwise keeps its own.
#[test]
fn the_flags_choose_for_new_and_resumed_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    let new = |words: &[&str]| {
        let mut all = vec!["-m", "opus", "--no-cache-1h", "-C", dir];
        all.extend(words);
        new_settings(&args(&all)).unwrap()
    };
    let default = new(&[]);
    assert_eq!(default.project_instructions, project::Files::One);
    assert!(default.worker_project_instructions);
    assert_eq!(
        new(&["-p", "Hi"]).project_instructions,
        project::Files::One
    );
    assert_eq!(
        new(&["--project-instructions", "both"]).project_instructions,
        project::Files::Both
    );
    let off = new(&[
        "--project-instructions",
        "off",
        "--no-worker-project-instructions",
    ]);
    assert_eq!(off.project_instructions, project::Files::Off);
    assert!(!off.worker_project_instructions);
    assert!(
        Args::try_parse_from([
            "fin",
            "--worker-project-instructions",
            "--no-worker-project-instructions"
        ])
        .is_err(),
        "one or the other"
    );
    assert!(Args::try_parse_from(["fin", "--project-instructions", "agents"]).is_err());

    // A resumed session (saved before the setting: one file, not to
    // workers) keeps its own without the flags, and takes them with them.
    let mut resumed = Settings {
        project_instructions: project::Files::One,
        worker_project_instructions: false,
        ..default
    };
    assert!(!project_from_args(&args(&["-c"]), &mut resumed));
    assert_eq!(resumed.project_instructions, project::Files::One);
    assert!(!resumed.worker_project_instructions);
    assert!(project_from_args(
        &args(&[
            "-c",
            "--project-instructions",
            "both",
            "--worker-project-instructions",
        ]),
        &mut resumed,
    ));
    assert_eq!(resumed.project_instructions, project::Files::Both);
    assert!(resumed.worker_project_instructions);
}

/// The session keeps both choices in its settings, leaving out what
/// sessions saved before them did (one file, not to workers), so those
/// load as they worked.
#[test]
fn the_choices_are_saved_with_the_session_and_old_sessions_load_as_they_were() {
    let temp = tempfile::tempdir().unwrap();
    let session = Session::create(temp.path()).unwrap();
    let model = ModelSettings {
        model: resolve_model("luna").unwrap().id,
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let mut settings = Settings {
        workspace: temp.path().to_path_buf(),
        main: model.clone(),
        worker: model,
        max_workers: NO_LIMIT,
        handover_at: 200_000,
        main_handover_at: None,
        allow_outside_workspace: false,
        show_thinking: false,
        created: chrono::Utc::now(),
        title: None,
        windows_shell: None,
        openai_auth: None,
        own_worker_model: false,
        text_replies: false,
        extra_rules: Default::default(),
        project_instructions: project::Files::Both,
        worker_project_instructions: true,
    };
    let file = session.dir.join("session.json");
    let saved = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
    };
    for (files, name) in [(project::Files::Both, "both"), (project::Files::Off, "off")] {
        settings.project_instructions = files;
        session.save_settings(&settings).unwrap();
        assert_eq!(saved()["project_instructions"], name);
        assert_eq!(saved()["worker_project_instructions"], true);
        let loaded = session.load_settings().unwrap();
        assert_eq!(loaded.project_instructions, files);
        assert!(loaded.worker_project_instructions);
    }
    settings.project_instructions = project::Files::One;
    settings.worker_project_instructions = false;
    session.save_settings(&settings).unwrap();
    assert!(saved().get("project_instructions").is_none(), "{}", saved());
    assert!(saved().get("worker_project_instructions").is_none(), "{}", saved());
    let loaded = session.load_settings().unwrap();
    assert_eq!(loaded.project_instructions, project::Files::One);
    assert!(!loaded.worker_project_instructions);
}

/// The main agent gets the files in the first message of each
/// conversation, once: a later message of the conversation doesn't repeat
/// them. A change applies from the next conversation on, with the files
/// as fin read them at its start.
#[tokio::test]
async fn the_main_agent_gets_them_once_per_conversation() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |index, body: String| {
        seen.lock().unwrap().push(body);
        Box::pin(async move { complete(&format!("m{index}"), "Noted.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    with_files(&mut harness, Some("Use tabs.\n"), Some("Use spaces."));
    let last = |bodies: &Arc<Mutex<Vec<String>>>| bodies.lock().unwrap().last().unwrap().clone();
    let say = async |harness: &mut Harness, inputs: &mut Inputs, text: &str| {
        let end = harness.turn(Prompt::User(text.into()), inputs).await;
        assert!(matches!(end, TurnEnd::Answered));
    };

    // One: AGENTS.md, first, then the message.
    say(&mut harness, &mut inputs, "Hi.").await;
    let first = user_messages(&last(&bodies));
    assert_eq!(
        first[0],
        format!("{MAIN_AGENTS}\n\nUse tabs.\n\n# My message\n\nHi.")
    );
    say(&mut harness, &mut inputs, "And now?").await;
    let body = last(&bodies);
    assert_eq!(body.matches("Use tabs.").count(), 1, "{body}");
    assert_eq!(user_messages(&body).last().unwrap(), "And now?");

    // Both, from the next conversation on: the conversation under way keeps
    // what it got.
    harness.project_command("both", Source::Terminal);
    assert!(texts(&harness.feed, "output").iter().any(|text| text.ends_with(
        "The conversation under way keeps what it got, and the next one gets the change."
    )));
    say(&mut harness, &mut inputs, "Still here?").await;
    assert!(!last(&bodies).contains("Use spaces."));
    harness.new_conversation();
    // fin read the files as it started: a later edit isn't sent.
    std::fs::write(harness.settings.workspace.join("AGENTS.md"), "Changed.").unwrap();
    say(&mut harness, &mut inputs, "Fresh.").await;
    assert_eq!(
        user_messages(&last(&bodies))[0],
        format!("{MAIN_AGENTS}\n\nUse tabs.\n\n{MAIN_CLAUDE}\n\nUse spaces.\n\n# My message\n\nFresh.")
    );

    // Off: neither, in the next conversation.
    harness.project_command("off", Source::Terminal);
    harness.new_conversation();
    say(&mut harness, &mut inputs, "Plain.").await;
    assert_eq!(user_messages(&last(&bodies))[0], "Plain.");
}

/// `/project-instructions` says what the agents get from the working
/// directory and how often, and changes it at any time: the session and
/// the clients have the change. It says what it takes when given anything
/// else, and that a session without workers has none to give them to.
#[tokio::test]
async fn the_command_says_and_changes_the_choices() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("m1", "Noted.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    with_files(&mut harness, Some("Use tabs."), None);
    let reading = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref());
    let output = |harness: &Harness| texts(&harness.feed, "output");

    harness.project_command("", Source::Terminal);
    assert!(output(&harness).contains(
        &"Project instructions (one): AGENTS.md, or CLAUDE.md when there is no AGENTS.md. Here the agents get AGENTS.md. The main agent gets it once per conversation, in its first message. Workers don't get it. /project-instructions one, both or off changes which files, and /project-instructions workers on or off whether workers get them."
            .to_string()
    ));
    harness.project_command("workers on", Source::Terminal);
    assert!(output(&harness).contains(
        &"Workers started from now on get the project instructions, once each, in their first message."
            .to_string()
    ));
    assert!(harness.settings.worker_project_instructions);
    assert!(harness.session.load_settings().unwrap().worker_project_instructions);
    assert!(reading(&harness).settings.worker_project_instructions);
    harness.project_command("workers on", Source::Terminal);
    assert!(
        output(&harness)
            .contains(&"Project instructions for workers are on already.".to_string())
    );

    harness.project_command("both", Source::Web);
    assert!(texts(&harness.feed, "notice").contains(
        &"From the web page: Project instructions (both): AGENTS.md and CLAUDE.md, each under its own name. Here the agents get AGENTS.md. The main agent gets it once per conversation, in its first message, and each worker once, in its first message."
            .to_string()
    ));
    assert_eq!(harness.settings.project_instructions, project::Files::Both);
    assert_eq!(
        harness.session.load_settings().unwrap().project_instructions,
        project::Files::Both
    );
    assert_eq!(
        reading(&harness).settings.project_instructions,
        project::Files::Both
    );

    harness.project_command("off", Source::Terminal);
    assert!(output(&harness).contains(
        &"Project instructions (off): neither AGENTS.md nor CLAUDE.md.".to_string()
    ));
    harness.project_command("sometimes", Source::Terminal);
    harness.project_command("workers maybe", Source::Terminal);
    let usage = "/project-instructions takes one, both or off, or workers on or off (/project-instructions both).".to_string();
    assert_eq!(output(&harness).iter().filter(|text| **text == usage).count(), 2);
    assert_eq!(harness.settings.project_instructions, project::Files::Off);

    // No file: the agents get none.
    harness.project = project::Found::default();
    harness.project_command("one", Source::Terminal);
    assert!(output(&harness).contains(
        &"Project instructions (one): AGENTS.md, or CLAUDE.md when there is no AGENTS.md. The working directory has neither."
            .to_string()
    ));

    // Without workers, there are none to give them to.
    harness.settings.max_workers = 0;
    harness.project_command("workers off", Source::Terminal);
    assert!(output(&harness).contains(
        &"This session has no workers (it started with --workers 0, or with the old worker tools).".to_string()
    ));
    assert!(harness.settings.worker_project_instructions);
}

/// Each worker gets the files in its first message, before its task, with
/// the text the main agent gets: from the lead's working directory, as fin
/// read it when it started, not the worker's own (an untracked or edited
/// AGENTS.md would be missing or older in a worktree of its own). A worker
/// started after workers are turned off doesn't get them, and one started
/// after `both` gets both.
#[tokio::test]
async fn workers_get_them_in_their_first_message_by_the_setting() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        seen.lock().unwrap().push(body.clone());
        Box::pin(async move {
            let start = |id: &str, task: &str| {
                tool_call(
                    id,
                    crew::START_AGENT,
                    serde_json::json!({"agent": WORKER, "task": task, "workspace": "sub"}),
                )
            };
            if body.contains("You are agent run r") {
                complete("w", "Done.")
            } else if body.contains("Started r3") {
                complete("m6", "Started r3.")
            } else if body.contains("Third.") {
                start("m5", "Third task.")
            } else if body.contains("Started r2") {
                complete("m4", "Started r2.")
            } else if body.contains("Second.") {
                start("m3", "Second task.")
            } else if body.contains("Started r1") {
                complete("m2", "Started r1.")
            } else {
                start("m1", "First task.")
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    // The worker works in `sub`, which has an AGENTS.md of its own.
    let sub = harness.settings.workspace.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("AGENTS.md"), "Sub rules.").unwrap();
    with_files(&mut harness, Some("Use tabs."), Some("Use spaces."));
    std::fs::write(harness.settings.workspace.join("AGENTS.md"), "Edited.").unwrap();
    harness.project_command("workers on", Source::Terminal);
    let worker = |handle: &'static str| {
        let bodies = bodies.clone();
        async move {
            until(|| {
                bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains(&format!("You are agent run {handle},")))
            })
            .await;
            worker_opening(&bodies.lock().unwrap(), handle)
        }
    };

    for (message, handle) in [("First.", "r1"), ("Second.", "r2"), ("Third.", "r3")] {
        match handle {
            "r2" => harness.project_command("workers off", Source::Terminal),
            "r3" => {
                harness.project_command("workers on", Source::Terminal);
                harness.project_command("both", Source::Terminal);
            }
            _ => {}
        }
        let end = harness.turn(Prompt::User(message.into()), &mut inputs).await;
        assert!(matches!(end, TurnEnd::Answered));
        let opening = worker(handle).await;
        match handle {
            "r1" => assert!(
                opening.starts_with(&format!(
                    "{WORKER_AGENTS}\n\nUse tabs.\n\n# Your task\n\nFirst task.\n\n# Working as an agent run\nYou are agent run r1,"
                )),
                "{opening}"
            ),
            "r2" => assert!(
                opening.starts_with("Second task.\n\n# Working as an agent run\n"),
                "{opening}"
            ),
            _ => assert!(
                opening.starts_with(&format!(
                    "{WORKER_AGENTS}\n\nUse tabs.\n\n{WORKER_CLAUDE}\n\nUse spaces.\n\n# Your task\n\nThird task."
                )),
                "{opening}"
            ),
        }
        assert!(!opening.contains("Sub rules."), "{opening}");
        assert!(!opening.contains("Edited."), "{opening}");
    }
    // The main agent got AGENTS.md once, in its first message.
    let lead = bodies
        .lock()
        .unwrap()
        .iter()
        .rfind(|body| !body.contains("You are agent run r"))
        .unwrap()
        .clone();
    assert_eq!(lead.matches("Use tabs.").count(), 1, "{lead}");
    harness.crew.clone().unwrap().shutdown().await;
}

/// A session saved before workers got the files loads with them off, and
/// its new workers don't get them until `/project-instructions workers on`.
#[tokio::test]
async fn an_old_session_keeps_them_from_its_workers() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("m1", "Noted.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    std::fs::write(
        harness.session.dir.join("session.json"),
        serde_json::to_string(&serde_json::json!({
            "workspace": harness.settings.workspace,
            "main": harness.settings.main,
            "worker": harness.settings.worker,
            "max_workers": NO_LIMIT,
            "handover_at": 200_000,
            "allow_outside_workspace": false,
            "show_thinking": false,
            "created": harness.settings.created,
        }))
        .unwrap(),
    )
    .unwrap();
    harness.settings = harness.session.load_settings().unwrap();
    assert_eq!(harness.settings.project_instructions, project::Files::One);
    assert!(!harness.settings.worker_project_instructions);
    with_files(&mut harness, Some("Use tabs."), None);
    let crew = harness.crew.clone().unwrap();
    assert_eq!(crew.project(), None);
    assert!(harness.project.for_main(harness.settings.project_instructions).is_some());
    harness.project_command("workers on", Source::Terminal);
    assert_eq!(
        crew.project().as_deref(),
        Some(format!("{WORKER_AGENTS}\n\nUse tabs.").as_str())
    );
}
