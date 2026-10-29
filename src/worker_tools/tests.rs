use std::time::Duration;

use super::*;
use crate::questions::ApprovalAnswer;

/// Gives an approval its answer, as the user's decision.
trait Decide {
    fn decide(self, answer: ApprovalAnswer);
}

impl Decide for crate::inputs::Answering {
    fn decide(self, answer: ApprovalAnswer) {
        match self {
            crate::inputs::Answering::Approval(sender) => sender.send(answer).unwrap(),
            crate::inputs::Answering::Text(_) => panic!("not an approval"),
        }
    }
}
use crate::agents::{ExtraRules, ModelSettings, resolve_model};
use crate::crew::{NewWorkers, Setup, WorkerSetup};
use crate::profiles::Roots;
use crate::remote;
use crate::ui::Output;

fn settings(name: &str, effort: Effort) -> ModelSettings {
    let model = resolve_model(name).unwrap();
    ModelSettings {
        model: providers::model_name(&model),
        effort,
        fast: false,
        cache_1h: false,
        handover_at: Some(crate::agents::default_handover(&model, None)),
    }
}

fn basis(temp: &std::path::Path, choice: ModelChoice) -> Basis {
    Basis {
        roots: Roots::new(temp.join("arko"))
            .workspace(temp.join("work"))
            .session(temp.join("session")),
        worker: settings("luna", Effort::High),
        providers: providers::Setup::new(Some(providers::OpenAiAuth::ChatGpt)),
        thinking_text: true,
        cc_compact: true,
        handover_at: 200_000,
        choice,
        worker_depth: 2,
        extra_rules: ExtraRules::Off,
        rules_text: None,
        billing: crate::billing::WorkerBilling::Api,
        api_budget: None,
        available: Default::default(),
    }
}

fn write(basis: &Basis, place: Place, name: &str, text: &str) {
    let folder = basis.roots.folder(place).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(format!("{name}.md")), text).unwrap();
}

/// What `keys` has: OpenAI only.
fn needs(model: &Model) -> Option<String> {
    match model.provider {
        Provider::OpenAI => None,
        Provider::ClaudeCode => Some("Claude Code signed in".to_string()),
        ref other => Some(providers::credential_name(other).to_string()),
    }
}

fn plans() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "ChatGPT plan".to_string(),
        "ChatGPT plan 5 h 12%, 7 d 40%".to_string(),
    )])
}

#[test]
fn the_options_list_the_profiles_and_the_models_usable_now() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), ModelChoice::Agent);
    write(
        &basis,
        Place::User,
        "3d-judge",
        "---\ndescription: Judges 3D results against a reference image\nmodel: sol\neffort: high\n---\nJudge.\n",
    );
    write(
        &basis,
        Place::Project,
        "quick-read",
        "---\ndescription: Reads files and answers short questions\nmodel: haiku\neffort: low\ncache: 5m\n---\n",
    );
    write(
        &basis,
        Place::Session,
        "reviewer",
        "---\ndescription: Reviews a diff\nmodel: luna\nmade_by: agent\nfast: true\n---\n",
    );
    write(
        &basis,
        Place::User,
        "splitter",
        "---\ndescription: Splits a review by file\nmodel: haiku\neffort: low\nnested: true\n---\n",
    );
    write(&basis, Place::Project, "notes", "---\nmodel sol\n---\n");
    write(
        &basis,
        Place::User,
        "future",
        "---\ndescription: From a newer Arko\nmodel: gpt9\n---\n",
    );
    let text = options_text(&basis, Show::All, &needs, &plans());

    // How workers are paid for comes first.
    assert!(
        text.starts_with(&format!(
            "{}\n\nWorker profiles (pass one as start_agent's profile)\n- default · gpt-6-luna, high effort · the session's worker model (/model worker)\n",
            crate::agents::billing_words(crate::billing::WorkerBilling::Api)
        )),
        "{text}"
    );
    assert!(text.contains(
        "\n- 3d-judge (user) · sol, high effort · Judges 3D results against a reference image"
    ));
    assert!(text.contains(
        "\n- quick-read (project) · haiku, low effort, 5m cache · Reads files and answers short questions · can't start yet, needs ANTHROPIC_API_KEY"
    ));
    assert!(text.contains("\n- reviewer (session, made by you) · luna, fast · Reviews a diff"));
    // A profile whose workers can start workers says so, and the others don't.
    assert!(
        text.contains(
            "\n- splitter (user) · haiku, low effort, can start workers · Splits a review by file"
        ),
        "{text}"
    );
    assert_eq!(text.matches("can start workers").count(), 1, "{text}");
    assert!(
        text.contains("\n- future (user) · gpt9 · From a newer Arko · can't start yet, `gpt9` isn't a model Arko knows."),
        "{text}"
    );
    assert!(text.contains("\nCan't be read\n- notes (project) · line 2 isn't `key: value`"));

    // The models: what the session reaches, with plan usage for the
    // ChatGPT login, and what it doesn't with how to get it.
    assert!(text.contains(
        "\n\nModels usable now (the plan that pays, or API and the price per million input and output tokens, and the size where a worker hands over to a fresh conversation)\n"
    ));
    assert!(
        text.contains("\n- sol · efforts low to max · ChatGPT plan 5 h 12%, 7 d 40%, shared with the user's own use · hands over at 250k · cache 30m, fixed"),
        "{text}"
    );
    assert!(text.contains("\n- luna · efforts low to max · "));
    assert!(text.contains("\nNot usable now\n"));
    assert!(text.contains(
        "\n- opus, fable, sonnet, haiku, mythos · needs ANTHROPIC_API_KEY. The user adds it with /keys."
    ));
    assert!(
        text.contains("\n- gemini, flash · needs GEMINI_API_KEY. The user adds it with /keys.")
    );
    assert!(text.contains(
        "\n- cc:opus, cc:fable, cc:sonnet, cc:haiku, cc:mythos · needs Claude Code signed in. The user signs in with `claude auth login`."
    ));
    assert!(text.contains("needs OPENROUTER_API_KEY. The user adds it with /keys."));

    // One part alone.
    let profiles_only = options_text(&basis, Show::Profiles, &needs, &plans());
    assert!(
        profiles_only.contains("\n\nWorker profiles") && !profiles_only.contains("Models usable")
    );
    let models_only = options_text(&basis, Show::Models, &needs, &plans());
    assert!(models_only.contains("\n\nModels usable now") && !models_only.contains("3d-judge"));
}

