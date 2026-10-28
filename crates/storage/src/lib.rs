//! Token storage backed by the operating system keyring and a generic JSON file cache.
//!
//! This crate intentionally knows nothing about the dashboard's domain types: [`LocalStateStore`]
//! persists any `Serialize`/`DeserializeOwned` value, so callers own the concrete cache schema.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

const SERVICE_NAME: &str = "invocra";
const ACCOUNT_NAME: &str = "personal-access-token";

/// Environment variable consulted before the OS keyring. Useful on headless/SSH machines and on
/// Linux setups where the kernel keyring is session-scoped.
pub const TOKEN_ENV_VAR: &str = "INVOCRA_GITLAB_TOKEN";

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("failed to create keyring entry")]
    CreateEntry {
        #[source]
        source: keyring::Error,
    },
    #[error("failed to save token in keyring")]
    SaveToken {
        #[source]
        source: keyring::Error,
    },
    #[error("failed to load token from keyring")]
    LoadToken {
        #[source]
        source: keyring::Error,
    },
    #[error("failed to create cache directory at {path}")]
    CreateCacheDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read cache file at {path}")]
    ReadCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to deserialize cache file at {path}")]
    DeserializeCache {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize dashboard cache")]
    SerializeCache {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write cache file at {path}")]
    WriteCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Keyring-backed storage for the GitLab personal access token.
#[derive(Debug, Clone)]
pub struct TokenStore {
    service: String,
    account: String,
}

impl Default for TokenStore {
    fn default() -> Self {
        Self::new(SERVICE_NAME, ACCOUNT_NAME)
    }
}

impl TokenStore {
    /// Creates a token store for a specific service and account pair.
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    /// Creates a token store for a specific GitLab instance, keyed by its URL. This is what lets a
    /// single keyring hold one token per instance.
    pub fn for_instance(url: impl Into<String>) -> Self {
        Self::new(SERVICE_NAME, url)
    }

    /// Saves a GitLab token in the system keyring.
    ///
    /// # Errors
    ///
    /// Returns an error if the keyring entry cannot be created or the token cannot be persisted.
    pub fn save_token(&self, token: &str) -> Result<()> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|source| StorageError::CreateEntry { source })?;
        entry
            .set_password(token)
            .map_err(|source| StorageError::SaveToken { source })?;
        Ok(())
    }

    /// Loads a GitLab token, preferring the [`TOKEN_ENV_VAR`] environment variable over the
    /// system keyring.
    ///
    /// # Errors
    ///
    /// Returns an error if the keyring entry cannot be created or the backend read fails.
    pub fn load_token(&self) -> Result<Option<String>> {
        if let Ok(token) = std::env::var(TOKEN_ENV_VAR) {
            let token = token.trim();
            if !token.is_empty() {
                return Ok(Some(token.to_string()));
            }
        }

        self.load_keyring_token()
    }

    fn load_keyring_token(&self) -> Result<Option<String>> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|source| StorageError::CreateEntry { source })?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(source) => Err(StorageError::LoadToken { source }),
        }
    }
}

/// File-backed JSON store for persisted application state (e.g. the dashboard cache).
///
/// The store is agnostic about *what* it persists: callers choose the concrete serializable type,
/// which keeps this crate free of any dependency on the domain or configuration crates.
#[derive(Debug, Clone)]
pub struct LocalStateStore {
    path: PathBuf,
}

