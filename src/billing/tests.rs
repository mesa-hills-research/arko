use super::*;
use crate::providers::resolve_model;

fn model(name: &str) -> Model {
    resolve_model(name).unwrap()
}

/// What `gate` does with `billing` and `budget` on `name`, by the ChatGPT
/// login or an OpenAI API key (`key`), for the main agent or a worker
/// (`nested`), with the model allowed for the session or not, when the day's
/// spend is `spent`.
fn gate_of(
    billing: WorkerBilling,
    budget: Option<f64>,
    name: &str,
    key: bool,
    nested: bool,
    allowed: bool,
    spent: f64,
) -> Gate {
    let auth = key.then_some(OpenAiAuth::ApiKey);
    gate(
        billing,
        budget,
        &model(name),
        auth,
        nested,
        allowed,
        || spent,
        || vec!["cc:opus".to_string(), "sol".to_string()],
    )
}

/// A plan's model always starts. One billed per token is refused under
/// Plans only, asked about under Ask before API, and starts under API
/// allowed, for every kind of billing per token.
#[test]
fn each_setting_refuses_asks_or_starts_by_how_the_model_is_billed() {
    use WorkerBilling::{Api, Ask, Plans};
    // The plans: cc: models, and OpenAI models on the ChatGPT login.
    for billing in WorkerBilling::ALL {
        for (name, key) in [("cc:opus", false), ("cc:haiku", true), ("sol", false)] {
            assert_eq!(
                gate_of(billing, Some(1.0), name, key, true, false, 100.0),
                Gate::Go,
                "{name} under {billing:?}"
            );
        }
    }
    // Billed per token: an Anthropic or OpenAI API key, Gemini, OpenRouter.
    for (name, key) in [
        ("opus", false),
        ("sol", true),
        ("gemini", false),
        ("openrouter:anthropic/claude-opus-5.5", false),
    ] {
        let Gate::Refuse(why) = gate_of(Plans, None, name, key, false, true, 0.0) else {
            panic!("{name} under Plans only");
        };
        assert!(
            why.starts_with(
                "The user lets workers run only on their plans (/worker-billing plans), and "
            ),
            "{why}"
        );
        assert!(
            why.ends_with("is billed per token. Start it on a plan model instead. Those usable now are cc:opus and sol."),
            "{why}"
        );
        assert_eq!(
            gate_of(Ask, None, name, key, false, false, 0.0),
            Gate::Ask { over: None },
            "{name}"
        );
        assert_eq!(gate_of(Api, None, name, key, false, false, 0.0), Gate::Go);
        // Allowed for the session, it starts without asking.
        assert_eq!(gate_of(Ask, None, name, key, false, true, 0.0), Gate::Go);
    }
    // No plan model to use says what the user can do.
    let none = gate(
        Plans,
        None,
        &model("opus"),
        None,
        false,
        false,
        || 0.0,
        Vec::new,
    );
    assert_eq!(
        none,
        Gate::Refuse("The user lets workers run only on their plans (/worker-billing plans), and Opus 5.5 is billed per token. No plan model is usable now, so ask the user to sign in to Claude Code or ChatGPT, or to change /worker-billing.".into())
    );
}

/// A worker can't ask the user, so its start of a worker billed per token
/// is refused under Ask before API, unless the user allowed the model for
/// the session.
#[test]
fn a_worker_s_start_billed_per_token_is_refused_unless_the_model_is_allowed() {
    let Gate::Refuse(why) = gate_of(WorkerBilling::Ask, None, "opus", false, true, false, 0.0)
    else {
        panic!("a worker's start");
    };
    assert_eq!(
        why,
        "A worker on Opus 5.5 needs the user's OK, since it's billed per token, and only the main agent's starts can ask for it. Start it on a plan model instead. Those usable now are cc:opus and sol. You can also ask your lead."
    );
    assert_eq!(
        gate_of(WorkerBilling::Ask, None, "opus", false, true, true, 0.0),
        Gate::Go
    );
}