#[test]
fn a_claude_model_by_key_shows_its_prices_and_cache_and_a_plan_model_its_usage() {
    let temp = tempfile::tempdir().unwrap();
    let mut basis = basis(temp.path(), ModelChoice::Agent);
    basis.providers = providers::Setup::new(None);
    let everything = |_: &Model| None;
    let plans = BTreeMap::from([(
        "Claude plan".to_string(),
        "Claude plan 5 h 62%, 7 d 31%".to_string(),
    )]);
    let text = options_text(&basis, Show::Models, &everything, &plans);
    let opus = resolve_model("opus").unwrap();
    // (The prices of today, as the tool prints them: a model's price can
    // have a period that starts on a date.)
    let prices = crate::billing::api_label(&opus);
    assert!(prices.starts_with("API, $"), "{prices}");
    let line = text
        .lines()
        .find(|line| line.starts_with("- opus ·"))
        .unwrap();
    assert_eq!(
        line,
        format!(
            "- opus · efforts low to max · {prices} · hands over at 950k · cache short (5m) or long (1h, twice the input price to write)"
        )
    );
    assert!(text.contains(
        "\n- cc:opus · efforts low to max · Claude plan 5 h 62%, 7 d 31%, shared with the user's own use · hands over at 950k, Claude Code compacts · cache short (5m) or long (1h, when you choose none on a Claude plan)"
    ), "{text}");
    // Without an API key, OpenAI models keep the ChatGPT login's cache.
    let sol = text
        .lines()
        .find(|line| line.starts_with("- sol ·"))
        .unwrap();
    assert!(sol.ends_with(" · cache 30m, fixed"), "{sol}");
    assert!(text.contains("\n- gemini · efforts low to high · API, $"));
    assert!(text.contains("\n- openrouter:<slug> · any model OpenRouter serves · API · "));
    assert!(!text.contains("Not usable now"));
    // A session that hands over in Claude Code says so.
    basis.cc_compact = false;
    let text = options_text(&basis, Show::Models, &everything, &plans);
    assert!(
        text.contains("hands over at 950k · cache short (5m) or long (1h, when you choose none")
    );
    // On an API key, OpenAI's long cache is 12 hours, at 2.5 times the input
    // price to write.
    basis.providers = providers::Setup::new(Some(providers::OpenAiAuth::ApiKey));
    let text = options_text(&basis, Show::Models, &everything, &plans);
    let sol = text
        .lines()
        .find(|line| line.starts_with("- sol ·"))
        .unwrap();
    assert!(
        sol.ends_with(" · cache short (30m) or long (12h, 2.5 times the input price to write)"),
        "{sol}"
    );
}

#[test]
fn a_setting_that_limits_the_choice_is_said_first() {
    let temp = tempfile::tempdir().unwrap();
    let profiles_only = options_text(
        &basis(temp.path(), ModelChoice::Profiles),
        Show::Profiles,
        &needs,
        &plans(),
    );
    assert!(profiles_only.starts_with(
        "The user lets you start workers with a profile only (/worker-models profiles)"
    ));
    let session = options_text(
        &basis(temp.path(), ModelChoice::Session),
        Show::Profiles,
        &needs,
        &plans(),
    );
    assert!(session.starts_with(
        "The user has every worker run the session's worker model (/worker-models session)"
    ));
}

