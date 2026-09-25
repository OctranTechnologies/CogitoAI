use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use harness_core::SessionId;
use harness_git::{CheckpointStore, GitClient, GitError, ShadowCheckpointStore};
use harness_session::{EventBus, EventType};
use tempfile::tempdir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn repository() -> tempfile::TempDir {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    git(root, &["init", "--quiet"]);
    git(root, &["config", "user.email", "harness@example.invalid"]);
    git(root, &["config", "user.name", "Harness Test"]);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    fs::write(root.join("other.txt"), "other\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "initial"]);
    temporary
}

fn session() -> SessionId {
    SessionId::new("git-session").unwrap()
}

#[test]
fn reports_clean_status_and_empty_diffs() {
    let temporary = repository();
    let client = GitClient::open(temporary.path()).unwrap();
    let status = client.status().unwrap();
    let diff = client.diff().unwrap();

    assert!(status.is_clean);
    assert!(status.branch.is_some());
    assert!(status.head.is_some());
    assert!(status.changed_files.is_empty());
    assert!(diff.unstaged.is_empty());
    assert!(diff.staged.is_empty());
}

#[test]
fn reports_dirty_staged_and_untracked_files() {
    let temporary = repository();
    let root = temporary.path();
    fs::write(root.join("tracked.txt"), "changed\n").unwrap();
    fs::write(root.join("new.txt"), "untracked\n").unwrap();
    fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    git(root, &["add", "new.txt"]);
    let client = GitClient::open(root).unwrap();
    let status = client.status().unwrap();
    let diff = client.diff().unwrap();
    let file_diff = client.diff_file(&root.join("tracked.txt")).unwrap();

    assert!(!status.is_clean);
    assert!(status.changed_files.contains(&"tracked.txt".to_owned()));
    assert!(status.staged_files.contains(&"new.txt".to_owned()));
    assert!(status.untracked_files.contains(&"untracked.txt".to_owned()));
    assert!(diff.staged.contains("new.txt"));
    assert!(file_diff.unstaged.contains("changed"));
}

#[test]
fn checkpoint_undo_restores_harness_edit_and_preserves_unrelated_user_edit() {
    let temporary = repository();
    let root = temporary.path();
    let store = ShadowCheckpointStore::new(temporary.path().join("harness-checkpoints")).unwrap();
    let checkpoint = store.create(&session(), root).unwrap();
    fs::write(root.join("other.txt"), "user edit\n").unwrap();
    fs::write(root.join("tracked.txt"), "harness edit\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("tracked.txt"))
        .unwrap();

    let report = store.undo(&checkpoint.id).unwrap();

    assert_eq!(report.restored_files.len(), 1);
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("other.txt")).unwrap(),
        "user edit\n"
    );
    assert_eq!(git(root, &["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn checkpoint_undo_handles_untracked_files_and_multiple_edits() {
    let temporary = repository();
    let root = temporary.path();
    let store = ShadowCheckpointStore::new(temporary.path().join("harness-checkpoints")).unwrap();
    let checkpoint = store.create(&session(), root).unwrap();
    fs::write(root.join("tracked.txt"), "harness one\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("tracked.txt"))
        .unwrap();
    fs::write(root.join("tracked.txt"), "harness two\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("tracked.txt"))
        .unwrap();
    fs::write(root.join("created.txt"), "new harness file\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("created.txt"))
        .unwrap();

    store.undo(&checkpoint.id).unwrap();

    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert!(!root.join("created.txt").exists());
}

#[test]
fn checkpoint_conflict_preserves_user_changes() {
    let temporary = repository();
    let root = temporary.path();
    let store = ShadowCheckpointStore::new(temporary.path().join("harness-checkpoints")).unwrap();
    let checkpoint = store.create(&session(), root).unwrap();
    fs::write(root.join("tracked.txt"), "harness edit\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("tracked.txt"))
        .unwrap();
    fs::write(root.join("tracked.txt"), "later user edit\n").unwrap();

    let error = store.undo(&checkpoint.id).unwrap_err();

    assert!(matches!(error, GitError::RestoreConflict { .. }));
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "later user edit\n"
    );
}

#[test]
fn lists_inspects_and_emits_checkpoint_events() {
    let temporary = repository();
    let root = temporary.path();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let store = ShadowCheckpointStore::with_event_bus(
        temporary.path().join("harness-checkpoints"),
        Some(bus),
    )
    .unwrap();
    let checkpoint = store.create(&session(), root).unwrap();
    fs::write(root.join("tracked.txt"), "harness\n").unwrap();
    store
        .record_harness_change(&checkpoint.id, &root.join("tracked.txt"))
        .unwrap();
    store.undo(&checkpoint.id).unwrap();

    let checkpoints = store.list().unwrap();
    let info = store.inspect(&checkpoint.id).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(info.id, checkpoint.id);
    assert_eq!(info.recorded_changes, vec!["tracked.txt"]);
    let events = events.lock().unwrap();
    assert!(events.contains(&EventType::CheckpointCreated));
    assert!(events.contains(&EventType::CheckpointRestored));
}

#[test]
fn rejects_paths_outside_checkpoint_repository() {
    let temporary = repository();
    let root = temporary.path();
    let external = tempdir().unwrap();
    let outside = external.path().join("outside.txt");
    fs::write(&outside, "outside").unwrap();
    let store = ShadowCheckpointStore::new(temporary.path().join("harness-checkpoints")).unwrap();
    let checkpoint = store.create(&session(), root).unwrap();

    let error = store
        .record_harness_change(&checkpoint.id, &outside)
        .unwrap_err();

    assert!(matches!(error, GitError::InvalidPath { .. }));
}
