//! Execution backends for agent-owned workspace and process operations.
//!
//! `LocalExecutionEnvironment` is the v1 backend. The trait keeps the policy
//! boundary above the backend so a future container implementation cannot
//! grant permissions by itself.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use harness_policy::NetworkAccess;
use tempfile::NamedTempFile;

use crate::{
    CancellationToken, ProcessError, ProcessEvent, ProcessRequest, ProcessResult, ProcessRunner,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub created_at: SystemTime,
}

/// Workspace and process capabilities available to coding tools.
///
/// Implementations must treat all paths as workspace-relative and validate
/// their final resolved location. A backend may return `None` from
/// `snapshot_workspace` when it has no snapshot facility.
pub trait ExecutionEnvironment: ProcessRunner {
    fn spawn_process(
        &self,
        workspace_root: &Path,
        request: ProcessRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError> {
        let root = fs::canonicalize(workspace_root).map_err(|error| ProcessError::Spawn {
            message: format!("could not resolve workspace: {error}"),
        })?;
        let cwd =
            fs::canonicalize(&request.working_directory).map_err(|error| ProcessError::Spawn {
                message: format!("could not resolve process working directory: {error}"),
            })?;
        if !cwd.starts_with(&root) {
            return Err(ProcessError::Spawn {
                message: "process working directory is outside the workspace".to_owned(),
            });
        }
        self.execute(request, cancellation, on_event)
    }

    fn read_workspace_file(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        max_bytes: u64,
    ) -> io::Result<Vec<u8>>;

    fn write_workspace_file_atomic(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        expected_contents: Option<&[u8]>,
        contents: &[u8],
    ) -> io::Result<bool>;

    fn create_workspace_file_atomic(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        contents: &[u8],
    ) -> io::Result<bool>;

    fn delete_workspace_file(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        expected_contents: &[u8],
    ) -> io::Result<bool>;

    fn move_workspace_file(
        &self,
        workspace_root: &Path,
        source: &Path,
        destination: &Path,
        expected_contents: &[u8],
    ) -> io::Result<bool>;

    /// Read a named process variable from the local account environment. Tools
    /// must classify secret access before calling this method.
    fn environment_variable(&self, name: &OsStr) -> Option<OsString>;

    /// Variables safe to inherit into an agent-started process. Provider
    /// credentials and common secret variables are always excluded.
    fn process_environment(&self) -> BTreeMap<OsString, OsString>;

    fn network_access(&self) -> NetworkAccess;

    fn snapshot_workspace(&self, workspace_root: &Path) -> io::Result<Option<WorkspaceSnapshot>>;
}

/// The local v1 environment. Process execution is local and does not provide
/// OS-level network isolation; recognized network actions are controlled by
/// `PolicyEngine` using `network_access`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalExecutionEnvironment {
    network_access: NetworkAccess,
}

impl LocalExecutionEnvironment {
    pub const fn new(network_access: NetworkAccess) -> Self {
        Self { network_access }
    }
}

impl Default for LocalExecutionEnvironment {
    fn default() -> Self {
        Self::new(NetworkAccess::Ask)
    }
}