/// worker_options says how workers are paid for, and under Plans only a
/// profile on a model billed per token can't start.
#[test]
fn the_options_say_how_workers_are_paid_for() {
    let temp = tempfile::tempdir().unwrap();
    let mut basis = basis(temp.path(), ModelChoice::Agent);
    basis.providers = providers::Setup::new(None);
    write(
        &basis,
        Place::User,
        "careful",
        "---\ndescription: Reviews carefully\nmodel: opus\n---\n",
    );
    write(
        &basis,
        Place::User,
        "planned",
        "---\ndescription: Reviews on the plan\nmodel: cc:opus\n---\n",
    );
    let everything = |_: &Model| None;
    for billing in crate::billing::WorkerBilling::ALL {
        basis.billing = billing;
        let text = options_text(&basis, Show::All, &everything, &BTreeMap::new());
        assert!(
            text.starts_with(crate::agents::billing_words(billing)),
            "{text}"
        );
        let blocked = text.contains("Reviews carefully · can't start yet, billed per token");
        assert_eq!(
            blocked,
            billing == crate::billing::WorkerBilling::Plans,
            "{text}"
        );
        assert!(text.contains("\n- planned (user) · cc:opus · Reviews on the plan\n"));
        // A model billed per token says so, a plan's says which plan.
        assert!(
            text.contains("\n- opus · efforts low to max · API, $"),
            "{text}"
        );
        assert!(text.contains(
            "\n- cc:opus · efforts low to max · Claude plan, shared with the user's own use"
        ));
    }
    // With every worker on the session's model, there's nothing to choose.
    basis.choice = ModelChoice::Session;
    let text = options_text(&basis, Show::All, &everything, &BTreeMap::new());
    assert!(!text.contains("Prefer the models"), "{text}");
}

#[test]
fn efforts_and_sizes_read_briefly() {
    let efforts = |name: &str| efforts_range(&resolve_model(name).unwrap());
    assert_eq!(efforts("opus"), "low to max");
    assert_eq!(efforts("gemini"), "low to high");
    assert_eq!(efforts("gpt-5-pro-2025-10-06"), "high");
    assert_eq!(efforts("gpt-5.4-nano-2026-03-17"), "low to xhigh");
    assert_eq!(thousands(950_000), "950k");
}

/// The tools with real parts: a crew in a temporary session folder, keys that
/// reach OpenAI, and an asker whose answers the test gives.
struct Fixture {
    options: Arc<dyn Tool>,
    save: Arc<dyn Tool>,
    basis: Arc<Basis>,
    crew: Arc<Crew>,
    feed: remote::Feed,
    questions: Option<tokio::sync::mpsc::UnboundedReceiver<crate::inputs::Question>>,
    _temp: tempfile::TempDir,
}

fn fixture(choice: ModelChoice, can_ask: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let basis = Arc::new(basis(temp.path(), choice));
    let feed = remote::Feed::new("main");
    let (crew, _) = Crew::new(Setup {
        session_id: "example".into(),
        session_dir: temp.path().join("session"),
        output: Arc::new(Output::new(None, true)),
        max_running: 4,
        cache_1h: false,
        openai_auth: None,
        cache_lifetime: arko_ai::prompt_cache::PromptCacheLifetime::Known(Duration::from_secs(300)),
        feed: Some(feed.clone()),
        refuse: None,
        asker: None,
        root: None,
    });
    let worker = settings("luna", Effort::High);
    let config =
        crate::agents::worker_config(&crate::agents::AgentSettings::for_test(&worker, &worker))
            .unwrap();
    crew.set_new_workers(NewWorkers {
        model: WorkerSetup::of(&config),
        cache_lifetime: arko_ai::prompt_cache::PromptCacheLifetime::Known(Duration::from_secs(300)),
        basis: basis.clone(),
    });
    let client = arko_ai::LLMClient::new()
        .with_openai_client(arko_ai::providers::openai::OpenAIClient::new("test-key"));
    let keys = Keys::new(
        Arc::new(client),
        Some(providers::OpenAiAuth::ApiKey),
        None,
        None,
    );
    let (sender, questions) = tokio::sync::mpsc::unbounded_channel();
    let asker = Arc::new(Asker {
        questions: can_ask.then_some(sender),
        feed: None,
    });
    let mut tools = tools(
        crew.clone(),
        keys,
        Arc::new(Mutex::new(Stats::default())),
        asker,
    );
    let save = tools.pop().unwrap();
    let options = tools.pop().unwrap();
    Fixture {
        options,
        save,
        basis,
        crew,
        feed,
        questions: can_ask.then_some(questions),
        _temp: temp,
    }
}

/// Waits for `future`, which a regression in the approval flow can leave
/// waiting for ever (a question never asked, a call that never ends), so the
/// test fails instead of blocking the run.
async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("it came in time")
}

