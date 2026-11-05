//! What the reader opened (Quiet rows, calls in Full, calls inside a Quiet
//! row, contents whole), by item. A row follows its own item alone, so
//! opening one wakes that row and not every row of a long transcript.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;

/// The items opened, of one kind of fold.
#[derive(Clone, Copy)]
pub struct Opened(StoredValue<Inner>);

#[derive(Default)]
struct Inner {
    open: HashSet<u64>,
    /// The trigger of each item a row follows.
    triggers: HashMap<u64, ArcTrigger>,
}

impl Opened {
    pub fn new() -> Self {
        Opened(StoredValue::new(Inner::default()))
    }

    /// Whether `id` is open, followed by the caller for that item alone.
    pub fn contains(&self, id: u64) -> bool {
        self.0.update_value(|inner| {
            inner
                .triggers
                .entry(id)
                .or_insert_with(ArcTrigger::new)
                .track();
        });
        self.contains_untracked(id)
    }

    pub fn contains_untracked(&self, id: u64) -> bool {
        self.0
            .try_with_value(|inner| inner.open.contains(&id))
            .unwrap_or(false)
    }

    /// Opens `id`, or closes it.
    pub fn set(&self, id: u64, open: bool) {
        let trigger = self
            .0
            .try_update_value(|inner| {
                let changed = if open {
                    inner.open.insert(id)
                } else {
                    inner.open.remove(&id)
                };
                changed.then(|| inner.triggers.get(&id).cloned()).flatten()
            })
            .flatten();
        if let Some(trigger) = trigger {
            trigger.notify();
        }
    }

    /// Opens `id` if it's closed, else closes it. Returns whether it's open.
    pub fn toggle(&self, id: u64) -> bool {
        let open = !self.contains_untracked(id);
        self.set(id, open);
        open
    }

    /// The page let go of the item `id`. Its rows go with it, so nothing
    /// is told.
    pub fn forget(&self, id: u64) {
        self.0.try_update_value(|inner| {
            inner.open.remove(&id);
            inner.triggers.remove(&id);
        });
    }

    /// Whether anything is kept of `id`, open or followed.
    #[cfg(test)]
    pub fn keeps(&self, id: u64) -> bool {
        self.0
            .with_value(|inner| inner.open.contains(&id) || inner.triggers.contains_key(&id))
    }

    /// Closes everything, as another feed's items have other IDs.
    pub fn clear(&self) {
        let triggers = self
            .0
            .try_update_value(|inner| {
                inner.open.clear();
                std::mem::take(&mut inner.triggers)
            })
            .unwrap_or_default();
        for trigger in triggers.into_values() {
            trigger.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening or closing an item wakes what follows that item alone, and an
    /// item let go of leaves no trigger behind.
    #[test]
    fn an_item_opened_wakes_its_own_rows_alone() {
        let owner = Owner::new();
        owner.with(|| {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicUsize, Ordering};
            let opened = Opened::new();
            let runs = Arc::new(AtomicUsize::new(0));
            let row = |id: u64| {
                let runs = runs.clone();
                Memo::new(move |_| {
                    runs.fetch_add(1, Ordering::Relaxed);
                    opened.contains(id)
                })
            };
            let (one, two) = (row(1), row(2));
            assert!(!one.get() && !two.get());
            assert_eq!(runs.load(Ordering::Relaxed), 2);

            assert!(opened.toggle(1));
            assert!(one.get() && !two.get());
            assert_eq!(runs.load(Ordering::Relaxed), 3);
            // Opening what is open already changes nothing.
            opened.set(1, true);
            assert!(one.get());
            assert_eq!(runs.load(Ordering::Relaxed), 3);
            assert!(!opened.toggle(1));
            assert!(!one.get() && !two.get());
            assert_eq!(runs.load(Ordering::Relaxed), 4);

            opened.set(2, true);
            two.dispose();
            opened.forget(2);
            assert!(!opened.contains_untracked(2));
            assert!(
                opened
                    .0
                    .with_value(|inner| !inner.triggers.contains_key(&2))
            );
            // Another feed closes everything, and what's shown hears it.
            opened.set(1, true);
            assert!(one.get());
            opened.clear();
            assert!(!one.get());
        });
    }
}
