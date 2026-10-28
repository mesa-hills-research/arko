//! What the page says of figures, workers, states and settings.

use crate::protocol::{CacheData, Delivery, ItemData, StatusData, WorkerData, WorkerState};

/// 2m14s, 45s, 1h05m.
pub fn duration(ms: f64) -> String {
    let s = (ms.max(0.0) / 1000.0).floor() as u64;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m{:02}s", s % 60);
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

/// 142k, 1.2M.
pub fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1e6;
        return if n >= 10_000_000 {
            format!("{m:.0}M")
        } else {
            format!("{m:.1}M")
        };
    }
    if n >= 1000 {
        return format!("{}k", (n as f64 / 1000.0).round());
    }
    n.to_string()
}

pub fn plural(n: impl std::fmt::Display + PartialEq<u32> + Copy, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A time in seconds as settings say it: `10 min`, `90 s`, `1 h`.
pub fn seconds_words(seconds: u64) -> String {
    if seconds.is_multiple_of(3600) {
        format!("{} h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// How long after the user's last message a monitor's warning wakes the
/// agent: "For 2 h after your last message", "Auto (54 min)", "Always",
/// "Never".
pub fn wakes_words(setting: Option<&str>, within: Option<u64>) -> String {
    match within {
        None => "Always".into(),
        Some(0) => "Never".into(),
        Some(seconds) if setting == Some("auto") => format!("Auto ({})", seconds_words(seconds)),
        Some(seconds) => format!("For {} after your last message", seconds_words(seconds)),
    }
}

/// Who chooses the workers' models, as `/worker-models` takes it, with the
/// library's names and sentences (`ModelChoice::label` and `about`, which
/// a test of Arko's finds in the built page).
pub const WORKER_MODELS: [(&str, &str, &str); 3] = [
    (
        "agent",
        "Main agent chooses",
        "The main agent chooses a profile, model and effort for each worker, and can save profiles.",
    ),
    (
        "profiles",
        "Profiles only",
        "The main agent starts workers with a profile only and can't choose a model or effort.",
    ),
    (
        "session",
        "Session's worker model",
        "Every worker runs the session's worker model, and none starts workers.",
    ),
];

/// What the worker depth setting does (the library's `workers::depth_about`).
pub const WORKER_DEPTH_ABOUT: &str = "How many levels of workers there can be, from 1 to 4. With 1 only the main agent starts workers. With more, a worker can start workers of its own when the agent that starts it says so, down to that level.";

/// What the worker retry cap does with `cap` (none: off), as the library's
/// `workers::retry_cap_about` says it.
pub fn retry_cap_about(cap: Option<u32>) -> String {
    match cap {
        None => "Workers keep retrying failed model requests. On, a worker fails after 10 in a row, and the workers it started stop with it.".into(),
        Some(cap) => format!(
            "A worker fails after {} in a row, and the workers it started stop with it. A model response starts the count again.",
            plural(cap, "failed model request", "failed model requests")
        ),
    }
}

/// What the message copies setting does (the library's `workers::copies_about`).
pub fn copies_about(on: bool) -> &'static str {
    if on {
        "The agent that started a worker gets a copy of each message you send the worker, with its next news."
    } else {
        "The agent that started a worker doesn’t see the messages you send the worker."
    }
}

/// What the built-in skills setting does (the library's
/// `builtin_skills::SETTING_ABOUT` and `AFTER_START`).
pub fn builtin_skills_about(started: bool) -> String {
    let mut text = String::from("Off leaves out all of them, whatever the rows below say.");
    if started {
        text.push_str(" The conversation going on keeps its skills. New workers and new conversations get the change.");
    }
    text
}

/// Who gets the extra rules for small models, as `/extra-rules` takes it.
pub const RULES: [(&str, &str, &str); 3] = [
    (
        "small",
        "Small models",
        "Agents on small models (Haiku, luna) get a few extra rules in their instructions.",
    ),
    (
        "all",
        "Every agent",
        "Every agent gets the extra rules for small models.",
    ),
    ("off", "Off", "No agent gets them."),
];

/// Which of the project's instruction files the agents get, as
/// `/project-instructions` takes it.
pub const PROJECT: [(&str, &str, &str); 3] = [
    (
        "one",
        "AGENTS.md, else CLAUDE.md",
        "CLAUDE.md only when there is no AGENTS.md.",
    ),
    (
        "both",
        "Both",
        "AGENTS.md and CLAUDE.md, each under its own name.",
    ),
    ("off", "Off", "The agents get neither file."),
];

/// How long a worker has run at `now`, in all its runs, or none until it has.
pub fn run_time(summary: &WorkerData, now: f64) -> Option<f64> {
    let ran = summary.ran_ms as f64;
    match summary.running_since {
        Some(since) => Some(ran + (now - since as f64).max(0.0)),
        None => (ran > 0.0).then_some(ran),
    }
}

/// What a worker's model calls cost: "$0.12", or "≈ $0.31 at API prices"
/// when a plan covered the calls, and nothing under half a cent.
pub fn cost_words(billed: f64, list: f64) -> String {
    if list < 0.005 || list.is_nan() {
        String::new()
    } else if list > billed + 0.005 {
        format!("≈ ${list:.2} at API prices")
    } else {
        format!("${billed:.2}")
    }
}

/// What a worker and the workers under it cost, once that's more than the
/// worker alone: "$0.50 with its workers".
pub fn family_cost_words(summary: &WorkerData) -> String {
    let list = summary.family_cost_list;
    if list < summary.cost_list + 0.005 || list.is_nan() {
        return String::new();
    }
    let billed = summary.family_cost_billed;
    if list > billed + 0.005 {
        format!("≈ ${list:.2} with its workers")
    } else {
        format!("${billed:.2} with its workers")
    }
}

/// A worker's notice label: its ID, and the worker that started it.
pub fn notice_label(agent: &str, summary: Option<&WorkerData>) -> String {
    match summary.and_then(|summary| summary.parent.as_deref()) {
        Some(parent) => format!("{agent} (under {parent})"),
        None => agent.to_string(),
    }
}

/// The workers in a family's order: each worker that no listed worker
/// started, then the workers it started, depth first, in the order they
/// started (the library's `remote::family_order`).
pub fn family_order<'a>(summaries: &[&'a WorkerData]) -> Vec<&'a WorkerData> {
    let ids: Vec<&str> = summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect();
    let mut order: Vec<&WorkerData> = Vec::new();
    fn place<'a>(
        summary: &'a WorkerData,
        summaries: &[&'a WorkerData],
        order: &mut Vec<&'a WorkerData>,
    ) {
        if order.iter().any(|placed| placed.id == summary.id) {
            return;
        }
        order.push(summary);
        for other in summaries {
            if other.id != summary.id && other.parent.as_deref() == Some(summary.id.as_str()) {
                place(other, summaries, order);
            }
        }
    }
    for summary in summaries {
        if summary
            .parent
            .as_deref()
            .is_none_or(|parent| !ids.contains(&parent))
        {
            place(summary, summaries, &mut order);
        }
    }
    for summary in summaries {
        if !order.iter().any(|placed| placed.id == summary.id) {
            order.push(summary);
        }
    }
    order
}

