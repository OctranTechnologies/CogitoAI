use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use harness_tools::ToolRequest;
use serde::{Deserialize, Serialize};

const MAX_HOOKS: usize = 64;
const MAX_PROTECTED_PATHS: usize = 128;
const MAX_HOOK_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    BeforeModel,
    BeforeTool,
    AfterTool,
    BeforeEdit,
    AfterEdit,
    BeforeCommand,
    AfterCommand,
    BeforeCompact,
    AfterCompact,
    SessionEnd,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::BeforeModel => "before_model",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::BeforeEdit => "before_edit",
            Self::AfterEdit => "after_edit",
            Self::BeforeCommand => "before_command",
            Self::AfterCommand => "after_command",
            Self::BeforeCompact => "before_compact",
            Self::AfterCompact => "after_compact",
            Self::SessionEnd => "session_end",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureMode {
    /// Report the failure to the model and continue the coding task.
    #[default]
    Continue,
    /// Stop the current operation when the hook cannot complete.
    Block,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    pub event: HookEvent,
    /// Shell syntax is interpreted by the same shell tool and policy used for
    /// model-requested commands. Hooks do not receive a policy bypass.
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub on_error: HookFailureMode,
}

fn default_timeout() -> u64 {
    DEFAULT_HOOK_TIMEOUT_MS
}

#[derive(Clone, Debug, Default)]
pub struct HookConfig {
    hooks: Vec<HookDefinition>,
    protected_matcher: Option<GlobSet>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HookFile {
    #[serde(default)]
    hooks: Vec<HookDefinition>,
    #[serde(default)]
    protected_paths: Vec<String>,
}

impl HookConfig {
    pub fn load(workspace_root: &Path) -> Result<Self, String> {
        let path = workspace_root.join(".agent").join("hooks.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let file: HookFile = toml::from_str(&contents)
            .map_err(|error| format!("invalid {}: {error}", path.display()))?;
        if file.hooks.len() > MAX_HOOKS {
            return Err(format!(
                "{} contains more than {MAX_HOOKS} hooks",
                path.display()
            ));
        }
        if file.protected_paths.len() > MAX_PROTECTED_PATHS {
            return Err(format!(
                "{} contains more than {MAX_PROTECTED_PATHS} protected path patterns",
                path.display()
            ));
        }
        for hook in &file.hooks {
            if hook.command.trim().is_empty() || hook.command.len() > 4_096 {
                return Err(format!(
                    "{} has an empty or overlong command for {}",
                    path.display(),
                    hook.event.as_str()
                ));
            }
            if !(1..=MAX_HOOK_TIMEOUT_MS).contains(&hook.timeout_ms) {
                return Err(format!(
                    "{} hook timeout must be between 1 and {MAX_HOOK_TIMEOUT_MS} milliseconds",
                    path.display()
                ));
            }
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &file.protected_paths {
            let path = Path::new(pattern);
            if pattern.trim().is_empty()
                || path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::RootDir
                    )
                })
            {
                return Err(format!("invalid protected path pattern: {pattern}"));
            }
            builder.add(
                Glob::new(pattern).map_err(|error| {
                    format!("invalid protected path pattern {pattern:?}: {error}")
                })?,
            );
        }
        let protected_matcher = (!file.protected_paths.is_empty())
            .then(|| builder.build())
            .transpose()
            .map_err(|error| format!("invalid protected path matcher: {error}"))?;
        Ok(Self {
            hooks: file.hooks,
            protected_matcher,
        })
    }

    pub fn hooks_for(&self, event: HookEvent) -> impl Iterator<Item = &HookDefinition> {
        self.hooks.iter().filter(move |hook| hook.event == event)
    }

