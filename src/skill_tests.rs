// End-to-end tests of the skills agents save: the main agent saves one and
// loads it in the same turn while its prompt cache holds, the next
// conversation lists it, and `/skills` says who saved it, shows it and
// removes it. Each follows a path a user takes, with the mock model.
// (Included in `main`'s tests, whose helpers they use.)

use crate::builtin_skills::BuiltinSkills;
use crate::mock_model::{ModelServer, complete, tool_call};

/// The agents of `harness` as a session builds them, with the skill folders
/// a session has (`home` as the home directory) and no built-in skills.
fn configure(harness: &mut Harness, home: &Path) {
    let folders = skills::folders_in(Some(&harness.settings.workspace), &harness.root, Some(home));
    harness.settings.builtin_skills = BuiltinSkills {
        none: true,
        off: Vec::new(),
    };
    harness.details = Arc::new(details::Details::new(
        harness.session.id.clone(),
        harness.session.dir.clone(),
        harness.root.clone(),
        harness.gauges.clone(),
        None,
        folders.clone(),
        mcp::Servers::default(),
        Vec::new(),
    ));
    harness.skill_folders = folders;
    let setup = providers::Setup::new(None);
    let agent_settings = AgentSettings {
        skill_folders: &harness.skill_folders.clone(),
        ..AgentSettings::for_session(&harness.settings, &setup)
    };
    harness.main_config = agents::main_config(&agent_settings).unwrap();
    harness.worker_config = agents::worker_config(&agent_settings).unwrap();
    harness.open(false).unwrap();
}

/// A skill folder `name` the agent wrote in `folder`.
fn draft(folder: &Path, name: &str) -> PathBuf {
    let dir = folder.join(name);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: How this project writes {name}.\n---\n\nWrite {name} in the past tense.\n"),
    )
    .unwrap();
    std::fs::write(dir.join("checklist.md"), "- Tag it.\n").unwrap();
    std::fs::write(dir.join("scripts/release.sh"), "echo release\n").unwrap();
    dir
}

/// The tools a request offers, by name.
fn tools(body: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

/// The parts of a request the prompt cache holds: its tool list and its
/// system prompt, as sent.
fn cached_prefix(body: &str) -> (String, String) {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    (body["tools"].to_string(), body["instructions"].to_string())
}

/// The main agent saves a skill it wrote and loads it in the same turn.
/// The requests after the save send the same tool list and system prompt,
/// byte for byte, so the prompt cache holds. The next conversation lists
/// the skill, and none before it did. With no skill at all at the start,
/// `load_skill` was offered already, with `add_skill` and `remove_skill`.
#[tokio::test]
async fn a_saved_skill_loads_at_once_and_the_cache_holds() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("Second.") {
                    complete("n1", "Listed.")
                } else if !body.contains("call_s1") {
                    tool_call(
                        "s1",
                        "add_skill",
                        serde_json::json!({
                            "name": "notes",
                            "source": "drafts/notes",
                            "why": "Later conversations need the steps."
                        }),
                    )
                } else if !body.contains("call_s2") {
                    tool_call("s2", "load_skill", serde_json::json!({ "name": "notes" }))
                } else {
                    complete("s3", "Saved and loaded.")
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    configure(&mut harness, &temp.path().join("home"));
    draft(&harness.settings.workspace.join("drafts"), "notes");
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Save a skill.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 3);
    let offered = tools(&sent[0]);
    for tool in ["load_skill", "add_skill", "remove_skill"] {
        assert!(offered.iter().any(|name| name == tool), "{tool}: {offered:?}");
    }
    assert!(sent[1].contains(
        "Saved the skill notes for this project. load_skill loads it now, and your skill list shows it from the next conversation."
    ));
    assert!(sent[2].contains("Write notes in the past tense."));
    assert!(sent[2].contains("checklist.md"));
    for body in &sent[1..] {
        assert_eq!(cached_prefix(body), cached_prefix(&sent[0]));
    }
    assert!(!sent[0].contains("**notes**"));
    let saved = harness.settings.workspace.join(".arko/skills/notes");
    assert!(saved.join("scripts/release.sh").is_file());
    assert!(!harness.settings.workspace.join(".agents").exists());

    harness.new_conversation();
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Second.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    let next = bodies.lock().unwrap().last().unwrap().clone();
    assert!(next.contains("**notes**"), "the next conversation lists it");
    assert!(next.contains("How this project writes notes."));
}