async fn call(tool: &Arc<dyn Tool>, pairs: &[(&str, Value)]) -> Result<String, String> {
    let directory = std::env::temp_dir();
    let context = TaskContext::new(uuid::Uuid::nil(), "main", "", &directory);
    let params = pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect();
    tool.execute(&context, params)
        .await
        .map_err(|error| match error {
            ToolError::ExecutionFailed(text) => text,
            other => other.to_string(),
        })
}

fn shown(feed: &remote::Feed) -> Vec<String> {
    feed.subscribe()
        .0
        .items
        .iter()
        .map(|item| {
            let item = serde_json::to_value(item).unwrap();
            item["text"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

#[test]
fn the_tools_are_defined_for_the_main_agent() {
    let fixture = fixture(ModelChoice::Agent, true);
    let options = fixture.options.definition();
    assert_eq!(options.name, "worker_options");
    assert_eq!(options.parameters.len(), 1);
    assert!(!options.parameters[0].required && options.parallel_safe);
    let save = fixture.save.definition();
    assert_eq!(save.name, "save_worker_profile");
    let names: Vec<(&str, bool)> = save
        .parameters
        .iter()
        .map(|parameter| (parameter.name.as_str(), parameter.required))
        .collect();
    assert_eq!(
        names,
        [
            ("name", true),
            ("description", true),
            ("model", false),
            ("effort", false),
            ("instructions", false),
            ("skills", false),
            ("mcp", false),
            ("scope", false),
        ]
    );
    // Several saves of one response run together, to ask in one card.
    assert!(save.parallel_safe);
    assert!(!fixture.save.uses_host_timeout());
    // The tools are the main agent's, among the lead's.
    for name in ["worker_options", "save_worker_profile"] {
        assert!(crate::crew::LEAD_TOOL_NAMES.contains(&name));
        assert!(!crate::crew::WORKER_TOOL_NAMES.contains(&name));
    }
}

#[tokio::test]
async fn worker_options_lists_what_is_there_and_takes_show() {
    let fixture = fixture(ModelChoice::Agent, true);
    write(
        &fixture.basis,
        Place::User,
        "judge",
        "---\ndescription: Judges\nmodel: sol\n---\n",
    );
    let all = call(&fixture.options, &[]).await.unwrap();
    assert!(all.contains("- judge (user) · sol · Judges"), "{all}");
    assert!(all.contains("Models usable now"));
    let profiles = call(&fixture.options, &[("show", "profiles".into())])
        .await
        .unwrap();
    assert!(!profiles.contains("Models usable now"));
    let models = call(&fixture.options, &[("show", "models".into())])
        .await
        .unwrap();
    assert!(!models.contains("judge"));
    assert_eq!(
        call(&fixture.options, &[("show", "everything".into())])
            .await
            .unwrap_err(),
        "show is profiles, models or all, not everything"
    );
}

#[tokio::test]
async fn a_profile_for_the_session_is_saved_without_asking() {
    let fixture = fixture(ModelChoice::Agent, true);
    let result = call(
        &fixture.save,
        &[
            ("name", "reviewer".into()),
            ("description", "Reviews a diff".into()),
            ("model", "sol".into()),
            ("effort", "high".into()),
            ("instructions", "Review it against the rules.".into()),
            ("scope", "session".into()),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        "Saved the worker profile reviewer for this session. start_agent takes it by name."
    );
    let found = profiles::read(&fixture.basis.roots, "reviewer").unwrap();
    assert_eq!(found.place, Place::Session);
    assert!(found.profile.made_by_agent());
    assert_eq!(found.profile.model.as_deref(), Some("sol"));
    assert_eq!(found.profile.effort, Some(Effort::High));
    assert_eq!(found.profile.instructions, "Review it against the rules.");
    assert!(
        shown(&fixture.feed).contains(
            &"The main agent saved the worker profile reviewer for this session.".to_string()
        ),
        "{:?}",
        shown(&fixture.feed)
    );
    // It can be saved again, being the agent's own.
    let again = call(
        &fixture.save,
        &[
            ("name", "reviewer".into()),
            ("description", "Reviews a diff again".into()),
            ("scope", "session".into()),
        ],
    )
    .await;
    assert!(again.is_ok(), "{again:?}");
}

/// With no scope a profile is kept with the project. The user isn't asked
/// while git ignores the profiles folder, and is when git tracks it.
#[tokio::test]
async fn a_profile_with_no_scope_is_kept_with_the_project_and_asks_when_git_tracks_it() {
    let mut fixture = fixture(ModelChoice::Agent, true);
    let mut questions = fixture.questions.take().unwrap();
    let folder = fixture.basis.roots.folder(Place::Project).unwrap();
    let work = folder.parent().unwrap().parent().unwrap();
    std::fs::create_dir_all(work).unwrap();
    let done = std::process::Command::new("git")
        .arg("-C")
        .arg(work)
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(done.success());

    // The folder is new, so the save makes it with a .gitignore, unasked.
    let result = call(
        &fixture.save,
        &[
            ("name", "reviewer".into()),
            ("description", "Reviews".into()),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        "Saved the worker profile reviewer for this project. start_agent takes it by name."
    );
    assert!(questions.try_recv().is_err(), "no question");
    assert!(folder.join(".gitignore").is_file());
    let found = profiles::read(&fixture.basis.roots, "reviewer").unwrap();
    assert_eq!(found.place, Place::Project);

    // Without the .gitignore git tracks the folder, so the user is asked.
    std::fs::remove_file(folder.join(".gitignore")).unwrap();
    let asking = {
        let save = fixture.save.clone();
        tokio::spawn(async move {
            call(
                &save,
                &[("name", "audit".into()), ("description", "Audits".into())],
            )
            .await
        })
    };
    let question = within(questions.recv()).await.unwrap();
    assert!(
        question
            .text()
            .contains("worker profile audit in this project's")
    );
    question.answer.unwrap().decide(ApprovalAnswer::Save);
    assert!(within(asking).await.unwrap().is_ok());
}

#[tokio::test]
async fn a_profile_for_every_session_waits_for_the_users_save() {
    let mut fixture = fixture(ModelChoice::Agent, true);
    let mut questions = fixture.questions.take().unwrap();
    let parameters = [
        ("name", "3d-judge".into()),
        (
            "description",
            "Judges 3D results against a reference image".into(),
        ),
        ("model", "sol".into()),
        ("effort", "high".into()),
        ("instructions", "You judge 3D models.".into()),
        ("scope", "user".into()),
    ];
    // Words of the user's ask for changes, and save nothing.
    let asking = {
        let save = fixture.save.clone();
        let parameters = parameters.clone();
        tokio::spawn(async move { call(&save, &parameters).await })
    };
    let question = within(questions.recv()).await.unwrap();
    assert!(
        question.text().starts_with(
            "The main agent wants to save the worker profile 3d-judge in your Arko folder, for every project.\n\nJudges 3D results against a reference image\nRuns on sol at high effort."
        ),
        "{}",
        question.text()
    );
    // Its card shows what it approves, with Save and Don't save.
    let card = question.approval.clone().unwrap();
    assert_eq!(
        card.title(),
        "Save the worker profile 3d-judge for every project?"
    );
    assert_eq!(card.runs_on(), "Runs on sol at high effort.");
    assert_eq!(
        card.description,
        "Judges 3D results against a reference image"
    );
    assert_eq!(question.asked[0].question.choices, ["Save", "Don't save"]);
    question
        .answer
        .unwrap()
        .decide(ApprovalAnswer::Edit("yes, but use medium effort".into()));
    assert_eq!(
        within(asking).await.unwrap().unwrap(),
        "The user didn't save 3d-judge for every project and wrote \"yes, but use medium effort\". If those words ask for changes to the profile, make them and call save_worker_profile again, which shows them the changed profile. If they don't (a new request, or words about something else), they are a message to you, not changes. Respond to it, call save_worker_profile again with the same profile, and end with a short reminder that it is still waiting for their decision."
    );
    assert!(profiles::read(&fixture.basis.roots, "3d-judge").is_err());
    // The user is told in the transcript that nothing was saved.
    assert_eq!(shown(&fixture.feed), ["Not saved. Arko asks again."]);

    // Save saves it, in the user's place.
    let asking = {
        let save = fixture.save.clone();
        let parameters = parameters.clone();
        tokio::spawn(async move { call(&save, &parameters).await })
    };
    within(questions.recv())
        .await
        .unwrap()
        .answer
        .unwrap()
        .decide(ApprovalAnswer::Save);
    let result = within(asking).await.unwrap().unwrap();
    assert!(
        result.starts_with("Saved the worker profile 3d-judge in the user's Arko folder, for every project. start_agent takes it by name."),
        "{result}"
    );
    let found = profiles::read(&fixture.basis.roots, "3d-judge").unwrap();
    assert_eq!(found.place, Place::User);
    assert!(found.profile.made_by_agent());
    assert!(
        shown(&fixture.feed)[1].starts_with(
            "The main agent saved the worker profile 3d-judge for every project. It is in "
        ),
        "{:?}",
        shown(&fixture.feed)
    );

    // For the project, the same.
    let asking = {
        let save = fixture.save.clone();
        tokio::spawn(async move {
            call(
                &save,
                &[
                    ("name", "audit".into()),
                    ("description", "Audits".into()),
                    ("scope", "project".into()),
                ],
            )
            .await
        })
    };
    let question = within(questions.recv()).await.unwrap();
    assert!(
        question
            .text()
            .contains("in this project's .agents/profiles folder")
    );
    question.answer.unwrap().decide(ApprovalAnswer::Save);
    assert!(
        asking
            .await
            .unwrap()
            .unwrap()
            .starts_with("Saved the worker profile audit for this project.")
    );
    assert_eq!(
        profiles::read(&fixture.basis.roots, "audit").unwrap().place,
        Place::Project
    );
}

#[tokio::test]
async fn an_empty_answer_or_no_one_to_ask_saves_nothing() {
    let mut fixture = fixture(ModelChoice::Agent, true);
    let mut questions = fixture.questions.take().unwrap();
    let asking = {
        let save = fixture.save.clone();
        tokio::spawn(async move {
            call(
                &save,
                &[
                    ("name", "audit".into()),
                    ("description", "Audits".into()),
                    ("scope", "user".into()),
                ],
            )
            .await
        })
    };
    within(questions.recv())
        .await
        .unwrap()
        .answer
        .unwrap()
        .decide(ApprovalAnswer::DontSave);
    assert_eq!(
        within(asking).await.unwrap().unwrap(),
        "The user chose not to save audit for every project, so don't ask again unless they want it. You may save it for this session with scope session."
    );
    // A session no one can answer in (-p, a script) can't approve.
    let alone = fixture_alone().await;
    assert_eq!(
        alone,
        "No one can approve it in this session. Save it for the session, or tell the user the setup you want."
    );
}

async fn fixture_alone() -> String {
    let fixture = fixture(ModelChoice::Agent, false);
    let refused = within(call(
        &fixture.save,
        &[
            ("name", "audit".into()),
            ("description", "Audits".into()),
            ("scope", "project".into()),
        ],
    ))
    .await
    .unwrap_err();
    assert!(profiles::read(&fixture.basis.roots, "audit").is_err());
    // For the session it needs no one.
    assert!(
        call(
            &fixture.save,
            &[
                ("name", "audit".into()),
                ("description", "Audits".into()),
                ("scope", "session".into()),
            ],
        )
        .await
        .is_ok()
    );
    refused
}

/// The refusal of a save with a description and `pairs`.
async fn refusal(tool: &Arc<dyn Tool>, pairs: Vec<(&str, Value)>) -> String {
    let mut all = vec![("description", Value::from("Does it"))];
    all.extend(pairs);
    call(tool, &all).await.unwrap_err()
}

#[tokio::test]
async fn a_profile_that_cant_work_is_refused_before_the_user_is_asked() {
    let mut fixture = fixture(ModelChoice::Agent, true);
    let mut questions = fixture.questions.take().unwrap();
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "Bad Name".into()), ("scope", "user".into())]
        )
        .await,
        "`Bad Name` isn't a profile name. A name is lowercase letters, digits and hyphens, up to 32 characters, starting with a letter or a digit."
    );
    assert_eq!(
        refusal(&fixture.save, vec![("name", "default".into())]).await,
        "`default` is reserved. Choose another name for the profile."
    );
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "x".into()), ("model", "gpt9".into())]
        )
        .await,
        "`gpt9` isn't a model Arko knows. worker_options lists the models usable now."
    );
    assert_eq!(
        refusal(
            &fixture.save,
            vec![
                ("name", "x".into()),
                ("model", "gpt-5.4-nano-2026-03-17".into()),
                ("effort", "max".into()),
            ]
        )
        .await,
        "gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max."
    );
    assert!(
        refusal(
            &fixture.save,
            vec![("name", "x".into()), ("effort", "extreme".into())]
        )
        .await
        .contains("`extreme` isn't an effort")
    );
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "x".into()), ("scope", "everywhere".into())]
        )
        .await,
        "scope is session, project or user, not everywhere"
    );
    assert_eq!(
        refusal(
            &fixture.save,
            vec![
                ("name", "x".into()),
                ("instructions", "i".repeat(4001).into())
            ]
        )
        .await,
        "The instructions are 4,001 characters, and the limit is 4,000."
    );
    // A name the user's profile has, in any place.
    write(
        &fixture.basis,
        Place::User,
        "mine",
        "---\ndescription: The user's\n---\n",
    );
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "mine".into()), ("scope", "project".into())]
        )
        .await,
        "There is already a profile `mine` made by the user. Choose another name."
    );
    let missing_name = {
        let save = fixture.save.clone();
        call(&save, &[("description", "x".into())])
            .await
            .unwrap_err()
    };
    assert_eq!(missing_name, "Give the profile a name.");
    assert!(questions.try_recv().is_err(), "the user was asked nothing");
}

