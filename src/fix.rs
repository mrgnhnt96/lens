//! The blocker fix loop's ledger: one file per finding that records every attempt (the fix's
//! diff and the re-review) and decides what comes next. The `/review` skill drives the agents;
//! this module keeps the rules that must not depend on a model: the attempt cap, when an attempt
//! counts as a fix, and when the loop has to stop and bring the human in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::git;

/// Attempts before the loop escalates whatever the escalation judge says.
pub const DEFAULT_CAP: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    pub version: u32,
    pub finding_id: String,
    /// The finding as the report has it.
    pub finding: Value,
    pub report: PathBuf,
    pub root: PathBuf,
    /// The working tree (untracked files included) before the first attempt.
    pub base_tree: String,
    pub cap: u32,
    pub status: Status,
    pub attempts: Vec<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Resolved,
    Escalated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub n: u32,
    pub before: String,
    pub after: String,
    /// The attempt's own diff, `before` to `after`.
    pub patch: PathBuf,
    pub changed_lines: u32,
    /// The fixer agent's JSON reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixer: Option<Value>,
    /// `None` when there was nothing to re-review: the fixer changed nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

/// The re-review of one attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    /// The verifier's reply on the original finding against the fixed code: `confirmed` means it
    /// still fails.
    pub original: Value,
    /// Blocker and major findings the fix introduced, each confirmed by a verifier.
    #[serde(default)]
    pub new_findings: Vec<Value>,
}

impl Review {
    fn still_fails(&self) -> bool {
        self.original.get("verdict").and_then(Value::as_str) != Some("dropped")
    }

    fn resolved(&self) -> bool {
        !self.still_fails() && self.new_findings.is_empty()
    }
}

/// The escalation judge's call after a failed attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub decision: Verdict,
    pub reason: String,
    /// What the human is needed for: `product`, `design`, `no-progress` or `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Continue,
    Escalate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Escalation {
    pub by: EscalatedBy,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscalatedBy {
    Judge,
    Cap,
}

/// What the skill does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Next {
    /// Spawn the fixer for the next attempt.
    Fix,
    /// Spawn the escalation judge on the failed attempt.
    Judge,
    /// The finding is fixed.
    Done,
    /// Stop and print `lens fix summary` for the human.
    Escalate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub ledger: PathBuf,
    pub finding: String,
    pub status: Status,
    pub attempts: u32,
    pub cap: u32,
    pub next: Next,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn next(&self) -> Next {
        match self.status {
            Status::Resolved => Next::Done,
            Status::Escalated => Next::Escalate,
            Status::Open => match self.attempts.last() {
                Some(last) if last.decision.is_none() => Next::Judge,
                _ => Next::Fix,
            },
        }
    }

    fn step(&self, path: &Path) -> Step {
        Step {
            ledger: path.to_path_buf(),
            finding: self.finding_id.clone(),
            status: self.status,
            attempts: self.attempts.len() as u32,
            cap: self.cap,
            next: self.next(),
        }
    }

    /// The tree before the attempt in progress, the tree now, and the diff between them.
    fn pending(&self) -> Result<(String, String, String)> {
        let before = self
            .attempts
            .last()
            .map_or(self.base_tree.clone(), |a| a.after.clone());
        let after = snapshot(&self.root)?;
        let diff = git(
            &self.root,
            &["diff", "--no-color", "--no-ext-diff", &before, &after],
        )?;
        Ok((before, after, diff))
    }

    fn work_dir(path: &Path) -> &Path {
        path.parent().unwrap_or(Path::new("."))
    }
}

/// Opens the loop for finding `id` of `report`: snapshots the working tree and writes the ledger
/// to `<work>/fix/<id>.json`. An open ledger for the same finding is picked up where it stopped.
pub fn start(report: &Path, id: &str, cap: u32) -> Result<Step> {
    if cap == 0 {
        bail!("the cap must be at least 1");
    }
    let report = report
        .canonicalize()
        .with_context(|| format!("finding {}", report.display()))?;
    let doc = read_json(&report)?;
    let finding = doc
        .get("findings")
        .and_then(Value::as_array)
        .and_then(|fs| {
            fs.iter()
                .find(|f| f.get("id").and_then(Value::as_str) == Some(id))
        })
        .with_context(|| format!("{} has no finding {id}", report.display()))?
        .clone();
    let work = report.with_extension("");
    let path = work.join("fix").join(format!("{id}.json"));
    if path.is_file() {
        let ledger = Ledger::load(&path)?;
        if ledger.status == Status::Open {
            return Ok(ledger.step(&path));
        }
    }
    let root = PathBuf::from(
        git(
            report.parent().unwrap_or(Path::new(".")),
            &["rev-parse", "--show-toplevel"],
        )?
        .trim(),
    );
    fs::create_dir_all(path.parent().unwrap())?;
    let ledger = Ledger {
        version: 1,
        finding_id: id.to_string(),
        finding,
        report: report.clone(),
        base_tree: snapshot(&root)?,
        root,
        cap,
        status: Status::Open,
        attempts: Vec::new(),
        escalation: None,
    };
    ledger.save(&path)?;
    update_report(&ledger, &path)?;
    Ok(ledger.step(&path))
}

