//! Who made a change, and when. Shared by undo history and annotations, so
//! the model does not have to depend on undo internals to record provenance.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Where a change or annotation came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Actor {
    #[default]
    Human,
    Agent(String),
    Analysis(String),
}

/// When and by whom a change was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub actor: Actor,
    /// Unix epoch milliseconds.
    pub at: u64,
}

impl Provenance {
    pub(crate) fn now(actor: &Actor) -> Self {
        Self {
            actor: actor.clone(),
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}
