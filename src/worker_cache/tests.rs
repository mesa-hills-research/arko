use super::*;
use crate::agents::{Effort, ModelSettings};

/// The generation settings of a worker on `name`, with the long cache when
/// `long`, and the short one asked for outright when `short`.
fn llm(name: &str, long: bool, short: bool) -> arko_agents::AgentLLMConfig {
    let model = providers::resolve_model(name).unwrap();
    let settings = ModelSettings {
        model: providers::model_name(&model),
        effort: Effort::High,
        fast: false,
        cache_1h: long,
        handover_at: None,
    };
    let mut llm = settings.llm_config(false).unwrap();
    if short {
        providers::ask_short_cache(&model, &mut llm.generation_config);
    }
    llm
}

fn at(minutes: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap() + chrono::Duration::minutes(minutes)
}

/// How long each run's cache lasts, by its provider and the cache it
/// asked for.
#[test]
fn a_workers_cache_lasts_as_its_run_set_it_up() {
    let lasts = |llm: &arko_agents::AgentLLMConfig, auth| {
        of("r1", llm, at(0), 100_000, auth).map(|cache| (cache.expires_at - at(0)).num_minutes())
    };
    let key = Some(OpenAiAuth::ApiKey);
    // Claude by API key: 5 minutes, or the hour.
    assert_eq!(lasts(&llm("opus", false, false), key), Some(5));
    assert_eq!(lasts(&llm("opus", true, false), key), Some(60));
    // Claude Code: an hour on a Claude plan, unless the short one was asked.
    assert_eq!(lasts(&llm("cc:opus", false, false), None), Some(60));
    assert_eq!(lasts(&llm("cc:opus", false, true), None), Some(5));
    // The ChatGPT login keeps its own 30 minutes.
    assert_eq!(
        lasts(&llm("sol", false, false), Some(OpenAiAuth::ChatGpt)),
        Some(30)
    );
    // Gemini documents none, so nothing is said.
    assert_eq!(lasts(&llm("gemini", false, false), key), None);
}

/// Continuing re-reads the conversation and writes it to the cache again:
/// at the model's cache write price, as `/cost` counts it. On Claude that's
/// 1.25 times the input price, twice with the long cache. On OpenAI it's the
/// table's write price, past the long-context threshold here. A plan pays
/// instead where one does.
#[test]
fn continuing_costs_the_re_read_and_the_cache_write() {
    let opus = providers::resolve_model("opus").unwrap();
    let input = opus
        .pricing
        .at(std::time::SystemTime::now())
        .input_cost_per_million;
    let cost =
        |llm: &arko_agents::AgentLLMConfig, auth| of("r1", llm, at(0), 400_000, auth).unwrap().cost;
    let key = Some(OpenAiAuth::ApiKey);
    let short = cost(&llm("opus", false, false), key);
    assert_eq!((short.tokens, short.plan), (400_000, None));
    let dollars = short.dollars.unwrap();
    assert!(
        (dollars - 0.4 * input * 1.25).abs() < 0.01,
        "{dollars} at {input} a million"
    );
    let long = cost(&llm("opus", true, false), key).dollars.unwrap();
    assert!((long - 0.4 * input * 2.0).abs() < 0.01, "{long}");
    let sol = providers::resolve_model("sol").unwrap();
    let sol_pricing = sol.pricing.at(std::time::SystemTime::now());
    assert!(400_000 > sol_pricing.large_input_threshold.unwrap());
    let sol_write = sol_pricing.large_cache_write_cost_per_million.unwrap();
    let on_key = cost(&llm("sol", false, false), key).dollars.unwrap();
    assert!((on_key - 0.4 * sol_write).abs() < 0.01, "{on_key}");
    assert_eq!(
        cost(&llm("cc:opus", false, false), None).plan,
        Some("your Claude plan")
    );
    assert_eq!(
        cost(&llm("sol", false, false), Some(OpenAiAuth::ChatGpt)).plan,
        Some("your ChatGPT plan")
    );
}

/// Continuing asks first once the cache has expired, and only for a
/// conversation of at least the threshold.
#[test]
fn continuing_asks_only_after_the_cache_expired_and_past_the_threshold() {
    let llm = llm("cc:opus", false, false);
    let big = of("r3", &llm, at(0), 340_000, None).unwrap();
    assert!(!big.asks(at(59)), "warm");
    assert!(big.asks(at(60)));
    let small = of("r3", &llm, at(0), CONFIRM_TOKENS - 1, None).unwrap();
    assert!(small.expired(at(180)) && !small.asks(at(180)));
    assert!(
        of("r3", &llm, at(0), CONFIRM_TOKENS, None)
            .unwrap()
            .asks(at(180))
    );
}

/// The warning, the agent's refusal and the line `list_agents` shows.
#[test]
fn the_words_say_when_it_expired_and_what_continuing_costs() {
    let cc = of("r3", &llm("cc:opus", false, false), at(0), 340_000, None).unwrap();
    assert_eq!(
        cc.warning(at(180)),
        "r3's cache expired 2 hours ago. Continuing re-reads about 340k tokens without it, using your Claude plan."
    );
    assert_eq!(
        cc.refusal(at(180)),
        "r3's cache expired 2 hours ago. Continuing re-reads about 340k tokens without it, using your Claude plan. Start a fresh worker for this, or call message_agent again with confirm_uncached: true to continue r3."
    );
    assert_eq!(
        cc.line(at(180)),
        "cache expired 2 h ago, continuing re-reads about 340k tokens"
    );
    assert_eq!(
        cc.line(at(48)),
        "cache lasts 12 min more, about 340k tokens"
    );
    let opus = WorkerCache {
        cost: SwitchCost {
            tokens: 340_000,
            dollars: Some(1.7),
            plan: None,
        },
        ..cc.clone()
    };
    assert_eq!(
        opus.warning(at(61)),
        "r3's cache expired 1 minute ago. Continuing re-reads about 340k tokens without it (≈ $1.70 at API prices)."
    );
    assert_eq!(
        opus.warning(at(60)),
        "r3's cache expired just now. Continuing re-reads about 340k tokens without it (≈ $1.70 at API prices)."
    );
    assert!(
        opus.warning(at(60 + 3 * 1440))
            .contains("expired 3 days ago")
    );
    for text in [cc.warning(at(180)), cc.refusal(at(180)), cc.line(at(180))] {
        assert!(!text.contains(';'), "{text}");
    }
    assert_eq!(short_tokens(820), "820");
    assert_eq!(short_tokens(8_200), "8.2k");
    assert_eq!(short_tokens(1_200_000), "1.2M");
}
