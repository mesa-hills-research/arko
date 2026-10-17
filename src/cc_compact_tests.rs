// End-to-end tests of what agents on cc: models do at the handover size:
// Claude Code compacts their conversation (the default), or they hand over.
// The flags choose it for a new session, the session keeps it, and
// `/cc-compact` changes it before the conversation starts. (Included in
// `main`'s tests, whose helpers they use.) Which configuration compacts is
// tested in `agents`.

use crate::mock_model::{ModelServer, complete};

fn args(words: &[&str]) -> Options {
    Args::parse_from(std::iter::once("arko").chain(words.iter().copied()))
        .options()
        .unwrap()
}

/// The feed's items of `kind`, oldest first.
fn texts(feed: &remote::Feed, kind: &str) -> Vec<String> {
    shown(feed)
        .into_iter()
        .filter(|(shown, _, _)| shown == kind)
        .map(|(_, text, _)| text)
        .collect()
}

/// Whether Arko answered with `text`.
fn answered(harness: &Harness, text: &str) -> bool {
    texts(&harness.feed, "output").iter().any(|said| said == text)
}

/// New sessions compact in Claude Code, and --no-cc-compact has them hand
/// over. The session saves only handing over, so a session saved before
/// the option loads compacting, as its agents were built.
#[test]
fn new_sessions_compact_unless_the_flag_says_hand_over() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    let new = |words: &[&str]| {
        let mut all = vec!["-m", "cc:opus", "-C", dir];
        all.extend(words);
        new_settings(&args(&all), &NoPrompter).unwrap().0
    };
    assert!(!new(&[]).cc_handover);
    assert!(!new(&["--cc-compact"]).cc_handover);
    assert!(new(&["--no-cc-compact", "-p", "Hi"]).cc_handover);
    assert!(
        Args::try_parse_from(["arko", "--cc-compact", "--no-cc-compact"]).is_err(),
        "one or the other"
    );

    let session = Session::create(temp.path()).unwrap();
    let file = session.dir.join("session.json");
    let saved = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
    };
    let mut settings = new(&[]);
    session.save_settings(&settings).unwrap();
    assert!(saved().get("cc_handover").is_none(), "{}", saved());
    assert!(!session.load_settings().unwrap().cc_handover);
    settings.cc_handover = true;
    session.save_settings(&settings).unwrap();
    assert_eq!(saved()["cc_handover"], true);
    assert!(session.load_settings().unwrap().cc_handover);
}

/// `/cc-compact` changes nothing in a session with no agent in Claude
/// Code, and says why. With workers on a cc: model, it changes the choice
/// before the conversation starts: the agents are built again, and the
/// session and the settings (the full screen's, the web page's) have it.
/// Once the conversation has started, the session keeps it, and Arko says
/// so. A resumed session's agents are built with the choice it saved.
#[tokio::test]
async fn cc_compact_changes_before_the_conversation_starts_only() {
    let server = ModelServer::start(Arc::new(|_, _| {
        Box::pin(async { complete("m1", "Hello.") })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    let reading = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref());
    let not_in_claude_code = "The agents aren't in Claude Code, so they hand over at the handover size and /cc-compact changes nothing in this session.";
    harness.cc_compact_command("off", Source::Terminal);
    assert!(answered(&harness, not_in_claude_code));
    assert!(!harness.settings.cc_handover);

    // The main agent on luna (the mock model), and the workers on cc:opus.
    let luna = harness.settings.main.clone();
    let cc = ModelSettings {
        model: "cc:opus".into(),
        ..luna.clone()
    };
    let managers = on_models(&mut harness, &luna, &cc, &providers::Setup::new(None));
    harness.publish_models();
    assert!(reading(&harness).settings.cc_compact);
    assert!(agents::compacts(&harness.worker_config));
    harness.cc_compact_command("", Source::Terminal);
    assert!(answered(
        &harness,
        "Agents in Claude Code compact at the handover size. Claude Code compacts the conversation, and the agent goes on in it. /cc-compact off changes that before the conversation starts."
    ));
    harness.cc_compact_command("off", Source::Terminal);
    assert!(answered(
        &harness,
        "Agents in Claude Code now hand over at the handover size. The agent writes a report and goes on from it in a fresh conversation."
    ));
    assert!(harness.settings.cc_handover);
    assert!(!agents::compacts(&harness.worker_config));
    assert!(harness.worker_config.runs_in_claude_code());
    assert!(harness.session.load_settings().unwrap().cc_handover);
    assert!(!reading(&harness).settings.cc_compact);
    harness.cc_compact_command("off", Source::Terminal);
    assert!(answered(
        &harness,
        "Agents in Claude Code hand over at the handover size already. The agent writes a report and goes on from it in a fresh conversation."
    ));
    harness.cc_compact_command("sometimes", Source::Terminal);
    assert!(answered(
        &harness,
        "/cc-compact takes on or off (/cc-compact off)."
    ));

    let end = harness
        .turn(Prompt::User("Hi.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.cc_compact_command("on", Source::Web);
    assert!(texts(&harness.feed, "notice").contains(
        &"Changed from the web page. This session keeps what its agents in Claude Code do at the handover size. The agent writes a report and goes on from it in a fresh conversation. A new session takes --cc-compact or --no-cc-compact."
            .to_string()
    ));
    assert!(harness.settings.cc_handover);
    assert!(!agents::compacts(&harness.worker_config));

    // Arko closes, and the session is resumed with its saved settings.
    let resumed = harness.session.load_settings().unwrap();
    assert!(resumed.cc_handover);
    let setup = providers::Setup::new(None);
    let (_, worker) = harness.agent_configs(&resumed, &setup).unwrap();
    assert!(!agents::compacts(&worker));
    let compacting = Settings {
        cc_handover: false,
        ..resumed
    };
    let (_, worker) = harness.agent_configs(&compacting, &setup).unwrap();
    assert!(agents::compacts(&worker));
    managers.shutdown().await;
}
