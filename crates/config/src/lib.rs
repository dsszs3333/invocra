//! Configuration loading, bootstrap and persistence for Invocra.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const EXAMPLE_CONFIG: &str = include_str!("../projects.example.toml");

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to resolve home directory")]
    HomeDirUnavailable,
    #[error("failed to create config directory at {path}")]
    CreateConfigDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write sample config to {path}")]
    WriteSampleConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read config from {path}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config in {path}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to parse bundled example config")]
    ParseBundledConfig {
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize config")]
    SerializeConfig {
        #[source]
        source: toml::ser::Error,
    },
    #[error("failed to write config to {path}")]
    WriteConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid [[group]] entry: exactly one of `id` or `path` must be set")]
    InvalidGroupEntry,
    #[error("config mixes the legacy top-level `gitlab_url` form with `[[instance]]`; use one or the other")]
    MixedConfigFormats,
    #[error("config has no GitLab instances")]
    NoInstances,
    #[error("`[[instance]]` entry has an empty `url`")]
    EmptyInstanceUrl,
    #[error("duplicate GitLab instance url `{url}`")]
    DuplicateInstance { url: String },
    #[error("instance url `{url}` is not http(s); use an https:// URL")]
    UnsupportedUrlScheme { url: String },
    #[error("instance url `{url}` uses plaintext http; the token would be sent in the clear. Set `allow_insecure_http = true` on this instance only if the network is trusted (e.g. VPN-only)")]
    InsecureUrl { url: String },
}

/// Checks an instance URL's transport: `https://` is always fine, `http://` only when explicitly
/// allowed (trusted network), anything else is rejected. Shared by config validation and the
/// onboarding wizard so both enforce the same rule.
///
/// # Errors
///
/// Returns [`ConfigError::UnsupportedUrlScheme`] for non-http(s) URLs, or
/// [`ConfigError::InsecureUrl`] for `http://` without `allow_insecure_http`.
pub fn check_url_scheme(url: &str, allow_insecure_http: bool) -> Result<()> {
    let url = url.trim();
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") {
        return if allow_insecure_http {
            Ok(())
        } else {
            Err(ConfigError::InsecureUrl {
                url: url.to_string(),
            })
        };
    }
    Err(ConfigError::UnsupportedUrlScheme {
        url: url.to_string(),
    })
}

/// A configured GitLab project entry persisted in `projects.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectEntry {
    pub name: String,
    pub id: u64,
}

/// A configured GitLab group whose projects are resolved automatically.
///
/// Exactly one of `id` or `path` must be set; see [`GroupEntry::reference`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GroupEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub include_subgroups: bool,
}

impl GroupEntry {
    /// Returns the group identifier usable in API URLs: a numeric id or a full path.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidGroupEntry`] unless exactly one of `id`/`path` is set.
    pub fn reference(&self) -> Result<String> {
        match (self.id, self.path.as_deref()) {
            (Some(id), None) => Ok(id.to_string()),
            (None, Some(path)) if !path.trim().is_empty() => Ok(path.trim().to_string()),
            _ => Err(ConfigError::InvalidGroupEntry),
        }
    }
}

/// A single configured GitLab instance: its URL and the projects/groups tracked on it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstanceConfig {
    pub url: String,
    /// Optional HTTP/HTTPS/SOCKS5 proxy this instance's requests route through, e.g.
    /// `socks5://127.0.0.1:1080`. Lets a VPN-only instance use a tunnel without forcing the others
    /// through it. Absent = direct connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Opt-in to sending the token over a plaintext `http://` URL. Off by default so a typo or
    /// MITM downgrade can't leak the token in cleartext; set `true` only for a trusted network
    /// (e.g. a self-hosted instance reachable only over VPN, where the tunnel provides transport
    /// security).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_insecure_http: bool,
    #[serde(rename = "project", default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectEntry>,
    #[serde(rename = "group", default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupEntry>,
}

