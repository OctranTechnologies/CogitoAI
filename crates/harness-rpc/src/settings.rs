//! Typed, secret-safe configuration for the desktop settings surfaces.
//!
//! # Secrets
//!
//! Credentials are **never** returned to a client. This module deliberately
//! reports only whether a credential is available and where it came from
//! ([`CredentialStatus`]), never the value. That holds for
//! [`SettingsSnapshot`], for every update response, and for error messages.
//!
//! Secrets are read through a [`SecretStore`]. The only implementation shipped
//! is [`EnvironmentSecretStore`], which reads an environment variable named by
//! the model settings. An operating-system keychain backend can be added
//! later without changing any frontend code, because clients only ever see
//! [`CredentialStatus`].
//!
//! No home-grown encryption is implemented here on purpose: an unverified
//! cipher is worse than an honest environment variable, since it looks like
//! protection without providing it.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use harness_core::discover_workspace;
use harness_models::{ModelCapabilities, ModelConfig, ProviderKind};
use harness_policy::{ExecutionMode, OperationKind, PolicyDecision, PolicyEngine, PolicyRule};
use serde::{Deserialize, Serialize};

use crate::Runtime;

/// Version reported by the runtime settings surface.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// Read from the process environment.
    Environment,
}

impl CredentialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
        }
    }
}

/// Whether a credential is available, and where from.
///
/// This is the only credential information a client ever receives. There is no
/// field, and no method, that can carry the secret itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CredentialStatus {
    /// True when a non-empty credential is available to the runtime.
    pub available: bool,
    pub source: CredentialSource,
    /// Name of the environment variable holding the credential.
    pub env_var: String,
}

impl CredentialStatus {
    /// Redacted summary suitable for logs and the UI.
    pub fn summary(&self) -> String {
        if self.available {
            format!("configured from {}", self.env_var)
        } else {
            format!("not set (set {})", self.env_var)
        }
    }
}

/// Supplies credentials to the runtime without ever revealing them.
pub trait SecretStore: Send + Sync {
    /// Returns true when a non-empty credential exists for `env_var`.
    fn is_available(&self, env_var: &str) -> bool;
    /// The credential for `env_var`, for internal use only.
    ///
    /// Callers must not log, serialize, or return this value.
    fn get(&self, env_var: &str) -> Option<String>;
    fn source(&self) -> CredentialSource;
}

/// Reads credentials from the process environment.
///
/// This is the only backend shipped. It is intentionally simple and honest: the
/// secret is never copied into runtime configuration, never serialized, and
/// never returned to a client.
#[derive(Debug, Default)]
pub struct EnvironmentSecretStore;

impl SecretStore for EnvironmentSecretStore {
    fn is_available(&self, env_var: &str) -> bool {
        std::env::var(env_var)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    }

    fn get(&self, env_var: &str) -> Option<String> {
        std::env::var(env_var)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    fn source(&self) -> CredentialSource {
        CredentialSource::Environment
    }
}

/// Redacts anything that looks like a credential in free-form text.
///
/// Applied to every error surfaced to a client so a provider that echoes a key
/// back cannot leak it into the UI or a log line.
pub fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut redacted = text.to_owned();
    for secret in secrets {
        if secret.len() >= 8 && redacted.contains(secret.as_str()) {
            redacted = redacted.replace(secret.as_str(), "[redacted]");
        }
    }
    redacted
}

/// Everything a client needs to render the Models screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelSettingsView {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub capabilities: ModelCapabilities,
    pub credential: CredentialStatus,
    /// Models this provider is known to support.
    pub available_models: Vec<String>,
    pub configured: bool,
}

/// One effective policy rule, described for a human reader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuleSummary {
    pub name: String,
    pub action: String,
    pub reason: String,
    pub tools: Vec<String>,
    pub operations: Vec<String>,
}

/// What the active mode does for each kind of operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub operation: String,
    pub effect: String,
}

/// Everything a client needs to render the Permissions screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionSettingsView {
    pub mode: String,
    pub mode_description: String,
    pub available_modes: Vec<String>,
    /// Built-in rules that always apply, in evaluation order.
    pub built_in_rules: Vec<RuleSummary>,
    /// Rules loaded from project configuration.
    pub configured_rules: Vec<RuleSummary>,
    pub default_behavior: Vec<OperationSummary>,
}

