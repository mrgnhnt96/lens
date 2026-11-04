//! `lens review` end to end, against a throwaway repo and a fake `claude` whose replies are canned:
//! what's under test is the pass around the agents (tiers, deltas, verification, the report, the
//! the pre-PR review protocol) and how each agent is started.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use lens::review::iso;
use serde_json::{json, Value};

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

/// A fake `claude -p`: logs how it was started and answers by role (named in its first message)
/// and prompt. It plays the prompt cache too: the first request with a given model, tools and
/// system prompt reports the system prompt's length as cache creation, later ones as cache read. The correctness reviewer raises a blocker and a minor in src/a.rs and a
/// major in src/b.rs; the verifier confirms both. The reviewer whose profile `$FAKE_FAIL` names replies with no JSON. The PR
/// pass's lead names one focus area. With `$FAKE_NIT` set it raises a nit in src/a.rs too. The plan turns away every agent of the role `$FAKE_REJECT`
/// names.
const FAKE: &str = r#"#!/usr/bin/env python3
import hashlib, json, os, re, sys, time
args = sys.argv[1:]
prompt = sys.stdin.read()
system = args[args.index("--system-prompt-file") + 1]
role = re.match(r'<role name="(\w+)">', prompt).group(1)
system_text = open(system).read()
key = hashlib.sha1("|".join([args[args.index("--model") + 1], args[args.index("--tools") + 1],
                             system_text]).encode()).hexdigest()
cached = os.environ["FAKE_LOG"] + ".cache." + key
if os.path.exists(cached):
    usage = {"cache_creation_input_tokens": 0, "cache_read_input_tokens": len(system_text)}
else:
    # Long enough for an agent started alongside to miss the cache too.
    time.sleep(0.3)
    open(cached, "w").close()
    usage = {"cache_creation_input_tokens": len(system_text), "cache_read_input_tokens": 0}
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps({"role": role, "args": args, "ttl": os.environ.get("CLAUDE_CODE_PROMPT_CACHE_TTL"),
                          "prompt": prompt, "system": system_text, "usage": usage}) + "\n")
if role == os.environ.get("FAKE_REJECT"):
    reset = int(time.time()) + 600
    window = {"utilization": 1.0, "resetsAt": reset, "status": "rejected"}
    print(json.dumps({"type": "rate_limit_event", "rate_limit_info": {
        "status": "rejected", "resetsAt": reset, "rateLimitType": "five_hour",
        "unifiedWindows": {"five_hour": window}}}))
    print(json.dumps({"type": "result", "is_error": True, "result": "limit reached",
                      "total_cost_usd": 0, "usage": {"input_tokens": 0, "output_tokens": 0}}))
    sys.exit(1)
code = open("src/a.rs").read()
fail = os.environ.get("FAKE_FAIL")
if role == "reviewer" and fail and f'name="{fail}"' in prompt:
    reply = None
elif role == "reviewer":
    findings = []
    if 're-review of a fix' not in prompt and 'name="correctness"' in prompt:
        findings = [
            {"file": "src/a.rs", "line": 3, "severity": "blocker", "title": "empty input accepted",
             "failureScenario": "parse(\"\") returns Ok", "confidence": 0.9,
             "suggestedFix": "reject empty input"},
            {"file": "src/a.rs", "line": 9, "severity": "minor", "title": "odd name",
             "failureScenario": "confusing", "confidence": 0.5},
        ]
        # A delta review looks only at the delta, which never touches src/b.rs.
        if os.environ.get("FAKE_NIT"):
            findings.append({"file": "src/a.rs", "line": 30, "severity": "nit", "title": "stray blank line",
                             "failureScenario": "none, it reads oddly", "confidence": 0.9})
        if "(delta.patch)" not in system_text:
            findings.append({"file": "src/b.rs", "line": 1, "severity": "major", "title": "leaks a handle",
                             "failureScenario": "b() never closes it", "confidence": 0.7,
                             "suggestedFix": "close it in b()"})
    reply = {"summary": "checked", "findings": findings}
    if "You are the lead reviewer" in prompt:
        reply.update({"risk": "moderate", "assessment": "**Impact**: small",
                      "focusAreas": [{"title": "parsing", "files": ["src/a.rs"], "why": "input"},
                                     {"title": "beyond the cap", "files": ["src/b.rs"], "why": "x"}]})
elif role == "verifier":
    if "src/b.rs" in prompt:
        reply = {"verdict": "confirmed", "severity": "major", "howItFails": "1. b() opens it",
                 "failureScenario": "b() never closes the handle it opens"}
    else:
        reply = {"verdict": "confirmed", "severity": "blocker", "howItFails": "1. parse(\"\")\n2. Ok"}
        if "suggestedFix: reject empty input" in prompt:
            reply["suggestedFix"] = "\n  return Err on an empty string in parse()\nmore"

else:
    reply = None
text = "I couldn't finish." if reply is None else "Done.\n```json\n" + json.dumps(reply) + "\n```\n"
print(json.dumps({"type": "system", "subtype": "init"}))
print(json.dumps({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": {
    "five_hour": {"utilization": 0.5, "resetsAt": 1793125800, "status": "allowed"},
    "seven_day": {"utilization": 0.25, "resetsAt": 1793466000, "status": "allowed"}}}}))
