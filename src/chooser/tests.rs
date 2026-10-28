use std::sync::Arc;

use super::*;
use crate::agents::{AgentSettings, Effort, ModelSettings, resolve_model};
use crate::profiles::{Author, Place, Profile};

fn settings(name: &str, effort: Effort) -> ModelSettings {
    let model = resolve_model(name).unwrap();
    ModelSettings {
        model: providers::model_name(&model),
        effort,
        fast: false,
        cache_1h: false,
        handover_at: Some(agents::default_handover(&model, None)),
    }
}

/// The basis of a session whose workers run `worker`, with profiles in a
/// temporary folder.
fn basis(temp: &std::path::Path, worker: ModelSettings) -> Basis {
    Basis {
        roots: profiles::Roots::new(temp.join("arko"))
            .workspace(temp.join("work"))
            .session(temp.join("session")),
        worker,
        providers: providers::Setup::new(None),
        thinking_text: true,
        cc_compact: true,
        handover_at: 200_000,
        choice: ModelChoice::Agent,
        worker_depth: 2,
        extra_rules: ExtraRules::Small,
        rules_text: Some("# Extra rules\n- Search with rg.\n".to_string()),
        billing: WorkerBilling::Api,
        api_budget: None,
    }
}

fn request(parameters: &[(&str, &str)]) -> StartRequest {
    StartRequest {
        workflow_run_id: uuid::Uuid::nil(),
        run_id: uuid::Uuid::new_v4(),
        handle: "r1".into(),
        agent: agents::WORKER.into(),
        caller_run_id: uuid::Uuid::new_v4(),
        caller_agent: agents::MAIN.into(),
        task: "Do it".into(),
        parameters: parameters
            .iter()
            .map(|(name, value)| (name.to_string(), Value::String(value.to_string())))
            .collect(),
    }
}

fn pick(basis: &Basis, parameters: &[(&str, &str)]) -> Result<WorkerSetup, String> {
    choose(basis, &request(parameters), None)
}

fn save(basis: &Basis, place: Place, profile: Profile) {
    profiles::save(&basis.roots, place, &profile, Author::User).unwrap();
}

/// A profile file written as a user would, which `profiles::save` would
/// refuse (a model or an effort that doesn't fit).
fn write_file(basis: &Basis, place: Place, name: &str, text: &str) {
    let folder = basis.roots.folder(place).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(format!("{name}.md")), text).unwrap();
}

fn profile(name: &str, model: Option<&str>, effort: Option<Effort>) -> Profile {
    let mut profile = Profile::new(name, format!("The {name} profile"));
    profile.model = model.map(str::to_string);
    profile.effort = effort;
    profile
}

fn model_of(setup: &WorkerSetup) -> String {
    providers::model_name(&setup.llm_config.model)
}

fn effort_of(setup: &WorkerSetup) -> String {
    serde_json::to_value(&setup.llm_config.generation_config).unwrap()["reasoning"].to_string()
}

/// The same setup, as JSON.
fn json(setup: &WorkerSetup) -> serde_json::Value {
    serde_json::to_value(setup).unwrap()
}

/// A start that chooses nothing gives the setup the registered worker has,
/// field by field, whatever the session's worker model is.
#[test]
fn a_start_with_no_choice_runs_the_session_s_worker_model_as_registered() {
    let temp = tempfile::tempdir().unwrap();
    let mut workers = vec![
        settings("luna", Effort::High),
        settings("opus", Effort::Xhigh),
        settings("cc:fable", Effort::High),
        settings("gemini", Effort::Low),
        settings("openrouter:anthropic/claude-opus-5.5", Effort::Medium),
        settings("sol", Effort::Max),
        // (Sessions keep the effort asked for, also one the model doesn't
        // take.)
        settings("cc:haiku", Effort::Xhigh),
    ];
    let mut one_hour = settings("sonnet", Effort::High);
    one_hour.cache_1h = true;
    workers.push(one_hour);
    let mut fast = settings("opus", Effort::High);
    fast.fast = true;
    workers.push(fast);
    for worker in workers {
        let basis = basis(temp.path(), worker.clone());
        let agent_settings = AgentSettings::for_test(&worker, &worker);
        let registered = agents::worker_config(&agent_settings).unwrap();
        let mut chosen = pick(&basis, &[]).unwrap();
        // (It also keeps the effort, for the lists.)
        assert_eq!(chosen.effort, Some(worker.effort), "{}", worker.model);
        chosen.effort = None;
        assert_eq!(
            json(&chosen),
            json(&WorkerSetup::of(&registered)),
            "{}",
            worker.model
        );
        // Naming `default`, or the session's own model, is the same.
        let mut named = pick(&basis, &[("profile", "default")]).unwrap();
        named.effort = None;
        assert_eq!(json(&named), json(&chosen));
        assert_eq!(chosen.context_added, None);
        assert_eq!(chosen.profile, None);
    }
}