impl ProcessRunner for LocalExecutionEnvironment {
    fn execute(
        &self,
        request: ProcessRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError> {
        crate::LocalProcessRunner.execute(request, cancellation, on_event)
    }
}

impl ExecutionEnvironment for LocalExecutionEnvironment {
    fn read_workspace_file(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        max_bytes: u64,
    ) -> io::Result<Vec<u8>> {
        let path = resolve_workspace_path(workspace_root, relative_path, false)?;
        let file = fs::File::open(path)?;
        let mut contents = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file exceeds the {max_bytes}-byte read limit"),
            ));
        }
        Ok(contents)
    }

    fn write_workspace_file_atomic(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        expected_contents: Option<&[u8]>,
        contents: &[u8],
    ) -> io::Result<bool> {
        let path = resolve_workspace_path(workspace_root, relative_path, true)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "workspace file has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(contents)?;
        temporary.as_file().sync_all()?;
        if let Ok(metadata) = fs::metadata(&path) {
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
        }
        if let Some(expected) = expected_contents {
            match fs::read(&path) {
                Ok(current) if current == expected => {}
                _ => return Ok(false),
            }
        }
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(true)
    }

    fn create_workspace_file_atomic(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        contents: &[u8],
    ) -> io::Result<bool> {
        let path = resolve_workspace_path(workspace_root, relative_path, true)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "workspace file has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(contents)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.error),
        }
    }

    fn delete_workspace_file(
        &self,
        workspace_root: &Path,
        relative_path: &Path,
        expected_contents: &[u8],
    ) -> io::Result<bool> {
        let path = resolve_workspace_path(workspace_root, relative_path, false)?;
        match fs::read(&path) {
            Ok(contents) if contents == expected_contents => fs::remove_file(path).map(|()| true),
            _ => Ok(false),
        }
    }

    fn move_workspace_file(
        &self,
        workspace_root: &Path,
        source: &Path,
        destination: &Path,
        expected_contents: &[u8],
    ) -> io::Result<bool> {
        let source = resolve_workspace_path(workspace_root, source, false)?;
        let destination = resolve_workspace_path(workspace_root, destination, true)?;
        if fs::read(&source)? != expected_contents || destination.exists() {
            return Ok(false);
        }
        let parent = destination.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "workspace file has no parent")
        })?;
        fs::create_dir_all(parent)?;
        fs::rename(source, destination)?;
        Ok(true)
    }

    fn environment_variable(&self, name: &OsStr) -> Option<OsString> {
        let name = name.to_string_lossy();
        self.process_environment()
            .into_iter()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(&name))
            .map(|(_, value)| value)
    }

    fn process_environment(&self) -> BTreeMap<OsString, OsString> {
        filtered_process_environment()
    }

    fn network_access(&self) -> NetworkAccess {
        self.network_access
    }

    fn snapshot_workspace(&self, _workspace_root: &Path) -> io::Result<Option<WorkspaceSnapshot>> {
        Ok(None)
    }
}

/// Shared default for standalone tools and unit tests.
pub fn local_execution_environment() -> &'static LocalExecutionEnvironment {
    static ENVIRONMENT: LocalExecutionEnvironment =
        LocalExecutionEnvironment::new(NetworkAccess::Ask);
    &ENVIRONMENT
}

fn resolve_workspace_path(
    workspace_root: &Path,
    relative_path: &Path,
    allow_missing_tail: bool,
) -> io::Result<PathBuf> {
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path must remain inside the workspace",
        ));
    }
    let root = fs::canonicalize(workspace_root)?;
    let candidate = root.join(relative_path);
    if !allow_missing_tail {
        let resolved = fs::canonicalize(candidate)?;
        if !resolved.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path resolves outside the workspace",
            ));
        }
        return Ok(resolved);
    }
    let mut existing = candidate.clone();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid workspace path"))?;
        suffix.push(name.to_os_string());
        if !existing.pop() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path escaped workspace",
            ));
        }
    }
    let base = fs::canonicalize(existing)?;
    if !base.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path resolves outside the workspace",
        ));
    }
    Ok(suffix
        .into_iter()
        .rev()
        .fold(base, |path, part| path.join(part)))
}

pub(crate) fn filtered_process_environment() -> BTreeMap<OsString, OsString> {
    // Preserve variables needed to launch development tools and native
    // processes, but never pass provider credentials or conventional secrets.
    filter_process_environment(std::env::vars_os())
}

fn filter_process_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    const SAFE_NAMES: &[&str] = &[
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "HOMEDRIVE",
        "HOMEPATH",
        "COMSPEC",
        "PATHEXT",
        "LANG",
        "LC_ALL",
        "TERM",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PNPM_HOME",
        "NVM_HOME",
        "NVM_SYMLINK",
        "GOPATH",
        "GOROOT",
        "VIRTUAL_ENV",
    ];
    variables
        .into_iter()
        .filter(|(key, _)| {
            let name = key.to_string_lossy();
            SAFE_NAMES
                .iter()
                .any(|safe| name.eq_ignore_ascii_case(safe))
                && !is_secret_environment_name(&name)
        })
        .collect()
}

