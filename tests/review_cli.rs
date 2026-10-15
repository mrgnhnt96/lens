//! `lens review` end to end, against a throwaway repo and a fake `claude` whose replies are canned:
//! what's under test is the pass around the agents (tiers, deltas, verification, the report and
//! the fix loop) and how each agent is started.

use std::fs;
use std::path::{Path, PathBuf};
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

/// A fake `claude -p`: logs how it was started and answers by role (the system prompt file's
/// name) and prompt. The correctness reviewer raises a blocker and a minor in src/a.rs and a
/// major in src/b.rs; the verifier confirms the major, and the blocker until the fixer has marked
/// src/a.rs FIXED. The reviewer whose profile `$FAKE_FAIL` names replies with no JSON. The PR
/// pass's lead names one focus area.
const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
prompt = sys.stdin.read()
system = args[args.index("--system-prompt-file") + 1]
role = os.path.basename(system).split(".")[0]
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps({"role": role, "args": args, "ttl": os.environ.get("CLAUDE_CODE_PROMPT_CACHE_TTL"),
                          "prompt": prompt, "system": open(system).read()}) + "\n")
code = open("src/a.rs").read()
fail = os.environ.get("FAKE_FAIL")
if role == "reviewer" and fail and f'name="{fail}"' in prompt:
    reply = None
elif role == "reviewer":
    findings = []
    if 're-review of a fix' not in prompt and 'name="correctness"' in prompt:
        findings = [
            {"file": "src/a.rs", "line": 3, "severity": "blocker", "title": "empty input accepted",
             "failureScenario": "parse(\"\") returns Ok", "confidence": 0.9},
            {"file": "src/a.rs", "line": 9, "severity": "minor", "title": "odd name",
             "failureScenario": "confusing", "confidence": 0.5},
        ]
        # A delta review looks only at the delta, which never touches src/b.rs.
        if "(delta.patch)" not in open(system).read():
            findings.append({"file": "src/b.rs", "line": 1, "severity": "major", "title": "leaks a handle",
                             "failureScenario": "b() never closes it", "confidence": 0.7})
    reply = {"summary": "checked", "findings": findings}
    if "You are the lead reviewer" in prompt:
        reply.update({"risk": "moderate", "assessment": "**Impact**: small",
                      "focusAreas": [{"title": "parsing", "files": ["src/a.rs"], "why": "input"},
                                     {"title": "beyond the cap", "files": ["src/b.rs"], "why": "x"}]})
elif role == "verifier":
    if "src/b.rs" in prompt:
        reply = {"verdict": "confirmed", "severity": "major", "howItFails": "1. b() opens it"}
    elif "FIXED" in code:
        reply = {"verdict": "dropped", "reason": "guarded now"}
    else:
        reply = {"verdict": "confirmed", "severity": "blocker", "howItFails": "1. parse(\"\")\n2. Ok"}
elif role == "fixer":
    open("src/a.rs", "a").write("// FIXED\n")
    reply = {"summary": "reject empty input", "files": ["src/a.rs"], "tests": "none", "needsDecision": None}
else:
    reply = {"decision": "escalate", "needs": "other", "reason": "stuck"}
text = "I couldn't finish." if reply is None else "Done.\n```json\n" + json.dumps(reply) + "\n```\n"
print(json.dumps({"type": "system", "subtype": "init"}))
print(json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}))
print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": text,
                  "total_cost_usd": 0.01, "num_turns": 1}))
"#;

struct Fixture {
    dir: tempfile::TempDir,
    fake: PathBuf,
    log: PathBuf,
}

impl Fixture {
    /// A repo on `feature` with `lines` lines added to src/a.rs since main.
    fn new(lines: usize) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("repo");
        fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q", "-b", "main"]);
        write(&d, "src/a.rs", "pub fn a() {}\n");
        write(&d, "README.md", "# a\n");
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "init"]);
        git(&d, &["switch", "-qc", "feature"]);
        let body: String = (0..lines).map(|i| format!("// line {i}\n")).collect();
        write(&d, "src/a.rs", &format!("pub fn a() {{}}\n{body}"));
        write(&d, "src/b.rs", "pub fn b() {}\n");
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "change"]);
        let fake = dir.path().join("claude");
        fs::write(&fake, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.path().join("calls.jsonl");
        Fixture { dir, fake, log }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn review(&self, args: &[&str]) -> String {
        self.review_with(&["main"], args, &[])
    }

    fn review_with(&self, base: &[&str], args: &[&str], env: &[(&str, &Path)]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_lens"))
            .arg("review")
            .args(base)
            .args(args)
            .envs(env.iter().copied())
            .current_dir(self.repo())
            // The files built into lens, written under this test's own cache.
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("LENS_CLAUDE", &self.fake)
            .env("FAKE_LOG", &self.log)
            // Keep tb out of it: no task context.
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "lens review: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn report(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.repo().join(".review/feature.json")).unwrap())
            .unwrap()
    }

    /// Every agent started so far, and clears the log.
    fn calls(&self) -> Vec<Value> {
        let calls = fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let _ = fs::remove_file(&self.log);
        calls
    }
}

