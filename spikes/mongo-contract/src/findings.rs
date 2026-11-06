//! Friction recorder. R-029 treats the spike as unfinished until it has produced
//! at least one finding, so findings are emitted by the code that actually ran
//! against mongod rather than written up from memory afterwards.

use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The contract expressed this cleanly.
    Held,
    /// Expressible, but only by stretching the contract's intent.
    Strained,
    /// The contract cannot express this; a change is required.
    Broke,
}

impl Fit {
    pub fn tag(&self) -> &'static str {
        match self {
            Fit::Held => "HELD",
            Fit::Strained => "STRAINED",
            Fit::Broke => "BROKE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub scenario: &'static str,
    pub fit: Fit,
    /// Which part of the contract this is about.
    pub surface: &'static str,
    pub detail: String,
}

#[derive(Clone, Default)]
pub struct Recorder {
    inner: Arc<Mutex<Vec<Finding>>>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, scenario: &'static str, fit: Fit, surface: &'static str, detail: impl Into<String>) {
        self.inner.lock().unwrap().push(Finding {
            scenario,
            fit,
            surface,
            detail: detail.into(),
        });
    }

    pub fn all(&self) -> Vec<Finding> {
        self.inner.lock().unwrap().clone()
    }
}