/// The workers in the order the sheet lists them: the families with a
/// worker going first, in the order they started, then the others, the
/// latest first, each family with its workers after it.
pub fn sheet_order<'a>(summaries: &[&'a WorkerData]) -> Vec<&'a WorkerData> {
    let ids: Vec<&str> = summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect();
    let mut families: Vec<Vec<&WorkerData>> = Vec::new();
    for summary in family_order(summaries) {
        let top = summary
            .parent
            .as_deref()
            .is_none_or(|parent| !ids.contains(&parent));
        if top || families.is_empty() {
            families.push(Vec::new());
        }
        families.last_mut().expect("a family").push(summary);
    }
    let going = |family: &Vec<&WorkerData>| family.iter().any(|summary| summary.state.is_active());
    let (active, done): (Vec<_>, Vec<_>) = families.into_iter().partition(going);
    active
        .into_iter()
        .flatten()
        .chain(done.into_iter().rev().flatten())
        .collect()
}

fn state_word(state: WorkerState) -> &'static str {
    match state {
        WorkerState::Queued => "queued",
        WorkerState::Running => "running",
        WorkerState::Finished => "finished",
        WorkerState::Failed => "failed",
        WorkerState::Stopped => "stopped",
    }
}

/// A worker's summary in words: its title, the meta line ("18 calls ·
/// 2m14s · running · under r1 · gpt-6.1-sol, high · profile judge ·
/// $0.12") and what it did last. `viewing` is the worker whose own view
/// shows, which says who started it already.
pub fn worker_words(
    summary: &WorkerData,
    now: f64,
    viewing: Option<&str>,
) -> (String, String, String) {
    let mut parts = vec![plural(summary.tool_calls, "call", "calls")];
    if let Some(ran) = run_time(summary, now) {
        parts.push(duration(ran));
    }
    parts.push(state_word(summary.state).into());
    if let Some(parent) = &summary.parent
        && Some(parent.as_str()) != viewing
    {
        parts.push(format!("under {parent}"));
    }
    if let Some(model) = &summary.model {
        parts.push(match &summary.effort {
            Some(effort) => format!("{model}, {effort}"),
            None => model.clone(),
        });
    }
    if let Some(profile) = &summary.profile {
        parts.push(format!("profile {profile}"));
    }
    for words in [
        cost_words(summary.cost_billed, summary.cost_list),
        family_cost_words(summary),
    ] {
        if !words.is_empty() {
            parts.push(words);
        }
    }
    let mut last = summary.last.clone().unwrap_or_default();
    if summary.state == WorkerState::Finished
        && let Some(line) = summary
            .report
            .as_deref()
            .and_then(|report| report.lines().find(|line| !line.trim().is_empty()))
    {
        last = line.to_string();
    }
    let title = if summary.title.is_empty() {
        summary.id.clone()
    } else {
        summary.title.clone()
    };
    (title, parts.join(" · "), last.trim().to_string())
}

