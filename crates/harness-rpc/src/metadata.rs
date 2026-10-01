//! Per-user metadata used to discover local Harness RPC runtimes.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const RPC_TRANSPORT_TCP_LOOPBACK: &str = "tcp-loopback";
pub const RUNTIME_METADATA_FORMAT_VERSION: u32 = 1;

/// Non-sensitive information needed to find and verify a local RPC runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeMetadata {
    pub metadata_version: u32,
    pub pid: u32,
    pub protocol_version: u32,
    pub transport: String,
    pub endpoint: String,
    pub started_at: u64,
    pub runtime_version: String,
    pub instance_id: String,
}

impl RuntimeMetadata {
    pub fn new(endpoint: std::net::SocketAddr, instance_id: impl Into<String>) -> Self {
        Self {
            metadata_version: RUNTIME_METADATA_FORMAT_VERSION,
            pid: std::process::id(),
            protocol_version: crate::RPC_PROTOCOL_VERSION,
            transport: RPC_TRANSPORT_TCP_LOOPBACK.to_owned(),
            endpoint: endpoint.to_string(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_id: instance_id.into(),
        }
    }
}

/// A snapshot preserves the exact bytes so stale cleanup cannot delete a newer
/// metadata file written by a competing process.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum MetadataSnapshot {
    Missing,
    Invalid(Vec<u8>),
    Valid(RuntimeMetadata, Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct RuntimeMetadataStore {
    directory: PathBuf,
}

impl RuntimeMetadataStore {
    pub fn for_current_user() -> Self {
        Self::new(default_runtime_directory())
    }

    /// Uses the platform application-data path when it is writable, falling
    /// back to a per-user temporary runtime directory in restricted installs.
    pub fn for_current_user_or_fallback() -> Self {
        let preferred = Self::for_current_user();
        if preferred.ensure_private_directory().is_ok() {
            return preferred;
        }
        let fallback = Self::new(fallback_runtime_directory());
        let _ = fallback.ensure_private_directory();
        fallback
    }

    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn metadata_path(&self, workspace_root: &Path) -> PathBuf {
        self.directory
            .join(format!("runtime-{}.json", workspace_key(workspace_root)))
    }

    pub(crate) fn startup_lock_path(&self, workspace_root: &Path) -> PathBuf {
        self.directory
            .join(format!("runtime-{}.lock", workspace_key(workspace_root)))
    }

    pub(crate) fn read(&self, workspace_root: &Path) -> io::Result<MetadataSnapshot> {
        let path = self.metadata_path(workspace_root);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(MetadataSnapshot::Missing)
            }
            Err(error) => return Err(error),
        };
        match serde_json::from_slice(&bytes) {
            Ok(metadata) => Ok(MetadataSnapshot::Valid(metadata, bytes)),
            Err(_) => Ok(MetadataSnapshot::Invalid(bytes)),
        }
    }

    pub(crate) fn write(
        &self,
        workspace_root: &Path,
        metadata: &RuntimeMetadata,
    ) -> io::Result<()> {
        self.ensure_private_directory()?;
        let destination = self.metadata_path(workspace_root);
        let temporary = self.directory.join(format!(
            ".runtime-{}-{}-{}.tmp",
            workspace_key(workspace_root),
            std::process::id(),
            sanitize_instance_id(&metadata.instance_id),
        ));
        let bytes = serde_json::to_vec_pretty(metadata)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_mode(&mut options);
        let mut file = options.open(&temporary)?;
        if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn remove_if_unchanged(
        &self,
        workspace_root: &Path,
        snapshot: &[u8],
    ) -> io::Result<bool> {
        let path = self.metadata_path(workspace_root);
        match fs::read(&path) {
            Ok(current) if current == snapshot => {
                fs::remove_file(path)?;
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn ensure_private_directory(&self) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Returns the platform's per-user runtime/application-data directory.
pub fn default_runtime_directory() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(root).join("CogitoAI").join("runtime");
        }
        if let Some(root) = std::env::var_os("APPDATA") {
            return PathBuf::from(root).join("CogitoAI").join("runtime");
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("CogitoAI")
            .join("runtime");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("cogitoai");
        }
        if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(state).join("cogitoai").join("runtime");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("cogitoai")
                .join("runtime");
        }
    }

    fallback_runtime_directory()
}

fn fallback_runtime_directory() -> PathBuf {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown-user".to_owned());
    let user = user
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::temp_dir()
        .join("CogitoAI")
        .join(user)
        .join("runtime")
}

pub(crate) fn new_instance_id() -> String {
    static INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{timestamp:x}-{sequence:x}", std::process::id())
}

/// Checks process existence without trusting the PID as runtime identity.
/// `None` means the platform check was unavailable; callers must still probe RPC.
pub(crate) fn process_exists(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }

