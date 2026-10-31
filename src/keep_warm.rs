//! Keep-warm. While the session waits for the user's next message, the
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
//!
//! A session keeps what it chose and what its refreshes sent in its folder,
//! so it goes on with them when it opens again. Keep-warm times the cache on
//! the session's monotonic clock and on the wall clock both, and a time has
//! passed once either says so ([`Moment`]).

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone};
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
    /// prices. On a key, where the framework caps a refresh's output, that's
    /// a ceiling. On a plan it counts what the refreshes would cost on a key,
    /// estimated, since nothing caps their output. Where Arko has no price
    /// for the model, it doesn't apply, and Arko says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollars: Option<f64>,
}

/// Keep-warm in the status, while it is on and the model can refresh its
/// cache out of band.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Status {
    /// When it stops (Unix milliseconds), meaning its end, or its hours after the
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
    /// It waits for the user's next message before it refreshes, because the
    /// session can't tell how long ago the cache was used (Arko opened
    /// again, the computer slept or its clock changed).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub waits: bool,
}

/// Whether the main agent's prompt cache on `model`, as Arko configures it,
/// refreshes out of band, meaning Anthropic's models, `cc:` models and OpenAI's.
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

/// What a refresh of the conversation's cache goes as, which keep-warm's
/// quote and the refresh both take from the conversation's own settings
/// ([`Harness::refresh_route`](crate::Harness::refresh_route)).
#[derive(Debug, Clone)]
pub(crate) struct Route {
    /// The conversation's model.
    pub(crate) model: arko_ai::Model,
    /// The conversation runs it in fast mode.
    pub(crate) fast: bool,
    /// A plan pays (a `cc:` model, the ChatGPT login), where nothing caps a
    /// refresh's output, so its price is an estimate.
    pub(crate) plan: bool,
}

/// The setting, which keeps every session's cache warm while it waits for
/// the user, meaning its hours and dollars, or `None` (off, the default). A missing
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
            most.map(|most| format!(
                " after the last message or {} (estimated on a plan)",
                dollars(most)
            ))
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
/// one to come in `now`'s time zone, also across a change of the clocks), a
/// number is of refreshes, and `$5` a dollar limit. The
/// setting (`default`) takes hours, and dollars after them.
pub(crate) fn parse<Tz: TimeZone>(argument: &str, now: DateTime<Tz>) -> Result<Words, String> {
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
            let at = next_time(&time, &now)?;
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

/// The next time of day `text` names after `now` (Unix milliseconds), in
/// `now`'s time zone, such as `7`, `7:00`, `07:30`, `19:30`, `7am`, `7:30pm`.
fn next_time<Tz: TimeZone>(text: &str, now: &DateTime<Tz>) -> Result<i64, String> {
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
        let time = day.and_hms_opt(hour, minute, 0).ok_or_else(invalid)?;
        if let Some(at) = first_after(&now.timezone(), time, now.timestamp_millis()) {
            return Ok(at);
        }
        day = day.succ_opt().ok_or_else(invalid)?;
    }
    Err(invalid())
}

/// The first moment after `after` (Unix milliseconds) that the local time
/// `time` names in `zone`. A time the clocks go through twice as they go
/// back is its first that comes after `after`. A time they skip as they go
/// forward is the first moment after the gap.
fn first_after<Tz: TimeZone>(zone: &Tz, time: NaiveDateTime, after: i64) -> Option<i64> {
    let mut time = time;
    // (A gap is an hour or two, past it within a day.)
    for _ in 0..24 * 60 {
        match zone.from_local_datetime(&time) {
            LocalResult::Single(at) => {
                return Some(at.timestamp_millis()).filter(|at| *at > after);
            }
            LocalResult::Ambiguous(first, second) => {
                return [first, second]
                    .into_iter()
                    .map(|at| at.timestamp_millis())
                    .find(|at| *at > after);
            }
            LocalResult::None => time += chrono::TimeDelta::minutes(1),
        }
    }
    None
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
    /// The cache lapsed before its refresh.
    Lapsed,
    /// The two clocks disagree on how long ago the cache was used (the
    /// computer slept, or its clock changed). It starts again with the next
    /// request.
    Clock,
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

/// What keep-warm's refreshes sent, meaning how many, what they cost at API prices
/// (what a plan's would cost on a key), what they billed, and the tokens
/// they read from the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Totals {
    sent: u32,
    spent: f64,
    billed: f64,
    read: usize,
}

impl Totals {
    /// What came since `earlier`.
    fn since(self, earlier: Totals) -> Totals {
        Totals {
            sent: self.sent.saturating_sub(earlier.sent),
            spent: (self.spent - earlier.spent).max(0.0),
            billed: (self.billed - earlier.billed).max(0.0),
            read: self.read.saturating_sub(earlier.read),
        }
    }
}

/// What a session chose in place of the setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Choice {
    /// It follows the setting.
    #[default]
    Setting,
    /// Off by `/keep-warm off`, an app's request, or an end.
    Off,
    /// On until these limits end it (`/keep-warm`, an app's request).
    On(Limits),
}

/// The file in a session's folder that keeps its keep-warm.
const SESSION_FILE: &str = "keep-warm-session.json";

/// Keep-warm in a session, meaning what it chose and what its refreshes sent. A
/// session keeps it in its folder, so it goes on as it was when the session
/// opens again (a resume, a switch between Chat and Agent). When to refresh
/// comes from it, the setting and the cache, each time the session's loop
/// asks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct KeepWarm {
    choice: Choice,
    /// Everything its refreshes sent in the session.
    totals: Totals,
    /// The totals as the session's own choice began, which its number of
    /// refreshes and its dollars count from. Under the setting they count
    /// from the session's start.
    from: Totals,
    /// The totals as it last said what it sent. A notice says what came
    /// since.
    told: Totals,
    /// When the latest request that keep-warm didn't send went (Unix
    /// milliseconds), a turn's or a refresh the user asked for. Its idle
    /// stretch starts there.
    requested_at: Option<i64>,
    /// The idle stretch (its `requested_at`) whose hours ran out, until
    /// another request.
    paused: Option<i64>,
    /// It said that Arko has no price for the refreshes.
    unpriced_told: bool,
    /// The most output any of its refreshes wrote, in tokens, which a plan's
    /// next one is taken to write too.
    most_output: usize,
    /// When the session saw its `requested_at` on its monotonic clock, none
    /// where it didn't see it (it opened since, or its clocks disagreed).
    #[serde(skip)]
    requested_seen: Option<Instant>,
    /// When the session's own end comes on its monotonic clock.
    #[serde(skip)]
    ends_seen: Option<Instant>,
}

