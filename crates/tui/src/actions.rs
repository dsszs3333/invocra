use std::time::Duration;

use gitlab::GitLabClient;
use tokio::sync::mpsc;

use crate::refresh::describe_gitlab_error;
use crate::AppEvent;

/// A mutating GitLab operation requested from the UI. Always confirmed by the user first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    RetryPipeline {
        project_id: u64,
        pipeline_id: u64,
    },
    CancelPipeline {
        project_id: u64,
        pipeline_id: u64,
    },
    RunPipeline {
        project_id: u64,
        ref_: String,
    },
    PlayJob {
        project_id: u64,
        job_id: u64,
        job_name: String,
    },
    RetryJob {
        project_id: u64,
        job_id: u64,
        job_name: String,
    },
    /// May fail with 403/404 on tiers without the approvals feature; surfaced as a status message.
    ApproveMr {
        project_id: u64,
        iid: u64,
    },
    MergeMr {
        project_id: u64,
        iid: u64,
        sha: String,
    },
}

impl Action {
    pub(crate) const fn project_id(&self) -> u64 {
        match self {
            Self::RetryPipeline { project_id, .. }
            | Self::CancelPipeline { project_id, .. }
            | Self::RunPipeline { project_id, .. }
            | Self::PlayJob { project_id, .. }
            | Self::RetryJob { project_id, .. }
            | Self::ApproveMr { project_id, .. }
            | Self::MergeMr { project_id, .. } => *project_id,
        }
    }

    /// True for actions that target a merge request; their completion refreshes the MR panel
    /// instead of the project snapshot.
    pub(crate) const fn is_mr_action(&self) -> bool {
        matches!(self, Self::ApproveMr { .. } | Self::MergeMr { .. })
    }

    /// The pipeline/job id this action targets, for marking the exact row as in-flight (spinner).
    /// `RunPipeline` has no pre-existing item, so it returns 0 and the spinner shows on the project.
    pub(crate) const fn item_id(&self) -> u64 {
        match self {
            Self::RetryPipeline { pipeline_id, .. } | Self::CancelPipeline { pipeline_id, .. } => {
                *pipeline_id
            }
            Self::PlayJob { job_id, .. } | Self::RetryJob { job_id, .. } => *job_id,
            Self::ApproveMr { iid, .. } | Self::MergeMr { iid, .. } => *iid,
            Self::RunPipeline { .. } => 0,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::RetryPipeline { pipeline_id, .. } => format!("Retry pipeline #{pipeline_id}"),
            Self::CancelPipeline { pipeline_id, .. } => format!("Cancel pipeline #{pipeline_id}"),
            Self::RunPipeline { ref_, .. } => format!("Run pipeline on '{ref_}'"),
            Self::PlayJob { job_name, .. } => format!("Play job '{job_name}'"),
            Self::RetryJob { job_name, .. } => format!("Retry job '{job_name}'"),
            Self::ApproveMr { iid, .. } => format!("Approve MR !{iid}"),
            Self::MergeMr { iid, .. } => format!("Merge MR !{iid}"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ActionReport {
    /// Which dashboard instance (tab) this report belongs to; the run loop drops reports from a
    /// tab the user has since switched away from.
    pub(crate) instance: usize,
    pub(crate) action: Action,
    pub(crate) result: Result<(), String>,
}

pub(crate) fn spawn_action(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    action: Action,
) {
    tokio::spawn(async move {
        let result = match &action {
            Action::RetryPipeline {
                project_id,
                pipeline_id,
            } => client
                .retry_pipeline(*project_id, *pipeline_id)
                .await
                .map(|_| ()),
            Action::CancelPipeline {
                project_id,
                pipeline_id,
            } => client
                .cancel_pipeline(*project_id, *pipeline_id)
                .await
                .map(|_| ()),
            Action::RunPipeline { project_id, ref_ } => {
                client.create_pipeline(*project_id, ref_).await.map(|_| ())
            }
            Action::PlayJob {
                project_id, job_id, ..
            } => client.play_job(*project_id, *job_id).await.map(|_| ()),
            Action::RetryJob {
                project_id, job_id, ..
            } => client.retry_job(*project_id, *job_id).await.map(|_| ()),
            Action::ApproveMr { project_id, iid } => {
                client.approve_merge_request(*project_id, *iid).await
            }
            Action::MergeMr {
                project_id,
                iid,
                sha,
            } => client.merge_merge_request(*project_id, *iid, sha).await,
        };

        let _ = tx
            .send(AppEvent::ActionCompleted(ActionReport {
                instance,
                action,
                result: result.map_err(|err| describe_gitlab_error(&err)),
            }))
            .await;
    });
}

/// Fetches a job trace and reports back tagged with `generation`; the run loop drops reports
/// whose generation no longer matches the open log viewer (close/reopen races).
pub(crate) fn spawn_trace_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
    job_id: u64,
    generation: u64,
    delay: Option<Duration>,
) {
    tokio::spawn(async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let result = client
            .get_job_trace(project_id, job_id)
            .await
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::TraceLoaded {
                instance,
                project_id,
                job_id,
                generation,
                result,
            })
            .await;
    });
}

/// Why a variables fetch failed, pre-classified so the UI can phrase 403 helpfully.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VariablesError {
    Forbidden,
    Other(String),
}

