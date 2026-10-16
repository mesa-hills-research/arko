// End-to-end tests of the extra rules for small models: the default and the
// flags that choose who gets them, the session that keeps the choice, and
// `/extra-rules`, which changes it before the conversation starts. Each
// follows a path a user takes, with the mock model. (Included in `main`'s
// tests, whose helpers they use.) Which agent gets them by its model is
// tested in `agents`.

use crate::mock_model::{ModelServer, complete};

/// The heading the extra rules start with in an agent's instructions.
const RULES: &str = "# Extra rules";

fn args(words: &[&str]) -> Options {
    Args::parse_from(std::iter::once("arko").chain(words.iter().copied()))
        .options()
        .unwrap()
}

/// The search index tools in `harness`'s session, as Arko has them where
/// the index store opens: the rules are about them.
fn with_index_tools(harness: &mut Harness, temp: &Path) {
    let indexes =
        index::Indexes::open(&temp.join("indexes"), &harness.settings.workspace, None, false)
            .unwrap();
    harness.tools.extend(indexes.tools());
    harness.indexes = Some(indexes);
    harness.open(false).unwrap();
}

/// The feed's items of `kind`, oldest first.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

/// Whether the feed has a notice that is `text`.
fn says(feed: &remote::Feed, text: &str) -> bool {
    texts(feed, "notice").iter().any(|said| said == text)
}

/// New sessions give the extra rules to agents on small models. With
/// --extra-rules every agent gets them, with --no-extra-rules none does, in
/// -p runs as in sessions someone takes part in. The flags can't be given
/// together.
#[test]
fn small_models_get_them_by_default_and_the_flags_override_it() {
    assert_eq!(extra_rules_for(&args(&[])), ExtraRules::Small);
    assert_eq!(extra_rules_for(&args(&["-p", "Hi"])), ExtraRules::Small);
    assert_eq!(extra_rules_for(&args(&["--extra-rules"])), ExtraRules::All);
    assert_eq!(
        extra_rules_for(&args(&["--no-extra-rules", "-p", "Hi"])),
        ExtraRules::Off
    );
    assert!(
        Args::try_parse_from(["arko", "--extra-rules", "--no-extra-rules"]).is_err(),
        "one or the other"
    );
}

/// The session keeps the choice in its settings. Off is left out, and a
/// session saved before the rules loads with them off, as its saved agents
/// were built.
#[test]
fn the_choice_is_saved_with_the_session_and_old_sessions_load_with_it_off() {
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
        extra_rules: ExtraRules::Small,
        ..Settings::for_test(temp.path(), &model)
    };
    let file = session.dir.join("session.json");
    let saved = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
    };
    for (rules, name) in [(ExtraRules::Small, "small"), (ExtraRules::All, "all")] {
        settings.extra_rules = rules;
        session.save_settings(&settings).unwrap();
        assert_eq!(saved()["extra_rules"], name);
        assert_eq!(session.load_settings().unwrap().extra_rules, rules);
    }
    settings.extra_rules = ExtraRules::Off;
    session.save_settings(&settings).unwrap();
    assert!(saved().get("extra_rules").is_none(), "{}", saved());
    assert_eq!(session.load_settings().unwrap().extra_rules, ExtraRules::Off);
}

/// `/extra-rules` says who gets the rules, and which of the session's
/// agents that is. Before the conversation starts it changes the choice:
/// the agents are built again, and the session and the settings (the full
/// screen's, the web page's) have it. Once the conversation has started,
/// the session keeps it, and Arko says so.
#[tokio::test]
async fn extra_rules_change_before_the_conversation_starts_only() {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = bodies.clone();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        seen.lock().unwrap().push(body);
        Box::pin(async { complete("m1", "Hello.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    // Both agents on luna, a small model, with the rules off, and the
    // search index tools (no index covers the working directory).
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    with_index_tools(&mut harness, temp.path());
    let reading = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref());
    harness.publish_models();
    assert_eq!(reading(&harness).settings.extra_rules, ExtraRules::Off);
    assert!(!harness.main_config.instructions.contains(RULES));

    harness.extra_rules_command("", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Extra rules for small models: no agent gets them (off). Neither the main agent nor the workers get them. /extra-rules small, all or off changes that before the conversation starts."
            .to_string()
    ));
    harness.extra_rules_command("small", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Extra rules for small models: only agents on small models (Haiku, luna) get them. The main agent and the workers get them."
            .to_string()
    ));
    assert_eq!(harness.settings.extra_rules, ExtraRules::Small);
    assert!(harness.main_config.instructions.contains(RULES));
    assert!(harness.worker_config.instructions.contains(RULES));
    assert_eq!(
        harness.session.load_settings().unwrap().extra_rules,
        ExtraRules::Small
    );
    assert_eq!(reading(&harness).settings.extra_rules, ExtraRules::Small);
    harness.extra_rules_command("sometimes", Source::Terminal);
    assert!(
        texts(&harness.feed, "output")
            .contains(&"/extra-rules takes small, all or off (/extra-rules off).".to_string())
    );
    harness.extra_rules_command("off", Source::Terminal);
    assert!(!harness.main_config.instructions.contains(RULES));
    assert!(!harness.worker_config.instructions.contains(RULES));
    harness.extra_rules_command("small", Source::Terminal);

    let end = harness
        .turn(Prompt::User("Hi.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    // The model got the rules.
    let first = bodies.lock().unwrap()[0].clone();
    assert!(first.contains(RULES), "{first}");

    harness.extra_rules_command("off", Source::Web);
    assert!(says(
        &harness.feed,
        "From the web page: The session keeps its extra rules: only agents on small models (Haiku, luna) get them. A new session takes --extra-rules or --no-extra-rules."
    ));
    assert_eq!(harness.settings.extra_rules, ExtraRules::Small);
    assert!(harness.main_config.instructions.contains(RULES));
    harness.extra_rules_command("", Source::Terminal);
    assert!(texts(&harness.feed, "output").contains(
        &"Extra rules for small models: only agents on small models (Haiku, luna) get them (small). The main agent and the workers get them. The session keeps this."
            .to_string()
    ));
}

/// A resumed session keeps its choice: its agents are built with the rules
/// as they were saved, also when they are off and a new session would
/// give them.
#[tokio::test]
async fn a_resumed_session_keeps_its_extra_rules() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("m1", "Noted.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    with_index_tools(&mut harness, temp.path());
    harness.extra_rules_command("small", Source::Terminal);
    harness
        .turn(Prompt::User("Apples are red.".into()), &mut inputs)
        .await;

    // Arko closes, and the session is resumed with its saved settings.
    harness.settings = harness.session.load_settings().unwrap();
    assert_eq!(harness.settings.extra_rules, ExtraRules::Small);
    let setup = providers::Setup::new(harness.settings.openai_auth);
    let (main, worker) = harness.agent_configs(&harness.settings, &setup).unwrap();
    assert!(main.instructions.contains(RULES));
    assert!(worker.instructions.contains(RULES));

    // A session saved before the rules (no field) loads with them off.
    let mut old = harness.settings.clone();
    old.extra_rules = ExtraRules::Off;
    harness.session.save_settings(&old).unwrap();
    let loaded = harness.session.load_settings().unwrap();
    assert_eq!(loaded.extra_rules, ExtraRules::Off);
    let (main, worker) = harness.agent_configs(&loaded, &setup).unwrap();
    assert!(!main.instructions.contains(RULES));
    assert!(!worker.instructions.contains(RULES));
}