impl KeepWarm {
    /// The session's keep-warm, as its folder `dir` keeps it, as the session
    /// opens at `now`. None kept (or unreadable) follows the setting. The
    /// session didn't see the request it kept, so it refreshes nothing until
    /// the next one.
    pub(crate) fn load(dir: &Path, now: Moment) -> Self {
        let mut kept: Self = std::fs::read_to_string(dir.join(SESSION_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if let Choice::On(limits) = kept.choice {
            kept.ends_seen = limits.ends_at.map(|at| seen_at(at, now));
        }
        kept
    }

    /// Keeps it in the session's folder `dir`.
    pub(crate) fn save(&self, dir: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        crate::session::write_whole(&dir.join(SESSION_FILE), text.as_bytes())
            .map_err(|error| error.to_string())
    }

    /// Its limits now, under the setting `setting`, meaning the session's own, else
    /// the setting's. `None` while it is off.
    pub(crate) fn limits(&self, setting: Option<Limits>) -> Option<Limits> {
        match self.choice {
            Choice::Setting => setting,
            Choice::Off => None,
            Choice::On(limits) => Some(limits),
        }
    }

    /// Whether it follows the setting (the session made no choice).
    pub(crate) fn follows_setting(&self) -> bool {
        self.choice == Choice::Setting
    }

    /// A request that keep-warm didn't send went at `at`, either a turn's, or a
    /// refresh the user asked for.
    pub(crate) fn requested(&mut self, at: Moment) {
        self.requested_at = Some(at.at);
        self.requested_seen = Some(at.seen);
    }

    /// The latest request it didn't send, where the session saw it.
    fn request(&self) -> Option<Moment> {
        Some(Moment {
            seen: self.requested_seen?,
            at: self.requested_at?,
        })
    }

    /// On until `limits` end it, in place of the setting, from `now`. Its
    /// number of refreshes and its dollars count from now.
    fn start(&mut self, limits: Limits, now: Moment) {
        self.choice = Choice::On(limits);
        self.ends_seen = limits.ends_at.map(|at| seen_at(at, now));
        self.from = self.totals;
        self.told = self.totals;
        self.paused = None;
    }

    /// Off for the session, and what it sent since it last said.
    fn off(&mut self) -> Totals {
        self.choice = Choice::Off;
        self.report()
    }

    /// What it sent since it last said, which it says now.
    fn report(&mut self) -> Totals {
        let said = self.totals.since(self.told);
        self.told = self.totals;
        said
    }

    /// What its number of refreshes and its dollars count.
    fn counted(&self) -> Totals {
        self.totals.since(self.from)
    }

    /// Its own end under `limits`.
    fn own_end(&self, limits: &Limits) -> Option<Moment> {
        Some(Moment {
            seen: self.ends_seen?,
            at: limits.ends_at?,
        })
    }

    /// When it stops, under `limits`, meaning its own end, or its hours after the
    /// idle stretch's start, whichever comes first.
    fn ends(&self, limits: &Limits) -> Option<Moment> {
        let idle = limits
            .idle_hours
            .zip(self.request())
            .map(|(hours, request)| request.plus(Duration::from_secs(u64::from(hours) * 3_600)));
        match (self.own_end(limits), idle) {
            (Some(own), Some(idle)) => Some(own.earliest(idle)),
            (own, idle) => own.or(idle),
        }
    }

    fn left(&self, limits: &Limits) -> Option<u32> {
        limits
            .refreshes
            .map(|refreshes| refreshes.saturating_sub(self.counted().sent))
    }

    /// Whether its next refresh, which costs `quote` at most (`None` where
    /// Arko has no price for it), would pass its dollars, with what it spent
    /// already.
    fn spent_out(&self, limits: &Limits, quote: Option<f64>) -> bool {
        limits
            .dollars
            .is_some_and(|most| self.counted().spent + quote.unwrap_or_default() > most + 1e-9)
    }

    /// Whether its hours stopped it in this idle stretch, or there is no
    /// request the session saw to count them from.
    fn paused(&self, limits: &Limits) -> bool {
        limits.idle_hours.is_some()
            && (self.request().is_none() || self.paused == self.requested_at)
    }

    /// Whether `at` comes before it stops under `limits`. The status, the
    /// loop's wake and each step all go by this one rule, so a refresh the
    /// status shows is one that goes.
    fn before_end(&self, limits: &Limits, at: Moment) -> bool {
        self.ends(limits).is_none_or(|end| !end.passed(at))
    }

    /// When the next refresh of `cache` goes under `limits` (on the
    /// monotonic clock), if one comes before it stops and the limits leave
    /// one.
    fn next(&self, limits: &Limits, cache: Cache) -> Option<Moment> {
        if self.left(limits) == Some(0)
            || self.spent_out(limits, cache.quote)
            || self.paused(limits)
        {
            return None;
        }
        let due = cache
            .used?
            .plus(cache_timer::refresh_after(cache.lifetime?));
        self.before_end(limits, due).then_some(due)
    }

    /// When the session's loop wakes for it under `limits`, for `cache` at
    /// `now`, on the monotonic clock, for the next refresh, or to stop.
    pub(crate) fn wake_at(&self, limits: &Limits, cache: Cache, now: Moment) -> Option<Instant> {
        if self.left(limits) == Some(0) || self.spent_out(limits, cache.quote) {
            return Some(now.seen);
        }
        if self.paused(limits) {
            return None;
        }
        let next = self.next(limits, cache).map(|due| due.seen);
        let end = self.ends(limits).map(|end| end.deadline(now));
        [next, end].into_iter().flatten().min()
    }

    /// What it does at `now` under `limits`, for `cache`.
    pub(crate) fn step(&self, now: Moment, limits: &Limits, cache: Cache) -> Step {
        if let Some(refreshes) = limits.refreshes.filter(|_| self.left(limits) == Some(0)) {
            return Step::Stop(Stop::Refreshes(refreshes));
        }
        if let Some(most) = limits
            .dollars
            .filter(|_| self.spent_out(limits, cache.quote))
        {
            return Step::Stop(Stop::Dollars(most));
        }
        if self.paused(limits) {
            return Step::Wait;
        }
        if !self.before_end(limits, now) {
            return Step::Stop(match self.own_end(limits) {
                Some(own) if own.passed(now) => Stop::Ended(own.at),
                _ => Stop::Idle(limits.idle_hours.unwrap_or_default()),
            });
        }
        let (Some(due), Some(used), Some(lifetime)) =
            (self.next(limits, cache), cache.used, cache.lifetime)
        else {
            return Step::Wait;
        };
        if !used.agrees(now) {
            Step::Stop(Stop::Clock)
        } else if now.seen < due.seen {
            Step::Wait
        } else if now.seen >= used.seen + cache_timer::lapse_after(lifetime) {
            Step::Stop(Stop::Lapsed)
        } else {
            Step::Refresh
        }
    }

    /// Counts a refresh it sent, which had `usage`.
    pub(crate) fn sent(&mut self, usage: &arko_ai::UsageInfo) {
        let billed = usage.billed();
        self.totals.sent += 1;
        self.totals.spent += usage.total_cost.max(billed);
        self.totals.billed += billed;
        self.totals.read += usage.cached_tokens.unwrap_or(0);
        self.most_output = self.most_output.max(usage.output_tokens);
    }

    /// The most output its refreshes wrote, in tokens.
    pub(crate) fn most_output(&self) -> usize {
        self.most_output
    }

    /// Stops it for `stop`, and says why and what it sent since it last
    /// said, on the plan `on_plan` if one pays. Its hours, and clocks that
    /// disagree, stop it until the next request, and anything else ends it
    /// for the session.
    fn stop(&mut self, stop: &Stop, on_plan: Option<&str>) -> String {
        let said = self.report();
        match stop {
            Stop::Idle(_) => self.paused = self.requested_at,
            Stop::Clock => self.requested_seen = None,
            _ => self.choice = Choice::Off,
        }
        stopped(stop, said, on_plan)
    }

    /// The status under `limits` at `now`, for `cache`, while a turn's
    /// requests run (`turn`) or not. A refresh shows only where the session
    /// can tell the cache's age, and it waits for the next message where a
    /// cache that it can't tell the age of shows (`shown`).
    pub(crate) fn status(
        &self,
        limits: &Limits,
        cache: Cache,
        shown: bool,
        turn: bool,
        now: Moment,
    ) -> Status {
        let told = cache.used.is_some_and(|used| used.agrees(now))
            && (limits.idle_hours.is_none() || self.request().is_some());
        Status {
            ends_at: self.ends(limits).map(|end| end.at),
            left: self.left(limits),
            next_at: (!turn && told)
                .then(|| self.next(limits, cache))
                .flatten()
                .map(|due| due.at),
            waits: !turn && shown && !told,
        }
    }
}

/// When the wall time `at` (Unix milliseconds) comes on the monotonic clock,
/// counted from `now`, or now if it passed.
fn seen_at(at: i64, now: Moment) -> Instant {
    now.seen + Duration::from_millis(u64::try_from(at - now.at).unwrap_or(0))
}

/// What keep-warm decides by, meaning the cache it refreshes, and what a refresh
/// costs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Cache {
    /// When it was last used, as the session saw it, with `None` where it
    /// didn't (none yet, or a time from before the session opened).
    pub(crate) used: Option<Moment>,
    /// How long it lasts, where the provider says.
    pub(crate) lifetime: Option<Duration>,
    /// What a refresh costs at most, at API prices, `None` where Arko has
    /// no price for the model.
    pub(crate) quote: Option<f64>,
}

/// A moment as the session saw it, meaning on its monotonic clock, which no one
/// sets, and on the wall clock the page shows (Unix milliseconds). The
/// refreshes go by the monotonic clock, while the two agree on how long ago
/// the cache was used ([`agrees`](Self::agrees)). An end has passed once
/// either clock says so, since stopping sooner spends nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moment {
    pub(crate) seen: Instant,
    pub(crate) at: i64,
}