/// The call's model and effort beat the profile's, which beat the session's.
#[test]
fn the_call_beats_the_profile_and_the_profile_beats_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    save(
        &basis,
        Place::User,
        profile("judge", Some("sol"), Some(Effort::Max)),
    );
    save(&basis, Place::User, profile("plain", None, None));

    let by_profile = pick(&basis, &[("profile", "judge")]).unwrap();
    assert_eq!(model_of(&by_profile), "gpt-6.1-sol");
    assert!(
        effort_of(&by_profile).contains("max"),
        "{}",
        effort_of(&by_profile)
    );
    assert_eq!(by_profile.profile.as_deref(), Some("judge"));

    // The call's model beats the profile's, and the profile's effort goes
    // with it as the nearest the model takes.
    let by_call = pick(&basis, &[("profile", "judge"), ("model", "astra")]).unwrap();
    assert_eq!(model_of(&by_call), "gpt-6-astra");
    assert!(effort_of(&by_call).contains("max"));
    // And the call's effort beats both.
    let both = pick(
        &basis,
        &[("profile", "judge"), ("model", "luna"), ("effort", "low")],
    )
    .unwrap();
    assert_eq!(model_of(&both), "gpt-6-luna");
    assert!(effort_of(&both).contains("low"));

    // A profile with nothing to say about the model leaves it to the session.
    let plain = pick(&basis, &[("profile", "plain")]).unwrap();
    assert_eq!(model_of(&plain), "gpt-6-luna");
    assert!(effort_of(&plain).contains("high"));
    assert_eq!(plain.profile.as_deref(), Some("plain"));

    // A model with no effort given keeps the nearest one the model takes to
    // the session's.
    let session = basis_with_worker(&basis, settings("sol", Effort::Max));
    let nano = pick(&session, &[("model", "gpt-5.4-nano-2026-03-17")]).unwrap();
    assert!(effort_of(&nano).contains("xhigh"), "{}", effort_of(&nano));
}

fn basis_with_worker(basis: &Basis, worker: ModelSettings) -> Basis {
    Basis {
        worker,
        ..basis.clone()
    }
}

/// An effort the model doesn't take is refused and not changed, when the
/// call or the profile's own model gave it. One from further away is fitted.
#[test]
fn an_effort_the_model_doesnt_take_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    write_file(
        &basis,
        Place::User,
        "nano",
        "---\ndescription: Nano\nmodel: gpt-5.4-nano-2026-03-17\neffort: max\n---\n",
    );
    save(&basis, Place::User, profile("hot", None, Some(Effort::Max)));
    let nano = "gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max.";
    assert_eq!(
        pick(
            &basis,
            &[("model", "gpt-5.4-nano-2026-03-17"), ("effort", "max")]
        )
        .unwrap_err(),
        nano
    );
    // A profile's own effort that the model doesn't take names the profile
    // and how to go on.
    assert_eq!(
        pick(&basis, &[("profile", "nano")]).unwrap_err(),
        format!(
            "The profile `nano` asks for max effort. {nano} Pass an effort of your own to start it anyway."
        )
    );
    // The call chose another model: the profile's effort is a hint.
    let fitted = pick(&basis, &[("profile", "nano"), ("model", "sol")]).unwrap();
    assert!(effort_of(&fitted).contains("max"));
    let fitted = pick(
        &basis,
        &[("profile", "hot"), ("model", "gpt-5.4-nano-2026-03-17")],
    )
    .unwrap();
    assert!(effort_of(&fitted).contains("xhigh"));
    // A profile that only sets an effort answers to the session's model.
    let on_luna = pick(&basis, &[("profile", "hot")]).unwrap();
    assert!(effort_of(&on_luna).contains("max"));
    let on_nano = basis_with_worker(&basis, settings("gpt-5.4-nano-2026-03-17", Effort::High));
    assert_eq!(
        pick(&on_nano, &[("profile", "hot")]).unwrap_err(),
        format!(
            "The profile `hot` asks for max effort. {nano} Pass an effort of your own to start it anyway."
        )
    );
    // A word that isn't an effort says what the efforts are.
    assert_eq!(
        pick(&basis, &[("effort", "extreme")]).unwrap_err(),
        "`extreme` isn't an effort. The efforts are low, medium, high, xhigh and max."
    );
}

/// A model that doesn't reason takes any effort and ignores it.
#[test]
fn a_model_that_does_not_reason_ignores_the_effort() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    let chat = arko_ai::Model::all()
        .into_iter()
        .find(|model| {
            model.provider == arko_ai::Provider::OpenAI && providers::efforts(model).is_empty()
        })
        .expect("a model that doesn't reason");
    let name = providers::model_name(&chat);
    let setup = pick(&basis, &[("model", name.as_str()), ("effort", "max")]).unwrap();
    assert_eq!(model_of(&setup), name);
}