print(json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": text}], "usage": usage}}))
print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": text,
                  "total_cost_usd": 0.01, "num_turns": 1,
                  "modelUsage": {args[args.index("--model") + 1]: {
                      "inputTokens": 10, "outputTokens": 20, "costUSD": 0.01, **{
                          "cacheCreationInputTokens": usage["cache_creation_input_tokens"],
                          "cacheReadInputTokens": usage["cache_read_input_tokens"]}}}}))
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

    fn run(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lens"))
            .arg("review")
            .args(args)
            .envs(env.iter().copied())
            .current_dir(self.repo())
            // The files built into lens and the state of reviews run by hand, under this test's
            // own cache.
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("LENS_CLAUDE", &self.fake)
            .env("FAKE_LOG", &self.log)
            .env("LENS_USAGE_DIR", self.usage())
            // Keep tb out of it: no task context.
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .output()
            .unwrap()
    }

    fn review_with(&self, base: &[&str], args: &[&str], env: &[(&str, &Path)]) -> String {
        self.review_logged(base, args, env).0
    }

    /// What `lens review` printed, and what it logged as it went.
    fn review_logged(
        &self,
        base: &[&str],
        args: &[&str],
        env: &[(&str, &Path)],
    ) -> (String, String) {
        let args: Vec<&str> = base.iter().chain(args).copied().collect();
        let out = self.run(&args, env);
        assert!(
            out.status.success(),
            "lens review: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn usage(&self) -> PathBuf {
        self.dir.path().join("usage")
    }

    /// The lines of one of the usage logs.
    fn usage_log(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.usage().join(name))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Where a review run by hand keeps its state: the one repo folder under the cache.
    fn state(&self) -> PathBuf {
        let root = self.dir.path().join("cache/lens/state");
        let dirs: Vec<PathBuf> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        assert!(dirs[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("repo-"));
        dirs[0].clone()
    }

    fn report(&self) -> Value {
        read_json(&self.state().join("feature.json"))
    }

    /// One round of the review protocol, as the board runs it, with the state in `state/` and the
    /// result in `result-<round>.json`.
    fn protocol(&self, round: u32, env: &[(&str, &Path)]) -> Output {
        let d = self.dir.path();
        fs::write(d.join("intent.md"), "# Reject empty input\n").unwrap();
        let result = d.join(format!("result-{round}.json"));
        let _ = fs::remove_file(&result);
        let round = round.to_string();
        let vars: [(&str, &Path); 8] = [
            ("TASKBOARD_REVIEW_PROTOCOL", Path::new("1")),
            ("TASKBOARD_REPO", &self.repo()),
            ("TASKBOARD_BASE", Path::new("main")),
            ("TASKBOARD_BRANCH", Path::new("feature")),
            ("TASKBOARD_ROUND", Path::new(&round)),
            ("TASKBOARD_INTENT", &d.join("intent.md")),
            ("TASKBOARD_STATE_DIR", &d.join("state")),
            ("TASKBOARD_RESULT", &result),
        ];
        let env: Vec<(&str, &Path)> = vars.iter().chain(env).copied().collect();
        self.run(&[], &env)
    }

    fn result(&self, round: u32) -> Value {
        read_json(&self.dir.path().join(format!("result-{round}.json")))
    }

    /// Makes the usage log hold one reading: `window` is `utilization` used and resets
    /// `resets_in` seconds from now.
    fn read_window(&self, window: &str, utilization: f64, resets_in: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let line = json!({ "at": iso(now), "window": window, "utilization": utilization,
                           "resetsAt": iso(now + resets_in), "status": "allowed" });
        fs::create_dir_all(self.usage()).unwrap();
        fs::write(self.usage().join("readings.jsonl"), format!("{line}\n")).unwrap();
    }

    /// A bare origin holding PR 7, and the fake `gh` and its data directory that serve it.
    fn pr_remote(&self) -> (PathBuf, PathBuf) {
        let (fx, d) = (self, self.repo());
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
        (gh, fake)
    }

    /// Commits a fix to src/a.rs (with anything already staged) and pushes it as PR 7's new head.
    fn push_fix(&self, fake: &Path) {
        let d = self.repo();
        write(
            &d,
            "src/a.rs",
            &(fs::read_to_string(d.join("src/a.rs")).unwrap() + "// a fix\n"),
        );
        git(&d, &["commit", "-qam", "fix"]);
        self.push_head(fake);
    }

    /// Force-pushes `feature` as PR 7's head and points the fake `gh` at it.
    fn push_head(&self, fake: &Path) {
        let d = self.repo();
        git(
            &d,
            &["push", "-q", "-f", "origin", "feature:refs/pull/7/head"],
        );
        let mut pr: Value = read_json(&fake.join("pr.json"));
        pr["headRefOid"] = json!(self.rev("HEAD"));
        fs::write(fake.join("pr.json"), pr.to_string()).unwrap();
    }

    /// Commits `files` on main, then rebases `feature` onto it (the rebase must apply cleanly).
    fn move_main(&self, files: &[(&str, &str)]) {
        let d = self.repo();
        git(&d, &["switch", "-q", "main"]);
        for (path, text) in files {
            write(&d, path, text);
        }
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-qm", "main moves"]);
        git(&d, &["switch", "-q", "feature"]);
        git(&d, &["rebase", "-q", "main"]);
    }

    /// Pushes main and `feature` as the base and head of PR 7.
    fn push_rebase(&self, fake: &Path) {
        git(&self.repo(), &["push", "-q", "origin", "main"]);
        self.push_head(fake);
    }

    /// Cuts child PR `n` from `feature`, adding `file`, and merges it back into `feature` (squashed
    /// or with a merge commit), listing it as merged in the fake `gh`. With `findings`, lens
    /// reviewed the child on its own PR and raised them.
    fn merge_child(&self, fake: &Path, n: u64, file: &str, squash: bool, findings: Option<Value>) {
        let d = self.repo();
        let base = self.rev("feature");
        let branch = format!("child-{n}");
        git(&d, &["switch", "-q", "-c", &branch]);
        write(&d, file, "pub fn c() {}\n");
        git(&d, &["add", file]);
        git(&d, &["commit", "-qm", &format!("child {n}")]);
        let head = self.rev("HEAD");
        git(&d, &["switch", "-q", "feature"]);
        if squash {
            git(&d, &["merge", "-q", "--squash", &branch]);
            git(&d, &["commit", "-qm", &format!("child {n} (#{n})")]);
        } else {
            git(
                &d,
                &[
                    "merge",
                    "-q",
                    "--no-ff",
                    "-m",
                    &format!("merge #{n}"),
                    &branch,
                ],
            );
        }
        if let Some(findings) = findings {
            let report =
                json!({"pass": "pr", "mergeBase": base, "head": head, "findings": findings});
            fs::write(
                self.state().join(format!("pr-{n}.json")),
                report.to_string(),
            )
            .unwrap();
        }
        let path = fake.join("children.json");
        let mut children: Vec<Value> = fs::read_to_string(&path)
            .map(|t| serde_json::from_str(&t).unwrap())
            .unwrap_or_default();
        children.push(json!({
            "number": n, "html_url": format!("https://github.test/pr/{n}"), "state": "closed",
            "merged_at": "2026-11-01T00:00:00Z", "merge_commit_sha": self.rev("HEAD"),
        }));
        fs::write(path, Value::from(children).to_string()).unwrap();
    }

    fn rev(&self, rev: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
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

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// `git status`, to show a review left the checkout as it was.
fn status(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--ignored"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
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
    let out = fx.review(&[]);
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
    // The verifier's rewrite of a scenario replaces the reviewer's; without one it stays.
    assert_eq!(findings[0]["failureScenario"], "parse(\"\") returns Ok");
    assert_eq!(
        findings[1]["failureScenario"],
        "b() never closes the handle it opens"
    );
    // The verifier sees the reviewer's fix and its first line is the one kept; a fix the
    // verifier doesn't give back isn't kept.
    assert_eq!(
        findings[0]["suggestedFix"],
        "return Err on an empty string in parse()"
    );
    assert!(findings[1].get("suggestedFix").is_none(), "{}", findings[1]);
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
    assert!(
        out.contains("  fix: return Err on an empty string in parse()"),
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
        // The diff is in the shared system prompt, not something each agent reads itself; the
        // role's instructions are in the first message.
        assert!(c["system"].as_str().unwrap().contains("+// line 59"));
        assert_eq!(
            c["system"], calls[0]["system"],
            "one system prompt for every agent"
        );
        assert!(!c["system"].as_str().unwrap().contains("<role"));
    }
    assert_eq!(report["totals"]["agents"], 4);
    assert_eq!(report["costUsd"], 0.04);
    assert_eq!(report["totals"]["costUsd"], 0.04);

    // One agent per model and tool set writes the change to the cache; every other agent of that
    // group reads it, the verifiers included, though their role differs from the reviewer's.
    let agents = report["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 4);
    let mut groups: Vec<(String, String)> = agents
        .iter()
        .map(|a| {
            (
                a["model"].as_str().unwrap().into(),
                a["tools"].as_str().unwrap().into(),
            )
        })
        .collect();
    groups.sort();
    groups.dedup();
    let change = calls[0]["system"].as_str().unwrap().len() as u64;
    for (model, tools) in &groups {
        let group: Vec<&Value> = agents
            .iter()
            .filter(|a| a["model"] == *model.as_str() && a["tools"] == *tools.as_str())
            .collect();
        let writes = group
            .iter()
            .filter(|a| a["cacheCreationInputTokens"] == change)
            .count();
        assert_eq!(writes, 1, "{model} {tools}: {group:?}");
        for a in &group {
            if a["cacheCreationInputTokens"] == 0 {
                assert_eq!(a["cacheReadInputTokens"], change, "{a}");
            }
        }
    }
    // Every agent, verifiers included, runs on Sonnet by default.
    assert!(agents.iter().all(|a| a["model"] == "sonnet"), "{agents:?}");
    assert_eq!(
        report["totals"]["cacheCreationInputTokens"],
        change * groups.len() as u64
    );
    assert_eq!(
        report["totals"]["cacheReadInputTokens"],
        change * (4 - groups.len() as u64)
    );
}

#[test]
fn every_agent_records_its_tokens_and_the_window_readings() {
    let fx = Fixture::new(60);
    fx.review(&[]);
    let ledger = fx.usage_log("agents.jsonl");
    assert_eq!(ledger.len(), 4, "{ledger:?}");
    for row in &ledger {
        assert_eq!(row["repo"], "repo");
        assert_eq!(row["model"], "sonnet");
        assert_eq!(
            (&row["inputTokens"], &row["outputTokens"]),
            (&10.into(), &20.into())
        );
        assert_eq!(row["costUsd"], 0.01);
        assert!(row["startedAt"].as_str().unwrap() <= row["finishedAt"].as_str().unwrap());
    }
    let mut labels: Vec<&str> = ledger
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    labels.sort();
    labels.dedup();
    assert_eq!(labels.len(), 4, "a row per agent: {labels:?}");

    let readings = fx.usage_log("readings.jsonl");
    assert_eq!(
        readings.len(),
        8,
        "both windows from each agent: {readings:?}"
    );
    let five: Vec<&Value> = readings
        .iter()
        .filter(|r| r["window"] == "five_hour")
        .collect();
    assert_eq!(five.len(), 4);
    assert!(five
        .iter()
        .all(|r| r["utilization"] == 0.5 && r["repo"] == "repo"));
}

#[test]
fn an_unchanged_tree_isnt_reviewed_again_and_a_change_reviews_only_the_delta() {
    let fx = Fixture::new(60);
    fx.review(&[]);
    fx.calls();

    let out = fx.review(&[]);
    assert!(out.contains("unchanged since the last review"), "{out}");
    assert!(fx.calls().is_empty());

    write(
        &fx.repo(),
        "src/a.rs",
        &(fs::read_to_string(fx.repo().join("src/a.rs")).unwrap() + "// one more\n"),
    );
    fx.review(&[]);
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
    assert!(fx.state().join("feature/prev/report.json").is_file());

    // --full reviews everything, even an unchanged tree.
    fx.review(&["--full"]);
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
    let out = fx.review_with(&["main"], &[], &[("FAKE_FAIL", Path::new("testing"))]);
    assert!(
        out.contains("Failed (no readable reply): testing."),
        "{out}"
    );
    assert_eq!(fx.report()["incomplete"], serde_json::json!(["testing"]));
    fx.calls();

    fx.review(&[]);
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
fn the_floor_reviews_risky_paths_with_one_sonnet_and_nothing_else() {
    let fx = Fixture::new(60);
    // No [floor] risky globs: nothing is risky.
    let out = fx.review(&["--floor"]);
    assert!(out.contains("no risky paths"), "{out}");
    assert!(fx.calls().is_empty());
    assert!(!fx.state().join("feature.json").exists());

    write(
        &fx.repo(),
        ".lens.toml",
        "[floor]\nrisky = [\"src/payments/**\"]\n",
    );
    fx.review(&["--floor"]);
    assert!(fx.calls().is_empty(), "src/a.rs and src/b.rs aren't risky");

    write(
        &fx.repo(),
        ".lens.toml",
        "[floor]\nrisky = [\"src/a.rs\"]\n",
    );
    fx.review(&["--floor"]);
    let calls = fx.calls();
    let reviewers: Vec<&Value> = calls.iter().filter(|c| c["role"] == "reviewer").collect();
    assert_eq!(reviewers.len(), 1);
    let args = reviewers[0]["args"].as_array().unwrap();
    let model = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[model + 1], "sonnet");
    assert!(reviewers[0]["prompt"]
        .as_str()
        .unwrap()
        .contains("name=\"correctness\""));
    assert!(reviewers[0]["prompt"]
        .as_str()
        .unwrap()
        .contains("this change caused"));
    let report = fx.report();
    assert_eq!(report["tier"], "floor");
    assert_eq!(report["profiles"][0]["reason"], "risky path: src/a.rs");
}

/// A `gh` stand-in for the PR pass: `pr view` answers from a canned PR, `api` lists no earlier
/// comments or reviews and the PRs stacked on it from `$FAKE/children.json` (none without it), and
/// a posted review is saved to `$FAKE/posted.json`.
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
elif args[0] == "api" and "pulls?base=" in args[-1] and os.path.exists(os.path.join(fake, "children.json")):
    print(open(os.path.join(fake, "children.json")).read())
elif args[0] == "api" and "POST" in args:
    open(os.path.join(fake, "posted.json"), "w").write(sys.stdin.read())
    print(json.dumps({"html_url": "https://github.test/pr/7#review-1"}))
elif args[0] == "api" and args[-1].endswith("/reviews") and os.path.exists(os.path.join(fake, "posted.json")):
    review = json.load(open(os.path.join(fake, "posted.json")))
    print(json.dumps([{"body": review["body"], "html_url": "https://github.test/pr/7#review-1"}]))
elif args[0] == "api":
    print("[]")
else:
    sys.exit("unexpected: " + " ".join(args))
"#;

#[test]
fn the_pr_pass_runs_the_lead_and_its_focus_areas_and_posts() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();

    // Every PR is in the sample, so PR 7 is reviewed whatever its hash.
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let out = fx.review_with(&[], &["--pr", "7"], &[("LENS_GH", &gh), ("FAKE", &fake)]);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["pass"], "pr");
    assert_eq!(report["risk"], "moderate");
    assert_eq!(report["assessment"], "**Impact**: small");
    assert_eq!(
        fs::read_to_string(fx.state().join("pr-7/impact.md")).unwrap(),
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
    // One post by default: every blocker, major and minor in the body, no inline comments.
    assert!(posted["comments"].as_array().unwrap().is_empty());
    assert!(posted["body"]
        .as_str()
        .unwrap()
        .contains("<!-- lens:finding key="));
    assert_eq!(posted["event"], "REQUEST_CHANGES");
    assert!(
        out.contains("Posted (request changes): https://github.test/pr/7#review-1"),
        "{out}"
    );
}

#[test]
fn the_pr_pass_never_verifies_a_nit() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let out = fx.review_with(
        &[],
        &["--pr", "7"],
        &[
            ("LENS_GH", &gh),
            ("FAKE", &fake),
            ("FAKE_NIT", Path::new("1")),
        ],
    );

    // The nit is never posted, so no verifier is spent on it; the others are still verified.
    let calls = fx.calls();
    let verifiers: Vec<&Value> = calls.iter().filter(|c| c["role"] == "verifier").collect();
    assert!(!verifiers.is_empty());
    assert!(verifiers
        .iter()
        .all(|v| !v["prompt"].as_str().unwrap().contains("stray blank line")));

    // It's listed as dropped, not as a finding, so the counts hold only what was verified.
    let report = read_json(&fx.state().join("pr-7.json"));
    let findings = report["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["severity"] != "nit"),
        "{findings:?}"
    );
    let nit = report["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["title"] == "stray blank line")
        .expect("the nit is in dropped");
    assert_eq!(nit["reason"], "nit: never posted, so not verified");
    assert!(out.contains("1 nit(s) left out unverified"), "{out}");

    let posted: Value =
        serde_json::from_str(&fs::read_to_string(fake.join("posted.json")).unwrap()).unwrap();
    assert!(!posted["body"]
        .as_str()
        .unwrap()
        .contains("stray blank line"));
}

#[test]
fn only_runs_the_named_profiles_and_leaves_the_prs_own_report_alone() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(
        &[],
        &["--pr", "7", "--dry-run", "--only", "correctness"],
        &env,
    );

    // One reviewer, no lead and so no focus areas; its findings are still verified.
    let calls = fx.calls();
    let reviewers: Vec<&Value> = calls.iter().filter(|c| c["role"] == "reviewer").collect();
    assert_eq!(reviewers.len(), 1, "{reviewers:?}");
    assert!(!reviewers[0]["prompt"]
        .as_str()
        .unwrap()
        .contains("You are the lead reviewer"));
    assert!(calls.iter().any(|c| c["role"] == "verifier"));

    let report = read_json(&fx.state().join(".only/pr-7.json"));
    assert_eq!(report["mode"], "full");
    let ran: Vec<&str> = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["ran"] == true)
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(ran, ["correctness"]);
    assert!(!fx.state().join("pr-7.json").exists());
    assert!(!fake.join("posted.json").exists());

    // A real review afterwards neither reuses nor builds on the partial run.
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let full = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(full["mode"], "full");
    assert!(fx.calls().iter().any(|c| c["prompt"]
        .as_str()
        .unwrap()
        .contains("You are the lead reviewer")));
}

#[test]
fn a_pr_whose_head_is_unchanged_reuses_its_last_review() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    assert!(first["lens"].as_str().is_some_and(|l| !l.is_empty()));
    assert!(!fx.calls().is_empty());

    // Same head, same lens: no agent runs and the findings are the same ones.
    let out = fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    assert!(fx.calls().is_empty(), "{out}");
    assert!(out.contains("unchanged since the last review"), "{out}");
    assert_eq!(
        read_json(&fx.state().join("pr-7.json"))["findings"],
        first["findings"]
    );

    // --full reviews it again.
    fx.review_with(&[], &["--pr", "7", "--dry-run", "--full"], &env);
    assert!(!fx.calls().is_empty());

    // Every run that reviewed is kept; the reuse wrote nothing new.
    let kept = fs::read_dir(fx.state().join("pr-7/reports"))
        .unwrap()
        .count();
    assert_eq!(kept, 2);
}

