//! Reads what a change touches from git.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The files a change touches, as paths relative to the repo root.
#[derive(Debug, Clone)]
pub struct Change {
    pub root: PathBuf,
    /// The commit the change is measured from: the merge base of `base` and `HEAD`.
    pub merge_base: String,
    pub files: Vec<String>,
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git printed non-UTF-8")
}

pub(crate) fn split_nul(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split('\0').filter(|p| !p.is_empty()).map(str::to_string)
}

/// Everything that differs between the merge base of `base` and `HEAD` and the working tree:
/// commits on the branch, staged and unstaged edits, and untracked files. Renames count as both
/// paths, so a profile keyed on the old location still sees the change.
pub fn changed_files(dir: &Path, base: &str) -> Result<Change> {
    let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?.trim());
    let merge_base = git(&root, &["merge-base", base, "HEAD"])
        .with_context(|| format!("finding the merge base of '{base}' and HEAD"))?
        .trim()
        .to_string();
    let mut files: Vec<String> = split_nul(&git(
        &root,
        &["diff", "--name-only", "--no-renames", "-z", &merge_base],
    )?)
    .chain(split_nul(&git(
        &root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?))
    .collect();
    files.sort();
    files.dedup();
    Ok(Change {
        root,
        merge_base,
        files,
    })
}

/// How a changed file differs from the merge base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

/// The status of each file in `change.files`. Untracked files count as added.
pub fn statuses(change: &Change) -> Result<Vec<(String, Status)>> {
    let out = git(
        &change.root,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            &change.merge_base,
        ],
    )?;
    let mut tracked = std::collections::HashMap::new();
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        let status = match code.chars().next() {
            Some('A') => Status::Added,
            Some('D') => Status::Deleted,
            _ => Status::Modified,
        };
        tracked.insert(path.to_string(), status);
    }
    Ok(change
        .files
        .iter()
        .map(|f| (f.clone(), tracked.get(f).copied().unwrap_or(Status::Added)))
        .collect())
}

/// The lines one file gained and lost since the merge base.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineChanges {
    /// `(line in the working tree, text)`.
    pub added: Vec<(u32, String)>,
    /// `(line at the merge base, text)`.
    pub removed: Vec<(u32, String)>,
}

/// `path` as it was at `rev`, as lines; empty when it didn't exist there.
pub fn lines_at(root: &Path, rev: &str, path: &str) -> Vec<String> {
    git(root, &["show", &format!("{rev}:{path}")])
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// What changed in `path` (relative to `root`) since `merge_base`. An untracked file comes back
/// as all added lines.
pub fn line_changes(root: &Path, merge_base: &str, path: &str) -> Result<LineChanges> {
    let tracked = !git(root, &["ls-files", "--", path])?.trim().is_empty()
        || !git(root, &["ls-tree", "--name-only", merge_base, "--", path])?
            .trim()
            .is_empty();
    if !tracked {
        let text = std::fs::read(root.join(path)).unwrap_or_default();
        let text = String::from_utf8_lossy(&text);
        return Ok(LineChanges {
            added: (1..).zip(text.lines().map(str::to_string)).collect(),
            removed: Vec::new(),
        });
    }
    let out = git(
        root,
        &[
            "diff",
            "-U0",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            merge_base,
            "--",
            path,
        ],
    )?;
    Ok(parse_hunks(&out))
}

/// Parses `git diff -U0` output for one file.
fn parse_hunks(diff: &str) -> LineChanges {
    let mut changes = LineChanges::default();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            let mut ranges = header.split(' ');
            let start = |r: Option<&str>| -> u32 {
                r.and_then(|r| r[1..].split(',').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0)
            };
            old = start(ranges.next());
            new = start(ranges.next());
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            changes.added.push((new, text.to_string()));
            new += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            changes.removed.push((old, text.to_string()));
            old += 1;
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zero_context_hunks() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -2 +2,2 @@ fn a\n-old\n+new\n+more\n\
                    @@ -9,2 +10,0 @@\n--- gone\n-gone too\n";
        let c = parse_hunks(diff);
        assert_eq!(
            c.added,
            vec![(2, "new".to_string()), (3, "more".to_string())]
        );
        assert_eq!(
            c.removed,
            vec![
                (2, "old".to_string()),
                (9, "-- gone".to_string()),
                (10, "gone too".to_string())
            ]
        );
    }
}
