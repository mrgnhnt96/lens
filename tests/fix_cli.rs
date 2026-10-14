//! `lens fix` end to end: the ledger the blocker fix loop keeps, against a throwaway repo with a
//! /review report. The agents' replies are canned; what's under test is the loop's rules.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// A repo on `feature` with a /review report holding one blocker, F1.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(
        d,
        "src/parser.rs",
        "pub fn parse(s: &str) -> Option<&str> {\n    Some(s)\n}\n",
    );
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "init"]);
    git(d, &["switch", "-qc", "feature"]);
    write(d, ".review/.gitignore", "*\n");
    let report = d.join(".review/feature.json");
    write(
        d,
        ".review/feature.json",
        &serde_json::to_string_pretty(&json!({
            "version": 1,
            "pass": "local",
            "branch": "feature",
            "findings": [{
                "id": "F1",
                "profiles": ["correctness"],
                "file": "src/parser.rs",
                "line": 2,
                "severity": "blocker",
                "title": "empty input parses as a valid token",
                "failureScenario": "parse(\"\") returns Some(\"\"), so login() accepts a blank password",
                "confidence": 0.8
            }]
        }))
        .unwrap(),
    );
    (dir, report)
}

fn lens(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lens"))
        .current_dir(dir)
        .arg("fix")
        .args(args)
        .output()
        .unwrap()
}

