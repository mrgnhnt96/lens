//! `lens cost`: what reviews spent. lens's `claude -p` agents never show in session transcripts,
//! so the reports under `.review/` are the only record: this adds up their `costUsd`, agents and
//! findings per repo and per day.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::review::iso;

/// Directories never worth descending into when looking for `.review`.
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".dart_tool",
    "build",
    ".venv",
];

/// What one report spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub repo: String,
    /// `YYYY-MM-DD`, from the report's start time.
    pub day: String,
    pub cost: f64,
    pub agents: u64,
    pub findings: u64,
    /// Present only when the report carries them.
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Total {
    pub reports: u64,
    pub cost: f64,
    pub agents: u64,
    pub findings: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub has_cache: bool,
}

impl Total {
    fn add(&mut self, e: &Entry) {
        self.reports += 1;
        self.cost += e.cost;
        self.agents += e.agents;
        self.findings += e.findings;
        if e.cache_creation.is_some() || e.cache_read.is_some() {
            self.has_cache = true;
        }
        self.cache_creation += e.cache_creation.unwrap_or(0);
        self.cache_read += e.cache_read.unwrap_or(0);
    }
}

/// The earliest start time to count, as an ISO prefix: `1d` is 24 hours ago, a date is its midnight.
pub fn since_cutoff(since: &str, now_secs: u64) -> Result<String> {
    if let Some(n) = since.strip_suffix('d').and_then(|n| n.parse::<u64>().ok()) {
        return Ok(iso(now_secs.saturating_sub(n * 86400)));
    }
    let ok = since.len() == 10
        && since.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if ok {
        return Ok(since.to_string());
    }
    bail!("--since takes a span like 1d or a date like 2026-10-19, not {since:?}")
}

/// Reads one report; `None` when it isn't a lens report (no `costUsd`) or has no start time.
pub fn entry(repo: &str, report: &Value) -> Option<Entry> {
    let cost = report["costUsd"].as_f64()?;
    let started = report["startedAt"]
        .as_str()
        .or_else(|| report["finishedAt"].as_str())?;
    // `lens review` writes `totals`; the other spellings are older or hand-made reports.
    let cache = |total: &str, flat: &str, nested: &str| {
        report["totals"][total]
            .as_u64()
            .or_else(|| report[flat].as_u64())
            .or_else(|| report["cache"][nested].as_u64())
    };
    Some(Entry {
        repo: repo.to_string(),
        day: started.get(..10)?.to_string(),
        cost,
        agents: report["totals"]["agents"]
            .as_u64()
            .or_else(|| report["agents"].as_u64())
            .unwrap_or(0),
        findings: report["findings"].as_array().map_or(0, |f| f.len() as u64),
        cache_creation: cache(
            "cacheCreationInputTokens",
            "cacheCreationTokens",
            "creation",
        ),
        cache_read: cache("cacheReadInputTokens", "cacheReadTokens", "read"),
    })
}

/// Every report under `root`: any `.json` below a `.review` directory, the repo being the
/// directory that holds `.review`. The `prev/` archives of earlier passes are counted only with
/// `with_prev`.
pub fn collect(root: &Path, cutoff: Option<&str>, with_prev: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(root, None, cutoff, with_prev, &mut out);
    out
}

fn walk(
    dir: &Path,
    review_of: Option<&Path>,
    cutoff: Option<&str>,
    with_prev: bool,
    out: &mut Vec<Entry>,
) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if path.is_symlink()
                || SKIP.contains(&name)
                || (name == "prev" && review_of.is_some() && !with_prev)
            {
                continue;
            }
            match review_of {
                Some(repo) => walk(&path, Some(repo), cutoff, with_prev, out),
                None if name == ".review" => walk(&path, Some(dir), cutoff, with_prev, out),
                None => walk(&path, None, cutoff, with_prev, out),
            }
        } else if let (Some(repo), true) = (review_of, name.ends_with(".json")) {
            let Some(report) = fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            else {
                continue;
            };
            let label = repo.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let Some(e) = entry(label, &report) else {
                continue;
            };
            let started = report["startedAt"].as_str().unwrap_or(&e.day);
            if cutoff.is_none_or(|c| started >= c) {
                out.push(e);
            }
        }
    }
}

pub fn run(root: &Path, since: Option<&str>, with_prev: bool) -> Result<String> {
    let now = crate::agent::now_secs();
    let cutoff = since.map(|s| since_cutoff(s, now)).transpose()?;
    let entries = collect(root, cutoff.as_deref(), with_prev);
    Ok(render(root, cutoff.as_deref(), &entries))
}

fn group<'a>(entries: &'a [Entry], key: impl Fn(&'a Entry) -> &'a str) -> BTreeMap<&'a str, Total> {
    let mut map: BTreeMap<&str, Total> = BTreeMap::new();
    for e in entries {
        map.entry(key(e)).or_default().add(e);
    }
    map
}