/// Everything a client needs to render the Project screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettingsView {
    pub workspace_path: String,
    pub repository_root: Option<String>,
    pub is_git_repository: bool,
    pub languages: Vec<String>,
    pub manifests: Vec<String>,
    pub package_manager: Option<String>,
    pub instruction_files: Vec<String>,
    pub monorepo: bool,
}

/// Everything a client needs to render the Verification screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationSettingsView {
    /// Commands the runtime will run for verification.
    pub commands: Vec<VerificationCommand>,
    /// Where those commands came from.
    pub source: Option<String>,
    /// True when the project overrides the built-in defaults.
    pub has_project_overrides: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationCommand {
    pub category: String,
    pub program: String,
    pub args: Vec<String>,
    /// True when explicitly configured rather than auto-detected.
    pub is_override: bool,
}

/// Everything a client needs to render the Runtime screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSettingsView {
    pub version: String,
    pub session_storage_path: String,
    pub checkpoint_storage_path: String,
    pub log_level: String,
    pub log_target: String,
    pub provider_names: Vec<String>,
    pub credential_source: String,
}

/// The full settings payload returned by `settings.inspect`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub models: ModelSettingsView,
    pub permissions: PermissionSettingsView,
    pub project: ProjectSettingsView,
    pub verification: VerificationSettingsView,
    pub runtime: RuntimeSettingsView,
}

/// A typed request to change the selected model.
///
/// Optional fields are left unchanged when absent.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdateModelRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// A typed request to change the permission mode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdatePermissionsRequest {
    pub mode: String,
}

/// The outcome of a provider connectivity check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectionTestResult {
    pub ok: bool,
    /// Human-readable summary. Never contains a credential.
    pub message: String,
    /// True when the test could not run because no credential is configured.
    pub skipped: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("invalid setting: {0}")]
    Invalid(String),
    #[error("unknown provider '{0}'")]
    UnknownProvider(String),
    #[error("unknown execution mode '{0}'")]
    UnknownMode(String),
    #[error("runtime cannot apply settings: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

/// Known models per provider, used for the picker without contacting a network.
fn available_models(provider: &ProviderKind) -> Vec<String> {
    match provider {
        ProviderKind::Mock => vec!["mock".to_owned()],
        ProviderKind::OpenAi => vec![
            "gpt-4o".to_owned(),
            "gpt-4o-mini".to_owned(),
            "gpt-4.1".to_owned(),
            "gpt-4.1-mini".to_owned(),
            "o3-mini".to_owned(),
        ],
    }
}

fn parse_provider(value: &str) -> Result<ProviderKind, SettingsError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mock" => Ok(ProviderKind::Mock),
        "openai" | "open_ai" | "open-ai" => Ok(ProviderKind::OpenAi),
        other => Err(SettingsError::UnknownProvider(other.to_owned())),
    }
}

fn provider_label(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "Mock (offline)",
        ProviderKind::OpenAi => "OpenAI",
    }
}

fn parse_mode(value: &str) -> Result<ExecutionMode, SettingsError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "readonly" | "read_only" | "read-only" => Ok(ExecutionMode::ReadOnly),
        "safe" => Ok(ExecutionMode::Safe),
        "normal" => Ok(ExecutionMode::Normal),
        "auto" => Ok(ExecutionMode::Auto),
        other => Err(SettingsError::UnknownMode(other.to_owned())),
    }
}

fn mode_name(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::ReadOnly => "read_only",
        ExecutionMode::Safe => "safe",
        ExecutionMode::Normal => "normal",
        ExecutionMode::Auto => "auto",
    }
}

fn mode_description(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::ReadOnly => {
            "Reading and searching only. Every write, patch, or command is denied."
        }
        ExecutionMode::Safe => {
            "Reading and searching only. Anything that changes the workspace needs your approval."
        }
        ExecutionMode::Normal => "Safe commands run directly; anything else asks before it runs.",
        ExecutionMode::Auto => {
            "Writes run directly and network access asks. Commands still follow the command rules."
        }
    }
}

fn operation_name(operation: &OperationKind) -> String {
    format!("{operation:?}").to_ascii_lowercase()
}

