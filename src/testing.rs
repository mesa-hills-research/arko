//! For tests only (the `testing` feature): a local model server that
//! answers as a test scripts it, and the client for it, which
//! [`Options::client`](crate::Options::client) gives a session in place of
//! the one its credentials would make. Not for builds that ship.

pub use crate::mock_model::*;
pub use crate::options::GivenClient;
/// A session's feed at random, from a seed, and a client of a feed reduced
/// to what the feed promises it, for testing a client of the feed. A client
/// that starts over from a snapshot at any moment should end where one
/// that followed every update does.
pub use crate::remote::random_session::{FeedMirror, RandomSession};
/// Stand-ins for the models that name sessions, which name those in the
/// Arko folder they are set for ([`name_sessions_with`]).
pub use crate::session_names::fake::{
    Fake as NamingFake, Step as NamingStep, name_with as name_sessions_with,
};
