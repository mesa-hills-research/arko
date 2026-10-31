//! Whether an approval answer includes the message box's text.

use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageSwitch {
    seen: HashSet<String>,
    newest_request: Option<String>,
    has_text: bool,
    for_answer: bool,
    manual: Option<bool>,
}

impl MessageSwitch {
    pub fn requests(&mut self, ids: HashSet<String>, has_text: bool) {
        self.edited(has_text);
        if let Some(newest) = ids.difference(&self.seen).max() {
            self.newest_request = Some(newest.clone());
            self.for_answer = !has_text;
            self.manual = None;
        }
        self.seen = ids;
    }

    pub fn edited(&mut self, has_text: bool) {
        self.has_text = has_text;
        if !has_text {
            self.for_answer = true;
            self.manual = None;
        }
    }

    pub fn includes(&self) -> bool {
        self.has_text && self.manual.unwrap_or(self.for_answer)
    }

    pub fn flip(&mut self) {
        if self.has_text {
            self.manual = Some(!self.includes());
        }
    }

    /// A refused send returns its draft's intent, not the send's emptying.
    pub fn restored(&mut self, previous: &Self, has_text: bool) {
        self.has_text = has_text;
        if self.newest_request == previous.newest_request {
            self.for_answer = previous.for_answer;
            self.manual = previous.manual;
        } else {
            self.for_answer = false;
            self.manual = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).into()).collect()
    }

    #[test]
    fn an_older_draft_stays_out_even_when_edited() {
        let mut switch = MessageSwitch::default();
        switch.requests(ids(&["a1"]), true);
        switch.edited(true);
        assert!(!switch.includes());
        switch.flip();
        switch.edited(true);
        assert!(switch.includes());
        switch.flip();
        switch.edited(true);
        assert!(!switch.includes());
    }

    #[test]
    fn an_empty_box_starts_a_message_for_the_answer() {
        let mut switch = MessageSwitch::default();
        switch.requests(ids(&["a1"]), false);
        assert!(!switch.includes());
        switch.edited(true);
        assert!(switch.includes());
        switch.flip();
        switch.edited(true);
        assert!(!switch.includes());
        switch.edited(false);
        switch.edited(true);
        assert!(switch.includes());
    }

    #[test]
    fn a_new_request_resets_the_choice_but_removing_one_keeps_it() {
        let mut switch = MessageSwitch::default();
        switch.requests(ids(&["a1"]), false);
        switch.edited(true);
        assert!(switch.includes());
        switch.requests(ids(&["a1", "a2"]), true);
        assert!(!switch.includes());
        switch.flip();
        switch.requests(ids(&["a1"]), true);
        assert!(switch.includes());
        switch.requests(ids(&[]), true);
        switch.requests(ids(&["a3"]), true);
        assert!(!switch.includes());
    }

    #[test]
    fn a_refusal_returns_the_drafts_choice() {
        for manual in [false, true] {
            let mut switch = MessageSwitch::default();
            switch.requests(ids(&["a1"]), true);
            if manual {
                switch.flip();
            }
            let sent = switch.clone();
            switch.edited(false);
            switch.restored(&sent, true);
            assert_eq!(switch.includes(), manual);
            switch.requests(ids(&[]), false);
            switch.restored(&sent, true);
            assert_eq!(
                switch.includes(),
                manual,
                "removing requests keeps the draft's choice"
            );
            switch.requests(ids(&["a2"]), false);
            switch.requests(ids(&[]), false);
            switch.restored(&sent, true);
            assert!(
                !switch.includes(),
                "new requests do not attach a restored draft"
            );
        }
    }
}
