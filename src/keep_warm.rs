//! Keep-warm: while the session waits for the user's next message, the
//! main agent's prompt cache is refreshed out of band before it lapses
//! ([`cache_timer::refresh_at`]), until one of its [`Limits`] ends it.
//! `/keep-warm` (or an app's [`Request::KeepWarm`](crate::Request::KeepWarm))
//! turns it on for the session, and the setting, kept in Arko's folder
//! ([`load`], [`save`]), for every session as it opens. It is off unless the
//! user turns it on.
//!
//! Each request moves its next refresh (a turn's, a refresh the user asked
//! for), and only the refreshes it sends count toward its limits. A refresh
//! that fails ends it with a notice, and so does a provider that takes no
//! refresh out of band, so it never adds a message to the conversation. When
//! it ends, a notice says why, how many refreshes it sent and what they
//! cost.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::cache_timer::{self, dollars, token_count};
use crate::lock;
use crate::remote::{Level, Source};

/// The setting's name, as /settings, the web page and what Arko says of it
/// call it.
pub const NAME: &str = "Keep warm";

/// The command that turns it on and off, and sets the setting.
pub const COMMAND: &str = "/keep-warm";

/// The file in Arko's folder that holds the setting.
const FILE: &str = "keep-warm.json";

/// The longest a time limit reaches, from now or from the latest request.
const MOST_HOURS: u32 = 24;

/// The most refreshes a number of them can ask for.
const MOST_REFRESHES: u32 = 1_000;

/// What ends keep-warm, whichever comes first. A limit left out doesn't
/// apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Limits {
    /// It ends at this time (Unix milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<i64>,
    /// It ends after this many refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshes: Option<u32>,
    /// It stops this many hours after the latest request it didn't send
    /// itself, and starts again with the next one. The setting has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_hours: Option<u32>,
    /// It ends before its refreshes cost more than this many dollars, at API
    /// prices (what a plan's would cost on a key). Where Arko has no price
    /// for the model, it doesn't apply, and Arko says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollars: Option<f64>,
}

/// Keep-warm in the status, while it is on and the model can refresh its
/// cache out of band.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Status {
    /// When it stops (Unix milliseconds): its end, or its hours after the
    /// latest request, whichever comes first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<i64>,
    /// How many refreshes it has left, where a number of them limits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<u32>,
    /// When its next refresh goes (Unix milliseconds). `None` while a turn
    /// runs (a question waiting for its answer too, since a refresh can't go
    /// then) and when no refresh comes before it stops. While it is set, the
    /// cache doesn't lapse at its `expires_at`, so a reminder of that waits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_at: Option<i64>,
}

/// Whether the main agent's prompt cache on `model`, as Arko configures it,
/// refreshes out of band: Anthropic's models, `cc:` models and OpenAI's.
/// OpenRouter's Claude models think with an effort, which a capped refresh
/// would change, and the others give no cache lifetime.
// shortcut: mirrors arko_agents' rules for the configurations Arko makes,
// until the framework says before a refresh whether it can.
pub(crate) fn refreshes_out_of_band(model: &arko_ai::Model) -> bool {
    use arko_ai::Provider;
    matches!(
        model.provider,
        Provider::Anthropic | Provider::ClaudeCode | Provider::OpenAI
    )
}

/// The model of the conversation the next message goes to, as the status's
/// `models` name it.
pub(crate) fn conversation_model(models: &crate::remote::Models) -> Option<arko_ai::Model> {
    let name = if models.conversation.is_empty() {
        &models.main
    } else {
        &models.conversation
    };
    crate::providers::resolve_model(name).ok()
}

/// The setting, which keeps every session's cache warm while it waits for
/// the user: its hours and dollars, or `None` (off, the default). A missing
/// or unreadable file is off.
pub fn load(root: &Path) -> Option<Limits> {
    let text = std::fs::read_to_string(root.join(FILE)).ok()?;
    let saved: Limits = serde_json::from_str(&text).ok()?;
    Some(Limits {
        idle_hours: Some(
            saved
                .idle_hours
                .filter(|hours| (1..=MOST_HOURS).contains(hours))?,
        ),
        dollars: saved.dollars.filter(|dollars| *dollars > 0.0),
        ..Limits::default()
    })
}

/// Keeps `setting` (its hours and dollars), or turns the setting off
/// (`None`). Sessions open with it from then on.
pub fn save(root: &Path, setting: Option<Limits>) -> Result<(), String> {
    let unsaved = |error: String| format!("Couldn't save the keep-warm setting ({error})");
    let path = root.join(FILE);
    let Some(setting) = setting else {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(unsaved(error.to_string()))
            }
            _ => Ok(()),
        };
    };
    let kept = Limits {
        idle_hours: setting.idle_hours,
        dollars: setting.dollars,
        ..Limits::default()
    };
    let text = serde_json::to_string_pretty(&kept).map_err(|error| unsaved(error.to_string()))?;
    std::fs::create_dir_all(root)
        .and_then(|()| crate::session::write_whole(&path, text.as_bytes()))
        .map_err(|error| unsaved(error.to_string()))
}

/// The command that sets `setting` for every session, such as
/// `/keep-warm default 15h $5` or `/keep-warm default off`. An app sends it
/// to the session as the user's input.
pub fn setting_command(setting: Option<Limits>) -> String {
    match setting.and_then(|limits| Some((limits.idle_hours?, limits.dollars))) {
        Some((hours, Some(dollars))) => {
            format!("{COMMAND} default {hours}h ${}", plain_dollars(dollars))
        }
        Some((hours, None)) => format!("{COMMAND} default {hours}h"),
        None => format!("{COMMAND} default off"),
    }
}

/// The setting in a sentence.
pub fn setting_sentence(setting: Option<Limits>) -> String {
    match setting.and_then(|limits| Some((limits.idle_hours?, limits.dollars))) {
        Some((hours, most)) => format!(
            "Every session keeps its prompt cache warm while it waits for you, for up to {}{}.",
            hours_words(hours),
            most.map(|most| format!(" after the last message or {}", dollars(most)))
                .unwrap_or_else(|| " after the last message".into())
        ),
        None => "Sessions keep their prompt cache warm only when you turn it on.".into(),
    }
}

/// The hours ← and → step through in the settings, off first.
pub const SETTING_HOURS: [u32; 8] = [0, 1, 2, 4, 8, 12, 15, 24];

/// The dollar limits ← and → step through in the settings, none first.
pub const SETTING_DOLLARS: [f64; 6] = [0.0, 1.0, 2.0, 5.0, 10.0, 20.0];

