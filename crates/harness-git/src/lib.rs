use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use harness_core::{CheckpointId, SessionId};
use harness_session::{EventBus, EventPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: String,
    pub index_status: char,
    pub worktree_status: char,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitStatus {
    pub repository_root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_clean: bool,
    pub changed_files: Vec<String>,
    pub staged_files: Vec<String>,
    pub unstaged_files: Vec<String>,
    pub untracked_files: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitDiff {
    pub unstaged: String,
    pub staged: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub exists: bool,
    pub content: Option<Vec<u8>>,
    pub executable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub session_id: SessionId,
    pub working_directory: PathBuf,
    pub created_at: String,
    pub reference: String,
    pub baseline: BTreeMap<String, FileSnapshot>,
    pub recorded_changes: BTreeMap<String, FileSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: CheckpointId,
    pub session_id: SessionId,
    pub working_directory: PathBuf,
    pub created_at: String,
    pub reference: String,
    pub baseline_file_count: usize,
    pub recorded_changes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub checkpoint_id: CheckpointId,
    pub restored_files: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("path is not a Git repository: {path}")]
    NotRepository { path: PathBuf },
    #[error("Git command failed: {message}")]
    Command { message: String },
    #[error("Git operation I/O failed: {message}")]
    Io { message: String },
    #[error("checkpoint not found: {id}")]
    CheckpointNotFound { id: CheckpointId },
    #[error("checkpoint path is outside the repository: {path}")]
    InvalidPath { path: PathBuf },
    #[error("checkpoint file is too large: {path}")]
    FileTooLarge { path: PathBuf },
    #[error("restore conflict; no files were changed: {paths:?}")]
    RestoreConflict { paths: Vec<PathBuf> },
    #[error("checkpoint serialization failed: {message}")]
    Serialization { message: String },
}

pub struct GitClient {
    root: PathBuf,
}

impl GitClient {
    pub fn open(working_directory: &Path) -> Result<Self, GitError> {
        let working_directory =
            fs::canonicalize(working_directory).map_err(|error| GitError::Io {
                message: error.to_string(),
            })?;
        let output = run_git(&working_directory, &["rev-parse", "--show-toplevel"])?;
        let root = fs::canonicalize(Path::new(output.trim())).map_err(|error| GitError::Io {
            message: error.to_string(),
        })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn status(&self) -> Result<GitStatus, GitError> {
        let output = run_git(
            &self.root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        let mut changed_files = Vec::new();
        let mut staged_files = Vec::new();
        let mut unstaged_files = Vec::new();
        let mut untracked_files = Vec::new();
        for line in output.lines() {
            if line.len() < 3 {
                continue;
            }
            let mut characters = line.chars();
            let index_status = characters.next().unwrap_or(' ');
            let worktree_status = characters.next().unwrap_or(' ');
            let path = line.get(3..).unwrap_or_default().to_owned();
            let path = path.rsplit(" -> ").next().unwrap_or(&path).to_owned();
            changed_files.push(path.clone());
            if index_status == '?' && worktree_status == '?' {
                untracked_files.push(path);
            } else {
                if index_status != ' ' {
                    staged_files.push(path.clone());
                }
                if worktree_status != ' ' {
                    unstaged_files.push(path);
                }
            }
        }
        changed_files.sort();
        changed_files.dedup();
        staged_files.sort();
        staged_files.dedup();
        unstaged_files.sort();
        unstaged_files.dedup();
        untracked_files.sort();
        untracked_files.dedup();
        Ok(GitStatus {
            repository_root: self.root.clone(),
            branch: self.branch()?,
            head: self.head()?,
            is_clean: changed_files.is_empty(),
            changed_files,
            staged_files,
            unstaged_files,
            untracked_files,
        })
    }

    pub fn branch(&self) -> Result<Option<String>, GitError> {
        run_git_optional(&self.root, &["symbolic-ref", "--short", "-q", "HEAD"])
    }

    pub fn head(&self) -> Result<Option<String>, GitError> {
        run_git_optional(&self.root, &["rev-parse", "HEAD"])
    }

    pub fn diff(&self) -> Result<GitDiff, GitError> {
        Ok(GitDiff {
            unstaged: run_git(&self.root, &["diff", "--no-ext-diff", "--no-color"])?,
            staged: run_git(
                &self.root,
                &["diff", "--cached", "--no-ext-diff", "--no-color"],
            )?,
        })
    }

    pub fn diff_file(&self, path: &Path) -> Result<GitDiff, GitError> {
        let path = self.relative_path(path)?;
        let path = path.to_string_lossy().replace('\\', "/");
        Ok(GitDiff {
            unstaged: run_git(
                &self.root,
                &["diff", "--no-ext-diff", "--no-color", "--", &path],
            )?,
            staged: run_git(
                &self.root,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--no-color",
                    "--",
                    &path,
                ],
            )?,
        })
    }

    pub fn list_worktree_files(&self) -> Result<Vec<String>, GitError> {
        let output = run_git_bytes(
            &self.root,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
        )?;
        Ok(String::from_utf8_lossy(&output)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf, GitError> {
        let absolute = fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path));
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| GitError::InvalidPath {
                path: absolute.to_path_buf(),
            })?;
        Ok(relative.to_path_buf())
    }
}

pub trait CheckpointStore: Send + Sync {
    fn create(
        &self,
        session_id: &SessionId,
        working_directory: &Path,
    ) -> Result<Checkpoint, GitError>;
    fn list(&self) -> Result<Vec<CheckpointInfo>, GitError>;
    fn inspect(&self, checkpoint_id: &CheckpointId) -> Result<CheckpointInfo, GitError>;
    fn load(&self, checkpoint_id: &CheckpointId) -> Result<Checkpoint, GitError>;
    fn record_harness_change(
        &self,
        checkpoint_id: &CheckpointId,
        path: &Path,
    ) -> Result<(), GitError>;
    fn restore(&self, checkpoint: &Checkpoint) -> Result<RestoreReport, GitError>;
    fn undo(&self, checkpoint_id: &CheckpointId) -> Result<RestoreReport, GitError> {
        let checkpoint = self.load(checkpoint_id)?;
        self.restore(&checkpoint)
    }
}

pub struct ShadowCheckpointStore {
    storage_root: PathBuf,
    event_bus: Option<EventBus>,
}

impl ShadowCheckpointStore {
    pub fn new(storage_root: impl Into<PathBuf>) -> Result<Self, GitError> {
        Self::with_event_bus(storage_root, None)
    }

    pub fn with_event_bus(
        storage_root: impl Into<PathBuf>,
        event_bus: Option<EventBus>,
    ) -> Result<Self, GitError> {
        let storage_root = storage_root.into();
        fs::create_dir_all(&storage_root).map_err(|error| GitError::Io {
            message: error.to_string(),
        })?;
        Ok(Self {
            storage_root,
            event_bus,
        })
    }

    fn create_checkpoint(
        &self,
        session_id: &SessionId,
        working_directory: &Path,
    ) -> Result<Checkpoint, GitError> {
        let client = GitClient::open(working_directory)?;
        let storage_root =
            fs::canonicalize(&self.storage_root).unwrap_or_else(|_| self.storage_root.clone());
        let storage_inside_repo = storage_root.starts_with(client.root());
        let mut baseline = BTreeMap::new();
        for relative in client.list_worktree_files()? {
            let absolute = client.root().join(&relative);
            if storage_inside_repo && absolute.starts_with(&storage_root) {
                continue;
            }
            if relative == ".git" || relative.starts_with(".git/") {
                continue;
            }
            let path = client.root().join(&relative);
            baseline.insert(relative, snapshot_file(&path)?);
        }
        let id = CheckpointId::new(format!("checkpoint-{}", unique_suffix())).map_err(|error| {
            GitError::Command {
                message: error.to_string(),
            }
        })?;
        let checkpoint = Checkpoint {
            id,
            session_id: session_id.clone(),
            working_directory: client.root().to_path_buf(),
            created_at: timestamp(),
            reference: client.head()?.unwrap_or_else(|| "unborn".to_owned()),
            baseline,
            recorded_changes: BTreeMap::new(),
        };
        self.persist(&checkpoint)?;
        if let Some(event_bus) = &self.event_bus {
            event_bus.publish(&harness_session::HarnessEvent::new(
                session_id.clone(),
                EventPayload::CheckpointCreated {
                    checkpoint_id: checkpoint.id.clone(),
                    reference: checkpoint.reference.clone(),
                },
                None,
                None,
            ));
        }
        Ok(checkpoint)
    }

    fn load(&self, checkpoint_id: &CheckpointId) -> Result<Checkpoint, GitError> {
        let path = self.checkpoint_path(checkpoint_id);
        if !path.is_file() {
            return Err(GitError::CheckpointNotFound {
                id: checkpoint_id.clone(),
            });
        }
        let contents = fs::read_to_string(path).map_err(|error| GitError::Io {
            message: error.to_string(),
        })?;
        serde_json::from_str(&contents).map_err(|error| GitError::Serialization {
            message: error.to_string(),
        })
    }

    fn persist(&self, checkpoint: &Checkpoint) -> Result<(), GitError> {
        let contents =
            serde_json::to_vec_pretty(checkpoint).map_err(|error| GitError::Serialization {
                message: error.to_string(),
            })?;
        fs::write(self.checkpoint_path(&checkpoint.id), contents).map_err(|error| GitError::Io {
            message: error.to_string(),
        })
    }

    fn checkpoint_path(&self, checkpoint_id: &CheckpointId) -> PathBuf {
        self.storage_root
            .join(format!("{}.json", checkpoint_id.as_str()))
    }

    fn emit_restore(&self, checkpoint: &Checkpoint, report: &RestoreReport) {
        if let Some(event_bus) = &self.event_bus {
            event_bus.publish(&harness_session::HarnessEvent::new(
                checkpoint.session_id.clone(),
                EventPayload::CheckpointRestored {
                    checkpoint_id: checkpoint.id.clone(),
                    restored_files: report.restored_files.clone(),
                    conflicts: report.conflicts.clone(),
                },
                None,
                None,
            ));
        }
    }
}

impl CheckpointStore for ShadowCheckpointStore {
    fn create(
        &self,
        session_id: &SessionId,
        working_directory: &Path,
    ) -> Result<Checkpoint, GitError> {
        self.create_checkpoint(session_id, working_directory)
    }

    fn list(&self) -> Result<Vec<CheckpointInfo>, GitError> {
        let mut checkpoints = Vec::new();
        for entry in fs::read_dir(&self.storage_root).map_err(|error| GitError::Io {
            message: error.to_string(),
        })? {
            let path = entry
                .map_err(|error| GitError::Io {
                    message: error.to_string(),
                })?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let contents = fs::read_to_string(path).map_err(|error| GitError::Io {
                message: error.to_string(),
            })?;
            let checkpoint: Checkpoint =
                serde_json::from_str(&contents).map_err(|error| GitError::Serialization {
                    message: error.to_string(),
                })?;
            checkpoints.push(info(&checkpoint));
        }
        checkpoints.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(checkpoints)
    }

    fn inspect(&self, checkpoint_id: &CheckpointId) -> Result<CheckpointInfo, GitError> {
        Ok(info(&self.load(checkpoint_id)?))
    }

    fn load(&self, checkpoint_id: &CheckpointId) -> Result<Checkpoint, GitError> {
        ShadowCheckpointStore::load(self, checkpoint_id)
    }

    fn record_harness_change(
        &self,
        checkpoint_id: &CheckpointId,
        path: &Path,
    ) -> Result<(), GitError> {
        let mut checkpoint = self.load(checkpoint_id)?;
        let relative = relative_path(&checkpoint.working_directory, path)?;
        let snapshot = snapshot_file(&checkpoint.working_directory.join(&relative))?;
        checkpoint.recorded_changes.insert(relative, snapshot);
        self.persist(&checkpoint)
    }

    fn restore(&self, checkpoint: &Checkpoint) -> Result<RestoreReport, GitError> {
        let mut conflicts = Vec::new();
        for (relative, after) in &checkpoint.recorded_changes {
            let current = snapshot_file(&checkpoint.working_directory.join(relative))?;
            if &current != after {
                conflicts.push(PathBuf::from(relative));
            }
        }
        if !conflicts.is_empty() {
            let report = RestoreReport {
                checkpoint_id: checkpoint.id.clone(),
                restored_files: Vec::new(),
                conflicts: conflicts.clone(),
            };
            self.emit_restore(checkpoint, &report);
            return Err(GitError::RestoreConflict { paths: conflicts });
        }
        let mut restored_files = Vec::new();
        for relative in checkpoint.recorded_changes.keys() {
            let path = checkpoint.working_directory.join(relative);
            match checkpoint.baseline.get(relative) {
                Some(baseline) if baseline.exists => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(|error| GitError::Io {
                            message: error.to_string(),
                        })?;
                    }
                    fs::write(&path, baseline.content.as_deref().unwrap_or_default()).map_err(
                        |error| GitError::Io {
                            message: error.to_string(),
                        },
                    )?;
                    set_executable(&path, baseline.executable)?;
                }
                _ => {
                    if path.exists() {
                        fs::remove_file(&path).map_err(|error| GitError::Io {
                            message: error.to_string(),
                        })?;
                    }
                }
            }
            restored_files.push(path);
        }
        let report = RestoreReport {
            checkpoint_id: checkpoint.id.clone(),
            restored_files,
            conflicts: Vec::new(),
        };
        self.emit_restore(checkpoint, &report);
        Ok(report)
    }
}

