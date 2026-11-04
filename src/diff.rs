//! Reads what a change touches from git.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// The commits of `range` (oldest first), each with its `git patch-id --stable`: `None` for a
/// commit that changes nothing. A range holding a merge commit is an error, as a merge's patch
/// doesn't say what it changed.
pub(crate) fn patch_ids(dir: &Path, range: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut commits = Vec::new();
    for line in git(dir, &["rev-list", "--reverse", "--parents", range])?.lines() {
        let mut shas = line.split(' ');
        let commit = shas.next().unwrap_or_default().to_string();
        if shas.count() > 1 {
            bail!("{range} holds the merge commit {commit}");
        }
        commits.push(commit);
    }
    let log = git(
        dir,
        &[
            "log",
            "-p",
            "--no-merges",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "--no-decorate",
            "--format=commit %H",
            range,
        ],
    )?;
    let ids = patch_id(dir, &log)?;
    Ok(commits
        .into_iter()
        .map(|c| {
            let id = ids.get(&c).cloned();
            (c, id)
        })
        .collect())
}

/// `git patch-id --stable` over `log`: each commit's patch-id by commit.
fn patch_id(dir: &Path, log: &str) -> Result<std::collections::HashMap<String, String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git patch-id")?;
    let mut stdin = child.stdin.take().context("git patch-id's input")?;
    let input = log.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().context("running git patch-id")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writing to git patch-id"))?
        .context("writing to git patch-id")?;
    if !out.status.success() {
        bail!(
            "git patch-id: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (id, commit) = l.split_once(' ')?;
            Some((commit.trim().to_string(), id.to_string()))
        })
        .collect())
}

/// Where `line` of a file's old side lands on its new side, given the file's `git diff -U0`:
/// moved by the lines the hunks above it added and removed, or onto the new start of the hunk
/// that changed it.
pub(crate) fn moved_line(diff: &str, line: u32) -> u32 {
    let mut offset: i64 = 0;
    for (old, old_len, new, new_len) in hunk_ranges(diff) {
        // A hunk that removes nothing adds its lines after `old`.
        let (start, end) = if old_len == 0 {
            (old + 1, old + 1)
        } else {
            (old, old + old_len)
        };
        if line < start {
            break;
        }
        if line >= end {
            offset += i64::from(new_len) - i64::from(old_len);
            continue;
        }
        return new.max(1);
    }
    u32::try_from((i64::from(line) + offset).max(1)).unwrap_or(line)
}

/// Each hunk header's `(old start, old length, new start, new length)`.
fn hunk_ranges(diff: &str) -> impl Iterator<Item = (u32, u32, u32, u32)> + '_ {
    diff.lines().filter_map(|line| {
        let mut ranges = line.strip_prefix("@@ ")?.split(' ');
        let range = |r: Option<&str>, sign: char| -> Option<(u32, u32)> {
            let mut parts = r?.strip_prefix(sign)?.split(',');
            let start = parts.next()?.parse().ok()?;
            let len = match parts.next() {
                Some(n) => n.parse().ok()?,
                None => 1,
            };
            Some((start, len))
        };
        let (old, old_len) = range(ranges.next(), '-')?;
        let (new, new_len) = range(ranges.next(), '+')?;
        Some((old, old_len, new, new_len))
    })
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
    fn moves_a_line_past_the_hunks_above_it() {
        // Two lines added above line 1, line 5 rewritten as two, lines 9-10 removed, and one
        // line added after line 12.
        let diff = "@@ -0,0 +1,2 @@\n+a\n+b\n@@ -5 +7,2 @@\n-x\n+y\n+z\n\
                    @@ -9,2 +11,0 @@\n-p\n-q\n@@ -12,0 +14 @@\n+r\n";
        let at = |line| moved_line(diff, line);
        assert_eq!(at(1), 3);
        assert_eq!(at(4), 6);
        assert_eq!(at(5), 7, "a rewritten line lands on its hunk's new start");
        assert_eq!(at(6), 9);
        assert_eq!(at(8), 11);
        assert_eq!(at(9), 11, "a removed line lands on the line before it");
        assert_eq!(at(10), 11);
        assert_eq!(at(11), 12);
        assert_eq!(at(12), 13);
        assert_eq!(at(13), 15, "a line after an insertion moves down");
        assert_eq!(moved_line("", 7), 7);
        assert_eq!(moved_line("@@ -1,3 +0,0 @@\n-a\n-b\n-c\n", 2), 1);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@t"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(dir.path(), args).unwrap();
        }
        dir
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", file]).unwrap();
        git(dir, &["commit", "-q", "--allow-empty", "-m", file]).unwrap();
        git(dir, &["rev-parse", "HEAD"]).unwrap().trim().to_string()
    }

    #[test]
    fn a_rebased_commit_keeps_its_patch_id() {
        let dir = repo();
        let d = dir.path();
        let base = commit(d, "a", "1\n2\n3\n");
        git(d, &["switch", "-q", "-c", "feature"]).unwrap();
        commit(d, "b", "b\n");
        git(d, &["commit", "-q", "--allow-empty", "-m", "nothing"]).unwrap();
        commit(d, "a", "1\n2\n3\nfour\n");
        let old = patch_ids(d, &format!("{base}..feature")).unwrap();
        assert_eq!(old.len(), 3);
        assert!(old[1].1.is_none(), "an empty commit has no patch-id");

        git(d, &["switch", "-q", "main"]).unwrap();
        let onto = commit(d, "c", "c\n");
        git(d, &["switch", "-q", "feature"]).unwrap();
        git(d, &["rebase", "-q", "--keep-empty", "main"]).unwrap();
        let new = patch_ids(d, &format!("{onto}..feature")).unwrap();
        assert_eq!(new.len(), 3);
        assert_ne!(new[0].0, old[0].0, "the commits are new");
        let ids =
            |v: &[(String, Option<String>)]| v.iter().map(|c| c.1.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&new), ids(&old));

        git(d, &["switch", "-q", "-c", "merged", &base]).unwrap();
        git(d, &["merge", "-q", "--no-ff", "-m", "merge", "main"]).unwrap();
        assert!(patch_ids(d, &format!("{base}..merged")).is_err());
    }

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
