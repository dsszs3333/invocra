//! GitLab client and response DTOs used by the dashboard.

use std::num::ParseIntError;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::instrument;

mod demo;
mod http;

/// Reserved, fictional server used by the built-in demo.
pub const DEMO_URL: &str = "https://demo.gitlab.example";

pub type Result<T> = std::result::Result<T, GitLabError>;

#[derive(Debug, Error)]
pub enum GitLabError {
    #[error("invalid built-in demo data")]
    DemoData {
        #[from]
        source: serde_json::Error,
    },
    #[error("Demo mode: actions are simulated")]
    DemoAction,
    #[error("Demo mode: no fixture for {path}")]
    DemoMissing { path: String },
    #[error("failed to encode GitLab token header")]
    InvalidTokenHeader {
        #[source]
        source: reqwest::header::InvalidHeaderValue,
    },
    #[error("failed to construct GitLab HTTP client")]
    BuildClient {
        #[source]
        source: reqwest::Error,
    },
    #[error("request failed: {url}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("GitLab returned error for {url}: {source}")]
    Status {
        url: String,
        /// HTTP status code GitLab responded with, as a raw `u16` so the public API does not leak
        /// the `reqwest` types. `None` when the failure carried no status.
        status: Option<u16>,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to decode GitLab response for {url}")]
    Decode {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("invalid x-next-page value: {value}")]
    InvalidNextPage {
        value: String,
        #[source]
        source: ParseIntError,
    },
    #[error("invalid GitLab pagination: {reason}")]
    Pagination { reason: &'static str },
    #[error("failed to write artifacts to {path}")]
    ArtifactsWrite {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsafe, invalid or excessive artifacts redirect for {url}")]
    ArtifactsRedirect { url: String },
}

impl GitLabError {
    /// Returns the HTTP status code if GitLab responded with an error status.
    fn http_status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => *status,
            _ => None,
        }
    }

    /// True when GitLab rejected the request with 403 (e.g. variables below Maintainer role).
    #[must_use]
    pub fn is_forbidden(&self) -> bool {
        self.http_status() == Some(403)
    }

    /// True when GitLab rejected the request with 401 (e.g. an invalid token during onboarding).
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        self.http_status() == Some(401)
    }
}

/// GitLab API client backed by HTTP or built-in demo data.
#[derive(Debug, Clone)]
pub struct GitLabClient {
    backend: Backend,
}

#[derive(Debug, Clone)]
enum Backend {
    Http(http::HttpClient),
    Demo(demo::DemoClient),
}

impl GitLabClient {
    /// Builds a completely offline client without credentials or an HTTP transport.
    ///
    /// # Errors
    /// Returns an error if the embedded fixtures cannot be decoded.
    pub fn demo() -> Result<Self> {
        Ok(Self {
            backend: Backend::Demo(demo::DemoClient::new()?),
        })
    }

    /// Whether this client serves the built-in demo data.
    #[must_use]
    pub fn is_demo(&self) -> bool {
        matches!(self.backend, Backend::Demo(_))
    }