fn step(dir: &Path, args: &[&str]) -> Value {
    let out = lens(dir, args);
    assert!(
        out.status.success(),
        "lens fix {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Writes an agent reply the way the skill hands it over: prose ending in a ```json block, saved
/// in the work folder so it stays out of the attempts' diffs.
fn reply(dir: &Path, name: &str, value: Value) -> String {
    let path = dir.join(".review/feature").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            "Done.\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&value).unwrap()
        ),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn still_fails() -> Value {
    json!({
        "original": {
            "verdict": "confirmed",
            "severity": "blocker",
            "howItFails": "login() (src/auth.rs:3) still reaches parse() with \"\""
        },
        "newFindings": []
    })
}

fn start(dir: &Path, report: &Path, extra: &[&str]) -> (Value, String) {
    let mut args = vec![
        "start",
        "--report",
        report.to_str().unwrap(),
        "--finding",
        "F1",
    ];
    args.extend_from_slice(extra);
    let s = step(dir, &args);
    let ledger = s["ledger"].as_str().unwrap().to_string();
    (s, ledger)
}

fn report_fix(report: &Path) -> Value {
    let doc: Value = serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    doc["findings"][0]["fix"].clone()
}

#[test]
fn a_fix_that_holds_up_resolves_the_finding() {
    let (dir, report) = fixture();
    let d = dir.path();
    let (s, ledger) = start(d, &report, &[]);
    assert_eq!(s["next"], "fix");
    assert!(ledger.ends_with(".review/feature/fix/F1.json"), "{ledger}");

    // Attempt 1 guards the wrong spot: the verifier still shows it failing, the judge says go on.
    write(
        d,
        "src/parser.rs",
        "pub fn parse(s: &str) -> Option<&str> {\n    Some(s.trim())\n}\n",
    );
    let fixer = reply(d, "fixer1", json!({"summary": "trim the input"}));
    let review = reply(d, "review1", still_fails());
    let s = step(
        d,
        &[
            "record", "--ledger", &ledger, "--fixer", &fixer, "--review", &review,
        ],
    );
    assert_eq!(s["next"], "judge");
    let decision = reply(
        d,
        "judge1",
        json!({"decision": "continue", "reason": "the fix is on the right function; one more try"}),
    );
    let s = step(d, &["judge", "--ledger", &ledger, "--decision", &decision]);
    assert_eq!(s["next"], "fix");

    // Attempt 2 rejects empty input, a new untracked test file comes with it, and the re-review
    // drops the finding.
    write(
        d,
        "src/parser.rs",
        "pub fn parse(s: &str) -> Option<&str> {\n    Some(s).filter(|s| !s.is_empty())\n}\n",
    );
    write(d, "tests/parse.rs", "#[test]\nfn empty() {}\n");
    let pending = lens(d, &["diff", "--ledger", &ledger]);
    let pending = String::from_utf8_lossy(&pending.stdout);
    assert!(
        pending.contains("tests/parse.rs"),
        "the pending diff has untracked files: {pending}"
    );
    let fixer = reply(d, "fixer2", json!({"summary": "reject empty input"}));
    let review = reply(
        d,
        "review2",
        json!({"original": {"verdict": "dropped", "reason": "parse() now returns None for \"\""}, "newFindings": []}),
    );
    let s = step(
        d,
        &[
            "record", "--ledger", &ledger, "--fixer", &fixer, "--review", &review,
        ],
    );
    assert_eq!(s["status"], "resolved");
    assert_eq!(s["next"], "done");
    assert_eq!(s["attempts"], 2);

    let saved: Value = serde_json::from_str(&fs::read_to_string(&ledger).unwrap()).unwrap();
    let patch = fs::read_to_string(saved["attempts"][1]["patch"].as_str().unwrap()).unwrap();
    assert!(patch.contains("+    Some(s).filter"), "{patch}");
    assert!(
        patch.contains("-    Some(s.trim())"),
        "attempt 2's diff is its own: {patch}"
    );
    assert!(
        patch.contains("tests/parse.rs"),
        "untracked files count: {patch}"
    );
    assert!(
        !patch.contains(".review"),
        "the report stays out of the diff: {patch}"
    );
    assert_eq!(report_fix(&report)["status"], "resolved");

    let out = lens(d, &["summary", "--ledger", &ledger]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("lens: fixed F1 in 2 attempts"), "{text}");

    // A resolved finding takes no more attempts.
    let out = lens(d, &["record", "--ledger", &ledger, "--review", &review]);
    assert!(!out.status.success());
}

#[test]
fn the_judge_escalates_a_product_call() {
    let (dir, report) = fixture();
    let d = dir.path();
    let (_, ledger) = start(d, &report, &[]);

    // The fixer can't choose between rejecting blanks and allowing passwordless accounts, so it
    // changes nothing; the judge brings the human in.
    let fixer = reply(
        d,
        "fixer1",
        json!({"summary": "no change", "needsDecision": "should blank passwords be allowed for SSO accounts?"}),
    );
    let pending = lens(d, &["diff", "--ledger", &ledger]);
    assert!(
        pending.stdout.is_empty(),
        "nothing changed, so nothing to re-review"
    );
    let s = step(d, &["record", "--ledger", &ledger, "--fixer", &fixer]);
    assert_eq!(s["next"], "judge");
    let decision = reply(
        d,
        "judge1",
        json!({"decision": "escalate", "needs": "product", "reason": "whether SSO accounts may have blank passwords is a product decision"}),
    );
    let s = step(d, &["judge", "--ledger", &ledger, "--decision", &decision]);
    assert_eq!(s["status"], "escalated");
    assert_eq!(s["next"], "escalate");
    assert_eq!(report_fix(&report)["status"], "escalated");

    let out = lens(d, &["summary", "--ledger", &ledger]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("lens: F1 needs you after 1 attempt: needs a product call"),
        "{text}"
    );
    assert!(text.contains("Why: whether SSO accounts"), "{text}");
    assert!(
        text.contains("1. no change (0 lines) · changed nothing"),
        "{text}"
    );
}

#[test]
fn the_cap_escalates_even_when_the_judge_would_go_on() {
    let (dir, report) = fixture();
    let d = dir.path();
    let (_, ledger) = start(d, &report, &["--cap", "3"]);
    let go_on = reply(
        d,
        "judge",
        json!({"decision": "continue", "reason": "getting closer"}),
    );
    let review = reply(d, "review", still_fails());
    for n in 1..=3 {
        write(
            d,
            "src/parser.rs",
            &format!("pub fn parse(s: &str) -> Option<&str> {{\n    Some(s) // try {n}\n}}\n"),
        );
        let fixer = reply(
            d,
            &format!("fixer{n}"),
            json!({"summary": format!("try {n}")}),
        );
        let s = step(
            d,
            &[
                "record", "--ledger", &ledger, "--fixer", &fixer, "--review", &review,
            ],
        );
        if n < 3 {
            assert_eq!(s["next"], "judge");
            assert_eq!(
                step(d, &["judge", "--ledger", &ledger, "--decision", &go_on])["next"],
                "fix"
            );
        } else {
            assert_eq!(s["status"], "escalated");
            assert_eq!(s["next"], "escalate");
        }
    }

    // The judge has no say once the cap is hit.
    let out = lens(d, &["judge", "--ledger", &ledger, "--decision", &go_on]);
    assert!(!out.status.success());

    let out = lens(d, &["summary", "--ledger", &ledger]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("lens: F1 needs you after 3 attempts: hit the 3-attempt cap"),
        "{text}"
    );
    assert!(
        text.contains("3. try 3 (2 lines) · still fails: login() (src/auth.rs:3)"),
        "{text}"
    );
    assert!(text.contains("`git diff "), "{text}");
}

#[test]
fn a_change_needs_a_re_review_and_a_new_finding_fails_the_attempt() {
    let (dir, report) = fixture();
    let d = dir.path();
    let (_, ledger) = start(d, &report, &[]);
    write(
        d,
        "src/parser.rs",
        "pub fn parse(s: &str) -> Option<&str> {\n    None\n}\n",
    );
    let out = lens(d, &["record", "--ledger", &ledger]);
    assert!(
        !out.status.success(),
        "a changed tree can't skip the re-review"
    );

    let review = reply(
        d,
        "review",
        json!({
            "original": {"verdict": "dropped", "reason": "parse() never returns Some now"},
            "newFindings": [{"file": "src/parser.rs", "line": 2, "severity": "blocker", "title": "every token is rejected"}]
        }),
    );
    let s = step(d, &["record", "--ledger", &ledger, "--review", &review]);
    assert_eq!(s["status"], "open");
    assert_eq!(s["next"], "judge");
}

#[test]
fn the_default_cap_is_five() {
    let (dir, report) = fixture();
    let (s, _) = start(dir.path(), &report, &[]);
    assert_eq!(s["cap"], 5);
}