impl Moment {
    pub(crate) fn now() -> Self {
        Self {
            seen: Instant::now(),
            at: now_ms(),
        }
    }

    /// The moment of a request sent at `at` (Unix milliseconds), seen at
    /// `now`. Its age on the wall clock counts where it's up to a minute (a
    /// streamed request's time is a few seconds old), and else it's now.
    pub(crate) fn sent(at: i64, now: Moment) -> Self {
        let age = u64::try_from(now.at - at)
            .ok()
            .map(Duration::from_millis)
            .filter(|age| *age <= Duration::from_secs(60))
            .unwrap_or_default();
        Self {
            seen: now.seen.checked_sub(age).unwrap_or(now.seen),
            at,
        }
    }

    /// Whether the two clocks agree, within [`CLOCKS_AGREE`], on how long
    /// ago it was at `now`. They disagree after the computer slept (the
    /// monotonic clock doesn't count the sleep) or its clock changed, and
    /// then no one can tell how long ago it was.
    fn agrees(self, now: Moment) -> bool {
        let seen = i64::try_from(now.seen.saturating_duration_since(self.seen).as_millis())
            .unwrap_or(i64::MAX);
        (now.at - self.at).abs_diff(seen) <= CLOCKS_AGREE.as_millis() as u64
    }

    /// `wait` later, on both clocks.
    fn plus(self, wait: Duration) -> Self {
        Self {
            seen: self.seen + wait,
            at: self
                .at
                .saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
        }
    }

    /// Whether an end at it has passed at `now`, by either clock.
    fn passed(self, now: Moment) -> bool {
        now.seen >= self.seen || now.at >= self.at
    }

    /// When an end at it comes on the monotonic clock, by whichever clock
    /// says sooner at `now`.
    fn deadline(self, now: Moment) -> Instant {
        let wall = Duration::from_millis(u64::try_from(self.at - now.at).unwrap_or(0));
        self.seen.min(now.seen + wall)
    }

    /// The first to pass of two ends, by either clock.
    fn earliest(self, other: Moment) -> Self {
        Self {
            seen: self.seen.min(other.seen),
            at: self.at.min(other.at),
        }
    }
}

/// How far the two clocks may differ on how long ago something was, for
/// [`Moment::agrees`].
const CLOCKS_AGREE: Duration = Duration::from_secs(2);

impl crate::ui::Stats {
    /// When the cache was last used, as the session saw it.
    pub(crate) fn cache_used(&self) -> Option<Moment> {
        Some(Moment {
            seen: self.cache_seen?,
            at: self.cache_refreshed_at?,
        })
    }
}

/// What refreshes sent (`said`), in a sentence, as "It sent 8 refreshes for
/// $0.32." On a plan (`on_plan`), what they read of it.
fn tally_sentence(said: Totals, on_plan: Option<&str>) -> String {
    let count = match said.sent {
        0 => return "It sent no refreshes.".into(),
        1 => "1 refresh".to_string(),
        sent => format!("{sent} refreshes"),
    };
    match on_plan.filter(|_| said.billed < 0.005) {
        Some(plan) => format!(
            "It sent {count}, which read {} cached tokens of {plan}.",
            token_count(said.read)
        ),
        None => format!("It sent {count} for {}.", dollars(said.billed)),
    }
}

