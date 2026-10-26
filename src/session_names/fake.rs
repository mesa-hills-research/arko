//! A stand-in for the models that name sessions, for tests. Each way does as
//! its steps say, and what was asked is kept. It names the sessions of the
//! Arko folder it is set for, whichever thread they run on.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;

use super::Way;
use super::naming::{Ask, Failure};

static FAKES: LazyLock<Mutex<HashMap<PathBuf, Arc<dyn Ask>>>> = LazyLock::new(Mutex::default);

/// Makes `fake` name the sessions in Arko's folder `root`.
pub(crate) fn name_with(root: &Path, fake: Arc<Fake>) {
    FAKES.lock().unwrap().insert(root.to_path_buf(), fake);
}

/// What names the sessions in `root`, if a test set one.
pub(super) fn asker_for(root: &Path) -> Option<Arc<dyn Ask>> {
    FAKES.lock().unwrap().get(root).cloned()
}

/// What a way does when asked.
#[derive(Clone)]
pub(crate) enum Step {
    Answer(&'static str),
    /// Answers once the gate opens.
    Gated(Arc<tokio::sync::Notify>, &'static str),
    /// Fails once the gate opens.
    GatedFail(Arc<tokio::sync::Notify>),
    Fail,
    Unavailable,
    Hang,
}

/// Each way does as its steps say, one for each time it is asked (the last
/// again and again).
#[derive(Default)]
pub(crate) struct Fake {
    steps: Mutex<HashMap<Way, VecDeque<Step>>>,
    pub(crate) asked: Mutex<Vec<(Way, String)>>,
}

impl Fake {
    pub(crate) fn new(steps: &[(Way, Step)]) -> Arc<Self> {
        let fake = Arc::new(Self::default());
        for (way, step) in steps {
            fake.set(*way, step.clone());
        }
        fake
    }

    /// What `way` does from now on.
    pub(crate) fn set(&self, way: Way, step: Step) {
        self.steps
            .lock()
            .unwrap()
            .insert(way, VecDeque::from([step]));
    }

    /// What `way` does the next time it is asked, then what it did before.
    pub(crate) fn next(&self, way: Way, step: Step) {
        self.steps
            .lock()
            .unwrap()
            .entry(way)
            .or_default()
            .push_front(step);
    }

    pub(crate) fn ways_asked(&self) -> Vec<Way> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|(way, _)| *way)
            .collect()
    }
}

#[async_trait]
impl Ask for Fake {
    async fn ask(&self, way: Way, message: &str) -> Result<String, Failure> {
        self.asked.lock().unwrap().push((way, message.to_string()));
        let step = {
            let mut steps = self.steps.lock().unwrap();
            let queue = steps.entry(way).or_default();
            if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            }
        };
        match step {
            Some(Step::Answer(text)) => Ok(text.to_string()),
            Some(Step::Gated(gate, text)) => {
                gate.notified().await;
                Ok(text.to_string())
            }
            Some(Step::GatedFail(gate)) => {
                gate.notified().await;
                Err(Failure::Failed("it failed".into()))
            }
            Some(Step::Fail) => Err(Failure::Failed("it failed".into())),
            Some(Step::Hang) => std::future::pending().await,
            Some(Step::Unavailable) | None => Err(Failure::Unavailable("not here".into())),
        }
    }
}