/// Once the day's spend reaches the budget, a start billed per token is
/// refused under API allowed and asked about under Ask before API, also for
/// a model allowed for the session. Below it nothing changes, and the spend
/// isn't even read without a budget.
#[test]
fn the_budget_reached_refuses_or_asks() {
    use WorkerBilling::{Api, Ask};
    let Gate::Refuse(why) = gate_of(Api, Some(20.0), "opus", false, false, false, 21.3) else {
        panic!("over the budget");
    };
    assert_eq!(
        why,
        "Today's API spend of $21.30 has reached the user's daily budget of $20 (/api-budget), so no worker starts on a model billed per token, such as Opus 5.5, until tomorrow. Start it on a plan model instead. Those usable now are cc:opus and sol."
    );
    assert!(
        matches!(
            gate_of(Api, Some(20.0), "opus", false, false, false, 20.0),
            Gate::Refuse(_)
        ),
        "reaching it is enough"
    );
    assert_eq!(
        gate_of(Api, Some(20.0), "opus", false, false, false, 19.99),
        Gate::Go
    );
    for allowed in [false, true] {
        assert_eq!(
            gate_of(Ask, Some(20.0), "opus", false, false, allowed, 21.3),
            Gate::Ask {
                over: Some((21.3, 20.0))
            }
        );
    }
    assert!(matches!(
        gate_of(Ask, Some(20.0), "opus", false, true, true, 21.3),
        Gate::Refuse(_)
    ));
    let read = std::cell::Cell::new(false);
    let started = gate(
        Api,
        None,
        &model("opus"),
        None,
        false,
        false,
        || {
            read.set(true);
            0.0
        },
        Vec::new,
    );
    assert_eq!(started, Gate::Go);
    assert!(!read.get(), "no budget, no spend read");
}

/// The card names the model, that it's billed per token and its price, the
/// task, and the budget when it was reached. Its choices allow once, allow
/// the model for the session, or deny.
#[test]
fn the_card_says_what_the_worker_costs() {
    let opus = model("opus");
    let (text, choices) = question(&opus, "Review the parser", None);
    let (input, output) = prices(&opus).unwrap();
    assert_eq!(
        text,
        format!(
            "Start a worker on Opus 5.5, billed per token? It costs ${} per million input tokens and ${} per million output tokens. The task is “Review the parser”.",
            per_million(input),
            per_million(output)
        )
    );
    assert_eq!(
        choices,
        ["Allow", "Allow Opus 5.5 for this session", "Deny"]
    );
    let (text, _) = question(&opus, "  ", Some((21.3, 20.0)));
    assert!(
        text.ends_with("per million output tokens. Today’s API spend of $21.30 has reached your daily budget of $20."),
        "{text}"
    );
    // A long task is cut, and one that ends a sentence gets no second
    // period.
    let (text, _) = question(&opus, &"word ".repeat(100), None);
    assert!(text.chars().count() < 300, "{text}");
    let (text, _) = question(&opus, "Review it.", None);
    assert!(text.ends_with(" The task is “Review it.”"), "{text}");
}

