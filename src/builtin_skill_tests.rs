// End-to-end tests of the built-in skills. The agents are offered them and
// load them by name, a skill of the same name in a skill folder replaces one,
// a session leaves out those it is told to, and `/skills` and the facts say
// which they are. Each follows a path a user takes, with the mock model.
// (Included in `main`'s tests, whose helpers they use.)

use crate::builtin_skills::BuiltinSkills;
use crate::details::SkillFacts;
use crate::mock_model::{ModelServer, complete, tool_call};

/// A skill of the user's in `folder`, with `text` as its instructions.
fn own_skill(folder: &Path, name: &str, text: &str) {
    let dir = folder.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: My {name} skill.\n---\n\n{text}\n"),
    )
    .unwrap();
}

/// `harness` reads its skills from `folders`.
fn with_folders(harness: &mut Harness, folders: Vec<PathBuf>) {
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
}

/// The agents of `harness` as a session builds them with the skill
/// `folders` and the built-in skills `builtin` gives.
fn configure(harness: &mut Harness, builtin: BuiltinSkills, folders: Vec<PathBuf>) {
    harness.settings.builtin_skills = builtin;
    harness.skill_folders = folders;
    let setup = providers::Setup::new(None);
    let agent_settings = AgentSettings {
        skill_folders: &harness.skill_folders.clone(),
        ..AgentSettings::for_session(&harness.settings, &setup)
    };
    harness.main_config = agents::main_config(&agent_settings).unwrap();
    harness.worker_config = agents::worker_config(&agent_settings).unwrap();
}

/// A session in `temp` with the skill `folders` and the built-in skills
/// `builtin` gives, on a model that loads the skill `load` (if any) and
/// then ends its turn. It runs one turn. Returns the requests' bodies.
async fn turn_loading(
    temp: &Path,
    builtin: BuiltinSkills,
    folders: Vec<PathBuf>,
    load: Option<&'static str>,
) -> Vec<String> {
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body.clone());
            Box::pin(async move {
                match load {
                    Some(name) if !body.contains("call_k1") => {
                        tool_call("k1", "load_skill", serde_json::json!({ "name": name }))
                    }
                    _ => complete("k2", "Done."),
                }
            })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp, server.client(), true);
    configure(&mut harness, builtin, folders);
    harness.open(false).unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(30),
        harness.turn(Prompt::User("Use a skill.".into()), &mut inputs),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Answered));
    bodies.lock().unwrap().clone()
}

/// The text of the pre-loaded built-in skill, `prose`, is in the agents'
/// instructions. They see the other built-in skills' names and descriptions,
/// and load one by name like any other skill.
#[tokio::test]
async fn the_built_in_skills_are_offered_and_load_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        Vec::new(),
        Some("ponytail"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    for name in ["ponytail", "ui-taste"] {
        assert!(bodies[0].contains(&format!("**{name}**")), "{name}");
    }
    assert!(
        bodies[0]
            .contains("Skills are instructions for particular kinds of work, built into Arko.")
    );
    assert!(bodies[0].contains("Load it before designing, building or reviewing a screen"));
    // (A description on folded lines reaches the agents whole.)
    assert!(bodies[0].contains("complains about over-engineering."));
    assert!(bodies[0].contains("load_skill"));
    // Pre-loaded, the text is there from the start, and nothing offers it.
    assert!(bodies[0].contains("--- BEGIN SKILL: prose ---"));
    assert!(bodies[0].contains("# Prose"));
    assert!(!bodies[0].contains("**prose**"));
    // On demand, the text comes with the call.
    assert!(!bodies[0].contains("# Ponytail"));
    assert!(bodies[1].contains("--- BEGIN SKILL: ponytail ---"));
    assert!(bodies[1].contains("# Ponytail"));
}