impl LocalStateStore {
    /// Creates a local state store for an explicit cache path.
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the cache file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads and deserializes persisted state from disk, if the file exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the JSON payload is invalid.
    pub fn load_state<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StorageError::ReadCache {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|source| StorageError::DeserializeCache {
                path: self.path.clone(),
                source,
            })
    }

    /// Serializes and writes state to disk, creating the parent directory if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, the value cannot be serialized, or the
    /// cache file cannot be written.
    pub fn save_state<T: Serialize>(&self, state: &T) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| StorageError::CreateCacheDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let raw = serde_json::to_string_pretty(state)
            .map_err(|source| StorageError::SerializeCache { source })?;
        let write_err = |source| StorageError::WriteCache {
            path: self.path.clone(),
            source,
        };
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(write_err)?;
        file.write_all(raw.as_bytes()).map_err(write_err)?;
        file.as_file().sync_all().map_err(write_err)?;
        file.persist(&self.path)
            .map_err(|err| write_err(err.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use keyring::credential::{Credential, CredentialBuilderApi, CredentialPersistence};
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    use super::LocalStateStore;

    #[test]
    fn token_store_uses_invocra_service_and_url_account() {
        let store = super::TokenStore::for_instance("https://first.example");
        assert_eq!(store.service, "invocra");
        assert_eq!(store.account, "https://first.example");
        assert_eq!(super::TOKEN_ENV_VAR, "INVOCRA_GITLAB_TOKEN");
    }

    struct SeededKeyring;

    impl CredentialBuilderApi for SeededKeyring {
        fn build(
            &self,
            target: Option<&str>,
            service: &str,
            user: &str,
        ) -> keyring::Result<Box<Credential>> {
            let entry = keyring::mock::default_credential_builder().build(target, service, user)?;
            match user {
                super::ACCOUNT_NAME => entry.set_password("legacy-test-token")?,
                "https://first.example" => entry.set_password("instance-test-token")?,
                _ => {}
            }
            Ok(entry)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::EntryOnly
        }
    }

    #[test]
    fn keyring_tokens_are_isolated_by_instance_without_legacy_fallback() {
        // Use the backend path directly so a user's env token cannot hide a keyring regression.
        keyring::set_default_credential_builder(Box::new(SeededKeyring));
        let first = super::TokenStore::for_instance("https://first.example");
        assert_eq!(
            first
                .load_keyring_token()
                .expect("load instance token")
                .as_deref(),
            Some("instance-test-token")
        );
        assert_eq!(
            super::TokenStore::default()
                .load_keyring_token()
                .expect("load legacy fixture")
                .as_deref(),
            Some("legacy-test-token")
        );
        for url in ["https://second.example", "https://unrelated.example"] {
            let token = super::TokenStore::for_instance(url)
                .load_keyring_token()
                .expect("load missing instance");
            assert!(token.is_none(), "a legacy token was selected for {url}");
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        value: u32,
        label: String,
    }

    #[test]
    fn load_state_returns_none_when_cache_file_is_missing() {
        let dir = tempdir().expect("tempdir");
        let store = LocalStateStore::from_path(dir.path().join("missing.json"));

        let state: Option<Sample> = store.load_state().expect("load cache");

        assert_eq!(state, None);
    }

    #[test]
    fn save_state_round_trips_value() {
        let dir = tempdir().expect("tempdir");
        let store = LocalStateStore::from_path(dir.path().join("cache.json"));
        let expected = Sample {
            value: 42,
            label: String::from("ok"),
        };

        store.save_state(&expected).expect("save state");
        let actual: Sample = store
            .load_state()
            .expect("load state")
            .expect("cached state");

        assert_eq!(actual, expected);
    }

    #[cfg(unix)]
    #[test]
    fn save_state_restricts_cache_file_to_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("tempdir");
        let store = LocalStateStore::from_path(dir.path().join("cache.json"));

        store
            .save_state(&Sample {
                value: 1,
                label: String::from("perm"),
            })
            .expect("save state");

        let mode = std::fs::metadata(store.path())
            .expect("cache metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn load_state_returns_deserialize_error_for_invalid_json() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "{not-json").expect("write invalid json");
        let store = LocalStateStore::from_path(path);

        let err = store.load_state::<Sample>().expect_err("invalid cache");

        assert_eq!(
            err.to_string(),
            format!(
                "failed to deserialize cache file at {}",
                store.path().display()
            )
        );
    }
    #[test]
    fn serialization_failure_preserves_existing_cache() {
        struct Unserializable;
        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic serialization failure"))
            }
        }
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "original cache").expect("seed");
        let store = LocalStateStore::from_path(&path);
        assert!(store.save_state(&Unserializable).is_err());
        assert_eq!(
            std::fs::read_to_string(path).expect("unchanged file"),
            "original cache"
        );
    }
}
