//! What became of each finding lens posted on a PR, so its reviewers can be judged by what their
//! findings led to. Nothing runs on a schedule: every lens review first checks the PRs whose
//! outcomes aren't settled yet ([`before_review`]), all of them in one go, and
//! `lens pr outcomes` runs the same check by hand. Each posted finding gets a fate:
//!
//! - **fixed**: the code within [`NEAR_LINES`] of its spot changed after the review that posted it;
//! - **dismissed**: it didn't, and its thread was resolved or someone answered it;
//! - **ignored**: it didn't, nobody answered, and the PR is merged or closed;
//! - **open**: none of these yet.
//!
//! A finding listed in a review's body (`[pr] comments = "single"`) has no thread of its own, so
//! only the code and the PR's state tell its fate. The kept reports (`pr-<n>/reports/`) say which
//! reviewer raised each finding, at what severity and at which commit; the result goes to
//! `pr-<n>/outcomes.json` beside them, with a summary by reviewer profile and severity.
//!
//! A PR is unchecked while lens has a report for it (`pr-<n>.json`) and its outcomes are missing,
//! older than that report, or not settled. They settle once the PR is merged or closed, when
//! every finding has a fate other than open.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Config, ForgeKind};
use crate::diff::git;
use crate::forge::Forge;
use crate::pr::{earlier_from, Finding, Posted, NEAR_LINES};

/// The file a PR's outcomes go to, in its work folder `pr-<n>/`.
pub const FILE: &str = "outcomes.json";

/// The fate of a posted finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fate {
    Fixed,
    Dismissed,
    Ignored,
    Open,
}

/// A PR's review thread, as far as telling a finding's fate needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Thread {
    /// The keys of the lens findings its first comment carries.
    pub keys: Vec<String>,
    pub resolved: bool,
    /// The answers after the first comment that aren't lens's own.
    pub replies: Vec<Reply>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub author: Option<String>,
    pub body: String,
}

/// What a forge says about a PR lens posted on: its state, head, posted findings and threads.
#[derive(Debug, Default)]
pub struct Discussion {
    /// `OPEN`, `MERGED` or `CLOSED`; `None` when the forge didn't say.
    pub state: Option<String>,
    /// The PR's head commit, fetched into the repo.
    pub head: Option<String>,
    /// The findings lens posted, from their markers.
    pub posted: Vec<Posted>,
    /// The threads lens started.
    pub threads: Vec<Thread>,
    /// Where each post holding findings was made, by its URL: the commit it was for and, for an
    /// inline comment, its line there. A finding no kept report holds is judged from this.
    pub spots: BTreeMap<String, Spot>,
}

/// The commit a post was made for and, for an inline comment, its line at that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spot {
    pub commit: String,
    pub line: Option<u32>,
}

impl Discussion {
    fn settled(&self) -> bool {
        matches!(self.state.as_deref(), Some("MERGED" | "CLOSED"))
    }
}

/// One posted finding and what became of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tracked {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The reviewer profiles that raised it; empty when no kept report holds it.
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The commit the review that first raised it was for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub fate: Fate,
    pub why: String,
}

/// How many posted findings came to each fate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub posted: usize,
    pub fixed: usize,
    pub dismissed: usize,
    pub ignored: usize,
    pub open: usize,
}

impl Tally {
    fn add(&mut self, fate: Fate) {
        self.posted += 1;
        match fate {
            Fate::Fixed => self.fixed += 1,
            Fate::Dismissed => self.dismissed += 1,
            Fate::Ignored => self.ignored += 1,
            Fate::Open => self.open += 1,
        }
    }

    /// The share of settled findings that were fixed, in percent; `None` while none is settled.
    pub fn fixed_share(&self) -> Option<u32> {
        let settled = self.posted - self.open;
        (settled > 0).then(|| ((self.fixed * 100) as f64 / settled as f64).round() as u32)
    }
}

/// Fates summed up by reviewer profile and by severity. A finding several profiles raised counts
/// for each of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub total: Tally,
    pub by_profile: BTreeMap<String, Tally>,
    pub by_severity: BTreeMap<String, Tally>,
}

