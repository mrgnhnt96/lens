//! Helpers shared by the review passes: reading an agent's reply and snapshotting a worktree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::diff::git;

/// The working tree as a tree object, untracked (not ignored) files included, without touching
/// the repo's index.
pub fn snapshot(root: &Path) -> Result<String> {
    let index =
        PathBuf::from(git(root, &["rev-parse", "--git-path", "lens-snapshot-index"])?.trim());
    let index = if index.is_absolute() {
        index
    } else {
        root.join(index)
    };
    let real = PathBuf::from(git(root, &["rev-parse", "--git-path", "index"])?.trim());
    let real = if real.is_absolute() {
        real
    } else {
        root.join(real)
    };
    if real.is_file() {
        fs::copy(&real, &index)?;
    }
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let tree = run(&["add", "-A", "."]).and_then(|_| run(&["write-tree"]));
    let _ = fs::remove_file(&index);
    tree
}

/// An agent's reply: plain JSON, or text ending in a fenced `json` block.
pub fn parse_reply(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Ok(v);
    }
    let start = text
        .rfind("```json")
        .context("no JSON and no ```json block in the reply")?;
    let body = &text[start + "```json".len()..];
    let body = body.find("```").map_or(body, |end| &body[..end]);
    serde_json::from_str(body.trim()).context("parsing the reply's ```json block")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_replies() {
        let reply = "I checked it.\n```json\n{\"verdict\": \"dropped\"}\n```\n";
        assert_eq!(parse_reply(reply).unwrap()["verdict"], "dropped");
        assert_eq!(parse_reply("{\"a\": 1}").unwrap()["a"], 1);
        assert!(parse_reply("no json here").is_err());
    }
}