#[tokio::test]
async fn the_settings_limit_who_saves_profiles() {
    for (choice, said) in [
        (
            ModelChoice::Profiles,
            "The user keeps the worker profiles themselves (/worker-models profiles). Tell them the setup you want, and they can save it with /profiles add.",
        ),
        (
            ModelChoice::Session,
            "The user has every worker run the session's worker model (/worker-models session), so a profile has no use here. Don't save one.",
        ),
    ] {
        let fixture = fixture(choice, true);
        let why = call(
            &fixture.save,
            &[("name", "x".into()), ("description", "Does it".into())],
        )
        .await
        .unwrap_err();
        assert_eq!(why, said);
        assert!(profiles::read(&fixture.basis.roots, "x").is_err());
    }
}

/// Under Plans only, a profile on a model billed per token isn't saved,
/// since no worker could start with it, and one on a plan's model is.
#[tokio::test]
async fn under_plans_only_a_profile_billed_per_token_is_refused() {
    let fixture = fixture(ModelChoice::Agent, true);
    let mut new = fixture.crew.new_workers().unwrap();
    new.basis = Arc::new(Basis {
        billing: WorkerBilling::Plans,
        ..(*new.basis).clone()
    });
    fixture.crew.set_new_workers(new);
    let save = |name: &str, model: &str| {
        let pairs = vec![
            ("name", Value::from(name)),
            ("description", Value::from("Thinks hard")),
            ("model", Value::from(model)),
            ("scope", Value::from("session")),
        ];
        let save = fixture.save.clone();
        async move { call(&save, &pairs).await }
    };
    let why = save("strong", "opus").await.unwrap_err();
    assert_eq!(
        why,
        "The user lets workers run only on their plans (/worker-billing plans), and claude-opus-5-5 is billed per token, so a worker couldn't start with this profile. Give it a model on the user's plans, which worker_options lists."
    );
    assert!(profiles::read(&fixture.basis.roots, "strong").is_err());
    // sol is on the ChatGPT plan here.
    assert!(save("planned", "sol").await.is_ok());
}

