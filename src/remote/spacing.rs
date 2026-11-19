//! The space between the transcript's rows, one table for the terminal, the
//! web page and Studio.
//!
//! A row has a [`Class`] by what it draws, and the space above it is the
//! [`Step`] that [`gap`] gives for the row before it. The header above the
//! first row and the composer below the last are places too, and each stands
//! where a row of its neighbour's kind would. Each surface only says how big a
//! step is: rows in the terminal ([`Step::rows`]), CSS pixels on the page
//! ([`Step::px`]), and in Studio the same pixels as rem at 16 px (stack 0.25,
//! apart 0.75, turn 1.5).
//!
//! The steps are between line boxes and the edges of what is drawn. Where an
//! edge is a line of text, the page adds that line's half-leading at the
//! header and the composer, so they stand where the next line's em box would.
//! The composer's edge is the top of the line drawn above it (its border),
//! where the end of a transcript that follows meets the view's bottom edge.
//!
//! Every match here names every class, so a new kind of row doesn't compile
//! until its spacing is decided.

/// What a row is, for the space around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// The user's message, sent or waiting to be read (a note).
    User,
    /// What is said: the agent's text and reply, a message between agents,
    /// a question card, a command's output, a worker's report.
    Said,
    /// A worker's card, a box of a few lines. (The terminal draws a worker
    /// as a line, which is `Work`.)
    Card,
    /// A closed line of work: a call, a Quiet row, a turn's count, a closed
    /// thought, a notice, "Working…", a worker's row in its lead's view.
    Work,
    /// A line of work that is open, with what it holds.
    Open,
    /// What an open line holds when it's a row of its own (a call's content
    /// in Full, under the call).
    Held,
    /// The line that ends a turn, with its time and cost.
    TurnEnd,
}

/// What stands above a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Above {
    /// The header, above the first row.
    Start,
    Row(Class),
}

/// What stands below a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Below {
    Row(Class),
    /// The composer, below the last row.
    Composer,
}

/// The space between two places, from the smallest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// Nothing between them (an empty transcript).
    None,
    /// Between the lines of one list of work, and between a line and what
    /// it holds.
    Stack,
    /// Around what is said and around a card, and below anything open.
    Apart,
    /// One exchange and the next: before the user's message, and after the
    /// line that ends a turn.
    Turn,
}

impl Step {
    /// In CSS pixels, on the page.
    pub fn px(self) -> u16 {
        match self {
            Step::None => 0,
            Step::Stack => 4,
            Step::Apart => 12,
            Step::Turn => 24,
        }
    }

    /// In blank rows, in the terminal. A row is about 20 px, so a list's
    /// lines touch, and an exchange stands a row apart as a paragraph does
    /// (the user's bar sets it apart, as the bubble does on the page).
    pub fn rows(self) -> usize {
        match self {
            Step::None | Step::Stack => 0,
            Step::Apart | Step::Turn => 1,
        }
    }

    /// Its name, as the page's rows carry it (`data-gap`).
    pub fn name(self) -> &'static str {
        match self {
            Step::None => "none",
            Step::Stack => "stack",
            Step::Apart => "apart",
            Step::Turn => "turn",
        }
    }
}

/// The space between `above` and `below`.
pub fn gap(above: Above, below: Below) -> Step {
    match (above, below) {
        (Above::Start, Below::Composer) => Step::None,
        // The header, where a row of the first row's kind would stand above
        // it, as that row stands closed, so opening it moves nothing.
        (Above::Start, Below::Row(class)) => between(closed(class), closed(class)),
        // The composer, where a row of the last row's kind would stand below
        // it.
        (Above::Row(class), Below::Composer) => between(class, next_of(class)),
        (Above::Row(above), Below::Row(below)) => between(above, below),
    }
}

/// The space between two rows.
fn between(above: Class, below: Class) -> Step {
    use Class::{Card, Held, Open, Said, TurnEnd, User, Work};
    match (above, below) {
        // An exchange starts with the user's message.
        (User | Said | Card | Work | Open | Held | TurnEnd, User) => Step::Turn,
        // And the next one after the line that ends a turn.
        (TurnEnd, Said | Card | Work | Open | Held | TurnEnd) => Step::Turn,
        // A list of work runs together, and a line runs into what it holds.
        // An open row has the space above it that it has closed. (What a
        // line holds shows only under an open line.)
        (Work, Work | Open | Held) => Step::Stack,
        (Open, Held) => Step::Stack,
        // What is said and cards stand apart, and so does anything below an
        // open row.
        (User | Said | Card, Said | Card | Work | Open | Held | TurnEnd) => Step::Apart,
        (Work, Said | Card | TurnEnd) => Step::Apart,
        (Open, Said | Card | Work | Open | TurnEnd) => Step::Apart,
        (Held, Said | Card | Work | Open | Held | TurnEnd) => Step::Apart,
    }
}

/// A row as it stands closed, for the space above it.
fn closed(class: Class) -> Class {
    match class {
        Class::User => Class::User,
        Class::Said => Class::Said,
        Class::Card => Class::Card,
        Class::Work | Class::Open | Class::Held => Class::Work,
        Class::TurnEnd => Class::TurnEnd,
    }
}

