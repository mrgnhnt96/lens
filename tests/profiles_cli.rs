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
    let out = Command::new(env!("CARGO_BIN_EXE_lens"))
        .args(["profiles", "--base", "main", "--repo"])
        .arg(repo)
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