impl Summary {
    pub fn of<'a>(findings: impl IntoIterator<Item = &'a Tracked>) -> Summary {
        let mut s = Summary::default();
        for f in findings {
            s.total.add(f.fate);
            if f.profiles.is_empty() {
                s.by_profile.entry(UNKNOWN.into()).or_default().add(f.fate);
            }
            for p in &f.profiles {
                s.by_profile.entry(p.clone()).or_default().add(f.fate);
            }
            let severity = f.severity.clone().unwrap_or_else(|| UNKNOWN.into());
            s.by_severity.entry(severity).or_default().add(f.fate);
        }
        s
    }
}

const UNKNOWN: &str = "unknown";

/// What `pr-<n>/outcomes.json` holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcomes {
    pub pr: u64,
    /// The PR's head when its outcomes were read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Merged or closed with every finding's fate known: the PR is checked no more.
    #[serde(default)]
    pub settled: bool,
    pub checked_at: String,
    pub findings: Vec<Tracked>,
    pub summary: Summary,
}

/// Before a review starts: checks the unchecked PRs in the repo's state folders, unless
/// `[review] track_outcomes` is off. It never stops the review; a failure is a warning.
pub fn before_review(root: &Path, states: &[PathBuf], cfg: &Config) {
    if !cfg.review.track_outcomes || cfg.pr.forge(root) != ForgeKind::Github {
        return;
    }
    let mut seen = BTreeSet::new();
    let canonical = |s: &PathBuf| s.canonicalize().unwrap_or_else(|_| s.clone());
    for state in states.iter().filter(|s| seen.insert(canonical(s))) {
        if unchecked(state).is_empty() {
            continue;
        }
        let checked =
            crate::forge::open(root, &cfg.pr).and_then(|forge| check(root, state, &*forge, &[]));
        match checked {
            Ok(done) if !done.is_empty() => eprintln!(
                "lens: checked what became of the findings on {} PR(s) ({} still unsettled)",
                done.len(),
                done.iter().filter(|o| !o.settled).count()
            ),
            Ok(_) => {}
            Err(e) => eprintln!("lens: couldn't check posted findings' outcomes: {e:#}"),
        }
    }
}

/// Checks the unchecked PRs in `state`, and the ones `also` names however they stand, in one
/// go: writes each one's `pr-<n>/outcomes.json` and returns them.
pub fn check(repo: &Path, state: &Path, forge: &dyn Forge, also: &[u64]) -> Result<Vec<Outcomes>> {
    let mut prs = unchecked(state);
    prs.extend(also);
    prs.sort_unstable();
    prs.dedup();
    if prs.is_empty() {
        return Ok(Vec::new());
    }
    let mut discussions = forge.discussions(&prs)?;
    let mut out = Vec::new();
    for n in prs {
        let Some(d) = discussions.remove(&n) else {
            continue;
        };
        let work = state.join(format!("pr-{n}"));
        let latest = state.join(format!("pr-{n}.json"));
        let kept = reports(&work, &latest);
        if kept.is_empty() {
            continue;
        }
        let o = record(repo, n, &kept, &d);
        fs::create_dir_all(&work).with_context(|| format!("making {}", work.display()))?;
        let path = work.join(FILE);
        fs::write(&path, serde_json::to_string_pretty(&o)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
        out.push(o);
    }
    Ok(out)
}

/// The PRs in `state` lens reported on whose outcomes are missing, older than the latest report,
/// or not settled.
pub fn unchecked(state: &Path) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(state) else {
        return Vec::new();
    };
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut out: Vec<u64> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let n: u64 = name
                .to_str()?
                .strip_prefix("pr-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            let path = state.join(format!("pr-{n}")).join(FILE);
            let stale = match (modified(&e.path()), modified(&path)) {
                (Some(report), Some(checked)) => report > checked,
                (_, checked) => checked.is_none(),
            };
            let settled = || {
                fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Outcomes>(&t).ok())
                    .is_some_and(|o| o.settled)
            };
            (stale || !settled()).then_some(n)
        })
        .collect();
    out.sort_unstable();
    out
}