/// Fast mode is on for the session's own worker model when the session has
/// it on, with a profile or without one, and for the profile that says so.
/// It is off for a worker on another model otherwise.
#[test]
fn fast_mode_is_off_unless_the_profile_asks_or_the_session_s_model_has_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut fast = settings("opus", Effort::High);
    fast.fast = true;
    let basis = basis(temp.path(), fast);
    let is_fast = |setup: &WorkerSetup| {
        serde_json::to_string(&setup.llm_config.generation_config)
            .unwrap()
            .contains("\"anthropic_fast_mode\":true")
    };
    assert!(is_fast(&pick(&basis, &[]).unwrap()));
    assert!(is_fast(&pick(&basis, &[("effort", "low")]).unwrap()));
    // Another model is a deliberate setup, and not fast.
    assert!(!is_fast(&pick(&basis, &[("model", "fable")]).unwrap()));
    // A profile that keeps the session's worker model, by not naming one or
    // by naming it, keeps its fast mode.
    save(&basis, Place::User, profile("notes", None, None));
    assert!(is_fast(&pick(&basis, &[("profile", "notes")]).unwrap()));
    save(&basis, Place::User, profile("same", Some("opus"), None));
    assert!(is_fast(&pick(&basis, &[("profile", "same")]).unwrap()));
    // A profile for another model is fast only when it says so.
    let mut quick = profile("quick", Some("claude-opus-4-8"), None);
    save(&basis, Place::User, quick.clone());
    assert!(!is_fast(&pick(&basis, &[("profile", "quick")]).unwrap()));
    quick.fast = true;
    save(&basis, Place::User, quick);
    assert!(is_fast(&pick(&basis, &[("profile", "quick")]).unwrap()));
    // A model without fast mode says so.
    let mut no_fast = profile("no-fast", Some("gemini"), None);
    no_fast.fast = true;
    save(&basis, Place::User, no_fast);
    assert_eq!(
        pick(&basis, &[("profile", "no-fast")]).unwrap_err(),
        "The profile `no-fast` asks for fast mode. gemini-3.1-pro-preview has no fast mode (Gemini models have none)"
    );
}

