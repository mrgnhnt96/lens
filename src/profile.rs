//! Reviewer profiles: `<name>/profile.md` folders with YAML frontmatter and a prompt body.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The folder a repo keeps its own profiles in; a profile there replaces the default of the same
/// name.
pub const REPO_PROFILES_DIR: &str = "review-profiles";

/// Profile folders kept outside the reviewed repo, as a path list (`:`-separated on Unix). Laid
/// over the defaults and the repo's `review-profiles/`, in order, the last folder winning.
/// `--profiles <dir>` replaces it, and `lens review` sets it for every process it starts.
pub const OVERRIDES_VAR: &str = "LENS_PROFILE_OVERRIDES";

/// Where a profile was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Shipped with lens.
    Default,
    /// The reviewed repo's own `review-profiles/` folder.
    Repo,
    /// A folder outside the repo, from `--profiles` or `$LENS_PROFILE_OVERRIDES`.
    Override,
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

/// The layers laid over each other in order: a profile replaces the one of the same name from an
/// earlier layer, and new names are added. Sorted by name.
pub fn merge(layers: impl IntoIterator<Item = Vec<Profile>>) -> Vec<Profile> {
    let mut by_name: BTreeMap<String, Profile> = BTreeMap::new();
    for profile in layers.into_iter().flatten() {
        by_name.insert(profile.meta.name.clone(), profile);
    }
    by_name.into_values().collect()
}

/// The defaults, then `<repo_root>/review-profiles/` when that folder exists, then each of
/// `overrides` in order. An override folder must exist; an empty one changes nothing.
pub fn load_all(
    defaults_dir: &Path,
    repo_root: &Path,
    overrides: &[PathBuf],
) -> Result<Vec<Profile>> {
    let mut layers = vec![load_dir(defaults_dir, Source::Default)?];
    let repo_dir = repo_root.join(REPO_PROFILES_DIR);
    if repo_dir.is_dir() {
        layers.push(load_dir(&repo_dir, Source::Repo)?);
    }
    for dir in overrides {
        layers.push(load_dir(dir, Source::Override)?);
    }
    Ok(merge(layers))
}

/// The override folders to use: `flag` when it names any, else the path list in `env`
/// (`$LENS_PROFILE_OVERRIDES`). Each is made absolute, since lens's scripts and agents run in
/// other folders, and must be a folder.
pub fn overrides(flag: Vec<PathBuf>, env: Option<&OsStr>) -> Result<Vec<PathBuf>> {
    let dirs = match (flag.is_empty(), env) {
        (false, _) => flag,
        (true, Some(list)) => std::env::split_paths(list)
            .filter(|p| !p.as_os_str().is_empty())
            .collect(),
        (true, None) => Vec::new(),
    };
    dirs.into_iter()
        .map(|dir| {
            let abs = dir
                .canonicalize()
                .with_context(|| format!("profile overrides folder {}", dir.display()))?;
            if !abs.is_dir() {
                bail!("profile overrides {} is not a folder", dir.display());
            }
            Ok(abs)
        })
        .collect()
}

/// The override folders `$LENS_PROFILE_OVERRIDES` names.
pub fn overrides_from_env() -> Result<Vec<PathBuf>> {
    overrides(Vec::new(), std::env::var_os(OVERRIDES_VAR).as_deref())
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
        let merged = merge([
            vec![named("a", Source::Default), named("b", Source::Default)],
            vec![named("b", Source::Repo), named("c", Source::Repo)],
        ]);
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

    fn write(dir: &Path, name: &str, description: &str) {
        let text = SAMPLE
            .replace("name: perf", &format!("name: {name}"))
            .replace("Speed.", description);
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("profile.md"), text).unwrap();
    }

    fn sources(profiles: &[Profile]) -> Vec<(&str, Source, &str)> {
        profiles
            .iter()
            .map(|p| (p.meta.name.as_str(), p.source, p.meta.description.as_str()))
            .collect()
    }

    #[test]
    fn overrides_win_over_repo_profiles_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let (defaults, repo, over) = (
            tmp.path().join("defaults"),
            tmp.path().join("repo"),
            tmp.path().join("over"),
        );
        write(&defaults, "a", "default a");
        write(&defaults, "b", "default b");
        write(&defaults, "c", "default c");
        write(&repo.join(REPO_PROFILES_DIR), "b", "repo b");
        write(&repo.join(REPO_PROFILES_DIR), "c", "repo c");
        write(&over, "c", "override c");
        write(&over, "d", "override d");
        let all = load_all(&defaults, &repo, &[over]).unwrap();
        assert_eq!(
            sources(&all),
            vec![
                ("a", Source::Default, "default a"),
                ("b", Source::Repo, "repo b"),
                ("c", Source::Override, "override c"),
                ("d", Source::Override, "override d"),
            ]
        );
    }

    #[test]
    fn later_override_folders_win_a_name_clash() {
        let tmp = tempfile::tempdir().unwrap();
        let (defaults, first, second) = (
            tmp.path().join("defaults"),
            tmp.path().join("first"),
            tmp.path().join("second"),
        );
        write(&defaults, "a", "default a");
        write(&first, "a", "first a");
        write(&second, "a", "second a");
        let all = load_all(&defaults, tmp.path(), &[first, second]).unwrap();
        assert_eq!(sources(&all), vec![("a", Source::Override, "second a")]);
    }

    #[test]
    fn an_empty_override_folder_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (defaults, empty) = (tmp.path().join("defaults"), tmp.path().join("empty"));
        write(&defaults, "a", "default a");
        fs::create_dir_all(&empty).unwrap();
        let all = load_all(&defaults, tmp.path(), &[empty]).unwrap();
        assert_eq!(sources(&all), vec![("a", Source::Default, "default a")]);
    }

    #[test]
    fn a_missing_override_folder_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let err = overrides(vec![missing.clone()], None).unwrap_err();
        assert!(format!("{err:#}").contains(&missing.display().to_string()));
        let list = std::env::join_paths([&missing]).unwrap();
        assert!(overrides(Vec::new(), Some(&list)).is_err());
    }

    #[test]
    fn a_file_is_not_an_override_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(overrides(vec![file], None).is_err());
    }

    #[test]
    fn the_flag_replaces_the_env_list() {
        let tmp = tempfile::tempdir().unwrap();
        let (flag, env) = (tmp.path().join("flag"), tmp.path().join("env"));
        fs::create_dir_all(&flag).unwrap();
        fs::create_dir_all(&env).unwrap();
        let list = std::env::join_paths([&env]).unwrap();
        let got = overrides(vec![flag.clone()], Some(&list)).unwrap();
        assert_eq!(got, vec![flag.canonicalize().unwrap()]);
        let got = overrides(Vec::new(), Some(&list)).unwrap();
        assert_eq!(got, vec![env.canonicalize().unwrap()]);
    }

    #[test]
    fn the_env_list_keeps_its_order_and_skips_empty_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let list = std::env::join_paths([b.as_path(), Path::new(""), a.as_path()]).unwrap();
        let got = overrides(Vec::new(), Some(&list)).unwrap();
        assert_eq!(
            got,
            vec![b.canonicalize().unwrap(), a.canonicalize().unwrap()]
        );
        assert!(overrides(Vec::new(), Some(OsStr::new("")))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn shipped_profiles_all_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles");
        let profiles = load_dir(&dir, Source::Default).unwrap();
        assert_eq!(profiles.len(), 15);
    }
}
