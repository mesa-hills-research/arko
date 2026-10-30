// End-to-end tests of the built-in skills: the agents are offered them and
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

/// The agents see the built-in skills' names and descriptions, and load one
/// by name like any other skill. Its text is not in the instructions.
#[tokio::test]
async fn the_built_in_skills_are_offered_and_load_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        Vec::new(),
        Some("user-facing-prose"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    for name in ["user-facing-prose", "repo-docs-style", "ponytail"] {
        assert!(bodies[0].contains(&format!("**{name}**")), "{name}");
    }
    assert!(
        bodies[0]
            .contains("Skills are instructions for particular kinds of work, built into Arko.")
    );
    assert!(bodies[0].contains("Load it before writing or reviewing any of these."));
    // (A description on folded lines reaches the agents whole.)
    assert!(bodies[0].contains("complains about over-engineering."));
    assert!(bodies[0].contains("load_skill"));
    // On demand: the text comes with the call.
    assert!(!bodies[0].contains("# User-facing prose"));
    assert!(bodies[1].contains("--- BEGIN SKILL: user-facing-prose ---"));
    assert!(bodies[1].contains("# User-facing prose"));
}

/// A skill of the same name in a skill folder replaces the built-in one,
/// which the agents then don't see. The folder's other skills and the
/// built-in ones that nothing replaces are all there.
#[tokio::test]
async fn a_skill_of_the_same_name_replaces_a_built_in_one() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("skills");
    own_skill(&folder, "repo-docs-style", "Write the way my team does.");
    own_skill(&folder, "review", "Review a diff.");
    let bodies = turn_loading(
        temp.path(),
        BuiltinSkills::default(),
        vec![folder.clone()],
        Some("repo-docs-style"),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0].matches("**repo-docs-style**").count(), 1);
    assert!(bodies[0].contains("My repo-docs-style skill."));
    assert!(!bodies[0].contains("Rules for repository documentation"));
    for name in ["user-facing-prose", "review"] {
        assert!(bodies[0].contains(&format!("**{name}**")), "{name}");
    }
    assert!(bodies[0].contains("built into Arko and from these folders:"));
    assert!(bodies[1].contains("Write the way my team does."));
    assert!(!bodies[1].contains("# Repository documentation style"));
}