/// The cache: the call, else the profile, else the session, and only for the
/// models that have a long one.
#[test]
fn the_cache_comes_from_the_call_then_the_profile_then_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("opus", Effort::High));
    let ttl = |setup: &WorkerSetup| setup.llm_config.generation_config.claude_cache_ttl();
    assert_eq!(ttl(&pick(&basis, &[]).unwrap()), None);
    assert_eq!(
        ttl(&pick(&basis, &[("cache", "1h")]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::OneHour)
    );
    let mut cached = profile("cached", None, None);
    cached.cache = Some(profiles::Cache::OneHour);
    save(&basis, Place::User, cached);
    assert_eq!(
        ttl(&pick(&basis, &[("profile", "cached")]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::OneHour)
    );
    assert_eq!(
        ttl(&pick(&basis, &[("profile", "cached"), ("cache", "5m")]).unwrap()),
        None
    );
    // The session's choice is the default for the models that take one.
    let mut hour = settings("opus", Effort::High);
    hour.cache_1h = true;
    let hour = basis_with_worker(&basis, hour);
    assert_eq!(
        ttl(&pick(&hour, &[]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::OneHour)
    );
    // A model with no cache lifetime to choose ignores it.
    assert_eq!(
        ttl(&pick(&hour, &[("model", "luna"), ("cache", "1h")]).unwrap()),
        None
    );
    // Claude Code takes one too, and chooses its own (an hour on a Claude
    // plan) unless one is asked for, 5 minutes included.
    assert_eq!(ttl(&pick(&basis, &[("model", "cc:opus")]).unwrap()), None);
    assert_eq!(
        ttl(&pick(&basis, &[("model", "cc:opus"), ("cache", "1h")]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::OneHour)
    );
    assert_eq!(
        ttl(&pick(&hour, &[("model", "cc:opus"), ("cache", "5m")]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::FiveMinutes)
    );
    assert_eq!(
        ttl(&pick(&basis, &[("model", "opus"), ("cache", "5m")]).unwrap()),
        None
    );
    assert_eq!(
        pick(&basis, &[("cache", "2h")]).unwrap_err(),
        "cache is long or short, not 2h"
    );
    assert_eq!(
        ttl(&pick(&basis, &[("cache", "long")]).unwrap()),
        Some(arko_ai::AnthropicCacheTtl::OneHour)
    );
    // OpenAI's long cache is the 12-hour one, on an API key only.
    let long = |basis: &Basis, parameters: &[(&str, &str)]| {
        pick(basis, parameters)
            .unwrap()
            .llm_config
            .generation_config
            .long_prompt_cache
    };
    assert_eq!(long(&hour, &[("model", "luna")]), None);
    let mut on_key = basis_with_worker(&basis, settings("luna", Effort::High));
    on_key.providers = providers::Setup::new(Some(providers::OpenAiAuth::ApiKey));
    assert_eq!(long(&on_key, &[("cache", "long")]), Some(true));
    assert_eq!(long(&on_key, &[("cache", "short")]), None);
    assert_eq!(
        long(
            &on_key,
            &[("model", "gpt-5.5-2026-04-23"), ("cache", "long")]
        ),
        None
    );
}

/// Each model hands over at its own size, not the session's worker model's.
#[test]
fn a_worker_hands_over_at_the_size_of_its_own_model() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    let size = |setup: WorkerSetup| setup.handover.unwrap().at_input_tokens.unwrap();
    let of = |model: &str| size(pick(&basis, &[("model", model)]).unwrap());
    assert_eq!(of("opus"), 950_000);
    assert_eq!(of("cc:opus"), 950_000);
    // (An OpenRouter model whose catalog Arko hasn't read has no known limit.)
    assert_eq!(
        of("openrouter:anthropic/claude-opus-5.5"),
        providers::UNKNOWN_LIMIT_HANDOVER
    );
    assert_eq!(of("gemini"), 200_000);
    assert!(of("sol") <= 200_000);
    // The session's own model goes by the size its settings give.
    let mut set = settings("luna", Effort::High);
    set.handover_at = Some(123_000);
    let own = basis_with_worker(&basis, set);
    assert_eq!(size(pick(&own, &[]).unwrap()), 123_000);
    assert_eq!(size(pick(&own, &[("model", "opus")]).unwrap()), 950_000);
}

/// A claude model in Claude Code compacts at the handover size instead of
/// handing over, unless the session says it hands over.
#[test]
fn a_worker_in_claude_code_compacts_as_the_session_says() {
    let temp = tempfile::tempdir().unwrap();
    let mut basis = basis(temp.path(), settings("luna", Effort::High));
    let cc = pick(&basis, &[("model", "cc:opus")]).unwrap();
    assert!(cc.compacts());
    assert!(cc.claude_code.is_some());
    let api = pick(&basis, &[("model", "opus")]).unwrap();
    assert!(!api.compacts());
    assert!(api.claude_code.is_none());
    basis.cc_compact = false;
    assert!(!pick(&basis, &[("model", "cc:opus")]).unwrap().compacts());
}

/// The profile's instructions go into the run's first message, and the
/// extra rules for a small model when the session's worker didn't get them.
#[test]
fn the_profile_and_the_extra_rules_go_into_the_first_message() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("sol", Effort::High));
    let mut reviewer = profile("reviewer", Some("sol"), None);
    reviewer.instructions = "Review the diff against the rules.".into();
    save(&basis, Place::Project, reviewer);
    let reviewed = pick(&basis, &[("profile", "reviewer")]).unwrap();
    assert_eq!(
        reviewed.context_added.as_deref(),
        Some("# Your profile (reviewer)\nReview the diff against the rules.")
    );

    // A small model gets the rules the session's worker, on sol, didn't.
    let small = pick(&basis, &[("model", "haiku")]);
    // (Haiku is Claude: the credential isn't checked here.)
    let small = small.unwrap();
    assert_eq!(
        small.context_added.as_deref(),
        Some("# Extra rules\n- Search with rg.")
    );
    let mut haiku_profile = profile("quick", Some("haiku"), None);
    haiku_profile.instructions = "Answer briefly.".into();
    save(&basis, Place::User, haiku_profile);
    assert_eq!(
        pick(&basis, &[("profile", "quick")])
            .unwrap()
            .context_added
            .as_deref(),
        Some("# Your profile (quick)\nAnswer briefly.\n\n# Extra rules\n- Search with rg.")
    );
    // Not twice: the session's worker is small already, and has them.
    let small_session = basis_with_worker(&basis, settings("haiku", Effort::High));
    assert_eq!(
        pick(&small_session, &[("model", "luna")])
            .unwrap()
            .context_added,
        None
    );
    // None for a model that isn't small, or where the setting is off.
    let mut off = basis.clone();
    off.extra_rules = ExtraRules::Off;
    assert_eq!(
        pick(&off, &[("model", "haiku")]).unwrap().context_added,
        None
    );
    assert_eq!(
        pick(&basis, &[("model", "opus")]).unwrap().context_added,
        None
    );
    let mut all = basis.clone();
    all.extra_rules = ExtraRules::All;
    assert_eq!(
        pick(&all, &[("model", "opus")]).unwrap().context_added,
        None
    );
}

/// What `worker_models` lets the main agent choose.
#[test]
fn the_setting_says_what_the_main_agent_may_choose() {
    let temp = tempfile::tempdir().unwrap();
    let mut basis = basis(temp.path(), settings("luna", Effort::High));
    save(&basis, Place::User, profile("judge", Some("sol"), None));
    basis.choice = ModelChoice::Profiles;
    assert!(pick(&basis, &[("profile", "judge")]).is_ok());
    assert!(pick(&basis, &[]).is_ok());
    assert!(pick(&basis, &[("cache", "1h")]).is_ok());
    for refused in [
        &[("model", "sol")][..],
        &[("effort", "low")],
        &[("profile", "judge"), ("model", "sol")],
    ] {
        let why = pick(&basis, refused).unwrap_err();
        assert!(
            why.starts_with(
                "The user lets you start workers with a profile only (/worker-models profiles)"
            ),
            "{why}"
        );
    }
    basis.choice = ModelChoice::Session;
    assert!(pick(&basis, &[]).is_ok());
    assert!(pick(&basis, &[("profile", "default")]).is_ok());
    assert!(pick(&basis, &[("cache", "1h")]).is_ok());
    for refused in [
        &[("profile", "judge")][..],
        &[("model", "sol")],
        &[("effort", "low")],
    ] {
        let why = pick(&basis, refused).unwrap_err();
        assert!(why.contains("(/worker-models session)"), "{why}");
    }
}

/// A profile that isn't there, or can't be read, says what the main agent
/// can use instead.
#[test]
fn a_profile_that_is_missing_or_broken_says_what_to_use() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    save(&basis, Place::User, profile("judge", Some("sol"), None));
    save(&basis, Place::Session, profile("quick", Some("luna"), None));
    assert_eq!(
        pick(&basis, &[("profile", "reviewr")]).unwrap_err(),
        "There is no worker profile `reviewr`. The profiles are default, judge, and quick. worker_options lists them."
    );
    assert!(
        pick(&basis, &[("profile", "../x")])
            .unwrap_err()
            .contains("isn't a profile name")
    );
    let folder = basis.roots.folder(Place::Project).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("notes.md"), "---\nmodel sol\n---\n").unwrap();
    let why = pick(&basis, &[("profile", "notes")]).unwrap_err();
    assert!(
        why.starts_with("The worker profile `notes` can't be used (")
            && why.ends_with("notes.md, line 2 isn't `key: value`). Ask the user to fix the file, or use a profile worker_options lists."),
        "{why}"
    );
    // A profile whose model Arko doesn't know says which profile it is.
    write_file(
        &basis,
        Place::User,
        "future",
        "---\ndescription: Future\nmodel: gpt9\n---\n",
    );
    // (Without the aliases, which worker_options has.)
    assert_eq!(
        pick(&basis, &[("profile", "future")]).unwrap_err(),
        "The profile `future` can't start a worker. `gpt9` isn't a model Arko knows. worker_options lists the models usable now."
    );
    // The same name in the call points to the models.
    let why = pick(&basis, &[("model", "gpt9")]).unwrap_err();
    assert_eq!(
        why,
        "`gpt9` isn't a model Arko knows. worker_options lists the models usable now."
    );
    // A parameter of the wrong type is told so.
    let mut bad = request(&[]);
    bad.parameters.insert("model".into(), Value::from(5));
    assert!(
        choose(&basis, &bad, None)
            .unwrap_err()
            .contains("model must be text")
    );
}