/// What `/keep-warm` takes.
pub const USAGE: &str = "/keep-warm 8h keeps the prompt cache warm for eight hours, /keep-warm until 7:00 until then, and /keep-warm 10 for ten refreshes. Add $5 to stop it at five dollars. /keep-warm off stops it, and /keep-warm default 15h $5 keeps every session warm while it waits for you.";

/// What a `/keep-warm` line asks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Words {
    /// What it is now.
    Show,
    Off,
    /// On, for this session.
    On(Limits),
    /// The setting for every session, or off.
    Setting(Option<Limits>),
}

/// Reads the words after `/keep-warm`, at `now`. A time (`8h`, `90m`) runs
/// from now, `until` takes a time of day (`7:00`, `7am`, `19:30`, the next
/// one to come), a number is of refreshes, and `$5` a dollar limit. The
/// setting (`default`) takes hours, and dollars after them.
pub(crate) fn parse(argument: &str, now: DateTime<Local>) -> Result<Words, String> {
    let words: Vec<String> = argument
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    match words.as_slice() {
        [] => return Ok(Words::Show),
        [default] if default == "default" => return Ok(Words::Show),
        [off] if off == "off" => return Ok(Words::Off),
        [default, off] if default == "default" && off == "off" => {
            return Ok(Words::Setting(None));
        }
        _ => {}
    }
    let (setting, words) = match words.split_first() {
        Some((first, rest)) if first == "default" => (true, rest),
        _ => (false, &words[..]),
    };
    let mut limits = Limits::default();
    let mut hours = None;
    let mut words = words.iter().map(String::as_str).peekable();
    let twice = |word: &str| format!("`{word}` repeats a limit. {USAGE}");
    while let Some(word) = words.next() {
        if word == "until" && !setting {
            let mut time = words
                .next()
                .ok_or("Give a time of day after until, such as until 7:00.")?
                .to_string();
            if let Some(half) = words.next_if(|next| matches!(*next, "am" | "pm")) {
                time.push_str(half);
            }
            let at = next_time(&time, now)?;
            if limits.ends_at.replace(at).is_some() {
                return Err(twice(word));
            }
        } else if let Some(amount) = word.strip_prefix('$') {
            let amount = amount
                .parse::<f64>()
                .ok()
                .filter(|amount| amount.is_finite() && *amount > 0.0)
                .ok_or_else(|| format!("`{word}` isn't an amount of dollars, such as $5."))?;
            if limits.dollars.replace(amount).is_some() {
                return Err(twice(word));
            }
        } else if let Ok(count) = word.parse::<u32>()
            && !setting
        {
            if !(1..=MOST_REFRESHES).contains(&count) {
                return Err(format!(
                    "Give from 1 to {MOST_REFRESHES} refreshes, such as /keep-warm 10."
                ));
            }
            if limits.refreshes.replace(count).is_some() {
                return Err(twice(word));
            }
        } else if let Some(minutes) = minutes_of(word) {
            if hours.replace(minutes).is_some() || limits.ends_at.is_some() {
                return Err(twice(word));
            }
        } else if setting {
            return Err(format!(
                "`{word}` isn't a number of hours or dollars. /keep-warm default 15h $5 keeps every session warm for up to 15 hours after its last message or $5, and /keep-warm default off turns that off."
            ));
        } else {
            return Err(format!(
                "`{word}` isn't a time, a time of day, a number of refreshes or dollars. {USAGE}"
            ));
        }
    }
    if setting {
        let hours = hours
            .filter(|minutes| minutes % 60 == 0)
            .map(|minutes| minutes / 60)
            .ok_or("Give the hours in whole hours, such as /keep-warm default 15h $5.")?;
        limits.idle_hours = Some(hours as u32);
        return Ok(Words::Setting(Some(limits)));
    }
    if let Some(minutes) = hours {
        if limits.ends_at.is_some() {
            return Err(format!("Give a time or a time of day, not both. {USAGE}"));
        }
        limits.ends_at = Some(now.timestamp_millis() + minutes * 60_000);
    }
    if limits.ends_at.is_none() && limits.refreshes.is_none() {
        return Err(format!(
            "Give a time, a time of day or a number of refreshes too. {USAGE}"
        ));
    }
    Ok(Words::On(limits))
}

/// The minutes in `word`, a whole number of minutes or hours (`90m`, `8h`)
/// up to a day.
fn minutes_of(word: &str) -> Option<i64> {
    let (number, scale) = match word.chars().last()? {
        'm' => (&word[..word.len() - 1], 1),
        'h' => (&word[..word.len() - 1], 60),
        _ => return None,
    };
    let minutes = number.parse::<i64>().ok()?.checked_mul(scale)?;
    (1..=MOST_HOURS as i64 * 60)
        .contains(&minutes)
        .then_some(minutes)
}

/// The next time of day `text` names after `now` (Unix milliseconds):
/// `7`, `7:00`, `07:30`, `19:30`, `7am`, `7:30pm`.
fn next_time(text: &str, now: DateTime<Local>) -> Result<i64, String> {
    let invalid = || format!("`{text}` isn't a time of day, such as 7:00, 7am or 19:30.");
    let (clock, half) = match text.strip_suffix("am") {
        Some(clock) => (clock, Some(false)),
        None => match text.strip_suffix("pm") {
            Some(clock) => (clock, Some(true)),
            None => (text, None),
        },
    };
    let (hour, minute) = clock.split_once(':').unwrap_or((clock, "0"));
    let (Ok(mut hour), Ok(minute)) = (hour.parse::<u32>(), minute.parse::<u32>()) else {
        return Err(invalid());
    };
    match half {
        Some(_) if !(1..=12).contains(&hour) => return Err(invalid()),
        Some(pm) => hour = hour % 12 + if pm { 12 } else { 0 },
        None => {}
    }
    let mut day = now.date_naive();
    for _ in 0..2 {
        let at = day
            .and_hms_opt(hour, minute, 0)
            .ok_or_else(invalid)?
            .and_local_timezone(Local)
            .earliest()
            .map(|at| at.timestamp_millis());
        if let Some(at) = at.filter(|at| *at > now.timestamp_millis()) {
            return Ok(at);
        }
        day = day.succ_opt().ok_or_else(invalid)?;
    }
    Err(invalid())
}

