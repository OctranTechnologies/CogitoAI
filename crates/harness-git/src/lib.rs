use std::path::PathBuf;

use harness_core::{CheckpointId, Error, SessionId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub session_id: SessionId,
    pub working_directory: PathBuf,
    pub reference: String,
}

pub trait CheckpointStore: Send + Sync {
    fn create(
        &self,
        session_id: &SessionId,
        working_directory: &std::path::Path,
    ) -> Result<Checkpoint, Error>;
    fn restore(&self, checkpoint: &Checkpoint) -> Result<(), Error>;
}