/// Global, instance-independent preferences (the `[settings]` table).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Settings {
    /// Ring the terminal bell and post a desktop notification when a watched pipeline finishes.
    #[serde(default = "default_true")]
    pub notifications: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notifications: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Top-level user configuration: one or more GitLab instances plus global settings.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(rename = "instance")]
    pub instances: Vec<InstanceConfig>,
    #[serde(default)]
    pub settings: Settings,
}

/// On-disk shape accepted by the parser. Supports both the new `[[instance]]` form and the legacy
/// flat form (top-level `gitlab_url` + `[[project]]`/`[[group]]`), normalized into [`AppConfig`].
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    gitlab_url: Option<String>,
    #[serde(rename = "project", default)]
    projects: Vec<ProjectEntry>,
    #[serde(rename = "group", default)]
    groups: Vec<GroupEntry>,
    #[serde(rename = "instance", default)]
    instances: Vec<InstanceConfig>,
    #[serde(default)]
    settings: Settings,
}

impl RawConfig {
    /// Normalizes the raw on-disk shape into an [`AppConfig`], folding any legacy flat form into a
    /// single instance.
    fn into_app_config(self) -> Result<AppConfig> {
        let has_legacy =
            self.gitlab_url.is_some() || !self.projects.is_empty() || !self.groups.is_empty();
        if has_legacy && !self.instances.is_empty() {
            return Err(ConfigError::MixedConfigFormats);
        }
        if has_legacy {
            let url = self.gitlab_url.ok_or(ConfigError::EmptyInstanceUrl)?;
            return Ok(AppConfig {
                instances: vec![InstanceConfig {
                    url,
                    proxy: None,
                    allow_insecure_http: false,
                    projects: self.projects,
                    groups: self.groups,
                }],
                settings: self.settings,
            });
        }
        Ok(AppConfig {
            instances: self.instances,
            settings: self.settings,
        })
    }
}

/// Replaces the instance with the same `url`, or appends it when none matches. Used to add or
/// re-configure a single instance without disturbing the others.
pub fn upsert_instance(config: &mut AppConfig, instance: InstanceConfig) {
    if let Some(existing) = config
        .instances
        .iter_mut()
        .find(|candidate| candidate.url == instance.url)
    {
        *existing = instance;
    } else {
        config.instances.push(instance);
    }
}

impl AppConfig {
    /// The proxy configured for `url`, if that instance exists and sets one. Used to carry an
    /// instance's proxy across config rewrites that don't collect one (wizard, discovery).
    pub fn proxy_for(&self, url: &str) -> Option<String> {
        self.instances
            .iter()
            .find(|instance| instance.url == url)
            .and_then(|instance| instance.proxy.clone())
    }

    /// Whether `url` is an existing instance flagged to allow plaintext http. Used to carry the
    /// opt-in across config rewrites (wizard, discovery) that don't collect it themselves.
    pub fn allow_insecure_http_for(&self, url: &str) -> bool {
        self.instances
            .iter()
            .find(|instance| instance.url == url)
            .is_some_and(|instance| instance.allow_insecure_http)
    }

    /// Validates entries that serde cannot check structurally: at least one instance, non-empty
    /// unique urls, and well-formed `[[group]]` entries (group id/path exclusivity).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoInstances`], [`ConfigError::EmptyInstanceUrl`],
    /// [`ConfigError::DuplicateInstance`], or [`ConfigError::InvalidGroupEntry`].
    pub fn validate(&self) -> Result<()> {
        if self.instances.is_empty() {
            return Err(ConfigError::NoInstances);
        }
        let mut seen = std::collections::HashSet::new();
        for instance in &self.instances {
            let url = instance.url.trim();
            if url.is_empty() {
                return Err(ConfigError::EmptyInstanceUrl);
            }
            if !seen.insert(url) {
                return Err(ConfigError::DuplicateInstance {
                    url: url.to_string(),
                });
            }
            check_url_scheme(url, instance.allow_insecure_http)?;
            for group in &instance.groups {
                group.reference()?;
            }
        }
        Ok(())
    }
}