#[test]
fn a_rereview_reviews_only_what_changed_since_the_last_posted_review() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(first["mode"], "full");
    fx.calls();

    fx.push_fix(&fake);

    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["delta"]["from"], first["head"]);
    assert_eq!(report["delta"]["changedLines"], 1);
    let calls = fx.calls();
    let system = calls[0]["system"].as_str().unwrap();
    assert!(
        system.contains("What changed since the last review"),
        "{system}"
    );
    assert!(system.contains("+// a fix"), "{system}");
    assert!(!system.contains("Changed code no test names"), "{system}");
    // The first review's blockers and majors are verified again at the new head.
    let carried = calls
        .iter()
        .filter(|c| {
            c["prompt"]
                .as_str()
                .unwrap_or("")
                .contains("an earlier review raised it")
        })
        .count();
    let serious = first["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == "blocker" || f["severity"] == "major")
        .count();
    assert!(serious > 0 && carried >= 1, "{carried} of {serious}");
    // The base and the PR's files are the same, so the lead's assessment is reused; its focus
    // area on src/a.rs, which the delta touches, is reviewed again.
    assert_lead_reused(&report, &first, &calls);
    assert!(ran(&report).contains(&"focus: parsing".to_string()));

    // --full reviews the whole change again.
    fx.review_with(&[], &["--pr", "7", "--dry-run", "--full"], &env);
    assert_eq!(read_json(&fx.state().join("pr-7.json"))["mode"], "full");
    assert!(fx.calls().iter().any(is_lead));
}

