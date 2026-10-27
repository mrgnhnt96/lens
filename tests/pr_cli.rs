//! `lens pr earlier` and `lens pr post` end to end, against a fake `gh` (`$LENS_GH`).

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

/// A `gh` stand-in: serves `$FAKE/comments.json` and `$FAKE/reviews.json`, saves a posted review
/// to `$FAKE/posted.json` and the token it saw to `$FAKE/token`. With `$FAKE/own`, the PR is the
/// poster's own and GitHub refuses a verdict on it.
const FAKE_GH: &str = r#"#!/bin/sh
printf '%s' "${GH_TOKEN:-}" > "$FAKE/token"
case "$*" in
  *--method\ POST*)
    body=$(cat)
    if [ -f "$FAKE/own" ] && ! printf '%s' "$body" | grep -q '"event":"COMMENT"'; then
      echo 'gh: Unprocessable Entity (HTTP 422): Can not request changes on your own pull request' >&2
      exit 1
    fi
    printf '%s' "$body" > "$FAKE/posted.json"; echo '{"html_url":"https://github.test/pr/7#review-1"}' ;;
  *pulls/7/comments*) cat "$FAKE/comments.json" ;;
  *pulls/7/reviews*) cat "$FAKE/reviews.json" ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#;

const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {
+    new();
 }
";

fn setup(dir: &Path, comments: Value) {
    let gh = dir.join("gh");
    fs::write(&gh, FAKE_GH).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Two pages, the way `gh api --paginate` prints them.
    fs::write(dir.join("comments.json"), format!("{comments}[]")).unwrap();
    fs::write(dir.join("reviews.json"), "[]").unwrap();
    fs::write(dir.join("diff.patch"), PATCH).unwrap();
    let report = json!({
        "version": 1,
        "pass": "pr",
        "head": "0123456789abcdef",
        "base": "origin/main",
        "risk": "low",
        "profiles": [{"name": "correctness", "ran": true}],
        "findings": [
            {"id": "F1", "profiles": ["correctness"], "file": "src/a.rs", "line": 2,
             "severity": "major", "title": "new() panics", "failureScenario": "a() panics",
             "howItFails": "1. a() calls new()"},
            {"id": "F2", "profiles": ["naming-intuitivity"], "file": "src/b.rs", "line": 9,
             "severity": "minor", "title": "misleading name", "failureScenario": "reader thinks x"}
        ],
        "dropped": []
    });
    fs::write(dir.join("pr-7.json"), report.to_string()).unwrap();
}

fn lens(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lens"))
        .args(args)
        .args(["--repo", dir.to_str().unwrap()])
        .env("LENS_GH", dir.join("gh"))
        .env("FAKE", dir)
        .env_remove("LENS_BOT_TOKEN")
        .env_remove("GH_TOKEN")
        .output()
        .unwrap()
}

fn post(dir: &Path, extra: &[&str]) -> Value {
    let report = dir.join("pr-7.json");
    let diff = dir.join("diff.patch");
    let mut args = vec![
        "pr",
        "post",
        "--pr",
        "7",
        "--report",
        report.to_str().unwrap(),
        "--diff",
        diff.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let out = lens(dir, &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn posts_one_review_inline_where_it_can() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), json!([]));
    let out = post(dir.path(), &[]);
    assert_eq!(out["posted"], true);
    assert_eq!(out["postAs"], "owner");
    assert_eq!(out["review"], "https://github.test/pr/7#review-1");
    assert_eq!(out["inline"], json!(["F1"]));
    assert_eq!(out["inBody"], json!(["F2"]));

    let sent: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("posted.json")).unwrap()).unwrap();
    assert_eq!(sent["commit_id"], "0123456789abcdef");
    assert_eq!(sent["event"], "REQUEST_CHANGES");
    assert_eq!(sent["comments"].as_array().unwrap().len(), 1);
    assert_eq!(sent["comments"][0]["path"], "src/a.rs");
    assert!(sent["body"].as_str().unwrap().contains("`src/b.rs:9`"));
    assert_eq!(fs::read_to_string(dir.path().join("token")).unwrap(), "");
}

#[test]
fn skips_findings_an_earlier_run_posted() {
    let dir = tempfile::tempdir().unwrap();
    let key = lens::pr::key("src/a.rs", "new() panics");
    setup(
        dir.path(),
        json!([{"path": "src/a.rs", "line": 2, "html_url": "https://github.test/c/1",
                "body": format!("old\n\n<!-- lens:finding key={key} -->")}]),
    );
    let out = post(dir.path(), &["--dry-run"]);
    assert_eq!(out["posted"], false);
    assert_eq!(out["inline"], json!([]));
    assert_eq!(out["inBody"], json!(["F2"]));
    assert_eq!(out["skipped"][0]["id"], "F1");
    assert!(!dir.path().join("posted.json").exists());
    assert!(out["payload"]["body"]
        .as_str()
        .unwrap()
        .contains("1 of them already posted"));

    let out = lens(dir.path(), &["pr", "earlier", "--pr", "7"]);
    assert!(out.status.success());
    let earlier: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(earlier["findings"][0]["key"], key);
}

#[test]
fn bot_identity_uses_the_configured_token() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), json!([]));
    fs::write(
        dir.path().join(".lens.toml"),
        "[pr]\npost_as = \"bot\"\nbot_token_env = \"REVIEW_BOT_TOKEN\"\n",
    )
    .unwrap();

    let report = dir.path().join("pr-7.json");
    let diff = dir.path().join("diff.patch");
    let args = [
        "pr",
        "post",
        "--pr",
        "7",
        "--report",
        report.to_str().unwrap(),
        "--diff",
        diff.to_str().unwrap(),
    ];
    let out = lens(dir.path(), &args);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("$REVIEW_BOT_TOKEN"));

    let out = Command::new(env!("CARGO_BIN_EXE_lens"))
        .args(args)
        .args(["--repo", dir.path().to_str().unwrap()])
        .env("LENS_GH", dir.path().join("gh"))
        .env("FAKE", dir.path())
        .env("REVIEW_BOT_TOKEN", "bot-secret")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out["postAs"], "bot");
    assert_eq!(
        fs::read_to_string(dir.path().join("token")).unwrap(),
        "bot-secret"
    );
}

#[test]
fn on_the_posters_own_pr_findings_post_as_a_plain_review() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), json!([]));
    fs::write(dir.path().join("own"), "").unwrap();
    let out = post(dir.path(), &[]);
    assert_eq!(out["posted"], true);
    assert_eq!(out["event"], "REQUEST_CHANGES");
    let sent: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("posted.json")).unwrap()).unwrap();
    assert_eq!(sent["event"], "COMMENT");
    assert_eq!(sent["comments"].as_array().unwrap().len(), 1);
}

#[test]
fn a_clean_review_of_the_posters_own_pr_posts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), json!([]));
    let report = dir.path().join("pr-7.json");
    let mut r: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    r["findings"] = json!([]);
    fs::write(&report, r.to_string()).unwrap();
    fs::write(dir.path().join("own"), "").unwrap();
    let out = post(dir.path(), &[]);
    assert_eq!(out["posted"], false);
    assert_eq!(out["event"], "APPROVE");
    assert!(!dir.path().join("posted.json").exists());
}