/// The credentials are checked on the model the worker ends up on, with
/// the text the check gives.
#[test]
fn a_model_the_session_cant_reach_is_refused_with_what_it_needs() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    let refuse: Refuse = Arc::new(|model: &arko_ai::Model| {
        (model.provider == arko_ai::Provider::Anthropic)
            .then(|| format!("No key for {}.", model.id))
    });
    let start = |parameters: &[(&str, &str)]| choose(&basis, &request(parameters), Some(&refuse));
    assert!(start(&[("model", "sol")]).is_ok());
    assert!(start(&[]).is_ok());
    assert_eq!(
        start(&[("model", "opus")]).unwrap_err(),
        "No key for claude-opus-5-5."
    );
    // Through a profile too, and before the effort is looked at.
    save(
        &basis,
        Place::User,
        profile("strong", Some("opus"), Some(Effort::High)),
    );
    assert_eq!(
        start(&[("profile", "strong")]).unwrap_err(),
        "No key for claude-opus-5-5."
    );
    // The session's own worker model is checked as well.
    let on_opus = basis_with_worker(&basis, settings("opus", Effort::High));
    assert_eq!(
        choose(&on_opus, &request(&[]), Some(&refuse)).unwrap_err(),
        "No key for claude-opus-5-5."
    );
}

/// A start at `depth` by a worker with `live` workers of its own.
fn at(depth: u32, live: usize) -> Family {
    Family { depth, live }
}

/// The call with `nested` set to `nested`.
fn nesting(basis: &Basis, family: Family, nested: bool) -> Result<WorkerSetup, String> {
    let mut call = request(&[]);
    call.parameters.insert("nested".into(), Value::Bool(nested));
    choose_in(basis, &call, None, family)
}

