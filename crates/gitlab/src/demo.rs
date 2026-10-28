//! Embedded read responses. Unknown endpoints fail closed and never fall back to HTTP.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{GitLabError, Result, DEMO_URL};

#[derive(Debug, Clone)]
pub(crate) struct DemoClient {
    projects: Arc<Vec<Value>>,
}

impl DemoClient {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            projects: Arc::new(serde_json::from_str(include_str!("demo/projects.json"))?),
        })
    }

    pub(super) fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let missing = || GitLabError::DemoMissing { path: path.into() };
        let url = reqwest::Url::parse(&format!("{DEMO_URL}{path}")).map_err(|_| missing())?;
        let segments: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
        let value = match segments.as_slice() {
            ["api", "v4", "projects"] => {
                Value::Array(self.projects.iter().map(|p| p["project"].clone()).collect())
            }
            ["api", "v4", "merge_requests"] => Value::Array(
                self.projects
                    .iter()
                    .flat_map(|p| rows(&p["merge_requests"]))
                    .cloned()
                    .collect(),
            ),
            ["api", "v4", "projects", id, endpoint @ ..] => {
                let id = id.parse::<u64>().map_err(|_| missing())?;
                let project = self
                    .projects
                    .iter()
                    .find(|p| p["project"]["id"] == id)
                    .ok_or_else(missing)?;
                project_response(project, endpoint, &url).ok_or_else(missing)?
            }
            _ => return Err(missing()),
        };
        Ok(serde_json::from_value(value)?)
    }

    pub(super) fn trace(&self, project_id: u64, job_id: u64) -> Result<String> {
        let job = self
            .projects
            .iter()
            .filter(|p| p["project"]["id"] == project_id)
            .flat_map(|p| rows(&p["pipelines"]))
            .flat_map(|p| rows(&p["jobs"]))
            .find(|job| job["id"] == job_id)
            .ok_or_else(|| GitLabError::DemoMissing {
                path: format!("job {job_id} trace"),
            })?;
        let job: crate::Job = serde_json::from_value(job.clone())?;
        Ok(match job.status {
            crate::CiStatus::Failed => include_str!("demo/failed.log"),
            crate::CiStatus::Running => include_str!("demo/running.log"),
            _ => {
                "\x1b[32mJob succeeded\x1b[0m\nArtifacts uploaded: coverage.xml, test-results.xml\n"
            }
        }
        .into())
    }
}

fn rows(value: &Value) -> impl Iterator<Item = &Value> + '_ {
    value.as_array().into_iter().flatten()
}

fn by_id(value: &Value, id: &str, key: &str) -> Option<Value> {
    let id = id.parse::<u64>().ok()?;
    rows(value).find(|row| row[key] == id).cloned()
}

fn project_response(project: &Value, endpoint: &[&str], url: &reqwest::Url) -> Option<Value> {
    match endpoint {
        [] => Some(project["project"].clone()),
        ["repository", "branches"] => Some(project["branches"].clone()),
        ["repository", "tags"] => Some(project["tags"].clone()),
        ["variables"] => Some(project["variables"].clone()),
        ["environments"] => Some(project["environments"].clone()),
        ["environments", id] => by_id(&project["environments"], id, "id"),
        ["merge_requests"] => Some(project["merge_requests"].clone()),
        ["merge_requests", iid] => by_id(&project["merge_requests"], iid, "iid"),
        ["pipelines"] => Some(pipelines(project, url)),
        ["pipelines", id] => by_id(&project["pipelines"], id, "id"),
        ["pipelines", id, "jobs"] => {
            by_id(&project["pipelines"], id, "id").map(|p| p["jobs"].clone())
        }
        ["pipelines", _, "bridges"] => Some(json!([])),
        _ => None,
    }
}

