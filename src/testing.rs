//! For tests only (the `testing` feature): a local model server that
//! answers as a test scripts it, and the client for it, which
//! [`Options::client`](crate::Options::client) gives a session in place of
//! the one its credentials would make. Not for builds that ship.

pub use crate::mock_model::*;
pub use crate::options::GivenClient;
/// Stand-ins for the models that name sessions, which name those in the
/// Arko folder they are set for ([`name_sessions_with`]).
pub use crate::session_names::fake::{
    Fake as NamingFake, Step as NamingStep, name_with as name_sessions_with,
};
