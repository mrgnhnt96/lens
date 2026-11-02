//! Where a repo's PRs live. The PR pass reads a PR, fetches it and posts its review through a
//! [`Forge`]; what to post (which findings, where, the markers) is forge-neutral, in [`crate::pr`].

use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ForgeKind, PrConfig};
use crate::pr::{Earlier, Review};

mod github;

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
}

impl PrView {
    /// The PR's title and description, for intent-alignment.
    pub fn intent(&self) -> String {
        format!("# {}\n\n{}\n", self.title, self.body)
    }
}

/// A forge's side of the PR pass, posting as `.lens.toml`'s `[pr] post_as`.
pub trait Forge {
    fn view(&self, pr: u64) -> Result<PrView>;

    /// Fetches the PR's base branch to `refs/remotes/origin/<base>` and its head to
    /// `refs/lens/pr/<n>`.
    fn fetch(&self, pr: &PrView) -> Result<()>;

    /// The findings earlier lens runs posted on PR `pr`, and the verdict the latest one gave.
    fn earlier(&self, pr: u64) -> Result<Earlier>;

    /// The request [`Forge::post`] sends, for a dry run to show.
    fn payload(&self, review: &Review) -> Value;

    /// Posts `review` on PR `pr` with its verdict and returns where it landed; `None` when the
    /// forge took nothing (an approval it wouldn't accept, with no findings to fall back on).
    fn post(&self, pr: u64, review: &Review) -> Result<Option<String>>;
}

/// The forge `[pr] forge` names, else the one origin's URL points at.
pub fn open(repo: &Path, config: &PrConfig) -> Result<Box<dyn Forge>> {
    match config.forge(repo) {
        ForgeKind::Github => Ok(Box::new(GitHub::new(repo, config)?)),
        ForgeKind::Bitbucket => {
            bail!("lens can't review Bitbucket pull requests yet; set [pr] forge = \"github\" in .lens.toml if this repo's PRs are on GitHub")
        }
    }
}
