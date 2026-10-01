pub mod bootstrap;
pub mod client;
pub mod host;
pub mod metadata;
pub mod process;
pub mod protocol;
pub mod server;
pub mod settings;

pub use bootstrap::{ConnectionState, ConnectionStatus, RuntimeConnectError, RuntimeConnector};
/// Name emphasizing the client-facing lifecycle manager role of the shared
/// runtime connector. Kept as an alias so existing embedders remain compatible.
pub type HarnessConnectionManager = RuntimeConnector;
pub use client::{RpcClient, RpcClientError, RpcClientReader, RpcClientWriter};
pub use harness_pty::{
    ExitReason, PtyError, PtyInfo, PtyManager, PtyRequest, SessionOrigin, TerminalEvent,
    TerminalSink,
};
pub use host::{
    serve_runtime_process, EmbeddedRuntimeLauncher, RuntimeLaunchConfig, RuntimeLaunchInfo,
    RuntimeLauncher,
};
pub use metadata::{default_runtime_directory, RuntimeMetadata, RuntimeMetadataStore};
pub use process::ProcessRuntimeLauncher;
pub use protocol::{
    RpcError, RpcNotification, RpcRequest, RpcResponse, ServerMessage, RPC_PROTOCOL_VERSION,
};
pub use server::{ApprovalBroker, RpcApprovalHandler, RpcServer, RpcServerError};
pub use settings::{
    ConnectionTestResult, CredentialSecret, CredentialSource, CredentialStatus,
    EnvironmentSecretStore, ModelSettingsView, PermissionSettingsView, ProjectSettingsView,
    ProviderCredentialRequest, ProviderCredentialView, ProviderDisconnectRequest,
    RuntimeSettingsView, SecretStore, SettingsError, SettingsSnapshot, UpdateModelRequest,
    UpdatePermissionsRequest, VerificationSettingsView,
};

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use harness_agent::{AgentOutcome, AgentRunner, AgentTask};
use harness_core::{AgentRuntime, Error, RunOutcome, RunRequest, SessionId};
use harness_git::{Checkpoint, CheckpointInfo, CheckpointStore, GitError, RestoreReport};
use harness_models::{
    has_model_environment_override, load_project_model_preference, load_user_model_preference,
    CredentialStore, ModelConfig, ModelPreference, ModelPreferenceError, ModelProvider,
    ModelRegistry, SystemCredentialStore,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::{
    EventPayload, HarnessEvent, Session, SessionLoadReport, SessionState, SessionStore,
    SessionSummary,
};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest, ToolResult};
use harness_verification::{VerificationReport, VerificationRequest, Verifier};

