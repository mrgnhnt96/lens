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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
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

fn split_nul(s: &str) -> impl Iterator<Item = String> + '_ {
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