pub(crate) fn spawn_variables_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
) {
    tokio::spawn(async move {
        let result = client
            .list_project_variables(project_id)
            .await
            .map_err(|err| {
                if err.is_forbidden() {
                    VariablesError::Forbidden
                } else {
                    VariablesError::Other(err.to_string())
                }
            });
        let _ = tx
            .send(AppEvent::VariablesLoaded {
                instance,
                project_id,
                result,
            })
            .await;
    });
}

/// Number of pipelines listed in the history overlay.
const PIPELINE_HISTORY_PER_PAGE: u32 = 20;

pub(crate) fn spawn_pipelines_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
    ref_: Option<String>,
) {
    tokio::spawn(async move {
        let result = client
            .list_pipelines(project_id, ref_.as_deref(), PIPELINE_HISTORY_PER_PAGE)
            .await
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::PipelinesLoaded {
                instance,
                project_id,
                result,
            })
            .await;
    });
}

/// Fetches the MR panel's list: the project's open MRs, or the instance-wide "my MRs" when
/// `api_scope` is set (the GitLab `scope=` parameter).
pub(crate) fn spawn_merge_requests_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: Option<u64>,
    scope: crate::state::MrScope,
    generation: u64,
) {
    tokio::spawn(async move {
        let result = match (scope.api_scope(), project_id) {
            (Some(scope), _) => client.list_my_merge_requests(scope).await,
            (None, Some(project_id)) => client.list_project_merge_requests(project_id).await,
            (None, None) => return,
        };
        let _ = tx
            .send(AppEvent::MergeRequestsLoaded {
                instance,
                scope,
                generation,
                project_id,
                result: result.map_err(|err| describe_gitlab_error(&err)),
            })
            .await;
    });
}

/// Fetches one MR's head pipeline (the list endpoint omits it).
pub(crate) fn spawn_mr_head_pipeline_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
    iid: u64,
    generation: u64,
) {
    tokio::spawn(async move {
        let result = client
            .get_merge_request(project_id, iid)
            .await
            .map(|details| details.head_pipeline)
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::MrHeadPipelineLoaded {
                instance,
                generation,
                project_id,
                iid,
                result,
            })
            .await;
    });
}

pub(crate) fn spawn_environments_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
) {
    tokio::spawn(async move {
        let result = client
            .list_environments(project_id)
            .await
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::EnvironmentsLoaded {
                instance,
                project_id,
                result,
            })
            .await;
    });
}

/// Scans `project_ids` for currently-running pipelines and reports which branch each is building.
/// Concurrency-limited like the dashboard refresh, since a group can resolve to 100+ projects.
/// Projects with nothing running — or that error — are simply absent from the result.
pub(crate) fn spawn_running_scan(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_ids: Vec<u64>,
) {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(8));
        let mut tasks = JoinSet::new();
        for project_id in project_ids {
            let client = client.clone();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                // ponytail: one running pipeline per project is enough to jump to; ignore the rest.
                let pipeline = client
                    .list_running_pipelines(project_id, 1)
                    .await
                    .ok()?
                    .into_iter()
                    .next()?;
                Some(crate::RunningHit {
                    project_id,
                    ref_: pipeline.ref_?,
                    pipeline_id: pipeline.id,
                })
            });
        }
        let mut hits = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            if let Ok(Some(hit)) = joined {
                hits.push(hit);
            }
        }
        let _ = tx
            .send(AppEvent::RunningScanCompleted { instance, hits })
            .await;
    });
}