fn is_secret_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|suffix| name.contains(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_operations_are_atomic_and_conflict_checked() {
        let temporary = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let environment = LocalExecutionEnvironment::default();
        let path = Path::new("src/lib.rs");
        assert!(environment
            .create_workspace_file_atomic(temporary.path(), path, b"one\r\n")
            .unwrap());
        assert!(!environment
            .create_workspace_file_atomic(temporary.path(), path, b"overwrite")
            .unwrap());
        assert_eq!(
            environment
                .read_workspace_file(temporary.path(), path, 32)
                .unwrap(),
            b"one\r\n"
        );
        assert!(environment
            .write_workspace_file_atomic(temporary.path(), path, Some(b"one\r\n"), b"two\r\n")
            .unwrap());
        assert!(!environment
            .write_workspace_file_atomic(temporary.path(), path, Some(b"one\r\n"), b"stale")
            .unwrap());
        assert!(environment
            .move_workspace_file(temporary.path(), path, Path::new("src/new.rs"), b"two\r\n",)
            .unwrap());
        assert!(environment
            .delete_workspace_file(temporary.path(), Path::new("src/new.rs"), b"two\r\n")
            .unwrap());
    }

    #[test]
    fn workspace_operations_reject_path_escape() {
        let temporary = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let environment = LocalExecutionEnvironment::default();
        assert!(environment
            .create_workspace_file_atomic(temporary.path(), Path::new("../outside"), b"no")
            .is_err());
    }

    #[test]
    fn process_spawn_rejects_a_working_directory_outside_the_workspace() {
        let temporary = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let request = ProcessRequest {
            program: "this-process-must-not-start".to_owned(),
            args: Vec::new(),
            working_directory: outside,
            timeout: std::time::Duration::from_secs(1),
            max_output_bytes: 128,
        };

        let result = LocalExecutionEnvironment::default().spawn_process(
            &workspace,
            request,
            &CancellationToken::new(),
            &mut |_| Ok(()),
        );
        assert!(
            matches!(result, Err(ProcessError::Spawn { message }) if message.contains("outside the workspace"))
        );
    }

    #[test]
    fn child_process_environment_keeps_toolchain_but_drops_secrets() {
        let variables = [
            (OsString::from("PATH"), OsString::from("/tools")),
            (OsString::from("CARGO_HOME"), OsString::from("/cargo")),
            (
                OsString::from("OPENAI_API_KEY"),
                OsString::from("do-not-forward"),
            ),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("do-not-forward"),
            ),
            (
                OsString::from("CUSTOM_PASSWORD"),
                OsString::from("do-not-forward"),
            ),
        ];
        let filtered = filter_process_environment(variables);
        assert_eq!(
            filtered.get(OsStr::new("PATH")),
            Some(&OsString::from("/tools"))
        );
        assert_eq!(
            filtered.get(OsStr::new("CARGO_HOME")),
            Some(&OsString::from("/cargo"))
        );
        assert!(!filtered.contains_key(OsStr::new("OPENAI_API_KEY")));
        assert!(!filtered.contains_key(OsStr::new("AWS_SECRET_ACCESS_KEY")));
        assert!(!filtered.contains_key(OsStr::new("CUSTOM_PASSWORD")));
    }

    #[test]
    fn environment_lookup_uses_the_safe_allowlist() {
        let environment = LocalExecutionEnvironment::default();
        assert_eq!(
            environment.environment_variable(OsStr::new("OPENAI_API_KEY")),
            None
        );
        assert_eq!(
            environment.environment_variable(OsStr::new("AWS_SECRET_ACCESS_KEY")),
            None
        );
        assert_eq!(
            environment.environment_variable(OsStr::new("PATH")),
            std::env::var_os("PATH")
        );
    }
}
