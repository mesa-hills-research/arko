// End-to-end tests of a worker's model, effort and profile, with the mock
// model: what the main agent can choose when it starts a worker, what the
// worker's first request carries, the refusals the main agent reads, and a
// worker keeping its setup when its profile changes and the session is
// resumed. (Included in `main`'s tests, whose helpers they use.) How a
// setup is chosen, case by case, is tested in `chooser`.

use crate::mock_model::{ModelServer, complete, tool_call};

/// The requests the model server got: (the agent that sent it, its model,
/// its effort, its body).
type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

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

fn model_and_effort(body: &str) -> (String, String) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    (
        body["model"].as_str().unwrap_or_default().to_string(),
        body["reasoning"]["effort"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// The user messages of a request, in order.
fn user_texts(body: &str) -> Vec<String> {
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

/// The main agent's mock: the user's message says which worker to start.
/// `Start A.` starts a worker whose task is `A` with the parameters in
/// `starts`, and the call's result ends the turn.
fn lead(body: &str, step: &str) -> String {
    // The latest "Start ..." message of the user, and whether a call's
    // result came after it. (A worker's report and the host's notes start
    // turns and come as user messages too: the main agent only notes them.)
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    let mut start: Option<String> = None;
    for item in parsed["input"].as_array().unwrap() {
        if item["role"] == "user" {
            let text = match &item["content"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            // (The project's instructions come first in the first message.)
            let message = text
                .rsplit_once("# My message\n\n")
                .map_or(text.as_str(), |(_, message)| message)
                .trim();
            if message.starts_with("Start ") {
                start = Some(message.to_string());
            }
        } else if item["type"] == "function_call_output" {
            start = None;
        }
    }
    let Some(user) = start else {
        return complete(step, "Done.");
    };
    let saving = |scope: Option<&str>| {
        let mut call = serde_json::json!({
            "name": "3d-judge",
            "description": "Judges 3D results against a reference image",
            "model": "sol",
            "effort": "high",
            "instructions": "You judge 3D models.",
        });
        if let Some(scope) = scope {
            call["scope"] = scope.into();
        }
        tool_call(step, crew::SAVE_WORKER_PROFILE, call)
    };
    match user.as_str() {
        "Start by listing the options." => {
            return tool_call(step, crew::WORKER_OPTIONS, serde_json::json!({}));
        }
        "Start by saving a profile." => return saving(None),
        "Start by saving a profile for everyone." => return saving(Some("user")),
        _ => {}
    }
    let mut parameters = serde_json::json!({"agent": WORKER, "task": user});
    let object = parameters.as_object_mut().unwrap();
    let with = |object: &mut serde_json::Map<String, serde_json::Value>, pairs: &[(&str, &str)]| {
        for (name, value) in pairs {
            object.insert(name.to_string(), (*value).into());
        }
    };
    match user.as_str() {
        "Start on a model." => with(object, &[("model", "astra"), ("effort", "low")]),
        "Start with the judge." => with(object, &[("profile", "judge")]),
        "Start with the saved profile." => with(object, &[("profile", "3d-judge")]),
        "Start with the judge on a model." => {
            with(object, &[("profile", "judge"), ("model", "luna")])
        }
        "Start on haiku." => with(object, &[("model", "haiku")]),
        "Start on a bad effort." => with(
            object,
            &[("model", "gpt-5.4-nano-2026-03-17"), ("effort", "max")],
        ),
        "Start with a missing profile." => with(object, &[("profile", "nobody")]),
        "Start with a cache." => with(object, &[("cache", "1h")]),
        "Start with the worktree profile." => with(object, &[("profile", "tree")]),
        "Start with the worktree profile and no worktree." => {
            with(object, &[("profile", "tree")]);
            object.insert("isolated".into(), false.into());
        }
        _ => {}
    }
    tool_call(step, crew::START_AGENT, parameters)
}

/// A harness whose main agent and workers run luna, with a mock that
/// answers workers at once and records every request.
async fn setup(
    temp: &Path,
) -> (
    Harness,
    Inputs,
    mpsc::UnboundedSender<Input>,
    ModelServer,
    Seen,
) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let who = sender(&body);
        let (model, effort) = model_and_effort(&body);
        record
            .lock()
            .unwrap()
            .push((who.clone(), model, effort, body.clone()));
        Box::pin(async move {
            if who == "main" {
                lead(&body, "m")
            } else {
                complete(&format!("w{who}"), "Worker done.")
            }
        })
    }))
    .await;
    let (harness, inputs, sender) = harness(temp, server.client(), true);
    // As a session does once it is open.
    harness.start_new_workers();
    (harness, inputs, sender, server, seen)
}