/// A skill of the same name in a skill folder replaces the built-in one,
/// which the agents then don't see. The folder's other skills and the
/// built-in ones that nothing replaces are all there.
#[tokio::test]
async fn a_skill_of_the_same_name_replaces_a_built_in_one() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("skills");
    own_skill(&folder, "prose", "Write the way my team does.");
    own_skill(&folder, "review", "Review a diff.");
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        vec![folder.clone()],
        Some("prose"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0].matches("**prose**").count(), 1);
    assert!(bodies[0].contains("My prose skill."));
    assert!(!bodies[0].contains("BEGIN SKILL: prose"));
    for name in ["ponytail", "review"] {
        assert!(bodies[0].contains(&format!("**{name}**")), "{name}");
    }
    assert!(bodies[0].contains("built into Arko and from these folders:"));
    assert!(bodies[1].contains("Write the way my team does."));
    assert!(!bodies[1].contains("# Prose"));
    // It loads on demand now, so it isn't one that is there already.
    assert!(!bodies[0].contains("is already in your instructions"));
}

/// `worker-choice` loads on demand like `ponytail`, and what it says is
/// there once loaded.
#[tokio::test]
async fn worker_choice_is_offered_and_loads_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        Vec::new(),
        Some("worker-choice"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("**worker-choice**"));
    assert!(bodies[0].contains("Load it before starting a worker or saving a worker profile."));
    assert!(!bodies[0].contains("# Choosing a worker"));
    assert!(bodies[1].contains("--- BEGIN SKILL: worker-choice ---"));
    assert!(bodies[1].contains("# Choosing a worker"));
    assert!(bodies[1].contains("## Billing comes first"));
}

/// A skill whose text is in the agents' instructions can't be loaded again.
/// Asked for it, `load_skill` says it is there already, where the skill
/// system says it wasn't found, and the instructions name it.
#[tokio::test]
async fn load_skill_says_a_pre_loaded_skill_is_there_already() {
    let temp = tempfile::tempdir().unwrap();
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        Vec::new(),
        Some("prose"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("--- BEGIN SKILL: prose ---"));
    assert!(
        bodies[0].contains("The prose skill is already in your instructions, so don't load it."),
        "the instructions name it"
    );
    let answer = "The prose skill is already in your instructions, under Active Skills. Follow it from there.";
    assert!(bodies[1].contains(answer), "{}", bodies[1]);
    assert!(!bodies[1].contains("not found"));
    // A name that is no skill is still not found, with the skills to load.
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        Vec::new(),
        Some("proze"),
    )
    .await;
    assert!(bodies[1].contains("Skill 'proze' not found. Available skills:"));
    assert!(!bodies[1].contains("Follow it from there."));
}

/// The answer is for the agent that has the skill in its instructions. A skill
/// of the user's marked `auto_load` gets it, and one only the main agent gets
/// is not found by a worker.
#[tokio::test]
async fn a_skill_in_the_instructions_is_told_apart_from_one_that_is_not_found() {
    use arko_agents::{InterceptedResult, TaskContext, ToolResultInterceptor};
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let folder = temp.path().join("skills");
    let skill = |name: &str, extra: &str| {
        let dir = folder.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: My {name} skill.\n{extra}---\n\n{name}.\n"),
        )
        .unwrap();
    };
    skill("house-style", "auto_load: true\n");
    skill("lead-only", "auto_load: true\nworkers: false\n");
    skill("review", "");
    with_folders(&mut harness, vec![folder]);
    harness.settings.builtin_skills = BuiltinSkills::default();
    harness.publish_details();
    let interceptor = crate::skills::PreLoaded::new(harness.details.clone());
    let asked = async |tool: &str, agent: &str, name: &str, raw: &str| -> Option<String> {
        let params = std::collections::HashMap::from([(
            "name".to_string(),
            serde_json::Value::from(name),
        )]);
        let context = TaskContext::new(uuid::Uuid::nil(), agent, "", temp.path());
        match interceptor.intercept(tool, raw, &params, &context).await {
            Ok(InterceptedResult::Replaced(text)) => Some(text),
            Ok(InterceptedResult::Unchanged) => None,
            Err(error) => panic!("{error}"),
        }
    };
    let not_found = |name: &str| format!("Skill '{}' not found. Available skills:\n- review", name.trim());
    let there = |name: &str| {
        Some(format!(
            "The {name} skill is already in your instructions, under Active Skills. Follow it from there."
        ))
    };
    for name in ["prose", "house-style", " prose "] {
        for agent in [crate::agents::MAIN, crate::agents::WORKER] {
            let answer = asked("load_skill", agent, name, &not_found(name)).await;
            assert_eq!(answer, there(name.trim()), "{agent} {name}");
        }
    }
    // Only the main agent has this one, so a worker's skill list doesn't.
    let lead_only = not_found("lead-only");
    assert_eq!(
        asked("load_skill", crate::agents::MAIN, "lead-only", &lead_only).await,
        there("lead-only")
    );
    assert_eq!(
        asked("load_skill", crate::agents::WORKER, "lead-only", &lead_only).await,
        None
    );
    // A skill to load, a name that is none, a skill the system loaded, and
    // another tool's result are as they were.
    for (tool, name, raw) in [
        ("load_skill", "review", not_found("review")),
        ("load_skill", "no-such-skill", not_found("no-such-skill")),
        (
            "load_skill",
            "house-style",
            "--- BEGIN SKILL: house-style ---\nDo it.\n--- END SKILL: house-style ---".into(),
        ),
        ("shell", "prose", not_found("prose")),
    ] {
        let answer = asked(tool, crate::agents::MAIN, name, &raw).await;
        assert_eq!(answer, None, "{tool} {name}");
    }
}