/// Only a choice alone allows. Words beside it, or of the user's own, go
/// back to the agent with a refusal, and so do "You decide" and "Not now".
#[test]
fn the_answer_decides_and_the_agent_reads_why_it_did_not_start() {
    let name = "Opus 5.5";
    assert_eq!(decision("Allow", name), Decision::Allow);
    assert_eq!(decision(" allow. ", name), Decision::Allow);
    assert_eq!(
        decision("Allow Opus 5.5 for this session", name),
        Decision::AllowForSession
    );
    assert_eq!(decision("Deny", name), Decision::Deny(None));
    assert_eq!(decision("deny", name), Decision::Deny(None));
    assert_eq!(decision("", name), Decision::Dismissed);
    assert_eq!(decision("You decide", name), Decision::LeftToAgent);
    assert_eq!(
        decision("Allow. Only for the tests", name),
        Decision::Deny(Some("Allow. Only for the tests".into()))
    );
    assert_eq!(
        decision("use sonnet", name),
        Decision::Deny(Some("use sonnet".into()))
    );
    let opus = model("opus");
    assert_eq!(
        refusal(&Decision::Deny(None), &opus),
        "The user denied a worker on Opus 5.5, which is billed per token, so it didn't start. Start it on a plan model, which worker_options lists, or ask the user what they want."
    );
    assert_eq!(
        refusal(&Decision::Deny(Some("use sonnet".into())), &opus),
        "The user didn't allow a worker on Opus 5.5, which is billed per token, and wrote “use sonnet”. Act on what they wrote. A worker on a model billed per token asks them again."
    );
    for decided in [
        Decision::LeftToAgent,
        Decision::Dismissed,
        Decision::Unanswered,
    ] {
        let why = refusal(&decided, &opus);
        assert!(why.contains("didn't start on Opus 5.5"), "{why}");
    }
    assert!(cant_ask(&opus).starts_with("No one can allow a worker billed per token"));
}

/// The setting's names and words, as every screen shows them, and how the
/// commands and the flags read it.
#[test]
fn the_setting_and_the_budget_read_and_write_alike() {
    assert_eq!(
        WorkerBilling::ALL.map(WorkerBilling::label),
        ["Plans only", "Ask before API", "API allowed"]
    );
    assert_eq!(WorkerBilling::default(), WorkerBilling::Ask);
    for billing in WorkerBilling::ALL {
        assert_eq!(WorkerBilling::parse(billing.name()), Some(billing));
        assert_eq!(
            billing.command(),
            format!("/worker-billing {}", billing.name())
        );
        assert!(!billing.about().contains(';'));
    }
    assert_eq!(WorkerBilling::parse(" API "), Some(WorkerBilling::Api));
    assert_eq!(WorkerBilling::parse("never"), None);
    assert_eq!(
        serde_json::to_string(&WorkerBilling::Plans).unwrap(),
        "\"plans\""
    );
    assert_eq!(parse_budget("off"), Ok(None));
    assert_eq!(parse_budget("on"), Ok(Some(DEFAULT_BUDGET)));
    assert_eq!(parse_budget(" 20 "), Ok(Some(20.0)));
    assert_eq!(parse_budget("$7.50"), Ok(Some(7.5)));
    for bad in ["0", "-3", "lots", "", "nan", "inf"] {
        let error = parse_budget(bad).unwrap_err();
        assert!(
            error.contains("isn't off, on or an amount of dollars"),
            "{bad}: {error}"
        );
    }
    assert!(check_budget(0.0).is_err());
    assert_eq!(budget_command(true), "/api-budget on");
    assert_eq!(budget_command(false), "/api-budget off");
    assert_eq!(
        budget_about(Some(20.0)),
        "Once $20 is billed per token today, across your sessions, new workers on API models stop starting. Running workers go on."
    );
    assert!(budget_about(None).starts_with("No daily limit. On, "));
    assert_eq!(money(20.0), "$20");
    assert_eq!(money(7.5), "$7.50");
    assert_eq!(money(0.254), "$0.25");
    assert_eq!(money(19.999), "$20");
    assert_eq!(
        budget_notice(WorkerBilling::Api, 21.3, 20.0),
        "Today’s API spend of $21.30 reached your daily budget of $20. New workers on API models don’t start until tomorrow. Running workers go on."
    );
    assert!(budget_notice(WorkerBilling::Ask, 21.3, 20.0).contains("ask you first"));
}

