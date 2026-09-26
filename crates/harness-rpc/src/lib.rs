pub mod client;
pub mod protocol;
pub mod server;
pub mod settings;

pub use client::{RpcClient, RpcClientError, RpcClientReader, RpcClientWriter};
pub use harness_pty::{
    ExitReason, PtyError, PtyInfo, PtyManager, PtyRequest, SessionOrigin, TerminalEvent,
    TerminalSink,
};
pub use protocol::{
    RpcError, RpcNotification, RpcRequest, RpcResponse, ServerMessage, RPC_PROTOCOL_VERSION,
};
pub use server::{ApprovalBroker, RpcApprovalHandler, RpcServer, RpcServerError};
pub use settings::{
    ConnectionTestResult, CredentialSource, CredentialStatus, EnvironmentSecretStore,
    ModelSettingsView, PermissionSettingsView, ProjectSettingsView, RuntimeSettingsView,
    SecretStore, SettingsError, SettingsSnapshot, UpdateModelRequest, UpdatePermissionsRequest,
    VerificationSettingsView,
};

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use harness_agent::{AgentOutcome, AgentRunner, AgentTask};
use harness_core::{AgentRuntime, Error, RunOutcome, RunRequest};
use harness_git::{Checkpoint, CheckpointInfo, CheckpointStore, GitError, RestoreReport};
use harness_models::{ModelConfig, ModelProvider};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::{
    HarnessEvent, Session, SessionLoadReport, SessionState, SessionStore, SessionSummary,
};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest, ToolResult};
use harness_verification::{VerificationReport, VerificationRequest, Verifier};

pub struct Runtime {
    agent: Arc<dyn AgentRuntime>,
    providers: Vec<Box<dyn ModelProvider>>,
    tools: ToolRegistry,
    /// Execution policy, replaceable at runtime when the user changes the
    /// permission mode.
    policy: Arc<RwLock<Arc<dyn Policy>>>,
    sessions: Arc<dyn SessionStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    verifiers: Vec<Arc<dyn Verifier>>,
    /// The active runner, rebuilt when model or permission settings change.
    agent_runner: RwLock<Option<Arc<AgentRunner>>>,
    /// Builds a runner for a given model and execution mode, so settings changes
    /// take effect without restarting the process.
    runner_factory: RwLock<Option<Arc<dyn AgentRunnerFactory>>>,
    workspace_root: Option<PathBuf>,
    /// Human-operated terminals.
    ///
    /// These are deliberately separate from `tools`: the agent reaches shell
    /// execution only through `ToolRegistry`, which evaluates every call against
    /// the policy engine. A terminal is interactive, ungoverned, and reachable
    /// only from an RPC client acting for a person. See the `harness-pty` module
    /// documentation for the full boundary.
    ptys: Arc<Mutex<PtyManager>>,
    /// Model selection, owned by the runtime so the desktop can read and change
    /// it without reaching into provider internals.
    model: RwLock<ModelConfig>,
}