    #[cfg(windows)]
    {
        use std::process::Command;

        // The numeric PID is the only interpolated value, so this command
        // cannot carry user input. PowerShell is available on supported
        // Windows versions; an unavailable process query remains best-effort.
        let script = format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        return Some(output.status.success());
    }

    #[cfg(unix)]
    {
        use std::process::Command;

        let output = Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .ok()?;
        return Some(output.status.success());
    }

    #[allow(unreachable_code)]
    None
}

fn workspace_key(workspace_root: &Path) -> String {
    let canonical =
        fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let path = canonical.to_string_lossy();
    #[cfg(windows)]
    let path = path.to_lowercase();
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn sanitize_instance_id(instance_id: &str) -> String {
    instance_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(40)
        .collect()
}

fn set_private_file_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_contains_only_discovery_fields_and_round_trips() {
        let directory = tempfile::tempdir().expect("runtime directory");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = RuntimeMetadataStore::new(directory.path());
        let endpoint = "127.0.0.1:43210".parse().expect("endpoint");
        let metadata = RuntimeMetadata::new(endpoint, "abc123");
        store
            .write(workspace.path(), &metadata)
            .expect("write metadata");

        let bytes = fs::read(store.metadata_path(workspace.path())).expect("read metadata");
        let text = String::from_utf8(bytes).expect("metadata is UTF-8");
        assert!(!text.contains(&workspace.path().display().to_string()));
        assert!(!text.contains("api_key"));
        assert!(!text.contains("secret"));
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid metadata JSON");
        let keys = value
            .as_object()
            .expect("metadata object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            [
                "endpoint",
                "instanceId",
                "metadataVersion",
                "pid",
                "protocolVersion",
                "runtimeVersion",
                "startedAt",
                "transport",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            store.read(workspace.path()).unwrap(),
            MetadataSnapshot::Valid(metadata, text.into_bytes())
        );
    }

    #[test]
    fn malformed_metadata_is_distinguished_and_removed_only_if_unchanged() {
        let directory = tempfile::tempdir().expect("runtime directory");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = RuntimeMetadataStore::new(directory.path());
        fs::create_dir_all(store.directory()).expect("create directory");
        let path = store.metadata_path(workspace.path());
        fs::write(&path, b"not json").expect("write malformed metadata");
        let MetadataSnapshot::Invalid(snapshot) = store.read(workspace.path()).unwrap() else {
            panic!("metadata should be invalid");
        };
        fs::write(&path, b"a newer file").expect("replace metadata");
        assert!(!store
            .remove_if_unchanged(workspace.path(), &snapshot)
            .unwrap());
        assert!(path.exists());
    }

    #[test]
    fn per_user_runtime_directory_has_platform_specific_location() {
        let path = default_runtime_directory();
        #[cfg(windows)]
        assert!(path.to_string_lossy().contains("CogitoAI\\runtime"));
        #[cfg(target_os = "macos")]
        assert!(path
            .to_string_lossy()
            .contains("Application Support/CogitoAI/runtime"));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(path.to_string_lossy().to_lowercase().contains("cogitoai"));
    }

    #[test]
    fn pid_check_detects_the_current_process() {
        assert_eq!(process_exists(std::process::id()), Some(true));
    }
}
