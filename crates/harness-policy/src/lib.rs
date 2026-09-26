use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use globset::Glob;
use harness_core::Error;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Permission {
    ReadWorkspace,
    WriteWorkspace,
    ExecuteCommand,
    AccessNetwork,
}

impl fmt::Display for Permission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::ReadWorkspace => "read-workspace",
            Self::WriteWorkspace => "write-workspace",
            Self::ExecuteCommand => "execute-command",
            Self::AccessNetwork => "access-network",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Read,
    Search,
    Write,
    Patch,
    Command,
    Network,
}

impl OperationKind {
    pub fn permission(self) -> Permission {
        match self {
            Self::Read | Self::Search => Permission::ReadWorkspace,
            Self::Write | Self::Patch => Permission::WriteWorkspace,
            Self::Command => Permission::ExecuteCommand,
            Self::Network => Permission::AccessNetwork,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

impl fmt::Display for PolicyDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    ReadOnly,
    Safe,
    Normal,
    Auto,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyRequest {
    pub tool_name: String,
    pub operation: OperationKind,
    pub workspace_root: PathBuf,
    pub path: Option<PathBuf>,
    pub command: Option<String>,
    pub mode: ExecutionMode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyEvaluation {
    pub decision: PolicyDecision,
    pub rule: String,
    pub reason: String,
}

impl PolicyEvaluation {
    pub fn new(
        decision: PolicyDecision,
        rule: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            decision,
            rule: rule.into(),
            reason: reason.into(),
        }
    }
}

pub trait Policy: Send + Sync {
    fn check(&self, permission: Permission) -> PolicyDecision;

    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Normal
    }

    fn evaluate(&self, request: &PolicyRequest) -> PolicyEvaluation {
        let decision = self.check(request.operation.permission());
        PolicyEvaluation::new(
            decision,
            "legacy-permission",
            format!("{:?}", request.operation.permission()),
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DenyAllPolicy;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AllowAllPolicy;

impl Policy for AllowAllPolicy {
    fn check(&self, _permission: Permission) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

impl Policy for DenyAllPolicy {
    fn check(&self, _permission: Permission) -> PolicyDecision {
        PolicyDecision::Deny
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyRule {
    #[serde(default = "default_rule_name")]
    pub name: String,
    pub action: PolicyDecision,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub operations: Option<Vec<OperationKind>>,
    #[serde(default)]
    pub modes: Option<Vec<ExecutionMode>>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub command_patterns: Option<Vec<String>>,
}

fn default_rule_name() -> String {
    "unnamed".to_owned()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyEngine {
    pub mode: ExecutionMode,
    pub workspace_root: PathBuf,
    pub rules: Vec<PolicyRule>,
}

#[derive(Debug, ThisError)]
pub enum PolicyError {
    #[error("invalid policy configuration: {reason}")]
    InvalidConfig { reason: String },
    #[error("invalid policy glob {pattern}: {message}")]
    InvalidGlob { pattern: String, message: String },
}

#[derive(Deserialize, Default)]
struct PolicyFile {
    policy: Option<PolicySettings>,
}

#[derive(Deserialize, Default)]
struct PolicySettings {
    mode: Option<ExecutionMode>,
    #[serde(default)]
    rules: Vec<PolicyRule>,
}

impl PolicyEngine {
    pub fn new(mode: ExecutionMode, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
            rules: Vec::new(),
        }
    }

    /// The rules loaded from configuration, in file order.
    ///
    /// Exposed so a client can show which rules are actually in force instead of
    /// asking the user to trust a mode name.
    pub fn configured_rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    pub fn from_toml(
        contents: &str,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Self, PolicyError> {
        let file: PolicyFile =
            toml::from_str(contents).map_err(|error| PolicyError::InvalidConfig {
                reason: error.to_string(),
            })?;
        let settings = file.policy.unwrap_or_default();
        Ok(Self {
            mode: settings.mode.unwrap_or(ExecutionMode::Normal),
            workspace_root: workspace_root.into(),
            rules: settings.rules,
        })
    }

    pub fn from_file(path: &Path, workspace_root: impl Into<PathBuf>) -> Result<Self, PolicyError> {
        let contents = fs::read_to_string(path).map_err(|error| PolicyError::InvalidConfig {
            reason: format!("{}: {error}", path.display()),
        })?;
        Self::from_toml(&contents, workspace_root)
    }

    pub fn evaluate_request(
        &self,
        request: &PolicyRequest,
    ) -> Result<PolicyEvaluation, PolicyError> {
        let mut normalized_request = request.clone();
        normalized_request.workspace_root = normalize_path(&request.workspace_root);
        if let Some(path) = &request.path {
            normalized_request.path = Some(normalize_path(path));
        }
        if let Some(path) = &normalized_request.path {
            if !path.starts_with(&normalized_request.workspace_root) {
                return Ok(PolicyEvaluation::new(
                    PolicyDecision::Deny,
                    "built-in:workspace-boundary",
                    "path is outside the permitted workspace",
                ));
            }
            if is_high_risk_path(path) {
                return Ok(PolicyEvaluation::new(
                    PolicyDecision::Deny,
                    "built-in:high-risk-path",
                    "path is protected as a credential or VCS secret",
                ));
            }
        }
        let mut matches = Vec::new();
        for (index, rule) in self.rules.iter().enumerate() {
            if rule_matches(rule, &normalized_request)? {
                matches.push((index, rule));
            }
        }
        if let Some((_, rule)) = matches
            .iter()
            .find(|(_, rule)| rule.action == PolicyDecision::Deny)
        {
            return Ok(PolicyEvaluation::new(
                PolicyDecision::Deny,
                rule.name.clone(),
                "explicit deny rule matched",
            ));
        }
        matches.sort_by_key(|(index, rule)| (-rule.priority, *index));
        if let Some((_, rule)) = matches.first() {
            return Ok(PolicyEvaluation::new(
                rule.action,
                rule.name.clone(),
                format!("rule {} matched", rule.name),
            ));
        }
        Ok(self.default_evaluation(&normalized_request))
    }

    fn default_evaluation(&self, request: &PolicyRequest) -> PolicyEvaluation {
        let decision = match request.mode {
            ExecutionMode::ReadOnly => match request.operation {
                OperationKind::Read | OperationKind::Search => PolicyDecision::Allow,
                _ => PolicyDecision::Deny,
            },
            ExecutionMode::Safe => match request.operation {
                OperationKind::Read | OperationKind::Search => PolicyDecision::Allow,
                _ => PolicyDecision::Ask,
            },
            ExecutionMode::Normal | ExecutionMode::Auto => match request.operation {
                OperationKind::Read
                | OperationKind::Search
                | OperationKind::Write
                | OperationKind::Patch => PolicyDecision::Allow,
                OperationKind::Command => {
                    if is_safe_command(request.command.as_deref().unwrap_or_default()) {
                        PolicyDecision::Allow
                    } else {
                        PolicyDecision::Ask
                    }
                }
                OperationKind::Network => PolicyDecision::Ask,
            },
        };
        let reason = match decision {
            PolicyDecision::Allow => format!("{:?} mode allows this operation", request.mode),
            PolicyDecision::Ask => format!(
                "{:?} mode requires approval for this operation",
                request.mode
            ),
            PolicyDecision::Deny => format!("{:?} mode denies this operation", request.mode),
        };
        PolicyEvaluation::new(decision, "mode-default", reason)
    }
}

impl Policy for PolicyEngine {
    fn mode(&self) -> ExecutionMode {
        self.mode
    }

    fn check(&self, permission: Permission) -> PolicyDecision {
        let request = PolicyRequest {
            tool_name: String::new(),
            operation: match permission {
                Permission::ReadWorkspace => OperationKind::Read,
                Permission::WriteWorkspace => OperationKind::Write,
                Permission::ExecuteCommand => OperationKind::Command,
                Permission::AccessNetwork => OperationKind::Network,
            },
            workspace_root: self.workspace_root.clone(),
            path: None,
            command: None,
            mode: self.mode,
        };
        self.default_evaluation(&request).decision
    }

    fn evaluate(&self, request: &PolicyRequest) -> PolicyEvaluation {
        self.evaluate_request(request).unwrap_or_else(|error| {
            PolicyEvaluation::new(PolicyDecision::Deny, "invalid-policy", error.to_string())
        })
    }
}

pub fn authorize(policy: &dyn Policy, permission: Permission) -> Result<(), Error> {
    let decision = policy.check(permission);
    ensure_decision(permission.to_string(), decision, "permission check")
}

pub fn authorize_request(policy: &dyn Policy, request: &PolicyRequest) -> Result<(), Error> {
    let evaluation = policy.evaluate(request);
    ensure_decision(
        format!("{} {}", request.operation_name(), request.tool_name),
        evaluation.decision,
        evaluation.reason,
    )
}

fn ensure_decision(
    capability: String,
    decision: PolicyDecision,
    reason: impl Into<String>,
) -> Result<(), Error> {
    match decision {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Ask => Err(Error::PermissionRequired {
            capability,
            reason: reason.into(),
        }),
        PolicyDecision::Deny => Err(Error::PermissionDenied { capability }),
    }
}

impl PolicyRequest {
    pub fn operation_name(&self) -> &'static str {
        match self.operation {
            OperationKind::Read => "read",
            OperationKind::Search => "search",
            OperationKind::Write => "write",
            OperationKind::Patch => "patch",
            OperationKind::Command => "command",
            OperationKind::Network => "network",
        }
    }
}

fn rule_matches(rule: &PolicyRule, request: &PolicyRequest) -> Result<bool, PolicyError> {
    if let Some(tools) = &rule.tools {
        let mut matched = false;
        for pattern in tools {
            if glob_matches(pattern, &request.tool_name)? {
                matched = true;
                break;
            }
        }
        if !matched {
            return Ok(false);
        }
    }
    if let Some(operations) = &rule.operations {
        if !operations.contains(&request.operation) {
            return Ok(false);
        }
    }
    if let Some(modes) = &rule.modes {
        if !modes.contains(&request.mode) {
            return Ok(false);
        }
    }
    if let Some(paths) = &rule.paths {
        let Some(path) = &request.path else {
            return Ok(false);
        };
        let relative = path.strip_prefix(&request.workspace_root).unwrap_or(path);
        let candidates = [
            relative.to_string_lossy().replace('\\', "/"),
            path.to_string_lossy().replace('\\', "/"),
        ];
        if !paths.iter().any(|pattern| {
            candidates
                .iter()
                .any(|candidate| glob_matches(pattern, candidate).unwrap_or(false))
        }) {
            return Ok(false);
        }
    }
    if let Some(patterns) = &rule.command_patterns {
        let Some(command) = &request.command else {
            return Ok(false);
        };
        let mut matched = false;
        for pattern in patterns {
            if glob_matches(pattern, command)? {
                matched = true;
                break;
            }
        }
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn glob_matches(pattern: &str, value: &str) -> Result<bool, PolicyError> {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(value))
        .map_err(|error| PolicyError::InvalidGlob {
            pattern: pattern.to_owned(),
            message: error.to_string(),
        })
}

fn is_high_risk_path(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    normalized.contains("/.ssh/")
        || normalized.ends_with("/.ssh")
        || file_name == ".env"
        || file_name.starts_with(".env.")
        || matches!(file_name.as_str(), "id_rsa" | "id_ed25519" | "id_ecdsa")
        || matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("pem" | "key" | "p12" | "pfx")
        )
}

fn is_safe_command(command: &str) -> bool {
    let command = command.trim().to_ascii_lowercase();
    command.starts_with("git status")
        || command.starts_with("git diff")
        || command.starts_with("git log")
        || command.starts_with("git show")
        || command.starts_with("cargo check")
        || command.starts_with("cargo test")
        || command.starts_with("cargo clippy")
        || command.starts_with("cargo fmt")
        || command.starts_with("npm test")
        || command.starts_with("npm run build")
        || command.starts_with("npm run lint")
        || command.starts_with("npm run typecheck")
        || command.starts_with("pnpm test")
        || command.starts_with("pnpm run build")
        || command.starts_with("pnpm run lint")
        || command.starts_with("pnpm run typecheck")
        || command.starts_with("yarn test")
        || command.starts_with("yarn build")
        || command == "ls"
        || command == "pwd"
}