fn roles(calls: &[Value]) -> Vec<String> {
    let mut r: Vec<String> = calls
        .iter()
        .map(|c| c["role"].as_str().unwrap().to_string())
        .collect();
    r.sort();
    r
}

#[test]
fn a_full_review_verifies_blockers_and_drops_the_rest() {
    let fx = Fixture::new(60);
    let out = fx.review(&["--no-fix"]);
    let report = fx.report();
    assert_eq!(report["mode"], "full");
    assert_eq!(report["tier"], "full");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[1]["id"], "F2");
    assert_eq!(findings[1]["file"], "src/b.rs");
    assert_eq!(findings[0]["id"], "F1");
    assert_eq!(findings[0]["severity"], "blocker");
    assert_eq!(findings[0]["howItFails"], "1. parse(\"\")\n2. Ok");
    assert_eq!(report["dropped"][0]["reason"], "below major");
    let ran: Vec<&str> = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["ran"] == true)
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(ran, ["correctness", "testing"]);
    assert!(report["tree"].as_str().is_some());
    assert!(
        out.contains("F1  blocker  src/a.rs:3  empty input accepted"),
        "{out}"
    );

    let calls = fx.calls();
    assert_eq!(
        roles(&calls),
        ["reviewer", "reviewer", "verifier", "verifier"]
    );
    for c in &calls {
        let args: Vec<&str> = c["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let at = args.iter().position(|a| *a == "--setting-sources").unwrap();
        assert_eq!(args[at + 1], "", "no user or project settings");
        assert!(args.contains(&"--strict-mcp-config"));
        assert_eq!(c["ttl"], "5m");
        // The diff is in the shared system prompt, not something each agent reads itself.
        assert!(c["system"].as_str().unwrap().contains("+// line 59"));
    }
    assert_eq!(report["agents"], 4);
    assert_eq!(report["costUsd"], 0.04);
}

#[test]
fn an_unchanged_tree_isnt_reviewed_again_and_a_change_reviews_only_the_delta() {
    let fx = Fixture::new(60);
    fx.review(&["--no-fix"]);
    fx.calls();

    let out = fx.review(&["--no-fix"]);
    assert!(out.contains("unchanged since the last review"), "{out}");
    assert!(fx.calls().is_empty());

    write(
        &fx.repo(),
        "src/a.rs",
        &(fs::read_to_string(fx.repo().join("src/a.rs")).unwrap() + "// one more\n"),
    );
    fx.review(&["--no-fix"]);
    let report = fx.report();
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["tier"], "small");
    assert_eq!(report["delta"]["changedLines"], 1);
    let calls = fx.calls();
    // Correctness only. The old blocker is verified again, merged with the one raised again; the
    // major in src/b.rs, which the delta didn't touch, carries over without a verifier.
    assert_eq!(roles(&calls), ["reviewer", "verifier"]);
    assert!(calls[0]["system"]
        .as_str()
        .unwrap()
        .contains("+// one more"));
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[1]["title"], "leaks a handle");
    assert!(fx.repo().join(".review/feature/prev/report.json").is_file());

    // --full reviews everything, even an unchanged tree.
    fx.review(&["--no-fix", "--full"]);
    assert_eq!(fx.report()["mode"], "full");
    assert_eq!(
        roles(&fx.calls())
            .iter()
            .filter(|r| *r == "reviewer")
            .count(),
        2
    );
}

#[test]
fn a_reviewer_that_failed_makes_the_next_pass_review_everything() {
    let fx = Fixture::new(60);
    let out = fx.review_with(
        &["main"],
        &["--no-fix"],
        &[("FAKE_FAIL", Path::new("testing"))],
    );
    assert!(
        out.contains("Failed (no readable reply): testing."),
        "{out}"
    );
    assert_eq!(fx.report()["incomplete"], serde_json::json!(["testing"]));
    fx.calls();

    fx.review(&["--no-fix"]);
    let report = fx.report();
    assert_eq!(report["mode"], "full");
    assert!(report.get("incomplete").is_none());
    assert_eq!(
        roles(&fx.calls())
            .iter()
            .filter(|r| *r == "reviewer")
            .count(),
        2
    );
}

