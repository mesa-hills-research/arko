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
    // (A turn that waits for an answer no one gives, as a regression in the
    // approval flow would leave it, fails here and doesn't block the run.)
    let end = tokio::time::timeout(
        Duration::from_secs(60),
        harness.turn(Prompt::User(message.into()), inputs),
    )
    .await
    .expect("the turn ended in time");
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
    // The worker's summary, which the clients list, says what it runs and
    // what its calls cost so far.
    let summary = harness
        .feed
        .worker_summaries()
        .into_iter()
        .find(|summary| summary.title.contains("Start with the judge."))
        .expect("the judge's summary");
    assert_eq!(summary.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(summary.effort.as_deref(), Some("xhigh"));
    assert_eq!(summary.profile.as_deref(), Some("judge"));
    assert_eq!((summary.parent.as_deref(), summary.depth), (None, 1));
    assert!(summary.cost_billed > 0.0 && summary.cost_list >= summary.cost_billed);
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
    // (With Claude Code signed in on the machine, it also offers cc:haiku.)
    assert!(
        result.contains("Can't start a worker on claude-haiku-5-5. It needs ANTHROPIC_API_KEY, and this session has none. Ask the user to add it with /keys")
            && result.ends_with(", or choose a model that worker_options lists as usable now."),
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
    assert!(options.starts_with("Worker profiles (pass one as start_agent's profile)\n- default · gpt-6-luna, high effort · the session's worker model (/model worker)\n\nModels usable now"), "{options}");
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
            // (The next question may be up before this one is seen gone.)
            until(|| feed.activity().question.map(|asked| asked.id) != Some(question.id)).await;
        }
    });
    let saved = harness.root.join("profiles").join("3d-judge.md");
    start_worker(&mut harness, &mut inputs, "Start by saving a profile for everyone.").await;
    assert_eq!(
        last_result(&seen),
        "The user didn't save 3d-judge for every project and answered \"no, use medium effort\". Only a plain yes saves it. If they asked for a change, make it and ask again. If they meant yes, ask again and tell them to answer yes. Or save it for this session with scope session."
    );
    assert!(!saved.exists());
    start_worker(&mut harness, &mut inputs, "Start by saving a profile for everyone.").await;
    assert!(
        last_result(&seen).starts_with("Saved the worker profile 3d-judge in the user's Arko folder, for every project."),
        "{}",
        last_result(&seen)
    );
    assert!(std::fs::read_to_string(&saved).unwrap().contains("made_by: agent"));
    tokio::time::timeout(Duration::from_secs(20), answers)
        .await
        .expect("the answers were given in time")
        .unwrap();
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The commands' output the feed shows, joined.
fn said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the last command said.
fn last_said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .rev()
        .find(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .unwrap_or_default()
}