#[tokio::test]
async fn a_profile_on_a_model_the_session_cant_reach_is_saved_with_a_warning() {
    let fixture = fixture(ModelChoice::Agent, true);
    let result = call(
        &fixture.save,
        &[
            ("name", "strong".into()),
            ("description", "Thinks hard".into()),
            ("model", "opus".into()),
            ("scope", "session".into()),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        "Saved the worker profile strong for this session. start_agent takes it by name. Workers on it can't start until the user adds ANTHROPIC_API_KEY with /keys."
    );
    assert!(profiles::read(&fixture.basis.roots, "strong").is_ok());
    // Claude Code's login is told as it is gotten.
    assert!(needs_warning("Claude Code signed in").contains("`claude auth login`"));
    assert!(needs_warning("Claude Code installed").contains("installs Claude Code"));
    // A profile that a nearer place hides says so.
    write(
        &fixture.basis,
        Place::Session,
        "strong",
        "---\ndescription: Made by the agent\nmade_by: agent\n---\n",
    );
    drop(fixture.crew);
}

#[tokio::test]
async fn a_saved_profile_hidden_by_a_nearer_one_says_so() {
    let mut fixture = fixture(ModelChoice::Agent, true);
    let mut questions = fixture.questions.take().unwrap();
    write(
        &fixture.basis,
        Place::Session,
        "judge",
        "---\ndescription: Made by the agent\nmade_by: agent\n---\n",
    );
    let asking = {
        let save = fixture.save.clone();
        tokio::spawn(async move {
            call(
                &save,
                &[
                    ("name", "judge".into()),
                    ("description", "For good".into()),
                    ("scope", "user".into()),
                ],
            )
            .await
        })
    };
    within(questions.recv())
        .await
        .unwrap()
        .answer
        .unwrap()
        .decide(ApprovalAnswer::Save);
    let result = within(asking).await.unwrap().unwrap();
    assert!(
        result.contains(
            "A profile of the same name for this session comes first, so start_agent takes that one instead."
        ),
        "{result}"
    );
}

#[test]
fn worker_options_lists_the_first_profiles_and_counts_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), ModelChoice::Agent);
    for number in 0..55 {
        write(
            &basis,
            Place::Project,
            &format!("p{number:02}"),
            "---\ndescription: One of many\n---\n",
        );
    }
    for number in 0..14 {
        write(
            &basis,
            Place::Project,
            &format!("bad{number:02}"),
            "no front matter",
        );
    }
    let text = options_text(&basis, Show::Profiles, &needs, &plans());
    assert!(text.contains("\n- p39 (project)"), "{text}");
    assert!(!text.contains("p40"), "{text}");
    assert!(
        text.contains("\n- and 15 more, which start_agent takes by name"),
        "{text}"
    );
    assert!(text.contains("\n- bad09 (project)"), "{text}");
    assert!(!text.contains("bad10"), "{text}");
    assert!(text.ends_with("\n- and 4 more"), "{text}");
    // Some profiles and no more than the cap stay as they were.
    assert!(text.lines().count() < 70, "{}", text.lines().count());
}