/// Resolves the configuration directory for the current user.
///
/// # Errors
///
/// Returns [`ConfigError::HomeDirUnavailable`] if the user home directory cannot be resolved.
pub fn config_dir() -> Result<PathBuf> {
    // ponytail: std::env::home_dir (un-deprecated since 1.85) reads $HOME on unix/mac, which is
    // all this app targets; dropped the dirs crate for it.
    std::env::home_dir()
        .map(|dir| dir.join(".config").join("invocra"))
        .ok_or(ConfigError::HomeDirUnavailable)
}

/// Resolves the full path to `projects.toml`.
///
/// # Errors
///
/// Returns any error from [`config_dir`].
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("projects.toml"))
}

/// Ensures the configuration file exists, creating a sample config if needed.
///
/// # Errors
///
/// Returns an error if the config directory cannot be created or the sample config cannot be
/// written.
pub fn ensure_config_exists() -> Result<ConfigBootstrap> {
    let path = config_path()?;
    if path.exists() {
        return Ok(ConfigBootstrap {
            path,
            created: false,
        });
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::CreateConfigDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let write_err = |source| ConfigError::WriteSampleConfig {
        path: path.clone(),
        source,
    };
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(write_err)?;
    file.write_all(EXAMPLE_CONFIG.as_bytes())
        .map_err(write_err)?;
    file.as_file().sync_all().map_err(write_err)?;
    if let Err(err) = file.persist_noclobber(&path) {
        if err.error.kind() == std::io::ErrorKind::AlreadyExists {
            return Ok(ConfigBootstrap {
                path,
                created: false,
            });
        }
        return Err(write_err(err.error));
    }

    Ok(ConfigBootstrap {
        path,
        created: true,
    })
}

/// Loads the current user configuration from disk.
///
/// # Errors
///
/// Returns an error if config bootstrap, reading, or TOML parsing fails.
pub fn load_config() -> Result<AppConfig> {
    let bootstrap = ensure_config_exists()?;
    load_config_from_path(&bootstrap.path)
}

/// Loads the bundled example configuration.
///
/// # Errors
///
/// Returns an error if the bundled TOML cannot be parsed.
pub fn default_config() -> Result<AppConfig> {
    let raw: RawConfig = toml::from_str(EXAMPLE_CONFIG)
        .map_err(|source| ConfigError::ParseBundledConfig { source })?;
    raw.into_app_config()
}

/// Loads a configuration file from an explicit path.
///
/// # Errors
///
/// Returns an error if the file cannot be read or its TOML content is invalid.
pub fn load_config_from_path(path: &Path) -> Result<AppConfig> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: RawConfig = toml::from_str(&raw).map_err(|source| ConfigError::ParseConfig {
        path: path.to_path_buf(),
        source,
    })?;
    let config = parsed.into_app_config()?;
    config.validate()?;
    Ok(config)
}

/// Persists the provided configuration to `projects.toml`.
///
/// # Errors
///
/// Returns an error if the directory cannot be created, the config cannot be serialized, or the
/// file cannot be written.
pub fn save_config(config: &AppConfig) -> Result<PathBuf> {
    let path = config_path()?;
    save_config_to_path(config, &path)?;
    Ok(path)
}

fn save_config_to_path(config: &AppConfig, path: &Path) -> Result<()> {
    config.validate()?;
    let raw =
        toml::to_string_pretty(config).map_err(|source| ConfigError::SerializeConfig { source })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::CreateConfigDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let write_err = |source| ConfigError::WriteConfig {
        path: path.to_path_buf(),
        source,
    };
    // NamedTempFile creates owner-only files before any proxy credentials are written.
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(write_err)?;
    file.write_all(raw.as_bytes()).map_err(write_err)?;
    file.as_file().sync_all().map_err(write_err)?;
    file.persist(path).map_err(|err| write_err(err.error))?;
    Ok(())
}

