//! Where a repo's PRs live. The PR pass reads a PR, fetches it and posts its review through a
//! [`Forge`]; what to post (which findings, where, the markers) is forge-neutral, in [`crate::pr`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ForgeKind, PrConfig};
use crate::outcome::Discussion;
use crate::pr::{Earlier, Markup, Review};

mod bitbucket;
mod github;

pub use bitbucket::Bitbucket;
#[cfg(test)]
pub(crate) use github::payload as github_payload;
pub use github::GitHub;

/// A PR's metadata, the same for every forge: what the PR pass writes to `pr.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrView {
    pub number: u64,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    /// The `workspace/repo` the head branch lives in, when it isn't origin's (a fork's PR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repo: Option<String>,
}

impl PrView {
    /// The PR's title and description, for intent-alignment.
    pub fn intent(&self) -> String {
        format!("# {}\n\n{}\n", self.title, self.body)
    }
}

/// A PR stacked on another: its base branch is the other PR's head branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub number: u64,
    pub url: String,
    pub merged: bool,
    /// The commit the merge made on the base branch, once merged, when the forge says.
    pub merge_commit: Option<String>,
}

/// A forge's side of the PR pass, posting as `.lens.toml`'s `[pr] post_as`.
pub trait Forge {
    fn view(&self, pr: u64) -> Result<PrView>;

    /// Fetches the PR's base branch to `refs/remotes/origin/<base>` and its head to
    /// `refs/lens/pr/<n>`.
    fn fetch(&self, pr: &PrView) -> Result<()>;

    /// The findings earlier lens runs posted on PR `pr`, and the verdict the latest one gave.
    fn earlier(&self, pr: u64) -> Result<Earlier>;

    /// How this forge renders what lens posts.
    fn markup(&self) -> Markup;

    /// What [`Forge::post`] sends for PR `pr`, for a dry run to show.
    fn payload(&self, pr: u64, review: &Review) -> Value;

    /// Posts `review` on PR `pr` with its verdict and returns where it landed; `None` when the
    /// forge took nothing (an approval it wouldn't accept, with no findings to fall back on).
    fn post(&self, pr: u64, review: &Review) -> Result<Option<String>>;

    /// What became of lens's findings on each of `prs`, read in as few calls as the forge
    /// allows, with each PR's head fetched to `refs/lens/pr/<n>`. A PR the forge can't tell about
    /// is left out; by default that is every PR.
    /// The open and merged PRs whose base is `branch`; declined ones are left out. By default
    /// none, for a forge that can't list them.
    fn children(&self, _branch: &str) -> Result<Vec<Child>> {
        Ok(vec![])
    }

    fn discussions(&self, _prs: &[u64]) -> Result<BTreeMap<u64, Discussion>> {
        Ok(BTreeMap::new())
    }
}

/// The forge `[pr] forge` names, else the one origin's URL points at.
pub fn open(repo: &Path, config: &PrConfig) -> Result<Box<dyn Forge>> {
    match config.forge(repo) {
        ForgeKind::Github => Ok(Box::new(GitHub::new(repo, config)?)),
        ForgeKind::Bitbucket => Ok(Box::new(Bitbucket::new(repo, config)?)),
    }
}
