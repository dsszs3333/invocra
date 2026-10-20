use std::collections::HashMap;
use std::sync::Arc;

use config::{GroupEntry, ProjectEntry};
use gitlab::{GitLabClient, GitLabError};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::cache::SnapshotOrigin;

use crate::state::{ProjectSnapshot, ProjectStatus};
use crate::AppEvent;

/// Limits parallel project fetches: groups can resolve to 100+ projects, three requests each.
const REFRESH_CONCURRENCY: usize = 8;
pub(crate) static REFRESH_LIMIT: Semaphore = Semaphore::const_new(REFRESH_CONCURRENCY);

/// Превращает ошибку GitLab в текст для пользователя; при 401 заменяет сырое
/// "401 Unauthorized" на подсказку про переавторизацию.
pub(crate) fn describe_gitlab_error(err: &GitLabError) -> String {
    auth_aware_message(err.is_unauthorized(), &err.to_string())
}

fn auth_aware_message(unauthorized: bool, raw: &str) -> String {
    if unauthorized {
        "Token expired or invalid — re-authenticate: invocra auth login".to_string()
    } else {
        raw.to_string()
    }
}

/// Where dashboard projects come from: static config entries plus auto-resolved groups.
#[derive(Debug, Clone, Default)]
pub struct RefreshSources {
    pub projects: Vec<ProjectEntry>,
    pub groups: Vec<GroupEntry>,
}

#[derive(Debug)]
pub(crate) struct RefreshReport {
    /// Which dashboard instance (tab) produced this report; the run loop drops reports from a tab
    /// the user has since switched away from.
    pub(crate) instance: usize,
    pub(crate) generation: u64,
    pub(crate) snapshots: Vec<ProjectSnapshot>,
    pub(crate) errors: usize,
    pub(crate) is_complete_failure: bool,
    pub(crate) group_errors: Vec<String>,
}

impl RefreshReport {
    pub(crate) fn has_usable_data(&self) -> bool {
        !self.is_complete_failure
    }
}

pub(crate) fn spawn_refresh(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    generation: u64,
    client: GitLabClient,
    sources: RefreshSources,
    ref_overrides: HashMap<u64, String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (group_projects, group_errors) = resolve_group_projects(&client, &sources.groups).await;
        let entries = merge_project_entries(&sources.projects, group_projects);

        let semaphore = Arc::new(Semaphore::new(REFRESH_CONCURRENCY));
        let mut tasks = JoinSet::new();
        for (entry, origin) in entries {
            let client = client.clone();
            let ref_override = ref_overrides.get(&entry.id).cloned();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                fetch_project_snapshot(client, entry, origin, ref_override).await
            });
        }
        let mut snapshots: Vec<ProjectSnapshot> = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(snapshot) = result {
                snapshots.push(snapshot);
            }
        }

        let errors = snapshots
            .iter()
            .filter(|snapshot| snapshot.has_error())
            .count();
        snapshots.sort_by(|left, right| left.configured.name.cmp(&right.configured.name));
        let is_complete_failure = (!snapshots.is_empty()
            && snapshots.iter().all(ProjectSnapshot::is_failed))
            || (snapshots.is_empty() && !group_errors.is_empty());

        let _ = tx
            .send(AppEvent::RefreshCompleted(RefreshReport {
                instance,
                generation,
                snapshots,
                errors,
                is_complete_failure,
                group_errors,
            }))
            .await;
    })
}

/// Re-fetches a single project after a mutating action; cheaper than a full refresh when the
/// dashboard tracks a hundred group projects. `delay` lets the caller wait for the server to catch
/// up before fetching (e.g. a tag-triggered pipeline that registers a moment after the tag).
pub(crate) fn spawn_refresh_one(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    generation: u64,
    client: GitLabClient,
    entry: (ProjectEntry, SnapshotOrigin),
    ref_override: Option<String>,
    delay: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let snapshot = fetch_project_snapshot(client, entry.0, entry.1, ref_override).await;
        let _ = tx
            .send(AppEvent::ProjectRefreshed {
                instance,
                generation,
                snapshot: Box::new(snapshot),
            })
            .await;
    })
}

/// Like [`spawn_refresh_one`] but for the background auto-refresh tick: reports via
/// [`AppEvent::ProjectAutoRefreshed`] so the merge keeps an open pipeline-history view intact.
pub(crate) fn spawn_refresh_one_auto(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    generation: u64,
    client: GitLabClient,
    configured: ProjectEntry,
    origin: SnapshotOrigin,
    ref_override: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let snapshot = fetch_project_snapshot(client, configured, origin, ref_override).await;
        let _ = tx
            .send(AppEvent::ProjectAutoRefreshed {
                instance,
                generation,
                snapshot: Box::new(snapshot),
            })
            .await;
    })
}