/// The word on a user's message for what became of it, when that isn't the
/// usual (a new turn).
pub fn delivery_tag(item: &ItemData) -> Option<&'static str> {
    match item.delivery {
        Some(Delivery::Note) if item.pending => Some("pending"),
        Some(Delivery::Answer) => Some("answer"),
        Some(Delivery::Queued) => Some("queued"),
        Some(Delivery::Refresh) => Some("kept the cache warm"),
        _ => None,
    }
}

/// The connection to Arko.
#[derive(Clone, Debug, PartialEq)]
pub enum Connection {
    Connecting,
    Live,
    Offline,
}

/// The header's state, as a key for its look and a label. While a worker's
/// own view shows, `viewing` is its summary (or its ID alone before the
/// summary comes), and the state is the worker's.
pub fn state_of(
    status: Option<&StatusData>,
    connection: &Connection,
    viewing: Option<(&str, Option<&WorkerData>)>,
) -> (&'static str, String) {
    match connection {
        Connection::Offline => return ("offline", "Offline".into()),
        Connection::Connecting => return ("connecting", "Connecting".into()),
        Connection::Live => {}
    }
    let Some(status) = status else {
        return ("connecting", "Connecting".into());
    };
    if status.question.is_some() {
        return (
            "asking",
            if viewing.is_some() {
                "Main asks you"
            } else {
                "Asks you"
            }
            .into(),
        );
    }
    if let Some((id, summary)) = viewing {
        return match summary {
            None => ("idle", id.to_string()),
            Some(summary) => {
                let key = match summary.state {
                    WorkerState::Running => "working",
                    WorkerState::Failed => "failed",
                    _ => "idle",
                };
                let word = if summary.state == WorkerState::Running {
                    "working"
                } else {
                    state_word(summary.state)
                };
                (key, format!("{id} {word}"))
            }
        };
    }
    if status.working {
        return ("working", "Working".into());
    }
    ("idle", "Idle".into())
}