pub struct Runtime {
    instance_id: String,
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
    session_root: Option<PathBuf>,
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
    /// Provider-neutral catalog used by RPC and CLI clients. Discovery stays
    /// in the runtime process, and its last successful result is workspace cached.
    model_registry: Arc<ModelRegistry>,
    /// Privileged environment/keychain access. Secret values are never passed
    /// to the frontend or session store.
    credential_store: Arc<dyn CredentialStore>,
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
        let model = ModelConfig::from_env();
        let credential_store: Arc<dyn CredentialStore> = Arc::new(SystemCredentialStore::new());
        let model_registry = Arc::new(ModelRegistry::with_builtins_and_credentials(
            &model,
            None,
            credential_store.as_ref(),
        ));
        Self {
            instance_id: metadata::new_instance_id(),
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
            session_root: None,
            // Retargeted by `with_workspace_root`; the placeholder root is
            // replaced before any terminal can be opened.
            ptys: Arc::new(Mutex::new(PtyManager::new("."))),
            model: RwLock::new(model),
            model_registry,
            credential_store,
        }
    }

    /// Unique to this runtime process instance, used by local discovery to
    /// distinguish it from a stale record whose PID may have been reused.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn with_instance_id(mut self, instance_id: String) -> Self {
        self.instance_id = instance_id;
        self
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
        let _ = self.model_registry.register_configured_model(&model);
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
        self.model_registry
            .set_cache_path(workspace_root.join(".cogito/model-catalog.json"));
        self.workspace_root = Some(workspace_root);
        self
    }

    /// Overrides the durable session directory reported to RPC clients.
    pub fn with_session_root(mut self, session_root: PathBuf) -> Self {
        self.session_root = Some(session_root);
        self
    }

    pub fn session_root(&self) -> Option<&std::path::Path> {
        self.session_root.as_deref()
    }

    /// Shared model catalog. Clients request list/refresh through RPC; they
    /// never call provider endpoints themselves.
    pub fn model_registry(&self) -> Arc<ModelRegistry> {
        Arc::clone(&self.model_registry)
    }

    pub fn with_credential_store(mut self, store: Arc<dyn CredentialStore>) -> Self {
        let model = self.model();
        let cache_path = self
            .workspace_root
            .as_ref()
            .map(|root| root.join(".cogito/model-catalog.json"));
        self.model_registry = Arc::new(ModelRegistry::with_builtins_and_credentials(
            &model,
            cache_path,
            store.as_ref(),
        ));
        self.credential_store = store;
        self
    }

    pub fn credential_store(&self) -> Arc<dyn CredentialStore> {
        Arc::clone(&self.credential_store)
    }

    /// Rebuilds the active runner after credential storage changes so provider
    /// adapters pick up the newly connected or disconnected key.
    pub fn refresh_agent_runner(&self) -> Result<(), Error> {
        let factory = self
            .runner_factory
            .read()
            .expect("runner factory lock poisoned")
            .clone();
        let Some(factory) = factory else {
            return Ok(());
        };
        let runner = factory.build(&self.model(), self.execution_mode())?;
        *self
            .agent_runner
            .write()
            .expect("agent runner lock poisoned") = Some(runner);
        Ok(())
    }

    /// Applies the saved user default followed by the workspace's project
    /// override. Explicit model environment configuration remains authoritative.
    pub fn apply_saved_model_preferences(&self) -> Result<(), Error> {
        if has_model_environment_override() {
            return Ok(());
        }
        let original = self.model();
        let mut preferred = original.clone();
        if let Some(preference) = load_user_model_preference().map_err(preference_error)? {
            preference
                .apply_to(&mut preferred)
                .map_err(preference_error)?;
        }
        if let Some(root) = &self.workspace_root {
            if let Some(preference) =
                load_project_model_preference(root).map_err(preference_error)?
            {
                preference
                    .apply_to(&mut preferred)
                    .map_err(preference_error)?;
            }
        }
        if preferred != original {
            self.apply_settings(preferred, self.execution_mode())?;
        }
        Ok(())
    }

    /// Persists a model-selection event after confirming the session belongs to
    /// this runtime's workspace.
    pub fn record_model_change(&self, session: &str, model: &ModelConfig) -> Result<(), Error> {
        let session_id = SessionId::new(session.to_owned())?;
        let existing = self.sessions.load(&session_id)?;
        if let Some(workspace_root) = &self.workspace_root {
            let workspace_root = std::fs::canonicalize(workspace_root)?;
            if existing.workspace_root != workspace_root {
                return Err(Error::InvalidRequest {
                    reason: "session belongs to a different workspace".to_owned(),
                });
            }
        }
        let reasoning_effort = model.reasoning_effort.and_then(|effort| {
            serde_json::to_value(effort)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
        });
        self.sessions.append_event(
            &session_id,
            HarnessEvent::new(
                session_id.clone(),
                EventPayload::ModelChanged {
                    provider: model_provider_id(model.provider).to_owned(),
                    model: model.model.clone(),
                    reasoning_effort,
                },
                None,
                None,
            ),
        )
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
            cancellation: None,
            event_bus: None,
            session_id: None,
            correlation_id: None,
        };
        self.tools.execute(&context, request)
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|provider| provider.descriptor().provider)
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
        let existing = self.sessions.load(session_id)?;
        let preference = existing.events.iter().rev().find_map(|event| {
            if let EventPayload::ModelChanged {
                provider,
                model,
                reasoning_effort,
            } = &event.payload
            {
                let effort = reasoning_effort.as_ref().and_then(|value| {
                    serde_json::from_value(serde_json::Value::String(value.clone())).ok()
                });
                ModelPreference::from_canonical_id(&format!("{provider}/{model}"), effort).ok()
            } else {
                None
            }
        });
        let resumed = self.sessions.resume(session_id)?;
        if let Some(preference) = preference {
            let mut model = self.model();
            preference.apply_to(&mut model).map_err(preference_error)?;
            if model != self.model() {
                self.apply_settings(model, self.execution_mode())?;
            }
        } else {
            self.apply_saved_model_preferences()?;
        }
        Ok(resumed)
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

fn preference_error(error: ModelPreferenceError) -> Error {
    Error::InvalidConfig {
        reason: error.to_string(),
    }
}

fn model_provider_id(provider: harness_models::ProviderKind) -> &'static str {
    match provider {
        harness_models::ProviderKind::Mock => "mock",
        harness_models::ProviderKind::OpenAi => "openai",
        harness_models::ProviderKind::Anthropic => "anthropic",
        harness_models::ProviderKind::Gemini => "gemini",
        harness_models::ProviderKind::OpenCodeZen => "opencode-zen",
        harness_models::ProviderKind::OpenCodeGo => "opencode-go",
    }
}
