use serde::{Deserialize, Serialize};

use crate::{Error, RunId, SessionId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub prompt: String,
}

impl RunRequest {
    pub fn new(
        run_id: RunId,
        session_id: SessionId,
        prompt: impl Into<String>,
    ) -> Result<Self, Error> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(Error::InvalidRequest {
                reason: "prompt must not be empty".to_owned(),
            });
        }
        Ok(Self {
            run_id,
            session_id,
            prompt,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunEvent {
    Started { run_id: RunId },
    Message { text: String },
    Completed { run_id: RunId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub events: Vec<RunEvent>,
}

pub trait AgentRuntime: Send + Sync {
    fn run(&self, request: RunRequest) -> Result<RunOutcome, Error>;
}