/// The first request of the worker whose task is `task`: its model, its
/// effort and its first message.
fn worker_with(seen: &Seen, task: &str) -> Option<(String, String, String)> {
    seen.lock()
        .unwrap()
        .iter()
        .find(|(who, _, _, body)| who != "main" && user_texts(body)[0].contains(task))
        .map(|(_, model, effort, body)| (model.clone(), effort.clone(), user_texts(body)[0].clone()))
}

/// The tool result the main agent read last, from its latest request.
fn last_result(seen: &Seen) -> String {
    let seen = seen.lock().unwrap();
    let (_, _, _, body) = seen.iter().rev().find(|(who, ..)| who == "main").unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

async fn start_worker(harness: &mut Harness, inputs: &mut Inputs, message: &str) {
    let end = harness.turn(Prompt::User(message.into()), inputs).await;
    assert!(
        matches!(end, TurnEnd::Answered),
        "{}",
        match end {
            TurnEnd::Answered => "answered",
            TurnEnd::Stopped => "stopped",
            TurnEnd::Failed => "failed",
            TurnEnd::Exit => "exit",
        }
    );
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(inputs)).await;
}

/// The main agent starts workers on a model and effort of its choice, with
/// a profile (the project's here), and with nothing, which runs the
/// session's worker model. The profile's instructions open the worker's
/// first message after the project's, and `/model worker` changes only the
/// default.
#[tokio::test]
async fn a_worker_runs_the_model_effort_and_profile_the_main_agent_chose() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;
    let workspace = harness.settings.workspace.clone();
    let folder = workspace.join(".agents").join("profiles");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("judge.md"),
        "---\ndescription: Judges the render\nmodel: sol\neffort: xhigh\n---\nJudge it against the reference.\n",
    )
    .unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "Use tabs.\n").unwrap();
    harness.settings.worker_project_instructions = true;
    harness.project = project::Found::read(&workspace);
    harness.give_workers_project();

    start_worker(&mut harness, &mut inputs, "Start on a model.").await;
    let (model, effort, opening) = worker_with(&seen, "Start on a model.").unwrap();
    assert_eq!((model.as_str(), effort.as_str()), ("gpt-6-astra", "low"));
    assert!(!opening.contains("Your profile"), "{opening}");

    start_worker(&mut harness, &mut inputs, "Start with the judge.").await;
    let (model, effort, opening) = worker_with(&seen, "Start with the judge.").unwrap();
    assert_eq!((model.as_str(), effort.as_str()), ("gpt-6.1-sol", "xhigh"));
    let project_at = opening.find("Use tabs.").expect("the project's instructions");
    let profile_at = opening
        .find("# Your profile (judge)\nJudge it against the reference.")
        .expect("the profile's instructions");
    let task_at = opening.find("# Your task").unwrap();
    assert!(project_at < profile_at && profile_at < task_at, "{opening}");
    // A profile that works in a worktree asks the call for one, and the
    // call's own `isolated` beats it.
    std::fs::write(
        folder.join("tree.md"),
        "---\ndescription: Works apart\nisolated: true\n---\n",
    )
    .unwrap();
    start_worker(&mut harness, &mut inputs, "Start with the worktree profile.").await;
    assert_eq!(
        last_result(&seen),
        "The profile `tree` works in a git worktree of its own, so start it with `isolated: true`."
    );
    assert!(worker_with(&seen, "Start with the worktree profile.").is_none());
    start_worker(
        &mut harness,
        &mut inputs,
        "Start with the worktree profile and no worktree.",
    )
    .await;
    assert!(worker_with(&seen, "Start with the worktree profile and no worktree.").is_some());
    // The call's model beats the profile's.
    start_worker(&mut harness, &mut inputs, "Start with the judge on a model.").await;
    let (model, effort, _) = worker_with(&seen, "Start with the judge on a model.").unwrap();
    assert_eq!((model.as_str(), effort.as_str()), ("gpt-6-luna", "xhigh"));

    // Nothing chosen: the session's worker model, luna.
    start_worker(&mut harness, &mut inputs, "Start plainly.").await;
    let (model, effort, opening) = worker_with(&seen, "Start plainly.").unwrap();
    assert_eq!((model.as_str(), effort.as_str()), ("gpt-6-luna", "high"));
    assert!(!opening.contains("Your profile"));

    // `/model worker` changes what a start with no choice runs, and not a
    // profile's.
    assert!(harness.live_command("/model worker astra", Source::Terminal));
    start_worker(&mut harness, &mut inputs, "Start again plainly.").await;
    assert_eq!(worker_with(&seen, "Start again plainly.").unwrap().0, "gpt-6-astra");
    start_worker(&mut harness, &mut inputs, "Start with the judge.").await;
    let judged: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(who, _, _, body)| who != "main" && user_texts(body)[0].contains("Start with the judge."))
        .map(|(_, model, ..)| model.clone())
        .collect();
    assert!(judged.iter().all(|model| model == "gpt-6.1-sol"), "{judged:?}");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// What the main agent reads when a start is refused: the model the
