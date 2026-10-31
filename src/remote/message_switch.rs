//! Whether an approval answer includes the message box's text.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageRevision {
    pub changed: u64,
    pub emptied: u64,
}

impl MessageRevision {
    pub fn edited(&mut self, empty: bool) {
        self.changed = self.changed.wrapping_add(1);
        if empty {
            self.emptied = self.changed;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManualChoice {
    include: bool,
    emptied_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageSwitch {
    seen: HashSet<String>,
    revision_at_request: u64,
    manual: Option<ManualChoice>,
}

impl MessageSwitch {
    pub fn requests(&mut self, ids: HashSet<String>, revision: MessageRevision) {
        if ids.iter().any(|id| !self.seen.contains(id)) {
            self.revision_at_request = revision.changed;
            self.manual = None;
        }
        self.seen = ids;
    }

    pub fn includes(&self, revision: MessageRevision) -> bool {
        if let Some(choice) = &self.manual
            && choice.emptied_at == revision.emptied
        {
            return choice.include;
        }
        self.revision_at_request != revision.changed
    }

    pub fn flip(&mut self, revision: MessageRevision) {
        self.manual = Some(ManualChoice {
            include: !self.includes(revision),
            emptied_at: revision.emptied,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_request_resets_the_choice_but_removing_one_keeps_it() {
        let mut switch = MessageSwitch::default();
        let mut revision = MessageRevision::default();
        switch.requests(HashSet::from(["a1".into()]), revision);
        revision.edited(false);
        assert!(switch.includes(revision));
        switch.flip(revision);
        revision.edited(false);
        assert!(!switch.includes(revision));
        revision.edited(true);
        revision.edited(false);
        assert!(switch.includes(revision));
        switch.requests(HashSet::from(["a1".into(), "a2".into()]), revision);
        assert!(!switch.includes(revision));
        switch.flip(revision);
        switch.requests(HashSet::from(["a1".into()]), revision);
        assert!(switch.includes(revision));
    }
}
