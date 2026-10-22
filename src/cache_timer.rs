//! The prompt cache's timer, as the full-screen terminal view, the web page
//! and apps show it, worked out from a session's [`CacheStatus`] and the
//! time, so each says the same words.
//!
//! While a session is idle and its provider says how long the cache keeps
//! the conversation, the timer counts down to the moment it lapses ("cache
//! 3:12"), warns in the last minute, then says "cache expired". It is
//! hidden while a turn's requests run, since each one refreshes the cache,
//! and where the lifetime isn't known. A resumed session counts from the
//! request it was left at, so it says how long is left, or that the cache
//! expired meanwhile.
//!
//! Times are Unix milliseconds, as [`CacheStatus::refreshed_at`] has them.

use std::time::Duration;

use chrono::{DateTime, Datelike as _, Local, TimeZone as _};

use crate::remote::{Activity, CacheStatus};

/// The cache timer, while idle with a known lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheTimer {
    /// It lapses in this many milliseconds.
    Counting { left: i64 },
    /// It lapsed.
    Expired,
}

/// The last minute, when the timer warns (in milliseconds).
pub const WARNING_MS: i64 = 60_000;

/// How long before the cache lapses an early reminder comes (in
/// milliseconds), for caches that last [`EARLY_FROM_LIFETIME`] or more.
pub const EARLY_MS: i64 = 5 * 60_000;

/// How long before it lapses the last reminder comes (in milliseconds).
pub const FINAL_MS: i64 = 60_000;

/// Caches that last less than this get only the last reminder.
pub const EARLY_FROM_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Whether the session's requests keep its prompt cache warm now, which is
/// while a turn runs and isn't waiting for the user's answer to a question.
pub fn requests_run(activity: &Activity) -> bool {
    activity.working && activity.question.is_none()
}

impl CacheTimer {
    /// The timer for `cache` at `now`, hidden (`None`) while requests run
    /// (each one refreshes the cache, see [`requests_run`]) and where its
    /// lifetime or its last use isn't known. The time left is never more
    /// than the lifetime, whatever the clock did.
    pub fn of(cache: &CacheStatus, requests_run: bool, now: i64) -> Option<Self> {
        if requests_run {
            return None;
        }
        let lifetime = lifetime_ms(cache.lifetime?);
        let left = (expires_at(cache)? - now).min(lifetime);
        Some(if left > 0 {
            CacheTimer::Counting { left }
        } else {
            CacheTimer::Expired
        })
    }

    /// "cache 3:12", "cache expired".
    pub fn label(self) -> String {
        match self {
            // The minutes and seconds left, counted up to the next second, so
            // the timer reads 0:01 until the moment it lapses.
            CacheTimer::Counting { left } => format!("cache {}", clock(left + 999)),
            CacheTimer::Expired => "cache expired".to_string(),
        }
    }

    /// In its last minute.
    pub fn warns(self) -> bool {
        matches!(self, CacheTimer::Counting { left } if left <= WARNING_MS)
    }
}

/// When the main agent's prompt cache lapses, when its lifetime and its
/// last use are known.
pub fn expires_at(cache: &CacheStatus) -> Option<i64> {
    let refreshed = cache.refreshed_at?;
    Some(refreshed.saturating_add(lifetime_ms(cache.lifetime?)))
}

fn lifetime_ms(lifetime: Duration) -> i64 {
    i64::try_from(lifetime.as_millis()).unwrap_or(i64::MAX / 2)
}

/// What a cache miss writes again, such as "about 180k tokens, ≈ $0.90" or
/// "about 180k tokens of your Claude plan".
pub fn recache(cache: &CacheStatus) -> Option<String> {
    let tokens = cache.tokens?;
    let mut text = format!("about {} tokens", token_count(tokens));
    if let Some(cost) = cache.miss_cost {
        text.push_str(&format!(", ≈ {}", dollars(cost)));
    } else if let Some(plan) = cache.on_plan {
        text.push_str(&format!(" of {plan}"));
    }
    Some(text)
}

