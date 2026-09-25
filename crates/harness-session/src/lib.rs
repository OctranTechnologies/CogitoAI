use std::path::PathBuf;

use harness_core::{Error, Id, RunId, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionEvent {
    RunStarted { run_id: RunId },
    Message { text: String },
    RunCompleted { run_id: RunId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_root: PathBuf,
    pub events: Vec<SessionEvent>,
}

impl Session {
    pub fn new(id: SessionId, workspace_root: PathBuf) -> Self {
        Self {
            id,
            workspace_root,
            events: Vec::new(),
        }
    }
}

pub trait SessionStore: Send + Sync {
    fn save(&self, session: &Session) -> Result<(), Error>;
    fn load(&self, id: &Id) -> Result<Session, Error>;
    fn append_event(&mut self, id: &Id, event: SessionEvent) -> Result<(), Error>;
}
