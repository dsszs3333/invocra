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
    pub(crate) updated_at: HashMap<u64, std::time::Instant>,
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

        // Finish the first snapshots sooner instead of spreading budget across eight projects.
        let concurrency = if client.request_status().limit.is_some() {
            2
        } else {
            REFRESH_CONCURRENCY
        };
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut tasks = JoinSet::new();
        for (entry, origin) in entries {
            let client = client.clone();
            let ref_override = ref_overrides.get(&entry.id).cloned();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                let snapshot =
                    fetch_project_snapshot(client, entry, origin, ref_override, None, None).await;
                (snapshot, std::time::Instant::now())
            });
        }
        let mut snapshots: Vec<ProjectSnapshot> = Vec::new();
        let mut updated_at = HashMap::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok((snapshot, time)) = result {
                if !snapshot.has_error() {
                    updated_at.insert(snapshot.configured.id, time);
                }
                if tx
                    .send(AppEvent::FullRefreshProgress {
                        instance,
                        generation,
                        snapshot: Box::new(snapshot.clone()),
                        updated_at: time,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
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
                updated_at,
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
        let snapshot =
            fetch_project_snapshot(client, entry.0, entry.1, ref_override, None, None).await;
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
    snapshot: ProjectSnapshot,
    ref_override: Option<String>,
    details_due: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let snapshot = fetch_project_snapshot(
            client,
            snapshot.configured.clone(),
            snapshot.origin,
            ref_override,
            snapshot.project.clone(),
            (!details_due).then_some(&snapshot),
        )
        .await;
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
    project: Option<gitlab::Project>,
    cached_details: Option<&ProjectSnapshot>,
) -> ProjectSnapshot {
    let _permit = if client.is_background() {
        Some(REFRESH_LIMIT.acquire().await)
    } else {
        None
    };
    let mut snapshot = ProjectSnapshot {
        details_updated_at: None,
        bridges: Vec::new(),
        configured,
        project,
        pipeline: None,
        jobs: Vec::new(),
        status: ProjectStatus::Ready,
        origin,
    };

    if snapshot.project.is_none() {
        match client.get_project(snapshot.configured.id).await {
            Ok(project) => {
                snapshot.project = Some(project);
            }
            Err(err) => {
                snapshot.status = ProjectStatus::Failed(describe_gitlab_error(&err));
                return snapshot;
            }
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
            let cached = cached_details.filter(|old| {
                !old.has_error()
                    && pipeline.status.is_active()
                    && old.pipeline.as_ref().is_some_and(|previous| {
                        previous.id == pipeline.id && previous.status == pipeline.status
                    })
            });
            snapshot.pipeline = Some(pipeline);
            if let Some(old) = cached {
                snapshot.jobs = old.jobs.clone();
                snapshot.bridges = old.bridges.clone();
                snapshot.details_updated_at = old.details_updated_at;
                return snapshot;
            }
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
        Err(err) => snapshot.status = ProjectStatus::Failed(describe_gitlab_error(&err)),
    }

    if !snapshot.has_error() {
        snapshot.details_updated_at = Some(std::time::Instant::now());
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
            updated_at: Default::default(),
            instance: 0,
            generation: 0,
            snapshots: vec![ProjectSnapshot {
                details_updated_at: None,
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
            updated_at: Default::default(),
            instance: 0,
            generation: 0,
            snapshots: vec![ProjectSnapshot {
                details_updated_at: None,
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

    #[tokio::test]
    async fn offline_latest_pipeline_request_differs_from_an_empty_result() {
        let server = httpmock::MockServer::start_async().await;
        let unavailable = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines")
                    .query_param("ref", "unavailable");
                then.status(503);
            })
            .await;
        let empty = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines")
                    .query_param("ref", "empty");
                then.status(200).json_body(serde_json::json!([]));
            })
            .await;
        let client = gitlab::GitLabClient::new(server.base_url(), "test", None).expect("client");
        let project = gitlab::Project {
            id: 1,
            name: "api".into(),
            path_with_namespace: None,
            default_branch: None,
            web_url: None,
        };
        for (branch, failed) in [("unavailable", true), ("empty", false)] {
            let snapshot = super::fetch_project_snapshot(
                client.clone(),
                entry("group/api", 1),
                SnapshotOrigin::Static,
                Some(branch.into()),
                Some(project.clone()),
                None,
            )
            .await;
            assert_eq!(snapshot.is_failed(), failed, "{branch}");
            assert_eq!(snapshot.has_error(), failed, "{branch}");
            assert!(snapshot.pipeline.is_none());
        }
        unavailable.assert_async().await;
        empty.assert_async().await;
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
    #[tokio::test]
    async fn full_refresh_limits_parallel_projects_only_when_a_local_budget_is_set() {
        for (limit, expected) in [(std::num::NonZeroU32::new(u32::MAX), 2), (None, 8)] {
            let server = httpmock::MockServer::start_async().await;
            let metadata = server
                .mock_async(|when, then| {
                    when.path_matches(r"^/api/v4/projects/[0-9]+$");
                    then.status(401).delay(std::time::Duration::from_secs(5));
                })
                .await;
            let client = gitlab::GitLabClient::new(server.base_url(), "test", None)
                .expect("client")
                .with_request_policy(limit, true)
                .background();
            let (tx, _rx) = tokio::sync::mpsc::channel(128);
            let task = super::spawn_refresh(
                tx,
                0,
                1,
                client,
                super::RefreshSources {
                    projects: (1..=8)
                        .map(|id| entry(&format!("project-{id}"), id))
                        .collect(),
                    groups: vec![],
                },
                Default::default(),
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while metadata.calls_async().await < expected {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("parallel project requests should start");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let started = metadata.calls_async().await;
            task.abort();
            assert!(task.await.expect_err("cancelled").is_cancelled());
            assert_eq!(started, expected);
        }
    }

    #[tokio::test]
    async fn full_refresh_reports_each_project_before_the_slow_project_finishes() {
        let server = httpmock::MockServer::start_async().await;
        let fast = server.mock_async(|when, then| {
            when.path("/api/v4/projects/1");
            then.status(200).json_body(serde_json::json!({"id": 1, "name": "fast", "path_with_namespace": "group/fast"}));
        }).await;
        let _slow = server.mock_async(|when, then| {
            when.path("/api/v4/projects/2");
            then.status(200).delay(std::time::Duration::from_secs(5))
                .json_body(serde_json::json!({"id": 2, "name": "slow", "path_with_namespace": "group/slow"}));
        }).await;
        let _pipeline = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines");
                then.status(200).json_body(serde_json::json!([]));
            })
            .await;
        let client = gitlab::GitLabClient::new(server.base_url(), "test", None).expect("client");
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        let task = super::spawn_refresh(
            tx,
            3,
            7,
            client,
            super::RefreshSources {
                projects: vec![entry("fast", 1), entry("slow", 2)],
                groups: vec![],
            },
            Default::default(),
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("fast project should arrive before slow one")
            .expect("progress");
        assert!(matches!(event, crate::AppEvent::FullRefreshProgress {
            instance: 3, generation: 7, snapshot, ..
        } if snapshot.configured.id == 1 && !snapshot.has_error()));
        assert!(!task.is_finished());
        fast.assert_async().await;
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
    }

    #[tokio::test]
    async fn background_snapshot_reuses_metadata_but_explicit_refresh_fetches_it() {
        let server = httpmock::MockServer::start_async().await;
        let project = serde_json::json!({"id": 1, "name": "api", "path_with_namespace": "group/api", "default_branch": "main"});
        let metadata = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1");
                then.status(200).json_body(project.clone());
            })
            .await;
        let pipeline = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines")
                    .query_param("ref", "main");
                then.status(200)
                    .json_body(serde_json::json!([{ "id": 42, "status": "running" }]));
            })
            .await;
        let jobs = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines/42/jobs");
                then.status(200).json_body(serde_json::json!([]));
            })
            .await;
        let bridges = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/1/pipelines/42/bridges");
                then.status(200).json_body(serde_json::json!([]));
            })
            .await;
        let client = gitlab::GitLabClient::new(server.base_url(), "test", None).expect("client");
        let snapshot = super::fetch_project_snapshot(
            client.clone(),
            entry("group/api", 1),
            SnapshotOrigin::Static,
            None,
            Some(serde_json::from_value(project).expect("project")),
            None,
        )
        .await;
        assert!(!snapshot.has_error());
        metadata.assert_calls_async(0).await;
        let explicit = super::fetch_project_snapshot(
            client,
            entry("group/api", 1),
            SnapshotOrigin::Static,
            None,
            None,
            None,
        )
        .await;
        assert!(!explicit.has_error());
        metadata.assert_calls_async(1).await;
        pipeline.assert_calls_async(2).await;
        jobs.assert_calls_async(2).await;
        bridges.assert_calls_async(2).await;
    }
    #[tokio::test]
    async fn cached_details_are_only_reused_for_the_same_active_pipeline_status() {
        for (id, status, force) in [
            (42, "running", false),
            (42, "running", true),
            (43, "running", false),
            (42, "success", false),
            (42, "pending", false),
        ] {
            let server = httpmock::MockServer::start_async().await;
            let latest = server
                .mock_async(|when, then| {
                    when.path("/api/v4/projects/1/pipelines");
                    then.status(200)
                        .json_body(serde_json::json!([{ "id": id, "status": status }]));
                })
                .await;
            let jobs = server.mock_async(|when, then| {
                when.path(format!("/api/v4/projects/1/pipelines/{id}/jobs"));
                then.status(200).json_body(serde_json::json!([{ "id": 20, "name": "fresh", "status": "running", "stage": "test" }]));
            }).await;
            let bridges = server.mock_async(|when, then| {
                when.path(format!("/api/v4/projects/1/pipelines/{id}/bridges"));
                then.status(200).json_body(serde_json::json!([{ "id": 90, "name": "fresh-downstream", "status": "running", "stage": "deploy", "downstream_pipeline": { "id": 200, "project_id": 2, "status": "running" } }]));
            }).await;
            let old_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
            let old = ProjectSnapshot {
                details_updated_at: Some(old_time),
                configured: entry("group/api", 1), origin: SnapshotOrigin::Static,
                project: Some(serde_json::from_value(serde_json::json!({ "id": 1, "name": "api", "path_with_namespace": "group/api", "default_branch": "main" })).expect("project")),
                pipeline: Some(serde_json::from_value(serde_json::json!({ "id": 42, "status": "running" })).expect("pipeline")),
                jobs: vec![serde_json::from_value(serde_json::json!({ "id": 10, "name": "old", "status": "running", "stage": "test" })).expect("job")],
                bridges: vec![], status: ProjectStatus::Ready,
            };
            let client =
                gitlab::GitLabClient::new(server.base_url(), "test", None).expect("client");
            let result = super::fetch_project_snapshot(
                client,
                old.configured.clone(),
                old.origin,
                None,
                old.project.clone(),
                (!force).then_some(&old),
            )
            .await;
            assert!(!result.has_error());
            let reused = id == 42 && status == "running" && !force;
            assert_eq!(result.jobs[0].id, if reused { 10 } else { 20 });
            assert_eq!(result.bridges.len(), usize::from(!reused));
            assert_eq!(result.pipeline.as_ref().expect("pipeline").id, id);
            assert_eq!(result.details_updated_at == Some(old_time), reused);
            latest.assert_calls_async(1).await;
            jobs.assert_calls_async(usize::from(!reused)).await;
            bridges.assert_calls_async(usize::from(!reused)).await;
        }
    }
}