#[test]
fn a_merged_child_reviewed_on_its_own_pr_is_not_reviewed_again_on_the_parent() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    fx.calls();

    let raised = json!([{"file": "src/c.rs", "line": 1, "severity": "major",
                         "title": "c is never called", "profiles": ["correctness"]}]);
    fx.merge_child(&fake, 8, "src/c.rs", false, Some(raised));
    fx.push_head(&fake);
    // The last review approved, so this one posts its verdict and the body that names the child.
    let mut earlier = read_json(&fake.join("posted.json"));
    earlier["body"] = json!(earlier["body"]
        .as_str()
        .unwrap()
        .replace("verdict=REQUEST_CHANGES", "verdict=APPROVE"));
    fs::write(fake.join("posted.json"), earlier.to_string()).unwrap();

    let (_, log) = fx.review_logged(&[], &["--pr", "7"], &env);
    assert!(log.contains("taking out #8"), "{log}");
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["stack"]["children"][0]["number"], 8);
    assert_eq!(report["stack"]["children"][0]["files"], json!(["src/c.rs"]));
    // Nothing is left once the child comes out, so only where it meets the parent is reviewed.
    assert_eq!(ran(&report), ["stack-integration"]);
    let calls = fx.calls();
    let stack = calls
        .iter()
        .filter_map(|c| c["prompt"].as_str())
        .find(|p| p.contains("## The stack"))
        .expect("the stack reviewer ran");
    assert!(
        stack.contains("#8 (https://github.test/pr/8): src/c.rs"),
        "{stack}"
    );
    assert!(stack.contains("Left over once the children came out (conflict resolutions, commits outside them; the delta above): none"), "{stack}");
    assert!(stack.contains("child-findings.json"), "{stack}");
    let posted: Value = read_json(&fake.join("posted.json"));
    let body = posted["body"].as_str().unwrap();
    assert!(
        body.contains("Code from child PR [#8](https://github.test/pr/8) was reviewed on that PR"),
        "{body}"
    );
    assert!(!body.contains("c is never called"), "{body}");
}