fn pipelines(project: &Value, url: &reqwest::Url) -> Value {
    let query = |key| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    let ref_ = query("ref");
    let status = query("status");
    let limit = query("per_page").and_then(|n| n.parse().ok()).unwrap_or(20);
    Value::Array(
        rows(&project["pipelines"])
            .filter(|p| ref_.as_ref().is_none_or(|r| p["ref"] == *r))
            .filter(|p| status.as_ref().is_none_or(|s| p["status"] == *s))
            .take(limit)
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use crate::{CiStatus, GitLabClient, GitLabError};

    #[tokio::test]
    async fn fixtures_deserialize_through_every_dashboard_read() {
        let client = GitLabClient::demo().expect("embedded fixtures");
        assert!(client.is_demo());
        let projects = client.list_projects().await.expect("projects");
        assert_eq!(projects.len(), 5);
        for project in projects {
            let id = project.id;
            assert_eq!(client.get_project(id).await.expect("project").id, id);
            assert_eq!(client.list_branches(id).await.expect("branches").len(), 3);
            assert_eq!(client.list_tags(id).await.expect("tags").len(), 2);
            assert_eq!(
                client
                    .list_environments(id)
                    .await
                    .expect("environments")
                    .len(),
                2
            );
            assert_eq!(
                client
                    .list_project_variables(id)
                    .await
                    .expect("variables")
                    .len(),
                3
            );
            let latest = client
                .get_latest_pipeline(id, Some("main"))
                .await
                .expect("latest")
                .expect("main");
            assert_eq!(latest.ref_.as_deref(), Some("main"));
            let running = client.list_running_pipelines(id, 1).await.expect("running");
            assert_eq!(running.len(), 1);
            assert_eq!(running[0].status, CiStatus::Running);
            let pipelines = client.list_pipelines(id, None, 20).await.expect("history");
            for pipeline in pipelines {
                assert_eq!(
                    client
                        .get_pipeline(id, pipeline.id)
                        .await
                        .expect("pipeline")
                        .id,
                    pipeline.id
                );
                assert!(client
                    .get_pipeline_bridges(id, pipeline.id)
                    .await
                    .expect("bridges")
                    .is_empty());
                let jobs = client
                    .get_pipeline_jobs(id, pipeline.id)
                    .await
                    .expect("jobs");
                assert_eq!(jobs.len(), 5);
                for job in jobs {
                    let trace = client.get_job_trace(id, job.id).await.expect("trace");
                    if job.status == CiStatus::Failed {
                        assert!(trace.contains("section_start:"));
                        assert!(trace.contains("\x1b[31m"));
                        assert!(trace.contains("idempotency key"));
                    }
                }
            }
            for mr in client.list_project_merge_requests(id).await.expect("MRs") {
                assert!(client
                    .get_merge_request(id, mr.iid)
                    .await
                    .expect("head")
                    .head_pipeline
                    .is_some());
            }
        }
        for scope in ["created_by_me", "assigned_to_me"] {
            assert_eq!(
                client
                    .list_my_merge_requests(scope)
                    .await
                    .expect("my MRs")
                    .len(),
                15
            );
        }
        assert!(client.get_project(999).await.is_err());
        assert!(client.get_current_user().await.is_err());
    }

    #[tokio::test]
    async fn demo_client_rejects_writes_and_preserves_artifact_destination() {
        let client = GitLabClient::demo().expect("demo");
        assert!(matches!(
            client.retry_job(1, 1052).await,
            Err(GitLabError::DemoAction)
        ));
        assert!(matches!(
            client.merge_merge_request(1, 1, "demo").await,
            Err(GitLabError::DemoAction)
        ));
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("artifacts.zip");
        std::fs::write(&path, "existing archive").expect("existing file");
        assert!(matches!(
            client.download_job_artifacts(1, 1050, &path).await,
            Err(GitLabError::DemoAction)
        ));
        assert_eq!(
            std::fs::read_to_string(path).expect("preserved file"),
            "existing archive"
        );
    }
}
