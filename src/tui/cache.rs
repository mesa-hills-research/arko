//! The prompt cache's timer in the status line: "cache 3:12" while the
//! agent waits for the user and the provider says how long the cache keeps
//! the conversation, in the warning colour for its last minute, then "cache
//! expired". It is hidden while the agent's requests run (each one
//! refreshes the cache), in a worker's view (the figures are the worker's)
//! and where the lifetime is not known. The words are
//! [`cache_timer`](crate::cache_timer)'s, which the web page and apps use.
//!
//! The terminal view has no notifications to send, so the warning colour
//! is what it does of Arko Studio's reminders five minutes and one minute
//! before the cache lapses.

use super::app::App;
use crate::cache_timer::{self, CacheTimer};

/// The time now (Unix milliseconds), which the timer counts from.
pub(super) fn now_ms() -> i64 {
    #[cfg(test)]
    if let Some(now) = tests::NOW.get() {
        return now;
    }
    chrono::Utc::now().timestamp_millis()
}

impl App {
    /// The timer the status line shows at `now`, if any.
    pub(super) fn cache_timer(&self, now: i64) -> Option<CacheTimer> {
        if self.watching.is_some() {
            return None;
        }
        CacheTimer::of(
            &self.reading.cache,
            cache_timer::requests_run(&self.activity),
            now,
        )
    }