/// A session saved before agents saved skills is told it lacks them, by
/// name.
#[test]
fn a_session_saved_before_skill_saves_is_told_it_lacks_them() {
    let opus = ModelSettings {
        model: resolve_model("opus").unwrap().id,
        effort: Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let folders = [PathBuf::from("/work/.claude/skills")];
    let settings = AgentSettings {
        skill_folders: &folders,
        ..AgentSettings::for_test(&opus, &opus)
    };
    let fresh = agents::main_config(&settings).unwrap();
    let mut old = fresh.clone();
    old.tool_config
        .available_tool_names
        .retain(|name| !skill_install::TOOL_NAMES.contains(&name.as_str()));
    let saved = SavedSetup {
        name: WORKFLOW_NAME.into(),
        mcp_tools: Default::default(),
        configs: [(MAIN.to_string(), old)].into(),
    };
    assert_eq!(
        unoffered(&saved, &[(MAIN, &fresh)]).unwrap(),
        "This session started without saving skills (add_skill), so its agents don't get them. A new session does."
    );
    // Workers never save skills, and their Skills section says nothing of
    // it.
    let worker = agents::worker_config(&settings).unwrap();
    assert!(
        !worker
            .tool_config
            .available_tool_names
            .iter()
            .any(|name| skill_install::TOOL_NAMES.contains(&name.as_str()))
    );
    assert!(fresh.instructions.contains(agents::SAVING_SKILLS));
    assert!(!worker.instructions.contains("add_skill"));
}

/// Prints to a list of lines.
#[derive(Clone, Default)]
struct Printed(Arc<Mutex<Vec<String>>>);

impl crate::ui::Printer for Printed {
    fn print(&mut self, text: String) -> Result<(), ()> {
        self.0
            .lock()
            .unwrap()
            .extend(text.lines().map(str::to_string));
        Ok(())
    }
}

impl Printed {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// `/skills` says which skill an agent saved and when, the models a skill
/// is for and one only the main agent gets. `/skills NAME` shows one in
/// full, and `/skills remove NAME` removes only one an agent saved.
#[tokio::test]
async fn skills_names_shows_and_removes_what_agents_saved() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let printed = Printed::default();
    harness.output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
    let home = temp.path().join("home");
    let workspace = harness.settings.workspace.clone();
    // The user's own skill, there as the session starts.
    let own = workspace.join(".claude/skills/quirks");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(
        own.join("SKILL.md"),
        "---\nname: quirks\ndescription: Haiku's habits.\nmodels: [haiku, cc:haiku]\n---\nMind them.\n",
    )
    .unwrap();
    configure(&mut harness, &home);
    // An agent's skill, saved through the tool.
    draft(&workspace.join("drafts"), "notes");
    let context = harness.skill_context(&harness.main_config);
    let add = skill_install::tools(context)
        .into_iter()
        .find(|tool| tool.name() == "add_skill")
        .unwrap();
    let params: std::collections::HashMap<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
        "name": "notes",
        "source": "drafts/notes",
        "why": "The release checklist changed.",
        "workers": false
    }))
    .unwrap();
    let call = arko_agents::TaskContext::new(uuid::Uuid::new_v4(), MAIN, "", &workspace);
    add.execute(&call, params).await.unwrap();
    printed.take();

    harness.list_skills_within(200);
    let lines = printed.take();
    let today = skill_install::day(&chrono::Utc::now());
    let saved = workspace.join(".arko/skills/notes");
    assert!(
        lines[0].ends_with(&format!(
            "  {}, added by the agent {today}, main agent only",
            saved.display()
        )),
        "{lines:?}"
    );
    assert!(lines[0].starts_with("notes "), "{lines:?}");
    assert!(
        lines[1].ends_with(&format!("  {}, for haiku and cc:haiku", own.display())),
        "{lines:?}"
    );

    harness.command("/skills notes");
    let lines = printed.take();
    let year = chrono::Local::now().format("%Y");
    assert_eq!(
        lines,
        [
            "notes".to_string(),
            "How this project writes notes.".into(),
            format!("{}, out of git", saved.display()),
            format!(
                "Added by the agent on {today} {year}, from {}",
                workspace.join("drafts/notes").display()
            ),
            "why      The release checklist changed.".into(),
            "workers  main agent only".into(),
            "models   all".into(),
            format!(
                "files    SKILL.md ({} characters), checklist.md, scripts/release.sh",
                std::fs::read_to_string(saved.join("SKILL.md"))
                    .unwrap()
                    .chars()
                    .count()
            ),
            "/skills remove notes removes it.".into(),
        ]
    );
    harness.command("/skills quirks");
    let lines = printed.take();
    assert_eq!(lines[3], "workers  get it");
    assert_eq!(lines[4], "models   haiku, cc:haiku");
    assert!(!lines.iter().any(|line| line.contains("remove")));
    harness.command("/skills nothing");
    assert_eq!(printed.take(), ["There's no skill named nothing."]);

    harness.command("/skills remove quirks");
    assert_eq!(
        printed.take(),
        [format!(
            "quirks wasn't added by an agent. Delete {} to remove it.",
            own.display()
        )]
    );
    assert!(own.is_dir());
    harness.command("/skills remove pdff");
    assert_eq!(printed.take(), ["There's no skill named pdff."]);
    harness.command("/skills remove notes");
    assert_eq!(printed.take(), ["Removed the skill notes."]);
    assert!(!saved.exists());
    assert!(crate::agent_added::skills(&harness.root).is_empty());
    harness.command("/skills remove notes");
    assert_eq!(printed.take(), ["There's no skill named notes."]);
}
