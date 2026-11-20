//! `lens profiles --base` end to end, against a throwaway git repo and the shipped profiles.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A repo with one commit on `main`, then a branch.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    write(dir.path(), "README.md", "hi\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    git(dir.path(), &["switch", "-qc", "feature"]);
    dir
}

fn lens_profiles(repo: &Path) -> Value {
    lens_profiles_with(repo, &[], None)
}

/// `lens profiles` with extra args and `$LENS_PROFILE_OVERRIDES` set to `overrides`.
fn lens_profiles_with(repo: &Path, args: &[&str], overrides: Option<&Path>) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lens"));
    cmd.env_remove("LENS_PROFILE_OVERRIDES");
    if let Some(dir) = overrides {
        cmd.env("LENS_PROFILE_OVERRIDES", dir);
    }
    let out = cmd
        .args(["profiles", "--base", "main", "--repo"])
        .arg(repo)
        .args(args)
        .env(
            "LENS_PROFILES_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles"),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn picked(v: &Value) -> Vec<(String, String)> {
    v["picked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().to_string(),
                p["decision"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn decision_of(v: &Value, name: &str) -> Option<String> {
    picked(v)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| d)
}

#[test]
fn rust_change_skips_dart_profiles_and_sees_committed_and_untracked_files() {
    let dir = repo();
    write(dir.path(), "src/auth/token.rs", "fn a() {}\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "auth"]);
    write(dir.path(), "src/new.rs", "fn b() {}\n");

    let v = lens_profiles(dir.path());
    assert_eq!(
        v["changedFiles"],
        serde_json::json!(["src/auth/token.rs", "src/new.rs"])
    );
    assert_eq!(v["languages"], serde_json::json!(["rust"]));
    assert_eq!(decision_of(&v, "correctness").as_deref(), Some("selected"));
    assert_eq!(decision_of(&v, "security").as_deref(), Some("selected"));
    assert_eq!(decision_of(&v, "architecture").as_deref(), Some("infer"));
    for dart in [
        "dart-style",
        "flutter-architecture",
        "flutter-widgets",
        "flutter-localization",
    ] {
        assert_eq!(decision_of(&v, dart), None, "{dart} should be skipped");
    }
}

#[test]
fn dart_change_picks_dart_profiles() {
    let dir = repo();
    write(dir.path(), "lib/l10n/app_en.arb", "{}\n");
    write(dir.path(), "lib/view.dart", "void main() {}\n");

    let v = lens_profiles(dir.path());
    assert_eq!(decision_of(&v, "dart-style").as_deref(), Some("selected"));
    assert_eq!(
        decision_of(&v, "flutter-widgets").as_deref(),
        Some("selected")
    );
    assert_eq!(
        decision_of(&v, "flutter-architecture").as_deref(),
        Some("selected")
    );
    assert_eq!(
        decision_of(&v, "flutter-localization").as_deref(),
        Some("selected")
    );
}

#[test]
fn repo_profile_overrides_default_by_name() {
    let dir = repo();
    write(
        dir.path(),
        "review-profiles/correctness/profile.md",
        "---\nname: correctness\ndescription: Repo's own.\nmodel: haiku\neffort: low\n\
         severityGuidance: x\nenabled: false\n---\nBody\n",
    );
    write(
        dir.path(),
        "review-profiles/house/profile.md",
        "---\nname: house\ndescription: House rules.\nmodel: sonnet\neffort: high\n\
         severityGuidance: x\nenabled: true\n---\nBody\n",
    );

    let v = lens_profiles(dir.path());
    assert_eq!(decision_of(&v, "correctness"), None);
    let skipped = v["skipped"].as_array().unwrap();
    assert!(skipped
        .iter()
        .any(|s| s["name"] == "correctness" && s["reason"] == "disabled"));
    let house = v["picked"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "house")
        .expect("repo-only profile is added");
    assert_eq!(house["source"], "repo");
}

#[test]
fn unknown_base_is_an_error() {
    let dir = repo();
    let out = Command::new(env!("CARGO_BIN_EXE_lens"))
        .args(["profiles", "--base", "nope", "--repo"])
        .arg(dir.path())
        .env(
            "LENS_PROFILES_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles"),
        )
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("merge base"));
}

fn profile_text(name: &str, description: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nmodel: sonnet\neffort: high\n\
         severityGuidance: x\nenabled: true\n---\nBody\n"
    )
}

fn picked_entry<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["picked"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("{name} not picked: {v}"))
}

#[test]
fn an_out_of_repo_folder_wins_over_the_repos_own_and_the_flag_over_the_env() {
    let dir = repo();
    write(
        dir.path(),
        "review-profiles/house/profile.md",
        &profile_text("house", "Repo's."),
    );
    let outside = tempfile::tempdir().unwrap();
    let (env_dir, flag_dir) = (outside.path().join("env"), outside.path().join("flag"));
    write(
        &env_dir,
        "house/profile.md",
        &profile_text("house", "From the env."),
    );
    write(
        &env_dir,
        "mine/profile.md",
        &profile_text("mine", "Only in the env."),
    );
    write(
        &flag_dir,
        "house/profile.md",
        &profile_text("house", "From the flag."),
    );

    let v = lens_profiles(dir.path());
    assert_eq!(picked_entry(&v, "house")["source"], "repo");

    let v = lens_profiles_with(dir.path(), &[], Some(&env_dir));
    let house = picked_entry(&v, "house");
    assert_eq!(house["source"], "override");
    assert_eq!(house["description"], "From the env.");
    assert_eq!(picked_entry(&v, "mine")["source"], "override");
    // The shipped profiles are still there.
    assert_eq!(picked_entry(&v, "correctness")["source"], "default");

    let flag = flag_dir.to_str().unwrap();
    let v = lens_profiles_with(dir.path(), &["--profiles", flag], Some(&env_dir));
    assert_eq!(picked_entry(&v, "house")["description"], "From the flag.");
    assert!(
        decision_of(&v, "mine").is_none(),
        "the flag replaces the env list"
    );
}