/// The start tools a setup gives a worker, and no others.
fn start_tools(setup: &WorkerSetup) -> Vec<String> {
    let registered = agents::worker_config(&AgentSettings::for_test(&luna(), &luna())).unwrap();
    let mut config = registered.clone();
    setup.apply(&mut config);
    NESTED_TOOL_NAMES
        .iter()
        .filter(|name| {
            config
                .tool_config
                .available_tool_names
                .iter()
                .any(|tool| tool == *name)
        })
        .map(|name| name.to_string())
        .collect()
}

fn luna() -> ModelSettings {
    settings("luna", Effort::High)
}

/// A worker is given the start tools and the paragraph about its own
/// workers when the call or its profile says `nested`, the call winning,
/// and not otherwise.
#[test]
fn a_worker_that_nests_gets_the_start_tools_and_the_paragraph() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), luna());
    let mut nests = profile("nests", None, None);
    nests.nested = true;
    nests.instructions = "Split the review by file.".into();
    save(&basis, Place::User, nests);
    save(&basis, Place::User, profile("plain", None, None));

    // Nothing asked for, nothing given.
    let plain = pick(&basis, &[]).unwrap();
    assert!(plain.tools_added.is_empty() && plain.context_added.is_none());
    assert!(start_tools(&plain).is_empty());
    assert!(start_tools(&pick(&basis, &[("profile", "plain")]).unwrap()).is_empty());

    // The call asks.
    let asked = nesting(&basis, Family::MAIN, true).unwrap();
    assert_eq!(start_tools(&asked), NESTED_TOOL_NAMES);
    assert_eq!(asked.tools_added, NESTED_TOOL_NAMES);
    assert_eq!(
        asked.context_added.as_deref(),
        Some(agents::NESTED_INSTRUCTIONS.trim())
    );
    // Not the tool that saves a profile, which stays with the main agent.
    assert!(
        !asked
            .tools_added
            .iter()
            .any(|tool| tool == crate::crew::SAVE_WORKER_PROFILE)
    );

    // The profile asks, and its instructions come before the paragraph.
    let profiled = pick(&basis, &[("profile", "nests")]).unwrap();
    assert_eq!(start_tools(&profiled), NESTED_TOOL_NAMES);
    let context = profiled.context_added.unwrap();
    let profile_at = context.find("# Your profile (nests)\nSplit the review by file.");
    let paragraph_at = context.find("# Workers of your own");
    assert!(
        profile_at.is_some() && profile_at < paragraph_at,
        "{context}"
    );

    // The call beats the profile.
    let mut call = request(&[("profile", "nests")]);
    call.parameters.insert("nested".into(), Value::Bool(false));
    let refused = choose_in(&basis, &call, None, Family::MAIN).unwrap();
    assert!(start_tools(&refused).is_empty());
    assert!(
        !refused
            .context_added
            .unwrap()
            .contains("Workers of your own")
    );
    // The extra rules for a small model come last.
    let big = basis_with_worker(&basis, settings("opus", Effort::High));
    let mut call = request(&[("model", "luna")]);
    call.parameters.insert("nested".into(), Value::Bool(true));
    let context = choose_in(&big, &call, None, Family::MAIN)
        .unwrap()
        .context_added
        .unwrap();
    let (paragraph, rules) = (
        context.find("Workers of your own"),
        context.find("Extra rules"),
    );
    assert!(
        paragraph.is_some() && rules.is_some() && paragraph < rules,
        "{context}"
    );
    // The paragraph is in the first message and not in the instructions,
    // which a resumed conversation keeps as they were.
    let worker = agents::worker_config(&AgentSettings::for_test(&luna(), &luna())).unwrap();
    assert!(!format!("{worker:?}").contains("Workers of your own"));
}