async fn resolve_group_projects(
    client: &GitLabClient,
    groups: &[GroupEntry],
) -> (Vec<ProjectEntry>, Vec<String>) {
    let mut projects = Vec::new();
    let mut errors = Vec::new();

    for group in groups {
        let reference = match group.reference() {
            Ok(reference) => reference,
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        match client
            .list_group_projects(&reference, group.include_subgroups)
            .await
        {
            Ok(found) => projects.extend(found.into_iter().map(|project| ProjectEntry {
                name: project.path_with_namespace,
                id: project.id,
            })),
            Err(err) => errors.push(format!("{reference}: {err}")),
        }
    }

    (projects, errors)
}

/// Merges static and group-resolved entries, deduplicating by project id.
/// Static entries win so user-chosen display names are preserved.
fn merge_project_entries(
    static_projects: &[ProjectEntry],
    group_projects: Vec<ProjectEntry>,
) -> Vec<(ProjectEntry, SnapshotOrigin)> {
    let mut by_id: HashMap<u64, (ProjectEntry, SnapshotOrigin)> = group_projects
        .into_iter()
        .map(|entry| (entry.id, (entry, SnapshotOrigin::Group)))
        .collect();
    for project in static_projects {
        by_id.insert(project.id, (project.clone(), SnapshotOrigin::Static));
    }

    let mut entries: Vec<_> = by_id.into_values().collect();
    entries.sort_by(|(left, _), (right, _)| left.name.cmp(&right.name));
    entries
}

async fn fetch_project_snapshot(
    client: GitLabClient,
    configured: ProjectEntry,
    origin: SnapshotOrigin,
    ref_override: Option<String>,
) -> ProjectSnapshot {
    let _permit = REFRESH_LIMIT.acquire().await;
    let mut snapshot = ProjectSnapshot {
        bridges: Vec::new(),
        configured,
        project: None,
        pipeline: None,
        jobs: Vec::new(),
        status: ProjectStatus::Ready,
        origin,
    };

    match client.get_project(snapshot.configured.id).await {
        Ok(project) => {
            snapshot.project = Some(project);
        }
        Err(err) => {
            snapshot.status = ProjectStatus::Failed(describe_gitlab_error(&err));
            return snapshot;
        }
    }

    // Track the override branch if set, else the repo default; fall back to the latest pipeline
    // across all refs only when neither is known (e.g. an empty repo).
    let tracked_ref = ref_override.or_else(|| {
        snapshot
            .project
            .as_ref()
            .and_then(|project| project.default_branch.clone())
    });

    match client
        .get_latest_pipeline(snapshot.configured.id, tracked_ref.as_deref())
        .await
    {
        Ok(Some(pipeline)) => {
            let pipeline_id = pipeline.id;
            snapshot.pipeline = Some(pipeline);
            match client
                .get_pipeline_jobs(snapshot.configured.id, pipeline_id)
                .await
            {
                Ok(jobs) => snapshot.jobs = jobs,
                Err(err) => {
                    snapshot.status = ProjectStatus::PartialError(describe_gitlab_error(&err));
                }
            }
            match client
                .get_pipeline_bridges(snapshot.configured.id, pipeline_id)
                .await
            {
                Ok(bridges) => snapshot.bridges = bridges,
                Err(err) => {
                    snapshot.status = ProjectStatus::PartialError(describe_gitlab_error(&err));
                }
            }
        }
        Ok(None) => {}
        Err(err) => snapshot.status = ProjectStatus::PartialError(describe_gitlab_error(&err)),
    }

    snapshot
}

#[cfg(test)]
mod tests {
    use config::ProjectEntry;

    use super::{auth_aware_message, merge_project_entries, RefreshReport};
    use crate::cache::SnapshotOrigin;
    use crate::state::{ProjectSnapshot, ProjectStatus};

    #[test]
    fn auth_aware_message_hints_reauth_on_401() {
        let msg = auth_aware_message(true, "401 Unauthorized");
        assert!(msg.contains("auth login"), "got: {msg}");
    }

    #[test]
    fn auth_aware_message_passes_other_errors_through() {
        assert_eq!(auth_aware_message(false, "offline"), "offline");
    }

    fn entry(name: &str, id: u64) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            id,
        }
    }

    #[test]
    fn refresh_report_flags_complete_failure_when_all_snapshots_failed() {
        let report = RefreshReport {
            instance: 0,
            generation: 0,
            snapshots: vec![ProjectSnapshot {
                bridges: Vec::new(),
                configured: entry("group/api", 1),
                project: None,
                pipeline: None,
                jobs: Vec::new(),
                status: ProjectStatus::Failed(String::from("offline")),
                origin: SnapshotOrigin::Static,
            }],
            errors: 1,
            is_complete_failure: true,
            group_errors: Vec::new(),
        };

        assert!(!report.has_usable_data());
    }

    #[test]
    fn refresh_report_keeps_partial_results_as_usable_data() {
        let report = RefreshReport {
            instance: 0,
            generation: 0,
            snapshots: vec![ProjectSnapshot {
                bridges: Vec::new(),
                configured: entry("group/api", 1),
                project: None,
                pipeline: None,
                jobs: Vec::new(),
                status: ProjectStatus::PartialError(String::from("jobs request failed")),
                origin: SnapshotOrigin::Static,
            }],
            errors: 1,
            is_complete_failure: false,
            group_errors: Vec::new(),
        };

        assert!(report.has_usable_data());
    }

    #[test]
    fn merge_prefers_static_entries_over_group_duplicates() {
        let static_projects = vec![entry("custom-name", 1)];
        let group_projects = vec![entry("group/api", 1), entry("group/web", 2)];

        let merged = merge_project_entries(&static_projects, group_projects);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0.name, "custom-name");
        assert_eq!(merged[0].1, SnapshotOrigin::Static);
        assert_eq!(merged[1].0.name, "group/web");
        assert_eq!(merged[1].1, SnapshotOrigin::Group);
    }

    #[test]
    fn merge_sorts_entries_by_name() {
        let merged = merge_project_entries(
            &[entry("b/api", 1)],
            vec![entry("a/web", 2), entry("c/cli", 3)],
        );

        let names: Vec<_> = merged
            .iter()
            .map(|(entry, _)| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["a/web", "b/api", "c/cli"]);
    }
}
