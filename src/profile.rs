//! Reviewer profiles: `<name>/profile.md` folders with YAML frontmatter and a prompt body.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The folder a repo keeps its own profiles in; a profile there replaces the default of the same
/// name.
pub const REPO_PROFILES_DIR: &str = "review-profiles";

/// Where a profile was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Shipped with lens.
    Default,
    /// The reviewed repo's own `review-profiles/` folder.
    Repo,
}

/// A profile's frontmatter. Field names follow `profiles/README.md`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    pub model: String,
    pub effort: String,
    pub severity_guidance: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inference_rules: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dispatch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minions: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub meta: Frontmatter,
    /// The `profile.md` file the profile came from.
    pub path: PathBuf,
    pub source: Source,
}

impl Profile {
    /// Parses a `profile.md`: a `---` fenced YAML block, then the reviewer prompt.
    pub fn parse(text: &str, path: PathBuf, source: Source) -> Result<Self> {
        let rest = text
            .strip_prefix("---")
            .with_context(|| format!("{}: no frontmatter", path.display()))?;
        let end = rest
            .find("\n---")
            .with_context(|| format!("{}: frontmatter is not closed", path.display()))?;
        let meta: Frontmatter = serde_yaml::from_str(&rest[..end])
            .with_context(|| format!("{}: bad frontmatter", path.display()))?;
        Ok(Self { meta, path, source })
    }

    pub fn load(path: &Path, source: Source) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text, path.to_path_buf(), source)
    }
}

/// Loads every `<dir>/<name>/profile.md`. A profile whose `name` doesn't match its folder is an
/// error, since overrides are matched by name.
pub fn load_dir(dir: &Path, source: Source) -> Result<Vec<Profile>> {
    let mut profiles = Vec::new();
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let file = entry.path().join("profile.md");
        if !file.is_file() {
            continue;
        }
        let profile = Profile::load(&file, source)?;
        let folder = entry.file_name();
        if profile.meta.name != folder.to_string_lossy() {
            bail!(
                "{}: name '{}' doesn't match its folder",
                file.display(),
                profile.meta.name
            );
        }
        profiles.push(profile);
    }
    profiles.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(profiles)
}

/// The defaults with the repo's profiles laid over them: a repo profile replaces the default of
/// the same name, and repo profiles with new names are added. Sorted by name.
pub fn merge(defaults: Vec<Profile>, repo: Vec<Profile>) -> Vec<Profile> {
    let mut by_name: BTreeMap<String, Profile> = defaults
        .into_iter()
        .map(|p| (p.meta.name.clone(), p))
        .collect();
    for profile in repo {
        by_name.insert(profile.meta.name.clone(), profile);
    }
    by_name.into_values().collect()
}

/// The defaults plus the overrides in `<repo_root>/review-profiles/`, when that folder exists.
pub fn load_all(defaults_dir: &Path, repo_root: &Path) -> Result<Vec<Profile>> {
    let defaults = load_dir(defaults_dir, Source::Default)?;
    let repo_dir = repo_root.join(REPO_PROFILES_DIR);
    let repo = if repo_dir.is_dir() {
        load_dir(&repo_dir, Source::Repo)?
    } else {
        Vec::new()
    };
    Ok(merge(defaults, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: perf\ndescription: Speed.\nmodel: sonnet\neffort: high\n\
        pathKeywords: [db, cache]\nseverityGuidance: >\n  major = slow.\nenabled: true\n---\n\nBody\n";

    fn named(name: &str, source: Source) -> Profile {
        let text = SAMPLE.replace("name: perf", &format!("name: {name}"));
        Profile::parse(&text, PathBuf::from(format!("{name}/profile.md")), source).unwrap()
    }

    #[test]
    fn parses_frontmatter() {
        let p = Profile::parse(SAMPLE, PathBuf::from("perf/profile.md"), Source::Default).unwrap();
        assert_eq!(p.meta.name, "perf");
        assert_eq!(p.meta.path_keywords, vec!["db", "cache"]);
        assert_eq!(p.meta.severity_guidance.trim(), "major = slow.");
        assert!(p.meta.languages.is_empty());
        assert!(!p.meta.dispatch);
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(Profile::parse("Body only", PathBuf::from("x"), Source::Default).is_err());
    }

    #[test]
    fn repo_profiles_override_defaults_by_name_and_add_new_ones() {
        let merged = merge(
            vec![named("a", Source::Default), named("b", Source::Default)],
            vec![named("b", Source::Repo), named("c", Source::Repo)],
        );
        let got: Vec<_> = merged
            .iter()
            .map(|p| (p.meta.name.as_str(), p.source))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", Source::Default),
                ("b", Source::Repo),
                ("c", Source::Repo)
            ]
        );
    }

    #[test]
    fn shipped_profiles_all_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles");
        let profiles = load_dir(&dir, Source::Default).unwrap();
        assert_eq!(profiles.len(), 14);
    }
}