    pub fn protected_path(&self, workspace_root: &Path, request: &ToolRequest) -> Option<PathBuf> {
        let matcher = self.protected_matcher.as_ref()?;
        let arguments = request.arguments.as_object()?;
        ["path", "file_path", "target_file", "destination_path"]
            .into_iter()
            .filter_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))
            .find_map(|value| {
                let relative = Path::new(value);
                let relative = if relative.is_absolute() {
                    relative.strip_prefix(workspace_root).ok()?.to_path_buf()
                } else {
                    relative.to_path_buf()
                };
                let normalized = relative.to_string_lossy().replace('\\', "/");
                matcher.is_match(&normalized).then_some(relative)
            })
    }
}

pub fn hook_shell_request(hook: &HookDefinition) -> ToolRequest {
    ToolRequest::new(
        "shell",
        serde_json::json!({
            "command": hook.command,
            "timeout_ms": hook.timeout_ms,
            "max_output_bytes": 8 * 1024
        }),
    )
}

pub fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "create_file"
            | "write_file"
            | "apply_patch"
            | "replace_text"
            | "replace_range"
            | "delete_file"
            | "rename_file"
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use harness_core::SessionId;
    use harness_policy::AllowAllPolicy;
    use harness_tools::{CancellationToken, ToolContext, ToolRegistry};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn loads_hooks_and_protected_paths_with_bounded_timeout() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join(".agent/hooks.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "protected_paths = [\"src/generated/**\"]\n\n[[hooks]]\nevent = \"after_edit\"\ncommand = \"cargo fmt --all\"\ntimeout_ms = 2500\non_error = \"block\"\n",
        )
        .unwrap();

        let hooks = HookConfig::load(temporary.path()).unwrap();
        assert_eq!(hooks.hooks_for(HookEvent::AfterEdit).count(), 1);
        let request = ToolRequest::new(
            "write_file",
            serde_json::json!({"path": "src/generated/api.rs"}),
        );
        assert_eq!(
            hooks.protected_path(temporary.path(), &request),
            Some(PathBuf::from("src/generated/api.rs"))
        );
    }

    #[test]
    fn rejects_broken_hook_configuration_and_unsafe_paths() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join(".agent/hooks.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[[hooks]]\nevent = \"before_model\"\n").unwrap();
        assert!(HookConfig::load(temporary.path()).is_err());

        fs::write(&path, "protected_paths = [\"../outside/**\"]\n").unwrap();
        assert!(HookConfig::load(temporary.path()).is_err());
    }

    #[test]
    fn hook_requests_keep_timeouts_and_run_as_normal_shell_tools() {
        let hook = HookDefinition {
            event: HookEvent::AfterEdit,
            command: "cargo fmt --all".to_owned(),
            timeout_ms: 1_500,
            on_error: HookFailureMode::Continue,
        };
        let request = hook_shell_request(&hook);
        assert_eq!(request.name, "shell");
        assert_eq!(request.arguments["timeout_ms"], 1_500);
        assert_eq!(request.arguments["command"], "cargo fmt --all");
    }

    #[test]
    fn hook_timeout_is_enforced_by_the_normal_shell_runner() {
        let temporary = tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let session_id = SessionId::new("hook-timeout-session".to_owned()).unwrap();
        let context = ToolContext {
            policy: &AllowAllPolicy,
            working_directory: temporary.path(),
            cancellation: Some(&cancellation),
            event_bus: None,
            session_id: Some(&session_id),
            correlation_id: None,
        };
        let command = if cfg!(windows) {
            "powershell -NoLogo -NoProfile -Command \"Start-Sleep -Seconds 2\""
        } else {
            "sleep 2"
        };
        let hook = HookDefinition {
            event: HookEvent::AfterCommand,
            command: command.to_owned(),
            timeout_ms: 30,
            on_error: HookFailureMode::Continue,
        };
        let result = ToolRegistry::with_workspace_tools()
            .execute(&context, hook_shell_request(&hook))
            .unwrap();
        assert_eq!(result.metadata["timed_out"], true);
        assert!(result.is_error);
    }
}