/// A session left without some or all built-in skills doesn't offer them,
/// and without any skill has no Skills section and no `load_skill`.
#[tokio::test]
async fn left_out_built_in_skills_are_not_offered() {
    let temp = tempfile::tempdir().unwrap();
    let one = BuiltinSkills {
        none: false,
        off: vec!["prose".into()],
    };
    let bodies = turn_loading(temp.path(), one, Vec::new(), None).await;
    assert!(bodies[0].contains("**ponytail**"));
    assert!(!bodies[0].contains("BEGIN SKILL"));
    assert!(!bodies[0].contains("# Prose"));
    let none = BuiltinSkills {
        none: true,
        off: Vec::new(),
    };
    let bodies = turn_loading(temp.path(), none.clone(), Vec::new(), None).await;
    assert!(!bodies[0].contains("BEGIN SKILL"));
    assert!(!bodies[0].contains("**ponytail**"));
    assert!(!bodies[0].contains("# Skills"));
    assert!(!bodies[0].contains("load_skill"));
    // The user's own skills still come, without a word about built-in ones.
    let folder = temp.path().join("skills");
    own_skill(&folder, "review", "Review a diff.");
    let bodies = turn_loading(temp.path(), none, vec![folder], None).await;
    assert!(bodies[0].contains("**review**"));
    assert!(bodies[0].contains("from these folders:"));
    assert!(!bodies[0].contains("built into Arko"));
}

/// What a front end reads of the skills, meaning which are built in, which a skill
/// of the user's replaces and from where, and which are left out.
#[tokio::test]
async fn the_facts_say_which_skills_are_built_in_and_which_are_left_out() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let folder = temp.path().join("skills");
    own_skill(&folder, "review", "Review a diff.");
    own_skill(&folder, "ponytail", "Mine.");
    with_folders(&mut harness, vec![folder.clone()]);
    harness.settings.builtin_skills = BuiltinSkills {
        none: false,
        off: vec!["prose".into()],
    };
    harness.publish_details();
    let facts = harness.details.skills().unwrap();
    assert_eq!(facts.builtin_off, ["prose"]);
    let skills: Vec<(&str, bool, Option<PathBuf>)> = facts
        .skills
        .iter()
        .map(|skill| {
            (
                skill.name.as_str(),
                skill.builtin,
                skill.replaces_builtin.clone(),
            )
        })
        .collect();
    assert_eq!(
        skills,
        [
            ("ponytail", false, Some(folder.join("ponytail"))),
            ("review", false, None),
            ("ui-taste", true, None),
            ("worker-choice", true, None),
        ]
    );
    assert_eq!(facts.skills[1].path, Some(folder.join("review")));
    assert_eq!(facts.skills[2].path, None);
    // With every built-in skill on and no replacement, they follow the
    // folders' skills, the pre-loaded one last.
    with_folders(&mut harness, Vec::new());
    harness.settings.builtin_skills = BuiltinSkills::default();
    harness.publish_details();
    let facts = harness.details.skills().unwrap();
    assert!(facts.builtin_off.is_empty());
    let skills: Vec<(&str, bool, bool)> = facts
        .skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.builtin, skill.always))
        .collect();
    assert_eq!(
        skills,
        [
            ("ponytail", true, false),
            ("ui-taste", true, false),
            ("worker-choice", true, false),
            ("prose", true, true)
        ]
    );
    assert!(facts.skills.iter().all(|skill: &SkillFacts| {
        skill.replaces_builtin.is_none() && !skill.description.is_empty()
    }));
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