/// Describes the built-in rules that always apply, in evaluation order.
fn built_in_rules() -> Vec<RuleSummary> {
    vec![
        RuleSummary {
            name: "workspace-boundary".to_owned(),
            action: "deny".to_owned(),
            reason: "Paths outside the open workspace are always denied.".to_owned(),
            tools: Vec::new(),
            operations: Vec::new(),
        },
        RuleSummary {
            name: "high-risk-path".to_owned(),
            action: "deny".to_owned(),
            reason: "Credential and VCS secret paths are always denied.".to_owned(),
            tools: Vec::new(),
            operations: Vec::new(),
        },
    ]
}

fn summarize_rules(rules: &[PolicyRule]) -> Vec<RuleSummary> {
    rules
        .iter()
        .map(|rule| RuleSummary {
            name: rule.name.clone(),
            action: format!("{:?}", rule.action).to_ascii_lowercase(),
            reason: match rule.action {
                PolicyDecision::Allow => "Allowed when this rule matches.".to_owned(),
                PolicyDecision::Ask => "Needs your approval when this rule matches.".to_owned(),
                PolicyDecision::Deny => "Denied when this rule matches.".to_owned(),
            },
            tools: rule.tools.clone().unwrap_or_default(),
            operations: rule
                .operations
                .as_ref()
                .map(|operations| {
                    operations
                        .iter()
                        .map(operation_name)
                        .map(|name| name.to_owned())
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// The behaviour of each operation when no configured rule matches.
fn default_behavior(mode: ExecutionMode) -> Vec<OperationSummary> {
    let effect = |decision: PolicyDecision| -> String {
        match decision {
            PolicyDecision::Allow => "allowed".to_owned(),
            PolicyDecision::Ask => "asks first".to_owned(),
            PolicyDecision::Deny => "denied".to_owned(),
        }
    };
    let command = if mode == ExecutionMode::Normal || mode == ExecutionMode::Auto {
        "known-safe commands run, others ask".to_owned()
    } else {
        "asks first".to_owned()
    };
    vec![
        OperationSummary {
            operation: "read".to_owned(),
            effect: effect(PolicyDecision::Allow),
        },
        OperationSummary {
            operation: "search".to_owned(),
            effect: effect(PolicyDecision::Allow),
        },
        OperationSummary {
            operation: "write".to_owned(),
            effect: if mode == ExecutionMode::ReadOnly {
                effect(PolicyDecision::Deny)
            } else {
                effect(PolicyDecision::Allow)
            },
        },
        OperationSummary {
            operation: "patch".to_owned(),
            effect: if mode == ExecutionMode::ReadOnly {
                effect(PolicyDecision::Deny)
            } else {
                effect(PolicyDecision::Allow)
            },
        },
        OperationSummary {
            operation: "command".to_owned(),
            effect: if mode == ExecutionMode::ReadOnly {
                effect(PolicyDecision::Deny)
            } else {
                command
            },
        },
        OperationSummary {
            // Network access always asks, in every mode except read-only, which
            // denies it outright.
            operation: "network".to_owned(),
            effect: if mode == ExecutionMode::ReadOnly {
                effect(PolicyDecision::Deny)
            } else {
                effect(PolicyDecision::Ask)
            },
        },
    ]
}

/// Parses an execution mode name as advertised by the Permissions screen.
///
/// Exposed so the advertised names and the modes the runtime accepts can be
/// checked against each other rather than assumed to line up.
pub fn parse_execution_mode(value: &str) -> Result<ExecutionMode, SettingsError> {
    parse_mode(value)
}

/// The execution mode name a client should send for `mode`.
pub fn execution_mode_name(mode: ExecutionMode) -> &'static str {
    mode_name(mode)
}

/// The mode names the Permissions screen offers.
pub fn advertised_execution_modes() -> Vec<String> {
    ["read_only", "safe", "normal", "auto"]
        .iter()
        .map(|value| (*value).to_owned())
        .collect()
}

/// Reports whether the selected model has a usable credential, without
/// ever reading the value into anything a client can observe.
pub fn credential_status(model: &ModelConfig, store: &dyn SecretStore) -> CredentialStatus {
    CredentialStatus {
        available: store.is_available(&model.api_key_env),
        source: store.source(),
        env_var: model.api_key_env.clone(),
    }
}

/// Capabilities the runtime knows about for the selected provider.
fn capabilities(model: &ModelConfig) -> ModelCapabilities {
    match model.provider {
        ProviderKind::Mock => ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
            reasoning: false,
            context_window: model.context_window,
        },
        ProviderKind::OpenAi => ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            reasoning: model.model.starts_with("o"),
            context_window: model.context_window,
        },
    }
}

/// Assembles the Models screen.
pub fn model_view(model: &ModelConfig, store: &dyn SecretStore) -> ModelSettingsView {
    let credential = credential_status(model, store);
    ModelSettingsView {
        provider: provider_label(&model.provider).to_owned(),
        model: model.model.clone(),
        base_url: model.base_url.clone(),
        api_key_env: model.api_key_env.clone(),
        capabilities: capabilities(model),
        // The mock provider needs no credential, so it is always "configured".
        configured: match model.provider {
            ProviderKind::Mock => true,
            ProviderKind::OpenAi => credential.available,
        },
        available_models: available_models(&model.provider),
        credential,
    }
}

/// Assembles the Permissions screen.
pub fn permission_view(runtime: &Runtime) -> PermissionSettingsView {
    let mode = runtime.execution_mode();
    let root = runtime
        .workspace_root()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Re-derive the configured rules from the project policy file so the screen
    // reflects what the engine actually loaded, not just the mode name.
    let configured_rules = PolicyEngine::from_file(&root.join(".agent/policy.toml"), &root)
        .map(|engine| summarize_rules(engine.configured_rules()))
        .unwrap_or_default();
    PermissionSettingsView {
        mode: mode_name(mode).to_owned(),
        mode_description: mode_description(mode).to_owned(),
        available_modes: advertised_execution_modes(),
        built_in_rules: built_in_rules(),
        configured_rules,
        default_behavior: default_behavior(mode),
    }
}

/// Assembles the Project screen from the discovered workspace.
pub fn project_view(
    workspace_path: &std::path::Path,
) -> Result<ProjectSettingsView, SettingsError> {
    let description = discover_workspace(workspace_path)
        .map_err(|error| SettingsError::Internal(error.to_string()))?;
    let manifests = description
        .manifests
        .iter()
        .map(|manifest| {
            manifest
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| manifest.path.to_string_lossy().into_owned())
        })
        .collect();
    Ok(ProjectSettingsView {
        workspace_path: description.current_directory.to_string_lossy().into_owned(),
        repository_root: description
            .repository_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned()),
        is_git_repository: description.repository_root.is_some(),
        languages: description
            .languages
            .iter()
            .map(|language| format!("{language:?}"))
            .collect(),
        manifests,
        package_manager: description
            .configuration
            .package_manager
            .map(|manager| format!("{manager:?}").to_ascii_lowercase()),
        instruction_files: description
            .instructions
            .iter()
            .map(|instruction| {
                let name = instruction
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| instruction.path.to_string_lossy().into_owned());
                format!("{:?} · {}", instruction.kind, name)
            })
            .collect(),
        monorepo: description.monorepo.is_monorepo,
    })
}