/// A session left without some or all built-in skills doesn't offer them,
/// and without any skill has no Skills section and no `load_skill`.
#[tokio::test]
async fn left_out_built_in_skills_are_not_offered() {
    let temp = tempfile::tempdir().unwrap();
    let one = BuiltinSkills {
        none: false,
        off: vec!["repo-docs-style".into()],
    };
    let bodies = turn_loading(temp.path(), one, Vec::new(), None).await;
    assert!(bodies[0].contains("**user-facing-prose**"));
    assert!(!bodies[0].contains("repo-docs-style"));
    let none = BuiltinSkills {
        none: true,
        off: Vec::new(),
    };
    let bodies = turn_loading(temp.path(), none.clone(), Vec::new(), None).await;
    assert!(!bodies[0].contains("user-facing-prose"));
    assert!(!bodies[0].contains("repo-docs-style"));
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

/// What a front end reads of the skills: which are built in, which a skill
/// of the user's replaces and from where, and which are left out.
#[tokio::test]
async fn the_facts_say_which_skills_are_built_in_and_which_are_left_out() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
    let folder = temp.path().join("skills");
    own_skill(&folder, "review", "Review a diff.");
    own_skill(&folder, "repo-docs-style", "Mine.");
    with_folders(&mut harness, vec![folder.clone()]);
    harness.settings.builtin_skills = BuiltinSkills {
        none: false,
        off: vec!["user-facing-prose".into()],
    };
    harness.publish_details();
    let facts = harness.details.skills().unwrap();
    assert_eq!(facts.builtin_off, ["user-facing-prose"]);
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
            (
                "repo-docs-style",
                false,
                Some(folder.join("repo-docs-style"))
            ),
            ("review", false, None),
            ("ponytail", true, None),
        ]
    );
    assert_eq!(facts.skills[1].path, Some(folder.join("review")));
    assert_eq!(facts.skills[2].path, None);
    // With every built-in skill on and no replacement, they follow the
    // folders' skills.
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
            ("user-facing-prose", true, false),
            ("repo-docs-style", true, false),
            ("ponytail", true, false)
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
    own_skill(&folder, "repo-docs-style", "Mine.");
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
        off: vec!["user-facing-prose".into()],
    };
    harness.publish_details();
    harness.list_skills_within(160);
    let text = printed.0.lock().unwrap().join("\n");
    let lines: Vec<&str> = text.lines().collect();
    // By name, the names in a column of their own, and no line over the width.
    let names = ["always-on", "ponytail", "repo-docs-style", "wide"];
    for (line, name) in lines.iter().zip(names) {
        assert!(line.starts_with(&format!("{name:<15}  ")), "{text}");
    }
    assert!(lines.iter().all(|line| line.width() <= 160), "{text}");
    // Each says where it is, in a column of its own (the built-in one that
    // it is built in), and nothing says what replaces what.
    let places = [
        format!("{}, always loaded", always.display()),
        "built in".to_string(),
        folder.join("repo-docs-style").display().to_string(),
        wide.display().to_string(),
    ];
    for (line, place) in lines.iter().zip(&places) {
        assert!(line.ends_with(&format!("  {place}")), "{text}");
        assert_eq!(line.width() - place.width(), lines[0].width() - places[0].width(), "{text}");
    }
    // A description is whole or cut with an ellipsis, wide characters too.
    assert!(lines[0].contains("  My always-on skill. "), "{text}");
    assert!(lines[1].contains("…  built in"), "{text}");
    assert!(lines[2].contains("  My repo-docs-style skill. "), "{text}");
    assert!(lines[3].contains("…  "), "{text}");
    assert!(!lines[3].contains(&"日本語".repeat(34)), "{text}");
    assert_eq!(
        lines[4],
        "Built-in skills left out (user-facing-prose).",
        "{text}"
    );
    assert_eq!(lines.len(), 5, "{text}");
    // Of the four skills, the three in the folder are the user's own.
    assert_eq!(harness.header(None).tools, ["3 skills"]);

    // On a narrow terminal the folders lose their leading folders, so each
    // skill still has a line.
    printed.0.lock().unwrap().clear();
    harness.list_skills_within(78);
    let text = printed.0.lock().unwrap().join("\n");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.iter().all(|line| line.width() <= 78), "{text}");
    assert!(lines[0].ends_with("  …/skills/always-on, always loaded"), "{text}");
    assert_eq!(lines.len(), 5, "{text}");

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
        text.contains("Built-in skills left out (user-facing-prose, repo-docs-style, ponytail)."),
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
        off: vec!["repo-docs-style".into()],
    };
    configure(&mut harness, one, Vec::new());
    let skills = harness.main_config.skills_config.as_mut().unwrap();
    assert_eq!(skills.embedded.len(), 2);
    skills.embedded[0] = arko_agents::EmbeddedSkill::new(
        "user-facing-prose",
        "---\nname: user-facing-prose\ndescription: Reworded for a later Arko.\n---\n\nNew text.\n",
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
    assert!(resumed.contains("**repo-docs-style**"), "left out later");
    assert!(resumed.contains("How to write any text a person reads"));
    assert!(!resumed.contains("Reworded for a later Arko."));
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
        ["user-facing-prose", "repo-docs-style", "ponytail"]
    );
    assert!(harness.live_command("/builtin-skills repo-docs-style off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "repo-docs-style is now off.");
    assert_eq!(
        given(&harness.main_config),
        ["user-facing-prose", "ponytail"]
    );
    assert_eq!(
        given(&harness.worker_config),
        ["user-facing-prose", "ponytail"]
    );
    assert!(harness.live_command("/builtin-skills", Source::Terminal));
    assert!(
        last_said(&harness.feed).starts_with(
            "Built-in skills are on, so the agents get user-facing-prose and ponytail. repo-docs-style is left out. /builtin-skills"
        )
    );
    assert!(harness.live_command("/builtin-skills ponytail off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "ponytail is now off.");
    assert_eq!(given(&harness.main_config), ["user-facing-prose"]);
    assert!(harness.live_command("/builtin-skills", Source::Terminal));
    assert!(last_said(&harness.feed).starts_with(
        "Built-in skills are on, so the agents get user-facing-prose. repo-docs-style and ponytail are left out."
    ));
    let saved = harness.session.load_settings().unwrap().builtin_skills;
    assert_eq!(saved, harness.settings.builtin_skills);
    assert_eq!(saved.off, ["repo-docs-style", "ponytail"]);
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
            ("user-facing-prose", true),
            ("repo-docs-style", false),
            ("ponytail", false)
        ]
    );
    assert!(harness.live_command("/builtin-skills repo-docs-style off", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "repo-docs-style is off already.");
    assert!(harness.live_command("/builtin-skills repo-docs-style on", Source::Web));
    assert!(
        last_said(&harness.feed)
            .starts_with("Changed from the web page. repo-docs-style is now on."),
        "{}",
        last_said(&harness.feed)
    );
    // Off for all leaves each name's own switch as it was.
    assert!(harness.live_command("/builtin-skills user-facing-prose off", Source::Terminal));
    assert!(harness.live_command("/builtin-skills off", Source::Terminal));
    assert!(given(&harness.main_config).is_empty());
    assert!(harness.live_command("/builtin-skills repo-docs-style on", Source::Terminal));
    assert_eq!(last_said(&harness.feed), "repo-docs-style is on already.");
    assert!(harness.live_command("/builtin-skills user-facing-prose on", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "user-facing-prose is now on. Built-in skills are off, so the agents get none until you turn them on."
    );
    assert!(harness.settings.builtin_skills.none);
    for (bad, said) in [
        (
            "repo-doc-style off",
            "repo-doc-style isn't a built-in skill. They are user-facing-prose, repo-docs-style, and ponytail.",
        ),
        (
            "maybe",
            "/builtin-skills takes on or off, or a built-in skill's name and on or off, such as /builtin-skills repo-docs-style off.",
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
    assert!(first.contains("**repo-docs-style**"));
    assert!(harness.live_command("/builtin-skills repo-docs-style off", Source::Terminal));
    assert_eq!(
        last_said(&harness.feed),
        "repo-docs-style is now off. The conversation going on keeps its skills. New workers and new conversations get the change."
    );
    let going_on = say(&mut harness, "Second.").await;
    assert!(going_on.contains("**repo-docs-style**"), "kept");
    harness.new_conversation();
    let new = say(&mut harness, "Third.").await;
    assert!(!new.contains("**repo-docs-style**"), "left out");
    assert!(new.contains("**ponytail**"));
    assert_eq!(
        harness.session.load_settings().unwrap().builtin_skills.off,
        ["repo-docs-style"]
    );
    harness.crew.as_ref().unwrap().shutdown().await;
}