/// session can't reach, the effort the model doesn't take, and a profile
/// that isn't there. No worker starts.
#[tokio::test]
async fn a_refused_start_tells_the_main_agent_what_to_do_instead() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;
    let started = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(who, ..)| who != "main")
            .count()
    };

    start_worker(&mut harness, &mut inputs, "Start on haiku.").await;
    let result = last_result(&seen);
    assert!(
        result.contains("Can't start a worker on claude-haiku-5-5. It needs ANTHROPIC_API_KEY, and this session has none. Ask the user to add one with /keys, or choose a model that worker_options lists as usable now."),
        "{result}"
    );
    start_worker(&mut harness, &mut inputs, "Start on a bad effort.").await;
    assert!(
        last_result(&seen).contains(
            "gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max."
        ),
        "{}",
        last_result(&seen)
    );
    start_worker(&mut harness, &mut inputs, "Start with a missing profile.").await;
    assert!(
        last_result(&seen)
            .contains("There is no worker profile `nobody`. The profiles are default. worker_options lists them."),
        "{}",
        last_result(&seen)
    );
    assert_eq!(started(&seen), 0, "no worker started");

    // What the user allows: profiles only, then the session's model only.
    harness.settings.worker_models = crate::agents::ModelChoice::Profiles;
    harness.start_new_workers();
    start_worker(&mut harness, &mut inputs, "Start on a model.").await;
    assert!(
        last_result(&seen).contains("(/worker-models profiles)"),
        "{}",
        last_result(&seen)
    );
    start_worker(&mut harness, &mut inputs, "Start with a cache.").await;
    assert_eq!(started(&seen), 1, "a start with no model or effort goes on");
    harness.settings.worker_models = crate::agents::ModelChoice::Session;
    harness.start_new_workers();
    start_worker(&mut harness, &mut inputs, "Start with the judge.").await;
    assert!(
        last_result(&seen).contains("(/worker-models session)"),
        "{}",
        last_result(&seen)
    );
    assert_eq!(started(&seen), 1);
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A worker keeps the setup it started with when Arko stops and the session
/// resumes, though its profile was edited and then deleted meanwhile: the
/// saved session still resumes, the run goes on on its model, and its first
/// message still carries the profile's instructions as it started.
#[tokio::test]
async fn a_worker_resumes_with_its_setup_after_its_profile_is_deleted() {
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold = held.clone();
    let first_started = Arc::new(tokio::sync::Notify::new());
    let started = first_started.clone();
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let who = sender(&body);
        let (model, effort) = model_and_effort(&body);
        record
            .lock()
            .unwrap()
            .push((who.clone(), model, effort, body.clone()));
        let (hold, started) = (hold.clone(), started.clone());
        Box::pin(async move {
            match who.as_str() {
                // The worker's first request waits, as if the model were
                // slow, until Arko stops.
                "r1" if !hold.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                    started.notify_one();
                    std::future::pending::<String>().await
                }
                "r1" => complete("w1", "First done."),
                _ => lead(&body, "m"),
            }
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    harness.start_new_workers();
    let folder = harness.root.join("profiles");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("judge.md");
    std::fs::write(
        &file,
        "---\ndescription: Judges\nmodel: sol\neffort: medium\n---\nJudge version one.\n",
    )
    .unwrap();
    assert!(matches!(
        harness
            .turn(Prompt::User("Start with the judge.".into()), &mut inputs)
            .await,
        TurnEnd::Answered
    ));
    tokio::time::timeout(Duration::from_secs(10), first_started.notified())
        .await
        .unwrap();
    // The record, with the profile's text, is kept with the session.
    let record = std::fs::read_to_string(harness.session.dir.join("worker-models.json")).unwrap();
    assert!(record.contains("Judge version one.") && record.contains("\"profile\": \"judge\""));

    // Arko stops. The profile is edited, then deleted, and a worker started
    // by hand has the edit.
    harness.crew.as_ref().unwrap().shutdown().await;
    std::fs::write(
        &file,
        "---\ndescription: Judges\nmodel: astra\neffort: low\n---\nJudge version two.\n",
    )
    .unwrap();
    std::fs::remove_file(&file).unwrap();
    // The session resumes: it passes the check of its saved runs, and r1
    // goes on where it stopped.
    harness.open(true).unwrap();
    harness.start_new_workers();
    let r1_requests = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(who, ..)| who == "r1")
            .map(|(_, model, effort, body)| (model.clone(), effort.clone(), body.clone()))
            .collect::<Vec<_>>()
    };
    until(|| r1_requests().len() >= 2).await;
    let requests = r1_requests();
    for (model, effort, body) in &requests {
        assert_eq!((model.as_str(), effort.as_str()), ("gpt-6.1-sol", "medium"));
        assert!(
            user_texts(body)[0].contains("# Your profile (judge)\nJudge version one."),
            "{}",
            user_texts(body)[0]
        );
    }
    let _ = tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    // `/workers` still says what it runs.
    assert!(harness.crew.as_ref().unwrap().describe().contains("r1"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The main agent lists the options through its tool, keeps a profile for
/// the session without asking, and starts a worker with it.
#[tokio::test]
async fn the_main_agent_lists_the_options_saves_a_profile_and_starts_a_worker_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;

    start_worker(&mut harness, &mut inputs, "Start by listing the options.").await;
    let options = last_result(&seen);
    assert!(options.starts_with("Worker profiles (start_agent's profile)\n- default · gpt-6-luna, high effort · the session's worker model (/model worker)\n\nModels usable now"), "{options}");
    assert!(options.contains("\n- sol · efforts low to max · "), "{options}");
    assert!(options.contains("Not usable now"), "{options}");

    start_worker(&mut harness, &mut inputs, "Start by saving a profile.").await;
    assert_eq!(
        last_result(&seen),
        "Saved the worker profile 3d-judge for this session. start_agent takes it by name."
    );
    assert!(
        harness
            .session
            .dir
            .join("profiles")
            .join("3d-judge.md")
            .is_file()
    );
    assert!(has(
        &harness.feed,
        "notice",
        "The main agent saved the worker profile 3d-judge for this session.",
        ""
    ));
    // The list now has it, and a worker starts with it.
    start_worker(&mut harness, &mut inputs, "Start by listing the options.").await;
    assert!(last_result(&seen).contains(
        "\n- 3d-judge (session, made by you) · sol, high effort · Judges 3D results against a reference image"
    ));
    start_worker(&mut harness, &mut inputs, "Start with the saved profile.").await;
    let (model, effort, opening) = worker_with(&seen, "Start with the saved profile.").unwrap();
    assert_eq!((model.as_str(), effort.as_str()), ("gpt-6.1-sol", "high"));
    assert!(opening.contains("# Your profile (3d-judge)\nYou judge 3D models."), "{opening}");
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A profile for every session reaches the user as a question, the same one
/// the agent's `ask_user` makes. A reply that is not a plain yes is feedback
/// and saves nothing, and a yes saves the file in Arko's folder.
#[tokio::test]
async fn a_profile_for_every_session_is_put_to_the_user_as_a_question() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, input, _server, seen) = setup(temp.path()).await;
    let feed = harness.feed.clone();
    // (The input stays open while the turns end: a closed one quits.)
    let sender = input.clone();
    let answers = tokio::spawn(async move {
        for reply in ["no, use medium effort", "Yes"] {
            until(|| feed.activity().question.is_some()).await;
            let question = feed.activity().question.unwrap();
            assert!(
                question.text.starts_with(
                    "The main agent wants to save the worker profile 3d-judge in your Arko folder, for every project.\n\nJudges 3D results against a reference image\nRuns on sol at high effort."
                ),
                "{}",
                question.text
            );
            assert!(question.text.contains("Instructions\nYou judge 3D models."));
            sender
                .send(Input::Answer {
                    question: question.id,
                    text: reply.into(),
                })
                .unwrap();
            until(|| feed.activity().question.is_none()).await;
        }
    });
    let saved = harness.root.join("profiles").join("3d-judge.md");
    start_worker(&mut harness, &mut inputs, "Start by saving a profile for everyone.").await;
    assert_eq!(
        last_result(&seen),
        "The user didn't save 3d-judge for every project and wrote \"no, use medium effort\". Change it and ask again, or save it for this session with scope session."
    );
    assert!(!saved.exists());
    start_worker(&mut harness, &mut inputs, "Start by saving a profile for everyone.").await;
    assert!(
        last_result(&seen).starts_with("Saved the worker profile 3d-judge in the user's Arko folder, for every project."),
        "{}",
        last_result(&seen)
    );
    assert!(std::fs::read_to_string(&saved).unwrap().contains("made_by: agent"));
    answers.await.unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}