/// Assembles the Verification screen.
pub fn verification_view(
    workspace_path: &std::path::Path,
) -> Result<VerificationSettingsView, SettingsError> {
    let description = discover_workspace(workspace_path)
        .map_err(|error| SettingsError::Internal(error.to_string()))?;
    let source = description.configuration.source.clone();
    let has_overrides = source.is_some();
    let commands = description.configuration.commands.clone();
    let mut listed: Vec<VerificationCommand> = Vec::new();
    for (category, commands) in [
        ("test", &commands.test),
        ("build", &commands.build),
        ("format", &commands.format),
        ("lint", &commands.lint),
        ("typecheck", &commands.typecheck),
    ] {
        // Each category holds zero or more specs; only the first is reported,
        // because verification runs one command per category.
        let Some(spec) = commands.first() else {
            continue;
        };
        listed.push(VerificationCommand {
            category: category.to_owned(),
            program: spec.program.clone(),
            args: spec.args.clone(),
            is_override: has_overrides,
        });
    }
    Ok(VerificationSettingsView {
        commands: listed,
        source: source.map(|path| path.to_string_lossy().into_owned()),
        has_project_overrides: has_overrides,
    })
}

/// Assembles the Runtime screen.
pub fn runtime_view(
    runtime: &Runtime,
    store: &dyn SecretStore,
    session_root: &std::path::Path,
) -> RuntimeSettingsView {
    let workspace = runtime
        .workspace_root()
        .unwrap_or(std::path::Path::new("."));
    RuntimeSettingsView {
        version: RUNTIME_VERSION.to_owned(),
        session_storage_path: session_root.to_string_lossy().into_owned(),
        checkpoint_storage_path: workspace
            .join(".cogito/checkpoints")
            .to_string_lossy()
            .into_owned(),
        log_level: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
        log_target: "runtime stdout".to_owned(),
        provider_names: runtime
            .provider_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        credential_source: store.source().as_str().to_owned(),
    }
}

