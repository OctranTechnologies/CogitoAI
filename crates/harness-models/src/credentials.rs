use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

/// Where a provider credential is resolved from. This status contains no key
/// material and is safe to expose over the runtime RPC boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    Environment,
    Keychain,
    None,
    Unavailable,
}

impl CredentialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keychain => "keychain",
            Self::None => "none",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Redacted, serializable status for one provider's credential.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CredentialStatus {
    pub available: bool,
    pub source: CredentialSource,
    pub env_var: String,
}

impl CredentialStatus {
    pub fn summary(&self) -> String {
        match (self.available, self.source) {
            (true, CredentialSource::Environment) => {
                format!("provided by environment variable {}", self.env_var)
            }
            (true, CredentialSource::Keychain) => "stored in the OS credential store".to_owned(),
            (_, CredentialSource::Unavailable) => "OS credential store unavailable".to_owned(),
            _ => format!("not connected (set {} or use /connect)", self.env_var),
        }
    }
}

/// Secret wrapper whose debug output can never reveal its contents.
pub struct CredentialSecret(Zeroizing<String>);

impl CredentialSecret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for CredentialSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialSecret([redacted])")
    }
}

/// Failure to access the system credential store. The underlying platform
/// error is intentionally not retained because it may contain sensitive
/// account metadata and is not actionable at this layer.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CredentialError {
    #[error("the OS credential store is unavailable")]
    StoreUnavailable,
    #[error("provider identifier is invalid")]
    InvalidProvider,
    #[error("credential must not be empty")]
    EmptyCredential,
}

/// Backend interface allows deterministic tests while production uses the OS
/// credential manager through the established `keyring` crate.
pub trait KeychainBackend: Send + Sync {
    fn get(&self, provider_id: &str) -> Result<Option<String>, CredentialError>;
    fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError>;
    fn delete(&self, provider_id: &str) -> Result<(), CredentialError>;
}

/// Unified environment-first credential resolver and OS-keychain writer.
pub trait CredentialStore: Send + Sync {
    fn get(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<Option<CredentialSecret>, CredentialError>;
    fn status(&self, provider_id: &str, env_var: &str)
        -> Result<CredentialStatus, CredentialError>;
    fn store(
        &self,
        provider_id: &str,
        env_var: &str,
        secret: &str,
    ) -> Result<CredentialStatus, CredentialError>;
    fn disconnect(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError>;
}

/// Environment-only implementation retained for isolated tests and headless
/// tools that deliberately do not access an OS credential manager.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvironmentCredentialStore;

impl CredentialStore for EnvironmentCredentialStore {
    fn get(
        &self,
        _provider_id: &str,
        env_var: &str,
    ) -> Result<Option<CredentialSecret>, CredentialError> {
        let Some(secret) = std::env::var(env_var)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(Zeroizing::new)
        else {
            return Ok(None);
        };
        harness_core::register_sensitive_value(&secret);
        Ok(Some(CredentialSecret(secret)))
    }

    fn status(
        &self,
        _provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        let available = std::env::var(env_var)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        Ok(CredentialStatus {
            available,
            source: if available {
                CredentialSource::Environment
            } else {
                CredentialSource::None
            },
            env_var: env_var.to_owned(),
        })
    }

    fn store(
        &self,
        _provider_id: &str,
        _env_var: &str,
        _secret: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        Err(CredentialError::StoreUnavailable)
    }

    fn disconnect(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        self.status(provider_id, env_var)
    }
}

#[derive(Clone)]
pub struct SystemCredentialStore {
    keychain: Arc<dyn KeychainBackend>,
}

impl fmt::Debug for SystemCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemCredentialStore")
            .field("keychain", &"OS credential manager")
            .finish()
    }
}