#[tokio::test]
async fn a_value_of_the_wrong_type_is_refused_and_not_read_as_the_default() {
    let fixture = fixture(ModelChoice::Agent, true);
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "x".into()), ("scope", serde_json::json!(["user"]))]
        )
        .await,
        "scope must be text"
    );
    assert_eq!(
        call(&fixture.options, &[("show", 5.into())])
            .await
            .unwrap_err(),
        "show must be text"
    );
    assert_eq!(
        refusal(&fixture.save, vec![("name", 3.into())]).await,
        "name must be text"
    );
}

#[tokio::test]
async fn an_effort_the_session_s_model_does_not_take_is_refused_for_a_profile_with_no_model() {
    let fixture = fixture(ModelChoice::Agent, true);
    let mut nano = (*fixture.basis).clone();
    nano.worker = settings("gpt-5.4-nano-2026-03-17", Effort::High);
    let worker = settings("luna", Effort::High);
    let config =
        crate::agents::worker_config(&crate::agents::AgentSettings::for_test(&worker, &worker))
            .unwrap();
    fixture.crew.set_new_workers(NewWorkers {
        model: WorkerSetup::of(&config),
        cache_lifetime: arko_ai::prompt_cache::PromptCacheLifetime::Known(Duration::from_secs(300)),
        basis: Arc::new(nano),
    });
    assert_eq!(
        refusal(
            &fixture.save,
            vec![("name", "hot".into()), ("effort", "max".into())]
        )
        .await,
        "gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max, and a profile with no model runs on it. Give the profile a model, or an effort that gpt-5.4-nano-2026-03-17 takes."
    );
    // With a model of its own, the profile is fine.
    let saved = call(
        &fixture.save,
        &[
            ("name", "hot".into()),
            ("description", "Thinks hard".into()),
            ("model", "sol".into()),
            ("effort", "max".into()),
            ("scope", "session".into()),
        ],
    )
    .await;
    assert!(saved.is_ok(), "{saved:?}");
}