fn table(title: &str, rows: Vec<(String, Total)>, all: &Total) -> Vec<String> {
    let width = rows
        .iter()
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0)
        .max(title.len())
        .max(5);
    let cache = all.has_cache;
    let line = |k: &str, t: &Total| {
        let mut s = format!(
            "{k:<width$}  {:>7}  {:>9.2}  {:>6}  {:>8}",
            t.reports, t.cost, t.agents, t.findings
        );
        if cache {
            s += &format!("  {:>14}  {:>12}", t.cache_creation, t.cache_read);
        }
        s
    };
    let mut head = format!(
        "{title:<width$}  {:>7}  {:>9}  {:>6}  {:>8}",
        "reports", "cost USD", "agents", "findings"
    );
    if cache {
        head += &format!("  {:>14}  {:>12}", "cache creation", "cache read");
    }
    let mut out = vec![head];
    out.extend(rows.iter().map(|(k, t)| line(k, t)));
    out.push(line("total", all));
    out
}

pub fn render(root: &Path, cutoff: Option<&str>, entries: &[Entry]) -> String {
    let mut all = Total::default();
    entries.iter().for_each(|e| all.add(e));
    let mut out = vec![format!(
        "Review spend under {}{}",
        root.display(),
        cutoff.map_or(String::new(), |c| format!(", since {c}"))
    )];
    if entries.is_empty() {
        out.push("No reports found.".into());
        return out.join("\n");
    }
    let rows = |m: BTreeMap<&str, Total>| -> Vec<(String, Total)> {
        m.into_iter().map(|(k, t)| (k.to_string(), t)).collect()
    };
    let mut by_repo = rows(group(entries, |e| &e.repo));
    by_repo.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost));
    out.push(String::new());
    out.extend(table("repo", by_repo, &all));
    out.push(String::new());
    out.extend(table("day", rows(group(entries, |e| &e.day)), &all));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(root: &Path, rel: &str, v: Value) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, v.to_string()).unwrap();
    }

    #[test]
    fn totals_per_repo_and_day_including_prev() {
        let dir = tempfile::tempdir().unwrap();
        let r = |cost: f64, at: &str| json!({"costUsd": cost, "agents": 2, "startedAt": at, "findings": [{}, {}]});
        write(
            dir.path(),
            "a/.review/feat/x.json",
            r(1.0, "2026-10-19T10:00:00Z"),
        );
        write(
            dir.path(),
            "a/.review/feat/x/prev/report.json",
            r(0.5, "2026-10-19T09:00:00Z"),
        );
        write(
            dir.path(),
            "b/.review/y.json",
            r(2.0, "2026-10-20T01:00:00Z"),
        );
        write(
            dir.path(),
            "b/.review/y/profiles.json",
            json!({"profiles": []}),
        );
        write(
            dir.path(),
            "b/node_modules/z/.review/q.json",
            r(9.0, "2026-10-20T01:00:00Z"),
        );
        let e = collect(dir.path(), None, true);
        assert_eq!(e.len(), 3);
        assert_eq!(collect(dir.path(), None, false).len(), 2);
        let by = group(&e, |e| &e.repo);
        assert_eq!(by["a"].cost, 1.5);
        assert_eq!(by["a"].findings, 4);
        assert_eq!(group(&e, |e| &e.day)["2026-10-19"].reports, 2);
        assert!(!by["a"].has_cache);
        let cut = collect(dir.path(), Some("2026-10-20"), true);
        assert_eq!(cut.len(), 1);
    }

    #[test]
    fn cache_totals_when_present() {
        let e = entry(
            "a",
            &json!({"costUsd": 1.0, "startedAt": "2026-10-19T10:00:00Z",
                    "cacheCreationTokens": 10, "cache": {"read": 5}}),
        )
        .unwrap();
        assert_eq!((e.cache_creation, e.cache_read), (Some(10), Some(5)));
        let text = render(Path::new("/r"), None, &[e]);
        assert!(text.contains("cache read"));
    }

    #[test]
    fn reads_the_totals_lens_review_writes() {
        let e = entry(
            "a",
            &json!({"costUsd": 1.0, "startedAt": "2026-10-19T10:00:00Z",
                    "agents": [{"label": "reviewer-correctness"}],
                    "totals": {"agents": 4, "costUsd": 1.0,
                               "cacheCreationInputTokens": 30, "cacheReadInputTokens": 20}}),
        )
        .unwrap();
        assert_eq!(e.agents, 4);
        assert_eq!((e.cache_creation, e.cache_read), (Some(30), Some(20)));
    }

    #[test]
    fn since_forms() {
        assert_eq!(
            since_cutoff("1d", 86400 * 3).unwrap(),
            "1970-01-03T00:00:00Z"
        );
        assert_eq!(since_cutoff("2026-10-19", 0).unwrap(), "2026-10-19");
        assert!(since_cutoff("yesterday", 0).is_err());
    }
}
