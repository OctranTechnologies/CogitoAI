use std::fs;

use harness_core::{discover_workspace, InstructionKind, Language, MonorepoIndicator};
use tempfile::tempdir;

#[test]
fn exposes_serializable_workspace_metadata() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("packages/app")).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"workspaces":["packages/*"],"scripts":{"test":"node test.js"}}"#,
    )
    .unwrap();
    fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
    fs::write(
        root.join("packages/app/index.ts"),
        "export const value = 1;\n",
    )
    .unwrap();
    fs::write(root.join("AGENTS.md"), "Keep discovery read-only.\n").unwrap();

    let description = discover_workspace(&root.join("packages/app")).unwrap();
    let json = serde_json::to_value(&description).unwrap();

    assert_eq!(description.languages, vec![Language::TypeScript]);
    assert!(description.monorepo.is_monorepo);
    assert_eq!(
        description.monorepo.indicators,
        vec![MonorepoIndicator::NpmWorkspaces]
    );
    assert_eq!(description.instructions[0].kind, InstructionKind::Agents);
    assert!(json["configuration"]["commands"]["test"].is_array());
}