    /// Builds a GitLab API client for the provided base URL and token.
    ///
    /// `proxy` routes this instance's traffic through an HTTP/HTTPS/SOCKS5 proxy (e.g.
    /// `socks5://127.0.0.1:1080`), so a VPN-only instance can use a tunnel without forcing every
    /// other instance through it. `None` connects directly (respecting system proxy env vars).
    ///
    /// # Errors
    ///
    /// Returns an error if the token cannot be encoded as an HTTP header, the proxy URL is invalid,
    /// or the underlying HTTP client cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            backend: Backend::Http(http::HttpClient::new(base_url, token, proxy)?),
        })
    }

    #[instrument(skip(self))]
    /// Fetches project metadata by numeric GitLab project id.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_project(&self, project_id: u64) -> Result<Project> {
        self.get_json(&format!("/api/v4/projects/{project_id}"))
            .await
    }

    #[instrument(skip(self))]
    /// Fetches the latest pipeline for a project, optionally scoped to a branch/ref.
    ///
    /// When `ref_` is `Some`, only pipelines for that ref are considered; when `None`, the latest
    /// pipeline across all refs is returned (used as a fallback for repos without a default branch).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_latest_pipeline(
        &self,
        project_id: u64,
        ref_: Option<&str>,
    ) -> Result<Option<Pipeline>> {
        let ref_query = ref_.map_or_else(String::new, |ref_| {
            format!("&ref={}", urlencoding::encode(ref_))
        });
        let pipelines: Vec<Pipeline> = self
            .get_json(&format!(
                "/api/v4/projects/{project_id}/pipelines?per_page=1&order_by=id&sort=desc{ref_query}"
            ))
            .await?;
        Ok(pipelines.into_iter().next())
    }

    #[instrument(skip(self))]
    /// Fetches one pipeline, including its current status and ref.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or its response cannot be decoded.
    pub async fn get_pipeline(&self, project_id: u64, pipeline_id: u64) -> Result<Pipeline> {
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/pipelines/{pipeline_id}"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists recent pipelines for a project, newest first, optionally scoped to a branch/ref.
    ///
    /// When `ref_` is `Some`, only pipelines for that ref are returned; when `None`, the latest
    /// pipelines across all refs are returned. Used by the pipeline-history picker.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn list_pipelines(
        &self,
        project_id: u64,
        ref_: Option<&str>,
        per_page: u32,
    ) -> Result<Vec<Pipeline>> {
        let ref_query = ref_.map_or_else(String::new, |ref_| {
            format!("&ref={}", urlencoding::encode(ref_))
        });
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/pipelines?per_page={per_page}&order_by=id&sort=desc{ref_query}"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists the branches of a project.
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_branches(&self, project_id: u64) -> Result<Vec<Branch>> {
        self.get_paginated_json(&format!(
            "/api/v4/projects/{project_id}/repository/branches?per_page=100"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists the tags of a project, most recent first.
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_tags(&self, project_id: u64) -> Result<Vec<Tag>> {
        self.get_paginated_json(&format!(
            "/api/v4/projects/{project_id}/repository/tags?per_page=100"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Creates a tag pointing at `ref_`. A non-empty `message` makes it an annotated tag.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn create_tag(
        &self,
        project_id: u64,
        tag_name: &str,
        ref_: &str,
        message: &str,
    ) -> Result<Tag> {
        let mut path = format!(
            "/api/v4/projects/{project_id}/repository/tags?tag_name={}&ref={}",
            urlencoding::encode(tag_name),
            urlencoding::encode(ref_)
        );
        if !message.is_empty() {
            path.push_str(&format!("&message={}", urlencoding::encode(message)));
        }
        self.post_json(&path).await
    }

    #[instrument(skip(self))]
    /// Fetches jobs for a specific pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_pipeline_jobs(&self, project_id: u64, pipeline_id: u64) -> Result<Vec<Job>> {
        self.get_paginated_json(&format!(
            "/api/v4/projects/{project_id}/pipelines/{pipeline_id}/jobs?per_page=100"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Fetches bridge (trigger) jobs for a pipeline, each carrying its downstream pipeline if one
    /// has been created.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_pipeline_bridges(
        &self,
        project_id: u64,
        pipeline_id: u64,
    ) -> Result<Vec<Bridge>> {
        self.get_paginated_json(&format!(
            "/api/v4/projects/{project_id}/pipelines/{pipeline_id}/bridges?per_page=100"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists a project's open merge requests, most recently updated first. One page of 50 —
    /// enough for a dashboard panel; deliberately not paginated further.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn list_project_merge_requests(&self, project_id: u64) -> Result<Vec<MergeRequest>> {
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=50"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists the current user's open merge requests across the instance. `scope` is the GitLab
    /// scope parameter: `created_by_me` or `assigned_to_me`. One page of 50, like the project list.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn list_my_merge_requests(&self, scope: &str) -> Result<Vec<MergeRequest>> {
        self.get_json(&format!(
            "/api/v4/merge_requests?state=opened&scope={}&order_by=updated_at&sort=desc&per_page=50",
            urlencoding::encode(scope)
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Fetches a single merge request, for the fields the list omits (notably `head_pipeline`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_merge_request(
        &self,
        project_id: u64,
        iid: u64,
    ) -> Result<MergeRequestDetails> {
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/merge_requests/{iid}"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Approves a merge request. May fail with 403/404 on tiers without the approvals feature.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn approve_merge_request(&self, project_id: u64, iid: u64) -> Result<()> {
        self.post_json::<serde::de::IgnoredAny>(&format!(
            "/api/v4/projects/{project_id}/merge_requests/{iid}/approve"
        ))
        .await
        .map(|_| ())
    }

    #[instrument(skip(self))]
    /// Merges a merge request.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status (405 when the
    /// MR is not mergeable), or the JSON body cannot be decoded.
    pub async fn merge_merge_request(&self, project_id: u64, iid: u64, sha: &str) -> Result<()> {
        self.put_json::<serde::de::IgnoredAny>(&format!(
            "/api/v4/projects/{project_id}/merge_requests/{iid}/merge?sha={}",
            urlencoding::encode(sha)
        ))
        .await
        .map(|_| ())
    }

    #[instrument(skip(self))]
    /// Lists a project's available environments, each with its last deployment.
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_environments(&self, project_id: u64) -> Result<Vec<Environment>> {
        let environments: Vec<Environment> = self
            .get_paginated_json(&format!(
                "/api/v4/projects/{project_id}/environments?states=available&per_page=100"
            ))
            .await?;
        // The LIST endpoint omits `last_deployment` — only the single-environment endpoint
        // returns it — so enrich each row with a detail fetch.
        // ponytail: sequential; projects rarely have more than a handful of environments. Switch
        // to a bounded JoinSet if someone shows up with dozens.
        let mut enriched = Vec::with_capacity(environments.len());
        for environment in environments {
            enriched.push(self.get_environment(project_id, environment.id).await?);
        }
        Ok(enriched)
    }

    #[instrument(skip(self))]
    /// Fetches one environment, including its `last_deployment` (the list endpoint omits it).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn get_environment(
        &self,
        project_id: u64,
        environment_id: u64,
    ) -> Result<Environment> {
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/environments/{environment_id}"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Lists all accessible projects for the authenticated user.
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_projects(&self) -> Result<Vec<AccessibleProject>> {
        self.get_paginated_json(
            "/api/v4/projects?membership=true&simple=true&order_by=path&sort=asc&per_page=100",
        )
        .await
    }

    #[instrument(skip(self))]
    /// Lists the currently-running pipelines of a project, newest first (at most `per_page`).
    ///
    /// Used to discover which branch a project is actively building. Pending/queued pipelines are
    /// excluded; only `status=running`. Empty when nothing is running.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn list_running_pipelines(
        &self,
        project_id: u64,
        per_page: u32,
    ) -> Result<Vec<Pipeline>> {
        self.get_json(&format!(
            "/api/v4/projects/{project_id}/pipelines?status=running&per_page={per_page}&order_by=id&sort=desc"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Fetches the authenticated user; used by onboarding to validate a freshly entered token.
    ///
    /// # Errors
    ///
    /// Returns an error (401 → [`GitLabError::is_unauthorized`]) if the token is invalid, or if
    /// the request fails or the body cannot be decoded.
    pub async fn get_current_user(&self) -> Result<CurrentUser> {
        self.get_json("/api/v4/user").await
    }

    #[instrument(skip(self))]
    /// Lists groups the authenticated user is a member of (Guest level and above).
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_groups(&self) -> Result<Vec<AccessibleGroup>> {
        self.get_paginated_json(
            "/api/v4/groups?min_access_level=10&all_available=false&order_by=path&sort=asc&per_page=100",
        )
        .await
    }

    #[instrument(skip(self))]
    /// Lists projects belonging to a group referenced by numeric id or full path.
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_group_projects(
        &self,
        group: &str,
        include_subgroups: bool,
    ) -> Result<Vec<AccessibleProject>> {
        let encoded = urlencoding::encode(group);
        self.get_paginated_json(&format!(
            "/api/v4/groups/{encoded}/projects?include_subgroups={include_subgroups}&archived=false&simple=true&order_by=path&sort=asc&per_page=100"
        ))
        .await
    }

    async fn get_paginated_json<T>(&self, path_with_query: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        match &self.backend {
            Backend::Http(client) => client.get_paginated_json(path_with_query).await,
            Backend::Demo(client) => client.get(path_with_query),
        }
    }

    #[instrument(skip(self, dest))]
    /// Downloads a job's artifacts archive to `dest`, streaming chunks to disk so a large archive
    /// never sits in memory. Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status (404 when the
    /// artifacts have expired), or the file cannot be written.
    pub async fn download_job_artifacts(
        &self,
        project_id: u64,
        job_id: u64,
        dest: &std::path::Path,
    ) -> Result<u64> {
        match &self.backend {
            Backend::Http(client) => {
                client
                    .download_job_artifacts(project_id, job_id, dest)
                    .await
            }
            Backend::Demo(_) => Err(GitLabError::DemoAction),
        }
    }

    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        match &self.backend {
            Backend::Http(client) => client.get_json(path).await,
            Backend::Demo(client) => client.get(path),
        }
    }

    async fn post_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        match &self.backend {
            Backend::Http(client) => client.post_json(path).await,
            Backend::Demo(_) => Err(GitLabError::DemoAction),
        }
    }

    /// Like [`Self::post_json`] but for PUT endpoints (e.g. merging an MR).
    async fn put_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        match &self.backend {
            Backend::Http(client) => client.put_json(path).await,
            Backend::Demo(_) => Err(GitLabError::DemoAction),
        }
    }

    #[instrument(skip(self))]
    /// Retries all failed or canceled jobs of a pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn retry_pipeline(&self, project_id: u64, pipeline_id: u64) -> Result<Pipeline> {
        self.post_json(&format!(
            "/api/v4/projects/{project_id}/pipelines/{pipeline_id}/retry"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Creates (triggers) a new pipeline on the given ref.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn create_pipeline(&self, project_id: u64, ref_: &str) -> Result<Pipeline> {
        self.post_json(&format!(
            "/api/v4/projects/{project_id}/pipeline?ref={}",
            urlencoding::encode(ref_)
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Cancels a running pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn cancel_pipeline(&self, project_id: u64, pipeline_id: u64) -> Result<Pipeline> {
        self.post_json(&format!(
            "/api/v4/projects/{project_id}/pipelines/{pipeline_id}/cancel"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Starts a manual job.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn play_job(&self, project_id: u64, job_id: u64) -> Result<Job> {
        self.post_json(&format!("/api/v4/projects/{project_id}/jobs/{job_id}/play"))
            .await
    }

    #[instrument(skip(self))]
    /// Retries a job. GitLab creates a new job with a new id; callers must re-fetch state.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the JSON
    /// body cannot be decoded.
    pub async fn retry_job(&self, project_id: u64, job_id: u64) -> Result<Job> {
        self.post_json(&format!(
            "/api/v4/projects/{project_id}/jobs/{job_id}/retry"
        ))
        .await
    }

    #[instrument(skip(self))]
    /// Fetches a job log. Logs can reach ~100 MB on gitlab.com, so only the last
    /// [`MAX_TRACE_BYTES`] bytes are kept (cut on a line boundary, with a truncation marker).
    /// Rate-limited reads (HTTP 429) are retried once using the same policy as JSON reads.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, GitLab returns a non-success status, or the body
    /// cannot be read.
    pub async fn get_job_trace(&self, project_id: u64, job_id: u64) -> Result<String> {
        match &self.backend {
            Backend::Http(client) => client.get_job_trace(project_id, job_id).await,
            Backend::Demo(client) => client.trace(project_id, job_id),
        }
    }

    #[instrument(skip(self))]
    /// Lists CI/CD variables of a project. Requires at least the Maintainer role; lower roles
    /// receive 403 (see [`GitLabError::is_forbidden`]).
    ///
    /// # Errors
    ///
    /// Returns an error if pagination headers are invalid, requests fail, GitLab returns a
    /// non-success status, or the JSON body cannot be decoded.
    pub async fn list_project_variables(&self, project_id: u64) -> Result<Vec<ProjectVariable>> {
        self.get_paginated_json(&format!(
            "/api/v4/projects/{project_id}/variables?per_page=100"
        ))
        .await
    }
}

/// Maximum job-trace tail kept in memory.
pub const MAX_TRACE_BYTES: usize = 2 * 1024 * 1024;

/// Keeps the trailing `cap` bytes of a trace, cut on a line boundary, prepending a marker.
fn truncate_trace_tail(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }

    let total = text.len();
    let tail_start = total - cap;
    // Move forward to the next line boundary so the first kept line is whole; fall back to the
    // nearest char boundary when the tail has no newline at all.
    let mut start = text.as_bytes()[tail_start..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(tail_start, |offset| tail_start + offset + 1);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "--- truncated ({total} bytes total) ---\n{}",
        &text[start..]
    )
}

fn parse_next_page_header(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn next_page_from_headers(headers: &reqwest::header::HeaderMap) -> Result<Option<String>> {
    let Some(value) = headers.get("x-next-page") else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| GitLabError::Pagination {
        reason: "non-ASCII x-next-page",
    })?;
    Ok(parse_next_page_header(value).map(ToOwned::to_owned))
}

/// Parses a `Retry-After` header expressed as an integer number of seconds (the form GitLab uses).
/// HTTP-date forms are ignored, falling back to the caller's default.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// CI status shared by pipelines and jobs.
///
/// Modelled as a typed enum (instead of a raw `String`) so call sites get exhaustive matching and
/// can't drift on spelling — e.g. GitLab uses `canceled`, not `cancelled`. Any status we don't
/// model explicitly is preserved verbatim in [`CiStatus::Other`]. It (de)serializes to/from the
/// raw GitLab string, so the on-disk cache format stays a plain string and remains compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum CiStatus {
    Created,
    WaitingForResource,
    Preparing,
    Pending,
    Running,
    Success,
    Failed,
    Canceling,
    Canceled,
    Skipped,
    Manual,
    Scheduled,
    /// A status GitLab reported that we don't model explicitly; kept verbatim.
    Other(String),
}

impl CiStatus {
    /// The raw GitLab wire string for this status.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Created => "created",
            Self::WaitingForResource => "waiting_for_resource",
            Self::Preparing => "preparing",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Canceling => "canceling",
            Self::Canceled => "canceled",
            Self::Skipped => "skipped",
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::Other(raw) => raw,
        }
    }

    /// True for statuses that can be retried (a failed or canceled pipeline/job).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Failed | Self::Canceled)
    }

    /// True for pipeline statuses that are still in flight and can be canceled.
    #[must_use]
    pub fn is_cancelable(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Pending | Self::Created | Self::WaitingForResource
        )
    }

    /// True for in-flight statuses that should keep auto-refreshing. Superset of
    /// [`is_cancelable`](Self::is_cancelable) (adds `Preparing`, `Scheduled`, `Canceling`, which are still
    /// progressing). `Manual` is intentionally not active — it is a stable wait-for-user state.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::Pending
                | Self::Created
                | Self::WaitingForResource
                | Self::Preparing
                | Self::Scheduled
                | Self::Canceling
        )
    }
}

impl From<&str> for CiStatus {
    fn from(value: &str) -> Self {
        match value {
            "created" => Self::Created,
            "waiting_for_resource" => Self::WaitingForResource,
            "preparing" => Self::Preparing,
            "pending" => Self::Pending,
            "running" => Self::Running,
            "success" => Self::Success,
            "failed" => Self::Failed,
            "canceling" => Self::Canceling,
            "canceled" => Self::Canceled,
            "skipped" => Self::Skipped,
            "manual" => Self::Manual,
            "scheduled" => Self::Scheduled,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl From<String> for CiStatus {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<CiStatus> for String {
    fn from(value: CiStatus) -> Self {
        match value {
            CiStatus::Other(raw) => raw,
            other => other.as_str().to_owned(),
        }
    }
}

impl std::fmt::Display for CiStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// GitLab project metadata used by the dashboard.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Project {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub path_with_namespace: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

/// Project summary returned by discovery endpoints.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AccessibleProject {
    pub id: u64,
    pub name: String,
    pub path_with_namespace: String,
}

/// Group summary returned by `GET /groups`, used by the onboarding picker.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AccessibleGroup {
    pub id: u64,
    pub name: String,
    pub full_path: String,
}

/// The authenticated user; used to validate a token during onboarding.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CurrentUser {
    pub username: String,
}

/// Pipeline summary returned by GitLab.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Pipeline {
    pub id: u64,
    #[serde(default)]
    pub iid: Option<u64>,
    pub status: CiStatus,
    #[serde(rename = "ref")]
    #[serde(default)]
    pub ref_: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    /// When the pipeline was created/triggered (ISO 8601), i.e. its start time.
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

/// A project branch returned by `GET /projects/:id/repository/branches`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    #[serde(default)]
    pub default: bool,
}

/// A project tag returned by `GET /projects/:id/repository/tags`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

/// Pipeline job summary returned by GitLab.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: CiStatus,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub web_url: Option<String>,
    /// Metadata of the job's artifacts archive; `None` when the job produced none (or they
    /// expired). Presence is what enables the "Download artifacts" action.
    #[serde(default)]
    pub artifacts_file: Option<ArtifactsFile>,
}

/// The artifacts archive attached to a job: its filename (usually `artifacts.zip`) and byte size.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactsFile {
    pub filename: String,
    #[serde(default)]
    pub size: u64,
}

/// Bridge (trigger) job returned by GitLab: a job whose "work" is running another pipeline.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Bridge {
    pub id: u64,
    pub name: String,
    pub status: CiStatus,
    #[serde(default)]
    pub stage: Option<String>,
    /// The child/downstream pipeline; `None` until GitLab has created it.
    #[serde(default)]
    pub downstream_pipeline: Option<DownstreamPipeline>,
}

/// Downstream pipeline reference embedded in a bridge job. `project_id` may differ from the
/// parent's project for multi-project pipelines.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DownstreamPipeline {
    pub id: u64,
    pub project_id: u64,
    pub status: CiStatus,
    #[serde(default)]
    pub web_url: Option<String>,
}

/// Merge request summary from the list endpoints. `head_pipeline` is NOT included here — GitLab
/// only returns it from the single-MR endpoint ([`GitLabClient::get_merge_request`]).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MergeRequest {
    pub id: u64,
    pub iid: u64,
    pub project_id: u64,
    pub title: String,
    #[serde(default)]
    pub author: Option<MrAuthor>,
    pub source_branch: String,
    pub target_branch: String,
    #[serde(default)]
    pub sha: Option<String>,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub has_conflicts: bool,
    #[serde(default)]
    pub detailed_merge_status: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MrAuthor {
    pub username: String,
}

/// The single-MR response, fetched for the fields the list endpoint omits.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MergeRequestDetails {
    #[serde(default)]
    pub head_pipeline: Option<Pipeline>,
}

/// Deployment environment returned by `GET /projects/:id/environments`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Environment {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub state: Option<String>,
    /// The environment's public URL, when configured (what "open in browser" should prefer).
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub last_deployment: Option<Deployment>,
}

/// The most recent deployment of an environment.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Deployment {
    #[serde(default)]
    pub status: Option<CiStatus>,
    #[serde(rename = "ref")]
    #[serde(default)]
    pub ref_: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The job that performed the deployment.
    #[serde(default)]
    pub deployable: Option<Job>,
}

/// Project CI/CD variable returned by GitLab. Values are secrets; never persist them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProjectVariable {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub variable_type: Option<String>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub masked: bool,
    #[serde(default)]
    pub environment_scope: Option<String>,
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde::Deserialize;

    use super::{next_page_from_headers, parse_next_page_header, retry_after};

    #[derive(Debug, Deserialize)]
    struct RefField {
        #[serde(rename = "ref")]
        ref_: Option<String>,
    }

    #[test]
    fn deserializes_ref_field() {
        let item: RefField = serde_json::from_str(r#"{"ref":"main"}"#).expect("json");
        assert_eq!(item.ref_.as_deref(), Some("main"));
    }

    #[test]
    fn parse_next_page_header_returns_none_for_empty_value() {
        assert_eq!(parse_next_page_header("   "), None);
    }

    #[test]
    fn ci_status_parses_known_and_unknown_values() {
        use super::CiStatus;
        assert_eq!(CiStatus::from("success"), CiStatus::Success);
        assert_eq!(CiStatus::from("canceled"), CiStatus::Canceled);
        assert_eq!(
            CiStatus::from("waiting_for_resource"),
            CiStatus::WaitingForResource
        );
        assert_eq!(
            CiStatus::from("some_new_status"),
            CiStatus::Other(String::from("some_new_status"))
        );
    }

    #[test]
    fn ci_status_is_active_covers_in_flight_states() {
        use super::CiStatus;
        for status in [
            CiStatus::Running,
            CiStatus::Pending,
            CiStatus::Created,
            CiStatus::WaitingForResource,
            CiStatus::Preparing,
            CiStatus::Scheduled,
            CiStatus::from("canceling"),
        ] {
            assert!(status.is_active(), "{status} should be active");
        }
        for status in [
            CiStatus::Success,
            CiStatus::Failed,
            CiStatus::Canceled,
            CiStatus::Skipped,
            CiStatus::Manual,
            CiStatus::Other(String::from("weird")),
        ] {
            assert!(!status.is_active(), "{status} should not be active");
        }
    }

    #[test]
    fn ci_status_serializes_as_plain_string_for_cache_compatibility() {
        use super::CiStatus;
        // The wire/cache representation must stay a bare JSON string so old caches still load.
        assert_eq!(
            serde_json::to_string(&CiStatus::Success).expect("encode"),
            r#""success""#
        );
        let parsed: CiStatus = serde_json::from_str(r#""failed""#).expect("decode");
        assert_eq!(parsed, CiStatus::Failed);
        // Unknown statuses round-trip verbatim.
        let unknown: CiStatus = serde_json::from_str(r#""brand_new""#).expect("decode");
        assert_eq!(unknown, CiStatus::Other(String::from("brand_new")));
        assert_eq!(
            serde_json::to_string(&unknown).expect("encode"),
            r#""brand_new""#
        );
    }

    #[test]
    fn parse_next_page_header_trims_valid_value() {
        assert_eq!(parse_next_page_header(" 42 "), Some("42"));
    }

    #[test]
    fn next_page_from_headers_extracts_owned_value() {
        let mut headers = HeaderMap::new();
        headers.insert("x-next-page", HeaderValue::from_static(" 7 "));

        assert_eq!(
            next_page_from_headers(&headers).expect("header"),
            Some(String::from("7"))
        );
    }

    #[tokio::test]
    async fn list_group_projects_encodes_path_and_follows_pagination() {
        let server = httpmock::MockServer::start_async().await;

        let first_page = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/groups/my-org%2Fbackend/projects")
                    .query_param("include_subgroups", "true")
                    .query_param("page", "1");
                then.status(200)
                    .header("x-next-page", "2")
                    .json_body(serde_json::json!([
                        {"id": 1, "name": "api", "path_with_namespace": "my-org/backend/api"}
                    ]));
            })
            .await;
        let second_page = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/groups/my-org%2Fbackend/projects")
                    .query_param("page", "2");
                then.status(200).json_body(serde_json::json!([
                    {"id": 2, "name": "web", "path_with_namespace": "my-org/backend/web"}
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let projects = client
            .list_group_projects("my-org/backend", true)
            .await
            .expect("group projects");

        first_page.assert_async().await;
        second_page.assert_async().await;
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].path_with_namespace, "my-org/backend/api");
        assert_eq!(projects[1].id, 2);
    }

    #[tokio::test]
    async fn get_pipeline_bridges_decodes_downstream_and_tolerates_missing_one() {
        let server = httpmock::MockServer::start_async().await;
        let bridges_mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/pipelines/99/bridges");
                then.status(200).json_body(serde_json::json!([
                    {
                        "id": 1, "name": "trigger-deploy", "status": "success", "stage": "deploy",
                        "downstream_pipeline": {
                            "id": 500, "project_id": 8, "status": "failed",
                            "web_url": "https://gitlab.example/child/-/pipelines/500"
                        }
                    },
                    // A bridge whose downstream pipeline hasn't been created yet.
                    {"id": 2, "name": "trigger-docs", "status": "created", "downstream_pipeline": null}
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let bridges = client.get_pipeline_bridges(7, 99).await.expect("bridges");

        bridges_mock.assert_async().await;
        assert_eq!(bridges.len(), 2);
        let downstream = bridges[0].downstream_pipeline.as_ref().expect("downstream");
        assert_eq!(downstream.project_id, 8);
        assert_eq!(downstream.status, super::CiStatus::Failed);
        assert!(bridges[1].downstream_pipeline.is_none());
    }

    #[tokio::test]
    async fn download_job_artifacts_streams_body_to_file() {
        let server = httpmock::MockServer::start_async().await;
        let body = vec![0xABu8; 64 * 1024]; // larger than one chunk boundary
        let archive = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/jobs/42/artifacts");
                then.status(200).body(&body);
            })
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("artifacts.zip");
        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let written = client
            .download_job_artifacts(7, 42, &dest)
            .await
            .expect("download");

        archive.assert_async().await;
        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dest).expect("read back"), body);
    }

    #[test]
    fn job_deserializes_artifacts_file_when_present_and_absent() {
        let with: super::Job = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "build", "status": "success",
            "artifacts_file": {"filename": "artifacts.zip", "size": 1024}
        }))
        .expect("job with artifacts");
        let archive = with.artifacts_file.expect("artifacts present");
        assert_eq!(archive.filename, "artifacts.zip");
        assert_eq!(archive.size, 1024);

        let without: super::Job = serde_json::from_value(serde_json::json!({
            "id": 2, "name": "lint", "status": "success"
        }))
        .expect("job without artifacts");
        assert!(without.artifacts_file.is_none());
    }

    #[tokio::test]
    async fn list_environments_enriches_each_row_with_its_last_deployment() {
        let server = httpmock::MockServer::start_async().await;
        // The list endpoint omits last_deployment — only the per-environment endpoint has it.
        let list = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/environments")
                    .query_param("states", "available");
                then.status(200).json_body(serde_json::json!([
                    {"id": 1, "name": "production", "state": "available"},
                    {"id": 2, "name": "staging", "state": "available"}
                ]));
            })
            .await;
        let production = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/environments/1");
                then.status(200).json_body(serde_json::json!({
                    "id": 1, "name": "production", "state": "available",
                    "external_url": "https://prod.example",
                    "last_deployment": {
                        "status": "success", "ref": "main", "sha": "abc123",
                        "created_at": "2026-07-01T10:00:00Z",
                        "deployable": {"id": 42, "name": "deploy-prod", "status": "success"}
                    }
                }));
            })
            .await;
        let staging = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/environments/2");
                then.status(200).json_body(serde_json::json!(
                    {"id": 2, "name": "staging", "state": "available", "last_deployment": null}
                ));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let envs = client.list_environments(7).await.expect("environments");

        list.assert_async().await;
        production.assert_async().await;
        staging.assert_async().await;
        assert_eq!(envs.len(), 2);
        let deployment = envs[0].last_deployment.as_ref().expect("deployment");
        assert_eq!(deployment.ref_.as_deref(), Some("main"));
        assert_eq!(
            deployment.deployable.as_ref().map(|job| job.name.as_str()),
            Some("deploy-prod")
        );
        assert!(envs[1].last_deployment.is_none());
    }

    #[tokio::test]
    async fn list_project_merge_requests_decodes_draft_and_conflicts() {
        let server = httpmock::MockServer::start_async().await;
        let list = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/merge_requests")
                    .query_param("state", "opened");
                then.status(200).json_body(serde_json::json!([
                    {
                        "id": 100, "iid": 5, "project_id": 7, "title": "Add feature",
                        "author": {"username": "alice"},
                        "source_branch": "feat", "target_branch": "main", "state": "opened",
                        "draft": true, "has_conflicts": true,
                        "detailed_merge_status": "draft_status",
                        "web_url": "https://gitlab.example/mr/5"
                    },
                    {
                        "id": 101, "iid": 6, "project_id": 7, "title": "Fix bug",
                        "source_branch": "fix", "target_branch": "main", "state": "opened"
                    }
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let mrs = client.list_project_merge_requests(7).await.expect("mrs");

        list.assert_async().await;
        assert_eq!(mrs.len(), 2);
        assert!(mrs[0].draft && mrs[0].has_conflicts);
        assert_eq!(
            mrs[0]
                .author
                .as_ref()
                .map(|author| author.username.as_str()),
            Some("alice")
        );
        assert!(!mrs[1].draft && mrs[1].author.is_none());
    }

    #[tokio::test]
    async fn get_merge_request_decodes_head_pipeline() {
        let server = httpmock::MockServer::start_async().await;
        let single = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/merge_requests/5");
                then.status(200).json_body(serde_json::json!({
                    "id": 100, "iid": 5,
                    "head_pipeline": {"id": 900, "status": "failed"}
                }));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let details = client.get_merge_request(7, 5).await.expect("details");

        single.assert_async().await;
        let pipeline = details.head_pipeline.expect("head pipeline");
        assert_eq!(pipeline.id, 900);
        assert_eq!(pipeline.status, super::CiStatus::Failed);
    }

    #[tokio::test]
    async fn approve_and_merge_hit_the_right_endpoints() {
        let server = httpmock::MockServer::start_async().await;
        let approve = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/merge_requests/5/approve");
                then.status(201).json_body(serde_json::json!({"id": 100}));
            })
            .await;
        let merge = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::PUT)
                    .path("/api/v4/projects/7/merge_requests/5/merge")
                    .query_param("sha", "reviewed-sha");
                then.status(200)
                    .json_body(serde_json::json!({"id": 100, "state": "merged"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        client.approve_merge_request(7, 5).await.expect("approve");
        client
            .merge_merge_request(7, 5, "reviewed-sha")
            .await
            .expect("merge");

        approve.assert_async().await;
        merge.assert_async().await;
    }

    #[test]
    fn retry_after_parses_integer_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static(" 5 "));
        assert_eq!(
            retry_after(&headers),
            Some(std::time::Duration::from_secs(5))
        );

        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn send_get_retries_once_after_429() {
        let server = httpmock::MockServer::start_async().await;

        let throttled = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/1/variables")
                    .query_param("page", "1");
                // Retry-After: 0 keeps the test fast; the retry still has to happen.
                then.status(429).header("retry-after", "0");
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let _ = client.list_project_variables(1).await;

        // Two attempts: the initial 429 plus the single retry.
        throttled.assert_calls_async(2).await;
    }

    #[test]
    fn truncate_trace_tail_returns_short_traces_unchanged() {
        let text = String::from("line one\nline two\n");
        assert_eq!(super::truncate_trace_tail(text.clone(), 1024), text);
    }

    #[test]
    fn truncate_trace_tail_keeps_whole_lines_and_adds_marker() {
        let text = String::from("first line\nsecond line\nthird line\n");
        let truncated = super::truncate_trace_tail(text.clone(), 20);

        assert!(truncated.starts_with(&format!("--- truncated ({} bytes total) ---\n", text.len())));
        assert!(truncated.ends_with("third line\n"));
        assert!(!truncated.contains("second"));
    }

    #[test]
    fn truncate_trace_tail_respects_utf8_boundaries_without_newlines() {
        let text = "є".repeat(100);
        let truncated = super::truncate_trace_tail(text, 7);
        assert!(truncated.contains("truncated"));
    }

    #[tokio::test]
    async fn retry_pipeline_posts_to_retry_endpoint() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/pipelines/99/retry");
                then.status(201)
                    .json_body(serde_json::json!({"id": 99, "status": "running"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let pipeline = client.retry_pipeline(7, 99).await.expect("retry");

        mock.assert_async().await;
        assert_eq!(pipeline.status, super::CiStatus::Running);
    }

    #[tokio::test]
    async fn client_does_not_follow_redirects() {
        // A redirect must never be followed: PRIVATE-TOKEN is a custom header, so reqwest would
        // forward it to the redirect target (it only strips Authorization cross-host), leaking the
        // token. Assert the target is never contacted.
        let server = httpmock::MockServer::start_async().await;
        let redirect = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/pipelines/99/retry");
                then.status(302)
                    .header("location", "/api/v4/projects/7/pipelines/99/leaked");
            })
            .await;
        let target = server
            .mock_async(|when, then| {
                when.path("/api/v4/projects/7/pipelines/99/leaked");
                then.status(201)
                    .json_body(serde_json::json!({"id": 99, "status": "running"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let result = client.retry_pipeline(7, 99).await;

        assert!(result.is_err(), "a 302 must not resolve to success");
        redirect.assert_async().await;
        target.assert_calls_async(0).await; // redirect target never hit -> token not leaked
    }

    #[tokio::test]
    async fn list_pipelines_requests_sorted_page_scoped_to_ref() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/pipelines")
                    .query_param("per_page", "20")
                    .query_param("order_by", "id")
                    .query_param("sort", "desc")
                    .query_param("ref", "main");
                then.status(200).json_body(serde_json::json!([
                    {"id": 99, "iid": 9, "status": "success", "ref": "main"},
                    {"id": 98, "iid": 8, "status": "failed", "ref": "main"},
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let pipelines = client
            .list_pipelines(7, Some("main"), 20)
            .await
            .expect("list");

        mock.assert_async().await;
        assert_eq!(pipelines.len(), 2);
        assert_eq!(pipelines[0].id, 99);
        assert_eq!(pipelines[1].status, super::CiStatus::Failed);
    }

    #[tokio::test]
    async fn list_running_pipelines_filters_by_running_status() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/pipelines")
                    .query_param("status", "running")
                    .query_param("order_by", "id")
                    .query_param("sort", "desc");
                then.status(200).json_body(
                    serde_json::json!([{"id": 50, "status": "running", "ref": "feature/x"}]),
                );
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let running = client.list_running_pipelines(7, 5).await.expect("list");

        mock.assert_async().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].ref_.as_deref(), Some("feature/x"));
    }

    #[tokio::test]
    async fn create_pipeline_posts_to_pipeline_endpoint_with_ref() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/pipeline")
                    .query_param("ref", "feature/x");
                then.status(201).json_body(
                    serde_json::json!({"id": 100, "status": "pending", "ref": "feature/x"}),
                );
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let pipeline = client.create_pipeline(7, "feature/x").await.expect("run");

        mock.assert_async().await;
        assert_eq!(pipeline.id, 100);
        assert_eq!(pipeline.status, super::CiStatus::Pending);
    }

    #[tokio::test]
    async fn list_tags_follows_pagination() {
        let server = httpmock::MockServer::start_async().await;
        let first = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/repository/tags")
                    .query_param("page", "1");
                then.status(200).header("x-next-page", "2").json_body(
                    serde_json::json!([{"name": "v1.0.0", "message": "first", "target": "abc"}]),
                );
            })
            .await;
        let second = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/repository/tags")
                    .query_param("page", "2");
                then.status(200)
                    .json_body(serde_json::json!([{"name": "v0.9.0"}]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let tags = client.list_tags(7).await.expect("tags");

        first.assert_async().await;
        second.assert_async().await;
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "v1.0.0");
        assert_eq!(tags[0].message.as_deref(), Some("first"));
        assert_eq!(tags[1].message, None);
    }

    #[tokio::test]
    async fn create_tag_encodes_params_and_includes_message_when_set() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/repository/tags")
                    .query_param("tag_name", "v2.0.0")
                    .query_param("ref", "feature/x")
                    .query_param("message", "release notes");
                then.status(201)
                    .json_body(serde_json::json!({"name": "v2.0.0", "message": "release notes"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let tag = client
            .create_tag(7, "v2.0.0", "feature/x", "release notes")
            .await
            .expect("create");

        mock.assert_async().await;
        assert_eq!(tag.name, "v2.0.0");
    }

    #[tokio::test]
    async fn create_tag_omits_message_when_empty() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/repository/tags")
                    .query_param("tag_name", "v2.0.0")
                    .query_param("ref", "main")
                    .query_param_missing("message");
                then.status(201)
                    .json_body(serde_json::json!({"name": "v2.0.0"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let tag = client
            .create_tag(7, "v2.0.0", "main", "")
            .await
            .expect("create");

        mock.assert_async().await;
        assert_eq!(tag.message, None);
    }

    #[tokio::test]
    async fn cancel_pipeline_posts_to_cancel_endpoint() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/pipelines/99/cancel");
                then.status(200)
                    .json_body(serde_json::json!({"id": 99, "status": "canceled"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let pipeline = client.cancel_pipeline(7, 99).await.expect("cancel");

        mock.assert_async().await;
        assert_eq!(pipeline.status, super::CiStatus::Canceled);
    }

    #[tokio::test]
    async fn play_and_retry_job_post_to_job_endpoints() {
        let server = httpmock::MockServer::start_async().await;
        let play = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/jobs/10/play");
                then.status(200).json_body(
                    serde_json::json!({"id": 10, "name": "deploy", "status": "pending"}),
                );
            })
            .await;
        let retry = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v4/projects/7/jobs/10/retry");
                then.status(201).json_body(
                    serde_json::json!({"id": 11, "name": "deploy", "status": "pending"}),
                );
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let played = client.play_job(7, 10).await.expect("play");
        let retried = client.retry_job(7, 10).await.expect("retry");

        play.assert_async().await;
        retry.assert_async().await;
        assert_eq!(played.id, 10);
        assert_eq!(retried.id, 11, "retry creates a new job id");
    }

    #[tokio::test]
    async fn get_job_trace_returns_plain_text() {
        let server = httpmock::MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/jobs/10/trace");
                then.status(200).body("step one\nstep two\n");
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let trace = client.get_job_trace(7, 10).await.expect("trace");

        mock.assert_async().await;
        assert_eq!(trace, "step one\nstep two\n");
    }

    #[tokio::test]
    async fn get_job_trace_retries_once_after_429_with_range() {
        let server = httpmock::MockServer::start_async().await;
        let throttled = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/jobs/10/trace")
                    .header("range", format!("bytes=-{}", super::MAX_TRACE_BYTES));
                then.status(429).header("retry-after", "0");
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let err = client.get_job_trace(7, 10).await.expect_err("still throttled");

        throttled.assert_calls_async(2).await;
        assert_eq!(err.http_status(), Some(429));
    }

    #[tokio::test]
    async fn get_job_trace_reports_404_without_retrying() {
        let server = httpmock::MockServer::start_async().await;
        let missing = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/jobs/10/trace");
                then.status(404);
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let err = client.get_job_trace(7, 10).await.expect_err("missing log");

        missing.assert_calls_async(1).await;
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn list_project_variables_maps_403_to_forbidden() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/variables");
                then.status(403)
                    .json_body(serde_json::json!({"message": "403 Forbidden"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let err = client
            .list_project_variables(7)
            .await
            .expect_err("403 should fail");

        assert!(err.is_forbidden());
    }

    #[tokio::test]
    async fn list_project_variables_follows_pagination() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/variables")
                    .query_param("page", "1");
                then.status(200)
                    .header("x-next-page", "2")
                    .json_body(serde_json::json!([{"key": "A", "value": "1", "masked": true}]));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/variables")
                    .query_param("page", "2");
                then.status(200).json_body(serde_json::json!([
                    {"key": "B", "value": "2", "environment_scope": "prod"}
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let variables = client.list_project_variables(7).await.expect("variables");

        assert_eq!(variables.len(), 2);
        assert_eq!(variables[0].key, "A");
        assert!(variables[0].masked);
        assert_eq!(variables[1].environment_scope.as_deref(), Some("prod"));
    }

    #[tokio::test]
    async fn get_current_user_validates_token() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/user")
                    .header("user-agent", concat!("invocra/", env!("CARGO_PKG_VERSION")));
                then.status(200)
                    .json_body(serde_json::json!({"id": 1, "username": "alex"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let user = client.get_current_user().await.expect("user");
        assert_eq!(user.username, "alex");
    }

    #[tokio::test]
    async fn get_current_user_maps_401_to_unauthorized() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET).path("/api/v4/user");
                then.status(401)
                    .json_body(serde_json::json!({"message": "401 Unauthorized"}));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "bad", None).expect("client");
        let err = client.get_current_user().await.expect_err("should fail");
        assert!(err.is_unauthorized());
    }

    #[tokio::test]
    async fn list_groups_returns_member_groups() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/groups")
                    .query_param("min_access_level", "10");
                then.status(200).json_body(serde_json::json!([
                    {"id": 5, "name": "Backend", "full_path": "my-org/backend"}
                ]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let groups = client.list_groups().await.expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].full_path, "my-org/backend");
    }

    #[tokio::test]
    async fn list_group_projects_accepts_numeric_group_reference() {
        let server = httpmock::MockServer::start_async().await;

        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/groups/99/projects")
                    .query_param("include_subgroups", "false");
                then.status(200).json_body(serde_json::json!([]));
            })
            .await;

        let client = super::GitLabClient::new(server.base_url(), "token", None).expect("client");
        let projects = client
            .list_group_projects("99", false)
            .await
            .expect("group projects");

        mock.assert_async().await;
        assert!(projects.is_empty());
    }
    #[tokio::test]
    async fn get_pipeline_uses_the_requested_project_and_pipeline() {
        let server = httpmock::MockServer::start_async().await;
        let request = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/8/pipelines/500");
                then.status(200)
                    .json_body(serde_json::json!({"id":500,"status":"canceled","ref":"feature"}));
            })
            .await;
        let client =
            super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
        let pipeline = client.get_pipeline(8, 500).await.expect("get pipeline");
        request.assert_async().await;
        assert_eq!(pipeline.status, super::CiStatus::Canceled);
    }

    #[tokio::test]
    async fn merge_rejects_changed_source_sha_without_retry() {
        let server = httpmock::MockServer::start_async().await;
        let request = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::PUT)
                    .path("/api/v4/projects/7/merge_requests/5/merge")
                    .query_param("sha", "reviewed-sha");
                then.status(409)
                    .json_body(serde_json::json!({"message":"SHA does not match HEAD"}));
            })
            .await;
        let client =
            super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
        let err = client
            .merge_merge_request(7, 5, "reviewed-sha")
            .await
            .expect_err("source changed");
        request.assert_async().await;
        assert_eq!(err.http_status(), Some(409));
    }

    #[test]
    fn artifacts_token_is_sent_only_to_the_original_origin() {
        let origin = reqwest::Url::parse("https://git.example/api/v4/projects/7/jobs/9/artifacts")
            .expect("url");
        let client = super::http::HttpClient::new("https://git.example", "test-token", None)
            .expect("client");
        let same = client
            .artifacts_request(&origin, &origin)
            .build()
            .expect("same origin request");
        assert_eq!(
            same.headers().get("PRIVATE-TOKEN").expect("token"),
            "test-token"
        );
        let cdn =
            reqwest::Url::parse("https://cdn.example/archive.zip?signature=secret").expect("url");
        let foreign = client
            .artifacts_request(&cdn, &origin)
            .build()
            .expect("cdn request");
        assert!(!foreign.headers().contains_key("PRIVATE-TOKEN"));
    }

    #[tokio::test]
    async fn artifacts_follow_same_origin_redirect_and_save_the_archive() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/api/v4/projects/7/jobs/9/artifacts");
                then.status(302)
                    .header("Location", "/archive.zip")
                    .body("redirect body");
            })
            .await;
        let archive = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/archive.zip")
                    .header("PRIVATE-TOKEN", "test-token");
                then.status(200).body("PK-archive");
            })
            .await;
        let client =
            super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("archive.zip");
        assert_eq!(
            client
                .download_job_artifacts(7, 9, &dest)
                .await
                .expect("archive"),
            10
        );
        archive.assert_async().await;
        assert_eq!(std::fs::read(dest).expect("archive file"), b"PK-archive");
    }

    #[tokio::test]
    async fn unsafe_or_looping_artifact_redirect_preserves_existing_file() {
        for location in [
            "http://unrelated.example/archive.zip",
            "/api/v4/projects/7/jobs/9/artifacts",
        ] {
            let server = httpmock::MockServer::start_async().await;
            server
                .mock_async(|when, then| {
                    when.method(httpmock::Method::GET)
                        .path("/api/v4/projects/7/jobs/9/artifacts");
                    then.status(302)
                        .header("Location", location)
                        .body("redirect body");
                })
                .await;
            let client =
                super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
            let dir = tempfile::tempdir().expect("tempdir");
            let dest = dir.path().join("archive.zip");
            std::fs::write(&dest, "original archive").expect("seed");
            assert!(client.download_job_artifacts(7, 9, &dest).await.is_err());
            assert_eq!(
                std::fs::read_to_string(dest).expect("old archive"),
                "original archive"
            );
        }
    }

    #[tokio::test]
    async fn jobs_and_bridges_follow_all_pages() {
        for route in ["jobs", "bridges"] {
            let server = httpmock::MockServer::start_async().await;
            let path = format!("/api/v4/projects/7/pipelines/9/{route}");
            let first = server
                .mock_async(|when, then| {
                    when.method(httpmock::Method::GET)
                        .path(&path)
                        .query_param("page", "1")
                        .query_param("per_page", "100");
                    then.status(200)
                        .header("x-next-page", "2")
                        .json_body(serde_json::json!([{"id":1,"name":"first","status":"success"}]));
                })
                .await;
            let second = server
                .mock_async(|when, then| {
                    when.method(httpmock::Method::GET)
                        .path(&path)
                        .query_param("page", "2");
                    then.status(200).json_body(
                        serde_json::json!([{"id":2,"name":"second","status":"success"}]),
                    );
                })
                .await;
            let client =
                super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
            let ids: Vec<u64> = if route == "jobs" {
                client
                    .get_pipeline_jobs(7, 9)
                    .await
                    .expect("jobs")
                    .into_iter()
                    .map(|job| job.id)
                    .collect()
            } else {
                client
                    .get_pipeline_bridges(7, 9)
                    .await
                    .expect("bridges")
                    .into_iter()
                    .map(|job| job.id)
                    .collect()
            };
            assert_eq!(ids, [1, 2]);
            first.assert_async().await;
            second.assert_async().await;
        }
    }

    #[tokio::test]
    async fn pagination_rejects_nonadvancing_page_without_repeating_request() {
        for next in ["0", "1"] {
            let server = httpmock::MockServer::start_async().await;
            let request = server
                .mock_async(|when, then| {
                    when.method(httpmock::Method::GET).path("/api/v4/projects");
                    then.status(200)
                        .header("x-next-page", next)
                        .json_body(serde_json::json!([]));
                })
                .await;
            let client =
                super::GitLabClient::new(server.base_url(), "test-token", None).expect("client");
            assert!(matches!(
                client.list_projects().await,
                Err(super::GitLabError::Pagination { .. })
            ));
            request.assert_calls_async(1).await;
        }
    }

    #[test]
    fn invalid_pagination_header_is_not_treated_as_end_of_list() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-next-page",
            HeaderValue::from_bytes(&[255]).expect("header bytes"),
        );
        assert!(next_page_from_headers(&headers).is_err());
    }
}