/// `save_worker_profile` takes `skills` and `mcp`, held to what the main
/// agent can give a worker, and the profile keeps them.
#[tokio::test]
async fn a_saved_profile_keeps_the_skills_and_servers_it_names() {
    let fixture = fixture(ModelChoice::Agent, true);
    let skills = fixture._temp.path().join("skills");
    for name in ["ponytail", "release-notes"] {
        let dir = skills.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\nDo {name}.\n"),
        )
        .unwrap();
    }
    let mut new = fixture.crew.new_workers().unwrap();
    new.basis = Arc::new(Basis {
        available: crate::chooser::Available {
            skills: Some(arko_agents::skill::SkillsConfig {
                global_sources: vec![arko_agents::skill::SkillSource {
                    path: skills.display().to_string(),
                    include: None,
                    exclude: None,
                    optional: false,
                }],
                ..Default::default()
            }),
            workspace: fixture._temp.path().into(),
        },
        ..(*new.basis).clone()
    });
    fixture.crew.set_new_workers(new);
    fixture.crew.set_mcp_hub(arko_agents::McpHub::new(vec![
        arko_agents::McpServerConfig::new("github", "true"),
    ]));
    let save = |name: &str, skills: &str, mcp: &str| {
        let pairs = vec![
            ("name", Value::from(name)),
            ("description", Value::from("Reads")),
            ("scope", Value::from("session")),
            ("skills", Value::from(skills)),
            ("mcp", Value::from(mcp)),
        ];
        let save = fixture.save.clone();
        async move { call(&save, &pairs).await }
    };
    let saved = save("narrow", "ponytail, release-notes", "none").await;
    assert!(saved.is_ok(), "{saved:?}");
    let saved = profiles::read(&fixture.basis.roots, "narrow")
        .unwrap()
        .profile;
    assert_eq!(
        saved.skills,
        Some(vec!["ponytail".to_string(), "release-notes".to_string()])
    );
    assert_eq!(saved.mcp, Some(Vec::new()));
    // Names the main agent can't give, and text that isn't a list, are
    // refused before anyone is asked.
    assert_eq!(
        save("typo", "ponytale", "github").await.unwrap_err(),
        "A worker can't get the skill `ponytale`. You can give ponytail and release-notes."
    );
    assert_eq!(
        save("typo", "ponytail", "figma").await.unwrap_err(),
        "A worker can't get the MCP server `figma`. You can give github."
    );
    assert_eq!(
        save("typo", ",", "none").await.unwrap_err(),
        "skills isn't names separated by commas, a JSON array or none"
    );
    assert!(profiles::read(&fixture.basis.roots, "typo").is_err());
    // The list of options says what the profile narrows.
    let basis = fixture.crew.new_workers().unwrap().basis;
    let text = options_text(&basis, Show::Profiles, &needs, &plans());
    assert!(
        text.contains("\n- narrow (session, made by you) · the worker model, only the skills ponytail and release-notes, no MCP servers · Reads"),
        "{text}"
    );
}
