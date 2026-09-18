//! Thin GitHub client over `octocrab`, with a base-URL override for offline
//! development against a mock.

use thiserror::Error;

use octocrab::models::issues::Issue;
use octocrab::models::pulls::PullRequest;
use octocrab::Octocrab;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("octocrab: {0}")]
    Octocrab(#[from] octocrab::Error),
    #[error("invalid uri: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("missing owner/repo in {0}")]
    BadRepo(String),
}

#[derive(Clone)]
pub struct GitHubClient {
    octocrab: Octocrab,
}

/// Split a `"owner/repo"` string into its two parts.
fn split_repo(repo: &str) -> Result<(&str, &str), GitHubError> {
    repo.split_once('/')
        .ok_or_else(|| GitHubError::BadRepo(repo.to_string()))
}

impl GitHubClient {
    /// Build a client from a personal access token.
    ///
    /// `base_url` (optional) overrides the API root, e.g. for a local mock:
    /// `GitHubClient::new(token, Some("http://localhost:4001"))`.
    pub fn new(token: &str, base_url: Option<&str>) -> Result<Self, GitHubError> {
        let mut builder = Octocrab::builder().personal_token(token.to_string());
        if let Some(base) = base_url {
            builder = builder.base_uri(base.parse::<http::Uri>()?)?;
        }
        Ok(Self {
            octocrab: builder.build()?,
        })
    }

    /// List open issues for a repo (`"owner/repo"`).
    pub async fn list_issues(&self, repo: &str) -> Result<Vec<Issue>, GitHubError> {
        let (owner, name) = split_repo(repo)?;
        let page = self
            .octocrab
            .issues(owner, name)
            .list()
            .state(octocrab::params::State::Open)
            .per_page(50)
            .send()
            .await?;
        Ok(page.items)
    }

    pub async fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: Option<&str>,
    ) -> Result<Issue, GitHubError> {
        let (owner, name) = split_repo(repo)?;
        let issues = self.octocrab.issues(owner, name);
        let mut req = issues.create(title);
        if let Some(body) = body {
            req = req.body(body);
        }
        Ok(req.send().await?)
    }

    pub async fn comment_issue(
        &self,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<octocrab::models::issues::Comment, GitHubError> {
        let (owner, name) = split_repo(repo)?;
        Ok(self
            .octocrab
            .issues(owner, name)
            .create_comment(issue_number, body)
            .await?)
    }

    pub async fn open_pr(
        &self,
        repo: &str,
        title: &str,
        head: &str,
        base: &str,
        body: Option<&str>,
    ) -> Result<PullRequest, GitHubError> {
        let (owner, name) = split_repo(repo)?;
        let pulls = self.octocrab.pulls(owner, name);
        let mut req = pulls.create(title, head, base);
        if let Some(body) = body {
            req = req.body(body);
        }
        Ok(req.send().await?)
    }
}