    /// What the timer says now and whether it warns: the screen draws again
    /// when this changes, once a second at most.
    pub(super) fn cache_label(&self) -> Option<(String, bool)> {
        let timer = self.cache_timer(now_ms())?;
        Some((timer.label(), timer.warns()))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::glyphs::{Symbols, tests::choose_symbols};
    use crate::remote::{Activity, CacheStatus, Question};
    use crate::tui::app::Info;
    use crate::tui::input::{Editor, History};
    use crate::tui::screen::draw;
    use crate::tui::theme::{Catalog, Depth};

    thread_local! {
        /// The time a test chose for its thread, in place of the clock's.
        pub(super) static NOW: Cell<Option<i64>> = const { Cell::new(None) };
    }

    /// The idle session's start: the cache was last used at this time.
    const AT: i64 = 1_800_000_000_000;

    /// Holds the clock a test set until it goes.
    struct Clock;

    impl Clock {
        fn at(now: i64) -> Self {
            NOW.set(Some(now));
            Clock
        }
    }

    impl Drop for Clock {
        fn drop(&mut self) {
            NOW.set(None);
        }
    }

    fn app() -> App {
        App::new(
            crate::remote::Snapshot::default(),
            Editor::new(History::load(None)),
            Info {
                workspace: "work".into(),
                handover_at: 250_000,
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        )
    }

    /// An app on a session whose cache was last used at `AT`, with the
    /// lifetime in minutes, and the time `after` milliseconds later.
    fn idle(minutes: u64, after: i64) -> (App, Clock) {
        let clock = Clock::at(AT + after);
        let mut app = app();
        app.reading.context_tokens = Some(142_000);
        app.reading.cost = 3.2;
        app.reading.cache = CacheStatus {
            lifetime: Some(Duration::from_secs(minutes * 60)),
            refreshed_at: Some(AT),
            tokens: Some(142_000),
            ..CacheStatus::default()
        };
        app.last_turn = Some(Duration::from_secs(134));
        (app, clock)
    }

    /// The screen's rows and the buffer's cells, drawn on a `width` by
    /// `height` terminal.
    fn drawn(app: &mut App, width: u16, height: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, buffer)
    }

    /// The status line: two rows above the input box.
    fn status_row(rows: &[String]) -> &str {
        let at = rows
            .iter()
            .position(|row| row.starts_with(" › ") || row.starts_with(" > "))
            .expect("the box");
        &rows[at - 2]
    }

    /// Whether the timer's cells are in the warning colour (`true`) or the
    /// dim one, in the status line of `rows`.
    fn timer_style(rows: &[String], buffer: &ratatui::buffer::Buffer) -> ratatui::style::Style {
        let y = rows.iter().position(|row| row == status_row(rows)).unwrap() as u16;
        let x = status_row(rows).find("cache").unwrap();
        let x = status_row(rows)[..x].chars().count() as u16;
        buffer[(x, y)].style()
    }

    /// While the agent waits, the status line counts the cache down after
    /// the context's size, at 80 and at 120 columns.
    #[test]
    fn the_status_line_counts_the_cache_down_while_idle() {
        for width in [80, 120] {
            // 1:18 after the request, 3:12 left of five minutes, which count
            // as lapsed 30 seconds early.
            let (mut app, _clock) = idle(5, 78_000);
            let (rows, buffer) = drawn(&mut app, width, 8);
            let status = status_row(&rows);
            assert!(
                status.starts_with(" last turn 2m14s · 142k/250k · cache 3:12 · main: $3.20"),
                "{width}: {status}"
            );
            let theme = app.theme.clone();
            assert_eq!(timer_style(&rows, &buffer).fg, theme.dim().fg, "{width}");
            assert!(!app.cache_label().unwrap().1);
        }
    }

    /// The last minute is in the warning colour, and once the cache lapsed
    /// the line says so, with the dim colour again.
    #[test]
    fn the_last_minute_warns_and_then_the_cache_is_expired() {
        let (mut app, _clock) = idle(5, 270_000 - 59_500);
        let (rows, buffer) = drawn(&mut app, 80, 8);
        assert!(status_row(&rows).contains("cache 1:00"), "{rows:#?}");
        let theme = app.theme.clone();
        assert_eq!(timer_style(&rows, &buffer).fg, theme.warning().fg);
        assert_ne!(theme.warning().fg, theme.dim().fg);

        // Half a second left reads 0:01.
        let (mut app, _clock) = idle(5, 269_500);
        let (rows, _) = drawn(&mut app, 80, 8);
        assert!(status_row(&rows).contains("cache 0:01"), "{rows:#?}");

        let (mut app, _clock) = idle(5, 270_000);
        let (rows, buffer) = drawn(&mut app, 80, 8);
        let status = status_row(&rows);
        assert!(
            status.starts_with(" last turn 2m14s · 142k/250k · cache expired · main: $3.20"),
            "{status}"
        );
        let theme = app.theme.clone();
        assert_eq!(timer_style(&rows, &buffer).fg, theme.dim().fg);
        // (An hour's cache reads in minutes until the last hour is gone.)
        let (mut app, _clock) = idle(60, 1_000);
        let (rows, _) = drawn(&mut app, 80, 8);
        assert!(status_row(&rows).contains("cache 59:29"), "{rows:#?}");
    }

    /// While requests run each one refreshes the cache, so the line shows no
    /// timer. A question waiting for the answer stops the requests, and the
    /// timer shows again, beside "asks you".
    #[test]
    fn the_timer_is_hidden_while_requests_run() {
        let (mut app, _clock) = idle(5, 78_000);
        app.set_activity(Activity {
            reporting: false,
            working: true,
            since: None,
            question: None,
        });
        let (rows, _) = drawn(&mut app, 120, 8);
        let status = status_row(&rows);
        assert!(status.starts_with(" ● working"), "{status}");
        assert!(!status.contains("cache"), "{status}");
        assert_eq!(app.cache_label(), None);

        app.set_activity(Activity {
            reporting: false,
            working: true,
            since: None,
            question: Some(Question {
                id: 1,
                text: "Tabs or spaces?".into(),
            }),
        });
        let mut questions = crate::tui::app::tests::asking().questions;
        questions.truncate(1);
        questions[0].waits = true;
        app.reading.questions = questions;
        let (rows, _) = drawn(&mut app, 120, 8);
        let status = status_row(&rows);
        assert!(
            status.starts_with(" ● asks you · 142k/250k · cache 3:12 · main: $3.20"),
            "{status}"
        );
    }

    /// A worker's view shows the worker's figures, not the main agent's
    /// cache. Where the provider gives no lifetime, or nothing was used yet,
    /// there is no timer.
    #[test]
    fn the_timer_is_hidden_in_a_workers_view_and_without_a_lifetime() {
        let (mut app, _clock) = idle(5, 78_000);
        app.watching = Some("r1".into());
        assert_eq!(app.cache_label(), None);
        let (rows, _) = drawn(&mut app, 120, 8);
        assert!(!status_row(&rows).contains("cache"), "{rows:#?}");
        app.watching = None;
        assert!(app.cache_label().is_some());

        app.reading.cache.lifetime = None;
        let (rows, _) = drawn(&mut app, 120, 8);
        assert!(!status_row(&rows).contains("cache"), "{rows:#?}");
        app.reading.cache.lifetime = Some(Duration::from_secs(300));
        app.reading.cache.refreshed_at = None;
        let (rows, _) = drawn(&mut app, 120, 8);
        assert!(!status_row(&rows).contains("cache"), "{rows:#?}");
    }

    /// With ARKO_SYMBOLS=ascii the line uses plain separators, at both
    /// widths, and the timer's words are the same.
    #[test]
    fn the_status_line_with_ascii_symbols() {
        let _chosen = choose_symbols(Symbols::Ascii);
        for width in [80, 120] {
            let (mut app, _clock) = idle(5, 78_000);
            let (rows, _) = drawn(&mut app, width, 8);
            let status = status_row(&rows);
            assert!(
                status.starts_with(" last turn 2m14s | 142k/250k | cache 3:12 | main: $3.20"),
                "{width}: {status}"
            );
            assert!(status.is_ascii(), "{status}");
            let (mut app, _clock) = idle(5, 400_000);
            let (rows, _) = drawn(&mut app, width, 8);
            assert!(
                status_row(&rows).contains("| cache expired |"),
                "{width}: {rows:#?}"
            );
        }
    }

    /// At 80 columns the timer stays in sight beside the other figures of a
    /// busy line, and the line keeps to the width.
    #[test]
    fn the_timer_stays_in_sight_on_a_busy_line_at_80_columns() {
        let (mut app, _clock) = idle(5, 78_000);
        app.reading.plans = vec![crate::remote::PlanStatus {
            plan: "Claude plan".into(),
            windows: [("5 h", 21.0), ("7 d", 4.0)]
                .into_iter()
                .map(|(label, used_percent)| crate::remote::PlanWindow {
                    label: label.into(),
                    used_percent,
                    resets_at: None,
                })
                .collect(),
            credits: None,
            extra_usage: None,
            set_aside: 0,
            text: String::new(),
        }];
        app.reading.api_price = Some(20.0);
        let (rows, _) = drawn(&mut app, 80, 8);
        let status = status_row(&rows);
        assert!(
            status.starts_with(
                " last turn 2m14s · 142k/250k · cache 3:12 · 5h: 21% · 7d: 4% · main: $3.20"
            ),
            "{status}"
        );
        assert!(status.chars().count() <= 80, "{status}");
    }

    /// The view draws again when the timer's words change, once a second,
    /// and not otherwise.
    #[test]
    fn the_label_changes_each_second() {
        let (mut app, _clock) = idle(5, 78_000);
        let label = |app: &App| app.cache_label().map(|(label, _)| label);
        assert_eq!(label(&app).as_deref(), Some("cache 3:12"));
        NOW.set(Some(AT + 78_400));
        assert_eq!(label(&app).as_deref(), Some("cache 3:12"));
        NOW.set(Some(AT + 79_000));
        assert_eq!(label(&app).as_deref(), Some("cache 3:11"));
        NOW.set(Some(AT + 1_000_000));
        assert_eq!(label(&app).as_deref(), Some("cache expired"));
        app.reading.cache = CacheStatus::default();
        assert_eq!(label(&app), None);
    }
}