/// The figures of the agent whose transcript shows: context, its handover
/// size, the cost, and the cost at API prices of what a plan covered.
fn agent_figures(
    status: &StatusData,
    viewing: Option<&WorkerData>,
) -> (Option<u64>, Option<u64>, f64, f64) {
    match viewing {
        None => (
            status.context_tokens,
            (status.settings.handover_at > 0).then_some(status.settings.handover_at),
            status.cost,
            status.api_price.unwrap_or(0.0),
        ),
        Some(summary) => {
            let agent = status.agents.iter().find(|agent| agent.id == summary.id);
            let billed = summary.cost_billed;
            let list = summary.cost_list;
            (
                agent.and_then(|agent| agent.context_tokens),
                agent.and_then(|agent| agent.handover_at),
                if billed >= 0.005 { billed } else { 0.0 },
                if list > billed + 1e-9 && list >= 0.005 {
                    list
                } else {
                    0.0
                },
            )
        }
    }
}

/// The header's figures: context against the handover size, then cost (or
/// the plan's usage on a subscription).
pub fn figures(status: &StatusData, viewing: Option<&WorkerData>) -> Vec<String> {
    let (context, handover, cost, api) = agent_figures(status, viewing);
    let mut parts = Vec::new();
    if let Some(context) = context {
        parts.push(match handover {
            Some(handover) => format!("{}/{}", tokens(context), tokens(handover)),
            None => tokens(context),
        });
    }
    if let Some(usage) = &status.plan_usage {
        // "Claude plan 5 h 21%, 7 d 4%" as "5 h 21%, 7 d 4%".
        let lower = usage.to_lowercase();
        let rest = lower
            .find("plan ")
            .map_or(usage.as_str(), |at| usage[at + 5..].trim_start());
        parts.push(rest.to_string());
    }
    if cost > 0.0 {
        parts.push(format!("${cost:.2}"));
    }
    if api > 0.0 {
        parts.push(format!("≈ ${api:.2}"));
    }
    parts
}

/// The figures' tooltip: "Context 142k tokens, handover size 250k".
pub fn context_title(status: &StatusData, viewing: Option<&WorkerData>) -> String {
    let (context, handover, ..) = agent_figures(status, viewing);
    let Some(context) = context else {
        return String::new();
    };
    match handover {
        Some(handover) => format!(
            "Context {} tokens, handover size {}",
            tokens(context),
            tokens(handover)
        ),
        None => format!("Context {} tokens", tokens(context)),
    }
}

/// The prompt cache's timer: the time left (ms), 0 once it lapsed, or none
/// where it's hidden (while requests run, and where the lifetime or last
/// use isn't known). Never more than the lifetime.
pub fn cache_timer(cache: &CacheData, running: bool, now: f64) -> Option<f64> {
    if running {
        return None;
    }
    let (lifetime, refreshed) = (cache.lifetime? as f64, cache.refreshed_at? as f64);
    Some((refreshed + lifetime - now).min(lifetime).max(0.0))
}

/// A length of time on a clock: 0:05, 3:12, 1:02:03.
pub fn clock(ms: f64) -> String {
    let s = (ms.max(0.0) / 1000.0).floor() as u64;
    let (h, m) = (s / 3600, s / 60 % 60);
    if h > 0 {
        format!("{h}:{m:02}:{:02}", s % 60)
    } else {
        format!("{m}:{:02}", s % 60)
    }
}

/// "cache 3:12", counted up to the next second, or "cache expired".
pub fn cache_label(left: f64) -> String {
    if left <= 0.0 {
        "cache expired".into()
    } else {
        format!("cache {}", clock((left / 1000.0).ceil() * 1000.0))
    }
}

/// The timer's last minute warns.
pub fn cache_warns(left: f64) -> bool {
    left > 0.0 && left <= 60_000.0
}

