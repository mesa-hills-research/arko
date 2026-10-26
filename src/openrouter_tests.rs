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