/// How deep workers go, and how many a worker has, are held at each start.
/// A worker at the deepest level is given no start tools, a start that asks
/// for them there is refused with the reason, and a profile that says
/// `nested` is started without them.
#[test]
fn the_depth_and_the_limit_are_held_at_each_start() {
    let temp = tempfile::tempdir().unwrap();
    let mut basis = basis(temp.path(), luna());
    let mut nests = profile("nests", None, None);
    nests.nested = true;
    save(&basis, Place::User, nests);

    // Two levels (the default). The main agent's worker (level 1) may nest.
    assert_eq!(
        start_tools(&nesting(&basis, Family::MAIN, true).unwrap()),
        NESTED_TOOL_NAMES
    );
    // Its worker is at level 2, the last, and can't start workers.
    let refused = nesting(&basis, at(2, 0), true).unwrap_err();
    assert_eq!(
        refused,
        "The worker would be at level 2, where it can't start workers of its own, because the user allows 2 levels of workers (/worker-depth). Start it without nested, or tell your lead you need more levels."
    );
    // A worker at level 1 starts workers at level 2, without start tools.
    let plain = nesting(&basis, at(2, 0), false).unwrap();
    assert!(plain.tools_added.is_empty() && plain.context_added.is_none());
    // A profile that says `nested` is started without when it is at the last
    // level, and the start goes on.
    let profiled = choose_in(&basis, &request(&[("profile", "nests")]), None, at(2, 0)).unwrap();
    assert!(profiled.tools_added.is_empty() && profiled.context_added.is_none());
    // A worker at level 2 starting a level-3 worker is refused.
    assert_eq!(
        nesting(&basis, at(3, 0), false).unwrap_err(),
        "Workers can't start workers here. The user allows 2 levels of workers (/worker-depth), and you are at level 2. Do the work yourself, or tell your lead what you need."
    );

    // One level turns nesting off. The main agent's `nested` is refused.
    basis.worker_depth = 1;
    let refused = nesting(&basis, Family::MAIN, true).unwrap_err();
    assert!(
        refused.contains("because the user allows 1 level of workers (/worker-depth)"),
        "{refused}"
    );
    assert!(nesting(&basis, Family::MAIN, false).is_ok());
    // A worker that was given the tools before the user lowered the depth.
    assert_eq!(
        nesting(&basis, at(2, 0), false).unwrap_err(),
        "Workers can't start workers here. The user allows 1 level of workers (/worker-depth), and you are at level 1. Do the work yourself, or tell your lead what you need."
    );

    // Four levels, the most.
    basis.worker_depth = 4;
    assert!(nesting(&basis, at(3, 0), true).is_ok());
    assert!(nesting(&basis, at(4, 0), true).is_err());
    assert!(nesting(&basis, at(4, 0), false).is_ok());
    assert!(nesting(&basis, at(5, 0), false).is_err());

    // A worker has eight workers of its own at most, running or queued.
    basis.worker_depth = 2;
    assert!(nesting(&basis, at(2, 7), false).is_ok());
    let full = nesting(&basis, at(2, 8), false).unwrap_err();
    assert_eq!(
        full,
        "You already have 8 workers running or queued, the most one worker can have. Wait for some of them with wait_for_agents, or stop one you don't need with stop_agent, then start the next."
    );
    // The main agent has no such limit, only the places the session allows.
    assert!(nesting(&basis, at(1, 40), false).is_ok());
}

/// What a worker that starts workers reads when a start is refused doesn't
/// send it to `worker_options`, which it doesn't have.
#[test]
fn a_nested_worker_is_not_sent_to_worker_options() {
    let temp = tempfile::tempdir().unwrap();
    let basis = Basis {
        worker_depth: 3,
        ..basis(temp.path(), luna())
    };
    let refuse: Refuse = Arc::new(|model: &arko_ai::Model| {
        Some(format!(
            "Can't start a worker on {}. It needs a key, and this session has none. Ask the user to add it with /keys, or choose a model that worker_options lists as usable now.",
            model.id
        ))
    });
    let start = |parameters: &[(&str, &str)], refuse: Option<&Refuse>| {
        choose_in(&basis, &request(parameters), refuse, at(2, 0)).unwrap_err()
    };
    let main = |parameters: &[(&str, &str)], refuse: Option<&Refuse>| {
        choose_in(&basis, &request(parameters), refuse, Family::MAIN).unwrap_err()
    };
    type Case<'a> = (Vec<(&'a str, &'a str)>, Option<&'a Refuse>);
    let cases: Vec<Case> = vec![
        (vec![("model", "opus")], Some(&refuse)),
        (vec![("model", "gpt9")], None),
        (vec![("profile", "nobody")], None),
    ];
    for (parameters, refuse) in cases {
        let for_main = main(&parameters, refuse);
        assert!(for_main.contains("worker_options"), "{for_main}");
        let for_worker = start(&parameters, refuse);
        assert!(!for_worker.contains("worker_options"), "{for_worker}");
        assert!(for_worker.contains("your lead"), "{for_worker}");
        // (A worker can't ask the user. Its lead can.)
        assert!(!for_worker.contains("Ask the user"), "{for_worker}");
    }
    // The same for what is the user's to do.
    let keyed = start(&[("model", "opus")], Some(&refuse));
    assert!(
        keyed.contains("Tell your lead to ask the user to add it with /keys"),
        "{keyed}"
    );
    let asking = {
        let mut call = request(&[]);
        call.parameters.insert("nested".into(), Value::Bool(true));
        choose_in(&basis, &call, None, at(3, 0)).unwrap_err()
    };
    assert_eq!(
        asking,
        "The worker would be at level 3, where it can't start workers of its own, because the user allows 3 levels of workers (/worker-depth). Start it without nested, or tell your lead you need more levels."
    );
    let broken = {
        write_file(&basis, Place::User, "broken", "no front matter");
        start(&[("profile", "broken")], None)
    };
    assert!(
        !broken.contains("worker_options") && !broken.contains("Ask the user"),
        "{broken}"
    );
    assert!(
        broken.contains("Tell your lead to ask the user to fix the file, or use another profile."),
        "{broken}"
    );
    let only = Basis {
        choice: ModelChoice::Profiles,
        ..basis.clone()
    };
    let text = choose_in(&only, &request(&[("model", "sol")]), None, at(2, 0)).unwrap_err();
    assert!(
        text.contains("/worker-models profiles") && !text.contains("worker_options"),
        "{text}"
    );
}