/// The row the composer stands in for below a row of `class`: one of its
/// kind. (What an open line holds is followed as the open line is.)
fn next_of(class: Class) -> Class {
    match class {
        Class::User => Class::User,
        Class::Said => Class::Said,
        Class::Card => Class::Card,
        Class::Work => Class::Work,
        Class::Open | Class::Held => Class::Open,
        Class::TurnEnd => Class::TurnEnd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Class::{Card, Held, Open, Said, TurnEnd, User, Work};

    const CLASSES: [Class; 7] = [User, Said, Card, Work, Open, Held, TurnEnd];

    fn rows(above: Class, below: Class) -> Step {
        gap(Above::Row(above), Below::Row(below))
    }

    /// Every pair is the larger of the steps its two kinds own, each kind
    /// one above it and one below it, but for a line and what it holds,
    /// which run together though an open line owns apart below it.
    #[test]
    fn each_pair_is_the_larger_step_of_its_two_kinds() {
        let above = |class| match class {
            User => Step::Turn,
            Said | Card | TurnEnd => Step::Apart,
            Work | Open | Held => Step::Stack,
        };
        let below = |class| match class {
            User | Said | Card | Open | Held => Step::Apart,
            Work => Step::Stack,
            TurnEnd => Step::Turn,
        };
        for upper in CLASSES {
            for lower in CLASSES {
                let want = if (upper, lower) == (Open, Held) {
                    Step::Stack
                } else {
                    below(upper).max(above(lower))
                };
                assert_eq!(rows(upper, lower), want, "{upper:?} above {lower:?}");
            }
        }
    }

    #[test]
    fn work_stacks_and_what_is_said_stands_apart() {
        assert_eq!(rows(Work, Work), Step::Stack);
        assert_eq!(rows(Work, Said), Step::Apart);
        assert_eq!(rows(Said, Work), Step::Apart);
        assert_eq!(rows(Said, Said), Step::Apart);
        assert_eq!(rows(Card, Work), Step::Apart);
        assert_eq!(rows(Open, Held), Step::Stack);
        assert_eq!(rows(Held, Work), Step::Apart);
        assert_eq!(rows(Open, Work), Step::Apart);
    }

    /// An exchange starts with the user's message and ends with the turn's
    /// line, which belongs to its turn.
    #[test]
    fn an_exchange_starts_with_the_users_message_and_ends_with_its_turns_line() {
        for class in CLASSES {
            assert_eq!(rows(class, User), Step::Turn, "{class:?}");
            assert_eq!(rows(TurnEnd, class), Step::Turn, "{class:?}");
        }
        assert_eq!(rows(Said, TurnEnd), Step::Apart);
        assert_eq!(rows(Work, TurnEnd), Step::Apart);
        assert_eq!(rows(User, Said), Step::Apart);
    }

    /// Related rows never stand further apart than unrelated ones.
    #[test]
    fn the_steps_grow_in_order() {
        assert!(Step::None.px() < Step::Stack.px());
        assert!(Step::Stack.px() < Step::Apart.px());
        assert!(Step::Apart.px() < Step::Turn.px());
        assert!(Step::Stack.rows() < Step::Apart.rows());
        assert!(Step::Apart.rows() <= Step::Turn.rows());
    }

    /// Opening a row never changes the space above it, the first row's
    /// included.
    #[test]
    fn opening_a_row_moves_nothing_above_it() {
        for class in CLASSES {
            assert_eq!(rows(class, Work), rows(class, Open), "{class:?}");
        }
        assert_eq!(
            gap(Above::Start, Below::Row(Work)),
            gap(Above::Start, Below::Row(Open))
        );
    }

    /// The composer stands where a row of the last row's kind would, and the
    /// header where one of the first row's kind would.
    #[test]
    fn the_edges_stand_where_a_row_of_the_same_kind_would() {
        for class in CLASSES {
            assert_eq!(
                gap(Above::Row(class), Below::Composer),
                rows(class, next_of(class)),
                "{class:?}"
            );
            assert_eq!(
                gap(Above::Start, Below::Row(class)),
                rows(closed(class), closed(class)),
                "{class:?}"
            );
        }
        assert_eq!(gap(Above::Row(Work), Below::Composer), Step::Stack);
        assert_eq!(gap(Above::Row(Said), Below::Composer), Step::Apart);
        assert_eq!(gap(Above::Row(Open), Below::Composer), Step::Apart);
        assert_eq!(gap(Above::Row(User), Below::Composer), Step::Turn);
        assert_eq!(gap(Above::Row(TurnEnd), Below::Composer), Step::Turn);
        assert_eq!(gap(Above::Start, Below::Row(User)), Step::Turn);
        assert_eq!(gap(Above::Start, Below::Row(Said)), Step::Apart);
        assert_eq!(gap(Above::Start, Below::Composer), Step::None);
    }

    /// In the terminal, a blank row stands between rows unless both are
    /// work, none between a line and what it holds, and one below anything
    /// open, as before the table. (It has no cards and no turn's line.)
    #[test]
    fn the_terminals_rows_between_rows() {
        let terminal = [User, Said, Work, Open, Held];
        let work = |class| matches!(class, Work | Open);
        for upper in terminal {
            for lower in terminal {
                if lower == Held && upper != Open {
                    continue;
                }
                let blank = match (upper, lower) {
                    (Open, Held) => 0,
                    (Open, _) | (Held, _) => 1,
                    (a, b) if work(a) && work(b) => 0,
                    _ => 1,
                };
                assert_eq!(
                    rows(upper, lower).rows(),
                    blank,
                    "{upper:?} above {lower:?}"
                );
            }
        }
    }
}