/// Records the attempt the fixer just made: snapshots the tree, saves the attempt's diff and
/// takes its re-review. `review` is `None` when the fixer changed nothing.
pub fn record(path: &Path, fixer: Option<Value>, review: Option<Review>) -> Result<Step> {
    let mut ledger = Ledger::load(path)?;
    if ledger.next() != Next::Fix {
        bail!(
            "{} is waiting for {:?}, not a fix attempt",
            ledger.finding_id,
            ledger.next()
        );
    }
    let n = ledger.attempts.len() as u32 + 1;
    let (before, after, diff) = ledger.pending()?;
    let patch = Ledger::work_dir(path).join(format!("{}-attempt-{n}.patch", ledger.finding_id));
    fs::write(&patch, &diff)?;
    let changed_lines = changed_lines(&diff);
    if changed_lines > 0 && review.is_none() {
        bail!("attempt {n} changed the code, so it needs a re-review (--review)");
    }
    let resolved = changed_lines > 0 && review.as_ref().is_some_and(Review::resolved);
    ledger.attempts.push(Attempt {
        n,
        before,
        after,
        patch,
        changed_lines,
        fixer,
        review,
        resolved,
        decision: None,
    });
    if resolved {
        ledger.status = Status::Resolved;
    } else if n >= ledger.cap {
        ledger.status = Status::Escalated;
        ledger.escalation = Some(Escalation {
            by: EscalatedBy::Cap,
            reason: format!("{n} attempts without a fix that holds up"),
            needs: None,
        });
    }
    ledger.save(path)?;
    update_report(&ledger, path)?;
    Ok(ledger.step(path))
}

/// The diff of the attempt in progress: everything the working tree gained since the last
/// recorded attempt (or the start), untracked files included. Empty when the fixer changed nothing.
pub fn pending_diff(path: &Path) -> Result<String> {
    Ok(Ledger::load(path)?.pending()?.2)
}

/// Takes the escalation judge's call on the last, failed attempt.
pub fn judge(path: &Path, decision: Decision) -> Result<Step> {
    let mut ledger = Ledger::load(path)?;
    if ledger.next() != Next::Judge {
        bail!(
            "{} is waiting for {:?}, not a judgment",
            ledger.finding_id,
            ledger.next()
        );
    }
    if decision.decision == Verdict::Escalate {
        ledger.status = Status::Escalated;
        ledger.escalation = Some(Escalation {
            by: EscalatedBy::Judge,
            reason: decision.reason.clone(),
            needs: decision.needs.clone(),
        });
    }
    ledger.attempts.last_mut().unwrap().decision = Some(decision);
    ledger.save(path)?;
    update_report(&ledger, path)?;
    Ok(ledger.step(path))
}

/// A short account of the loop for the human: what the finding is, why it stopped, and what
/// each attempt tried.
pub fn summary(path: &Path) -> Result<String> {
    let ledger = Ledger::load(path)?;
    let f = &ledger.finding;
    let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("");
    let at = match f.get("line").and_then(Value::as_u64) {
        Some(line) => format!("{}:{line}", s("file")),
        None => s("file").to_string(),
    };
    let tries = plural(ledger.attempts.len(), "attempt");
    let mut out = match (&ledger.status, &ledger.escalation) {
        (Status::Resolved, _) => format!("lens: fixed {} in {tries}", ledger.finding_id),
        (Status::Escalated, Some(e)) => {
            let why = match (e.by, e.needs.as_deref()) {
                (EscalatedBy::Cap, _) => format!("hit the {}-attempt cap", ledger.cap),
                (_, Some("product")) => "needs a product call".into(),
                (_, Some("design")) => "needs a design call".into(),
                (_, Some("no-progress")) => "attempts stopped making progress".into(),
                _ => "needs you".into(),
            };
            format!("lens: {} needs you after {tries}: {why}", ledger.finding_id)
        }
        _ => format!("lens: fixing {} ({tries} so far)", ledger.finding_id),
    };
    out += &format!("\n\n{}  {at}  {}\n", s("severity"), s("title"));
    if !s("failureScenario").is_empty() {
        out += &format!("  {}\n", s("failureScenario"));
    }
    if let Some(e) = &ledger.escalation {
        out += &format!("\nWhy: {}\n", e.reason);
    }
    if !ledger.attempts.is_empty() {
        out += "\nAttempts:\n";
    }
    for a in &ledger.attempts {
        out += &format!("  {}. {}\n", a.n, attempt_line(a));
    }
    if ledger.status == Status::Escalated {
        out += &format!(
            "\nThe attempts are left in the working tree; `git diff {}` shows them all.\n",
            short(&ledger.base_tree)
        );
    }
    out += &format!("Ledger: {}\n", path.display());
    Ok(out)
}

