//! Typed, secret-safe configuration for the desktop settings surfaces.
//!
//! # Secrets
//!
//! Credentials are **never** returned to a client. This module deliberately
//! reports only whether a credential is available and where it came from
//! ([`CredentialStatus`]), never the value. That holds for
//! [`SettingsSnapshot`], for every update response, and for error messages.
//!
//! Secrets are read environment-first through a [`SecretStore`]. Interactive
//! credentials are stored by the OS credential manager; clients only ever see
//! [`CredentialStatus`], never values. No plaintext project configuration or
//! home-grown encryption is used.

use std::path::PathBuf;
use std::sync::Arc;

use harness_core::discover_workspace;
use harness_models::{
    provider_from_config, save_project_model_preference, save_user_model_preference,
    validate_provider_credential, CredentialError, CredentialStore, ModelCapabilities, ModelConfig,
    ModelPreference, ModelRegistry, ModelRegistryFilter, ProviderKind, ReasoningEffort,
    SystemCredentialStore,
};
use harness_policy::{ExecutionMode, OperationKind, PolicyDecision, PolicyEngine, PolicyRule};
use serde::{Deserialize, Serialize};

use crate::Runtime;

pub use harness_models::CredentialStore as SecretStore;
pub use harness_models::EnvironmentCredentialStore as EnvironmentSecretStore;
pub use harness_models::{
    CredentialSecret, CredentialSource, CredentialStatus, EnvironmentCredentialStore,
    KeychainBackend,
};