#[test]
fn a_squash_merged_child_leaves_only_the_parents_own_commits_to_review() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    fx.calls();

    // The child's review raised the blocker the correctness reviewer raises again on the parent.
    let raised = json!([{"file": "src/a.rs", "line": 3, "severity": "blocker",
                         "title": "empty input accepted", "profiles": ["correctness"]}]);
    fx.merge_child(&fake, 8, "src/c.rs", true, Some(raised));
    fx.push_fix(&fake);

    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["stack"]["children"][0]["number"], 8);
    assert_eq!(report["delta"]["changedLines"], 1);
    let ran = ran(&report);
    assert!(ran.contains(&"correctness".to_string()), "{ran:?}");
    assert!(ran.contains(&"stack-integration".to_string()), "{ran:?}");
    let calls = fx.calls();
    let reviewer = calls
        .iter()
        .find(|c| {
            c["prompt"]
                .as_str()
                .unwrap_or("")
                .contains("name=\"correctness\"")
        })
        .unwrap();
    let delta = delta_section(reviewer);
    assert!(delta.contains("+// a fix"), "{delta}");
    assert!(!delta.contains("src/c.rs"), "{delta}");
    // The blocker the child already raised isn't raised again here; it points at the child.
    let findings = report["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .all(|f| f["title"] != "empty input accepted"),
        "{findings:?}"
    );
    let dropped = report["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["title"] == "empty input accepted")
        .expect("the child's finding is in dropped");
    assert_eq!(dropped["child"], 8);
    assert_eq!(
        dropped["reason"],
        "already raised on child PR #8 (https://github.test/pr/8)"
    );
}

#[test]
fn a_merged_child_lens_never_reviewed_is_reviewed_on_the_parent() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    fx.calls();

    fx.merge_child(&fake, 8, "src/c.rs", false, None);
    fx.push_head(&fake);

    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["stack"]["children"], json!([]));
    assert_eq!(report["stack"]["kept"][0]["number"], 8);
    assert_eq!(report["stack"]["kept"][0]["reason"], "no lens review");
    assert!(!ran(&report).contains(&"stack-integration".to_string()));
    let calls = fx.calls();
    let reviewer = calls
        .iter()
        .find(|c| {
            c["prompt"]
                .as_str()
                .unwrap_or("")
                .contains("name=\"correctness\"")
        })
        .unwrap();
    assert!(delta_section(reviewer).contains("+pub fn c() {}"));
}

fn line_of(report: &Value, title: &str) -> u64 {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["title"] == title)
        .unwrap_or_else(|| panic!("no {title} in {report}"))["line"]
        .as_u64()
        .unwrap()
}

/// The "what changed since the last review" section of an agent's system prompt.
fn delta_section(call: &Value) -> &str {
    let system = call["system"].as_str().unwrap();
    let (_, rest) = system
        .split_once("What changed since the last review")
        .unwrap_or_else(|| panic!("no delta in {system}"));
    rest.split("\n## ").next().unwrap()
}

fn carried(calls: &[Value]) -> Vec<&str> {
    calls
        .iter()
        .filter_map(|c| c["prompt"].as_str())
        .filter(|p| p.contains("an earlier review raised it"))
        .collect()
}

const HEADER: &str = "// h0\n// h1\n// h2\n// h3\n// h4\n";

