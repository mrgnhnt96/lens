//! `lens sweep` end to end: a repo with a bare origin, a fake `claude` whose replies are canned
//! and a fake `gh`. What's under test is the sweep around the agents: the watermark, the gate,
//! areas, dedup against earlier sweeps, the fix commits and the PR.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A fake `claude -p` that answers by role. Reviewers raise a blocker and a minor in src/a.rs and
/// a major in src/b.rs, each only when its file is in the change block. The lead splits the range
/// into one area per file. The verifier confirms the major, and the blocker until the fixer has
/// marked src/a.rs FIXED. The reviewer whose profile `$FAKE_FAIL` names replies with no JSON.
const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, re, sys
args = sys.argv[1:]
prompt = sys.stdin.read()
system = open(args[args.index("--system-prompt-file") + 1]).read()
role = re.match(r'<role name="(\w+)">', prompt).group(1)
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps({"role": role, "prompt": prompt, "system": system}) + "\n")
code = open("src/a.rs").read()
fail = os.environ.get("FAKE_FAIL")
if role == "reviewer" and fail and f'name="{fail}"' in prompt:
    reply = None
elif role == "reviewer":
    findings = []
    if 're-review of a fix' not in prompt and 'name="correctness"' in prompt:
        if "src/a.rs" in system:
            findings += [
                {"file": "src/a.rs", "line": 3, "severity": "blocker", "title": "empty input accepted",
                 "failureScenario": "parse(\"\") returns Ok", "confidence": 0.9},
                {"file": "src/a.rs", "line": 9, "severity": "minor", "title": "odd name",
                 "failureScenario": "confusing", "confidence": 0.5},
            ]
        if "src/b.rs" in system:
            findings.append({"file": "src/b.rs", "line": 1, "severity": "major", "title": "leaks a handle",
                             "failureScenario": "b() never closes it", "confidence": 0.7})
    reply = {"summary": "checked", "findings": findings}
    if "You are the lead reviewer" in prompt:
        reply.update({"risk": "moderate", "assessment": "**Impact**: small",
                      "areas": [{"title": "parsing", "files": ["src/a.rs"], "why": "input"},
                                {"title": "handles", "files": ["src/b.rs"], "why": "resources"}]})
elif role == "verifier":
    if "src/b.rs" in prompt:
        reply = {"verdict": "confirmed", "severity": "major", "howItFails": "1. b() opens it"}
    elif "FIXED" in code:
        reply = {"verdict": "dropped", "reason": "guarded now"}
    else:
        reply = {"verdict": "confirmed", "severity": "blocker", "howItFails": "1. parse(\"\")\n2. Ok"}
elif role == "fixer":
    open("src/a.rs", "a").write("// FIXED\n")
    reply = {"summary": "reject empty input", "files": ["src/a.rs"], "tests": "cargo test parse",
             "needsDecision": None}
else:
    reply = {"decision": "escalate", "needs": "other", "reason": "stuck"}
text = "I couldn't finish." if reply is None else "Done.\n```json\n" + json.dumps(reply) + "\n```\n"
print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": text,
                  "total_cost_usd": 0.01, "num_turns": 1}))
"#;