impl SystemCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_backend(keychain: Arc<dyn KeychainBackend>) -> Self {
        Self { keychain }
    }

    fn environment_credential(&self, env_var: &str) -> Option<String> {
        std::env::var(env_var)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    fn resolve_source(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialSource, CredentialError> {
        if self
            .environment_credential(env_var)
            .map(Zeroizing::new)
            .is_some()
        {
            return Ok(CredentialSource::Environment);
        }
        match self.keychain.get(provider_id) {
            Ok(Some(secret)) => {
                if !Zeroizing::new(secret).trim().is_empty() {
                    Ok(CredentialSource::Keychain)
                } else {
                    Ok(CredentialSource::None)
                }
            }
            Ok(_) => Ok(CredentialSource::None),
            Err(CredentialError::StoreUnavailable) => Ok(CredentialSource::Unavailable),
            Err(error) => Err(error),
        }
    }

    fn make_status(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        let source = self.resolve_source(provider_id, env_var)?;
        Ok(CredentialStatus {
            available: matches!(
                source,
                CredentialSource::Environment | CredentialSource::Keychain
            ),
            source,
            env_var: env_var.to_owned(),
        })
    }
}

impl Default for SystemCredentialStore {
    fn default() -> Self {
        Self {
            keychain: Arc::new(OsKeychain),
        }
    }
}

impl CredentialStore for SystemCredentialStore {
    fn get(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<Option<CredentialSecret>, CredentialError> {
        if let Some(secret) = self.environment_credential(env_var).map(Zeroizing::new) {
            harness_core::register_sensitive_value(&secret);
            return Ok(Some(CredentialSecret(secret)));
        }
        let secret = self.keychain.get(provider_id)?.map(Zeroizing::new);
        if let Some(secret) = secret.filter(|value| !value.trim().is_empty()) {
            harness_core::register_sensitive_value(&secret);
            Ok(Some(CredentialSecret(secret)))
        } else {
            Ok(None)
        }
    }

    fn status(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        self.make_status(provider_id, env_var)
    }

    fn store(
        &self,
        provider_id: &str,
        env_var: &str,
        secret: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        validate_provider_id(provider_id)?;
        if secret.trim().is_empty() {
            return Err(CredentialError::EmptyCredential);
        }
        harness_core::register_sensitive_value(secret);
        self.keychain.set(provider_id, secret)?;
        self.make_status(provider_id, env_var)
    }

    fn disconnect(
        &self,
        provider_id: &str,
        env_var: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        validate_provider_id(provider_id)?;
        self.keychain.delete(provider_id)?;
        self.make_status(provider_id, env_var)
    }
}

#[derive(Debug, Default)]
struct OsKeychain;

impl KeychainBackend for OsKeychain {
    fn get(&self, provider_id: &str) -> Result<Option<String>, CredentialError> {
        validate_provider_id(provider_id)?;
        let entry = keyring_entry(provider_id)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(CredentialError::StoreUnavailable),
        }
    }

    fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError> {
        validate_provider_id(provider_id)?;
        keyring_entry(provider_id)?
            .set_password(secret)
            .map_err(|_| CredentialError::StoreUnavailable)
    }

    fn delete(&self, provider_id: &str) -> Result<(), CredentialError> {
        validate_provider_id(provider_id)?;
        match keyring_entry(provider_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(CredentialError::StoreUnavailable),
        }
    }
}

fn keyring_entry(provider_id: &str) -> Result<keyring::Entry, CredentialError> {
    keyring::Entry::new("CogitoAI", provider_id).map_err(|_| CredentialError::StoreUnavailable)
}

fn validate_provider_id(provider_id: &str) -> Result<(), CredentialError> {
    if provider_id.is_empty()
        || provider_id.len() > 80
        || !provider_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CredentialError::InvalidProvider);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryKeychain(Mutex<HashMap<String, String>>);

    impl KeychainBackend for MemoryKeychain {
        fn get(&self, provider_id: &str) -> Result<Option<String>, CredentialError> {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(provider_id)
                .cloned())
        }

        fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(provider_id.to_owned(), secret.to_owned());
            Ok(())
        }

        fn delete(&self, provider_id: &str) -> Result<(), CredentialError> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(provider_id);
            Ok(())
        }
    }

    #[test]
    fn reads_from_keychain_and_supports_reconnect_and_disconnect() {
        let store = SystemCredentialStore::with_backend(Arc::new(MemoryKeychain::default()));
        assert_eq!(
            store
                .status("openai", "COGITO_TEST_OPENAI_KEY")
                .unwrap()
                .source,
            CredentialSource::None
        );
        store
            .store("openai", "COGITO_TEST_OPENAI_KEY", "test-keychain-secret-1")
            .unwrap();
        let secret = store
            .get("openai", "COGITO_TEST_OPENAI_KEY")
            .unwrap()
            .unwrap();
        assert_eq!(secret.expose_secret(), "test-keychain-secret-1");
        assert_eq!(format!("{secret:?}"), "CredentialSecret([redacted])");
        store
            .store("openai", "COGITO_TEST_OPENAI_KEY", "test-keychain-secret-2")
            .unwrap();
        assert_eq!(
            store
                .disconnect("openai", "COGITO_TEST_OPENAI_KEY")
                .unwrap()
                .source,
            CredentialSource::None
        );
    }

    #[test]
    fn environment_credential_takes_priority_over_keychain() {
        const ENV: &str = "COGITO_CREDENTIALS_TEST_PRIORITY_KEY";
        let store = SystemCredentialStore::with_backend(Arc::new(MemoryKeychain::default()));
        store
            .store("openai", ENV, "keychain-secret-should-not-win")
            .unwrap();
        std::env::set_var(ENV, "environment-secret-wins");

        let status = store.status("openai", ENV).unwrap();
        let credential = store.get("openai", ENV).unwrap().unwrap();
        assert_eq!(status.source, CredentialSource::Environment);
        assert_eq!(credential.expose_secret(), "environment-secret-wins");

        std::env::remove_var(ENV);
        assert_eq!(
            store.status("openai", ENV).unwrap().source,
            CredentialSource::Keychain
        );
    }

    #[test]
    fn empty_secrets_and_unsafe_provider_names_are_rejected() {
        let store = SystemCredentialStore::with_backend(Arc::new(MemoryKeychain::default()));
        assert_eq!(
            store.store("openai", "OPENAI_API_KEY", "  "),
            Err(CredentialError::EmptyCredential)
        );
        assert_eq!(
            store.store("../openai", "OPENAI_API_KEY", "secret-value"),
            Err(CredentialError::InvalidProvider)
        );
    }
}