/// What the timer means, for a tooltip or a line of help.
pub fn tooltip(timer: CacheTimer, cache: &CacheStatus, now: i64) -> String {
    match timer {
        CacheTimer::Counting { .. } => {
            let until = expires_at(cache)
                .map(|at| when(at.div_euclid(1000), now))
                .unwrap_or_default();
            let mut text = format!("The prompt cache keeps this conversation until {until}.");
            if let Some(recache) = recache(cache) {
                text.push_str(&format!(" Letting it expire re-caches {recache}."));
            }
            text
        }
        CacheTimer::Expired => match recache(cache) {
            Some(recache) => format!("The next message re-caches {recache}."),
            None => "The prompt cache has expired.".to_string(),
        },
    }
}

/// What a reminder says the two choices cost, such as "Refresh ≈ $0.04 ·
/// Letting it expire re-caches ≈ $0.90". On a plan, it counts tokens.
pub fn refresh_choices(cache: &CacheStatus) -> String {
    match (cache.refresh_cost, cache.miss_cost) {
        (Some(refresh), Some(miss)) => format!(
            "Refresh ≈ {} · Letting it expire re-caches ≈ {}",
            dollars(refresh),
            dollars(miss)
        ),
        _ => match (cache.tokens, cache.on_plan) {
            (Some(tokens), Some(plan)) => format!(
                "Refresh reads {} cached tokens · Letting it expire writes them again, on {plan}",
                token_count(tokens)
            ),
            (Some(tokens), None) => format!(
                "Letting it expire re-caches about {} tokens",
                token_count(tokens)
            ),
            _ => String::new(),
        },
    }
}

/// What letting the cache lapse costs, such as "Letting it expire re-caches
/// ≈ $0.90". On a plan, it counts tokens.
pub fn expiry_cost(cache: &CacheStatus) -> Option<String> {
    match (cache.miss_cost, cache.tokens, cache.on_plan) {
        (Some(miss), _, _) => Some(format!("Letting it expire re-caches ≈ {}", dollars(miss))),
        (None, Some(tokens), Some(plan)) => Some(format!(
            "Letting it expire writes {} tokens again, on {plan}",
            token_count(tokens)
        )),
        (None, Some(tokens), None) => Some(format!(
            "Letting it expire re-caches about {} tokens",
            token_count(tokens)
        )),
        (None, None, _) => None,
    }
}

/// What a reminder says while a question waits. Answering keeps the cache
/// warm, and it says what letting the cache lapse costs.
pub fn answer_choices(cache: &CacheStatus) -> String {
    let mut text = "Answering before then keeps it warm".to_string();
    if let Some(expiry) = expiry_cost(cache) {
        text.push_str(" · ");
        text.push_str(&expiry);
    }
    text
}

/// A reminder's headline, `left` milliseconds before the cache lapses, such
/// as "The prompt cache expires in 5 minutes" or "The prompt cache expires in
/// a minute".
pub fn headline(left: i64) -> String {
    match (left.max(0) + 59_999) / 60_000 {
        0 | 1 => "The prompt cache expires in a minute".to_string(),
        minutes => format!("The prompt cache expires in {minutes} minutes"),
    }
}

/// A token count, short: "820", "8.2k", "142k", "1.02M", "1M".
fn token_count(count: usize) -> String {
    /// `value` with at most `decimals` decimals, trailing zeros dropped.
    fn trimmed(value: f64, decimals: usize) -> String {
        let text = format!("{value:.decimals$}");
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    }
    let count = count as f64;
    if count < 1_000. {
        return format!("{count}");
    }
    // Rounded first, so that 9,960 reads "10k", not "10.0k", and 999,600
    // reads "1M", not "1000k".
    let thousands = count / 1_000.;
    if (thousands * 10.).round() / 10. < 10. {
        return format!("{}k", trimmed(thousands, 1));
    }
    if thousands.round() < 1_000. {
        return format!("{}k", thousands.round());
    }
    format!("{}M", trimmed(count / 1_000_000., 2))
}