/// The label beside a model in a picker: the price of one billed per token,
/// and nothing for a plan's.
#[test]
fn a_model_billed_per_token_is_labelled_with_its_price() {
    let (input, output) = prices(&model("opus")).unwrap();
    assert_eq!(
        label("opus", None),
        Some(format!(
            "API, ${}/${}",
            per_million(input),
            per_million(output)
        ))
    );
    assert_eq!(label("cc:opus", None), None);
    assert_eq!(label("sol", Some(OpenAiAuth::ChatGpt)), None);
    assert!(label("sol", Some(OpenAiAuth::ApiKey)).is_some());
    assert_eq!(label("nothing-like-a-model", None), None);
    assert_eq!(per_million(5.0), "5");
    assert_eq!(per_million(1.25), "1.25");
    assert_eq!(per_million(0.1), "0.10");
    assert_eq!(per_million(0.075), "0.075");
}

/// The day starts at local midnight. What the agents' calls billed before it
/// belongs to the day before, and costs added by hand aren't an agent's.
#[test]
fn the_day_starts_at_local_midnight() {
    use arko_workflows::budget::CostEntry;
    let now = Local::now();
    let start = day_start(now);
    assert!(start <= now.with_timezone(&Utc));
    assert_eq!(start.with_timezone(&Local).date_naive(), now.date_naive());
    assert_eq!(
        start.with_timezone(&Local).time(),
        chrono::NaiveTime::MIN,
        "midnight"
    );
    // Midnight is the start of its own day.
    assert_eq!(day_start(start.with_timezone(&Local)), start);
    let at = |timestamp: DateTime<Utc>, amount: f64, agent: bool| CostEntry {
        amount,
        source: if agent {
            CostSource::Agent {
                agent_name: "worker".into(),
                usage: Box::default(),
            }
        } else {
            CostSource::Manual {
                description: "embeddings".into(),
            }
        },
        timestamp,
        metadata: None,
    };
    let second = chrono::Duration::seconds(1);
    let entries = [
        at(start - second, 4.0, true),
        at(start, 1.5, true),
        at(start + second, 2.0, true),
        at(start + second, 9.0, false),
    ];
    assert_eq!(billed_since(&entries, start), 3.5);
    assert_eq!(billed_since(&entries, start + second), 2.0);
}

/// Each session keeps its own spend of the day in the shared file, and the
/// day's total adds them up. A file of another day starts over.
#[test]
fn the_sessions_share_the_day_s_spend() {
    let (text, total) = recorded("", "2026-10-28", "a", 3.0);
    assert_eq!(total, 3.0);
    let (text, total) = recorded(&text, "2026-10-28", "b", 2.5);
    assert_eq!(total, 5.5);
    // A session's own spend is replaced, not added.
    let (text, total) = recorded(&text, "2026-10-28", "a", 4.0);
    assert_eq!(total, 6.5);
    // The next day counts from nothing.
    let (next, total) = recorded(&text, "2026-10-29", "b", 1.0);
    assert_eq!(total, 1.0);
    assert!(!next.contains("2026-10-28"), "{next}");
    // A file that can't be read starts over.
    assert_eq!(recorded("{not json", "2026-10-29", "a", 2.0).1, 2.0);

    // Through the file in Arko's folder, the other sessions count, and a
    // session without a workflow yet has spent nothing.
    let temp = tempfile::tempdir().unwrap();
    let day = Local::now().date_naive().to_string();
    share(&temp.path().join(SPEND_FILE), &day, "other", 12.0).unwrap();
    let mine = DaySpend::new(Some(temp.path()), "mine");
    assert_eq!(mine.today(None, true), 12.0);
    let alone = DaySpend::new(None, "alone");
    assert_eq!(alone.today(None, true), 0.0);
    // The user is told once a day.
    assert!(mine.tell_once());
    assert!(!mine.tell_once());
}

/// A model the user allowed for the session stays allowed when the session
/// is opened again.
#[test]
fn a_model_allowed_for_the_session_is_kept_in_its_folder() {
    let temp = tempfile::tempdir().unwrap();
    let allowed = Allowed::load(temp.path());
    let opus = model("opus");
    assert!(!allowed.has(&opus));
    allowed.add(&opus);
    assert!(allowed.has(&opus));
    assert!(!allowed.has(&model("sonnet")));
    let again = Allowed::load(temp.path());
    assert!(again.has(&opus));
}