/// A token count, short: 820, 8.2k, 142k, 1.02M.
pub fn cache_tokens(count: u64) -> String {
    let trimmed = |value: f64, decimals: usize| {
        let text = format!("{value:.decimals$}");
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    };
    if count < 1000 {
        return count.to_string();
    }
    let thousands = count as f64 / 1000.0;
    if (thousands * 10.0).round() / 10.0 < 10.0 {
        return format!("{}k", trimmed(thousands, 1));
    }
    if thousands.round() < 1000.0 {
        return format!("{}k", thousands.round());
    }
    format!("{}M", trimmed(count as f64 / 1e6, 2))
}

pub fn dollars(amount: f64) -> String {
    if amount > 0.0 && amount < 0.005 {
        "< $0.01".into()
    } else {
        format!("${amount:.2}")
    }
}

/// What a miss writes again: "about 180k tokens, ≈ $0.90", "about 180k
/// tokens of your Claude plan".
fn cache_recache(cache: &CacheData) -> Option<String> {
    let mut text = format!("about {} tokens", cache_tokens(cache.tokens?));
    if let Some(cost) = cache.miss_cost {
        text.push_str(&format!(", ≈ {}", dollars(cost)));
    } else if let Some(plan) = &cache.on_plan {
        text.push_str(&format!(" of {plan}"));
    }
    Some(text)
}