fn info(checkpoint: &Checkpoint) -> CheckpointInfo {
    CheckpointInfo {
        id: checkpoint.id.clone(),
        session_id: checkpoint.session_id.clone(),
        working_directory: checkpoint.working_directory.clone(),
        created_at: checkpoint.created_at.clone(),
        reference: checkpoint.reference.clone(),
        baseline_file_count: checkpoint.baseline.len(),
        recorded_changes: checkpoint.recorded_changes.keys().cloned().collect(),
    }
}

fn snapshot_file(path: &Path) -> Result<FileSnapshot, GitError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileSnapshot {
                exists: false,
                content: None,
                executable: false,
            })
        }
        Err(error) => {
            return Err(GitError::Io {
                message: error.to_string(),
            })
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(GitError::InvalidPath {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(GitError::InvalidPath {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > 10 * 1024 * 1024 {
        return Err(GitError::FileTooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(FileSnapshot {
        exists: true,
        content: Some(fs::read(path).map_err(|error| GitError::Io {
            message: error.to_string(),
        })?),
        executable: is_executable(path),
    })
}

fn is_executable(_path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(_path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn set_executable(path: &Path, executable: bool) -> Result<(), GitError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| GitError::Io {
                message: error.to_string(),
            })?
            .permissions();
        let mode = if executable { 0o755 } else { 0o644 };
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).map_err(|error| GitError::Io {
            message: error.to_string(),
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<String, GitError> {
    let absolute = fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path));
    let root = fs::canonicalize(root).unwrap_or_else(|_| normalize_path(root));
    let relative = absolute
        .strip_prefix(&root)
        .map_err(|_| GitError::InvalidPath {
            path: absolute.to_path_buf(),
        })?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() || relative.split('/').any(|component| component == "..") {
        return Err(GitError::InvalidPath { path: absolute });
    }
    Ok(relative)
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

fn unique_suffix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{timestamp}", std::process::id())
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
        .to_string()
}

fn run_git(directory: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = run_git_bytes(directory, args)?;
    String::from_utf8(output).map_err(|error| GitError::Command {
        message: error.to_string(),
    })
}

fn run_git_optional(directory: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .map_err(|error| GitError::Command {
            message: error.to_string(),
        })?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    } else {
        Ok(None)
    }
}

fn run_git_bytes(directory: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .map_err(|error| GitError::Command {
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(GitError::Command {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}