/// Version reported by the runtime settings surface.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Redacts anything that looks like a credential in free-form text.
///
/// Applied to every error surfaced to a client so a provider that echoes a key
/// back cannot leak it into the UI or a log line.
pub fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut redacted = harness_core::redact_sensitive(text);
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
    pub provider_id: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub capabilities: ModelCapabilities,
    pub credential: CredentialStatus,
    /// Catalogued or manually configured model IDs for this provider.
    pub available_models: Vec<String>,
    /// Reasoning levels advertised for this exact selected model. Empty means
    /// the runtime cannot safely offer a configurable effort control.
    pub reasoning_levels: Vec<String>,
    pub reasoning_effort: Option<String>,
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
    pub credentials: Vec<ProviderCredentialView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCredentialView {
    pub provider_id: String,
    pub provider: String,
    pub credential: CredentialStatus,
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
    pub reasoning_effort: Option<String>,
    /// `user`, `project`, `session`, or `none`. Omitted defaults to `none` for
    /// older clients that only change the in-memory runtime selection.
    pub preference_scope: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub record_session_event: bool,
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

pub struct ProviderCredentialRequest {
    pub provider_id: String,
    pub api_key: zeroize::Zeroizing<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDisconnectRequest {
    pub provider_id: String,
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

fn parse_provider(value: &str) -> Result<ProviderKind, SettingsError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mock" => Ok(ProviderKind::Mock),
        "openai" | "open_ai" | "open-ai" => Ok(ProviderKind::OpenAi),
        "anthropic" => Ok(ProviderKind::Anthropic),
        "gemini" | "google" | "google_gemini" => Ok(ProviderKind::Gemini),
        "opencode-zen" | "opencode_zen" | "opencodezen" | "opencode" => {
            Ok(ProviderKind::OpenCodeZen)
        }
        "opencode-go" | "opencode_go" | "opencodego" => Ok(ProviderKind::OpenCodeGo),
        other => Err(SettingsError::UnknownProvider(other.to_owned())),
    }
}

fn provider_label(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "Mock (offline)",
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::Gemini => "Google Gemini",
        ProviderKind::OpenCodeZen => "OpenCode Zen",
        ProviderKind::OpenCodeGo => "OpenCode Go",
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
    store
        .status(provider_id(model.provider), &model.api_key_env)
        .unwrap_or(CredentialStatus {
            available: false,
            source: CredentialSource::Unavailable,
            env_var: model.api_key_env.clone(),
        })
}

/// Capabilities the runtime knows about for the selected provider.
fn capabilities(model: &ModelConfig) -> ModelCapabilities {
    provider_from_config(model)
        .map(|provider| provider.descriptor().capabilities)
        .unwrap_or_default()
}

/// Assembles the Models screen.
pub fn model_view(model: &ModelConfig, store: &dyn SecretStore) -> ModelSettingsView {
    model_view_with_registry(model, store, None)
}

fn model_view_with_registry(
    model: &ModelConfig,
    store: &dyn SecretStore,
    registry: Option<&ModelRegistry>,
) -> ModelSettingsView {
    let credential = credential_status(model, store);
    let provider_id = provider_from_config(model)
        .map(|provider| provider.descriptor().provider)
        .unwrap_or_default();
    let available_models = registry
        .map(|registry| {
            registry
                .models(&ModelRegistryFilter {
                    provider_id: Some(provider_id.clone()),
                    requirements: Vec::new(),
                })
                .into_iter()
                .map(|descriptor| descriptor.id)
                .collect()
        })
        .unwrap_or_default();
    let selected_descriptor = registry.and_then(|registry| {
        registry
            .models(&ModelRegistryFilter {
                provider_id: Some(provider_id.clone()),
                requirements: Vec::new(),
            })
            .into_iter()
            .find(|descriptor| descriptor.id == model.model)
    });
    let reasoning_levels = selected_descriptor
        .and_then(|descriptor| descriptor.metadata.reasoning_levels)
        .unwrap_or_default()
        .into_iter()
        .map(reasoning_effort_name)
        .collect();
    ModelSettingsView {
        provider: provider_label(&model.provider).to_owned(),
        provider_id,
        model: model.model.clone(),
        base_url: model.base_url.clone(),
        api_key_env: model.api_key_env.clone(),
        capabilities: capabilities(model),
        // The mock provider needs no credential, so it is always "configured".
        configured: match model.provider {
            ProviderKind::Mock => true,
            ProviderKind::OpenAi
            | ProviderKind::Anthropic
            | ProviderKind::Gemini
            | ProviderKind::OpenCodeZen
            | ProviderKind::OpenCodeGo => credential.available,
        },
        available_models,
        reasoning_levels,
        reasoning_effort: model.reasoning_effort.map(reasoning_effort_name),
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
        provider_names: runtime.provider_names(),
        credential_source: "environment variables, then OS credential store".to_owned(),
        credentials: provider_credentials(runtime, store),
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
        models: model_view_with_registry(&model, store, Some(&runtime.model_registry())),
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
    let current = runtime.model();
    let mut next = current.clone();
    if let Some(provider) = &request.provider {
        let provider = parse_provider(provider)?;
        next.select_provider(provider);
    }
    if let Some(model) = &request.model {
        next.model = model.trim().to_owned();
    }
    if (next.provider != current.provider || next.model != current.model)
        && request.reasoning_effort.is_none()
    {
        next.reasoning_effort = None;
    }
    if let Some(base_url) = &request.base_url {
        next.base_url = base_url.trim().to_owned();
    }
    if let Some(api_key_env) = &request.api_key_env {
        next.api_key_env = api_key_env.trim().to_owned();
    }
    if let Some(effort) = &request.reasoning_effort {
        let provider_id = provider_from_config(&next)
            .map(|provider| provider.descriptor().provider)
            .unwrap_or_default();
        let selected = runtime
            .model_registry()
            .models(&ModelRegistryFilter {
                provider_id: Some(provider_id),
                requirements: Vec::new(),
            })
            .into_iter()
            .find(|descriptor| descriptor.id == next.model);
        let supported = selected
            .as_ref()
            .and_then(|descriptor| descriptor.metadata.reasoning_levels.as_ref())
            .is_some_and(|levels| !levels.is_empty());
        if !supported {
            return Err(SettingsError::Invalid(
                "the selected model does not advertise configurable reasoning levels".to_owned(),
            ));
        }
        next.reasoning_effort = if effort.eq_ignore_ascii_case("off") {
            None
        } else {
            Some(
                serde_json::from_value(serde_json::Value::String(effort.to_ascii_lowercase()))
                    .map_err(|_| SettingsError::Invalid("unsupported reasoning effort".into()))?,
            )
        };
        if let Some(effort) = next.reasoning_effort {
            let allowed = selected
                .and_then(|descriptor| descriptor.metadata.reasoning_levels)
                .is_some_and(|levels| levels.contains(&effort));
            if !allowed {
                return Err(SettingsError::Invalid(
                    "reasoning effort is not supported by the selected model".to_owned(),
                ));
            }
        }
    }
    next.validate().map_err(SettingsError::Invalid)?;
    match request.preference_scope.as_deref().unwrap_or("none") {
        "none" | "user" | "project" | "session" => {}
        scope => {
            return Err(SettingsError::Invalid(format!(
                "unknown model preference scope '{scope}'"
            )))
        }
    }
    if request.record_session_event && request.session_id.is_none() {
        return Err(SettingsError::Invalid(
            "session_id is required to record a model change".to_owned(),
        ));
    }
    let mode = runtime.execution_mode();
    runtime
        .apply_settings(next.clone(), mode)
        .map_err(|error| SettingsError::Unavailable(error.to_string()))?;

    let preference = ModelPreference::from_canonical_id(
        &format!("{}/{}", model_provider_id(next.provider), next.model),
        next.reasoning_effort,
    )
    .map_err(|error| SettingsError::Internal(error.to_string()))?;
    match request.preference_scope.as_deref().unwrap_or("none") {
        "none" => {}
        "user" => save_user_model_preference(&preference)
            .map_err(|error| SettingsError::Internal(error.to_string()))?,
        "project" => {
            let root = runtime.workspace_root().ok_or_else(|| {
                SettingsError::Invalid("project preference needs an open workspace".to_owned())
            })?;
            save_project_model_preference(root, &preference)
                .map_err(|error| SettingsError::Internal(error.to_string()))?;
        }
        "session" => {}
        _ => unreachable!("preference scope validated above"),
    }
    if request.record_session_event {
        let session_id = request.session_id.as_deref().ok_or_else(|| {
            SettingsError::Invalid("session_id is required to record a model change".to_owned())
        })?;
        runtime
            .record_model_change(session_id, &next)
            .map_err(|error| SettingsError::Unavailable(error.to_string()))?;
    }
    Ok(next)
}

fn reasoning_effort_name(effort: ReasoningEffort) -> String {
    serde_json::to_value(effort)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn model_provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
    }
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
    if model.provider == ProviderKind::Mock {
        return ConnectionTestResult {
            ok: true,
            skipped: false,
            message: "The mock provider runs locally; no connection is required.".to_owned(),
        };
    }

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
        message: format!(
            "{} is ready. The runtime can read {}.",
            provider_label(&model.provider),
            credential.env_var
        ),
    }
}