/// `/worker-models` says who chooses the workers' models and changes it at
/// any time. Before the conversation starts the agents are built again with
/// what the main agent may choose in their instructions. After, the main
/// agent reads a note, and the start hook enforces the new choice.
#[tokio::test]
async fn worker_models_sets_who_chooses_the_workers_models() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;
    assert!(harness.live_command("/worker-models", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "Models for workers is set to agent. The main agent chooses a profile, model and effort for each worker, and can save profiles. /worker-models agent, profiles or session changes that."
    );
    assert!(harness.live_command("/worker-models everyone", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "`everyone` isn't one of agent, profiles or session, such as /worker-models profiles."
    );
    assert!(harness.live_command("/worker-models agent", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("Models for workers is set to agent already."));

    // Before the conversation starts, the agents are built again.
    assert!(harness.main_config.instructions.contains("`save_worker_profile`"));
    assert!(harness.live_command("/worker-models profiles", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "The main agent starts workers with a profile only. It can't choose a model or effort by itself."
    );
    assert_eq!(harness.settings.worker_models, crate::agents::ModelChoice::Profiles);
    assert!(!harness.main_config.instructions.contains("`save_worker_profile`"));
    assert!(harness.main_config.instructions.contains("with a profile, a named setup the user saved"));
    assert_eq!(
        harness.session.load_settings().unwrap().worker_models,
        crate::agents::ModelChoice::Profiles
    );
    start_worker(&mut harness, &mut inputs, "Start on a model.").await;
    assert!(last_result(&seen).contains("(/worker-models profiles)"));

    // From the page, it says where it came from. After the start, the main
    // agent is told, and the hook goes by the new choice at once.
    assert!(harness.live_command("/worker-models session", Source::Web));
    assert_eq!(
        last_said(&harness.feed),
        "Changed from the web page. Every worker runs the session's worker model, gpt-6-luna (high effort)."
    );
    assert!(harness.conversation_started());
    start_worker(&mut harness, &mut inputs, "Start with the judge.").await;
    assert!(last_result(&seen).contains("(/worker-models session)"));
    let told = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(who, ..)| who == "main")
        .any(|(_, _, _, body)| {
            body.contains("The user now has every worker run the session's worker model, gpt-6-luna (high effort). Start workers without a profile, model or effort.")
        });
    assert!(told, "the main agent read a note");
    assert!(harness.live_command("/worker-models agent", Source::Terminal));
    start_worker(&mut harness, &mut inputs, "Start on a model.").await;
    assert!(worker_with(&seen, "Start on a model.").is_some_and(|(model, ..)| model == "gpt-6-astra"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/worker-depth` says how many levels of workers the session allows and
/// sets it from 1 to 4, saved with the session.
#[tokio::test]
async fn worker_depth_is_kept_and_says_what_it_allows() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender, _server, _seen) = setup(temp.path()).await;
    assert!(harness.live_command("/worker-depth", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "Worker depth is 2. Workers can go 2 levels deep. A worker the main agent starts can start workers of its own when the main agent allows it. /worker-depth 1 to 4 changes it."
    );
    for bad in ["0", "5", "deep"] {
        assert!(harness.live_command(&format!("/worker-depth {bad}"), Source::Terminal));
        assert!(
            last_said(&harness.feed).contains("from 1 to 4"),
            "{bad}: {}",
            last_said(&harness.feed)
        );
    }
    assert_eq!(harness.settings.worker_depth, 2);
    assert!(harness.live_command("/worker-depth 3", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "Worker depth is now 3. Workers can go 3 levels deep. A worker the main agent starts can start workers of its own when the main agent allows it."
    );
    assert_eq!(harness.settings.worker_depth, 3);
    assert_eq!(harness.session.load_settings().unwrap().worker_depth, 3);
    assert!(harness.live_command("/worker-depth 3", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("Worker depth is 3 already."));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// `/profiles` lists the profiles of the three places with where each is,
/// what hides what and what can't be read, shows one whole, makes one for
/// every project and deletes one.
#[tokio::test]
async fn profiles_lists_shows_adds_and_removes() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender, _server, _seen) = setup(temp.path()).await;
    assert!(harness.live_command("/profiles", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "No worker profiles yet. /profiles add NAME MODEL creates one, or ask the agent to save one."
    );

    // Adding one writes the file in Arko's folder, with the words given.
    assert!(harness.live_command(
        "/profiles add 3d-judge sol high Judges 3D results against a reference image",
        Source::Terminal
    ));
    let file = harness.root.join("profiles").join("3d-judge.md");
    assert_eq!(
        last_said(&harness.feed),
        format!(
            "Saved the worker profile 3d-judge for every project in {}. Edit the file to write its description and instructions.",
            file.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "---\ndescription: Judges 3D results against a reference image\nmodel: sol\neffort: high\n---\n"
    );
    assert!(harness.live_command("/profiles add 3d-judge opus", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("There is a profile `3d-judge` for every project already."));
    assert!(harness.live_command("/profiles add quick gpt9", Source::Terminal));
    assert!(last_said(&harness.feed).contains("`gpt9` isn't a model Arko knows."));
    assert!(harness.live_command("/profiles add Bad opus", Source::Terminal));
    assert!(last_said(&harness.feed).contains("isn't a profile name"));
    assert!(harness.live_command("/profiles add quick", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("/profiles lists the worker profiles"));
    assert!(harness.live_command("/profiles add quick luna", Source::Terminal));
    assert!(
        std::fs::read_to_string(harness.root.join("profiles").join("quick.md"))
            .unwrap()
            .contains("description: Describe what this profile is for")
    );
    // Without a description, the reply says the main agent reads the
    // placeholder until it is edited.
    assert!(last_said(&harness.feed).ends_with(
        "Until you do, the main agent reads \"Describe what this profile is for\" as its description."
    ));

    // The project's and the session's, a hidden one and one that can't be
    // read.
    let project = harness.settings.workspace.join(".agents").join("profiles");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("reviewer.md"), "---\ndescription: The project's reviewer\nmodel: opus\neffort: high\n---\n").unwrap();
    std::fs::write(project.join("notes.md"), "---\nmodel sol\n---\n").unwrap();
    let session = harness.session.dir.join("profiles");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(
        session.join("reviewer.md"),
        "---\ndescription: Reviews a diff\nmade_by: agent\nmodel: sol\n---\nBody.\n",
    )
    .unwrap();
    assert!(harness.live_command("/profiles", Source::Terminal));
    let listed = last_said(&harness.feed);
    let expected = format!(
        "Worker profiles
  default   luna, high effort
      the session's worker model
  3d-judge  sol, high effort   every project
      Judges 3D results against a reference image
  quick     luna               every project
      Describe what this profile is for
  reviewer  sol                this session
      Reviews a diff (made by the main agent)
Hidden by a nearer profile of the same name
  reviewer                     this project
Can't be read
  notes                        this project
      line 2 isn't `key: value`
/profiles NAME shows one. The files are in
  this session
    {}
  this project
    {}
  every project
    {}",
        session.display(),
        project.display(),
        harness.root.join("profiles").display()
    );
    assert_eq!(listed, expected);
    // What a profile is for wraps under its row, so an 80-column screen
    // wraps no line of the list but a folder's path.
    std::fs::write(
        harness.root.join("profiles").join("long.md"),
        format!(
            "---\ndescription: {}\nmodel: gpt9\n---\n",
            "Reads every file in the repository and says what each one is for, in a line"
        ),
    )
    .unwrap();
    std::fs::write(project.join("x".repeat(60) + ".md"), "no front matter").unwrap();
    assert!(harness.live_command("/profiles", Source::Terminal));
    let wide = last_said(&harness.feed);
    for line in wide.lines().filter(|line| !line.starts_with("    /")) {
        assert!(line.chars().count() <= 76, "{} columns: {line}", line.chars().count());
    }
    assert!(wide.contains("\n      Reads every file in the repository"), "{wide}");
    assert!(wide.contains("(can't start, `gpt9` isn't a model Arko knows.)"), "{wide}");

    assert!(harness.live_command("/profiles reviewer", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        format!(
            "reviewer · for this session\n{}\nReviews a diff\nRuns on sol. The main agent made it.\nInstructions\nBody.",
            session.join("reviewer.md").display()
        )
    );
    assert!(harness.live_command("/profiles default", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("default · the session's worker model\nIt runs gpt-6-luna"));
    assert!(harness.live_command("/profiles nobody", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "There is no worker profile `nobody`. /profiles lists them."
    );
    assert!(harness.live_command("/profiles notes", Source::Terminal));
    assert!(last_said(&harness.feed).contains("can't be used (line 2 isn't `key: value`)"));
    // A file that can't be read is removed by its name too.
    assert!(harness.live_command("/profiles remove notes", Source::Terminal));
    let removed = last_said(&harness.feed);
    assert!(
        removed.starts_with("Removed ")
            && removed.ends_with("notes.md for this project, which couldn't be read."),
        "{removed}"
    );
    assert!(!project.join("notes.md").exists());
    assert!(harness.live_command("/profiles notes", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "There is no worker profile `notes`. /profiles lists them."
    );

    // Removing deletes the one shown, which brings the next into force.
    assert!(harness.live_command("/profiles remove reviewer", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        format!(
            "Removed the worker profile reviewer for this session ({}). The workers started with it keep their setup.",
            session.join("reviewer.md").display()
        )
    );
    assert!(harness.live_command("/profiles reviewer", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("reviewer · for this project"));
    assert!(harness.live_command("/profiles remove nobody", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with("There is no worker profile `nobody`."));
    assert!(said(&harness.feed).contains("Removed the worker profile reviewer"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

fn args(words: &[&str]) -> Options {
    Args::parse_from(std::iter::once("arko").chain(words.iter().copied()))
        .options()
        .unwrap()
}

/// `--worker-models` and `--worker-depth` set a new session's choice and
/// depth. Without them a new session has the main agent choose, two levels
/// deep.
#[test]
fn the_flags_set_who_chooses_and_how_deep_for_a_new_session() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    let new = |words: &[&str]| {
        let mut all = vec!["-m", "opus", "--no-cache-1h", "-C", dir];
        all.extend(words);
        new_settings(&args(&all), &NoPrompter).unwrap().0
    };
    let default = new(&[]);
    assert_eq!(default.worker_models, crate::agents::ModelChoice::Agent);
    assert_eq!(default.worker_depth, 2);
    let set = new(&["--worker-models", "profiles", "--worker-depth", "3"]);
    assert_eq!(set.worker_models, crate::agents::ModelChoice::Profiles);
    assert_eq!(set.worker_depth, 3);
    for (word, choice) in [
        ("agent", crate::agents::ModelChoice::Agent),
        ("session", crate::agents::ModelChoice::Session),
    ] {
        assert_eq!(new(&["--worker-models", word]).worker_models, choice);
    }
    for depth in 1..=4 {
        assert_eq!(new(&["--worker-depth", &depth.to_string()]).worker_depth, depth);
    }
    for bad in [
        ["--worker-models", "everyone"],
        ["--worker-depth", "0"],
        ["--worker-depth", "5"],
        ["--worker-depth", "deep"],
    ] {
        assert!(Args::try_parse_from(["arko", bad[0], bad[1]]).is_err(), "{bad:?}");
    }
}

/// `/session` says who chooses the workers' models and how deep workers go,
/// in a session that has workers.
#[tokio::test]
async fn session_says_who_chooses_the_workers_models_and_how_deep_they_go() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender, _server, _seen) = setup(temp.path()).await;
    harness.mirrored(|harness| harness.command("/session"));
    assert!(
        said(&harness.feed).contains(
            "models for workers agent (main agent chooses), worker depth 2"
        ),
        "{}",
        said(&harness.feed)
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// A session that resumes reads what each worker cost from the runs it
/// saved, and the worker's summary keeps the setup it started with.
#[tokio::test]
async fn a_resumed_session_reads_what_its_workers_cost_from_their_saved_runs() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, _seen) = setup(temp.path()).await;
    start_worker(&mut harness, &mut inputs, "Start on a model.").await;
    let summary = |harness: &Harness| harness.feed.worker_summary("r1").unwrap();
    let before = summary(&harness);
    assert!(before.cost_billed > 0.0, "{before:?}");
    assert_eq!(before.model.as_deref(), Some("gpt-6-astra"));

    // As if the page and the cost had been lost: the saved runs say it.
    let mut lost = crate::remote::WorkerInfo::new();
    lost.model = before.model.clone();
    lost.costs = Some((0.0, 0.0));
    harness.feed.worker_info("r1", lost);
    assert_eq!(summary(&harness).cost_billed, 0.0);
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    let after = summary(&harness);
    assert!((after.cost_billed - before.cost_billed).abs() < 1e-12, "{after:?}");
    assert!((after.cost_list - before.cost_list).abs() < 1e-12, "{after:?}");
    assert_eq!(after.model, before.model);
    assert_eq!(after.effort.as_deref(), Some("low"));
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// The `start_agent` the model is offered names the profile, model and
/// effort in a session that resumes, also one the main agent's saved
/// configuration doesn't list the profile tools for: the definitions are
/// made when a request is.
#[tokio::test]
async fn a_resumed_session_offers_start_agent_with_its_new_parameters() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender, _server, seen) = setup(temp.path()).await;
    start_worker(&mut harness, &mut inputs, "Start plainly.").await;
    harness.crew.as_ref().unwrap().shutdown().await;
    harness.open(true).unwrap();
    harness.start_new_workers();
    start_worker(&mut harness, &mut inputs, "Start again plainly.").await;
    let tools = |body: &str| -> Vec<(String, Vec<String>)> {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                let properties = tool["parameters"]["properties"]
                    .as_object()
                    .map(|properties| properties.keys().cloned().collect())
                    .unwrap_or_default();
                (tool["name"].as_str().unwrap().to_string(), properties)
            })
            .collect()
    };
    let last = seen
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(who, ..)| who == "main")
        .map(|(_, _, _, body)| tools(body))
        .unwrap();
    let start = last.iter().find(|(name, _)| name == "start_agent").unwrap();
    for parameter in ["profile", "model", "effort", "cache"] {
        assert!(start.1.iter().any(|name| name == parameter), "{start:?}");
    }
    assert!(last.iter().any(|(name, _)| name == "worker_options"));
    harness.crew.as_ref().unwrap().shutdown().await;
}
