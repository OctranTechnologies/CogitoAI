use std::sync::Arc;

use harness_core::{AgentRuntime, Error, RunOutcome, RunRequest};
use harness_git::{Checkpoint, CheckpointInfo, CheckpointStore, GitError, RestoreReport};
use harness_models::ModelProvider;
use harness_policy::Policy;
use harness_session::{HarnessEvent, Session, SessionStore, SessionSummary};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest, ToolResult};
use harness_verification::{VerificationReport, VerificationRequest, Verifier};

pub struct Runtime {
    agent: Arc<dyn AgentRuntime>,
    providers: Vec<Box<dyn ModelProvider>>,
    tools: ToolRegistry,
    policy: Arc<dyn Policy>,
    sessions: Arc<dyn SessionStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    verifiers: Vec<Arc<dyn Verifier>>,
}

impl Runtime {
    pub fn new(
        agent: Arc<dyn AgentRuntime>,
        providers: Vec<Box<dyn ModelProvider>>,
        tools: ToolRegistry,
        policy: Arc<dyn Policy>,
        sessions: Arc<dyn SessionStore>,
        checkpoints: Arc<dyn CheckpointStore>,
        verifiers: Vec<Arc<dyn Verifier>>,
    ) -> Self {
        Self {
            agent,
            providers,
            tools,
            policy,
            sessions,
            checkpoints,
            verifiers,
        }
    }

    pub fn run(&self, request: RunRequest) -> Result<RunOutcome, Error> {
        self.agent.run(request)
    }

    pub fn execute_tool(
        &self,
        working_directory: &std::path::Path,
        request: ToolRequest,
    ) -> Result<ToolResult, Error> {
        let context = ToolContext {
            policy: self.policy.as_ref(),
            working_directory,
            event_bus: None,
            session_id: None,
            correlation_id: None,
        };
        self.tools.execute(&context, request)
    }

    pub fn provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .map(|provider| provider.name())
            .collect()
    }

    pub fn create_session(&self, workspace_root: &std::path::Path) -> Result<Session, Error> {
        self.sessions.create(workspace_root)
    }

    pub fn append_session_event(
        &self,
        session_id: &harness_core::SessionId,
        event: HarnessEvent,
    ) -> Result<(), Error> {
        self.sessions.append_event(session_id, event)
    }

    pub fn load_session(&self, session_id: &harness_core::SessionId) -> Result<Session, Error> {
        self.sessions.load(session_id)
    }

    pub fn recent_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>, Error> {
        self.sessions.recent(limit)
    }

    pub fn create_checkpoint(
        &self,
        session_id: &harness_core::SessionId,
        working_directory: &std::path::Path,
    ) -> Result<Checkpoint, GitError> {
        self.checkpoints.create(session_id, working_directory)
    }

    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>, GitError> {
        self.checkpoints.list()
    }

    pub fn inspect_checkpoint(
        &self,
        checkpoint_id: &harness_core::CheckpointId,
    ) -> Result<CheckpointInfo, GitError> {
        self.checkpoints.inspect(checkpoint_id)
    }

    pub fn record_checkpoint_change(
        &self,
        checkpoint_id: &harness_core::CheckpointId,
        path: &std::path::Path,
    ) -> Result<(), GitError> {
        self.checkpoints.record_harness_change(checkpoint_id, path)
    }

    pub fn restore_checkpoint(&self, checkpoint: &Checkpoint) -> Result<RestoreReport, GitError> {
        self.checkpoints.restore(checkpoint)
    }

    pub fn undo_checkpoint(
        &self,
        checkpoint_id: &harness_core::CheckpointId,
    ) -> Result<RestoreReport, GitError> {
        self.checkpoints.undo(checkpoint_id)
    }

    pub fn verify(&self, request: &VerificationRequest) -> Result<Vec<VerificationReport>, Error> {
        let mut reports = Vec::new();
        for verifier in &self.verifiers {
            reports.extend(verifier.verify(request)?);
        }
        Ok(reports)
    }
}