#[test]
fn a_docs_only_change_runs_no_agents() {
    let fx = Fixture::new(1);
    git(&fx.repo(), &["reset", "-q", "--hard", "main"]);
    write(&fx.repo(), "README.md", "# a\n\nMore words.\n");
    fx.review(&[]);
    let report = fx.report();
    assert_eq!(report["tier"], "skip");
    assert!(report["findings"].as_array().unwrap().is_empty());
    assert!(fx.calls().is_empty());
}

#[test]
fn the_fix_loop_fixes_a_blocker_and_marks_it_resolved() {
    let fx = Fixture::new(60);
    let out = fx.review(&[]);
    let report = fx.report();
    let f1 = &report["findings"][0];
    assert_eq!(f1["fix"]["status"], "resolved", "{out}");
    assert!(fs::read_to_string(fx.repo().join("src/a.rs"))
        .unwrap()
        .contains("FIXED"));
    let roles = roles(&fx.calls());
    assert_eq!(roles.iter().filter(|r| *r == "fixer").count(), 1);
    assert!(!roles.contains(&"escalator".to_string()));
    assert!(out.contains("fixed by the fix loop"), "{out}");

    // The fix changed the tree after the review, but the ledger's tree is the reviewed one.
    let again = fx.review(&[]);
    assert!(again.contains("unchanged since the last review"), "{again}");
}

/// A `gh` stand-in for the PR pass: `pr view` answers from a canned PR, `api` lists no earlier
/// comments or reviews, and a posted review is saved to `$FAKE/posted.json`.
const FAKE_GH: &str = r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
fake = os.environ["FAKE"]
pr = json.load(open(os.path.join(fake, "pr.json")))
if args[:2] == ["pr", "view"]:
    if "--jq" in args:
        print(pr[args[args.index("--jq") + 1].lstrip(".")])
    else:
        print(json.dumps(pr))
elif args[0] == "api" and "POST" in args:
    open(os.path.join(fake, "posted.json"), "w").write(sys.stdin.read())
    print(json.dumps({"html_url": "https://github.test/pr/7#review-1"}))
elif args[0] == "api":
    print("[]")
else:
    sys.exit("unexpected: " + " ".join(args))
"#;

#[test]
fn the_pr_pass_runs_the_lead_and_its_focus_areas_and_posts() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let origin = fx.dir.path().join("origin.git");
    git(
        fx.dir.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(&d, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(
        &d,
        &["push", "-q", "origin", "main", "feature:refs/pull/7/head"],
    );
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let fake = fx.dir.path().join("ghfake");
    fs::create_dir_all(&fake).unwrap();
    fs::write(
        fake.join("pr.json"),
        serde_json::json!({
            "number": 7, "url": "https://github.test/pr/7", "title": "Change a", "body": "Adds lines.",
            "baseRefName": "main", "headRefName": "feature", "headRefOid": head.trim(),
        })
        .to_string(),
    )
    .unwrap();
    let gh = fake.join("gh");
    fs::write(&gh, FAKE_GH).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = fx.review_with(&[], &["--pr", "7"], &[("LENS_GH", &gh), ("FAKE", &fake)]);
    let report: Value =
        serde_json::from_str(&fs::read_to_string(d.join(".review/pr-7.json")).unwrap()).unwrap();
    assert_eq!(report["pass"], "pr");
    assert_eq!(report["risk"], "moderate");
    assert_eq!(report["assessment"], "**Impact**: small");
    assert_eq!(
        fs::read_to_string(d.join(".review/pr-7/impact.md")).unwrap(),
        "**Impact**: small"
    );
    let names: Vec<&str> = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["ran"] == true)
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "impact-assessment");
    // The lead named two focus areas; impact-assessment's maxMinions caps how many get a reviewer.
    let cap = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles/impact-assessment/profile.md"),
    )
    .unwrap()
    .lines()
    .find_map(|l| {
        l.strip_prefix("maxMinions:")
            .map(|n| n.trim().parse::<usize>().unwrap())
    })
    .unwrap_or(0);
    let focus = names.iter().filter(|n| n.starts_with("focus: ")).count();
    assert_eq!(focus, cap.min(2));
    if cap >= 1 {
        assert!(names.contains(&"focus: parsing"));
    }
    // Every severity is verified and kept on the PR pass, minor included.
    let severities: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["severity"].as_str().unwrap())
        .collect();
    assert!(
        severities.contains(&"blocker") && severities.contains(&"major"),
        "{severities:?}"
    );
    let posted: Value =
        serde_json::from_str(&fs::read_to_string(fake.join("posted.json")).unwrap()).unwrap();
    assert!(!posted["comments"].as_array().unwrap().is_empty());
    assert!(
        out.contains("Posted: https://github.test/pr/7#review-1"),
        "{out}"
    );
    let roles = roles(&fx.calls());
    assert!(!roles.contains(&"fixer".to_string()));
}