/// A fake `gh`: logs its args and prints a PR URL.
const GH: &str = r#"#!/bin/sh
echo "$@" >> "$FAKE_GH_LOG"
echo "https://github.com/o/r/pull/7"
"#;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// A repo whose origin/main has one commit.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let d = dir.path().join("repo");
        fs::create_dir_all(&d).unwrap();
        git(
            dir.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(&d, &["init", "-q", "-b", "main"]);
        git(&d, &["remote", "add", "origin", origin.to_str().unwrap()]);
        write(&d, "src/a.rs", "pub fn a() {}\n");
        write(&d, "README.md", "# a\n");
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "init"]);
        git(&d, &["push", "-q", "origin", "main"]);
        for (name, text) in [("claude", FAKE), ("gh", GH)] {
            let path = dir.path().join(name);
            fs::write(&path, text).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        Fixture { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// Commits `files` on main and pushes them to origin.
    fn push(&self, files: &[(&str, &str)]) {
        for (path, text) in files {
            write(&self.repo(), path, text);
        }
        git(&self.repo(), &["add", "."]);
        git(&self.repo(), &["commit", "-qm", "change"]);
        git(&self.repo(), &["push", "-q", "origin", "main"]);
    }

    fn sweep(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_lens"))
            .arg("sweep")
            .args(args)
            .envs(env.iter().copied())
            .current_dir(self.repo())
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("LENS_CLAUDE", self.dir.path().join("claude"))
            .env("LENS_GH", self.dir.path().join("gh"))
            .env("FAKE_LOG", self.dir.path().join("calls.jsonl"))
            .env("FAKE_GH_LOG", self.dir.path().join("gh.log"))
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "lens sweep: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn watermark(&self) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["rev-parse", "--verify", "--quiet", "refs/lens/swept"])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_string())
    }

    fn origin_main(&self) -> String {
        git(&self.repo(), &["rev-parse", "origin/main"])
            .trim()
            .to_string()
    }

    /// The sweep reports, oldest first.
    fn reports(&self) -> Vec<Value> {
        let mut paths: Vec<PathBuf> = fs::read_dir(self.repo().join(".review"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort_by_key(|p| fs::metadata(p).unwrap().modified().unwrap());
        paths
            .iter()
            .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
            .collect()
    }
}

#[test]
fn the_first_sweep_sets_the_watermark_and_an_empty_range_does_nothing() {
    let fx = Fixture::new();
    let out = fx.sweep(&["--dry-run"], &[]);
    assert!(out.contains("no sweep has run here yet"), "{out}");
    assert_eq!(fx.watermark(), None);

    let out = fx.sweep(&[], &[]);
    assert!(out.contains("first sweep"), "{out}");
    assert_eq!(fx.watermark(), Some(fx.origin_main()));

    let out = fx.sweep(&["--now"], &[]);
    assert!(out.contains("nothing new"), "{out}");
    assert!(!fx.dir.path().join("calls.jsonl").exists());
}

#[test]
fn a_sweep_fixes_blockers_on_its_branch_opens_one_pr_and_moves_the_watermark() {
    let fx = Fixture::new();
    fx.sweep(&[], &[]);
    fx.push(&[
        ("src/a.rs", "pub fn a() {}\npub fn parse() {}\n"),
        ("src/b.rs", "pub fn b() {}\n"),
    ]);
    let out = fx.sweep(&[], &[]);
    assert!(
        out.contains("Blockers: 1 (1 fixed), majors: 1, minors: 1"),
        "{out}"
    );
    assert!(out.contains("PR: https://github.com/o/r/pull/7"), "{out}");
    assert_eq!(fx.watermark(), Some(fx.origin_main()));

    let report = &fx.reports()[0];
    assert_eq!(report["pass"], "sweep");
    assert_eq!(report["commits"], 1);
    let areas: Vec<&str> = report["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(areas, ["parsing", "handles"]);
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0]["severity"], "blocker");
    assert_eq!(findings[0]["area"], 1);
    assert_eq!(findings[0]["fix"]["status"], "resolved");
    assert!(findings[0]["commit"].is_string());
    assert_eq!(findings[1]["file"], "src/b.rs");
    assert_eq!(report["minors"][0]["id"], "M1");
    assert_eq!(report["watermark"]["moved"], true);

    // Each area's reviewers read their own change block, holding only that area's diff.
    let calls: Vec<Value> = fs::read_to_string(fx.dir.path().join("calls.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let area_reviewer = calls
        .iter()
        .find(|c| {
            c["role"] == "reviewer"
                && c["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("This area (2/2): handles")
        })
        .unwrap();
    let system = area_reviewer["system"].as_str().unwrap();
    assert!(system.contains("src/b.rs") && !system.contains("+pub fn parse"));

    // One commit per fix on the sweep's branch, pushed, and the PR body is the digest.
    let branch = report["branch"].as_str().unwrap();
    let log = git(
        &fx.repo(),
        &[
            "log",
            "--format=%B",
            &format!("origin/main..origin/{branch}"),
        ],
    );
    assert!(log.starts_with("fix: empty input accepted"), "{log}");
    assert!(log.contains("Tests: cargo test parse"), "{log}");
    let gh = fs::read_to_string(fx.dir.path().join("gh.log")).unwrap();
    assert!(
        gh.contains(&format!("pr create --base main --head {branch}")),
        "{gh}"
    );
    let digest_path = gh.split("--body-file ").nth(1).unwrap().trim();
    let digest = fs::read_to_string(digest_path).unwrap();
    assert!(digest.contains("### Fixed"), "{digest}");
    assert!(digest.contains("**M1** `src/a.rs:9` odd name"), "{digest}");
    // The worktree is gone; the branch stays for the PR.
    assert!(!fx
        .repo()
        .join(report["worktree"].as_str().unwrap())
        .exists());
}

#[test]
fn the_next_sweep_waits_and_skips_what_an_earlier_one_reported() {
    let fx = Fixture::new();
    fx.sweep(&[], &[]);
    fx.push(&[("src/a.rs", "pub fn a() {}\npub fn parse() {}\n")]);
    fx.sweep(&[], &[]);
    fx.push(&[(
        "src/a.rs",
        "pub fn a() {}\npub fn parse() {}\npub fn more() {}\n",
    )]);

    let out = fx.sweep(&[], &[]);
    assert!(out.contains("sweep deferred"), "{out}");

    let out = fx.sweep(&["--now"], &[]);
    assert!(out.contains("Blockers: 0 (0 fixed)"), "{out}");
    assert!(out.contains("2 skipped as already reported"), "{out}");
    let report = fx.reports().pop().unwrap();
    assert!(report["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .starts_with("reported by the sweep of"));
}

#[test]
fn a_failed_reviewer_or_a_dry_run_leaves_the_watermark() {
    let fx = Fixture::new();
    fx.sweep(&[], &[]);
    let start = fx.watermark();
    fx.push(&[("src/b.rs", "pub fn b() {}\n")]);

    let out = fx.sweep(&["--dry-run"], &[]);
    assert!(out.contains("left where it was (dry run)"), "{out}");
    assert_eq!(fx.watermark(), start);
    assert!(!fx.dir.path().join("gh.log").exists());

    let out = fx.sweep(&["--now"], &[("FAKE_FAIL", "correctness")]);
    assert!(
        out.contains("Watermark: not moved; 1 reviewer(s) failed"),
        "{out}"
    );
    assert_eq!(fx.watermark(), start);
}