/// Builds a secret store for the runtime, defaulting to the environment.
pub fn default_secret_store() -> Arc<dyn SecretStore> {
    Arc::new(SystemCredentialStore::new())
}

pub fn provider_credentials(
    runtime: &Runtime,
    store: &dyn CredentialStore,
) -> Vec<ProviderCredentialView> {
    [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Gemini,
        ProviderKind::OpenCodeZen,
        ProviderKind::OpenCodeGo,
    ]
    .into_iter()
    .map(|provider| {
        let model = credential_model(runtime, provider);
        ProviderCredentialView {
            provider_id: provider_id(provider).to_owned(),
            provider: provider_label(&provider).to_owned(),
            credential: credential_status(&model, store),
        }
    })
    .collect()
}

pub fn validate_provider_key(
    runtime: &Runtime,
    provider_id: &str,
    api_key: &str,
) -> ConnectionTestResult {
    let model = match credential_model_by_id(runtime, provider_id) {
        Ok(model) => model,
        Err(error) => {
            return ConnectionTestResult {
                ok: false,
                skipped: false,
                message: harness_core::redact_sensitive(&error.to_string()),
            };
        }
    };
    if api_key.trim().is_empty() {
        return ConnectionTestResult {
            ok: false,
            skipped: false,
            message: "Enter a non-empty provider key.".to_owned(),
        };
    }
    match validate_provider_credential(&model, api_key) {
        Ok(()) => ConnectionTestResult {
            ok: true,
            skipped: false,
            message: format!("{} credential validated.", provider_label(&model.provider)),
        },
        Err(error) => ConnectionTestResult {
            ok: false,
            skipped: false,
            message: harness_core::redact_sensitive(&redact_secrets(
                &error.to_string(),
                &[api_key.to_owned()],
            )),
        },
    }
}

pub fn connect_provider(
    runtime: &Runtime,
    store: &dyn CredentialStore,
    provider_id: &str,
    api_key: &str,
) -> Result<ProviderCredentialView, SettingsError> {
    let model = credential_model_by_id(runtime, provider_id)?;
    let validation = validate_provider_key(runtime, provider_id, api_key);
    if !validation.ok {
        return Err(SettingsError::Unavailable(validation.message));
    }
    let status = credential_status(&model, store);
    if status.source != CredentialSource::Environment {
        store
            .store(provider_id, &model.api_key_env, api_key)
            .map_err(credential_error)?;
    }
    runtime
        .model_registry()
        .register_configured_model_with_credentials(&model, store)
        .map_err(|_| {
            SettingsError::Unavailable("could not refresh provider credentials".to_owned())
        })?;
    let updated = credential_status(&model, store);
    Ok(ProviderCredentialView {
        provider_id: provider_id.to_owned(),
        provider: provider_label(&model.provider).to_owned(),
        credential: updated,
    })
}

pub fn disconnect_provider(
    runtime: &Runtime,
    store: &dyn CredentialStore,
    provider_id: &str,
) -> Result<ProviderCredentialView, SettingsError> {
    let model = credential_model_by_id(runtime, provider_id)?;
    let credential = store
        .disconnect(provider_id, &model.api_key_env)
        .map_err(credential_error)?;
    runtime
        .model_registry()
        .register_configured_model_with_credentials(&model, store)
        .map_err(|_| {
            SettingsError::Unavailable("could not refresh provider credentials".to_owned())
        })?;
    Ok(ProviderCredentialView {
        provider_id: provider_id.to_owned(),
        provider: provider_label(&model.provider).to_owned(),
        credential,
    })
}

fn credential_model_by_id(
    runtime: &Runtime,
    provider_id: &str,
) -> Result<ModelConfig, SettingsError> {
    let provider = match provider_id {
        "openai" => ProviderKind::OpenAi,
        "anthropic" => ProviderKind::Anthropic,
        "gemini" => ProviderKind::Gemini,
        "opencode-zen" => ProviderKind::OpenCodeZen,
        "opencode-go" => ProviderKind::OpenCodeGo,
        _ => return Err(SettingsError::UnknownProvider(provider_id.to_owned())),
    };
    Ok(credential_model(runtime, provider))
}

fn credential_model(runtime: &Runtime, provider: ProviderKind) -> ModelConfig {
    let active = runtime.model();
    if active.provider == provider {
        active
    } else {
        ModelConfig::for_provider(provider)
    }
}

fn credential_error(error: CredentialError) -> SettingsError {
    SettingsError::Unavailable(error.to_string())
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
    }
}
