//! What the threads of one session's runtime share when several sessions
//! run in one process (`arko::spawn`): the session's ID, which tags its log
//! lines, and its settings' [`Env`]. Each thread of the runtime enters the
//! context as it starts, so code anywhere in the session (the framework's
//! too) finds it without being handed it. A process with one session, the
//! command line, has none: the process's settings are the session's.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::credentials::Env;

/// One session's context.
#[derive(Debug, Default)]
pub(crate) struct SessionContext {
    /// Its ID, once it is known (a new session's, once it is created).
    id: OnceLock<String>,
    /// Its settings and keys, once its folder is known.
    env: Mutex<Option<Env>>,
    /// The work it started that goes on after it closes.
    lingering: Lingering,
}

/// How much work of a session goes on after it closes (a model naming it),
/// which the thread that runs the session waits for before it stops its
/// runtime.
#[derive(Debug, Default)]
struct Lingering {
    count: AtomicUsize,
    done: tokio::sync::Notify,
}

/// Work of a session that goes on after it closes, for as long as this is
/// held.
#[derive(Debug)]
pub(crate) struct Linger(Arc<SessionContext>);

impl Drop for Linger {
    fn drop(&mut self) {
        self.0.lingering.count.fetch_sub(1, Ordering::SeqCst);
        self.0.lingering.done.notify_waiters();
    }
}

impl SessionContext {
    /// Marks work that goes on after the session closes, until the result
    /// is dropped.
    pub(crate) fn linger(self: &Arc<Self>) -> Linger {
        self.lingering.count.fetch_add(1, Ordering::SeqCst);
        Linger(self.clone())
    }

    /// Waits until no work lingers, or `limit` has passed.
    pub(crate) async fn lingered(&self, limit: std::time::Duration) {
        let wait = async {
            loop {
                let done = self.lingering.done.notified();
                if self.lingering.count.load(Ordering::SeqCst) == 0 {
                    return;
                }
                done.await;
            }
        };
        let _ = tokio::time::timeout(limit, wait).await;
    }
}

thread_local! {
    static CONTEXT: RefCell<Option<Arc<SessionContext>>> = const { RefCell::new(None) };
}

/// This thread works for the session of `context` from now on.
pub(crate) fn enter(context: Arc<SessionContext>) {
    CONTEXT.with(|current| *current.borrow_mut() = Some(context));
}

/// The context of the session this thread works for, if any.
fn current() -> Option<Arc<SessionContext>> {
    CONTEXT.with(|current| current.borrow().clone())
}

/// The ID of the session this thread works for: `Some("")` while the
/// session opens and has none yet, `None` outside a session's runtime.
pub(crate) fn session_id() -> Option<String> {
    current().map(|context| context.id.get().cloned().unwrap_or_default())
}

/// The session this thread works for is `id`, when it runs in a context.
pub(crate) fn name_session(id: &str) {
    if let Some(context) = current() {
        let _ = context.id.set(id.to_string());
    }
}

/// The session's settings, when this thread works for a session that has
/// them.
pub(crate) fn env() -> Option<Env> {
    current().and_then(|context| crate::lock(&context.env).clone())
}

/// Runs `work` on this thread as a session with the settings `env` would
/// run it (a session not yet opened, whose folder is known), then goes back
/// to this thread's own context.
pub(crate) fn with_env<T>(env: Env, work: impl FnOnce() -> T) -> T {
    let context = SessionContext::default();
    *crate::lock(&context.env) = Some(env);
    within(Some(Arc::new(context)), work)
}

/// The context of the session this thread works for, to do its work from
/// another thread later ([`within`]).
pub(crate) fn handle() -> Option<Arc<SessionContext>> {
    current()
}

/// Runs `work` on this thread for the session of `context` (its settings,
/// its log tag), then goes back to this thread's own context. `None` (a
/// process with one session): as the process.
pub(crate) fn within<T>(context: Option<Arc<SessionContext>>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<SessionContext>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTEXT.with(|current| *current.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CONTEXT.with(|current| current.replace(context)));
    work()
}

/// Sets the session's settings, when this thread works for a session in a
/// context. Returns whether it did (else they are the process's).
pub(crate) fn set_env(env: &Env) -> bool {
    match current() {
        Some(context) => {
            *crate::lock(&context.env) = Some(env.clone());
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A session's thread waits for the work that lingers after it closes,
    /// and no longer than the limit.
    #[tokio::test]
    async fn work_that_lingers_is_waited_for_up_to_a_limit() {
        let context = Arc::new(SessionContext::default());
        // Nothing lingers: no wait.
        context.lingered(Duration::from_secs(5)).await;
        let linger = context.linger();
        let waiting = tokio::spawn({
            let context = context.clone();
            async move { context.lingered(Duration::from_secs(30)).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "it waits while the work goes on");
        drop(linger);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("it ends with the work")
            .unwrap();
        // Work that doesn't end is waited for up to the limit only.
        let _forever = context.linger();
        let started = std::time::Instant::now();
        context.lingered(Duration::from_millis(100)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