/// The notice of `stop`, with what was sent since keep-warm last said
/// (`said`), on the plan `on_plan` if one pays.
fn stopped(stop: &Stop, said: Totals, on_plan: Option<&str>) -> String {
    let tally = tally_sentence(said, on_plan);
    match stop {
        Stop::Ended(at) => format!(
            "Keep-warm ended at {}. {tally}",
            cache_timer::time_of_day(*at)
        ),
        Stop::Refreshes(count) => match on_plan.filter(|_| said.billed < 0.005) {
            Some(plan) => format!(
                "Keep-warm ended after its {}, which read {} cached tokens of {plan}.",
                refreshes_words(*count),
                token_count(said.read)
            ),
            None => format!(
                "Keep-warm ended after its {} for {}.",
                refreshes_words(*count),
                dollars(said.billed)
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
            "Keep-warm stopped because the prompt cache expired before its refresh. {tally}"
        ),
        Stop::Clock => format!(
            "Keep-warm paused because the computer slept or its clock changed. It starts again after your next message. {tally}"
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

/// Dollars as a command takes them, as "5", "2.5".
fn plain_dollars(amount: f64) -> String {
    let text = format!("{amount:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The wall clock now (Unix milliseconds).
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl crate::Harness {
    /// Keep-warm as the session opens, meaning the setting, and what the session
    /// kept of its own (its choice, what its refreshes sent, its latest
    /// request).
    pub(crate) fn keep_warm_at_start(&mut self) {
        *lock(&self.gauges.keep_warm) = load(&self.root);
        lock(&self.stats).keep_warm = KeepWarm::load(&self.session.dir, Moment::now());
    }

    /// Keeps the session's keep-warm in its folder, for when it opens again.
    pub(crate) fn save_keep_warm(&self) {
        let kept = lock(&self.stats).keep_warm.clone();
        if let Err(error) = kept.save(&self.session.dir) {
            log::warn!(
                "could not keep the session's keep-warm in {} ({error})",
                self.session.dir.display()
            );
        }
    }

    /// Keep-warm's limits now, under the setting, or `None` while it is off.
    fn keep_warm_limits(&self) -> Option<Limits> {
        let setting = *lock(&self.gauges.keep_warm);
        lock(&self.stats).keep_warm.limits(setting)
    }

    /// What a refresh of the conversation the next message goes to goes as, meaning
    /// its model, from its own settings (a resumed one's may not be the
    /// session's), and whether a plan pays. The status has it too.
    pub(crate) fn refresh_route(&self) -> Option<Route> {
        let settings = self.conversation_settings();
        let model = crate::providers::resolve_model(&settings.model).ok()?;
        let chatgpt = self.settings.openai_auth.or_else(|| self.openai_auth())
            == Some(crate::providers::OpenAiAuth::ChatGpt);
        let plan = match model.provider {
            arko_ai::Provider::ClaudeCode => true,
            arko_ai::Provider::OpenAI => chatgpt,
            _ => false,
        };
        Some(Route {
            model,
            fast: settings.fast,
            plan,
        })
    }

    /// The cache keep-warm refreshes and what a refresh of it costs, while
    /// there is a conversation whose model refreshes it out of band.
    /// Otherwise none, and keep-warm waits for its end.
    fn keep_warm_cache(&self) -> Cache {
        let route = self
            .refresh_route()
            .filter(|route| refreshes_out_of_band(&route.model));
        let Some(route) = route.filter(|_| !matches!(self.conversation, crate::Conversation::New))
        else {
            return Cache::default();
        };
        let stats = lock(&self.stats);
        Cache {
            used: stats.cache_used(),
            lifetime: stats.cache_lifetime,
            quote: crate::status::refresh_quote(&stats, &route),
        }
    }

    /// When the session's loop, waiting for the user, wakes for keep-warm,
    /// on the monotonic clock, for its next refresh, or to stop.
    pub(crate) fn keep_warm_wake(&self) -> Option<Instant> {
        let limits = self.keep_warm_limits()?;
        let cache = self.keep_warm_cache();
        lock(&self.stats)
            .keep_warm
            .wake_at(&limits, cache, Moment::now())
    }

    /// What keep-warm does at `now`, while the session waits for the user, meaning a
    /// refresh, a stop and its notice, or nothing yet.
    pub(crate) async fn keep_warm_tick(&mut self, now: Moment) {
        let Some(limits) = self.keep_warm_limits() else {
            return;
        };
        let cache = self.keep_warm_cache();
        let step = lock(&self.stats).keep_warm.step(now, &limits, cache);
        match step {
            Step::Wait => {}
            Step::Stop(stop) => self.stop_keep_warm(stop),
            Step::Refresh => {
                // Without a price, its dollars can't count, so it says so once.
                let unpriced = limits.dollars.filter(|_| {
                    let kept = &mut lock(&self.stats).keep_warm;
                    cache.quote.is_none() && !std::mem::replace(&mut kept.unpriced_told, true)
                });
                if let Some(most) = unpriced {
                    self.say(
                        Level::Warn,
                        &format!(
                            "Arko has no price for this model's refreshes, so keep-warm's {} limit doesn't apply.",
                            dollars(most)
                        ),
                    );
                }
                match Box::pin(self.refresh_out_of_band(true)).await {
                    Ok(Some(usage)) => lock(&self.stats).keep_warm.sent(&usage),
                    Ok(None) => self.stop_keep_warm(Stop::Unsupported),
                    Err(error) => self.stop_keep_warm(Stop::Failed(error)),
                }
            }
        }
        self.save_keep_warm();
        self.gauges.touch();
    }

    /// Ends keep-warm (or stops it until the next request, for its hours)
    /// and says why, with what it sent.
    fn stop_keep_warm(&mut self, stop: Stop) {
        let on_plan = self.keep_warm_plan();
        let text = {
            let mut stats = lock(&self.stats);
            // (No one can tell the cache's age until the next request.)
            if stop == Stop::Clock {
                stats.cache_seen = None;
            }
            stats.keep_warm.stop(&stop, on_plan)
        };
        let level = match stop {
            Stop::Lapsed | Stop::Clock | Stop::Failed(_) | Stop::Unsupported => Level::Warn,
            _ => Level::Info,
        };
        self.say(level, &text);
        self.save_keep_warm();
        self.gauges.touch();
    }

    /// The plan that pays for the main agent's refreshes, as the status
    /// names it ("your Claude plan"), if one does.
    fn keep_warm_plan(&self) -> Option<&'static str> {
        crate::status::cache_status(
            &lock(&self.stats),
            &lock(&self.gauges.models),
            None,
            None,
            false,
        )
        .on_plan
    }

    /// `/keep-warm`, typed at the prompt or sent from the web page.
    pub(crate) fn keep_warm_command(&mut self, argument: &str, source: Source) {
        let before = (lock(&self.stats).keep_warm.clone(), load(&self.root));
        let result = match parse(argument, Local::now()) {
            Err(error) => Err(error),
            Ok(Words::Show) => Ok(self.keep_warm_described()),
            Ok(Words::Off) => Ok(self.keep_warm_off()),
            Ok(Words::On(limits)) => self.keep_warm_on(limits),
            Ok(Words::Setting(setting)) => self.keep_warm_setting(setting),
        };
        let after = (lock(&self.stats).keep_warm.clone(), load(&self.root));
        self.save_keep_warm();
        self.setting_reply(result, source, before != after);
    }

    /// An app's [`Request::KeepWarm`](crate::Request::KeepWarm) is on with
    /// `limits`, or off.
    pub(crate) fn keep_warm_request(&mut self, limits: Option<Limits>) -> Result<(), String> {
        let done = match limits {
            Some(limits) => self.keep_warm_on(limits).map(|_| ()),
            None => {
                self.keep_warm_off();
                Ok(())
            }
        };
        self.save_keep_warm();
        done
    }

    /// Turns keep-warm on for the session, until `limits` end it, in place
    /// of the setting. It's refused where the model can't refresh its cache
    /// out of band, and while the session waits with a cache that lapsed.
    pub(crate) fn keep_warm_on(&mut self, limits: Limits) -> Result<String, String> {
        let route = self.refresh_route();
        if let Some(route) = route
            .as_ref()
            .filter(|route| !refreshes_out_of_band(&route.model))
        {
            return Err(format!(
                "{} can't refresh its prompt cache without adding a message, so it can't be kept warm.",
                crate::models::display_name(&crate::providers::model_name(&route.model))
            ));
        }
        if limits.ends_at.is_none() && limits.refreshes.is_none() && limits.idle_hours.is_none() {
            return Err(format!("Give a time or a number of refreshes. {USAGE}"));
        }
        let quote =
            route.and_then(|route| crate::status::refresh_quote(&lock(&self.stats), &route));
        if let (Some(most), Some(quote)) = (limits.dollars, quote)
            && quote > most
        {
            return Err(format!(
                "A refresh costs up to {}, more than the {} limit.",
                dollars(quote),
                dollars(most)
            ));
        }
        let lapsed = {
            let stats = lock(&self.stats);
            stats
                .cache_used()
                .zip(stats.cache_lifetime)
                .is_some_and(|(used, lifetime)| {
                    used.plus(cache_timer::lapse_after(lifetime))
                        .passed(Moment::now())
                })
        };
        if lapsed && !self.feed.activity().working {
            return Err(
                "The prompt cache has expired, so there's nothing to keep warm. Your next message caches the conversation again."
                    .into(),
            );
        }
        lock(&self.stats).keep_warm.start(limits, Moment::now());
        self.gauges.touch();
        Ok(self.keep_warm_described())
    }

    /// Turns keep-warm off for the session, also where the setting has it
    /// on, and says what it sent.
    fn keep_warm_off(&mut self) -> String {
        let was_on = self.keep_warm_limits().is_some();
        let on_plan = self.keep_warm_plan();
        let said = lock(&self.stats).keep_warm.off();
        self.gauges.touch();
        if was_on {
            format!("Keep-warm is off. {}", tally_sentence(said, on_plan))
        } else {
            "Keep-warm is off.".into()
        }
    }

    /// Sets the setting for every session. This one follows it unless
    /// `/keep-warm` chose for it, with what its refreshes sent so far.
    fn keep_warm_setting(&mut self, setting: Option<Limits>) -> Result<String, String> {
        save(&self.root, setting)?;
        *lock(&self.gauges.keep_warm) = setting;
        let mut text = setting_sentence(setting);
        if !lock(&self.stats).keep_warm.follows_setting() {
            text.push_str(" This session keeps what /keep-warm set.");
        }
        self.gauges.touch();
        Ok(text)
    }

    /// What keep-warm does in this session, and the setting.
    fn keep_warm_described(&self) -> String {
        let kept = lock(&self.stats).keep_warm.clone();
        let setting = *lock(&self.gauges.keep_warm);
        let Some(limits) = kept.limits(setting) else {
            return format!(
                "Keep-warm is off in this session. {} {USAGE}",
                setting_sentence(load(&self.root))
            );
        };
        let mut parts = Vec::new();
        match (kept.ends(&limits), limits.idle_hours) {
            (Some(end), _) => parts.push(format!("until {}", cache_timer::time_of_day(end.at))),
            (None, Some(hours)) => parts.push(format!(
                "for up to {} after your last message",
                hours_words(hours)
            )),
            (None, None) => {}
        }
        if let Some(left) = kept.left(&limits) {
            parts.push(format!("for {}", refreshes_words(left)));
        }
        if let Some(most) = limits.dollars {
            parts.push(format!("up to {}", dollars(most)));
        }
        let mut text = format!("Keep-warm is on {}", parts.join(" or "));
        if parts.len() > 1 {
            text.push_str(", whichever comes first");
        }
        text.push('.');
        let route = self.refresh_route();
        let cache = crate::status::cache_status(
            &lock(&self.stats),
            &lock(&self.gauges.models),
            route.as_ref(),
            setting,
            false,
        );
        if let Some(each) = cache_timer::refresh_each(&cache) {
            text.push_str(&format!(" {each}"));
        }
        if cache.keep_warm.is_some_and(|kept| kept.waits) {
            text.push(' ');
            text.push_str(cache_timer::KEEP_WARM_WAITS);
        }
        if kept.follows_setting() {
            text.push_str(" The setting turned it on, and /keep-warm off turns it off here.");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::harness::Prompt;
    use crate::mock_model::ModelServer;
    use crate::remote::Kind;

    const MINUTE: i64 = 60_000;
    const FIVE: Duration = Duration::from_secs(300);
    const HOUR: Duration = Duration::from_secs(3_600);

    /// The tests' monotonic clock starts here.
    fn base() -> Instant {
        static BASE: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
        *BASE
    }

    /// The moment `ms` milliseconds after the tests' start, on both clocks
    /// (the wall clock at 0 then).
    fn t(ms: i64) -> Moment {
        Moment {
            seen: base() + Duration::from_millis(ms as u64),
            at: ms,
        }
    }

    /// The status for `cache`, at its last use (when the clocks agree).
    fn seen_status(kept: &KeepWarm, limits: &Limits, cache: Cache, turn: bool) -> Status {
        kept.status(limits, cache, true, turn, cache.used.unwrap_or(t(0)))
    }

    /// When the loop wakes at `now`, in milliseconds from the tests' start.
    fn wake(kept: &KeepWarm, limits: &Limits, cache: Cache, now: i64) -> Option<i64> {
        kept.wake_at(limits, cache, t(now))
            .map(|at| at.duration_since(base()).as_millis() as i64)
    }

    /// Keep-warm the session turned on with `limits`, its latest request at
    /// 0:00, and the limits.
    fn on(limits: Limits) -> (KeepWarm, Limits) {
        let mut kept = KeepWarm::default();
        kept.requested(t(0));
        kept.start(limits, t(0));
        (kept, limits)
    }

    fn ending(ends_at: Option<i64>, refreshes: Option<u32>) -> Limits {
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

    /// The cache last used at `used` that lasts `lifetime`, each refresh of
    /// it costing $0.04 at most.
    fn cached(used: Option<i64>, lifetime: Option<Duration>) -> Cache {
        Cache {
            used: used.map(t),
            lifetime,
            quote: Some(0.04),
        }
    }

    /// Sends the refresh due at `now`, which leaves the cache last used then.
    fn refresh(kept: &mut KeepWarm, used: &mut i64, now: i64) {
        kept.sent(&usage(0.04));
        *used = now;
    }

    /// A five-minute cache refreshes at 4:00 and an hour's at 55:00, and
    /// each refresh moves the next one.
    #[test]
    fn a_cache_is_refreshed_a_lead_before_it_lapses() {
        let (mut kept, limits) = on(ending(Some(10 * 60 * MINUTE), None));
        let five = |used| cached(Some(used), Some(FIVE));
        assert_eq!(wake(&kept, &limits, five(0), 0), Some(4 * MINUTE));
        assert_eq!(kept.step(t(4 * MINUTE - 1), &limits, five(0)), Step::Wait);
        assert_eq!(kept.step(t(4 * MINUTE), &limits, five(0)), Step::Refresh);
        let hour = cached(Some(0), Some(HOUR));
        assert_eq!(wake(&kept, &limits, hour, 0), Some(55 * MINUTE));
        assert_eq!(kept.step(t(55 * MINUTE), &limits, hour), Step::Refresh);
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        assert_eq!(wake(&kept, &limits, five(used), used), Some(8 * MINUTE));
        assert_eq!(
            seen_status(&kept, &limits, five(used), false).next_at,
            Some(8 * MINUTE)
        );
        // None goes while a turn runs, and a cache whose time the session
        // can't vouch for waits for a request.
        assert_eq!(seen_status(&kept, &limits, five(used), true).next_at, None);
        assert_eq!(
            kept.step(t(9 * MINUTE), &limits, cached(None, Some(FIVE))),
            Step::Wait
        );
    }

    /// A turn's request moves the schedule, and isn't counted.
    #[test]
    fn a_turn_moves_the_next_refresh() {
        let (mut kept, limits) = on(ending(None, Some(3)));
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        // The user's message at 6:00 left the cache last used then.
        let turn = 6 * MINUTE;
        kept.requested(t(turn));
        let cache = cached(Some(turn), Some(FIVE));
        assert_eq!(wake(&kept, &limits, cache, turn), Some(10 * MINUTE));
        assert_eq!(kept.step(t(8 * MINUTE), &limits, cache), Step::Wait);
        assert_eq!(seen_status(&kept, &limits, cache, false).left, Some(2));
    }

    /// The end stops it, and a refresh that would come after it isn't sent.
    /// So does the count, right after its last refresh.
    #[test]
    fn the_end_and_the_count_stop_it() {
        let end = 6 * MINUTE;
        let (mut kept, limits) = on(ending(Some(end), None));
        // The refresh at 4:00 goes. The next, at 8:00, would be after the
        // end, so the loop wakes at 6:00 to stop.
        assert_eq!(
            kept.step(t(4 * MINUTE), &limits, cached(Some(0), Some(FIVE))),
            Step::Refresh
        );
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        let cache = cached(Some(used), Some(FIVE));
        assert_eq!(seen_status(&kept, &limits, cache, false).next_at, None);
        assert_eq!(wake(&kept, &limits, cache, used), Some(end));
        assert_eq!(
            kept.step(t(end), &limits, cache),
            Step::Stop(Stop::Ended(end))
        );
        // A refresh due right at the end doesn't go, and the status doesn't
        // show it, only the end at 8:00, when the refresh after 4:00 would be.
        let (kept, limits) = on(ending(Some(8 * MINUTE), None));
        assert_eq!(seen_status(&kept, &limits, cache, false).next_at, None);
        assert_eq!(wake(&kept, &limits, cache, used), Some(8 * MINUTE));
        assert_eq!(
            kept.step(t(8 * MINUTE), &limits, cache),
            Step::Stop(Stop::Ended(8 * MINUTE))
        );

        let (mut kept, limits) = on(ending(None, Some(2)));
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        assert_eq!(
            seen_status(&kept, &limits, cached(Some(used), Some(FIVE)), false).left,
            Some(1)
        );
        refresh(&mut kept, &mut used, 8 * MINUTE);
        let cache = cached(Some(used), Some(FIVE));
        // It wakes at once to stop.
        assert_eq!(wake(&kept, &limits, cache, used), Some(used));
        let stop = Stop::Refreshes(2);
        assert_eq!(
            kept.step(t(8 * MINUTE), &limits, cache),
            Step::Stop(stop.clone())
        );
        assert_eq!(
            kept.stop(&stop, None),
            "Keep-warm ended after its 2 refreshes for $0.08."
        );
        assert_eq!(kept.limits(None), None, "ended for the session");
    }

    /// The dollars stop it before a refresh would pass them, and a cache
    /// that lapsed stops it too.
    #[test]
    fn dollars_and_a_lapsed_cache_stop_it() {
        let (mut kept, limits) = on(Limits {
            refreshes: Some(100),
            dollars: Some(0.1),
            ..Limits::default()
        });
        let mut used = 0;
        refresh(&mut kept, &mut used, 4 * MINUTE);
        refresh(&mut kept, &mut used, 8 * MINUTE);
        let cache = cached(Some(used), Some(FIVE));
        assert_eq!(wake(&kept, &limits, cache, used), Some(used));
        assert_eq!(
            kept.step(t(9 * MINUTE), &limits, cache),
            Step::Stop(Stop::Dollars(0.1))
        );
        assert_eq!(
            kept.stop(&Stop::Dollars(0.1), None),
            "Keep-warm stopped at its $0.10 limit. It sent 2 refreshes for $0.08."
        );
        let (kept, limits) = on(ending(None, Some(5)));
        assert_eq!(
            kept.step(t(4 * MINUTE + 30_000), &limits, cached(Some(0), Some(FIVE))),
            Step::Stop(Stop::Lapsed)
        );
    }

    /// While the two clocks disagree on how long ago the cache was used, by
    /// more than a couple of seconds, no one can tell its age, so nothing is
    /// sent, meaning a wall clock set forward, a computer that slept and a wall clock
    /// set back each pause keep-warm. While they agree, the refresh goes by
    /// the monotonic clock, and the loop sleeps by it.
    #[test]
    fn clocks_that_disagree_pause_it() {
        let (kept, limits) = on(ending(None, Some(5)));
        let cache = cached(Some(0), Some(FIVE));
        let at = |seen: i64, wall: i64| Moment {
            seen: t(seen).seen,
            at: wall,
        };
        // The wall clock set forward, with 1:00 on the monotonic clock, 4:10 on
        // the wall. The loop still sleeps until 4:00 by the monotonic clock.
        let forward = at(MINUTE, 250_000);
        assert_eq!(kept.step(forward, &limits, cache), Step::Stop(Stop::Clock));
        assert_eq!(
            kept.wake_at(&limits, cache, forward),
            Some(t(4 * MINUTE).seen)
        );
        // The computer slept, with 4:00 on the monotonic clock, hours on the wall.
        let slept = at(4 * MINUTE, 6 * 60 * MINUTE);
        assert_eq!(kept.step(slept, &limits, cache), Step::Stop(Stop::Clock));
        // The wall clock set back, with 4:40 on the monotonic clock, 1:00 on the
        // wall.
        let back = at(280_000, MINUTE);
        assert_eq!(kept.step(back, &limits, cache), Step::Stop(Stop::Clock));
        // A second apart is the clocks agreeing, so the refresh goes at 4:00.
        assert_eq!(
            kept.step(at(4 * MINUTE, 4 * MINUTE + 1_000), &limits, cache),
            Step::Refresh
        );
        // The status shows no refresh while they disagree, and says it
        // waits.
        let status = kept.status(&limits, cache, true, false, forward);
        assert_eq!((status.next_at, status.waits), (None, true));
        assert!(!seen_status(&kept, &limits, cache, false).waits);
    }

    /// Each refresh, the first too, is priced before it goes, from the cache
    /// and the model as they are then, and one that would pass the dollars
    /// doesn't go. Without a price, the dollars don't apply.
    #[test]
    fn each_refresh_is_priced_before_it_goes() {
        let (kept, limits) = on(Limits {
            refreshes: Some(10),
            dollars: Some(0.01),
            ..Limits::default()
        });
        let five = |quote| Cache {
            used: Some(t(0)),
            lifetime: Some(FIVE),
            quote,
        };
        assert_eq!(wake(&kept, &limits, five(Some(0.04)), 0), Some(0));
        assert_eq!(
            kept.step(t(4 * MINUTE), &limits, five(Some(0.04))),
            Step::Stop(Stop::Dollars(0.01))
        );
        assert_eq!(
            seen_status(&kept, &limits, five(Some(0.04)), false).next_at,
            None
        );
        // Without a price, it goes by its other limits.
        assert_eq!(wake(&kept, &limits, five(None), 0), Some(4 * MINUTE));
        // A cache that grew after a cheap refresh, with $0.01 spent and $0.50
        // next passes $0.30.
        let (mut kept, limits) = on(Limits {
            refreshes: Some(10),
            dollars: Some(0.3),
            ..Limits::default()
        });
        kept.sent(&usage(0.01));
        assert_eq!(
            kept.step(t(4 * MINUTE), &limits, five(Some(0.2))),
            Step::Refresh
        );
        assert_eq!(
            kept.step(t(4 * MINUTE), &limits, five(Some(0.5))),
            Step::Stop(Stop::Dollars(0.3))
        );
    }

    /// The setting's hours count from the latest request keep-warm didn't
    /// send, which its own refreshes don't move. They stop it until the
    /// next request, which starts it again.
    #[test]
    fn the_settings_hours_count_from_the_latest_request() {
        let limits = Limits {
            idle_hours: Some(1),
            ..Limits::default()
        };
        let mut kept = KeepWarm::default();
        assert_eq!(kept.limits(Some(limits)), Some(limits));
        // No request yet, so nothing to keep warm.
        assert_eq!(wake(&kept, &limits, cached(None, Some(FIVE)), 0), None);
        kept.requested(t(0));
        // Refreshes at 4:00, 8:00 and on to 56:00. The next would be at
        // 60:00, the end of the hour, where it stops instead.
        let mut used = 0;
        let mut sent = 0;
        while let Some(at) = wake(&kept, &limits, cached(Some(used), Some(FIVE)), used) {
            match kept.step(t(at), &limits, cached(Some(used), Some(FIVE))) {
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
        // The status shows no refresh at 60:00, as none goes there.
        let cache = cached(Some(used), Some(FIVE));
        assert_eq!(seen_status(&kept, &limits, cache, false).next_at, None);
        assert_eq!(
            kept.stop(&Stop::Idle(1), Some("your Claude plan")),
            "Keep-warm stopped 1 hour after your last message, and starts again after your next one. It sent 14 refreshes for $0.56."
        );
        assert_eq!(wake(&kept, &limits, cache, 60 * MINUTE), None);
        assert_eq!(kept.step(t(70 * MINUTE), &limits, cache), Step::Wait);
        // The user's next message, at 90:00, starts another hour.
        let turn = 90 * MINUTE;
        kept.requested(t(turn));
        let cache = cached(Some(turn), Some(FIVE));
        assert_eq!(wake(&kept, &limits, cache, turn), Some(94 * MINUTE));
        assert_eq!(
            seen_status(&kept, &limits, cache, false).ends_at,
            Some(150 * MINUTE)
        );
    }

    /// What the session chose and what its refreshes sent are kept in its
    /// folder and come back as it opens again. A change of the setting
    /// keeps them, and a lower dollar limit counts what was spent already.
    /// A latest request dated after now (the clock was ahead) isn't vouched
    /// for, so the setting's hours wait for the next one.
    #[test]
    fn the_session_keeps_its_choice_and_what_it_spent() {
        let temp = tempfile::tempdir().unwrap();
        let setting = Limits {
            idle_hours: Some(15),
            dollars: Some(5.0),
            ..Limits::default()
        };
        let mut kept = KeepWarm::default();
        assert_eq!(KeepWarm::load(temp.path(), t(0)), kept, "none kept yet");
        kept.requested(t(0));
        let mut used = 0;
        for n in 1..=3 {
            refresh(&mut kept, &mut used, n * 4 * MINUTE);
        }
        kept.save(temp.path()).unwrap();
        let mut back = KeepWarm::load(temp.path(), t(13 * MINUTE));
        assert_eq!(back.counted().sent, 3);
        assert_eq!(back.requested_at, Some(0));
        // The session didn't see that request, so its hours wait for the
        // next one, and so does a cache restored with it.
        assert_eq!(back.request(), None);
        assert_eq!(
            wake(&back, &setting, cached(None, Some(FIVE)), 13 * MINUTE),
            None
        );
        let restored = back.status(
            &setting,
            cached(None, Some(FIVE)),
            true,
            false,
            t(13 * MINUTE),
        );
        assert_eq!((restored.next_at, restored.waits), (None, true));
        // The same setting again, or a lower one, keeps what was spent, as $0.12
        // passes a limit of $0.10.
        let lower = Limits {
            dollars: Some(0.1),
            ..setting
        };
        assert_eq!(back.limits(Some(lower)), Some(lower));
        assert_eq!(
            back.step(t(16 * MINUTE), &lower, cached(Some(used), Some(FIVE))),
            Step::Stop(Stop::Dollars(0.1))
        );
        assert!(!back.spent_out(&setting, Some(0.04)));
        // Off stays off when the session opens again.
        assert_eq!(back.off().sent, 3);
        back.save(temp.path()).unwrap();
        let back = KeepWarm::load(temp.path(), t(20 * MINUTE));
        assert_eq!(back.limits(Some(setting)), None);
        assert!(!back.follows_setting());
    }

    /// On a plan, the notice counts the tokens its refreshes read.
    #[test]
    fn on_a_plan_the_notice_counts_tokens() {
        let (mut kept, _) = on(ending(Some(60 * MINUTE), None));
        kept.sent(&usage(0.0));
        let said = kept.totals.since(kept.told);
        assert_eq!(
            tally_sentence(said, Some("your Claude plan")),
            "It sent 1 refresh, which read 180k cached tokens of your Claude plan."
        );
        assert_eq!(
            stopped(
                &Stop::Failed("the cache wasn't refreshed (the server said no)".into()),
                said,
                None
            ),
            "Keep-warm stopped. The cache wasn't refreshed (the server said no). It sent 1 refresh for $0.00."
        );
    }

    /// US Eastern time with 2026's changes of the clocks, for the tests, with
    /// daylight time (UTC-4) from March 8 at 2:00 to November 1 at 2:00, and
    /// standard time (UTC-5) otherwise.
    #[derive(Debug, Clone, Copy)]
    struct Eastern;

    impl Eastern {
        fn offset_at(utc: &chrono::NaiveDateTime) -> chrono::FixedOffset {
            let utc_at = |month, day, hour| {
                chrono::NaiveDate::from_ymd_opt(2026, month, day)
                    .unwrap()
                    .and_hms_opt(hour, 0, 0)
                    .unwrap()
            };
            let daylight = (utc_at(3, 8, 7)..utc_at(11, 1, 6)).contains(utc);
            chrono::FixedOffset::west_opt(if daylight { 4 } else { 5 } * 3_600).unwrap()
        }
    }

    impl TimeZone for Eastern {
        type Offset = chrono::FixedOffset;

        fn from_offset(_: &chrono::FixedOffset) -> Self {
            Eastern
        }

        fn offset_from_local_date(
            &self,
            local: &chrono::NaiveDate,
        ) -> LocalResult<chrono::FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &chrono::NaiveDateTime,
        ) -> LocalResult<chrono::FixedOffset> {
            let fits: Vec<chrono::FixedOffset> = [4, 5]
                .into_iter()
                .map(|hours| chrono::FixedOffset::west_opt(hours * 3_600).unwrap())
                .filter(|offset| Self::offset_at(&(*local - *offset)) == *offset)
                .collect();
            match fits[..] {
                [one] => LocalResult::Single(one),
                [first, second] => LocalResult::Ambiguous(first, second),
                _ => LocalResult::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> chrono::FixedOffset {
            Self::offset_at(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> chrono::FixedOffset {
            Self::offset_at(utc)
        }
    }

    /// `until` takes the next moment its time names across a change of the
    /// clocks. Of a time the clocks go through twice as they go back, it's
    /// the first still to come, and for a time they skip as they go forward,
    /// the first moment after the gap. One set before the clocks go back
    /// ends at that time on the clock after, and the status says it so.
    #[test]
    fn until_reads_a_time_of_day_across_a_change_of_the_clocks() {
        let utc = |month, day, hour, minute| {
            chrono::Utc
                .with_ymd_and_hms(2026, month, day, hour, minute, 0)
                .unwrap()
                .timestamp_millis()
        };
        let until = |words: &str, now: i64| {
            let now = chrono::Utc
                .timestamp_millis_opt(now)
                .unwrap()
                .with_timezone(&Eastern);
            match parse(words, now) {
                Ok(Words::On(limits)) => limits.ends_at.unwrap(),
                other => panic!("{words}: {other:?}"),
            }
        };
        // November 1 at 0:45 daylight time, so 1:30 comes first in daylight
        // time, at 5:30 UTC.
        assert_eq!(until("until 1:30", utc(11, 1, 4, 45)), utc(11, 1, 5, 30));
        // At 1:40 daylight time, past the first 1:30, then the second, in
        // standard time, at 6:30 UTC.
        assert_eq!(until("until 1:30", utc(11, 1, 5, 40)), utc(11, 1, 6, 30));
        // March 8 at midnight, where 2:30 doesn't come, so 3:00 daylight time.
        assert_eq!(until("until 2:30", utc(3, 8, 5, 0)), utc(3, 8, 7, 0));
        // October 31 at 23:00 daylight time, so 7:00 comes in standard time,
        // nine hours on.
        let ends = until("until 7:00", utc(11, 1, 3, 0));
        assert_eq!(ends, utc(11, 1, 12, 0));
        assert_eq!(cache_timer::time_of_day_in(ends, &Eastern), "07:00");
    }

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    /// The command's words are a time from now, a time of day to come, a
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
            "Every session keeps its prompt cache warm while it waits for you, for up to 15 hours after the last message or $5.00 (estimated on a plan)."
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
        assert_eq!(harness.keep_warm_limits(), None);
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
        // Its refresh is due a second after the turn's request.
        let used = lock(&harness.stats).cache_used().unwrap();
        let due = used.plus(Duration::from_secs(1));
        assert!(harness.keep_warm_wake().unwrap() <= due.seen);
        harness.keep_warm_tick(due).await;
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

    /// A refresh goes, and is priced, as the conversation runs. Its model and
    /// fast mode are the conversation's, whatever the session would start a
    /// new one with.
    #[tokio::test]
    async fn the_route_is_the_conversations_own() {
        let server = model(Arc::default(), false).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = answered(temp.path(), &server).await;
        // New conversations would start in fast mode from now on.
        harness.settings.main.fast = true;
        let route = harness.refresh_route().unwrap();
        assert_eq!(
            route.model.id,
            crate::providers::resolve_model("luna").unwrap().id
        );
        assert!(!route.fast, "the conversation runs without it");
        assert!(!route.plan, "an API key pays");
        harness.publish_models();
        let published = lock(&harness.gauges.keep_warm_route).clone().unwrap();
        assert_eq!(
            (published.model.id, published.fast, published.plan),
            (route.model.id, route.fast, route.plan)
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// Clocks that disagree pause keep-warm with a notice and send nothing,
    /// and the next request starts it again.
    #[tokio::test]
    async fn clocks_that_disagree_pause_it_until_the_next_message() {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = model(bodies.clone(), false).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = answered(temp.path(), &server).await;
        assert!(harness.live_command("/keep-warm 8h", Source::Terminal));
        let used = lock(&harness.stats).cache_used().unwrap();
        // A second on the monotonic clock, ten minutes on the wall.
        let skewed = Moment {
            seen: used.seen + Duration::from_secs(1),
            at: used.at + 10 * MINUTE,
        };
        harness.keep_warm_tick(skewed).await;
        assert_eq!(bodies.lock().unwrap().len(), 1, "nothing was sent");
        assert_eq!(
            said(&harness.feed).pop().unwrap(),
            "Keep-warm paused because the computer slept or its clock changed. It starts again after your next message. It sent no refreshes."
        );
        assert_eq!(lock(&harness.stats).cache_used(), None);
        assert_eq!(
            harness
                .keep_warm_limits()
                .map(|limits| limits.ends_at.is_some()),
            Some(true)
        );
        let status = crate::remote::Meter::read(harness.gauges.as_ref()).cache;
        assert!(status.keep_warm.unwrap().waits);
        harness
            .turn(Prompt::User("Again.".into()), &mut inputs)
            .await;
        assert!(lock(&harness.stats).cache_used().is_some());
        assert!(harness.keep_warm_wake().is_some());
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// A first turn that fails leaves no conversation, so its cache has
    /// nothing to keep warm, and neither the status nor the loop counts it.
    #[tokio::test]
    async fn a_failed_first_turn_leaves_no_cache_to_keep_warm() {
        // (A reply without a call fails a turn that ends with one.)
        let server = ModelServer::start(Arc::new(|_, _| {
            Box::pin(async { crate::mock_model::reply("a", "Noted.") })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) =
            crate::tests::harness(temp.path(), server.client(), true);
        harness.publish_models();
        assert!(harness.live_command("/keep-warm 5", Source::Terminal));
        let end = harness.turn(Prompt::User("Hi.".into()), &mut inputs).await;
        assert!(matches!(end, crate::harness::TurnEnd::Failed));
        assert_eq!(lock(&harness.stats).cache_used(), None);
        let cache = crate::remote::Meter::read(harness.gauges.as_ref()).cache;
        assert_eq!(cache.refreshed_at, None);
        assert_eq!(cache.keep_warm.and_then(|kept| kept.next_at), None);
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
            .keep_warm_request(Some(ending(None, Some(3))))
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
            // (Ten minutes ago on the wall clock is enough.)
            stats.cache_refreshed_at = Some(now_ms() - 10 * MINUTE);
            stats.cache_seen = Some(Instant::now());
        }
        assert!(
            harness
                .keep_warm_request(Some(ending(None, Some(3))))
                .unwrap_err()
                .starts_with("The prompt cache has expired")
        );
        // A limit below what one refresh costs.
        harness.publish_models();
        {
            let mut stats = lock(&harness.stats);
            stats.cache_refreshed_at = Some(now_ms());
            stats.cache_seen = Some(Instant::now());
            stats.context_tokens = Some(10_000_000);
        }
        let dear = Limits {
            refreshes: Some(3),
            dollars: Some(0.01),
            ..Limits::default()
        };
        let refused = harness.keep_warm_request(Some(dear)).unwrap_err();
        assert!(
            refused.starts_with("A refresh costs up to $")
                && refused.ends_with(", more than the $0.01 limit."),
            "{refused}"
        );
        assert_eq!(harness.keep_warm_limits(), None);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// The setting is kept for every session and keeps a session that made
    /// no choice warm, also one that opens again. `/keep-warm` in a session
    /// goes in its place there, also when it turns it off, which the session
    /// keeps when it opens again. A change of the setting then leaves it.
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
                "Changed from the web page. Every session keeps its prompt cache warm while it waits for you, for up to 15 hours after the last message or $5.00 (estimated on a plan)."
            ),
            "{:?}",
            said(&harness.feed)
        );
        assert_eq!(harness.keep_warm_limits(), Some(setting));
        assert!(lock(&harness.stats).keep_warm.follows_setting());
        let reading = crate::remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.settings.keep_warm, Some(setting));
        // The session opens again, and follows the setting.
        harness.keep_warm_at_start();
        assert_eq!(harness.keep_warm_limits(), Some(setting));
        assert!(harness.live_command("/keep-warm off", Source::Terminal));
        assert_eq!(
            said(&harness.feed).last().unwrap(),
            "Keep-warm is off. It sent no refreshes."
        );
        // It opens again (a resume, a switch between Chat and Agent), still
        // off.
        lock(&harness.stats).keep_warm = KeepWarm::default();
        harness.keep_warm_at_start();
        assert_eq!(harness.keep_warm_limits(), None);
        assert!(harness.live_command("/keep-warm default 8h", Source::Terminal));
        assert!(
            said(&harness.feed)
                .last()
                .unwrap()
                .ends_with("This session keeps what /keep-warm set."),
        );
        assert_eq!(harness.keep_warm_limits(), None);
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