/// What ended keep-warm, or stopped it until the next request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stop {
    /// Its end came (Unix milliseconds).
    Ended(i64),
    /// It sent its number of refreshes.
    Refreshes(u32),
    /// Its hours after the latest request passed. It starts again with the
    /// next request.
    Idle(u32),
    /// Its next refresh would cost more than its dollars.
    Dollars(f64),
    /// The cache lapsed before its refresh (the computer slept).
    Lapsed,
    /// A refresh failed, for this reason.
    Failed(String),
    /// The provider takes no refresh out of band.
    Unsupported,
}

/// What keep-warm does when the session's loop wakes for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    Wait,
    Refresh,
    Stop(Stop),
}

/// What keep-warm's refreshes sent, since it started or last stopped for
/// its hours.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tally {
    sent: u32,
    /// What they billed, in dollars.
    billed: f64,
    /// The tokens they read from the cache.
    read: usize,
}

/// Keep-warm in a session, while it is on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeepWarm {
    pub(crate) limits: Limits,
    /// It comes from the setting (else the session turned it on).
    pub(crate) from_setting: bool,
    tally: Tally,
    /// All its refreshes, and what they cost at API prices.
    sent: u32,
    spent: f64,
    /// What its latest refresh cost at API prices, which the next is taken
    /// to cost.
    last_cost: f64,
    /// When the idle stretch began, the latest request it didn't send, and
    /// the time its own latest refresh left (Unix milliseconds).
    since: Option<i64>,
    own: Option<i64>,
    /// The idle stretch its hours stopped it in, until a request starts
    /// another.
    paused: Option<i64>,
    /// It said that Arko has no price for the refreshes.
    pub(crate) unpriced_told: bool,
}

impl KeepWarm {
    pub(crate) fn new(limits: Limits, from_setting: bool) -> Self {
        Self {
            limits,
            from_setting,
            tally: Tally::default(),
            sent: 0,
            spent: 0.0,
            last_cost: 0.0,
            since: None,
            own: None,
            paused: None,
            unpriced_told: false,
        }
    }

    /// When the idle stretch of a cache last used at `used` began: its
    /// latest request that keep-warm didn't send.
    fn stretch_start(&self, used: i64) -> i64 {
        match (self.own, self.since) {
            (Some(own), Some(since)) if own == used => since,
            _ => used,
        }
    }

    /// When it stops, for a cache last used at `used`.
    fn ends_at(&self, used: Option<i64>) -> Option<i64> {
        let idle = self
            .limits
            .idle_hours
            .zip(used)
            .map(|(hours, used)| self.stretch_start(used) + i64::from(hours) * 3_600_000);
        [self.limits.ends_at, idle].into_iter().flatten().min()
    }

    fn left(&self) -> Option<u32> {
        self.limits
            .refreshes
            .map(|refreshes| refreshes.saturating_sub(self.sent))
    }

    /// Whether its next refresh would pass its dollars.
    fn spent_out(&self) -> bool {
        self.limits
            .dollars
            .is_some_and(|most| self.spent + self.last_cost > most + 1e-9)
    }

    /// Whether its hours stopped it in this idle stretch.
    fn paused(&self, used: Option<i64>) -> bool {
        self.paused.is_some() && self.paused == used.map(|used| self.stretch_start(used))
    }

    /// When the next refresh goes for a cache last used at `used` that lasts
    /// `lifetime`, if one comes before it stops.
    fn next_at(&self, used: i64, lifetime: Duration) -> Option<i64> {
        if self.left() == Some(0) || self.spent_out() || self.paused(Some(used)) {
            return None;
        }
        let due = cache_timer::refresh_at(used, lifetime);
        match self.ends_at(Some(used)) {
            Some(end) if due > end => None,
            _ => Some(due),
        }
    }

    /// When the session's loop wakes for it, for the cache last used at
    /// `used` that lasts `lifetime`: for the next refresh, or to stop.
    pub(crate) fn wake_at(&self, used: Option<i64>, lifetime: Option<Duration>) -> Option<i64> {
        if self.left() == Some(0) || self.spent_out() {
            return Some(i64::MIN);
        }
        if self.paused(used) {
            return None;
        }
        let due = used
            .zip(lifetime)
            .map(|(used, lifetime)| cache_timer::refresh_at(used, lifetime));
        [due, self.ends_at(used)].into_iter().flatten().min()
    }

    /// What it does at `now`, for the cache last used at `used` that lasts
    /// `lifetime`.
    pub(crate) fn step(&self, now: i64, used: Option<i64>, lifetime: Option<Duration>) -> Step {
        if let Some(refreshes) = self.limits.refreshes.filter(|_| self.left() == Some(0)) {
            return Step::Stop(Stop::Refreshes(refreshes));
        }
        if let Some(most) = self.limits.dollars.filter(|_| self.spent_out()) {
            return Step::Stop(Stop::Dollars(most));
        }
        if self.paused(used) {
            return Step::Wait;
        }
        if let Some(end) = self.ends_at(used).filter(|end| now >= *end) {
            return Step::Stop(match self.limits.ends_at {
                Some(at) if at == end => Stop::Ended(at),
                _ => Stop::Idle(self.limits.idle_hours.unwrap_or_default()),
            });
        }
        let Some((used, lifetime)) = used.zip(lifetime) else {
            return Step::Wait;
        };
        if now < cache_timer::refresh_at(used, lifetime) {
            Step::Wait
        } else if now >= cache_timer::lapses_at(used, lifetime) {
            Step::Stop(Stop::Lapsed)
        } else {
            Step::Refresh
        }
    }

    /// Counts a refresh of the cache last used at `used` that left it last
    /// used at `refreshed` and had `usage`.
    pub(crate) fn sent(&mut self, used: i64, refreshed: Option<i64>, usage: &arko_ai::UsageInfo) {
        self.since = Some(self.stretch_start(used));
        self.own = refreshed;
        let billed = usage.billed();
        let cost = usage.total_cost.max(billed);
        self.tally.sent += 1;
        self.tally.billed += billed;
        self.tally.read += usage.cached_tokens.unwrap_or(0);
        self.sent += 1;
        self.spent += cost;
        self.last_cost = cost;
    }

    /// Its hours stopped it until a request starts another idle stretch,
    /// for the cache last used at `used`.
    fn pause(&mut self, used: Option<i64>) {
        self.paused = used.map(|used| self.stretch_start(used));
        self.tally = Tally::default();
    }

    /// The status, for the cache last used at `used` that lasts `lifetime`,
    /// while a turn's requests run (`turn`) or not.
    pub(crate) fn status(
        &self,
        used: Option<i64>,
        lifetime: Option<Duration>,
        turn: bool,
    ) -> Status {
        Status {
            ends_at: self.ends_at(used),
            left: self.left(),
            next_at: used
                .zip(lifetime)
                .filter(|_| !turn)
                .and_then(|(used, lifetime)| self.next_at(used, lifetime)),
        }
    }

