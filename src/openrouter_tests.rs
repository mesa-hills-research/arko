// End-to-end tests of the OpenRouter settings: a change saved while Arko
// runs reaches the agents' next request, the words of `/openrouter` change
// the file, and a model switched to is kept and sized from what is known of
// it. (Included in `main`'s tests, whose helpers they use.)

use crate::credentials;
use crate::mock_model::{ModelServer, openrouter_tool_call};

/// What the commands printed and the session said, joined.
fn said_all(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .filter(|(kind, _, _)| kind == "output" || kind == "notice")
        .map(|(_, text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the last command printed.
fn last_said(feed: &remote::Feed) -> String {
    shown(feed)
        .into_iter()
        .rev()
        .find(|(kind, _, _)| kind == "output")
        .map(|(_, text, _)| text)
        .unwrap_or_default()
}

fn opus_on_openrouter() -> ModelSettings {
    ModelSettings {
        model: "openrouter:anthropic/claude-opus-5.5".into(),
        effort: Effort::Xhigh,
        fast: false,
        cache_1h: false,
        handover_at: Some(500_000),
    }
}

/// OpenRouter's settings saved while a session runs reach its agents' next
/// request, which carries them as `provider`. Until then the requests go as
/// they did, and a session in another folder isn't touched.
#[tokio::test]
async fn saved_settings_reach_the_agents_at_their_next_request() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, text: String| {
        seen.lock()
            .unwrap()
            .push(serde_json::from_str(&text).unwrap_or_default());
        Box::pin(async move {
            openrouter_tool_call(
                "1",
                "complete_task",
                serde_json::json!({"summary": "Done."}),
                0.01,
            )
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
    let model = opus_on_openrouter();
    let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
    let root = harness.root.clone();
    harness.keys.use_local(server.url());
    harness.keys.share(root.clone());
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-test"));

    let end = harness.turn(Prompt::User("One.".into()), &mut inputs).await;
    assert!(matches!(end, TurnEnd::Answered));
    let first = bodies.lock().unwrap()[0].clone();
    assert_eq!(first["provider"], serde_json::json!({"sort": "price"}));

    // Saved by an app (nobody's own session): the agents use it at once, and
    // the session says so.
    let mut settings = crate::openrouter::Settings::load(&root);
    settings.set_zdr(true);
    settings.set_deny_data_collection(true);
    settings.set_sort(crate::openrouter::Sort::Throughput);
    settings.save(&root).unwrap();
    until(|| said_all(&harness.feed).contains("OpenRouter settings changed")).await;
    let end = harness.turn(Prompt::User("Two.".into()), &mut inputs).await;
    assert!(matches!(end, TurnEnd::Answered));
    let second = bodies.lock().unwrap()[1].clone();
    assert_eq!(second["model"], "anthropic/claude-opus-5.5");
    assert_eq!(
        second["provider"],
        serde_json::json!({"zdr": true, "data_collection": "deny", "sort": "throughput"})
    );

    // Another folder's settings aren't this session's.
    let other = temp.path().join("other");
    let mut elsewhere = crate::openrouter::Settings::default();
    elsewhere.set_ignore(vec!["azure".into()]);
    elsewhere.save(&other).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let end = harness
        .turn(Prompt::User("Three.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(bodies.lock().unwrap()[2]["provider"], second["provider"]);
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A key replaced while the session runs keeps the settings.
#[tokio::test]
async fn a_replaced_key_keeps_the_settings() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, text: String| {
        seen.lock()
            .unwrap()
            .push(serde_json::from_str(&text).unwrap_or_default());
        Box::pin(async move {
            openrouter_tool_call(
                "1",
                "complete_task",
                serde_json::json!({"summary": "Done."}),
                0.01,
            )
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
    let model = opus_on_openrouter();
    let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
    let root = harness.root.clone();
    let mut settings = crate::openrouter::Settings::default();
    settings.set_ignore(vec!["azure".into()]);
    settings.save(&root).unwrap();
    harness.keys.use_local(server.url());
    harness.keys.share(root.clone());
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-first"));
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-second"));
    let end = harness.turn(Prompt::User("One.".into()), &mut inputs).await;
    assert!(matches!(end, TurnEnd::Answered));
    assert_eq!(
        bodies.lock().unwrap()[0]["provider"]["ignore"],
        serde_json::json!(["azure"])
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// The words of `/openrouter` change the file, say what they did, and ask
/// for the forms when they aren't one.
#[tokio::test]
async fn the_openrouter_words_change_the_settings_file() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
    let root = harness.root.clone();
    let say = |harness: &mut Harness, line: &str| {
        assert!(harness.live_command(line, Source::Terminal), "{line}");
        last_said(&harness.feed)
    };
    assert_eq!(
        say(&mut harness, "/openrouter zdr on"),
        "Zero data retention is on. Only providers that keep no data serve requests, and the list of models shows the models offered that way. Open sessions use it from their next request."
    );
    say(&mut harness, "/openrouter data-collection deny");
    say(&mut harness, "/openrouter region us-or-eu");
    say(&mut harness, "/openrouter sort latency");
    say(&mut harness, "/openrouter precision bf16");
    say(&mut harness, "/openrouter only Anthropic, google-vertex");
    say(&mut harness, "/openrouter ignore azure");
    let settings = crate::openrouter::Settings::load(&root);
    assert!(settings.zdr() && settings.deny_data_collection());
    assert_eq!(settings.region(), crate::openrouter::Region::UsOrEu);
    assert_eq!(settings.sort(), crate::openrouter::Sort::Latency);
    assert_eq!(settings.precision(), crate::openrouter::Precision::Bf16);
    assert_eq!(settings.only(), ["anthropic", "google-vertex"]);
    assert_eq!(settings.ignore(), ["azure"]);

    // None takes a list off, and the list says so.
    assert_eq!(
        say(&mut harness, "/openrouter only none"),
        "Any provider may serve requests. Open sessions use it from their next request."
    );
    say(&mut harness, "/openrouter ignore none");
    say(&mut harness, "/openrouter zdr off");
    assert_eq!(crate::openrouter::Settings::load(&root).only().len(), 0);

    // A word that isn't one says what the forms are, and changes nothing.
    let before = std::fs::read_to_string(root.join(crate::openrouter::FILE)).unwrap();
    let wrong = say(&mut harness, "/openrouter region mars");
    assert_eq!(wrong, "/openrouter region takes any, us, eu or us-or-eu.");
    let wrong = say(&mut harness, "/openrouter zdr maybe");
    assert_eq!(wrong, "/openrouter zdr takes on or off.");
    let wrong = say(&mut harness, "/openrouter colour red");
    assert!(wrong.contains("/openrouter takes these forms."), "{wrong}");
    assert!(
        wrong.contains("/openrouter precision any|fp8|bf16"),
        "{wrong}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(crate::openrouter::FILE)).unwrap(),
        before
    );

    // Alone, it says the settings and the forms (the screen opens a menu).
    let shown = say(&mut harness, "/openrouter");
    assert!(shown.starts_with("OpenRouter\n"), "{shown}");
    for (label, value) in [
        ("Zero data retention", "Off"),
        ("Region", "US or EU"),
        ("Models", "not loaded"),
    ] {
        let row = format!("\n  {label:<21}  {value}\n");
        assert!(shown.contains(&row), "{row:?} in {shown}");
    }
    for form in crate::openrouter::FORMS {
        assert!(shown.contains(form), "{form}");
    }
    assert!(shown.contains("account's own privacy settings"), "{shown}");

    // A file that can't be read isn't written over.
    std::fs::write(root.join(crate::openrouter::FILE), "{ oops").unwrap();
    let broken = say(&mut harness, "/openrouter zdr on");
    assert!(broken.contains("openrouter.json can't be read"), "{broken}");
    assert_eq!(
        std::fs::read_to_string(root.join(crate::openrouter::FILE)).unwrap(),
        "{ oops"
    );
}

/// A model switched to with `/model` is kept among the models picked, and
/// sized from what was kept of it, with no list of models to read.
#[tokio::test]
async fn a_model_switched_to_is_kept_and_sized_from_what_is_known() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
    let root = harness.root.clone();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(crate::openrouter::FILE),
        r#"{"recent": [{"slug": "acme/kept", "label": "Kept", "context": 131072}]}"#,
    )
    .unwrap();
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-test"));
    assert!(harness.live_command("/model openrouter:acme/other", Source::Terminal));
    assert_eq!(
        harness.settings.main.handover_at,
        Some(providers::UNKNOWN_LIMIT_HANDOVER),
        "nothing is known of it"
    );
    assert!(harness.live_command("/model openrouter:acme/kept high", Source::Terminal));
    assert_eq!(harness.settings.main.model, "openrouter:acme/kept");
    let mut sized = arko_ai::Model::openrouter("acme/kept");
    sized.max_input_tokens = 131_072;
    assert_eq!(
        harness.settings.main.handover_at,
        Some(agents::default_handover(&sized, None))
    );

    // Newest first, with what was kept, in the file and in what the
    // session publishes for the picker.
    let settings = crate::openrouter::Settings::load(&root);
    assert_eq!(
        settings.recent_names(),
        ["openrouter:acme/kept", "openrouter:acme/other"]
    );
    assert_eq!(settings.recent()[0].context, Some(131_072));
    let models = remote::Meter::read(harness.gauges.as_ref()).models;
    assert_eq!(models.recent, settings.recent_names());
    let group = crate::models::menu(&models, |_| None)
        .into_iter()
        .find(|group| group.name.starts_with("OpenRouter"))
        .unwrap();
    assert_eq!(
        group
            .models
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["openrouter:acme/kept", "openrouter:acme/other"]
    );

    // A file that can't be read stops the switch, since the settings may
    // be in it.
    std::fs::write(root.join(crate::openrouter::FILE), "{ oops").unwrap();
    assert!(harness.live_command("/model openrouter:acme/third", Source::Terminal));
    assert!(
        last_said(&harness.feed).contains("openrouter.json can't be read"),
        "{}",
        last_said(&harness.feed)
    );
    assert_eq!(harness.settings.main.model, "openrouter:acme/kept");
}

/// The model server for a main agent that starts one worker, both on
/// OpenRouter: every request body and head is kept.
async fn lead_and_worker() -> (ModelServer, Arc<Mutex<Vec<serde_json::Value>>>) {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, text: String| {
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        seen.lock().unwrap().push(body.clone());
        Box::pin(async move {
            let worker = text.contains("You are agent run r");
            let answered = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if worker || answered {
                openrouter_tool_call(
                    "w",
                    "complete_task",
                    serde_json::json!({"summary": "Done."}),
                    0.01,
                )
            } else {
                openrouter_tool_call(
                    "m",
                    crew::START_AGENT,
                    serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    0.01,
                )
            }
        })
    }))
    .await;
    (server, bodies)
}

/// The requests of the main agent and of workers, apart.
fn by_agent(
    bodies: &Arc<Mutex<Vec<serde_json::Value>>>,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    bodies
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .partition(|body| !body.to_string().contains("You are agent run r"))
}

/// `/openrouter` changes the requests of the session that ran it at once,
/// the main agent's and its workers', not only those of the others.
#[tokio::test]
async fn the_openrouter_command_changes_its_own_sessions_requests_and_its_workers() {
    let (server, bodies) = lead_and_worker().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let model = opus_on_openrouter();
    let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
    let root = harness.root.clone();
    harness.keys.use_local(server.url());
    harness.keys.share(root.clone());
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-test"));

    assert!(harness.live_command("/openrouter zdr on", Source::Terminal));
    assert!(harness.live_command("/openrouter sort throughput", Source::Terminal));
    assert!(harness.live_command("/openrouter ignore azure", Source::Terminal));
    // (The session installed them itself, and doesn't announce its own.)
    assert!(!said_all(&harness.feed).contains("OpenRouter settings changed"));
    let end = harness
        .turn(Prompt::User("Start a worker.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    until(|| !by_agent(&bodies).1.is_empty()).await;
    let _ =
        tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs)).await;
    let (main, workers) = by_agent(&bodies);
    let wanted = serde_json::json!({"zdr": true, "sort": "throughput", "ignore": ["azure"]});
    assert_eq!(main[0]["provider"], wanted);
    assert_eq!(workers[0]["provider"], wanted);
    let heads = server.heads().join("\n").to_ascii_lowercase();
    assert!(
        heads.contains("authorization: bearer sk-or-test"),
        "{heads}"
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A key replaced while a change to the settings is made is used with the
/// settings as the change leaves them, whichever came first: the request
/// goes out with the new key and the new settings.
#[tokio::test]
async fn a_key_replaced_during_a_settings_change_goes_out_with_both() {
    let (server, bodies) = lead_and_worker().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let model = opus_on_openrouter();
    let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
    let root = harness.root.clone();
    harness.keys.use_local(server.url());
    harness.keys.share(root.clone());
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-old"));

    // The change has read the file (zero data retention off) and holds the
    // lock, and the key is replaced meanwhile.
    let (reached, go) = crate::openrouter::pause_next_change(&root);
    let change = {
        let root = root.clone();
        std::thread::spawn(move || {
            crate::openrouter::Settings::update(&root, |settings| settings.set_zdr(true))
        })
    };
    reached.recv().unwrap();
    let replace = {
        let keys = harness.keys.clone();
        std::thread::spawn(move || keys.apply(&credentials::OPENROUTER, Some("sk-or-new")))
    };
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !replace.is_finished(),
        "the replacement waits for the change"
    );
    go.send(()).unwrap();
    change.join().unwrap().unwrap();
    replace.join().unwrap();

    let end = harness
        .turn(Prompt::User("Hello.".into()), &mut inputs)
        .await;
    let _ = end;
    let (main, _) = by_agent(&bodies);
    // (Without a sort of its own, a Claude request is sorted by price.)
    assert_eq!(
        main[0]["provider"],
        serde_json::json!({"zdr": true, "sort": "price"})
    );
    let heads = server.heads().join("\n").to_ascii_lowercase();
    assert!(heads.contains("authorization: bearer sk-or-new"), "{heads}");
    assert!(!heads.contains("sk-or-old"), "{heads}");
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// Settings saved after the session's client was made and before the session
/// hears of changes are used by its first request.
#[tokio::test]
async fn settings_saved_while_a_session_starts_reach_its_first_request() {
    let (server, bodies) = lead_and_worker().await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let model = opus_on_openrouter();
    let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
    let root = harness.root.clone();
    // The client as the session made it (no settings), not yet sharing.
    harness.keys.use_local(server.url());
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-test"));
    assert!(harness.keys.installed().unwrap().1.provider.is_none());
    // Saved while the rest of the start goes on, with no one listening.
    crate::openrouter::Settings::update(&root, |settings| {
        settings.set_zdr(true);
        settings.set_region(crate::openrouter::Region::Any);
    })
    .unwrap();
    assert!(harness.keys.installed().unwrap().1.provider.is_none());
    // The session starts hearing of changes, and takes the ones it missed.
    harness.keys.share(root.clone());
    assert_eq!(
        harness.keys.installed().unwrap().1.provider.unwrap().zdr,
        Some(true)
    );
    let end = harness
        .turn(Prompt::User("Hello.".into()), &mut inputs)
        .await;
    let _ = end;
    let (main, _) = by_agent(&bodies);
    // (Without a sort of its own, a Claude request is sorted by price.)
    assert_eq!(
        main[0]["provider"],
        serde_json::json!({"zdr": true, "sort": "price"})
    );
    harness.crew.as_ref().unwrap().shutdown().await;
    managers.shutdown().await;
}

/// A key replaced while the settings file can't be read leaves the client
/// as it is, with the old key and the restrictions it has, and says so. A
/// session that has none doesn't get one without the restrictions. Once the
/// file is fixed and changed, the new key is installed with the settings.
#[tokio::test]
async fn a_key_replaced_while_the_settings_cant_be_read_installs_no_unrestricted_client() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    crate::openrouter::Settings::update(&root, |settings| {
        settings.set_zdr(true);
        settings.set_region(crate::openrouter::Region::Eu);
    })
    .unwrap();
    let session = |root: &std::path::Path| {
        let keys = crate::keys::Keys::new(Arc::new(LLMClient::new()), None, None, None);
        keys.share(root.to_path_buf());
        keys
    };
    let keys = session(&root);
    keys.apply(&credentials::OPENROUTER, Some("sk-or-old"));
    let (key, options) = keys.installed().unwrap();
    assert_eq!(key, "sk-or-old");
    assert_eq!(options.provider.unwrap().zdr, Some(true));

    std::fs::write(root.join(crate::openrouter::FILE), "{ oops").unwrap();
    let said = keys.apply(&credentials::OPENROUTER, Some("sk-or-new"));
    assert!(said.contains("openrouter.json can't be read"), "{said}");
    assert!(
        said.contains("keeps the key and the settings it has"),
        "{said}"
    );
    let (key, options) = keys.installed().unwrap();
    assert_eq!(key, "sk-or-old", "the client it has stays");
    assert_eq!(options.provider.unwrap().zdr, Some(true));
    assert!(keys.reaches(&arko_ai::Provider::OpenRouter));

    // Fixed by hand, and the next change installs the key replaced.
    std::fs::write(
        root.join(crate::openrouter::FILE),
        r#"{"region": "eu", "provider": {"zdr": true}}"#,
    )
    .unwrap();
    crate::openrouter::Settings::update(&root, |settings| {
        settings.set_sort(crate::openrouter::Sort::Price)
    })
    .unwrap();
    let (key, options) = keys.installed().unwrap();
    assert_eq!(key, "sk-or-new");
    assert_eq!(options.provider.as_ref().unwrap().zdr, Some(true));
    assert!(options.provider.unwrap().sort.is_some());

    // A session with no OpenRouter client doesn't get one without the file.
    std::fs::write(root.join(crate::openrouter::FILE), "{ oops").unwrap();
    let fresh = session(&root);
    let said = fresh.apply(&credentials::OPENROUTER, Some("sk-or-key"));
    assert!(said.contains("can't be used until it is fixed"), "{said}");
    assert!(fresh.installed().is_none());
    assert!(!fresh.reaches(&arko_ai::Provider::OpenRouter));
    std::fs::write(
        root.join(crate::openrouter::FILE),
        r#"{"provider": {"zdr": true}}"#,
    )
    .unwrap();
    crate::openrouter::Settings::update(&root, |settings| settings.set_deny_data_collection(true))
        .unwrap();
    let (key, options) = fresh.installed().unwrap();
    assert_eq!(key, "sk-or-key");
    assert_eq!(options.provider.unwrap().zdr, Some(true));
    assert!(fresh.reaches(&arko_ai::Provider::OpenRouter));
}

/// Another process holds the settings file's lock for `held`: for this long
/// a change must wait.
fn hold_the_settings_lock(root: &std::path::Path, held: Duration) -> std::thread::JoinHandle<()> {
    std::fs::create_dir_all(root).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(format!("{}.lock", crate::openrouter::FILE)))
        .unwrap();
    file.lock().unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(held);
        drop(file);
    })
}

/// A task that counts, as an async heartbeat: it stops if the thread the
/// runtime has is held up.
fn heartbeat() -> (
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let beats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting = beats.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    (beats, task)
}

/// `/openrouter` waits for the settings' lock away from the session's tasks:
/// while another process holds it, the session goes on (a heartbeat keeps
/// beating), and the change is made, installed and said once it lets go.
#[tokio::test]
async fn the_openrouter_command_waits_for_the_lock_without_holding_up_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
    let root = harness.root.clone();
    harness
        .keys
        .apply(&credentials::OPENROUTER, Some("sk-or-test"));
    harness.keys.share_async(root.clone()).await;
    let holder = hold_the_settings_lock(&root, Duration::from_millis(600));
    let (beats, task) = heartbeat();
    harness
        .openrouter_live("/openrouter zdr on", "zdr on", Source::Terminal)
        .await;
    let beaten = beats.load(std::sync::atomic::Ordering::SeqCst);
    task.abort();
    holder.join().unwrap();
    assert!(beaten >= 20, "the session's tasks went on: {beaten} beats");
    assert!(
        last_said(&harness.feed).starts_with("Zero data retention is on."),
        "{}",
        last_said(&harness.feed)
    );
    assert!(crate::openrouter::Settings::read(&root).unwrap().zdr());
    let (_, options) = harness.keys.installed().unwrap();
    assert_eq!(
        options.provider.unwrap().zdr,
        Some(true),
        "installed when said"
    );
}

/// A session starting takes the settings it missed on a thread made for
/// waiting, so a change in progress doesn't stop the tasks, and it is done
/// once they are taken.
#[tokio::test]
async fn a_session_starting_waits_for_a_change_in_progress_without_stopping_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let keys = crate::keys::Keys::new(Arc::new(LLMClient::new()), None, None, None);
    keys.apply(&credentials::OPENROUTER, Some("sk-or-test"));
    crate::openrouter::Settings::update(&root, |settings| settings.set_zdr(true)).unwrap();
    // A change is in progress in this process: it has the folder's lock.
    let (reached, go) = crate::openrouter::pause_next_change(&root);
    let change = {
        let root = root.clone();
        std::thread::spawn(move || {
            crate::openrouter::Settings::update(&root, |settings| {
                settings.set_deny_data_collection(true)
            })
        })
    };
    reached.recv().unwrap();
    let (beats, task) = heartbeat();
    let starting = {
        let (keys, root) = (keys.clone(), root.clone());
        tokio::spawn(async move { keys.share_async(root).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!starting.is_finished(), "it waits for the change");
    assert!(
        beats.load(std::sync::atomic::Ordering::SeqCst) >= 15,
        "and the tasks go on"
    );
    go.send(()).unwrap();
    starting.await.unwrap();
    change.join().unwrap().unwrap();
    task.abort();
    let provider = keys.installed().unwrap().1.provider.unwrap();
    assert_eq!(provider.zdr, Some(true));
    assert_eq!(
        provider.data_collection,
        Some(arko_ai::DataCollection::Deny),
        "with the change it waited for"
    );
}

/// A file emptied or blanked, as a truncated write leaves it, isn't "no
/// settings": a client that has the restrictions keeps them when a key is
/// replaced, and a session without one gets none.
#[tokio::test]
async fn an_emptied_settings_file_installs_no_unrestricted_client() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    crate::openrouter::Settings::update(&root, |settings| settings.set_zdr(true)).unwrap();
    let session = |root: &std::path::Path| {
        let keys = crate::keys::Keys::new(Arc::new(LLMClient::new()), None, None, None);
        keys.share(root.to_path_buf());
        keys
    };
    let keys = session(&root);
    keys.apply(&credentials::OPENROUTER, Some("sk-or-old"));
    let file = root.join(crate::openrouter::FILE);
    for text in ["", "   \n"] {
        std::fs::write(&file, text).unwrap();
        let said = keys.apply(&credentials::OPENROUTER, Some("sk-or-new"));
        assert!(said.contains("can't be read"), "{text:?}: {said}");
        let (key, options) = keys.installed().unwrap();
        assert_eq!(key, "sk-or-old", "{text:?}");
        assert_eq!(options.provider.unwrap().zdr, Some(true), "{text:?}");
        let fresh = session(&root);
        let said = fresh.apply(&credentials::OPENROUTER, Some("sk-or-key"));
        assert!(
            said.contains("can't be used until it is fixed"),
            "{text:?}: {said}"
        );
        assert!(fresh.installed().is_none() && !fresh.reaches(&arko_ai::Provider::OpenRouter));
    }
}

/// A key removed, then added again while the settings can't be read, comes
/// back once the file is fixed and changed: the replacement is installed,
/// and the removal no longer holds.
#[tokio::test]
async fn a_key_added_after_its_removal_returns_once_the_settings_are_fixed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    crate::openrouter::Settings::update(&root, |settings| settings.set_zdr(true)).unwrap();
    let keys = crate::keys::Keys::new(Arc::new(LLMClient::new()), None, None, None);
    keys.share(root.clone());
    keys.apply(&credentials::OPENROUTER, Some("sk-or-old"));
    assert!(keys.reaches(&arko_ai::Provider::OpenRouter));
    keys.apply(&credentials::OPENROUTER, None);
    assert!(!keys.reaches(&arko_ai::Provider::OpenRouter), "removed");

    std::fs::write(root.join(crate::openrouter::FILE), "").unwrap();
    let said = keys.apply(&credentials::OPENROUTER, Some("sk-or-new"));
    assert!(said.contains("can't be used until it is fixed"), "{said}");
    assert!(!keys.reaches(&arko_ai::Provider::OpenRouter));
    assert_eq!(
        keys.installed().unwrap().0,
        "sk-or-old",
        "nothing new installed"
    );

    std::fs::write(
        root.join(crate::openrouter::FILE),
        r#"{"provider": {"zdr": true}}"#,
    )
    .unwrap();
    crate::openrouter::Settings::update(&root, |settings| {
        settings.set_sort(crate::openrouter::Sort::Latency)
    })
    .unwrap();
    assert!(keys.reaches(&arko_ai::Provider::OpenRouter), "it is back");
    let (key, options) = keys.installed().unwrap();
    assert_eq!(key, "sk-or-new");
    let provider = options.provider.unwrap();
    assert_eq!(provider.zdr, Some(true));
    assert!(provider.sort.is_some());
}