/// The state folder lens keeps the reviews of the repo holding `repo` in.
pub fn state_of(repo: &Path) -> Result<PathBuf> {
    let root = crate::diff::git(repo, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    Ok(crate::review::state_dir(
        &crate::assets::state_root()?,
        &root,
    ))
}

/// Every PR's stored outcomes in the state folder `state`, by PR number.
pub fn stored(state: &Path) -> Result<Vec<Outcomes>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(state) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", state.display())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_pr = name
            .to_str()
            .and_then(|n| n.strip_prefix("pr-"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let path = entry.path().join(FILE);
        if !is_pr || !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        out.push(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        );
    }
    out.sort_by_key(|o: &Outcomes| o.pr);
    Ok(out)
}

/// PR `number`'s outcomes: each finding `d` says lens posted, judged against `reports`, the PR's
/// reports oldest first.
fn record(repo: &Path, number: u64, reports: &[Value], d: &Discussion) -> Outcomes {
    let origins = origins(reports);
    let mut seen = BTreeSet::new();
    let findings: Vec<Tracked> = d
        .posted
        .iter()
        .filter(|p| seen.insert(p.key.clone()))
        .map(|p| {
            let origin = origins.get(&p.key);
            let spot = p.url.as_ref().and_then(|u| d.spots.get(u));
            let at = match (origin, spot) {
                (Some(o), _) => Some((o.head.as_str(), o.finding.file.as_str(), o.finding.line)),
                (None, Some(s)) => p
                    .path
                    .as_deref()
                    .zip(s.line.or(p.line))
                    .map(|(file, line)| (s.commit.as_str(), file, line)),
                (None, None) => None,
            };
            let changed = at
                .zip(d.head.as_ref())
                .and_then(|((from, file, line), head)| changed_near(repo, from, head, file, line));
            let thread = d.threads.iter().find(|t| t.keys.contains(&p.key));
            let (fate, why) = judge(changed, thread, d.settled());
            let mut t = tracked(p, origin, fate, why);
            if origin.is_none() {
                t.posted_at = spot.map(|s| s.commit.clone());
                t.line = spot.and_then(|s| s.line).or(t.line);
            }
            t
        })
        .collect();
    Outcomes {
        pr: number,
        head: d.head.clone(),
        state: d.state.clone(),
        settled: d.settled() && findings.iter().all(|f| f.fate != Fate::Open),
        checked_at: crate::review::iso(crate::agent::now_secs()),
        summary: Summary::of(&findings),
        findings,
    }
}

/// Where a finding came from: the earliest report that holds it.
#[derive(Debug, Clone)]
struct Origin {
    finding: Finding,
    head: String,
}

fn tracked(p: &Posted, origin: Option<&Origin>, fate: Fate, why: String) -> Tracked {
    let f = origin.map(|o| &o.finding);
    Tracked {
        key: p.key.clone(),
        id: f.map(|f| f.id.clone()),
        profiles: f.map(|f| f.profiles.clone()).unwrap_or_default(),
        severity: f.map(|f| f.severity.clone()),
        title: f.map(|f| f.title.clone()),
        file: f.map(|f| f.file.clone()).or_else(|| p.path.clone()),
        line: f.map(|f| f.line).or(p.line),
        posted_at: origin.map(|o| o.head.clone()),
        url: p.url.clone(),
        fate,
        why,
    }
}

/// The fate of a finding: `changed` is whether the code at its spot changed since it was posted
/// (`None` when that couldn't be read), `thread` its review thread, `settled` whether the PR is
/// merged or closed.
fn judge(changed: Option<bool>, thread: Option<&Thread>, settled: bool) -> (Fate, String) {
    if changed == Some(true) {
        return (Fate::Fixed, "the code at its spot changed since".into());
    }
    let unread = if changed.is_none() {
        " (the code at its spot couldn't be compared)"
    } else {
        ""
    };
    if let Some(r) = thread.and_then(|t| t.replies.first()) {
        let who = r.author.as_deref().unwrap_or("someone");
        let said = r.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return (
            Fate::Dismissed,
            format!("{who} answered without a change{unread}: {}", said.trim()),
        );
    }
    if thread.is_some_and(|t| t.resolved) {
        return (
            Fate::Dismissed,
            format!("its thread was resolved without a change{unread}"),
        );
    }
    if settled {
        return (
            Fate::Ignored,
            format!("the PR was settled with no change or answer{unread}"),
        );
    }
    (Fate::Open, format!("no change or answer yet{unread}"))
}

/// The kept reports in `<work>/reports/` and the latest one, oldest first.
fn reports(work: &Path, latest: &Path) -> Vec<Value> {
    let mut paths: Vec<PathBuf> = fs::read_dir(work.join("reports"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|e| e == "json"));
    paths.push(latest.to_path_buf());
    let mut out: Vec<Value> = paths
        .iter()
        .filter_map(|p| serde_json::from_str(&fs::read_to_string(p).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a["startedAt"].as_str().cmp(&b["startedAt"].as_str()));
    out
}

/// Each finding key's earliest report: who raised it, at what severity and at which commit.
fn origins(reports: &[Value]) -> BTreeMap<String, Origin> {
    let mut out = BTreeMap::new();
    for r in reports {
        let Some(head) = r["head"].as_str() else {
            continue;
        };
        for f in r["findings"].as_array().into_iter().flatten() {
            let Ok(finding) = serde_json::from_value::<Finding>(f.clone()) else {
                continue;
            };
            out.entry(finding.key()).or_insert_with(|| Origin {
                finding,
                head: head.to_string(),
            });
        }
    }
    out
}

/// Whether `file` changed within [`NEAR_LINES`] of `line` between commits `from` and `to`;
/// `None` when either commit can't be read.
fn changed_near(repo: &Path, from: &str, to: &str, file: &str, line: u32) -> Option<bool> {
    if from == to {
        return Some(false);
    }
    let have = |c: &str| git(repo, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok();
    if !have(from) {
        let _ = git(repo, &["fetch", "--quiet", "origin", from]);
    }
    if !have(from) || !have(to) {
        return None;
    }
    let patch = git(repo, &["diff", "-U0", "--no-renames", from, to, "--", file]).ok()?;
    Some(touches(&patch, line))
}

/// Whether a `-U0` patch removes or rewrites old-side lines within [`NEAR_LINES`] of `line`, or
/// inserts lines there.
fn touches(patch: &str, line: u32) -> bool {
    let (lo, hi) = (line.saturating_sub(NEAR_LINES), line + NEAR_LINES);
    patch
        .lines()
        .filter_map(|l| l.strip_prefix("@@ -"))
        .filter_map(|rest| {
            let old = rest.split_whitespace().next()?;
            let (start, count) = match old.split_once(',') {
                Some((s, c)) => (s.parse::<u32>().ok()?, c.parse::<u32>().ok()?),
                None => (old.parse::<u32>().ok()?, 1),
            };
            // A pure insertion goes after old line `start`.
            Some(if count == 0 {
                (start, start + 1)
            } else {
                (start, start + count - 1)
            })
        })
        .any(|(a, b)| a <= hi && b >= lo)
}

/// The lens finding keys `body` carries.
fn keys_of(body: &str) -> Vec<String> {
    earlier_from(&[json!({ "body": body })], &[])
        .findings
        .into_iter()
        .map(|p| p.key)
        .collect()
}

/// How many PRs one GitHub query asks about, to stay well under its node limit.
pub const GITHUB_BATCH: usize = 20;

/// A GraphQL query for `prs`' state, head, lens reviews and review threads, each PR under the
/// alias `pr<n>`. It reads the first 100 reviews and threads of each.
pub fn github_query(prs: &[u64]) -> String {
    let mut q = String::from(
        "query($owner: String!, $name: String!) {\n  repository(owner: $owner, name: $name) {\n",
    );
    for n in prs {
        let _ = writeln!(q, "    pr{n}: pullRequest(number: {n}) {{ ...outcome }}");
    }
    q.push_str(
        "  }
}
fragment outcome on PullRequest {
  state
  headRefOid
  reviews(first: 100) { nodes { body url } }
  reviewThreads(first: 100) {
    nodes {
      isResolved path line originalLine
      comments(first: 50) { nodes { body url author { login } originalCommit { oid } } }
    }
  }
}
",
    );
    q
}

/// Each PR's [`Discussion`] from the answer to [`github_query`]; a PR it doesn't hold is left
/// out.
pub fn github_discussions(answer: &Value, prs: &[u64]) -> BTreeMap<u64, Discussion> {
    prs.iter()
        .filter_map(|&n| {
            let pr = answer.pointer(&format!("/data/repository/pr{n}"))?;
            pr.is_object().then(|| (n, github_discussion(pr)))
        })
        .collect()
}

fn github_discussion(pr: &Value) -> Discussion {
    let nodes = |v: &Value, at: &str| -> Vec<Value> {
        v.pointer(at)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let reviews: Vec<Value> = nodes(pr, "/reviews/nodes")
        .iter()
        .map(|r| json!({ "body": r["body"], "html_url": r["url"] }))
        .collect();
    let mut spots = BTreeMap::new();
    for r in &reviews {
        let url = r["html_url"].as_str();
        if let Some((url, commit)) = url.zip(earlier_from(&[], std::slice::from_ref(r)).head) {
            spots.insert(url.to_string(), Spot { commit, line: None });
        }
    }
    let mut first_comments = Vec::new();
    let mut threads = Vec::new();
    for t in nodes(pr, "/reviewThreads/nodes") {
        let comments = nodes(&t, "/comments/nodes");
        let Some((first, rest)) = comments.split_first() else {
            continue;
        };
        let keys = keys_of(first["body"].as_str().unwrap_or(""));
        if keys.is_empty() {
            continue;
        }
        first_comments.push(json!({
            "body": first["body"], "path": t["path"], "line": t["line"], "html_url": first["url"],
        }));
        let commit = first.pointer("/originalCommit/oid").and_then(Value::as_str);
        if let Some((url, commit)) = first["url"].as_str().zip(commit) {
            let line = t["originalLine"].as_u64().map(|l| l as u32);
            spots.insert(
                url.to_string(),
                Spot {
                    commit: commit.into(),
                    line,
                },
            );
        }
        let replies = rest
            .iter()
            .filter_map(|c| {
                let body = c["body"].as_str().unwrap_or("");
                keys_of(body).is_empty().then(|| Reply {
                    author: c
                        .pointer("/author/login")
                        .and_then(Value::as_str)
                        .map(String::from),
                    body: body.to_string(),
                })
            })
            .collect();
        threads.push(Thread {
            keys,
            resolved: t["isResolved"] == true,
            replies,
        });
    }
    Discussion {
        state: pr["state"].as_str().map(String::from),
        head: pr["headRefOid"].as_str().map(String::from),
        posted: earlier_from(&first_comments, &reviews).findings,
        threads,
        spots,
    }
}

/// `summary` as a table, under `heading`.
pub fn render(heading: &str, summary: &Summary) -> String {
    let t = &summary.total;
    let mut out = format!(
        "{heading}: {} posted findings: {} fixed, {} dismissed, {} ignored, {} open\n",
        t.posted, t.fixed, t.dismissed, t.ignored, t.open
    );
    if t.posted == 0 {
        return out;
    }
    let width = summary
        .by_profile
        .keys()
        .chain(summary.by_severity.keys())
        .map(|k| k.len() + 2)
        .max()
        .unwrap_or(0)
        .max("by severity".len());
    let _ = writeln!(
        out,
        "\n{:width$}  posted  fixed  dismissed  ignored  open  fixed%",
        ""
    );
    for (title, rows) in [
        ("by profile", &summary.by_profile),
        ("by severity", &summary.by_severity),
    ] {
        let _ = writeln!(out, "{title}");
        for (name, t) in rows {
            let share = t.fixed_share().map_or("-".into(), |s| format!("{s}%"));
            let _ = writeln!(
                out,
                "  {name:w$}  {:>6}  {:>5}  {:>9}  {:>7}  {:>4}  {share:>6}",
                t.posted,
                t.fixed,
                t.dismissed,
                t.ignored,
                t.open,
                w = width - 2
            );
        }
    }
    let _ = writeln!(
        out,
        "\nfixed% is fixed over settled (fixed, dismissed or ignored) findings."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(profiles: &[&str], severity: Option<&str>, fate: Fate) -> Tracked {
        Tracked {
            key: "k".into(),
            id: None,
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            severity: severity.map(String::from),
            title: None,
            file: None,
            line: None,
            posted_at: None,
            url: None,
            fate,
            why: String::new(),
        }
    }

    fn thread(resolved: bool, replies: &[&str]) -> Thread {
        Thread {
            keys: vec!["k".into()],
            resolved,
            replies: replies
                .iter()
                .map(|b| Reply {
                    author: Some("dev".into()),
                    body: b.to_string(),
                })
                .collect(),
        }
    }

    fn outcomes(pr: u64, settled: bool) -> Outcomes {
        Outcomes {
            pr,
            head: None,
            state: None,
            settled,
            checked_at: "t".into(),
            findings: vec![],
            summary: Summary::default(),
        }
    }

    #[test]
    fn a_change_near_the_spot_touches_it_and_one_further_off_does_not() {
        let at = |old: &str| format!("diff --git a/f b/f\n@@ -{old} +1,1 @@\n-x\n+y\n");
        assert!(touches(&at("42,1"), 42));
        assert!(touches(&at("45"), 42));
        assert!(touches(&at("30,10"), 42));
        assert!(!touches(&at("46,2"), 42));
        assert!(!touches(&at("10,5"), 42));
        // An insertion after line 38 lands next to line 39, within reach of 42.
        assert!(touches(&at("38,0"), 42));
        assert!(!touches(&at("37,0"), 42));
        assert!(!touches("", 42));
    }

    #[test]
    fn a_change_at_its_spot_is_a_fix_whatever_the_thread_says() {
        let t = thread(true, &["nope"]);
        assert_eq!(judge(Some(true), Some(&t), true).0, Fate::Fixed);
    }

    #[test]
    fn an_answer_or_a_resolve_without_a_change_dismisses() {
        let (fate, why) = judge(
            Some(false),
            Some(&thread(false, &["\nThis is intended."])),
            false,
        );
        assert_eq!(fate, Fate::Dismissed);
        assert_eq!(why, "dev answered without a change: This is intended.");
        let (fate, why) = judge(None, Some(&thread(true, &[])), false);
        assert_eq!(fate, Fate::Dismissed);
        assert!(why.contains("couldn't be compared"), "{why}");
    }

    #[test]
    fn unanswered_and_unchanged_is_ignored_once_the_pr_settles_and_open_before() {
        assert_eq!(
            judge(Some(false), Some(&thread(false, &[])), true).0,
            Fate::Ignored
        );
        assert_eq!(judge(Some(false), None, true).0, Fate::Ignored);
        assert_eq!(judge(Some(false), None, false).0, Fate::Open);
    }

    #[test]
    fn a_finding_belongs_to_the_earliest_report_that_raised_it() {
        let f = |title: &str, profile: &str| {
            json!({"id": "F1", "profiles": [profile], "file": "src/a.rs", "line": 3,
                   "severity": "major", "title": title})
        };
        let reports = [
            json!({"head": "aaa", "startedAt": "1", "findings": [f("x panics", "correctness")]}),
            json!({"head": "bbb", "startedAt": "2",
                   "findings": [f("x panics", "testing"), f("y leaks", "security")]}),
        ];
        let o = origins(&reports);
        let x = &o[&crate::pr::key("src/a.rs", "x panics")];
        assert_eq!(x.head, "aaa");
        assert_eq!(x.finding.profiles, ["correctness"]);
        assert_eq!(o[&crate::pr::key("src/a.rs", "y leaks")].head, "bbb");
    }

    #[test]
    fn reports_come_oldest_first_with_the_latest_among_them() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("pr-7").join("reports");
        fs::create_dir_all(&kept).unwrap();
        let at = |t: &str| json!({ "startedAt": t }).to_string();
        fs::write(kept.join("b.json"), at("2026-10-02")).unwrap();
        fs::write(kept.join("a.json"), at("2026-10-01")).unwrap();
        fs::write(kept.join("notes.txt"), "{}").unwrap();
        let latest = dir.path().join("pr-7.json");
        fs::write(&latest, at("2026-10-03")).unwrap();
        let rs = reports(&dir.path().join("pr-7"), &latest);
        let got: Vec<&str> = rs.iter().filter_map(|r| r["startedAt"].as_str()).collect();
        assert_eq!(got, ["2026-10-01", "2026-10-02", "2026-10-03"]);
    }

    #[test]
    fn one_github_query_asks_about_every_pr() {
        let q = github_query(&[3, 12]);
        assert!(
            q.contains("pr3: pullRequest(number: 3) { ...outcome }"),
            "{q}"
        );
        assert!(
            q.contains("pr12: pullRequest(number: 12) { ...outcome }"),
            "{q}"
        );
        assert!(q.contains("fragment outcome on PullRequest"), "{q}");
    }

    #[test]
    fn github_answers_give_each_pr_its_posted_findings_threads_and_state() {
        let inline = "**major** x\n<!-- lens:finding key=00aa11bb22cc33dd -->";
        let body = "lens review\n\n- **minor** `src/b.rs:9` y\n  <!-- lens:finding key=1111222233334444 -->\n\n<!-- lens:review verdict=APPROVE head=abc1234 -->";
        let answer = json!({"data": {"repository": {
            "pr3": {
                "state": "MERGED",
                "headRefOid": "cafe",
                "reviews": {"nodes": [{"body": body, "url": "https://g/r1"}]},
                "reviewThreads": {"nodes": [
                    {"isResolved": true, "path": "src/a.rs", "line": 4, "originalLine": 3,
                     "comments": {"nodes": [
                        {"body": inline, "url": "https://g/c1", "author": {"login": "lens-bot"},
                         "originalCommit": {"oid": "beef"}},
                        {"body": "Not a bug: the caller checks.", "url": "https://g/c2",
                         "author": {"login": "dev"}}
                    ]}},
                    {"isResolved": false, "path": "src/a.rs", "line": 9, "comments": {"nodes": [
                        {"body": "a human's own thread", "author": {"login": "dev"}}
                    ]}},
                    {"isResolved": false, "comments": {"nodes": []}}
                ]}
            },
            "pr12": null
        }}});
        let mut all = github_discussions(&answer, &[3, 12, 40]);
        assert_eq!(all.keys().copied().collect::<Vec<_>>(), [3]);
        let d = all.remove(&3).unwrap();
        assert_eq!(d.state.as_deref(), Some("MERGED"));
        assert!(d.settled());
        assert_eq!(d.head.as_deref(), Some("cafe"));
        let spot = |commit: &str, line| Spot {
            commit: commit.into(),
            line,
        };
        assert_eq!(d.spots["https://g/c1"], spot("beef", Some(3)));
        assert_eq!(d.spots["https://g/r1"], spot("abc1234", None));
        let posted: Vec<(&str, Option<&str>, Option<u32>)> = d
            .posted
            .iter()
            .map(|p| (p.key.as_str(), p.path.as_deref(), p.line))
            .collect();
        assert_eq!(
            posted,
            [
                ("00aa11bb22cc33dd", Some("src/a.rs"), Some(4)),
                ("1111222233334444", Some("src/b.rs"), Some(9)),
            ]
        );
        assert_eq!(
            d.threads,
            [Thread {
                keys: vec!["00aa11bb22cc33dd".into()],
                resolved: true,
                replies: vec![Reply {
                    author: Some("dev".into()),
                    body: "Not a bug: the caller checks.".into()
                }],
            }]
        );
    }

    #[test]
    fn a_pr_stays_unchecked_until_its_outcomes_are_settled_and_newer_than_its_report() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        let write = |n: u64, o: &Outcomes| {
            fs::create_dir_all(s.join(format!("pr-{n}"))).unwrap();
            let path = s.join(format!("pr-{n}")).join(FILE);
            fs::write(path, serde_json::to_string(o).unwrap()).unwrap();
        };
        for n in [1u64, 2, 3, 4] {
            fs::write(s.join(format!("pr-{n}.json")), "{}").unwrap();
        }
        fs::write(s.join("feature.json"), "{}").unwrap();
        write(2, &outcomes(2, false));
        write(3, &outcomes(3, true));
        write(4, &outcomes(4, true));
        // PR 4 was reviewed again after it settled.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(s.join("pr-4.json"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(unchecked(s), [1, 2, 4]);
        assert!(unchecked(&s.join("missing")).is_empty());
    }

    #[test]
    fn a_settled_pr_with_no_answer_ignores_and_settles() {
        let dir = tempfile::tempdir().unwrap();
        let d = Discussion {
            state: Some("CLOSED".into()),
            head: None,
            posted: vec![Posted {
                key: "k1".into(),
                path: Some("src/a.rs".into()),
                line: Some(3),
                url: None,
            }],
            threads: vec![],
            spots: BTreeMap::new(),
        };
        let o = record(dir.path(), 5, &[], &d);
        assert!(o.settled);
        assert_eq!(o.findings[0].fate, Fate::Ignored);
        assert_eq!(o.findings[0].file.as_deref(), Some("src/a.rs"));
        assert_eq!(o.summary.by_profile["unknown"].ignored, 1);
        let open = record(dir.path(), 5, &[], &Discussion { state: None, ..d });
        assert!(!open.settled);
    }

    #[test]
    fn a_finding_no_kept_report_holds_is_judged_from_where_it_was_posted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let lines = |changed: usize| {
            (1..=30)
                .map(|n| {
                    if n == changed {
                        "changed\n".into()
                    } else {
                        format!("{n}\n")
                    }
                })
                .collect::<String>()
        };
        git(&["init", "-q"]);
        fs::write(repo.join("a.rs"), lines(0)).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        let posted = git(&["rev-parse", "HEAD"]);
        fs::write(repo.join("a.rs"), lines(20)).unwrap();
        git(&["commit", "-qam", "two"]);
        let head = git(&["rev-parse", "HEAD"]);
        let at = |line| Posted {
            key: format!("k{line}"),
            path: Some("a.rs".into()),
            line: None,
            url: Some(format!("u{line}")),
        };
        let spot = |line| Spot {
            commit: posted.clone(),
            line: Some(line),
        };
        let d = Discussion {
            state: Some("OPEN".into()),
            head: Some(head),
            posted: vec![at(19), at(5)],
            threads: vec![],
            spots: BTreeMap::from([("u19".into(), spot(19)), ("u5".into(), spot(5))]),
        };
        let o = record(repo, 9, &[], &d);
        let fates: Vec<(Option<u32>, Fate)> = o.findings.iter().map(|f| (f.line, f.fate)).collect();
        assert_eq!(fates, [(Some(19), Fate::Fixed), (Some(5), Fate::Open)]);
        assert_eq!(o.findings[0].posted_at.as_deref(), Some(posted.as_str()));
        assert_eq!(o.findings[1].why, "no change or answer yet");
    }

    #[test]
    fn the_summary_counts_a_finding_for_each_profile_that_raised_it() {
        let fs = [
            tracked(&["correctness", "testing"], Some("major"), Fate::Fixed),
            tracked(&["correctness"], Some("minor"), Fate::Dismissed),
            tracked(&[], None, Fate::Open),
        ];
        let s = Summary::of(&fs);
        assert_eq!(s.total.posted, 3);
        assert_eq!(
            s.by_profile["correctness"],
            Tally {
                posted: 2,
                fixed: 1,
                dismissed: 1,
                ignored: 0,
                open: 0
            }
        );
        assert_eq!(s.by_profile["testing"].fixed, 1);
        assert_eq!(s.by_profile["unknown"].open, 1);
        assert_eq!(s.by_severity["unknown"].posted, 1);
        assert_eq!(s.by_profile["correctness"].fixed_share(), Some(50));
        assert_eq!(s.by_profile["unknown"].fixed_share(), None);
    }

    #[test]
    fn the_table_shows_each_profile_and_severity() {
        let s = Summary::of(&[tracked(&["correctness"], Some("major"), Fate::Fixed)]);
        let table = render("lens: PR #7", &s);
        assert!(
            table.starts_with("lens: PR #7: 1 posted findings: 1 fixed, 0 dismissed"),
            "{table}"
        );
        assert!(table.contains("\n  correctness       1"), "{table}");
        assert!(table.contains("100%\n"), "{table}");
        assert!(table.contains("\n  major             1"), "{table}");
        assert_eq!(render("x", &Summary::default()).lines().count(), 1);
    }

    #[test]
    fn stored_outcomes_are_read_from_each_pr_folder_in_number_order() {
        let dir = tempfile::tempdir().unwrap();
        for n in [12u64, 3] {
            let work = dir.path().join(format!("pr-{n}"));
            fs::create_dir_all(&work).unwrap();
            let o = outcomes(n, false);
            fs::write(work.join(FILE), serde_json::to_string(&o).unwrap()).unwrap();
        }
        fs::create_dir_all(dir.path().join("pr-x")).unwrap();
        fs::create_dir_all(dir.path().join("main")).unwrap();
        let got: Vec<u64> = stored(dir.path()).unwrap().iter().map(|o| o.pr).collect();
        assert_eq!(got, [3, 12]);
        assert!(stored(&dir.path().join("missing")).unwrap().is_empty());
    }
}
