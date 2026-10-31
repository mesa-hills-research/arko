//! A page for tests, on the test's thread, whose effects run when the test
//! says.

use std::cell::RefCell;

use any_spawner::{CustomExecutor, Executor, PinnedFuture, PinnedLocalFuture};
use futures::executor::{LocalPool, LocalSpawner};
use futures::task::LocalSpawnExt;

use crate::app::Page;
use crate::layout::View;

thread_local! {
    static POOL: RefCell<LocalPool> = RefCell::new(LocalPool::new());
    static SPAWNER: LocalSpawner = POOL.with(|pool| pool.borrow().spawner());
}

/// Keeps the effects a test's thread spawns until [`run_effects`].
struct OnThisThread;

impl CustomExecutor for OnThisThread {
    fn spawn(&self, future: PinnedFuture<()>) {
        self.spawn_local(future);
    }

    fn spawn_local(&self, future: PinnedLocalFuture<()>) {
        SPAWNER.with(|spawner| spawner.spawn_local(future).expect("the pool runs"));
    }

    fn poll_local(&self) {
        run_effects();
    }
}

/// The page, showing the Full view.
pub fn page() -> Page {
    // (Each test's thread keeps its own. The first one sets the executor for
    // all, so the others' say it's set already.)
    let _ = Executor::init_local_custom_executor(OnThisThread);
    Page::with(View::Full, 0.0)
}

/// Runs the effects that are due.
pub fn run_effects() {
    POOL.with(|pool| {
        if let Ok(mut pool) = pool.try_borrow_mut() {
            pool.run_until_stalled();
        }
    });
}