/// `/skills` has a line for each skill with its name, description and where
/// it comes from, the descriptions cut to fit the width (counted in
/// columns, so wide characters take two) and the folders shortened when the
/// width is narrow, and names the built-in skills left out. The banner
/// counts only the user's own.
#[tokio::test]
async fn skills_lists_each_skill_with_its_description_and_where_it_comes_from() {
    use unicode_width::UnicodeWidthStr;

    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let printed = Printed::default();
    harness.output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
    let folder = temp.path().join("skills");
    own_skill(&folder, "ui-taste", "Mine.");
    let skill_file = |name: &str, description: &str, extra: &str| {
        let dir = folder.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n{extra}---\n\n{name}.\n"),
        )
        .unwrap();
        dir
    };
    let always = skill_file("always-on", "My always-on skill.", "auto_load: true\n");
    let wide = skill_file("wide", &"日本語".repeat(34), "");
    with_folders(&mut harness, vec![folder.clone()]);
    harness.settings.builtin_skills = BuiltinSkills {
        none: false,
        off: vec!["ponytail".into()],
    };
    harness.publish_details();
    harness.list_skills_within(160);
    let text = printed.0.lock().unwrap().join("\n");
    let lines: Vec<&str> = text.lines().collect();
    // By name, the names in a column of their own, and no line over the width.
    let names = ["always-on", "prose", "ui-taste", "wide", "worker-choice"];
    for (line, name) in lines.iter().zip(names) {
        assert!(line.starts_with(&format!("{name:<9}  ")), "{text}");
    }
    assert!(lines.iter().all(|line| line.width() <= 160), "{text}");
    // Each says where it is, in a column of its own (the built-in ones that
    // they are built in), and nothing says what replaces what.
    let places = [
        format!("{}, always loaded", always.display()),
        "built in, always loaded".to_string(),
        folder.join("ui-taste").display().to_string(),
        wide.display().to_string(),
        "built in".to_string(),
    ];
    for (line, place) in lines.iter().zip(&places) {
        assert!(line.ends_with(&format!("  {place}")), "{text}");
        assert_eq!(line.width() - place.width(), lines[0].width() - places[0].width(), "{text}");
    }
    // A description is whole or cut with an ellipsis, wide characters too.
    assert!(lines[0].contains("  My always-on skill. "), "{text}");
    assert!(lines[1].contains("…  built in"), "{text}");
    assert!(lines[2].contains("  My ui-taste skill. "), "{text}");
    assert!(lines[3].contains("…  "), "{text}");
    assert!(!lines[3].contains(&"日本語".repeat(34)), "{text}");
    assert_eq!(lines[5], "Built-in skills left out (ponytail).", "{text}");
    assert_eq!(lines.len(), 6, "{text}");
    // Of the five skills, the three in the folder are the user's own.
    assert_eq!(harness.header(None).tools, ["3 skills"]);

    // On a narrow terminal the folders lose their leading folders, so each
    // skill still has a line.
    printed.0.lock().unwrap().clear();
    harness.list_skills_within(78);
    let text = printed.0.lock().unwrap().join("\n");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.iter().all(|line| line.width() <= 78), "{text}");
    assert!(lines[0].contains("  …/"), "{text}");
    assert!(
        lines[0].ends_with("/skills/always-on, always loaded"),
        "{text}"
    );
    assert_eq!(lines.len(), 6, "{text}");

    printed.0.lock().unwrap().clear();
    harness.settings.builtin_skills = BuiltinSkills {
        none: true,
        off: Vec::new(),
    };
    with_folders(&mut harness, Vec::new());
    harness.publish_details();
    harness.list_skills();
    let text = printed.0.lock().unwrap().join("\n");
    assert!(text.starts_with("No skills."), "{text}");
    assert!(
        text.contains("Built-in skills left out (prose, ponytail, ui-taste, worker-choice)."),
        "{text}"
    );
    assert!(harness.header(None).tools.is_empty());
}