/// Information about config bootstrap side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigBootstrap {
    pub path: PathBuf,
    pub created: bool,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        config_dir, default_config, load_config_from_path, AppConfig, ConfigError, GroupEntry,
        InstanceConfig, ProjectEntry, Settings,
    };

    #[test]
    fn parses_legacy_flat_config_as_single_instance() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
gitlab_url = "https://gitlab.com"

[[project]]
name = "api"
id = 42
"#,
        )
        .expect("write config");

        let config = load_config_from_path(&path).expect("load config");

        assert_eq!(
            config,
            AppConfig {
                instances: vec![InstanceConfig {
                    url: "https://gitlab.com".into(),
                    proxy: None,
                    allow_insecure_http: false,
                    projects: vec![ProjectEntry {
                        name: "api".into(),
                        id: 42,
                    }],
                    groups: Vec::new(),
                }],
                settings: Settings::default(),
            }
        );
    }

    #[test]
    fn parses_multiple_instances() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
[[instance]]
url = "https://gitlab.com"

[[instance.project]]
name = "api"
id = 1

[[instance]]
url = "https://git.corp"

[[instance.group]]
path = "team"
include_subgroups = true
"#,
        )
        .expect("write config");

        let config = load_config_from_path(&path).expect("load config");

        assert_eq!(config.instances.len(), 2);
        assert_eq!(config.instances[0].url, "https://gitlab.com");
        assert_eq!(config.instances[0].projects.len(), 1);
        assert_eq!(config.instances[1].url, "https://git.corp");
        assert_eq!(config.instances[1].groups.len(), 1);
        assert!(config.instances[1].groups[0].include_subgroups);
    }

    #[test]
    fn parses_instance_with_groups_by_id_and_path() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
[[instance]]
url = "https://gitlab.com"

[[instance.group]]
id = 99

[[instance.group]]
path = "my-org/backend"
include_subgroups = true

[[instance.project]]
name = "standalone"
id = 7
"#,
        )
        .expect("write config");

        let config = load_config_from_path(&path).expect("load config");
        let instance = &config.instances[0];

        assert_eq!(instance.projects.len(), 1);
        assert_eq!(
            instance.groups,
            vec![
                GroupEntry {
                    id: Some(99),
                    path: None,
                    include_subgroups: false,
                },
                GroupEntry {
                    id: None,
                    path: Some("my-org/backend".into()),
                    include_subgroups: true,
                },
            ]
        );
        assert_eq!(instance.groups[0].reference().expect("reference"), "99");
        assert_eq!(
            instance.groups[1].reference().expect("reference"),
            "my-org/backend"
        );
    }

    #[test]
    fn rejects_mixed_legacy_and_instance_forms() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
gitlab_url = "https://gitlab.com"

[[instance]]
url = "https://git.corp"
"#,
        )
        .expect("write config");

        let err = load_config_from_path(&path).expect_err("mixed forms should fail");
        assert!(matches!(err, ConfigError::MixedConfigFormats));
    }

    #[test]
    fn rejects_duplicate_instance_urls() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
[[instance]]
url = "https://gitlab.com"

[[instance]]
url = "https://gitlab.com"
"#,
        )
        .expect("write config");

        let err = load_config_from_path(&path).expect_err("duplicate urls should fail");
        assert!(matches!(err, ConfigError::DuplicateInstance { .. }));
    }

    #[test]
    fn rejects_group_with_both_id_and_path() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            r#"
[[instance]]
url = "https://gitlab.com"

