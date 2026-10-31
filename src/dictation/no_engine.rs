//! The service in a build without the `dictation` feature: it opens
//! nothing, and says this build has no dictation.

use std::convert::Infallible;
use std::path::Path;

use super::models::{GRANITE, SpeechModel};
use super::{DictationError, DictationOptions, Sink, Source};

pub struct DictationService;

impl DictationService {
    pub fn new(_root: &Path) -> Self {
        DictationService
    }

    pub fn shared(_root: &Path) -> &'static DictationService {
        &DictationService
    }

    pub fn model(&self) -> &'static SpeechModel {
        &GRANITE
    }

    pub fn open(
        &self,
        _options: DictationOptions,
        _sink: Sink,
    ) -> Result<Dictation, DictationError> {
        Err(DictationError::Unavailable)
    }
}

/// Never made in this build.
pub struct Dictation(Infallible);

impl Dictation {
    pub fn id(&self) -> u64 {
        match self.0 {}
    }

    pub fn push(&self, _samples: &[f32], _rate: u32) {
        match self.0 {}
    }

    pub fn record(&self, _source: &Source) -> Result<Capture, String> {
        match self.0 {}
    }

    pub fn finish(self) {
        match self.0 {}
    }

    pub fn cancel(self) {
        match self.0 {}
    }
}

/// Never made in this build.
pub struct Capture(Infallible);