/// A path under the home directory is written with a `~`.
#[test]
fn a_path_in_the_home_directory_is_written_with_a_tilde() {
    let home = Path::new("/home/ada");
    let skill = home.join(".claude/skills/review");
    assert_eq!(
        with_tilde(&skill, Some(home)),
        Path::new("~").join(".claude/skills/review").display().to_string()
    );
    assert_eq!(with_tilde(&skill, None), skill.display().to_string());
    assert_eq!(
        with_tilde(Path::new("/srv/skills/review"), Some(home)),
        "/srv/skills/review"
    );
}

/// A conversation saved with the built-in skills resumes after some were
/// left out and after their text changed (as a newer Arko's would), with
/// the skills it was saved with. A resumed session doesn't tell its agents
/// they lack skills they never had.
#[tokio::test]
async fn a_saved_conversation_resumes_after_the_built_in_skills_changed() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { complete("k1", "Done.") })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    configure(&mut harness, BuiltinSkills::default(), Vec::new());
    harness.open(false).unwrap();
    let end = harness
        .turn(Prompt::User("First.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    harness.crew.as_ref().unwrap().shutdown().await;

    // Another Arko builds its agents with one skill left out and another's
    // text changed.
    let one = BuiltinSkills {
        none: false,
        off: vec!["ponytail".into()],
    };
    configure(&mut harness, one, Vec::new());
    let skills = harness.main_config.skills_config.as_mut().unwrap();
    assert_eq!(skills.embedded.len(), 3);
    skills.embedded[0] = arko_agents::EmbeddedSkill::new(
        "prose",
        "---\nname: prose\ndescription: Reworded for a later Arko.\nauto_load: true\n---\n\nNew text.\n",
    );
    harness.open(true).unwrap();
    harness.start_new_workers();
    assert!(harness.unoffered.is_none(), "{:?}", harness.unoffered);
    let end = harness
        .turn(Prompt::User("Second.".into()), &mut inputs)
        .await;
    assert!(matches!(end, TurnEnd::Answered));
    let bodies = bodies.lock().unwrap().clone();
    let resumed = bodies
        .iter()
        .rfind(|body| body.contains("Second."))
        .unwrap();
    assert!(resumed.contains("**ponytail**"), "left out later");
    assert!(resumed.contains("# Prose"));
    assert!(!resumed.contains("New text."));
    harness.crew.as_ref().unwrap().shutdown().await;
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

/// The built-in skills `config`'s agent gets.
fn given(config: &arko_agents::AgentConfig) -> Vec<String> {
    config
        .skills_config
        .iter()
        .flat_map(|skills| skills.embedded.iter())
        .map(|skill| skill.name.clone())
        .collect()
}

/// `/builtin-skills` says what the agents get and changes it, for all the
/// built-in skills or one by name. Before the conversation starts the agents
/// are built again, and the clients read the choice from the status.
#[tokio::test]
async fn builtin_skills_sets_what_the_agents_get_before_the_conversation_starts() {
    let temp = tempfile::tempdir().unwrap();
    let server =
        ModelServer::start(Arc::new(|_, _| Box::pin(async { complete("k1", "Done.") }))).await;
    let (mut harness, _inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/builtin-skills", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "Built-in skills are off, so the agents get none of them. /builtin-skills on or off changes all of them, and /builtin-skills NAME on or off one."
    );
    assert!(harness.live_command("/builtin-skills on", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "Built-in skills are now on.");
    assert_eq!(
        given(&harness.main_config),
        ["prose", "ponytail", "ui-taste", "worker-choice"]
    );
    assert!(harness.live_command("/builtin-skills prose off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "prose is now off.");
    assert_eq!(
        given(&harness.main_config),
        ["ponytail", "ui-taste", "worker-choice"]
    );
    assert_eq!(
        given(&harness.worker_config),
        ["ponytail", "ui-taste", "worker-choice"]
    );
    assert!(harness.live_command("/builtin-skills", Source::Terminal));
    assert!(
        last_said(&harness.feed).starts_with(
            "Built-in skills are on, so the agents get ponytail, ui-taste, and worker-choice. prose is left out. /builtin-skills"
        ),
        "{}",
        last_said(&harness.feed)
    );
    assert!(harness.live_command("/builtin-skills ponytail off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "ponytail is now off.");
    assert_eq!(given(&harness.main_config), ["ui-taste", "worker-choice"]);
    assert!(harness.live_command("/builtin-skills", Source::Terminal));
    assert!(
        last_said(&harness.feed).starts_with(
            "Built-in skills are on, so the agents get ui-taste and worker-choice. prose and ponytail are left out."
        ),
        "{}",
        last_said(&harness.feed)
    );
    let saved = harness.session.load_settings().unwrap().builtin_skills;
    assert_eq!(saved, harness.settings.builtin_skills);
    assert_eq!(saved.off, ["prose", "ponytail"]);
    let live = remote::Meter::read(harness.gauges.as_ref()).settings;
    assert!(live.builtin_skills);
    let listed: Vec<(&str, bool)> = live
        .builtin_skill_list
        .iter()
        .map(|skill| (skill.name.as_str(), skill.on))
        .collect();
    assert_eq!(
        listed,
        [
            ("prose", false),
            ("ponytail", false),
            ("ui-taste", true),
            ("worker-choice", true)
        ]
    );
    assert!(harness.live_command("/builtin-skills prose off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "prose is off already.");
    assert!(harness.live_command("/builtin-skills prose on", Source::Web));
    assert!(
        last_said(&harness.feed).starts_with("Changed from the web page. prose is now on."),
        "{}",
        last_said(&harness.feed)
    );
    // Off for all leaves each name's own switch as it was.
    assert!(harness.live_command("/builtin-skills ui-taste off", Source::Terminal));
    assert!(harness.live_command("/builtin-skills worker-choice off", Source::Terminal));
    assert!(harness.live_command("/builtin-skills off", Source::Terminal));
    assert!(given(&harness.main_config).is_empty());
    assert!(harness.live_command("/builtin-skills prose on", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "prose is on already.");
    assert!(harness.live_command("/builtin-skills ui-taste on", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "ui-taste is now on. Built-in skills are off, so the agents get none until you turn them on."
    );
    assert!(harness.settings.builtin_skills.none);
    for (bad, said) in [
        (
            "pros off",
            "pros isn't a built-in skill. They are prose, ponytail, ui-taste, and worker-choice.",
        ),
        (
            "maybe",
            "/builtin-skills takes on or off, or a built-in skill's name and on or off, such as /builtin-skills prose off.",
        ),
    ] {
        assert!(harness.live_command(&format!("/builtin-skills {bad}"), Source::Terminal));
        assert_eq!(last_said(&harness.feed), said, "{bad}");
    }
    harness.crew.as_ref().unwrap().shutdown().await;
}

/// After the conversation has started, a change to the built-in skills
/// reaches the workflow at once. The conversation going on keeps its skills,
/// a new conversation gets the change, and the session saves it.
#[tokio::test]
async fn a_change_to_the_built_in_skills_after_the_start_reaches_the_next_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let server = ModelServer::start(Arc::new({
        let bodies = bodies.clone();
        move |_, body: String| {
            bodies.lock().unwrap().push(body);
            Box::pin(async { complete("k1", "Done.") })
        }
    }))
    .await;
    let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
    assert!(harness.live_command("/builtin-skills on", Source::Terminal));
    let mut say = async |harness: &mut Harness, text: &str| {
        let end = harness.turn(Prompt::User(text.into()), &mut inputs).await;
        assert!(matches!(end, TurnEnd::Answered));
        bodies.lock().unwrap().last().unwrap().clone()
    };
    let first = say(&mut harness, "First.").await;
    assert!(first.contains("**ponytail**"));
    assert!(harness.live_command("/builtin-skills ponytail off", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "ponytail is now off. The conversation going on keeps its skills. New workers and new conversations get the change."
    );
    let going_on = say(&mut harness, "Second.").await;
    assert!(going_on.contains("**ponytail**"), "kept");
    harness.new_conversation();
    let new = say(&mut harness, "Third.").await;
    assert!(!new.contains("**ponytail**"), "left out");
    assert!(new.contains("**ui-taste**"));
    assert_eq!(
        harness.session.load_settings().unwrap().builtin_skills.off,
        ["ponytail"]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}