    /// Whether a refresh that cost nothing at API prices shows that Arko has
    /// no price for the model, where it has dollars to count.
    pub(crate) fn unpriced(&self, usage: &arko_ai::UsageInfo) -> bool {
        self.limits.dollars.is_some()
            && !self.unpriced_told
            && usage.total_cost <= 0.0
            && usage.billed() <= 0.0
    }

    /// What its refreshes sent, in a sentence: "It sent 8 refreshes for
    /// $0.32." On a plan (`on_plan`), what they read of it.
    fn tally_sentence(&self, on_plan: Option<&str>) -> String {
        let Tally { sent, billed, read } = self.tally;
        let count = match sent {
            0 => return "It sent no refreshes.".into(),
            1 => "1 refresh".to_string(),
            sent => format!("{sent} refreshes"),
        };
        match on_plan.filter(|_| billed < 0.005) {
            Some(plan) => format!(
                "It sent {count}, which read {} cached tokens of {plan}.",
                token_count(read)
            ),
            None => format!("It sent {count} for {}.", dollars(billed)),
        }
    }

    /// The notice of `stop`, with what it sent, on the plan `on_plan` if
    /// one pays.
    fn stopped(&self, stop: &Stop, on_plan: Option<&str>) -> String {
        let tally = self.tally_sentence(on_plan);
        match stop {
            Stop::Ended(at) => format!(
                "Keep-warm ended at {}. {tally}",
                cache_timer::time_of_day(*at)
            ),
            Stop::Refreshes(count) => match on_plan.filter(|_| self.tally.billed < 0.005) {
                Some(plan) => format!(
                    "Keep-warm ended after its {}, which read {} cached tokens of {plan}.",
                    refreshes_words(*count),
                    token_count(self.tally.read)
                ),
                None => format!(
                    "Keep-warm ended after its {} for {}.",
                    refreshes_words(*count),
                    dollars(self.tally.billed)
                ),
            },
            Stop::Idle(hours) => format!(
                "Keep-warm stopped {} after your last message, and starts again after your next one. {tally}",
                hours_words(*hours)
            ),
            Stop::Dollars(most) => {
                format!("Keep-warm stopped at its {} limit. {tally}", dollars(*most))
            }
            Stop::Lapsed => format!(
                "Keep-warm stopped because the prompt cache expired before its refresh, as happens when the computer sleeps. {tally}"
            ),
            Stop::Failed(why) => {
                let why = crate::ui::sentence(why.trim_end_matches('.'));
                format!("Keep-warm stopped. {why}. {tally}")
            }
            Stop::Unsupported => format!(
                "Keep-warm stopped because this model's prompt cache can't be refreshed without a message. {tally}"
            ),
        }
    }
}

/// "15 hours", "1 hour".
fn hours_words(hours: u32) -> String {
    if hours == 1 {
        "1 hour".into()
    } else {
        format!("{hours} hours")
    }
}

/// "10 refreshes", "1 refresh".
fn refreshes_words(count: u32) -> String {
    if count == 1 {
        "1 refresh".into()
    } else {
        format!("{count} refreshes")
    }
}

