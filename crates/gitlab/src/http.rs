//! HTTP transport; demo clients never construct this type.

use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::{next_page_from_headers, truncate_trace_tail, GitLabError, Result, MAX_TRACE_BYTES};

#[derive(Debug, Clone)]
pub(crate) struct HttpClient {
    pub(super) limiter: Arc<crate::rate_limit::RateLimiter>,
    pub(super) priority: crate::rate_limit::Priority,
    base_url: String,
    client: reqwest::Client,
    artifacts_client: reqwest::Client,
    token_header: HeaderValue,
}

impl HttpClient {
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let mut token_header = HeaderValue::from_str(&token.into())
            .map_err(|source| GitLabError::InvalidTokenHeader { source })?;
        token_header.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("PRIVATE-TOKEN", token_header.clone());

        let build_client = |headers, total_timeout| {
            let mut builder = reqwest::Client::builder()
                .default_headers(headers)
                .user_agent(concat!("invocra/", env!("CARGO_PKG_VERSION")))
                // Never follow redirects: the PRIVATE-TOKEN header is custom, so reqwest would NOT
                // strip it on a cross-host redirect (it only strips Authorization), which would leak
                // the token to an attacker-controlled host. GitLab's API needs no redirects anyway.
                .redirect(reqwest::redirect::Policy::none())
                // Transparently gzip-decode responses; trims trace/JSON bytes on slow links.
                .gzip(true)
                .connect_timeout(std::time::Duration::from_secs(20))
                .read_timeout(std::time::Duration::from_secs(20));
            if total_timeout {
                builder = builder.timeout(std::time::Duration::from_secs(20));
            }
            if let Some(proxy) = proxy {
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy)
                        .map_err(|source| GitLabError::BuildClient { source })?,
                );
            }
            builder
                .build()
                .map_err(|source| GitLabError::BuildClient { source })
        };
        let client = build_client(headers, true)?;
        // Archives may take longer than 20s overall; keep connect/read inactivity limits instead.
        let artifacts_client = build_client(HeaderMap::new(), false)?;

        Ok(Self {
            limiter: Arc::default(),
            priority: crate::rate_limit::Priority::Interactive,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            artifacts_client,
            token_header,
        })
    }

    pub(super) async fn get_paginated_json<T>(&self, path_with_query: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut page = 1usize;
        let mut items = Vec::new();

        // hard ceiling so a hostile/MITM server can't drive x-next-page forever and OOM
        // the client; 1000 pages * per_page=100 = 100k items, far past any real account. Raise if a
        // legitimate listing ever exceeds it.
        const MAX_PAGES: usize = 1000;

        for _ in 0..MAX_PAGES {
            let url = format!("{}{}&page={page}", self.base_url, path_with_query);
            let response = self.send_get(&url, None).await?;

            let next_page = next_page_from_headers(response.headers())?;

            let mut batch =
                response
                    .json::<Vec<T>>()
                    .await
                    .map_err(|source| GitLabError::Decode {
                        url: url.clone(),
                        source,
                    })?;
            items.append(&mut batch);

            match next_page {
                Some(next) => {
                    let next_page =
                        next.parse::<usize>()
                            .map_err(|source| GitLabError::InvalidNextPage {
                                value: next,
                                source,
                            })?;
                    if next_page <= page {
                        return Err(GitLabError::Pagination {
                            reason: "x-next-page does not advance",
                        });
                    }
                    page = next_page;
                }
                None => return Ok(items),
            }
        }

        Err(GitLabError::Pagination {
            reason: "page limit exceeded",
        })
    }

    pub async fn download_job_artifacts(
        &self,
        project_id: u64,
        job_id: u64,
        dest: &std::path::Path,
    ) -> Result<u64> {
        use std::io::Write;

        let url = format!(
            "{}/api/v4/projects/{project_id}/jobs/{job_id}/artifacts",
            self.base_url
        );
        let redirect_err = || GitLabError::ArtifactsRedirect { url: url.clone() };
        let mut current = reqwest::Url::parse(&url).map_err(|_| redirect_err())?;
        let origin = current.clone();
        let mut redirects = 0;
        let mut response = loop {
            let same_origin = current.origin() == origin.origin();
            let permit = if same_origin {
                Some(self.limiter.acquire(self.priority).await)
            } else {
                None
            };
            let response = self
                .artifacts_request(&current, &origin)
                .send()
                .await
                .map_err(|source| GitLabError::Request {
                    url: url.clone(),
                    source: source.without_url(),
                })?;
            if let Some(permit) = permit {
                permit.observe(response.status(), response.headers());
            }
            if !response.status().is_redirection() {
                break response
                    .error_for_status()
                    .map_err(|source| GitLabError::Status {
                        url: url.clone(),
                        status: source.status().map(|status| status.as_u16()),
                        source: source.without_url(),
                    })?;
            }
            redirects += 1;
            if redirects > 5 {
                return Err(redirect_err());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(redirect_err)?;
            let next = current.join(location).map_err(|_| redirect_err())?;
            if (next.scheme() != "https" && next.origin() != origin.origin())
                || !next.username().is_empty()
                || next.password().is_some()
            {
                return Err(redirect_err());
            }
            current = next;
        };

        let write_err = |source: std::io::Error| GitLabError::ArtifactsWrite {
            path: dest.to_path_buf(),
            source,
        };
        // ponytail: short blocking writes on the multi-thread runtime; move to spawn_blocking or
        // tokio::fs only if downloads ever make the UI stutter.
        let parent = dest
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(write_err)?;
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| GitLabError::Request {
                url: url.clone(),
                source: source.without_url(),
            })?
        {
            file.write_all(&chunk).map_err(write_err)?;
            written += chunk.len() as u64;
        }
        file.as_file().sync_all().map_err(write_err)?;
        file.persist(dest).map_err(|err| write_err(err.error))?;
        Ok(written)
    }

    pub(super) fn artifacts_request(
        &self,
        url: &reqwest::Url,
        origin: &reqwest::Url,
    ) -> reqwest::RequestBuilder {
        let request = self.artifacts_client.get(url.clone());
        if url.origin() == origin.origin() {
            request.header("PRIVATE-TOKEN", self.token_header.clone())
        } else {
            request
        }
    }

    pub(super) async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}{}", self.base_url, path);
        let response = self.send_get(&url, None).await?;

        response
            .json::<T>()
            .await
            .map_err(|source| GitLabError::Decode { url, source })
    }

    pub(super) async fn send_get(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let mut request = self.client.get(url);
            if let Some(range) = range {
                request = request.header(reqwest::header::RANGE, range);
            }
            let permit = self.limiter.acquire(self.priority).await;
            let response = request
                .send()
                .await
                .map_err(|source| GitLabError::Request {
                    url: url.to_owned(),
                    source,
                })?;

            permit.observe(response.status(), response.headers());
            if attempt == 0 && response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                continue;
            }

            return response
                .error_for_status()
                .map_err(|source| GitLabError::Status {
                    url: url.to_owned(),
                    status: source.status().map(|status| status.as_u16()),
                    source,
                });
        }

        unreachable!("retry loop returns on the final attempt")
    }

    pub(super) async fn post_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}{}", self.base_url, path);
        let permit = self.limiter.acquire(self.priority).await;
        let response =
            self.client
                .post(&url)
                .send()
                .await
                .map_err(|source| GitLabError::Request {
                    url: url.clone(),
                    source,
                })?;
        permit.observe(response.status(), response.headers());
        let response = response
            .error_for_status()
            .map_err(|source| GitLabError::Status {
                url: url.clone(),
                status: source.status().map(|status| status.as_u16()),
                source,
            })?;

        response
            .json::<T>()
            .await
            .map_err(|source| GitLabError::Decode { url, source })
    }

    pub(super) async fn put_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}{}", self.base_url, path);
        let permit = self.limiter.acquire(self.priority).await;
        let response =
            self.client
                .put(&url)
                .send()
                .await
                .map_err(|source| GitLabError::Request {
                    url: url.clone(),
                    source,
                })?;
        permit.observe(response.status(), response.headers());
        let response = response
            .error_for_status()
            .map_err(|source| GitLabError::Status {
                url: url.clone(),
                status: source.status().map(|status| status.as_u16()),
                source,
            })?;

        response
            .json::<T>()
            .await
            .map_err(|source| GitLabError::Decode { url, source })
    }

    pub async fn get_job_trace(&self, project_id: u64, job_id: u64) -> Result<String> {
        let url = format!(
            "{}/api/v4/projects/{project_id}/jobs/{job_id}/trace",
            self.base_url
        );
        // Request only the retained tail; servers that ignore Range are truncated locally.
        let range = format!("bytes=-{MAX_TRACE_BYTES}");
        let response = self.send_get(&url, Some(&range)).await?;

        let text = response
            .text()
            .await
            .map_err(|source| GitLabError::Decode { url, source })?;
        Ok(truncate_trace_tail(text, MAX_TRACE_BYTES))
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use httpmock::{Method, MockServer};
    use serde_json::Value;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn response_quota_preserves_manual_reserve_across_client_clones() {
        let server = MockServer::start_async().await;
        let reset = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs()
            + 60;
        let mock = server
            .mock_async(|_, then| {
                then.status(200)
                    .header("ratelimit-remaining", "3")
                    .header("ratelimit-reset", reset.to_string())
                    .body("{}");
            })
            .await;
        let client = HttpClient::new(server.base_url(), "test", None).expect("client");
        let _: Value = client.get_json("/initial").await.expect("initial quota");
        let mut automatic = client.clone();
        automatic.priority = crate::rate_limit::Priority::OpenPipeline;
        let mut poll = Box::pin(automatic.get_json::<Value>("/poll"));
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut poll)
            .await
            .is_err());
        let _: Value = client.post_json("/manual").await.expect("manual action");
        let _: Value = client.put_json("/manual").await.expect("manual action");
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut poll)
            .await
            .is_err());
        mock.assert_calls_async(3).await;
        let reserve_wait = client.limiter.status().wait;
        assert!(!reserve_wait.is_zero());
        assert_eq!(client.limiter.status().server_pause, Duration::ZERO);
        let _: Value = client
            .get_json("/reserve")
            .await
            .expect("last reserved request");
        let server_pause = client.limiter.status().server_pause;
        assert!(!server_pause.is_zero());
        assert!(server_pause <= reserve_wait);
        mock.assert_calls_async(4).await;
        drop(poll);
        assert_eq!(client.limiter.status().waiting, 0);
    }

    #[tokio::test]
    async fn get_post_put_trace_and_artifacts_share_one_budget() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|_, then| {
                then.status(200).body("{}");
            })
            .await;
        let client = HttpClient::new(server.base_url(), "test", None).expect("client");
        client.limiter.configure(std::num::NonZeroU32::new(600));
        let start = Instant::now();
        let _: Value = client.get_json("/first").await.expect("GET");
        let clone = client.clone();
        let _: Value = clone.post_json("/second").await.expect("POST");
        let _: Value = client.put_json("/third").await.expect("PUT");
        clone.get_job_trace(1, 2).await.expect("trace");
        let dir = tempfile::tempdir().expect("directory");
        client
            .download_job_artifacts(1, 2, &dir.path().join("archive"))
            .await
            .expect("artifacts");
        assert!(start.elapsed() >= Duration::from_millis(400));
        mock.assert_calls_async(5).await;
    }

    #[tokio::test]
    async fn mutations_and_artifacts_record_cooldown_without_retrying() {
        for method in ["POST", "PUT", "GET"] {
            let server = MockServer::start_async().await;
            let mock = server
                .mock_async(|when, then| {
                    when.method(match method {
                        "POST" => Method::POST,
                        "PUT" => Method::PUT,
                        _ => Method::GET,
                    });
                    then.status(429).header("retry-after", "120");
                })
                .await;
            let client = HttpClient::new(server.base_url(), "test", None).expect("client");
            let dir = tempfile::tempdir().expect("directory");
            let failed = match method {
                "POST" => client.post_json::<Value>("/mutate").await.is_err(),
                "PUT" => client.put_json::<Value>("/mutate").await.is_err(),
                _ => client
                    .download_job_artifacts(1, 2, &dir.path().join("archive"))
                    .await
                    .is_err(),
            };
            assert!(failed);
            mock.assert_calls_async(1).await;
            let clone = client.clone();
            assert!(clone.limiter.status().server_pause > Duration::from_secs(119));
            assert!(tokio::time::timeout(
                Duration::from_millis(20),
                clone.get_json::<Value>("/blocked")
            )
            .await
            .is_err());
            mock.assert_calls_async(1).await;
            assert!(!dir.path().join("archive").exists());
        }
    }
}
