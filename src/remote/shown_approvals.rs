//! Requests must have been shown long enough to read before an answer takes them.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct ShownApprovals {
    shown_at_ms: HashMap<String, u64>,
}

impl ShownApprovals {
    pub fn retain(&mut self, ids: &HashSet<String>) {
        self.shown_at_ms.retain(|id, _| ids.contains(id));
    }

    /// Leaving sight before the guard ends starts the wait over next time.
    pub fn visible(&mut self, ids: &HashSet<String>, now_ms: u64) {
        self.shown_at_ms
            .retain(|id, shown| ids.contains(id) || now_ms.saturating_sub(*shown) >= 500);
        for id in ids {
            self.shown_at_ms.entry(id.clone()).or_insert(now_ms);
        }
    }

    pub fn can_answer(&self, id: &str, now_ms: u64) -> bool {
        self.shown_at_ms
            .get(id)
            .is_some_and(|shown| now_ms.saturating_sub(*shown) >= 500)
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
