//! Requests must have been shown long enough to read before an answer takes them.

use std::collections::{HashMap, HashSet};

/// When each of the approval tray's requests came into sight, so an answer
/// takes only those shown for [`READ_MS`](Self::READ_MS), never one the user
/// had no time to read. The terminal and the web page keep one per tray, and
/// so can an app.
///
/// Call [`visible`](Self::visible) each time the tray draws, with the
/// requests on screen (none while it's hidden), and
/// [`retain`](Self::retain) when requests leave the tray. An answer takes
/// the requests [`can_answer`](Self::can_answer) allows, and
/// [`ready_at_ms`](Self::ready_at_ms) says when to draw again for the next
/// one. Times are milliseconds on any clock that doesn't go back.
#[derive(Debug, Default)]
pub struct ShownApprovals {
    shown_at_ms: HashMap<String, u64>,
}

impl ShownApprovals {
    /// How long a request must have been in sight before an answer takes
    /// it, in milliseconds.
    pub const READ_MS: u64 = 500;

    /// Forgets the requests not in `ids`, the tray's requests now.
    pub fn retain(&mut self, ids: &HashSet<String>) {
        self.shown_at_ms.retain(|id, _| ids.contains(id));
    }

    /// The requests `ids` are on screen at `now_ms`. A request that leaves
    /// sight before it can be answered starts its wait over when it shows
    /// again.
    pub fn visible(&mut self, ids: &HashSet<String>, now_ms: u64) {
        self.shown_at_ms
            .retain(|id, shown| ids.contains(id) || now_ms.saturating_sub(*shown) >= Self::READ_MS);
        for id in ids {
            self.shown_at_ms.entry(id.clone()).or_insert(now_ms);
        }
    }

    /// The next time the visible requests change which answers are available.
    pub fn ready_at_ms(&self, now_ms: u64) -> Option<u64> {
        self.shown_at_ms
            .values()
            .map(|shown| shown.saturating_add(Self::READ_MS))
            .filter(|ready| *ready > now_ms)
            .min()
    }

    /// Whether an answer at `now_ms` takes the request `id`.
    pub fn can_answer(&self, id: &str, now_ms: u64) -> bool {
        self.shown_at_ms
            .get(id)
            .is_some_and(|shown| now_ms.saturating_sub(*shown) >= Self::READ_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rendered_requests_old_enough_can_be_answered() {
        let mut shown = ShownApprovals::default();
        assert!(!shown.can_answer("a1", 1000));
        shown.visible(&HashSet::from(["a1".into()]), 1000);
        shown.visible(&HashSet::from(["a1".into()]), 1200);
        shown.visible(&HashSet::from(["a1".into(), "a2".into()]), 1400);
        assert_eq!(shown.ready_at_ms(1400), Some(1500));
        assert_eq!(shown.ready_at_ms(1500), Some(1900));
        assert_eq!(shown.ready_at_ms(1900), None);
        assert!(!shown.can_answer("a1", 1499));
        assert!(shown.can_answer("a1", 1500));
        assert!(!shown.can_answer("a2", 1500));
        shown.visible(&HashSet::new(), 1500);
        assert!(!shown.can_answer("a2", 2000), "unseen time does not count");
        assert!(
            shown.can_answer("a1", 2000),
            "a read request stays eligible"
        );
        shown.retain(&HashSet::from(["a2".into()]));
        assert!(!shown.can_answer("a1", 2000));
    }
}