/// Dollars as a command takes them: "5", "2.5".
fn plain_dollars(amount: f64) -> String {
    let text = format!("{amount:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The time now (Unix milliseconds), which keep-warm counts from.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl crate::Harness {
    /// The setting, as the session opens: on, the session keeps its cache
    /// warm by it. A session that sends one message (-p) doesn't.
    pub(crate) fn keep_warm_at_start(&mut self) {
        let setting = load(&self.root);
        *lock(&self.gauges.keep_warm) = setting;
        if let Some(limits) = setting.filter(|_| !self.one_shot) {
            lock(&self.stats).keep_warm = Some(KeepWarm::new(limits, true));
        }
    }

    /// The model of the conversation the next message goes to (which the
    /// status has as [`conversation_model`]).
    fn keep_warm_model(&self) -> Option<arko_ai::Model> {
        crate::providers::resolve_model(&self.conversation_settings().model).ok()
    }

    /// The cache keep-warm refreshes, when it was last used and how long it
    /// lasts, while there is a conversation whose model refreshes it out of
    /// band. Otherwise none, and keep-warm waits for its end.
    fn keep_warm_cache(&self) -> (Option<i64>, Option<Duration>) {
        let refreshes = !matches!(self.conversation, crate::Conversation::New)
            && self
                .keep_warm_model()
                .is_some_and(|model| refreshes_out_of_band(&model));
        let stats = lock(&self.stats);
        if refreshes {
            (stats.cache_refreshed_at, stats.cache_lifetime)
        } else {
            (None, None)
        }
    }

    /// When the session's loop, waiting for the user, wakes for keep-warm
    /// (Unix milliseconds): for its next refresh, or to stop.
    pub(crate) fn keep_warm_wake(&self) -> Option<i64> {
        let (used, lifetime) = self.keep_warm_cache();
        lock(&self.stats)
            .keep_warm
            .as_ref()?
            .wake_at(used, lifetime)
    }

    /// What keep-warm does at `now`, while the session waits for the user: a
    /// refresh, a stop and its notice, or nothing yet.
    pub(crate) async fn keep_warm_tick(&mut self, now: i64) {
        let (used, lifetime) = self.keep_warm_cache();
        let Some(step) = lock(&self.stats)
            .keep_warm
            .as_ref()
            .map(|keep_warm| keep_warm.step(now, used, lifetime))
        else {
            return;
        };
        match step {
            Step::Wait => {}
            Step::Stop(stop) => self.stop_keep_warm(stop),
            Step::Refresh => match Box::pin(self.refresh_out_of_band()).await {
                Ok(Some(usage)) => {
                    let unpriced = {
                        let mut stats = lock(&self.stats);
                        let refreshed = stats.cache_refreshed_at;
                        stats.keep_warm.as_mut().and_then(|keep_warm| {
                            keep_warm.sent(used.unwrap_or(now), refreshed, &usage);
                            let unpriced = keep_warm.unpriced(&usage);
                            keep_warm.unpriced_told |= unpriced;
                            unpriced.then_some(keep_warm.limits.dollars).flatten()
                        })
                    };
                    if let Some(most) = unpriced {
                        self.say(
                            Level::Warn,
                            &format!(
                                "Arko has no price for this model's refreshes, so keep-warm's {} limit doesn't apply.",
                                dollars(most)
                            ),
                        );
                    }
                }
                Ok(None) => self.stop_keep_warm(Stop::Unsupported),
                Err(error) => self.stop_keep_warm(Stop::Failed(error)),
            },
        }
        self.gauges.touch();
    }

    /// Ends keep-warm (or stops it until the next request, for its hours)
    /// and says why, with what it sent.
    fn stop_keep_warm(&mut self, stop: Stop) {
        let on_plan = self.keep_warm_plan();
        let text = {
            let mut stats = lock(&self.stats);
            let used = stats.cache_refreshed_at;
            let Some(keep_warm) = stats.keep_warm.as_mut() else {
                return;
            };
            let text = keep_warm.stopped(&stop, on_plan);
            if matches!(stop, Stop::Idle(_)) {
                keep_warm.pause(used);
            } else {
                stats.keep_warm = None;
            }
            text
        };
        let level = match stop {
            Stop::Lapsed | Stop::Failed(_) | Stop::Unsupported => Level::Warn,
            _ => Level::Info,
        };
        self.say(level, &text);
        self.gauges.touch();
    }

    /// The plan that pays for the main agent's refreshes, as the status
    /// names it ("your Claude plan"), if one does.
    fn keep_warm_plan(&self) -> Option<&'static str> {
        crate::status::cache_status(&lock(&self.stats), &lock(&self.gauges.models), false).on_plan
    }

    /// `/keep-warm`, typed at the prompt or sent from the web page.
    pub(crate) fn keep_warm_command(&mut self, argument: &str, source: Source) {
        let before = (
            lock(&self.stats).keep_warm.as_ref().map(|kept| kept.limits),
            load(&self.root),
        );
        let result = match parse(argument, Local::now()) {
            Err(error) => Err(error),
            Ok(Words::Show) => Ok(self.keep_warm_described()),
            Ok(Words::Off) => Ok(self.keep_warm_off()),
            Ok(Words::On(limits)) => self.keep_warm_on(limits),
            Ok(Words::Setting(setting)) => self.keep_warm_setting(setting),
        };
        let after = (
            lock(&self.stats).keep_warm.as_ref().map(|kept| kept.limits),
            load(&self.root),
        );
        self.setting_reply(result, source, before != after);
    }

    /// An app's [`Request::KeepWarm`](crate::Request::KeepWarm): on with
    /// `limits`, or off.
    pub(crate) fn keep_warm_request(&mut self, limits: Option<Limits>) -> Result<(), String> {
        match limits {
            Some(limits) => self.keep_warm_on(limits).map(|_| ()),
            None => {
                self.keep_warm_off();
                Ok(())
            }
        }
    }

    /// Turns keep-warm on for the session, until `limits` end it, in place
    /// of the setting's. It's refused where the model can't refresh its cache
    /// out of band, and while the session waits with a cache that lapsed.
    pub(crate) fn keep_warm_on(&mut self, limits: Limits) -> Result<String, String> {
        let model = self.keep_warm_model();
        if let Some(model) = model.as_ref().filter(|model| !refreshes_out_of_band(model)) {
            return Err(format!(
                "{} can't refresh its prompt cache without adding a message, so it can't be kept warm.",
                crate::models::display_name(&crate::providers::model_name(model))
            ));
        }
        if limits.ends_at.is_none() && limits.refreshes.is_none() && limits.idle_hours.is_none() {
            return Err(format!("Give a time or a number of refreshes. {USAGE}"));
        }
        let lapsed = {
            let stats = lock(&self.stats);
            stats
                .cache_refreshed_at
                .zip(stats.cache_lifetime)
                .is_some_and(|(used, lifetime)| now_ms() >= cache_timer::lapses_at(used, lifetime))
        };
        if lapsed && !self.feed.activity().working {
            return Err(
                "The prompt cache has expired, so there's nothing to keep warm. Your next message caches the conversation again."
                    .into(),
            );
        }
        lock(&self.stats).keep_warm = Some(KeepWarm::new(limits, false));
        self.keep_warm_chosen = true;
        self.gauges.touch();
        Ok(self.keep_warm_described())
    }

    /// Turns keep-warm off for the session, also where the setting has it
    /// on, and says what it sent.
    fn keep_warm_off(&mut self) -> String {
        self.keep_warm_chosen = true;
        let on_plan = self.keep_warm_plan();
        let ended = lock(&self.stats).keep_warm.take();
        self.gauges.touch();
        match ended {
            Some(ended) => format!("Keep-warm is off. {}", ended.tally_sentence(on_plan)),
            None => "Keep-warm is off.".into(),
        }
    }

    /// Sets the setting for every session, and for this one unless
    /// `/keep-warm` chose for it.
    fn keep_warm_setting(&mut self, setting: Option<Limits>) -> Result<String, String> {
        save(&self.root, setting)?;
        *lock(&self.gauges.keep_warm) = setting;
        let mut text = setting_sentence(setting);
        if self.keep_warm_chosen {
            text.push_str(" This session keeps what /keep-warm set.");
        } else if !self.one_shot {
            lock(&self.stats).keep_warm = setting.map(|limits| KeepWarm::new(limits, true));
        }
        self.gauges.touch();
        Ok(text)
    }

    /// What keep-warm does in this session, and the setting.
    fn keep_warm_described(&self) -> String {
        let (kept, used) = {
            let stats = lock(&self.stats);
            (stats.keep_warm.clone(), stats.cache_refreshed_at)
        };
        let Some(kept) = kept else {
            return format!(
                "Keep-warm is off in this session. {} {USAGE}",
                setting_sentence(load(&self.root))
            );
        };
        let mut limits = Vec::new();
        match (kept.ends_at(used), kept.limits.idle_hours) {
            (Some(end), _) => limits.push(format!("until {}", cache_timer::time_of_day(end))),
            (None, Some(hours)) => limits.push(format!(
                "for up to {} after your last message",
                hours_words(hours)
            )),
            (None, None) => {}
        }
        if let Some(left) = kept.left() {
            limits.push(format!("for {}", refreshes_words(left)));
        }
        if let Some(most) = kept.limits.dollars {
            limits.push(format!("up to {}", dollars(most)));
        }
        let mut text = format!("Keep-warm is on {}", limits.join(" or "));
        if limits.len() > 1 {
            text.push_str(", whichever comes first");
        }
        text.push('.');
        let cache =
            crate::status::cache_status(&lock(&self.stats), &lock(&self.gauges.models), false);
        if let Some(each) = cache_timer::refresh_each(&cache) {
            text.push_str(&format!(" {each}"));
        }
        if kept.from_setting {
            text.push_str(" The setting turned it on, and /keep-warm off turns it off here.");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone as _;

    use super::*;
    use crate::harness::Prompt;
    use crate::mock_model::ModelServer;
    use crate::remote::Kind;

    const MINUTE: i64 = 60_000;
    const FIVE: Duration = Duration::from_secs(300);
    const HOUR: Duration = Duration::from_secs(3_600);

    fn on(limits: Limits) -> KeepWarm {
        KeepWarm::new(limits, false)
    }

    fn limits(ends_at: Option<i64>, refreshes: Option<u32>) -> Limits {
        Limits {
            ends_at,
            refreshes,
            ..Limits::default()
        }
    }

    fn usage(billed: f64) -> arko_ai::UsageInfo {
        arko_ai::UsageInfo {
            cached_tokens: Some(180_000),
            total_cost: billed,
            billed_cost: Some(billed),
            ..arko_ai::UsageInfo::default()
        }
    }

    /// Sends the refresh due at `now`, which leaves the cache last used then.
    fn refresh(kept: &mut KeepWarm, used: &mut i64, now: i64) {
        kept.sent(*used, Some(now), &usage(0.04));
        *used = now;
    }

    /// A five-minute cache refreshes at 4:00 and an hour's at 55:00, and
    /// each refresh moves the next one.
    #[test]
    fn a_cache_is_refreshed_a_lead_before_it_lapses() {
        let kept = on(limits(Some(10 * 60 * MINUTE), None));
        assert_eq!(kept.wake_at(Some(0), Some(FIVE)), Some(4 * MINUTE));
        assert_eq!(kept.step(4 * MINUTE - 1, Some(0), Some(FIVE)), Step::Wait);
        assert_eq!(kept.step(4 * MINUTE, Some(0), Some(FIVE)), Step::Refresh);
        assert_eq!(kept.wake_at(Some(0), Some(HOUR)), Some(55 * MINUTE));
        assert_eq!(kept.step(55 * MINUTE, Some(0), Some(HOUR)), Step::Refresh);
        let mut kept = kept;
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        assert_eq!(kept.wake_at(Some(used), Some(FIVE)), Some(8 * MINUTE));
        assert_eq!(
            kept.status(Some(used), Some(FIVE), false).next_at,
            Some(8 * MINUTE)
        );
        // None goes while a turn runs, and a cache whose time isn't known
        // waits for a request.
        assert_eq!(kept.status(Some(used), Some(FIVE), true).next_at, None);
        assert_eq!(kept.step(9 * MINUTE, None, Some(FIVE)), Step::Wait);
    }

    /// A turn's request moves the schedule, and isn't counted.
    #[test]
    fn a_turn_moves_the_next_refresh() {
        let mut kept = on(limits(None, Some(3)));
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        // The user's message at 6:00 left the cache last used then.
        let turn = 6 * MINUTE;
        assert_eq!(kept.wake_at(Some(turn), Some(FIVE)), Some(10 * MINUTE));
        assert_eq!(kept.step(8 * MINUTE, Some(turn), Some(FIVE)), Step::Wait);
        assert_eq!(kept.status(Some(turn), Some(FIVE), false).left, Some(2));
    }

    /// The end stops it, and a refresh that would come after it isn't sent.
    /// So does the count, right after its last refresh.
    #[test]
    fn the_end_and_the_count_stop_it() {
        let end = 6 * MINUTE;
        let kept = on(limits(Some(end), None));
        // The refresh at 4:00 goes. The next, at 8:00, would be after the
        // end, so the loop wakes at 6:00 to stop.
        assert_eq!(kept.step(4 * MINUTE, Some(0), Some(FIVE)), Step::Refresh);
        let mut kept = kept;
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        assert_eq!(kept.status(Some(used), Some(FIVE), false).next_at, None);
        assert_eq!(kept.wake_at(Some(used), Some(FIVE)), Some(end));
        assert_eq!(
            kept.step(end, Some(used), Some(FIVE)),
            Step::Stop(Stop::Ended(end))
        );

        let mut kept = on(limits(None, Some(2)));
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        assert_eq!(kept.status(Some(used), Some(FIVE), false).left, Some(1));
        refresh(&mut kept, &mut used, 8 * MINUTE);
        assert_eq!(kept.wake_at(Some(used), Some(FIVE)), Some(i64::MIN));
        assert_eq!(
            kept.step(8 * MINUTE, Some(used), Some(FIVE)),
            Step::Stop(Stop::Refreshes(2))
        );
        assert_eq!(
            kept.stopped(&Stop::Refreshes(2), None),
            "Keep-warm ended after its 2 refreshes for $0.08."
        );
    }

    /// The dollars stop it before a refresh would pass them, and a cache
    /// that lapsed (the computer slept) stops it too.
    #[test]
    fn dollars_and_a_lapsed_cache_stop_it() {
        let mut kept = on(Limits {
            refreshes: Some(100),
            dollars: Some(0.1),
            ..Limits::default()
        });
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        refresh(&mut kept, &mut used, 8 * MINUTE);
        assert_eq!(kept.wake_at(Some(used), Some(FIVE)), Some(i64::MIN));
        assert_eq!(
            kept.step(9 * MINUTE, Some(used), Some(FIVE)),
            Step::Stop(Stop::Dollars(0.1))
        );
        assert_eq!(
            kept.stopped(&Stop::Dollars(0.1), None),
            "Keep-warm stopped at its $0.10 limit. It sent 2 refreshes for $0.08."
        );
        let kept = on(limits(None, Some(5)));
        assert_eq!(
            kept.step(4 * MINUTE + 30_000, Some(0), Some(FIVE)),
            Step::Stop(Stop::Lapsed)
        );
    }

    /// The setting's hours count from the latest request it didn't send,
    /// stop it until the next, and start again with it.
    #[test]
    fn the_settings_hours_count_from_the_latest_request() {
        let mut kept = KeepWarm::new(
            Limits {
                idle_hours: Some(1),
                ..Limits::default()
            },
            true,
        );
        // Refreshes at 4:00, 8:00 and on to 56:00. The next would be at
        // 60:00, the end of the hour, where it stops instead.
        let mut used = 0;
        let mut sent = 0;
        while let Some(at) = kept.wake_at(Some(used), Some(FIVE)) {
            match kept.step(at, Some(used), Some(FIVE)) {
                Step::Refresh => {
                    refresh(&mut kept, &mut used, at);
                    sent += 1;
                }
                stop => {
                    assert_eq!((at, stop), (60 * MINUTE, Step::Stop(Stop::Idle(1))));
                    break;
                }
            }
        }
        assert_eq!((sent, used), (14, 56 * MINUTE));
        assert_eq!(
            kept.stopped(&Stop::Idle(1), Some("your Claude plan")),
            "Keep-warm stopped 1 hour after your last message, and starts again after your next one. It sent 14 refreshes for $0.56."
        );
        kept.pause(Some(used));
        assert_eq!(kept.wake_at(Some(used), Some(FIVE)), None);
        assert_eq!(kept.step(70 * MINUTE, Some(used), Some(FIVE)), Step::Wait);
        // The user's next message, at 90:00, starts another hour.
        let turn = 90 * MINUTE;
        assert_eq!(kept.wake_at(Some(turn), Some(FIVE)), Some(94 * MINUTE));
        assert_eq!(
            kept.status(Some(turn), Some(FIVE), false).ends_at,
            Some(150 * MINUTE)
        );
    }

    /// On a plan, the notice counts the tokens its refreshes read.
    #[test]
    fn on_a_plan_the_notice_counts_tokens() {
        let mut kept = on(limits(Some(60 * MINUTE), None));
        kept.sent(0, Some(4 * MINUTE), &usage(0.0));
        assert_eq!(
            kept.tally_sentence(Some("your Claude plan")),
            "It sent 1 refresh, which read 180k cached tokens of your Claude plan."
        );
        assert_eq!(
            kept.stopped(
                &Stop::Failed("the cache wasn't refreshed (the server said no)".into()),
                None
            ),
            "Keep-warm stopped. The cache wasn't refreshed (the server said no). It sent 1 refresh for $0.00."
        );
    }

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    /// The command's words: a time from now, a time of day to come, a
    /// number of refreshes, dollars, off, and the setting's hours and
    /// dollars.
    #[test]
    fn the_command_reads_times_counts_and_dollars() {
        let now = at(23, 0);
        let ms = now.timestamp_millis();
        let on = |words: &str| match parse(words, now) {
            Ok(Words::On(limits)) => limits,
            other => panic!("{words}: {other:?}"),
        };
        assert_eq!(parse("", now), Ok(Words::Show));
        assert_eq!(parse("off", now), Ok(Words::Off));
        assert_eq!(on("8h").ends_at, Some(ms + 8 * 60 * MINUTE));
        assert_eq!(on("90m").ends_at, Some(ms + 90 * MINUTE));
        let morning = Local
            .with_ymd_and_hms(2026, 10, 31, 7, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        for words in [
            "until 7:00",
            "until 7",
            "until 07:00",
            "until 7am",
            "until 7 am",
        ] {
            assert_eq!(on(words).ends_at, Some(morning), "{words}");
        }
        assert_eq!(
            on("until 23:30").ends_at,
            Some(at(23, 30).timestamp_millis())
        );
        assert_eq!(
            on("until 11:30pm").ends_at,
            Some(at(23, 30).timestamp_millis())
        );
        assert_eq!(on("10").refreshes, Some(10));
        let both = on("8h $2.50");
        assert_eq!(
            (both.ends_at, both.dollars),
            (Some(ms + 480 * MINUTE), Some(2.5))
        );
        assert_eq!(
            parse("default 15h $5", now),
            Ok(Words::Setting(Some(Limits {
                idle_hours: Some(15),
                dollars: Some(5.0),
                ..Limits::default()
            })))
        );
        assert_eq!(parse("default off", now), Ok(Words::Setting(None)));
        for wrong in [
            "soon",
            "25h",
            "$0",
            "8h until 7:00",
            "$5",
            "until 25:00",
            "0",
            "8h 9h",
        ] {
            assert!(parse(wrong, now).is_err(), "{wrong}");
        }
        assert!(
            parse("default $5", now)
                .unwrap_err()
                .contains("whole hours")
        );
        assert!(parse("default 10", now).is_err());
    }

    /// The setting is kept in Arko's folder, its command and sentence read
    /// it back, and a file that holds nothing usable is off.
    #[test]
    fn the_setting_is_kept_and_read_back() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(load(temp.path()), None);
        let setting = Limits {
            idle_hours: Some(15),
            dollars: Some(5.0),
            ..Limits::default()
        };
        save(temp.path(), Some(setting)).unwrap();
        assert_eq!(load(temp.path()), Some(setting));
        assert_eq!(setting_command(Some(setting)), "/keep-warm default 15h $5");
        assert_eq!(
            parse(
                setting_command(Some(setting)).trim_start_matches("/keep-warm "),
                Local::now()
            ),
            Ok(Words::Setting(Some(setting)))
        );
        assert_eq!(
            setting_sentence(Some(setting)),
            "Every session keeps its prompt cache warm while it waits for you, for up to 15 hours after the last message or $5.00."
        );
        save(temp.path(), None).unwrap();
        assert_eq!(load(temp.path()), None);
        assert_eq!(setting_command(None), "/keep-warm default off");
        std::fs::write(temp.path().join(FILE), r#"{"dollars": 5}"#).unwrap();
        assert_eq!(load(temp.path()), None, "hours are needed");
    }

    /// A model that replies to each turn, and to each refresh (a request that
    /// isn't streamed) with a reply that read 900 cached tokens, or refuses
    /// it (`refuses`). It keeps the bodies of the requests.
    async fn model(bodies: Arc<Mutex<Vec<String>>>, refuses: bool) -> ModelServer {
        ModelServer::start(Arc::new(move |_, body: String| {
            let streamed = body.contains("\"stream\":true");
            bodies.lock().unwrap().push(body);
            Box::pin(async move {
                if streamed {
                    crate::mock_model::complete("a", "Noted.")
                } else if refuses {
                    crate::mock_model::overflow()
                } else {
                    crate::mock_model::whole_reply("k", "Noted.", 900)
                }
            })
        }))
        .await
    }

    /// What the feed says, its outputs and notices, newest last.
    fn said(feed: &crate::remote::Feed) -> Vec<String> {
        feed.subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| matches!(item.kind, Kind::Output | Kind::Notice))
            .map(|item| item.text)
            .collect()
    }

    /// A session that answered one message, its cache lasting 61 seconds, so
    /// keep-warm refreshes it a second after each use (its lead is a minute).
    async fn answered(
        temp: &Path,
        server: &ModelServer,
    ) -> (
        crate::Harness,
        crate::inputs::Inputs,
        tokio::sync::mpsc::UnboundedSender<crate::Input>,
    ) {
        let (mut harness, mut inputs, sender) = crate::tests::harness(temp, server.client(), true);
        harness.turn(Prompt::User("Hi.".into()), &mut inputs).await;
        harness.publish_models();
        lock(&harness.stats).cache_lifetime = Some(Duration::from_secs(61));
        (harness, inputs, sender)
    }

    /// While the session waits for the user, its loop refreshes the cache out
    /// of band before it lapses, each refresh moving the next, and the
    /// count ends it with a notice of what it sent.
    #[tokio::test]
    async fn the_sessions_loop_refreshes_until_the_count_ends_it() {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = model(bodies.clone(), false).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = answered(temp.path(), &server).await;
        assert!(harness.live_command("/keep-warm 2", Source::Terminal));
        assert!(
            said(&harness.feed)
                .last()
                .unwrap()
                .starts_with("Keep-warm is on for 2 refreshes."),
            "{:?}",
            said(&harness.feed)
        );
        let status = crate::remote::Meter::read(harness.gauges.as_ref()).cache;
        assert_eq!(status.keep_warm.unwrap().left, Some(2));
        let feed = harness.feed.clone();
        let quits = async {
            tokio::time::timeout(Duration::from_secs(30), async {
                while !said(&feed)
                    .iter()
                    .any(|text| text.starts_with("Keep-warm ended"))
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("keep-warm ended");
            sender.send(crate::Input::Quit(Some(Vec::new()))).unwrap();
        };
        tokio::time::timeout(
            Duration::from_secs(60),
            futures_util::future::join(harness.repl(&mut inputs), quits),
        )
        .await
        .unwrap();
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3, "the turn and two refreshes");
        assert!(
            bodies[1..]
                .iter()
                .all(|body| !body.contains("\"stream\":true"))
        );
        let ended = said(&harness.feed)
            .into_iter()
            .find(|text| text.starts_with("Keep-warm ended"))
            .unwrap();
        assert!(
            ended.starts_with("Keep-warm ended after its 2 refreshes for "),
            "{ended}"
        );
        assert_eq!(lock(&harness.stats).keep_warm, None);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// A refresh that fails ends keep-warm with a notice of why, and sends
    /// no other.
    #[tokio::test]
    async fn a_failed_refresh_ends_it() {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = model(bodies.clone(), true).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = answered(temp.path(), &server).await;
        assert!(harness.live_command("/keep-warm 8h", Source::Terminal));
        let at = harness.keep_warm_wake().unwrap();
        let used = lock(&harness.stats).cache_refreshed_at.unwrap();
        assert_eq!(at, used + 1_000);
        harness.keep_warm_tick(at).await;
        assert_eq!(bodies.lock().unwrap().len(), 2);
        let stopped = said(&harness.feed).pop().unwrap();
        assert!(
            stopped.starts_with("Keep-warm stopped. The cache wasn't refreshed (")
                && stopped.ends_with("It sent no refreshes."),
            "{stopped}"
        );
        assert_eq!(harness.keep_warm_wake(), None);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// It's refused where the model can't refresh its cache out of band,
    /// and where the cache lapsed while the session waits.
    #[tokio::test]
    async fn it_is_refused_where_it_would_add_messages_or_the_cache_lapsed() {
        let server = model(Arc::default(), false).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) =
            crate::tests::harness(temp.path(), server.client(), true);
        let luna = harness.settings.main.model.clone();
        harness.settings.main.model = "openrouter:anthropic/claude-opus-5.5".into();
        let refused = harness
            .keep_warm_request(Some(limits(None, Some(3))))
            .unwrap_err();
        assert!(
            refused.ends_with(
                "can't refresh its prompt cache without adding a message, so it can't be kept warm."
            ),
            "{refused}"
        );
        harness.settings.main.model = luna;
        {
            let mut stats = lock(&harness.stats);
            stats.cache_lifetime = Some(FIVE);
            stats.cache_refreshed_at = Some(now_ms() - 10 * MINUTE);
        }
        assert!(
            harness
                .keep_warm_request(Some(limits(None, Some(3))))
                .unwrap_err()
                .starts_with("The prompt cache has expired")
        );
        assert_eq!(lock(&harness.stats).keep_warm, None);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// The setting is kept for every session and turns keep-warm on as one
    /// opens. `/keep-warm` in a session goes in its place there, also when
    /// it turns it off, and a change of the setting then leaves it.
    #[tokio::test]
    async fn the_setting_keeps_sessions_warm_unless_one_chose() {
        let server = model(Arc::default(), false).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) =
            crate::tests::harness(temp.path(), server.client(), true);
        assert!(harness.live_command("/keep-warm default 15h $5", Source::Web));
        let setting = load(&harness.root).unwrap();
        assert_eq!((setting.idle_hours, setting.dollars), (Some(15), Some(5.0)));
        assert!(
            said(&harness.feed).last().unwrap().starts_with(
                "Changed from the web page. Every session keeps its prompt cache warm while it waits for you, for up to 15 hours after the last message or $5.00."
            ),
            "{:?}",
            said(&harness.feed)
        );
        assert!(
            lock(&harness.stats)
                .keep_warm
                .as_ref()
                .unwrap()
                .from_setting
        );
        let reading = crate::remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.settings.keep_warm, Some(setting));
        // A session that opens takes it.
        lock(&harness.stats).keep_warm = None;
        harness.keep_warm_at_start();
        assert_eq!(
            lock(&harness.stats)
                .keep_warm
                .as_ref()
                .map(|kept| kept.limits),
            Some(setting)
        );
        assert!(harness.live_command("/keep-warm off", Source::Terminal));
        assert_eq!(
            said(&harness.feed).last().unwrap(),
            "Keep-warm is off. It sent no refreshes."
        );
        assert!(harness.live_command("/keep-warm default 8h", Source::Terminal));
        assert!(
            said(&harness.feed)
                .last()
                .unwrap()
                .ends_with("This session keeps what /keep-warm set."),
        );
        assert_eq!(lock(&harness.stats).keep_warm, None);
        assert!(harness.live_command("/keep-warm default off", Source::Terminal));
        assert_eq!(load(&harness.root), None);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    #[test]
    fn the_name_and_the_command_are_public() {
        assert_eq!(crate::keep_warm::NAME, "Keep warm");
        assert!(crate::commands::find("/keep-warm 8h").is_some());
        assert!(crate::commands::find(COMMAND).is_some());
        // The web page's settings row calls it by the same name.
        let page: &[u8] = include_bytes!("remote/page/arko-web_bg.wasm");
        assert!(
            page.windows(NAME.len())
                .any(|bytes| bytes == NAME.as_bytes())
        );
    }
}