/// Fetches a pipeline's jobs and bridge jobs. `project_id` keys the dashboard entry the result
/// lands in; `pipeline_project_id` is the project the pipeline actually belongs to — they differ
/// when inspecting a downstream pipeline of a multi-project bridge.
pub(crate) fn spawn_pipeline_jobs_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    generation: u64,
    client: GitLabClient,
    project_id: u64,
    pipeline_project_id: u64,
    pipeline_id: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _permit = crate::refresh::REFRESH_LIMIT.acquire().await;
        let (pipeline, jobs, bridges) = tokio::join!(
            client.get_pipeline(pipeline_project_id, pipeline_id),
            client.get_pipeline_jobs(pipeline_project_id, pipeline_id),
            client.get_pipeline_bridges(pipeline_project_id, pipeline_id),
        );
        let result = match (pipeline, jobs, bridges) {
            (Ok(pipeline), Ok(jobs), Ok(bridges)) => Ok((pipeline, jobs, bridges)),
            (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => {
                Err(describe_gitlab_error(&err))
            }
        };
        let _ = tx
            .send(AppEvent::PipelineJobsLoaded {
                instance,
                generation,
                project_id,
                pipeline_id,
                result,
            })
            .await;
    })
}

/// Downloads a job's artifacts archive to `~/Downloads` (falling back to the current directory)
/// as `<job-name>-<job_id>-<filename>`, reporting the written path and size.
pub(crate) fn spawn_artifacts_download(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
    job_id: u64,
    job_name: String,
    filename: String,
) {
    tokio::spawn(async move {
        let dest = artifacts_dest(&job_name, job_id, &filename);
        let result = client
            .download_job_artifacts(project_id, job_id, &dest)
            .await
            .map(|written| (dest, written))
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::ArtifactsDownloaded {
                instance,
                project_id,
                job_id,
                result,
            })
            .await;
    });
}

/// Destination path for an artifacts download. The job name is sanitized like cache filenames so
/// it cannot smuggle path separators; the archive filename comes from GitLab and is sanitized too.
fn artifacts_dest(job_name: &str, job_id: u64, filename: &str) -> std::path::PathBuf {
    let sanitize = |raw: &str| -> String {
        raw.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '.' {
                    ch
                } else {
                    '-'
                }
            })
            .collect()
    };
    let dir = std::env::home_dir()
        .map(|home| home.join("Downloads"))
        .filter(|downloads| downloads.is_dir())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join(format!(
        "{}-{job_id}-{}",
        sanitize(job_name),
        sanitize(filename)
    ))
}

pub(crate) fn spawn_branches_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
) {
    tokio::spawn(async move {
        let result = client
            .list_branches(project_id)
            .await
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::BranchesLoaded {
                instance,
                project_id,
                result,
            })
            .await;
    });
}

pub(crate) fn spawn_tags_fetch(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
) {
    tokio::spawn(async move {
        let result = client
            .list_tags(project_id)
            .await
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::TagsLoaded {
                instance,
                project_id,
                result,
            })
            .await;
    });
}

pub(crate) fn spawn_tag_create(
    tx: mpsc::Sender<AppEvent>,
    instance: usize,
    client: GitLabClient,
    project_id: u64,
    tag_name: String,
    ref_: String,
    message: String,
) {
    tokio::spawn(async move {
        let result = client
            .create_tag(project_id, &tag_name, &ref_, &message)
            .await
            .map(|_| ())
            .map_err(|err| describe_gitlab_error(&err));
        let _ = tx
            .send(AppEvent::TagCreated {
                instance,
                project_id,
                tag_name,
                result,
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::Action;

    #[test]
    fn action_exposes_project_id_and_description() {
        let action = Action::PlayJob {
            project_id: 7,
            job_id: 10,
            job_name: String::from("deploy"),
        };
        assert_eq!(action.project_id(), 7);
        assert_eq!(action.describe(), "Play job 'deploy'");

        let action = Action::RetryPipeline {
            project_id: 3,
            pipeline_id: 42,
        };
        assert_eq!(action.project_id(), 3);
        assert_eq!(action.describe(), "Retry pipeline #42");
    }
}