[[instance.group]]
id = 1
path = "my-org"
"#,
        )
        .expect("write config");

        let err = load_config_from_path(&path).expect_err("ambiguous group should fail");
        assert!(matches!(err, ConfigError::InvalidGroupEntry));
    }

    #[test]
    fn rejects_group_without_id_or_path() {
        let group = GroupEntry {
            id: None,
            path: None,
            include_subgroups: false,
        };
        assert!(matches!(
            group.reference(),
            Err(ConfigError::InvalidGroupEntry)
        ));
    }

    #[test]
    fn round_trips_multi_instance_config() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        let config = AppConfig {
            instances: vec![
                InstanceConfig {
                    url: "https://gitlab.example.com".into(),
                    proxy: Some("socks5://127.0.0.1:1080".into()),
                    allow_insecure_http: false,
                    projects: vec![ProjectEntry {
                        name: "group/api".into(),
                        id: 7,
                    }],
                    groups: vec![GroupEntry {
                        id: None,
                        path: Some("my-org/backend".into()),
                        include_subgroups: true,
                    }],
                },
                InstanceConfig {
                    url: "https://git.corp".into(),
                    proxy: None,
                    allow_insecure_http: false,
                    projects: vec![ProjectEntry {
                        name: "infra".into(),
                        id: 9,
                    }],
                    groups: Vec::new(),
                },
            ],
            settings: Settings::default(),
        };

        let raw = toml::to_string_pretty(&config).expect("serialize config");
        fs::write(&path, raw).expect("write config");

        let reloaded = load_config_from_path(&path).expect("reload config");
        assert_eq!(reloaded, config);
    }

    #[test]
    fn check_url_scheme_enforces_https_by_default() {
        use super::check_url_scheme;
        assert!(check_url_scheme("https://gitlab.com", false).is_ok());
        // Plaintext http is rejected unless explicitly allowed.
        assert!(matches!(
            check_url_scheme("http://git.local", false),
            Err(ConfigError::InsecureUrl { .. })
        ));
        assert!(check_url_scheme("http://git.local", true).is_ok());
        // Anything that isn't http(s) is rejected outright.
        assert!(matches!(
            check_url_scheme("ftp://git.local", true),
            Err(ConfigError::UnsupportedUrlScheme { .. })
        ));
    }

    #[test]
    fn validate_rejects_plaintext_http_without_optin() {
        let config = AppConfig {
            instances: vec![InstanceConfig {
                url: "http://git.local".into(),
                proxy: None,
                allow_insecure_http: false,
                projects: Vec::new(),
                groups: Vec::new(),
            }],
            settings: Settings::default(),
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InsecureUrl { .. })
        ));
    }

    #[test]
    #[cfg(unix)]
    fn save_config_writes_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(&path, "x").expect("seed");
        super::save_config_to_path(&super::default_config().expect("sample"), &path).expect("save");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn resolves_dot_config_directory() {
        let path = config_dir().expect("config dir");
        assert!(path.ends_with(".config/invocra"));
    }

    #[test]
    fn parses_default_config() {
        let config = default_config().expect("default config");
        assert_eq!(config.instances.len(), 1);
        assert_eq!(config.instances[0].url, "https://gitlab.com");
        assert!(!config.instances[0].projects.is_empty());
    }

    #[test]
    fn load_config_from_path_returns_parse_error_for_invalid_toml() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(&path, "gitlab_url = [").expect("write invalid config");

        let err = load_config_from_path(&path).expect_err("invalid config should fail");

        match err {
            ConfigError::ParseConfig { path: err_path, .. } => assert_eq!(err_path, path),
            other => panic!("unexpected error: {other}"),
        }
    }
    #[test]
    fn invalid_config_does_not_replace_existing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(&path, "original content").expect("seed");
        let invalid = AppConfig {
            instances: vec![],
            settings: Settings::default(),
        };
        assert!(super::save_config_to_path(&invalid, &path).is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("unchanged file"),
            "original content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_save_replaces_file_atomically_and_keeps_old_reader_intact() {
        use std::io::Read;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("projects.toml");
        fs::write(&path, "original content").expect("seed");
        let mut old_reader = fs::File::open(&path).expect("old reader");
        let cfg = super::default_config().expect("sample");
        super::save_config_to_path(&cfg, &path).expect("save");
        let mut original = String::new();
        old_reader
            .read_to_string(&mut original)
            .expect("old content");
        assert_eq!(original, "original content");
        assert_eq!(load_config_from_path(&path).expect("new config"), cfg);
    }
}
