//! Persisted dashboard cache types and their on-disk location.
//!
//! These mirror the in-memory dashboard state in a serializable form. They live in the `tui`
//! crate — the only consumer — so the `storage` crate stays a generic JSON file store, decoupled
//! from the `gitlab` and `config` crates.

use std::collections::HashMap;
use std::path::PathBuf;

use config::ProjectEntry;
use gitlab::{Bridge, Job, Pipeline, Project};
use serde::{Deserialize, Serialize};

/// Resolves a URL-bound cache path independent of the order of instance tabs.
/// Legacy caches have no server identity and must not be adopted automatically.
///
/// # Errors
///
/// Returns an error if the config directory cannot be resolved.
pub fn instance_cache_path(url: &str) -> config::Result<PathBuf> {
    Ok(config::config_dir()?.join(instance_cache_filename(url)))
}

pub(crate) fn cache_identity(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

fn instance_cache_filename(url: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cache_identity(url).as_bytes());
    let hash: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("dashboard-cache-{hash}.json")
}

/// Short display label for an instance tab: the url without its scheme.
#[must_use]
pub fn instance_label(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_end_matches('/')
        .to_string()
}

/// Persisted dashboard cache for offline startup and refresh fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CachedDashboardState {
    pub instance_url: String,
    pub saved_at_unix_seconds: u64,
    pub snapshots: Vec<CachedProjectSnapshot>,
    /// Per-project branch the dashboard tracks instead of the repo default, keyed by project id.
    /// Absent in older caches, hence `#[serde(default)]`.
    #[serde(default)]
    pub ref_overrides: HashMap<u64, String>,
}

/// Persisted form of a dashboard project snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CachedProjectSnapshot {
    pub configured: ProjectEntry,
    pub project: Option<Project>,
    pub pipeline: Option<Pipeline>,
    pub jobs: Vec<Job>,
    /// Absent in older caches, hence `#[serde(default)]`.
    #[serde(default)]
    pub bridges: Vec<Bridge>,
    pub status: CachedProjectStatus,
    #[serde(default)]
    pub origin: SnapshotOrigin,
}

/// How a project ended up on the dashboard: configured directly or resolved from a group.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub(crate) enum SnapshotOrigin {
    #[default]
    Static,
    Group,
}

/// Persisted snapshot status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) enum CachedProjectStatus {
    Ready,
    Failed(String),
    PartialError(String),
}

#[cfg(test)]
mod tests {
    use super::{CachedProjectSnapshot, SnapshotOrigin};

    #[test]
    fn cached_snapshot_defaults_origin_for_legacy_payloads() {
        // Caches written before `origin` existed must still parse, defaulting to `Static`.
        let json = r#"{
            "configured": {"name": "group/api", "id": 7},
            "project": null,
            "pipeline": null,
            "jobs": [],
            "status": "Ready"
        }"#;

        let snapshot: CachedProjectSnapshot = serde_json::from_str(json).expect("parse legacy");

        assert_eq!(snapshot.origin, SnapshotOrigin::Static);
    }

    #[test]
    fn cached_snapshot_round_trips_through_json() {
        let json = r#"{
            "configured": {"name": "group/api", "id": 7},
            "project": null,
            "pipeline": null,
            "jobs": [],
            "status": {"Failed": "boom"},
            "origin": "Group"
        }"#;

        let snapshot: CachedProjectSnapshot = serde_json::from_str(json).expect("parse");
        let reencoded = serde_json::to_string(&snapshot).expect("encode");
        let reparsed: CachedProjectSnapshot = serde_json::from_str(&reencoded).expect("reparse");

        assert_eq!(snapshot, reparsed);
        assert_eq!(snapshot.origin, SnapshotOrigin::Group);
    }
    #[test]
    fn cache_filenames_distinguish_urls_and_normalize_trailing_slashes() {
        assert_ne!(
            super::instance_cache_filename("https://git-a.example"),
            super::instance_cache_filename("https://git.a.example")
        );
        assert_ne!(
            super::instance_cache_filename("http://git.example"),
            super::instance_cache_filename("https://git.example")
        );
        assert_eq!(
            super::instance_cache_filename(" https://git.example/// "),
            super::instance_cache_filename("https://git.example")
        );
        assert!(
            super::instance_cache_filename(&format!("https://{}", "x".repeat(1000))).len() < 100
        );
    }
}