/// Builds the complete settings snapshot.
pub fn snapshot(
    runtime: &Runtime,
    store: &dyn SecretStore,
    session_root: &std::path::Path,
) -> Result<SettingsSnapshot, SettingsError> {
    let model = runtime.model();
    let workspace = runtime
        .workspace_root()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(SettingsSnapshot {
        models: model_view(&model, store),
        permissions: permission_view(runtime),
        project: project_view(&workspace)?,
        verification: verification_view(&workspace)?,
        runtime: runtime_view(runtime, store, session_root),
    })
}

/// Applies a model change, validating before anything is mutated.
pub fn apply_model(
    runtime: &Runtime,
    request: &UpdateModelRequest,
) -> Result<ModelConfig, SettingsError> {
    let mut next = runtime.model();
    if let Some(provider) = &request.provider {
        next.provider = parse_provider(provider)?;
    }
    if let Some(model) = &request.model {
        next.model = model.trim().to_owned();
    }
    if let Some(base_url) = &request.base_url {
        next.base_url = base_url.trim().to_owned();
    }
    if let Some(api_key_env) = &request.api_key_env {
        next.api_key_env = api_key_env.trim().to_owned();
    }
    next.validate().map_err(SettingsError::Invalid)?;
    let mode = runtime.execution_mode();
    runtime
        .apply_settings(next.clone(), mode)
        .map_err(|error| SettingsError::Unavailable(error.to_string()))?;
    Ok(next)
}

/// Applies a permission-mode change, validating the mode first.
pub fn apply_permissions(
    runtime: &Runtime,
    request: &UpdatePermissionsRequest,
) -> Result<ExecutionMode, SettingsError> {
    let mode = parse_mode(&request.mode)?;
    let model = runtime.model();
    runtime
        .apply_settings(model, mode)
        .map_err(|error| SettingsError::Unavailable(error.to_string()))?;
    Ok(mode)
}

/// Reports whether a provider is reachable, without exposing any credential.
///
/// The check is deliberately shallow: it confirms the runtime is configured and
/// that a credential exists. It never performs a request that could echo a key
/// into an error message, and the message is redacted regardless.
pub fn test_model_connection(model: &ModelConfig, store: &dyn SecretStore) -> ConnectionTestResult {
    let credential = credential_status(model, store);
    if let Err(reason) = model.validate() {
        return ConnectionTestResult {
            ok: false,
            skipped: false,
            message: redact_secrets(&format!("Model configuration is invalid: {reason}"), &[]),
        };
    }
    match model.provider {
        ProviderKind::Mock => ConnectionTestResult {
            ok: true,
            skipped: false,
            message: "The mock provider runs locally; no connection is required.".to_owned(),
        },
        ProviderKind::OpenAi => {
            if !credential.available {
                return ConnectionTestResult {
                    ok: false,
                    skipped: true,
                    message: format!(
                        "No credential found. Set {} in the runtime environment.",
                        credential.env_var
                    ),
                };
            }
            ConnectionTestResult {
                ok: true,
                skipped: false,
                message: format!("Ready. The runtime can read {}.", credential.env_var),
            }
        }
    }
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Builds a secret store for the runtime, defaulting to the environment.
pub fn default_secret_store() -> Arc<dyn SecretStore> {
    Arc::new(EnvironmentSecretStore)
}