#[test]
fn a_rebase_that_keeps_every_patch_reuses_the_review_with_its_lines_moved() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    // Main's later edits land above the PR's hunk but outside its context, so its patch holds.
    fx.move_main(&[("src/a.rs", &format!("{HEADER}pub fn a() {{}}\n"))]);
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    let at = line_of(&first, "empty input accepted");
    assert!(!fx.calls().is_empty());

    fx.move_main(&[("src/a.rs", &format!("// top\n{HEADER}pub fn a() {{}}\n"))]);
    fx.push_rebase(&fake);
    let (out, log) = fx.review_logged(&[], &["--pr", "7", "--dry-run"], &env);
    assert!(fx.calls().is_empty(), "{out}");
    assert!(out.contains("rebased with the same patches"), "{out}");
    assert!(log.contains("every commit's patch is the same"), "{log}");
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "rebased");
    assert_eq!(report["rebased"]["from"], first["head"]);
    assert_eq!(report["head"], json!(fx.rev("HEAD")));
    assert_eq!(report["mergeBase"], json!(fx.rev("main")));
    assert!(report["delta"].is_null());
    assert_eq!(line_of(&report, "empty input accepted"), at + 1);
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        first["findings"].as_array().unwrap().len()
    );

    // Nothing new was posted, so the next rebase starts from the reused report, not the posted head.
    let second = report["head"].clone();
    fx.move_main(&[(
        "src/a.rs",
        &format!("// top\n// top\n{HEADER}pub fn a() {{}}\n"),
    )]);
    fx.push_rebase(&fake);
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    assert!(fx.calls().is_empty());
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "rebased");
    assert_eq!(report["rebased"]["from"], second);
    assert_eq!(line_of(&report, "empty input accepted"), at + 2);

    // --full reviews it again.
    fx.review_with(&[], &["--pr", "7", "--dry-run", "--full"], &env);
    assert_eq!(read_json(&fx.state().join("pr-7.json"))["mode"], "full");
    assert!(fx.calls().iter().any(is_lead));
}

#[test]
fn a_rebase_with_one_edited_commit_reviews_only_that_commit() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    fx.move_main(&[("src/a.rs", &format!("{HEADER}pub fn a() {{}}\n"))]);
    write(&d, "src/b.rs", "pub fn b() {}\npub fn b2() {}\n");
    git(&d, &["commit", "-qam", "more"]);
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    let at = line_of(&first, "empty input accepted");
    fx.calls();

    // Four lines up, so the carried blocker sits too far from the one the delta raises to merge.
    let top = "// top\n".repeat(4);
    fx.move_main(&[("src/a.rs", &format!("{top}{HEADER}pub fn a() {{}}\n"))]);
    write(&d, "src/b.rs", "pub fn b() {}\npub fn b3() {}\n");
    git(&d, &["commit", "-qa", "--amend", "--no-edit"]);
    fx.push_rebase(&fake);
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["delta"]["from"], json!(fx.rev("HEAD~1")));
    let calls = fx.calls();
    let delta = delta_section(&calls[0]);
    assert!(delta.contains("+pub fn b3() {}"), "{delta}");
    assert!(!delta.contains("+// line 0"), "{delta}");
    // The earlier blocker is verified again where it sits now.
    let carried = carried(&calls);
    assert!(
        carried
            .iter()
            .any(|p| p.contains(&format!("line: {}", at + 4))),
        "{carried:?}"
    );
}

#[test]
fn a_rebase_whose_conflicts_changed_a_commit_reviews_the_resolution() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    fx.move_main(&[("src/c.rs", "pub fn c() {}\n")]);
    write(&d, "src/c.rs", "pub fn c() { todo!() }\n");
    git(&d, &["commit", "-qam", "fill c"]);
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    fx.calls();

    git(&d, &["switch", "-q", "main"]);
    write(&d, "src/c.rs", "pub fn c() -> u8 { 0 }\n");
    git(&d, &["commit", "-qam", "c returns"]);
    git(&d, &["switch", "-q", "feature"]);
    let conflicted = Command::new("git")
        .arg("-C")
        .arg(&d)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "rebase",
            "-q",
            "main",
        ])
        .output()
        .unwrap();
    assert!(!conflicted.status.success(), "the rebase should conflict");
    write(&d, "src/c.rs", "pub fn c() -> u8 { todo!() }\n");
    git(&d, &["add", "src/c.rs"]);
    git(&d, &["-c", "core.editor=true", "rebase", "--continue"]);
    fx.push_rebase(&fake);

    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["delta"]["from"], json!(fx.rev("HEAD~1")));
    let calls = fx.calls();
    let delta = delta_section(&calls[0]);
    assert!(delta.contains("+pub fn c() -> u8 { todo!() }"), "{delta}");
    assert!(!delta.contains("+// line 0"), "{delta}");
}

#[test]
fn a_rebase_from_a_head_that_cant_be_fetched_is_reviewed_in_full() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    fx.calls();

    // The posted review names a head that no longer exists anywhere.
    let mut posted = read_json(&fake.join("posted.json"));
    let body = posted["body"].as_str().unwrap();
    let head = first["head"].as_str().unwrap();
    assert!(body.contains(head), "{body}");
    posted["body"] = json!(body.replace(head, &"e".repeat(40)));
    fs::write(fake.join("posted.json"), posted.to_string()).unwrap();

    fx.move_main(&[("README.md", "# a\n\nMoved.\n")]);
    fx.push_rebase(&fake);
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "full");
    assert!(report["rebased"].is_null());
    assert!(fx.calls().iter().any(is_lead));
}

#[test]
fn a_rereview_with_nothing_posted_reuses_the_lead_until_the_files_change() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(&d, ".lens.toml", "[pr]\nsample = 1\n");
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let first = read_json(&fx.state().join("pr-7.json"));
    assert!(fx.calls().iter().any(is_lead));

    // No review was posted, so this is a full review, but the lead's assessment still holds.
    fx.push_fix(&fake);
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    let report = read_json(&fx.state().join("pr-7.json"));
    assert_eq!(report["mode"], "full");
    assert_lead_reused(&report, &first, &fx.calls());
    assert_eq!(report["risk"], first["risk"]);
    assert_eq!(
        fs::read_to_string(fx.state().join("pr-7/impact.md")).unwrap(),
        "**Impact**: small"
    );
    assert!(ran(&report).contains(&"focus: parsing".to_string()));

    // A commit that adds a file changes what the PR touches: the lead runs again.
    write(&d, "src/c.rs", "pub fn c() {}\n");
    git(&d, &["add", "src/c.rs"]);
    fx.push_fix(&fake);
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    assert!(fx.calls().iter().any(is_lead));
    assert!(read_json(&fx.state().join("pr-7.json"))["lead"]["reusedFrom"].is_null());
}

fn is_lead(call: &Value) -> bool {
    call["prompt"]
        .as_str()
        .unwrap_or("")
        .contains("You are the lead reviewer")
}

fn ran(report: &Value) -> Vec<String> {
    report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["ran"] == true)
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

fn assert_lead_reused(report: &Value, earlier: &Value, calls: &[Value]) {
    assert!(!calls.iter().any(is_lead), "the lead ran again");
    assert_eq!(report["assessment"], earlier["assessment"]);
    assert_eq!(report["lead"]["reusedFrom"], earlier["head"]);
    let lead = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "impact-assessment")
        .unwrap();
    assert_eq!(lead["ran"], false, "{lead}");
}

