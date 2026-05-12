use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::types::{GitHubError, OpenAutoMergePr, RecentMerge, RepoConfig};

const PULLS_PER_PAGE: u32 = 100;

#[derive(Debug, Deserialize)]
struct RepoResponse {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PrRepoRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct PrHead {
    #[serde(rename = "ref")]
    ref_: String,
    sha: String,
    repo: Option<PrRepoRef>,
}

#[derive(Debug, Deserialize)]
struct PrBase {
    #[serde(rename = "ref")]
    ref_: String,
    repo: PrRepoRef,
}

#[derive(Debug, Deserialize)]
struct PrUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct PrListItem {
    number: i64,
    title: String,
    body: Option<String>,
    auto_merge: Option<serde_json::Value>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    user: Option<PrUser>,
    head: PrHead,
    base: PrBase,
}

pub struct GitHubClient {
    http: Client,
    owner: String,
    name: String,
    default_branch: Arc<RwLock<Option<String>>>,
}

impl GitHubClient {
    pub fn new(repo: &RepoConfig) -> Result<Self, GitHubError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        let auth = HeaderValue::from_str(&format!("Bearer {}", repo.github_token))
            .map_err(|e| GitHubError::new(e.to_string(), None, "<init>"))?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("pr-manager-channel/0.1"),
        );

        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .default_headers(headers)
            .build()
            .map_err(|e| GitHubError::new(e.to_string(), None, "<init>"))?;

        Ok(Self {
            http,
            owner: repo.github_owner.clone(),
            name: repo.github_name.clone(),
            default_branch: Arc::new(RwLock::new(None)),
        })
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.name)
    }

    fn url(&self, path: &str) -> String {
        format!("https://api.github.com{path}")
    }

    pub async fn get_default_branch(&self) -> Result<String, GitHubError> {
        if let Some(b) = self.default_branch.read().await.clone() {
            return Ok(b);
        }
        let path = self.repo_path();
        let url = self.url(&path);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), e.status().map(status_u16), &path))?;
        let resp = ensure_success(resp, &path)?;
        let parsed: RepoResponse = resp
            .json()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), None, &path))?;
        let branch = parsed.default_branch;
        *self.default_branch.write().await = Some(branch.clone());
        Ok(branch)
    }

    pub async fn get_default_branch_sha(&self) -> Result<(String, String), GitHubError> {
        let branch = self.get_default_branch().await?;
        let encoded = url::form_urlencoded::byte_serialize(branch.as_bytes()).collect::<String>();
        let path = format!("{}/commits/{}", self.repo_path(), encoded);
        let resp = self
            .http
            .get(self.url(&path))
            .send()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), e.status().map(status_u16), &path))?;
        let resp = ensure_success(resp, &path)?;
        let parsed: CommitResponse = resp
            .json()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), None, &path))?;
        Ok((parsed.sha, branch))
    }

    pub async fn list_open_auto_merge_prs(&self) -> Result<Vec<OpenAutoMergePr>, GitHubError> {
        let mut out = Vec::new();
        let mut page = 1;

        loop {
            let items = self
                .list_pulls_page(PullListQuery {
                    state: "open",
                    sort: None,
                    direction: None,
                    per_page: PULLS_PER_PAGE,
                    page,
                })
                .await?;
            let is_last_page = items.len() < PULLS_PER_PAGE as usize;

            for p in items {
                if p.auto_merge.is_none() {
                    continue;
                }
                let head_repo = match p.head.repo {
                    Some(r) => r,
                    None => continue,
                };
                out.push(OpenAutoMergePr {
                    number: p.number,
                    title: p.title,
                    body: p.body.unwrap_or_default(),
                    head_branch: p.head.ref_,
                    head_sha: p.head.sha,
                    head_repo_id: head_repo.id,
                    base_repo_id: p.base.repo.id,
                    base_branch: p.base.ref_,
                    author_login: p.user.map(|u| u.login),
                });
            }

            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(out)
    }

    pub async fn list_recently_merged(&self, limit: u32) -> Result<Vec<RecentMerge>, GitHubError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let mut page = 1;

        loop {
            let items = self
                .list_pulls_page(PullListQuery {
                    state: "closed",
                    sort: Some("updated"),
                    direction: Some("desc"),
                    per_page: PULLS_PER_PAGE,
                    page,
                })
                .await?;
            let is_last_page = items.len() < PULLS_PER_PAGE as usize;

            for p in items {
                if out.len() as u32 >= limit {
                    break;
                }
                let merged_at = match p.merged_at {
                    Some(s) if !s.is_empty() => s,
                    _ => continue,
                };
                out.push(RecentMerge {
                    number: p.number,
                    title: p.title,
                    merged_at,
                });
            }

            if out.len() as u32 >= limit || is_last_page {
                break;
            }
            page += 1;
        }

        Ok(out)
    }

    async fn list_pulls_page(
        &self,
        query: PullListQuery<'_>,
    ) -> Result<Vec<PrListItem>, GitHubError> {
        let path = format!("{}/pulls", self.repo_path());
        let page = query.page.to_string();
        let per_page = query.per_page.to_string();
        let mut request = self.http.get(self.url(&path)).query(&[
            ("state", query.state),
            ("per_page", per_page.as_str()),
            ("page", page.as_str()),
        ]);

        if let Some(sort) = query.sort {
            request = request.query(&[("sort", sort)]);
        }
        if let Some(direction) = query.direction {
            request = request.query(&[("direction", direction)]);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), e.status().map(status_u16), &path))?;
        let resp = ensure_success(resp, &path)?;
        resp.json()
            .await
            .map_err(|e| GitHubError::new(e.to_string(), None, &path))
    }
}

struct PullListQuery<'a> {
    state: &'a str,
    sort: Option<&'a str>,
    direction: Option<&'a str>,
    per_page: u32,
    page: u32,
}

fn status_u16(s: StatusCode) -> u16 {
    s.as_u16()
}

fn ensure_success(
    resp: reqwest::Response,
    endpoint: &str,
) -> Result<reqwest::Response, GitHubError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    Err(GitHubError::new(
        format!("Request failed with status code {}", status.as_u16()),
        Some(status.as_u16()),
        endpoint,
    ))
}
