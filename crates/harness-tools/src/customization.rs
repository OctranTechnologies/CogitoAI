use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use harness_core::discover_instructions;
use harness_policy::{OperationKind, Permission};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Tool, ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec};

const MAX_SKILLS: usize = 48;
const MAX_SKILL_BYTES: u64 = 128 * 1024;
const SKILL_METADATA_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    /// Names only; resource contents are retrieved separately on demand.
    pub resources: Vec<String>,
}

#[derive(Clone, Debug)]
struct SkillEntry {
    metadata: SkillMetadata,
    skill_file: PathBuf,
}

pub fn available_skills(workspace_root: &Path) -> Vec<SkillMetadata> {
    scan_skills(workspace_root)
        .into_iter()
        .map(|entry| entry.metadata)
        .collect()
}

fn scan_skills(workspace_root: &Path) -> Vec<SkillEntry> {
    let Ok(root) = fs::canonicalize(workspace_root) else {
        return Vec::new();
    };
    let skills_root = root.join(".agent").join("skills");
    let Ok(root_metadata) = fs::symlink_metadata(&skills_root) else {
        return Vec::new();
    };
    if !root_metadata.file_type().is_dir() {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let Ok(directories) = fs::read_dir(&skills_root) else {
        return Vec::new();
    };
    for directory in directories.flatten() {
        let name = directory.file_name().to_string_lossy().into_owned();
        if !valid_skill_name(&name) || !directory.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = directory.path().join("SKILL.md");
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_SKILL_BYTES {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if !canonical.starts_with(&skills_root) {
            continue;
        }
        let Some((description, when_to_use)) = read_skill_metadata(&canonical, &name) else {
            continue;
        };
        let mut resources = skill_resources(&directory.path());
        resources.sort();
        resources.truncate(32);
        entries.push(SkillEntry {
            metadata: SkillMetadata {
                name,
                description,
                when_to_use,
                resources,
            },
            skill_file: canonical,
        });
    }
    entries.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    entries.truncate(MAX_SKILLS);
    entries
}

fn read_skill_metadata(path: &Path, fallback_name: &str) -> Option<(String, Option<String>)> {
    let file = fs::File::open(path).ok()?;
    let mut prefix = Vec::with_capacity(SKILL_METADATA_BYTES);
    file.take(SKILL_METADATA_BYTES as u64)
        .read_to_end(&mut prefix)
        .ok()?;
    let prefix = String::from_utf8_lossy(&prefix);
    Some(parse_skill_metadata(&prefix, fallback_name))
}

fn skill_resources(root: &Path) -> Vec<String> {
    let mut resources = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > 4 || resources.len() >= 64 {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() && entry.file_name() != "SKILL.md" {
                if let Ok(relative) = entry.path().strip_prefix(root) {
                    resources.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    resources
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

fn parse_skill_metadata(contents: &str, fallback_name: &str) -> (String, Option<String>) {
    let Some(rest) = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))
    else {
        return (fallback_description(contents, fallback_name), None);
    };
    let mut description = None;
    let mut when_to_use = None;
    for line in rest.lines().take_while(|line| line.trim() != "---") {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\"', '\'']).to_owned();
        match key.trim() {
            "description" => description = Some(value),
            "when_to_use" | "when-to-use" => when_to_use = Some(value),
            _ => {}
        }
    }
    let body = rest
        .split_once("\n---")
        .map(|(_, body)| body.trim())
        .unwrap_or(rest);
    (
        description
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback_description(body, fallback_name)),
        when_to_use.filter(|value| !value.is_empty()),
    )
}

fn fallback_description(contents: &str, name: &str) -> String {
    let first = contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or(name);
    first.chars().take(180).collect()
}

pub struct InstructionsTool;

impl Tool for InstructionsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_instructions".to_owned(),
            description: "Load inherited AGENTS.md / CLAUDE.md instructions for a relevant workspace path when entering a subdirectory".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let root = fs::canonicalize(context.working_directory).map_err(|error| ToolError::Io {
            operation: "resolve workspace root".to_owned(),
            message: error.to_string(),
        })?;
        let relative = request
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let candidate = Path::new(relative);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root.join(candidate)
        };
        let canonical = fs::canonicalize(&path).map_err(|_| ToolError::NotFound { path })?;
        if !canonical.starts_with(&root) {
            return Err(ToolError::PathOutsideWorkspace { path: canonical });
        }
        let instructions =
            discover_instructions(&root, &canonical).map_err(|error| ToolError::Io {
                operation: "load inherited instructions".to_owned(),
                message: error.to_string(),
            })?;
        if instructions.is_empty() {
            return Ok(ToolResult::new(
                "No inherited instruction files apply to this path.",
            ));
        }
        Ok(ToolResult::new(
            instructions
                .into_iter()
                .map(|instruction| {
                    format!(
                        "--- {} ({:?}, precedence {}) ---\n{}",
                        instruction.path.display(),
                        instruction.kind,
                        instruction.precedence,
                        instruction.content
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ))
    }
}

pub struct ListSkillsTool;

impl Tool for ListSkillsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_skills".to_owned(),
            description: "List available project skill names and short metadata without loading their instructions".to_owned(),
            arguments_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        _request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let skills = available_skills(context.working_directory);
        if skills.is_empty() {
            return Ok(ToolResult::new(
                "No project skills are installed in .agent/skills/",
            ));
        }
        Ok(ToolResult::new(
            skills
                .into_iter()
                .map(|skill| {
                    format!(
                        "{}: {}{}{}",
                        skill.name,
                        skill.description,
                        skill
                            .when_to_use
                            .map(|when| format!("\n  Use when: {when}"))
                            .unwrap_or_default(),
                        if skill.resources.is_empty() {
                            String::new()
                        } else {
                            format!("\n  Resources: {}", skill.resources.join(", "))
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

pub struct LoadSkillTool;

impl Tool for LoadSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".to_owned(),
            description: "Load full instructions for one relevant skill after reviewing its metadata; use only when it applies to the task".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "minLength": 1}},
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let name = request
            .arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| valid_skill_name(name))
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "load_skill".to_owned(),
                message: "name must be a lowercase skill directory name".to_owned(),
            })?;
        let entry = scan_skills(context.working_directory)
            .into_iter()
            .find(|entry| entry.metadata.name == name)
            .ok_or_else(|| ToolError::NotFound {
                path: PathBuf::from(format!(".agent/skills/{name}/SKILL.md")),
            })?;
        let body = fs::read_to_string(&entry.skill_file).map_err(|error| ToolError::Io {
            operation: "read skill instructions".to_owned(),
            message: error.to_string(),
        })?;
        let (metadata, _) = parse_skill_metadata(&body, name);
        let instructions = body
            .split_once("\n---")
            .map(|(_, content)| content.trim())
            .unwrap_or(body.as_str())
            .to_owned();
        Ok(ToolResult::new(format!(
            "Skill: {name}\nDescription: {metadata}\nInstructions:\n{instructions}\n\nOptional files are listed by list_skills; load/read them separately only when needed."
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use harness_policy::{AllowAllPolicy, DenyAllPolicy};
    use tempfile::tempdir;

    use super::*;

    fn context<'a>(root: &'a Path, policy: &'a dyn harness_policy::Policy) -> ToolContext<'a> {
        ToolContext {
            policy,
            working_directory: root,
            execution_environment: crate::local_execution_environment(),
            cancellation: None,
            event_bus: None,
            session_id: None,
            correlation_id: None,
        }
    }

    #[test]
    fn skills_expose_metadata_first_and_load_content_only_when_selected() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let skill = root.join(".agent/skills/rust-tests");
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: rust-tests\ndescription: Run focused Rust validation\nwhen_to_use: when editing Rust crates\n---\nPrefer the narrowest cargo test first.\n",
        )
        .unwrap();
        fs::write(skill.join("references/checks.md"), "Long reference details").unwrap();
        fs::write(skill.join("setup.sh"), "echo setup").unwrap();

        let metadata = available_skills(root);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].name, "rust-tests");
        assert_eq!(metadata[0].description, "Run focused Rust validation");
        assert_eq!(metadata[0].resources, ["references/checks.md", "setup.sh"]);

        let loaded = LoadSkillTool
            .execute(
                &context(root, &AllowAllPolicy),
                ToolRequest::new("load_skill", json!({"name": "rust-tests"})),
            )
            .unwrap();
        assert!(loaded
            .output
            .contains("Prefer the narrowest cargo test first."));
        assert!(!loaded.output.contains("Long reference details"));
    }

    #[test]
    fn skill_loader_rejects_traversal_and_tools_obey_deny_policy() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let skills = root.join(".agent/skills/safe");
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("SKILL.md"), "Do safe work.").unwrap();
        assert!(LoadSkillTool
            .execute(
                &context(root, &AllowAllPolicy),
                ToolRequest::new("load_skill", json!({"name": "../outside"})),
            )
            .is_err());

        let registry = crate::ToolRegistry::with_workspace_tools();
        assert!(registry
            .execute(
                &context(root, &DenyAllPolicy),
                ToolRequest::new("list_skills", json!({})),
            )
            .is_err());
    }

    #[test]
    fn instruction_lookup_returns_inherited_scope_and_rejects_escape() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo");
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("AGENTS.md"), "repository instruction").unwrap();
        fs::write(nested.join("AGENTS.md"), "source instruction").unwrap();
        fs::write(nested.join("lib.rs"), "pub fn app() {}\n").unwrap();
        fs::write(temporary.path().join("outside.txt"), "outside").unwrap();

        let tool = InstructionsTool;
        let result = tool
            .execute(
                &context(&root, &AllowAllPolicy),
                ToolRequest::new("get_instructions", json!({"path": "src/lib.rs"})),
            )
            .unwrap();
        assert!(result.output.contains("repository instruction"));
        assert!(result.output.contains("source instruction"));
        assert!(tool
            .execute(
                &context(&root, &AllowAllPolicy),
                ToolRequest::new("get_instructions", json!({"path": "../outside.txt"})),
            )
            .is_err());
    }
}