#[test]
fn keep_reports_off_keeps_only_the_latest() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(
        &d,
        ".lens.toml",
        "[pr]\nsample = 1\n[review]\nkeep_reports = false\n",
    );
    let env = [("LENS_GH", gh.as_path()), ("FAKE", fake.as_path())];
    fx.review_with(&[], &["--pr", "7", "--dry-run"], &env);
    assert!(fx.state().join("pr-7.json").is_file());
    assert!(!fx.state().join("pr-7/reports").exists());
}

#[test]
fn a_pr_outside_the_sample_runs_nothing() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    write(&d, ".lens.toml", "[pr]\nsample = 0\n");
    // No gh and no origin: a PR outside the sample is skipped before either is needed.
    let out = fx.review_with(&[], &["--pr", "7"], &[]);
    assert!(
        out.contains("PR #7 isn't in the sample ([pr] sample = 0)"),
        "{out}"
    );
    assert!(fx.calls().is_empty());
    assert!(!fx.dir.path().join("cache/lens/state").exists());
}

#[test]
fn a_review_by_hand_keeps_its_state_out_of_the_repo() {
    let fx = Fixture::new(60);
    let before = status(&fx.repo());
    fx.review(&[]);
    assert_eq!(status(&fx.repo()), before);
    assert!(!fx.repo().join(".review").exists());
    assert!(fx.state().join("feature/diff.patch").is_file());
    assert_eq!(fx.report()["pass"], "local");
}