/// Builds an [`AgentRunner`] for a selected model and execution mode.
///
/// The runtime stores this so that changing a setting in the desktop can
/// rebuild the runner in place; without it, model or permission changes would
/// require restarting the process.
pub trait AgentRunnerFactory: Send + Sync {
    fn build(&self, model: &ModelConfig, mode: ExecutionMode) -> Result<Arc<AgentRunner>, Error>;
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
            policy: Arc::new(RwLock::new(policy)),
            sessions,
            checkpoints,
            verifiers,
            agent_runner: RwLock::new(None),
            runner_factory: RwLock::new(None),
            workspace_root: None,
            // Retargeted by `with_workspace_root`; the placeholder root is
            // replaced before any terminal can be opened.
            ptys: Arc::new(Mutex::new(PtyManager::new("."))),
            model: RwLock::new(ModelConfig::from_env()),
        }
    }

    pub fn with_agent_runner(self, agent_runner: Arc<AgentRunner>) -> Self {
        *self
            .agent_runner
            .write()
            .expect("agent runner lock poisoned") = Some(agent_runner);
        self
    }

    /// Registers the factory used to rebuild the runner when settings change.
    pub fn with_runner_factory(self, factory: Arc<dyn AgentRunnerFactory>) -> Self {
        *self
            .runner_factory
            .write()
            .expect("runner factory lock poisoned") = Some(factory);
        self
    }

    /// The policy currently in force.
    pub fn policy(&self) -> Arc<dyn Policy> {
        Arc::clone(&self.policy.read().expect("policy lock poisoned"))
    }

    /// The execution mode currently in force.
    pub fn execution_mode(&self) -> ExecutionMode {
        self.policy().mode()
    }

    /// The model currently selected.
    pub fn model(&self) -> ModelConfig {
        self.model.read().expect("model lock poisoned").clone()
    }

    /// Applies a new model selection and execution mode.
    ///
    /// The runner and policy are rebuilt together so both the model and the
    /// permission mode take effect on the next run. Returns an error when no
    /// factory is registered, because silently ignoring the change would leave
    /// the desktop showing settings that are not actually in force.
    pub fn apply_settings(&self, model: ModelConfig, mode: ExecutionMode) -> Result<(), Error> {
        model
            .validate()
            .map_err(|reason| Error::InvalidConfig { reason })?;
        let factory = self
            .runner_factory
            .read()
            .expect("runner factory lock poisoned")
            .clone();
        let Some(factory) = factory else {
            return Err(Error::InvalidConfig {
                reason: "this runtime cannot change model or permission settings at runtime"
                    .to_owned(),
            });
        };
        let runner = factory.build(&model, mode)?;
        *self.model.write().expect("model lock poisoned") = model;
        *self.policy.write().expect("policy lock poisoned") = Arc::new(PolicyEngine::new(
            mode,
            self.workspace_root.clone().unwrap_or_default(),
        ));
        *self
            .agent_runner
            .write()
            .expect("agent runner lock poisoned") = Some(runner);
        Ok(())
    }

    pub fn with_workspace_root(mut self, workspace_root: PathBuf) -> Self {
        // Terminals are confined to the open workspace, so the manager is
        // recreated against the final root.
        *self.ptys.lock().expect("PTY manager lock poisoned") = PtyManager::new(&workspace_root);
        self.workspace_root = Some(workspace_root);
        self
    }

    /// Human-operated terminal sessions for the open workspace.
    pub fn ptys(&self) -> std::sync::MutexGuard<'_, PtyManager> {
        self.ptys.lock().expect("PTY manager lock poisoned")
    }

    pub fn run_agent(
        &self,
        task: &AgentTask,
        cancellation: &harness_tools::CancellationToken,
    ) -> Result<AgentOutcome, harness_agent::AgentError> {
        self.current_runner()
            .ok_or_else(|| {
                harness_agent::AgentError::Core("agent runtime is not configured".to_owned())
            })?
            .run(task, cancellation)
    }

    fn current_runner(&self) -> Option<Arc<AgentRunner>> {
        self.agent_runner
            .read()
            .expect("agent runner lock poisoned")
            .clone()
    }

    pub fn agent_event_bus(&self) -> Option<harness_session::EventBus> {
        self.current_runner().map(|runner| runner.event_bus())
    }

    pub fn workspace_root(&self) -> Option<&std::path::Path> {
        self.workspace_root.as_deref()
    }

    pub fn run(&self, request: RunRequest) -> Result<RunOutcome, Error> {
        self.agent.run(request)
    }

    pub fn execute_tool(
        &self,
        working_directory: &std::path::Path,
        request: ToolRequest,
    ) -> Result<ToolResult, Error> {
        let policy = self.policy.read().expect("policy lock poisoned").clone();
        let context = ToolContext {
            policy: policy.as_ref(),
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

    pub fn inspect_session(
        &self,
        session_id: &harness_core::SessionId,
    ) -> Result<SessionLoadReport, Error> {
        self.sessions.load_with_report(session_id)
    }

    pub fn resume_session(&self, session_id: &harness_core::SessionId) -> Result<Session, Error> {
        self.sessions.resume(session_id)
    }

    pub fn session_state(
        &self,
        session_id: &harness_core::SessionId,
    ) -> Result<SessionState, Error> {
        self.sessions.load(session_id)?.state()
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