fn attempt_line(a: &Attempt) -> String {
    let tried = a
        .fixer
        .as_ref()
        .and_then(|f| f.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("(no summary)");
    let outcome = match &a.review {
        _ if a.resolved => "fixed".to_string(),
        None => "changed nothing".to_string(),
        Some(r) if r.still_fails() => {
            let how = r
                .original
                .get("howItFails")
                .and_then(Value::as_str)
                .and_then(|h| h.lines().next())
                .unwrap_or("");
            if how.is_empty() {
                "still fails".to_string()
            } else {
                format!("still fails: {how}")
            }
        }
        Some(r) => format!("introduced {}", plural(r.new_findings.len(), "new finding")),
    };
    format!("{tried} ({} lines) · {outcome}", a.changed_lines)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

/// The working tree as a tree object, untracked (not ignored) files included, without touching
/// the repo's index.
fn snapshot(root: &Path) -> Result<String> {
    let index = PathBuf::from(git(root, &["rev-parse", "--git-path", "lens-fix-index"])?.trim());
    let index = if index.is_absolute() {
        index
    } else {
        root.join(index)
    };
    let real = PathBuf::from(git(root, &["rev-parse", "--git-path", "index"])?.trim());
    let real = if real.is_absolute() {
        real
    } else {
        root.join(real)
    };
    if real.is_file() {
        fs::copy(&real, &index)?;
    }
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let tree = run(&["add", "-A", "."]).and_then(|_| run(&["write-tree"]));
    let _ = fs::remove_file(&index);
    tree
}

fn changed_lines(diff: &str) -> u32 {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++ ")
                && !l.starts_with("--- ")
        })
        .count() as u32
}

/// An agent's reply: plain JSON, or text ending in a fenced `json` block.
pub fn parse_reply(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Ok(v);
    }
    let start = text
        .rfind("```json")
        .context("no JSON and no ```json block in the reply")?;
    let body = &text[start + "```json".len()..];
    let body = body.find("```").map_or(body, |end| &body[..end]);
    serde_json::from_str(body.trim()).context("parsing the reply's ```json block")
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Mirrors the loop's state onto the finding in the report, as `fix`.
fn update_report(ledger: &Ledger, path: &Path) -> Result<()> {
    let mut doc = read_json(&ledger.report)?;
    let Some(finding) = doc
        .get_mut("findings")
        .and_then(Value::as_array_mut)
        .and_then(|fs| {
            fs.iter_mut()
                .find(|f| f.get("id").and_then(Value::as_str) == Some(&ledger.finding_id))
        })
    else {
        return Ok(());
    };
    finding["fix"] = serde_json::json!({
        "status": ledger.status,
        "attempts": ledger.attempts.len(),
        "ledger": path,
    });
    fs::write(&ledger.report, serde_json::to_string_pretty(&doc)? + "\n")
        .with_context(|| format!("writing {}", ledger.report.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_replies() {
        let reply = "I checked it.\n```json\n{\"verdict\": \"dropped\"}\n```\n";
        assert_eq!(parse_reply(reply).unwrap()["verdict"], "dropped");
        assert_eq!(parse_reply("{\"a\": 1}").unwrap()["a"], 1);
        assert!(parse_reply("no json here").is_err());
    }

    #[test]
    fn counts_changed_lines_without_headers() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-a\n+b\n+c\n";
        assert_eq!(changed_lines(diff), 3);
    }

    #[test]
    fn a_review_resolves_only_when_dropped_and_clean() {
        let review = |verdict: &str, new: usize| Review {
            original: serde_json::json!({ "verdict": verdict }),
            new_findings: vec![Value::Null; new],
        };
        assert!(review("dropped", 0).resolved());
        assert!(!review("dropped", 1).resolved());
        assert!(!review("confirmed", 0).resolved());
        assert!(!review("garbled", 0).resolved());
    }
}