#[test]
fn the_protocol_writes_a_result_and_never_touches_the_worktree() {
    let fx = Fixture::new(60);
    let before = status(&fx.repo());
    let out = fx.protocol(1, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(status(&fx.repo()), before);
    assert!(!fx.repo().join(".review").exists());
    assert!(!fx.dir.path().join("cache/lens/state").exists());
    assert!(fx.dir.path().join("state/feature.json").is_file());

    let result = fx.result(1);
    assert_eq!(result["protocol"], 1);
    assert_eq!(result["reviewer"], "lens");
    assert_eq!(result["verdict"], "fail");
    assert!(result["headline"]
        .as_str()
        .unwrap()
        .starts_with("2 findings"));
    let findings = result["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0]["id"], "F1");
    assert_eq!(findings[0]["severity"], "blocker");
    assert_eq!(findings[0]["file"], "src/a.rs");
    assert_eq!(findings[0]["line"], 3);
    assert_eq!(
        findings[0]["detail"],
        "parse(\"\") returns Ok\n\nFix: return Err on an empty string in parse()\n\n1. parse(\"\")\n2. Ok"
    );
    assert_eq!(findings[0]["url"], Value::Null);
    assert_eq!(findings[1]["id"], "F2");
    assert_eq!(findings[1]["file"], "src/b.rs");
    assert!(result["cost"]["usd"].as_f64().unwrap() > 0.0);

    // The intent reached intent-alignment, and no fix loop ran.
    let calls = fx.calls();
    assert!(calls.iter().any(|c| c["prompt"]
        .as_str()
        .unwrap()
        .contains("name=\"intent-alignment\"")));
}

#[test]
fn the_next_round_drops_what_triage_settled_and_reviews_only_the_delta() {
    let fx = Fixture::new(60);
    assert!(fx.protocol(1, &[]).status.success());
    fx.calls();

    // The triage dismissed the blocker and sent the major to be fixed; the task's agent changed
    // src/a.rs.
    let mut previous = fx.result(1);
    previous["findings"][0]["outcome"] = "dismiss".into();
    previous["findings"][0]["reason"] = "empty input is allowed".into();
    previous["findings"][1]["outcome"] = "fix".into();
    let prev = fx.dir.path().join("previous.json");
    fs::write(&prev, previous.to_string()).unwrap();
    write(
        &fx.repo(),
        "src/a.rs",
        &(fs::read_to_string(fx.repo().join("src/a.rs")).unwrap() + "// one more\n"),
    );

    let out = fx.protocol(2, &[("TASKBOARD_PREVIOUS", &prev)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = read_json(&fx.dir.path().join("state/feature.json"));
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["settled"][0]["id"], "F1");
    let calls = fx.calls();
    // The correctness reviewer raised the blocker again: it's dropped unverified. The major the
    // triage sent to be fixed is verified again, though the delta didn't touch src/b.rs.
    let verifiers: Vec<&Value> = calls.iter().filter(|c| c["role"] == "verifier").collect();
    assert_eq!(verifiers.len(), 1);
    assert!(verifiers[0]["prompt"]
        .as_str()
        .unwrap()
        .contains("src/b.rs"));
    let result = fx.result(2);
    assert_eq!(result["verdict"], "fail");
    let findings = result["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["id"], "F2");
    assert_eq!(findings[0]["title"], "leaks a handle");

    // Round 3 on the same tree: the triage backlogged F2, so nothing is left and nothing runs.
    let mut previous = fx.result(2);
    previous["findings"][0]["outcome"] = "backlog".into();
    fs::write(&prev, previous.to_string()).unwrap();
    assert!(fx
        .protocol(3, &[("TASKBOARD_PREVIOUS", &prev)])
        .status
        .success());
    assert!(fx.calls().is_empty());
    let result = fx.result(3);
    assert_eq!(result["verdict"], "pass");
    assert!(result["findings"].as_array().unwrap().is_empty());
    assert_eq!(result["cost"]["usd"], 0.0);
}

#[test]
fn a_docs_only_change_is_a_skip_under_the_protocol() {
    let fx = Fixture::new(1);
    git(&fx.repo(), &["reset", "-q", "--hard", "main"]);
    write(&fx.repo(), "README.md", "# a\n\nMore words.\n");
    assert!(fx.protocol(1, &[]).status.success());
    assert_eq!(fx.result(1)["verdict"], "skip");
    assert!(fx.calls().is_empty());

    git(&fx.repo(), &["checkout", "-q", "README.md"]);
    assert!(fx.protocol(2, &[]).status.success());
    let result = fx.result(2);
    assert_eq!(result["verdict"], "skip");
    assert!(result["headline"]
        .as_str()
        .unwrap()
        .contains("nothing to review"));
}

#[test]
fn an_incomplete_review_is_a_reviewer_error_under_the_protocol() {
    let fx = Fixture::new(60);
    let out = fx.protocol(1, &[("FAKE_FAIL", Path::new("testing"))]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("incomplete"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!fx.dir.path().join("result-1.json").exists());
}

#[test]
fn backoff_pauses_reviews_while_a_window_is_past_its_limit_and_says_when_it_lifts() {
    let fx = Fixture::new(60);
    write(
        &fx.repo(),
        ".lens.toml",
        "[backoff.five_hour]\npause_at = 0.85\n",
    );
    fx.read_window("five_hour", 0.9, 3600);
    let out = fx.review(&[]);
    assert!(
        out.contains("lens: paused: backoff: the plan's five_hour window is 90% used (pause_at 85%); resumes at "),
        "{out}"
    );
    assert!(fx.calls().is_empty());
    assert!(!fx.dir.path().join("cache/lens/state").exists());

    fx.read_window("five_hour", 0.2, 3600);
    let (_, log) = fx.review_logged(&["main"], &[], &[]);
    assert!(log.contains("lens: backoff lifted: "), "{log}");
    assert!(!fx.calls().is_empty());
}

#[test]
fn backoff_lighter_at_runs_the_correctness_reviewer_only() {
    let fx = Fixture::new(60);
    write(
        &fx.repo(),
        ".lens.toml",
        "[backoff.five_hour]\nlighter_at = 0.7\n",
    );
    fx.read_window("five_hour", 0.75, 3600);
    let (_, log) = fx.review_logged(&["main"], &[], &[]);
    assert!(
        log.contains("lens: backoff: the plan's five_hour window is 75% used (lighter_at 70%); running correctness only, skipping testing"),
        "{log}"
    );
    let reviewers: Vec<Value> = fx
        .calls()
        .into_iter()
        .filter(|c| c["role"] == "reviewer")
        .collect();
    assert_eq!(reviewers.len(), 1);
    let report = fx.report();
    let testing = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "testing")
        .unwrap();
    assert_eq!(
        (&testing["ran"], &testing["reason"]),
        (&json!(false), &json!("backoff"))
    );
}

#[test]
fn backoff_ahead_of_pace_skips_prs_outside_the_smaller_share() {
    let fx = Fixture::new(60);
    write(
        &fx.repo(),
        ".lens.toml",
        "[backoff.seven_day]\nahead_by = 0.1\nsample = 0\n",
    );
    // A day into the week with 60% used: 46 points ahead of an even pace.
    fx.read_window("seven_day", 0.6, 6 * 86400);
    // No gh and no origin: a skipped PR needs neither.
    let out = fx.review_with(&[], &["--pr", "7"], &[]);
    assert!(
        out.contains("lens: PR #7 skipped: backoff: the plan's seven_day window is 60% used, 46 points ahead of an even pace (ahead_by 10%), so lens reviews 0% of PRs"),
        "{out}"
    );
    assert!(fx.calls().is_empty());
}

#[test]
fn backoff_lighter_at_drops_the_lead_and_its_focus_areas_from_the_pr_pass() {
    let fx = Fixture::new(60);
    let d = fx.repo();
    let (gh, fake) = fx.pr_remote();
    write(
        &d,
        ".lens.toml",
        "[pr]\nsample = 1\n[backoff.five_hour]\nlighter_at = 0.7\n",
    );
    fx.read_window("five_hour", 0.75, 3600);
    let (_, log) = fx.review_logged(&[], &["--pr", "7"], &[("LENS_GH", &gh), ("FAKE", &fake)]);
    assert!(log.contains("lens: backoff: "), "{log}");
    let calls = fx.calls();
    assert!(
        calls.iter().all(|c| !c["prompt"]
            .as_str()
            .unwrap()
            .contains("You are the lead reviewer")),
        "the lead sat the pass out"
    );
    assert_eq!(
        calls.iter().filter(|c| c["role"] == "reviewer").count(),
        1,
        "correctness only"
    );
}

#[test]
fn a_review_the_backoff_lightened_is_incomplete_so_the_next_one_runs_everything() {
    let fx = Fixture::new(60);
    write(
        &fx.repo(),
        ".lens.toml",
        "[backoff.five_hour]\nlighter_at = 0.7\n",
    );
    fx.read_window("five_hour", 0.75, 3600);
    fx.review(&[]);
    let incomplete = fx.report()["incomplete"].clone();
    assert!(
        incomplete.as_array().unwrap().contains(&json!("testing")),
        "{incomplete}"
    );
    fx.calls();

    // The window eases and nothing changed, yet the lightened report isn't taken as current.
    fx.read_window("five_hour", 0.2, 3600);
    fx.review(&[]);
    let reviewers = fx
        .calls()
        .iter()
        .filter(|c| c["role"] == "reviewer")
        .count();
    assert!(reviewers > 1, "{reviewers} reviewers");
    assert!(fx.report().get("incomplete").is_none());
}

#[test]
fn a_held_plan_window_still_shows_a_review_that_matches_the_tree() {
    let fx = Fixture::new(60);
    fx.review(&[]);
    // A turned-away run holds the window and puts the last review back.
    let out = fx.review_with(
        &["main"],
        &["--full"],
        &[("FAKE_REJECT", Path::new("reviewer"))],
    );
    assert!(out.contains("lens: paused: "), "{out}");
    assert!(fx.usage().join("full.json").is_file());
    fx.calls();

    // The window is still held, but nothing changed, so no agent is needed.
    let out = fx.review(&[]);
    assert!(out.contains("unchanged since the last review"), "{out}");
    assert!(!out.contains("paused"), "{out}");
    assert!(fx.calls().is_empty());

    // A change needs agents, and the held window turns them away before any starts.
    write(
        &fx.repo(),
        "src/a.rs",
        &(fs::read_to_string(fx.repo().join("src/a.rs")).unwrap() + "// one more\n"),
    );
    let out = fx.review(&[]);
    assert!(out.contains("lens: paused: "), "{out}");
    assert!(fx.calls().is_empty());
}

#[test]
fn a_local_review_the_plan_turns_away_mid_pass_keeps_the_last_report() {
    let fx = Fixture::new(60);
    fx.review(&[]);
    let before = fs::read_to_string(fx.state().join("feature.json")).unwrap();
    fx.calls();

    for role in ["reviewer", "verifier"] {
        let out = fx.review_with(&["main"], &["--full"], &[("FAKE_REJECT", Path::new(role))]);
        assert!(out.contains("lens: paused: "), "{role}: {out}");
        assert!(out.contains("Nothing was reviewed"), "{role}: {out}");
        assert_eq!(
            fs::read_to_string(fx.state().join("feature.json")).unwrap(),
            before,
            "{role}: the last review stays the current one"
        );
    }
}