/// With every worker on the session's worker model, no worker starts
/// workers, and a start that asks for it is told why.
#[test]
fn no_worker_nests_when_every_worker_runs_the_session_s_model() {
    let temp = tempfile::tempdir().unwrap();
    let basis = Basis {
        choice: ModelChoice::Session,
        ..basis(temp.path(), luna())
    };
    let refused = nesting(&basis, Family::MAIN, true).unwrap_err();
    assert_eq!(
        refused,
        "The user has every worker run the session's worker model (/worker-models session), and no worker starts workers then. Start the worker without nested."
    );
    assert!(
        nesting(&basis, Family::MAIN, false)
            .unwrap()
            .tools_added
            .is_empty()
    );
    assert!(pick(&basis, &[]).unwrap().tools_added.is_empty());
    // Profiles only still lets a worker nest, and the nested worker's own
    // starts follow the same setting.
    let profiles = Basis {
        choice: ModelChoice::Profiles,
        ..basis.clone()
    };
    assert_eq!(
        start_tools(&nesting(&profiles, Family::MAIN, true).unwrap()),
        NESTED_TOOL_NAMES
    );
    let refused = choose_in(&profiles, &request(&[("model", "sol")]), None, at(2, 0));
    assert!(refused.unwrap_err().contains("(/worker-models profiles)"));
}

/// `nested` is true or false, not a guess.
#[test]
fn nested_must_be_true_or_false() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), luna());
    let mut call = request(&[]);
    call.parameters
        .insert("nested".into(), Value::String("yes".into()));
    assert_eq!(
        choose(&basis, &call, None).unwrap_err(),
        "nested must be true or false"
    );
    call.parameters.insert("nested".into(), Value::Null);
    assert!(choose(&basis, &call, None).is_ok());
}

/// The credential check may run a command, so it is made where blocking
/// is allowed: on a multi-threaded runtime, on a single-threaded one and
/// outside any runtime.
#[test]
fn the_credential_check_runs_on_any_kind_of_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let basis = Arc::new(basis(temp.path(), settings("luna", Effort::High)));
    let refuse: Refuse = Arc::new(|_: &arko_ai::Model| Some("Checked.".to_string()));
    let start = {
        let (basis, refuse) = (basis.clone(), refuse.clone());
        move || choose(&basis, &request(&[]), Some(&refuse)).unwrap_err()
    };
    assert_eq!(start(), "Checked.");
    for runtime in [
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap(),
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap(),
    ] {
        assert_eq!(runtime.block_on(async { start() }), "Checked.");
        let start = start.clone();
        let from_a_task =
            runtime.block_on(async move { tokio::spawn(async move { start() }).await.unwrap() });
        assert_eq!(from_a_task, "Checked.");
    }
}

/// A refusal names the profiles there are, and the first few when a
/// project's folder holds hundreds.
#[test]
fn a_refusal_names_at_most_twenty_profiles() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    for number in 0..25 {
        save(
            &basis,
            Place::User,
            profile(&format!("p{number:02}"), None, None),
        );
    }
    let why = pick(&basis, &[("profile", "nobody")]).unwrap_err();
    assert!(
        why.starts_with("There is no worker profile `nobody`. The profiles are default, p00, p01,"),
        "{why}"
    );
    assert!(
        why.contains(", p19, and 5 more. worker_options lists them."),
        "{why}"
    );
    assert!(!why.contains("p20"), "{why}");
}

/// The profile names the main agent's instructions carry are valid ones, and
/// a model only when it resolves and is short, so no text a file made up
/// reaches the cached prompt.
#[test]
fn the_instructions_name_profiles_and_only_short_known_models() {
    let temp = tempfile::tempdir().unwrap();
    let basis = basis(temp.path(), settings("luna", Effort::High));
    let long = format!("openrouter:{}", "a".repeat(60));
    for (name, model) in [
        ("short", "sol"),
        ("long", long.as_str()),
        ("tick", "openrouter:a`b"),
        ("slug", "openrouter:anthropic/claude-opus-5.5"),
        ("unknown", "gpt9"),
    ] {
        write_file(
            &basis,
            Place::User,
            name,
            &format!("---\ndescription: D\nmodel: {model}\n---\n"),
        );
    }
    let named: Vec<(String, Option<String>)> = named_profiles(&basis.roots)
        .into_iter()
        .map(|profile| (profile.name, profile.model))
        .collect();
    assert_eq!(
        named,
        [
            ("long".to_string(), None),
            ("short".to_string(), Some("sol".to_string())),
            (
                "slug".to_string(),
                Some("openrouter:anthropic/claude-opus-5.5".to_string())
            ),
            ("tick".to_string(), None),
            ("unknown".to_string(), None),
        ]
    );
}