/// A moment in local time, as the page's clock reads it.
#[derive(Clone, Copy, Debug)]
pub struct Local {
    pub year: i32,
    /// 0 for January.
    pub month: u32,
    pub day: u32,
    /// 0 for Sunday.
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    /// The local day's number, counted from any fixed day.
    pub day_number: i64,
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "14:00" today, "Mon 09:00" within a week, else "Oct 20" (with the year
/// for another one).
pub fn when_words(then: Local, now: Local) -> String {
    let days = then.day_number - now.day_number;
    let time = format!("{:02}:{:02}", then.hour, then.minute);
    if days == 0 {
        return time;
    }
    if (1..7).contains(&days) {
        return format!("{} {time}", WEEKDAYS[then.weekday as usize % 7]);
    }
    let day = format!("{} {}", MONTHS[then.month as usize % 12], then.day);
    if then.year == now.year {
        day
    } else {
        format!("{day}, {}", then.year)
    }
}

/// What the timer means, for its tooltip. `until` is when the cache lapses
/// in words.
pub fn cache_title(left: f64, cache: &CacheData, until: &str) -> String {
    let recache = cache_recache(cache);
    if left <= 0.0 {
        return match recache {
            Some(recache) => format!("The next message re-caches {recache}."),
            None => "The prompt cache has expired.".into(),
        };
    }
    let mut text = format!("The prompt cache keeps this conversation until {until}.");
    if let Some(recache) = recache {
        text.push_str(&format!(" Letting it expire re-caches {recache}."));
    }
    text
}

/// An image's caption, its label and its size ("chart.png · 12 KB"), as
/// Studio says it. A label that ends with the size (`read_file` names its
/// images so) says it once.
pub fn image_caption(label: &str, bytes: u64) -> String {
    let size = match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes.div_ceil(1024)),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    };
    if label.is_empty() {
        size
    } else if label.ends_with(&size) {
        label.to_string()
    } else {
        format!("{label} · {size}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::worker;

    #[test]
    fn an_images_caption_is_its_label_and_size() {
        assert_eq!(image_caption("chart.png", 12_000), "chart.png · 12 KB");
        assert_eq!(image_caption("", 512), "512 B");
        assert_eq!(image_caption("photo.jpg", 3_500_000), "photo.jpg · 3.3 MB");
        assert_eq!(
            image_caption("chart.png, 457 KB", 467_200),
            "chart.png, 457 KB"
        );
    }

    #[test]
    fn figures_and_workers_read_briefly() {
        assert_eq!(duration(5_000.0), "5s");
        assert_eq!(duration(134_000.0), "2m14s");
        assert_eq!(duration(3_900_000.0), "1h05m");
        assert_eq!(tokens(820), "820");
        assert_eq!(tokens(142_318), "142k");
        assert_eq!(tokens(1_200_000), "1.2M");
        assert_eq!(tokens(12_000_000), "12M");
        let mut summary = worker("r2");
        summary.tool_calls = 18;
        summary.running_since = Some(0);
        summary.ran_ms = 4_000;
        summary.parent = Some("r1".into());
        summary.model = Some("gpt-6.1-sol".into());
        summary.effort = Some("high".into());
        summary.profile = Some("judge".into());
        summary.cost_billed = 0.12;
        summary.cost_list = 0.12;
        let (title, meta, _) = worker_words(&summary, 130_000.0, None);
        assert_eq!(title, "Task of r2");
        assert_eq!(
            meta,
            "18 calls · 2m14s · running · under r1 · gpt-6.1-sol, high · profile judge · $0.12"
        );
        // Not in the view of the worker that started it.
        assert!(!worker_words(&summary, 0.0, Some("r1")).1.contains("under"));
    }

    #[test]
    fn a_worker_that_a_worker_started_says_what_the_family_cost() {
        let mut summary = worker("r1");
        summary.cost_billed = 0.10;
        summary.cost_list = 0.10;
        summary.family_cost_billed = 0.50;
        summary.family_cost_list = 0.50;
        assert_eq!(family_cost_words(&summary), "$0.50 with its workers");
        summary.family_cost_list = 0.90;
        assert_eq!(family_cost_words(&summary), "≈ $0.90 with its workers");
        assert_eq!(cost_words(0.0, 0.31), "≈ $0.31 at API prices");
        assert_eq!(cost_words(0.0, 0.001), "");
        let mut child = worker("r2");
        child.parent = Some("r1".into());
        assert_eq!(notice_label("r2", Some(&child)), "r2 (under r1)");
        assert_eq!(notice_label("r1", Some(&summary)), "r1");
    }

    #[test]
    fn a_family_is_listed_with_each_workers_workers_after_it() {
        let mut r1 = worker("r1");
        r1.state = WorkerState::Finished;
        let r2 = worker("r2");
        let mut r3 = worker("r3");
        r3.parent = Some("r1".into());
        r3.state = WorkerState::Finished;
        let mut r4 = worker("r4");
        r4.state = WorkerState::Finished;
        let all = [&r1, &r2, &r3, &r4];
        let order: Vec<&str> = family_order(&all).iter().map(|w| w.id.as_str()).collect();
        assert_eq!(order, ["r1", "r3", "r2", "r4"]);
        let sheet: Vec<&str> = sheet_order(&all).iter().map(|w| w.id.as_str()).collect();
        assert_eq!(sheet, ["r2", "r4", "r1", "r3"]);
    }

    #[test]
    fn the_monitor_wakes_and_workers_settings_read_as_words() {
        assert_eq!(
            wakes_words(Some("2h"), Some(7200)),
            "For 2 h after your last message"
        );
        assert_eq!(wakes_words(Some("auto"), Some(3240)), "Auto (54 min)");
        assert_eq!(wakes_words(Some("always"), None), "Always");
        assert_eq!(wakes_words(Some("off"), Some(0)), "Never");
        assert!(
            retry_cap_about(Some(1))
                .starts_with("A worker fails after 1 failed model request in a row")
        );
        assert!(retry_cap_about(None).starts_with("Workers keep retrying"));
        assert!(builtin_skills_about(true).contains("keeps its skills"));
        assert!(!builtin_skills_about(false).contains("keeps its skills"));
    }

    #[test]
    fn the_cache_timer_counts_down_while_idle_and_warns_in_the_last_minute() {
        let cache = CacheData {
            lifetime: Some(300_000),
            refreshed_at: Some(0),
            ..CacheData::default()
        };
        assert_eq!(cache_timer(&cache, true, 0.0), None);
        assert_eq!(cache_timer(&CacheData::default(), false, 0.0), None);
        let left = cache_timer(&cache, false, 108_000.0).unwrap();
        assert_eq!(cache_label(left), "cache 3:12");
        assert!(!cache_warns(left));
        assert!(cache_warns(cache_timer(&cache, false, 250_000.0).unwrap()));
        assert_eq!(
            cache_label(cache_timer(&cache, false, 400_000.0).unwrap()),
            "cache expired"
        );
        // A clock that went back still shows at most the lifetime.
        assert_eq!(cache_timer(&cache, false, -60_000.0), Some(300_000.0));
        assert_eq!(cache_label(0.5), "cache 0:01");
    }

    #[test]
    fn the_cache_timers_tooltip_says_when_it_lapses_and_what_a_miss_recaches() {
        let cache = CacheData {
            lifetime: Some(300_000),
            refreshed_at: Some(0),
            tokens: Some(180_000),
            miss_cost: Some(0.9),
            ..CacheData::default()
        };
        assert_eq!(
            cache_title(1000.0, &cache, "14:05"),
            "The prompt cache keeps this conversation until 14:05. Letting it expire re-caches about 180k tokens, ≈ $0.90."
        );
        assert_eq!(
            cache_title(0.0, &cache, ""),
            "The next message re-caches about 180k tokens, ≈ $0.90."
        );
        let plan = CacheData {
            miss_cost: None,
            on_plan: Some("your Claude plan".into()),
            ..cache
        };
        assert_eq!(
            cache_title(0.0, &plan, ""),
            "The next message re-caches about 180k tokens of your Claude plan."
        );
        assert_eq!(cache_tokens(820), "820");
        assert_eq!(cache_tokens(8_240), "8.2k");
        assert_eq!(cache_tokens(142_318), "142k");
        assert_eq!(cache_tokens(1_020_000), "1.02M");
        let at = |day_number, weekday, hour| Local {
            year: 2026,
            month: 9,
            day: 20,
            weekday,
            hour,
            minute: 5,
            day_number,
        };
        assert_eq!(when_words(at(10, 1, 14), at(10, 1, 9)), "14:05");
        assert_eq!(when_words(at(11, 2, 9), at(10, 1, 9)), "Tue 09:05");
        assert_eq!(when_words(at(30, 2, 9), at(10, 1, 9)), "Oct 20");
    }

    #[test]
    fn a_user_message_is_tagged_only_for_what_is_unusual() {
        use crate::protocol::Kind;
        let mut item = crate::store::tests::item(1, Kind::User, "main", "hi");
        assert_eq!(delivery_tag(&item), None);
        item.delivery = Some(Delivery::Note);
        assert_eq!(delivery_tag(&item), None);
        item.pending = true;
        assert_eq!(delivery_tag(&item), Some("pending"));
        item.delivery = Some(Delivery::Answer);
        assert_eq!(delivery_tag(&item), Some("answer"));
    }

    #[test]
    fn the_header_shows_the_workers_own_figures_while_its_view_shows() {
        let mut status = StatusData {
            context_tokens: Some(142_000),
            cost: 1.2,
            ..StatusData::default()
        };
        status.settings.handover_at = 250_000;
        assert_eq!(figures(&status, None), ["142k/250k", "$1.20"]);
        status.agents = vec![crate::protocol::AgentData {
            id: "r1".into(),
            context_tokens: Some(30_000),
            handover_at: Some(200_000),
            ..Default::default()
        }];
        let mut summary = worker("r1");
        summary.cost_billed = 0.0;
        summary.cost_list = 0.4;
        assert_eq!(figures(&status, Some(&summary)), ["30k/200k", "≈ $0.40"]);
        assert_eq!(
            context_title(&status, None),
            "Context 142k tokens, handover size 250k"
        );
        status.plan_usage = Some("Claude plan 5 h 21%, 7 d 4%".into());
        assert_eq!(figures(&status, None)[1], "5 h 21%, 7 d 4%");
        let live = Connection::Live;
        assert_eq!(state_of(Some(&status), &live, None).1, "Idle");
        assert_eq!(
            state_of(Some(&status), &live, Some(("r1", Some(&summary)))).1,
            "r1 working"
        );
        assert_eq!(state_of(None, &Connection::Offline, None).1, "Offline");
    }
}