/// Dollars, to the cent: "$1.20", and "< $0.01" for less than a cent.
fn dollars(amount: f64) -> String {
    if amount > 0. && amount < 0.005 {
        "< $0.01".to_string()
    } else {
        format!("${amount:.2}")
    }
}

/// A length of time on a clock: "0:05", "1:23", "12:00", "1:02:03".
fn clock(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// A local time of day for a Unix time in seconds: "14:00" today, "Mon
/// 09:00" within a week, else "Oct 20".
fn when(unix_seconds: i64, now_ms: i64) -> String {
    let (Some(then), Some(now)) = (
        Local.timestamp_opt(unix_seconds, 0).single(),
        Local.timestamp_millis_opt(now_ms).single(),
    ) else {
        return String::new();
    };
    when_local(then, now)
}

fn when_local(then: DateTime<Local>, now: DateTime<Local>) -> String {
    let days = (then.date_naive() - now.date_naive()).num_days();
    if days == 0 {
        then.format("%H:%M").to_string()
    } else if (1..7).contains(&days) {
        then.format("%a %H:%M").to_string()
    } else if then.year() == now.year() {
        then.format("%b %-d").to_string()
    } else {
        then.format("%b %-d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Question;

    fn cache(lifetime_minutes: u64, refreshed_at: i64) -> CacheStatus {
        CacheStatus {
            lifetime: Some(Duration::from_secs(lifetime_minutes * 60)),
            refreshed_at: Some(refreshed_at),
            tokens: Some(180_000),
            read: Some(170_000),
            written: Some(10_000),
            miss_cost: Some(0.9),
            refresh_cost: Some(0.04),
            on_plan: None,
        }
    }

    #[test]
    fn the_timer_counts_down_while_idle() {
        let cache = cache(5, 0);
        // 3:12 left.
        let timer = CacheTimer::of(&cache, false, 300_000 - 192_000).unwrap();
        assert_eq!(timer.label(), "cache 3:12");
        assert!(!timer.warns());
        // Half a second left reads 0:01, and it warns.
        let timer = CacheTimer::of(&cache, false, 299_500).unwrap();
        assert_eq!(timer.label(), "cache 0:01");
        assert!(timer.warns());
        // The last minute warns from its first moment.
        assert!(CacheTimer::of(&cache, false, 240_000).unwrap().warns());
        assert!(!CacheTimer::of(&cache, false, 239_999).unwrap().warns());
        assert_eq!(
            CacheTimer::of(&cache, false, 300_000),
            Some(CacheTimer::Expired)
        );
        assert_eq!(CacheTimer::Expired.label(), "cache expired");
        assert!(!CacheTimer::Expired.warns());
        // Hidden while working, and when the lifetime or the time isn't
        // known.
        assert_eq!(CacheTimer::of(&cache, true, 1_000), None);
        let unknown = CacheStatus {
            lifetime: None,
            ..cache.clone()
        };
        assert_eq!(CacheTimer::of(&unknown, false, 1_000), None);
        let unused = CacheStatus {
            refreshed_at: None,
            ..cache
        };
        assert_eq!(CacheTimer::of(&unused, false, 1_000), None);
    }

    /// An hour's cache reads in hours, and a clock that went back never
    /// shows more than the lifetime.
    #[test]
    fn the_time_left_never_passes_the_lifetime() {
        let hour = cache(60, 0);
        let timer = CacheTimer::of(&hour, false, 1_000).unwrap();
        assert_eq!(timer.label(), "cache 59:59");
        // The request is dated ten minutes after now.
        let ahead = cache(5, 600_000);
        let timer = CacheTimer::of(&ahead, false, 0).unwrap();
        assert_eq!(timer, CacheTimer::Counting { left: 300_000 });
        assert_eq!(timer.label(), "cache 5:00");
        // An hour or more left reads with hours.
        assert_eq!(clock(3_723_000), "1:02:03");
        assert_eq!(clock(-5), "0:00");
    }

    /// A question waiting for the user's answer stops the turn's requests:
    /// the cache counts down then.
    #[test]
    fn requests_stop_while_a_question_waits() {
        let mut activity = Activity {
            working: true,
            ..Activity::default()
        };
        assert!(requests_run(&activity));
        activity.question = Some(Question {
            id: 1,
            text: "Tabs or spaces?".into(),
        });
        assert!(!requests_run(&activity));
        assert!(!requests_run(&Activity::default()));
        let cache = cache(5, 0);
        assert_eq!(
            answer_choices(&cache),
            "Answering before then keeps it warm · Letting it expire re-caches ≈ $0.90"
        );
    }

    #[test]
    fn the_cache_says_what_each_choice_costs() {
        let mut cache = cache(5, 0);
        assert_eq!(
            refresh_choices(&cache),
            "Refresh ≈ $0.04 · Letting it expire re-caches ≈ $0.90"
        );
        assert_eq!(
            tooltip(CacheTimer::Expired, &cache, 400_000),
            "The next message re-caches about 180k tokens, ≈ $0.90."
        );
        cache.miss_cost = None;
        cache.refresh_cost = None;
        cache.on_plan = Some("your Claude plan");
        assert_eq!(
            recache(&cache).as_deref(),
            Some("about 180k tokens of your Claude plan")
        );
        assert_eq!(
            refresh_choices(&cache),
            "Refresh reads 180k cached tokens · Letting it expire writes them again, on your Claude plan"
        );
        assert_eq!(
            expiry_cost(&cache).as_deref(),
            Some("Letting it expire writes 180k tokens again, on your Claude plan")
        );
        cache.on_plan = None;
        assert_eq!(
            expiry_cost(&cache).as_deref(),
            Some("Letting it expire re-caches about 180k tokens")
        );
        cache.tokens = None;
        assert_eq!(expiry_cost(&cache), None);
        assert_eq!(
            tooltip(CacheTimer::Expired, &cache, 400_000),
            "The prompt cache has expired."
        );
    }

    /// The tooltip names the time it lapses, in the local day's terms.
    #[test]
    fn the_tooltip_names_when_the_cache_lapses() {
        let at = Local
            .with_ymd_and_hms(2026, 10, 22, 14, 0, 0)
            .single()
            .unwrap();
        let now = at.timestamp_millis();
        // Refreshed at 13:55 with five minutes: it lapses at 14:00.
        let cache = cache(5, now - 5 * 60_000);
        let timer = CacheTimer::Counting { left: 120_000 };
        assert_eq!(
            tooltip(timer, &cache, now - 120_000),
            "The prompt cache keeps this conversation until 14:00. Letting it expire re-caches about 180k tokens, ≈ $0.90."
        );
        // Tomorrow's, and another day's.
        let tomorrow = (at + chrono::Duration::days(1)).timestamp_millis();
        assert!(when(tomorrow.div_euclid(1000), now).contains(':'));
        let later = (at + chrono::Duration::days(30)).timestamp_millis();
        assert_eq!(when(later.div_euclid(1000), now), "Nov 21");
    }

    #[test]
    fn the_thresholds_and_headlines_are_the_reminders() {
        assert_eq!((EARLY_MS, FINAL_MS, WARNING_MS), (300_000, 60_000, 60_000));
        assert_eq!(EARLY_FROM_LIFETIME, Duration::from_secs(900));
        assert_eq!(headline(EARLY_MS), "The prompt cache expires in 5 minutes");
        assert_eq!(headline(299_999), "The prompt cache expires in 5 minutes");
        assert_eq!(headline(FINAL_MS), "The prompt cache expires in a minute");
        assert_eq!(headline(0), "The prompt cache expires in a minute");
        assert_eq!(headline(61_000), "The prompt cache expires in 2 minutes");
    }

    #[test]
    fn token_counts_are_short() {
        for (count, text) in [
            (820, "820"),
            (8_200, "8.2k"),
            (9_960, "10k"),
            (142_000, "142k"),
            (999_600, "1M"),
            (1_020_000, "1.02M"),
        ] {
            assert_eq!(token_count(count), text);
        }
        assert_eq!(dollars(0.001), "< $0.01");
        assert_eq!(dollars(1.2), "$1.20");
    }
}
