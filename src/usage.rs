//! What lens's agents spend of the Claude plan, kept across runs in `~/.lens/usage/` (or
//! `$LENS_USAGE_DIR`), one JSON object per line:
//!
//! - `agents.jsonl`, the ledger: a row per agent run and model, with its tokens and cost, from the
//!   `result` event's `modelUsage`.
//! - `readings.jsonl`: every reading of the plan's 5-hour and 7-day windows lens sees, from the
//!   `rate_limit_event`s of its agents' streams.
//!
//! Many sessions share the windows, so a run's own before and after readings say little; the
//! ledger and readings are kept side by side so a fit over many gaps can tell what lens's share is.
//!
//! `lens usage` reads them back: what lens spent, how far the windows moved, and what that says
//! about how much of a window lens's dollars take.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cost::since_cutoff;
use crate::review::{iso, ROLES};

/// Where an agent's usage goes, and the repo it ran for.
#[derive(Debug, Clone)]
pub struct Log {
    pub dir: PathBuf,
    pub repo: String,
}

/// One agent run's use of one model.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Row {
    pub started_at: String,
    pub finished_at: String,
    pub repo: String,
    pub label: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
}

/// One window's utilization as the server reported it.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Reading {
    pub at: String,
    /// `five_hour` or `seven_day`.
    pub window: String,
    /// The share of the window used, 0 to 1.
    pub utilization: f64,
    pub resets_at: String,
    pub status: String,
    pub repo: String,
    pub label: String,
}

const LEDGER: &str = "agents.jsonl";
const READINGS: &str = "readings.jsonl";

/// Agents run on threads of one process; this keeps their lines whole.
static WRITE: Mutex<()> = Mutex::new(());

/// `$LENS_USAGE_DIR`, else `~/.lens/usage`; `None` without a home.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("LENS_USAGE_DIR") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".lens/usage")),
    }
}

impl Log {
    pub fn new(repo: &str) -> Option<Log> {
        Some(Log {
            dir: dir()?,
            repo: repo.to_string(),
        })
    }

    pub fn ledger(&self) -> PathBuf {
        self.dir.join(LEDGER)
    }

    pub fn readings(&self) -> PathBuf {
        self.dir.join(READINGS)
    }

    /// Records a `rate_limit_event`'s window readings, taken at `at`.
    pub fn record_readings(&self, label: &str, at: &str, event: &Value) {
        let rows = readings(&self.repo, label, at, event);
        self.append(self.readings(), &rows);
    }

    /// Records a `result` event's usage for an agent run between `started_at` and `finished_at`.
    pub fn record_result(
        &self,
        label: &str,
        model: &str,
        started_at: &str,
        finished_at: &str,
        result: &Value,
    ) {
        let rows = rows(&self.repo, label, model, started_at, finished_at, result);
        self.append(self.ledger(), &rows);
    }

    /// Usage is a side record: failing to write it never fails the review.
    fn append(&self, path: PathBuf, rows: &[impl Serialize]) {
        if rows.is_empty() {
            return;
        }
        let mut text = String::new();
        for row in rows {
            if let Ok(line) = serde_json::to_string(row) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// The ledger rows of a `result` event: one per model in `modelUsage`, else one for `model` from
/// the run's `usage` and `total_cost_usd`.
pub fn rows(
    repo: &str,
    label: &str,
    model: &str,
    started_at: &str,
    finished_at: &str,
    result: &Value,
) -> Vec<Row> {
    let row = |model: &str, tokens: [u64; 4], cost_usd: f64| {
        let [input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens] =
            tokens;
        Row {
            started_at: started_at.to_string(),
            finished_at: finished_at.to_string(),
            repo: repo.to_string(),
            label: label.to_string(),
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cost_usd,
        }
    };
    let n = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0);
    if let Some(models) = result["modelUsage"].as_object().filter(|m| !m.is_empty()) {
        return models
            .iter()
            .map(|(model, u)| {
                let tokens = [
                    n(u, "inputTokens"),
                    n(u, "outputTokens"),
                    n(u, "cacheCreationInputTokens"),
                    n(u, "cacheReadInputTokens"),
                ];
                row(model, tokens, u["costUSD"].as_f64().unwrap_or(0.0))
            })
            .collect();
    }
    let u = &result["usage"];
    if !u.is_object() {
        return Vec::new();
    }
    let tokens = [
        n(u, "input_tokens"),
        n(u, "output_tokens"),
        n(u, "cache_creation_input_tokens"),
        n(u, "cache_read_input_tokens"),
    ];
    vec![row(
        model,
        tokens,
        result["total_cost_usd"].as_f64().unwrap_or(0.0),
    )]
}

/// The window readings of a `rate_limit_event`, from `rate_limit_info.unifiedWindows`.
pub fn readings(repo: &str, label: &str, at: &str, event: &Value) -> Vec<Reading> {
    let Some(windows) = event["rate_limit_info"]["unifiedWindows"].as_object() else {
        return Vec::new();
    };
    windows
        .iter()
        .filter_map(|(window, w)| {
            Some(Reading {
                at: at.to_string(),
                window: window.clone(),
                utilization: w["utilization"].as_f64()?,
                resets_at: w["resetsAt"].as_u64().map(iso).unwrap_or_default(),
                status: w["status"].as_str().unwrap_or("").to_string(),
                repo: repo.to_string(),
                label: label.to_string(),
            })
        })
        .collect()
}

/// The least a window must rise across a stretch of readings before its lens spend is read as a
/// bound: utilization comes rounded, so smaller rises are mostly rounding.
const MIN_RISE: f64 = 0.02;

/// Seconds since the epoch of an `iso` time; `None` when it isn't one.
pub fn secs(iso: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm), the inverse of `iso`.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Every window reading recorded in `dir`.
pub fn recorded(dir: &Path) -> Vec<Reading> {
    load(&dir.join(READINGS))
}

/// Every line of a log that parses; a missing log is empty.
fn load<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// An agent's role from its label: `reviewer-correctness` and `sweep-3-reviewer-x` are reviewers.
pub fn role(label: &str) -> &str {
    label
        .split('-')
        .find(|part| ROLES.contains(part))
        .unwrap_or_else(|| label.split('-').next().unwrap_or(label))
}

/// What a group of ledger rows spent.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Spend {
    /// Agent runs, not rows: a run that used two models is one run.
    pub runs: u64,
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost: f64,
}

/// `rows` grouped by `key`, most spent first.
pub fn spend<'a>(rows: &'a [Row], key: impl Fn(&'a Row) -> &'a str) -> Vec<(String, Spend)> {
    // A run is its repo, label and start time.
    type Runs<'a> = BTreeSet<(&'a str, &'a str, &'a str)>;
    let mut groups: BTreeMap<&str, (Spend, Runs)> = BTreeMap::new();
    for r in rows {
        let (s, runs) = groups.entry(key(r)).or_default();
        runs.insert((&r.repo, &r.label, &r.started_at));
        s.input += r.input_tokens;
        s.output += r.output_tokens;
        s.cache_creation += r.cache_creation_input_tokens;
        s.cache_read += r.cache_read_input_tokens;
        s.cost += r.cost_usd;
    }
    let mut out: Vec<(String, Spend)> = groups
        .into_iter()
        .map(|(k, (mut s, runs))| {
            s.runs = runs.len() as u64;
            (k.to_string(), s)
        })
        .collect();
    out.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost));
    out
}

/// What lens spent between two readings, taking the share of each run that fell between them.
pub fn spent_between(rows: &[Row], from: u64, to: u64) -> f64 {
    rows.iter()
        .filter_map(|r| {
            let (s, f) = (secs(&r.started_at)?, secs(&r.finished_at)?);
            let share = if f <= s {
                f64::from(u8::from(from < f && f <= to))
            } else {
                to.min(f).saturating_sub(from.max(s)) as f64 / (f - s) as f64
            };
            Some(r.cost_usd * share)
        })
        .sum()
}

/// How one window moved over the span, and what lens spent while it did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Window {
    pub name: String,
    pub first: Reading,
    pub last: Reading,
    pub peak: f64,
    /// The rises added up, each period counted from 0 after a reset.
    pub risen: f64,
    pub resets: u64,
    /// Stretches of readings that rose at least `MIN_RISE`, as (lens's spend, the rise).
    pub stretches: Vec<(f64, f64)>,
}

impl Window {
    /// The least a full window can hold in lens's dollars, per 1%: every stretch's rise includes
    /// whatever else used the plan, so its lens spend over its rise is a floor. Pooled over every
    /// stretch: single stretches rest on whole-percent readings, so the tightest of them is noise.
    pub fn floor(&self) -> Option<f64> {
        let (cost, rise) = self
            .stretches
            .iter()
            .fold((0.0, 0.0), |(c, r), (sc, sr)| (c + sc, r + sr));
        if rise <= 0.0 || cost <= 0.0 {
            return None;
        }
        Some(cost / (rise * 100.0))
    }
}

/// One window's readings, in time order, against the ledger.
pub fn window(name: &str, readings: &[&Reading], rows: &[Row]) -> Option<Window> {
    let (first, last) = (*readings.first()?, *readings.last()?);
    let mut w = Window {
        name: name.to_string(),
        first: first.clone(),
        last: last.clone(),
        peak: readings.iter().map(|r| r.utilization).fold(0.0, f64::max),
        ..Window::default()
    };
    let mut anchor = first.utilization;
    let mut anchor_at = first;
    let mut top = first.utilization;
    for pair in readings.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        if next.resets_at != prev.resets_at {
            w.resets += 1;
            w.risen += next.utilization;
            anchor = next.utilization;
            anchor_at = next;
            top = next.utilization;
            continue;
        }
        // Out-of-step readings from parallel agents dip slightly under the same
        // resets_at; those are not resets, so only rises above the running max count.
        if next.utilization <= top {
            continue;
        }
        w.risen += next.utilization - top;
        top = next.utilization;
        let rise = top - anchor;
        if rise + 1e-9 >= MIN_RISE {
            if let (Some(from), Some(to)) = (secs(&anchor_at.at), secs(&next.at)) {
                w.stretches.push((spent_between(rows, from, to), rise));
            }
            anchor = top;
            anchor_at = next;
        }
    }
    Some(w)
}

/// The usage report for everything at or after `cutoff`.
pub fn report(dir: &Path, cutoff: Option<&str>, now: u64) -> String {
    let after = |at: &str| cutoff.is_none_or(|c| at >= c);
    let rows: Vec<Row> = load::<Row>(&dir.join(LEDGER))
        .into_iter()
        .filter(|r| after(&r.started_at))
        .collect();
    let mut readings: Vec<Reading> = load::<Reading>(&dir.join(READINGS))
        .into_iter()
        .filter(|r| after(&r.at))
        .collect();
    readings.sort_by(|a, b| a.at.cmp(&b.at));
    render(dir, cutoff, &rows, &readings, now)
}

pub fn run(dir: &Path, since: Option<&str>, now_secs: u64) -> Result<String> {
    let cutoff = since.map(|s| since_cutoff(s, now_secs)).transpose()?;
    Ok(report(dir, cutoff.as_deref(), now_secs))
}

fn table(title: &str, groups: Vec<(String, Spend)>, all: &Spend) -> Vec<String> {
    let width = groups
        .iter()
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0)
        .max(title.len())
        .max(5);
    let line = |k: &str, s: &Spend| {
        format!(
            "{k:<width$}  {:>5}  {:>9.2}  {:>10}  {:>10}  {:>12}  {:>12}",
            s.runs, s.cost, s.input, s.output, s.cache_creation, s.cache_read
        )
    };
    let mut out = vec![format!(
        "{title:<width$}  {:>5}  {:>9}  {:>10}  {:>10}  {:>12}  {:>12}",
        "runs", "cost USD", "input", "output", "cache write", "cache read"
    )];
    out.extend(groups.iter().map(|(k, s)| line(k, s)));
    out.push(line("total", all));
    out
}

/// A run that was turned away: it spent no tokens, so it has no model worth a row.
fn rejected(r: &Row) -> bool {
    r.input_tokens + r.output_tokens + r.cache_creation_input_tokens + r.cache_read_input_tokens
        == 0
}

/// `t` as time from `now`: `3h39m ago`, `in 2h05m`, `in 4d3h`; `t` itself when it isn't a time.
fn relative(t: &str, now: u64) -> String {
    let Some(at) = secs(t) else {
        return t.to_string();
    };
    let d = at.abs_diff(now);
    let span = match (d / 86400, d / 3600 % 24, d / 60 % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (days, h, _) => format!("{days}d{h}h"),
    };
    if at > now {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

fn pct(u: f64) -> String {
    format!("{:.0}%", u * 100.0)
}

pub fn render(
    dir: &Path,
    cutoff: Option<&str>,
    rows: &[Row],
    readings: &[Reading],
    now: u64,
) -> String {
    let mut out = vec![format!(
        "lens usage in {}{}",
        dir.display(),
        cutoff.map_or(String::new(), |c| format!(", since {c}"))
    )];
    if rows.is_empty() && readings.is_empty() {
        out.push("Nothing recorded yet.".into());
        return out.join("\n");
    }
    if let Some((_, all)) = spend(rows, |_| "total").pop() {
        let start = rows.iter().map(|r| r.started_at.as_str()).min();
        let end = rows.iter().map(|r| r.finished_at.as_str()).max();
        if let (Some(start), Some(end)) = (start, end) {
            out.push(format!(
                "spend from {start} ({}) to {end} ({})",
                relative(start, now),
                relative(end, now)
            ));
        }
        out.push(String::new());
        out.extend(table("repo", spend(rows, |r| &r.repo), &all));
        out.push(String::new());
        out.extend(table("role", spend(rows, |r| role(&r.label)), &all));
        out.push(String::new());
        let used: Vec<Row> = rows.iter().filter(|r| !rejected(r)).cloned().collect();
        if let Some((_, used_all)) = spend(&used, |_| "total").pop() {
            out.extend(table("model", spend(&used, |r| &r.model), &used_all));
        }
        let turned_away = rows.iter().filter(|r| rejected(r)).collect::<Vec<_>>();
        if !turned_away.is_empty() {
            let runs = spend(
                &turned_away.iter().map(|r| (*r).clone()).collect::<Vec<_>>(),
                |_| "rejected",
            )[0]
            .1
            .runs;
            out.push(format!(
                "rejected runs (no tokens, not in the model table): {runs}"
            ));
        }
    }
    let names: BTreeSet<&str> = readings.iter().map(|r| r.window.as_str()).collect();
    for name in names {
        let of: Vec<&Reading> = readings.iter().filter(|r| r.window == name).collect();
        let Some(w) = window(name, &of, rows) else {
            continue;
        };
        out.push(String::new());
        out.push(format!(
            "{name}: {} at {} ({}) -> {} at {} ({}) (peak {}, rose {} in all{}), resets {} ({})",
            pct(w.first.utilization),
            w.first.at,
            relative(&w.first.at, now),
            pct(w.last.utilization),
            w.last.at,
            relative(&w.last.at, now),
            pct(w.peak),
            pct(w.risen),
            match w.resets {
                0 => String::new(),
                1 => ", across 1 reset".into(),
                n => format!(", across {n} resets"),
            },
            w.last.resets_at,
            relative(&w.last.resets_at, now),
        ));
        match w.floor() {
            Some(pooled) => {
                let left = (1.0 - w.last.utilization).max(0.0) * 100.0 * pooled;
                let n = w.stretches.len();
                out.push(format!(
                    "  1% of it holds at least ${pooled:.3} of lens work (pooled over {n} stretch{})",
                    if n == 1 { "" } else { "es" }
                ));
                out.push(format!(
                    "  so the {} left holds at least ${left:.2} of lens work (same pooled figure), if nothing else ran",
                    pct(1.0 - w.last.utilization)
                ));
            }
            None => out.push(format!(
                "  too little lens spend over a rise of {}+ to tell what 1% holds",
                pct(MIN_RISE)
            )),
        }
    }
    out.push(String::new());
    out.push(
        "Other sessions share the windows, so the $ per 1% are floors: lens's real share of a window is at most its spend over these."
            .into(),
    );
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rate_limit_event() -> Value {
        json!({"type": "rate_limit_event", "rate_limit_info": {
            "status": "allowed", "resetsAt": 1793125800, "rateLimitType": "five_hour",
            "unifiedWindows": {
                "five_hour": {"utilization": 0.68, "resetsAt": 1793125800, "status": "allowed"},
                "seven_day": {"utilization": 0.55, "resetsAt": 1793466000, "status": "allowed"}}}})
    }

    #[test]
    fn a_rate_limit_event_reads_both_windows() {
        let r = readings(
            "lens",
            "reviewer-x",
            "2026-10-27T12:00:00Z",
            &rate_limit_event(),
        );
        assert_eq!(r.len(), 2);
        let five = r.iter().find(|r| r.window == "five_hour").unwrap();
        assert_eq!(five.utilization, 0.68);
        assert_eq!(five.resets_at, iso(1793125800));
        assert_eq!(five.status, "allowed");
        assert_eq!(five.at, "2026-10-27T12:00:00Z");
        assert!(readings("lens", "x", "t", &json!({"rate_limit_info": {}})).is_empty());
    }

    #[test]
    fn a_result_gives_a_row_per_model() {
        let result = json!({"type": "result", "total_cost_usd": 0.5,
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "modelUsage": {
                "claude-sonnet-5-5": {"inputTokens": 2, "outputTokens": 4,
                    "cacheReadInputTokens": 649, "cacheCreationInputTokens": 3008, "costUSD": 0.4},
                "claude-haiku-5-5": {"inputTokens": 1, "outputTokens": 1, "costUSD": 0.1}}});
        let rows = rows("lens", "reviewer-x", "sonnet", "s", "f", &result);
        assert_eq!(rows.len(), 2);
        let sonnet = rows
            .iter()
            .find(|r| r.model == "claude-sonnet-5-5")
            .unwrap();
        assert_eq!(
            (
                sonnet.input_tokens,
                sonnet.output_tokens,
                sonnet.cache_creation_input_tokens,
                sonnet.cache_read_input_tokens
            ),
            (2, 4, 3008, 649)
        );
        assert_eq!(sonnet.cost_usd, 0.4);
        assert_eq!(
            (sonnet.started_at.as_str(), sonnet.finished_at.as_str()),
            ("s", "f")
        );
    }

    #[test]
    fn a_result_without_model_usage_falls_back_to_its_totals() {
        let result = json!({"total_cost_usd": 0.25,
            "usage": {"input_tokens": 3, "output_tokens": 5, "cache_read_input_tokens": 7}});
        let rows = rows("lens", "x", "sonnet", "s", "f", &result);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "sonnet");
        assert_eq!(
            (rows[0].input_tokens, rows[0].cache_read_input_tokens),
            (3, 7)
        );
        assert_eq!(rows[0].cost_usd, 0.25);
        assert!(super::rows("lens", "x", "m", "s", "f", &json!({})).is_empty());
    }

    #[test]
    fn records_append_one_line_each() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log {
            dir: dir.path().join("usage"),
            repo: "lens".into(),
        };
        log.record_readings("x", "t1", &rate_limit_event());
        log.record_readings("x", "t2", &rate_limit_event());
        let result = json!({"modelUsage": {"m": {"inputTokens": 1, "costUSD": 0.1}}});
        log.record_result("x", "m", "s", "f", &result);
        let lines = |p: PathBuf| -> Vec<Value> {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        let readings = lines(log.readings());
        assert_eq!(readings.len(), 4);
        assert_eq!(readings[3]["at"], "t2");
        assert_eq!(readings[0]["resetsAt"], iso(1793125800));
        let ledger = lines(log.ledger());
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["repo"], "lens");
        assert_eq!(ledger[0]["costUsd"], 0.1);
    }

    fn row(label: &str, model: &str, start: &str, finish: &str, cost: f64) -> Row {
        Row {
            started_at: start.into(),
            finished_at: finish.into(),
            repo: "lens".into(),
            label: label.into(),
            model: model.into(),
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: cost,
            ..Row::default()
        }
    }

    fn reading(at: &str, u: f64, resets_at: &str) -> Reading {
        Reading {
            at: at.into(),
            window: "five_hour".into(),
            utilization: u,
            resets_at: resets_at.into(),
            ..Reading::default()
        }
    }

    #[test]
    fn secs_inverts_iso() {
        for t in [0, 86399, 951_782_400, 1_793_125_800] {
            assert_eq!(secs(&iso(t)), Some(t));
        }
        assert_eq!(secs("2026-10"), None);
    }

    #[test]
    fn roles_from_labels() {
        assert_eq!(role("reviewer-correctness"), "reviewer");
        assert_eq!(role("pr-3-verifier"), "verifier");
        assert_eq!(role("mapper"), "mapper");
    }

    #[test]
    fn spend_counts_a_two_model_run_once() {
        let rows = [
            row(
                "reviewer-a",
                "sonnet",
                "2026-10-27T10:00:00Z",
                "2026-10-27T10:01:00Z",
                0.4,
            ),
            row(
                "reviewer-a",
                "haiku",
                "2026-10-27T10:00:00Z",
                "2026-10-27T10:01:00Z",
                0.1,
            ),
            row(
                "verifier-1",
                "sonnet",
                "2026-10-27T10:02:00Z",
                "2026-10-27T10:03:00Z",
                1.0,
            ),
        ];
        let by_role = spend(&rows, |r| role(&r.label));
        assert_eq!(by_role[0].0, "verifier");
        assert_eq!(by_role[1].1.runs, 1);
        assert!((by_role[1].1.cost - 0.5).abs() < 1e-9);
        assert_eq!(by_role[1].1.input, 20);
        let by_model = spend(&rows, |r| &r.model);
        assert_eq!(by_model[0].1.runs, 2);
    }

    #[test]
    fn spend_between_takes_the_overlapping_share() {
        let rows = [row("x", "m", &iso(100), &iso(200), 1.0)];
        assert!((spent_between(&rows, 150, 300) - 0.5).abs() < 1e-9);
        assert_eq!(spent_between(&rows, 200, 300), 0.0);
        let instant = [row("x", "m", &iso(100), &iso(100), 1.0)];
        assert_eq!(spent_between(&instant, 50, 100), 1.0);
        assert_eq!(spent_between(&instant, 100, 150), 0.0);
    }

    #[test]
    fn a_small_dip_under_the_same_reset_is_not_a_reset() {
        let r = "2026-10-27T15:00:00Z";
        let readings = [
            reading(&iso(0), 0.50, r),
            reading(&iso(10), 0.49, r),
            reading(&iso(20), 0.51, r),
        ];
        let refs: Vec<&Reading> = readings.iter().collect();
        let w = window("five_hour", &refs, &[]).unwrap();
        assert_eq!(w.resets, 0);
        // Only the climb past the earlier 0.50 counts, not the dip's recovery.
        assert!((w.risen - 0.01).abs() < 1e-9);
    }

    #[test]
    fn a_changed_reset_time_is_a_reset() {
        let readings = [
            reading(&iso(0), 0.81, "2026-10-27T18:30:00Z"),
            reading(&iso(10), 1.0, "2026-10-27T18:30:00Z"),
            reading(&iso(20), 0.0, "2026-10-27T23:30:00Z"),
            reading(&iso(30), 0.23, "2026-10-27T23:30:00Z"),
        ];
        let refs: Vec<&Reading> = readings.iter().collect();
        let w = window("five_hour", &refs, &[]).unwrap();
        assert_eq!(w.resets, 1);
        assert!((w.risen - 0.42).abs() < 1e-9);
    }

    #[test]
    fn a_dip_inside_a_stretch_does_not_re_anchor_it() {
        let r = "2026-10-27T15:00:00Z";
        let readings = [
            reading(&iso(0), 0.10, r),
            reading(&iso(10), 0.08, r),
            reading(&iso(20), 0.13, r),
        ];
        let refs: Vec<&Reading> = readings.iter().collect();
        let rows = [row("x", "m", &iso(0), &iso(20), 3.0)];
        let w = window("five_hour", &refs, &rows).unwrap();
        // One stretch from the first reading, rising 0.03 from 0.10, not 0.05 from the dip.
        assert_eq!(w.stretches.len(), 1);
        let (spent, rise) = w.stretches[0];
        assert!((spent - 3.0).abs() < 1e-9);
        assert!((rise - 0.03).abs() < 1e-9);
    }

    #[test]
    fn a_reset_re_bases_the_stretch_at_the_post_reset_reading() {
        let readings = [
            reading(&iso(0), 0.50, "2026-10-27T15:00:00Z"),
            reading(&iso(100), 0.90, "2026-10-27T15:00:00Z"),
            reading(&iso(200), 0.05, "2026-10-27T20:00:00Z"),
            reading(&iso(300), 0.08, "2026-10-27T20:00:00Z"),
        ];
        let refs: Vec<&Reading> = readings.iter().collect();
        // The $100 row sits between the old stretch and the post-reset reading.
        let rows = [
            row("x", "m", &iso(0), &iso(100), 1.0),
            row("x", "m", &iso(100), &iso(200), 100.0),
            row("x", "m", &iso(200), &iso(300), 3.0),
        ];
        let w = window("five_hour", &refs, &rows).unwrap();
        assert_eq!(w.stretches.len(), 2);
        let (spent, rise) = w.stretches[1];
        assert!((spent - 3.0).abs() < 1e-9, "spent {spent}");
        assert!((rise - 0.03).abs() < 1e-9);
    }

    #[test]
    fn a_window_bounds_what_one_percent_holds() {
        let r = "2026-10-27T15:00:00Z";
        let readings = [
            reading(&iso(0), 0.10, r),
            reading(&iso(50), 0.11, r),
            reading(&iso(100), 0.12, r),
            reading(&iso(200), 0.20, r),
            // A reset: the next period counts from 0.
            reading(&iso(300), 0.03, "2026-10-27T20:00:00Z"),
        ];
        let refs: Vec<&Reading> = readings.iter().collect();
        let rows = [row("x", "m", &iso(0), &iso(200), 4.0)];
        let w = window("five_hour", &refs, &rows).unwrap();
        assert_eq!(w.resets, 1);
        assert!((w.risen - 0.13).abs() < 1e-9);
        assert_eq!(w.peak, 0.20);
        // 0.10 -> 0.12 over 0..100 ($2), then 0.12 -> 0.20 over 100..200 ($2).
        assert_eq!(w.stretches.len(), 2);
        assert!((w.floor().unwrap() - 0.4).abs() < 1e-9);
        let flat = [reading(&iso(0), 0.5, r), reading(&iso(9), 0.5, r)];
        let refs: Vec<&Reading> = flat.iter().collect();
        assert!(window("five_hour", &refs, &rows).unwrap().floor().is_none());
    }

    #[test]
    fn report_reads_the_logs_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(report(dir.path(), None, 0).contains("Nothing recorded yet."));
        let log = Log {
            dir: dir.path().into(),
            repo: "lens".into(),
        };
        let result =
            json!({"modelUsage": {"claude-sonnet-5-5": {"inputTokens": 1, "costUSD": 3.0}}});
        log.record_result("reviewer-a", "sonnet", &iso(1000), &iso(2000), &result);
        let event = |u: f64| {
            json!({"rate_limit_info": {"unifiedWindows": {
            "five_hour": {"utilization": u, "resetsAt": 1793125800}}}})
        };
        log.record_readings("reviewer-a", &iso(1000), &event(0.40));
        log.record_readings("reviewer-a", &iso(2000), &event(0.50));
        let text = report(dir.path(), None, 3000);
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains("five_hour: 40%"), "{text}");
        assert!(text.contains("1% of it holds at least $0.300"), "{text}");
        assert!(text.contains("50% left holds at least $15.00"), "{text}");
        assert!(report(dir.path(), Some("2100"), 0).contains("Nothing recorded yet."));
    }

    #[test]
    fn times_read_relative_to_now() {
        let now = 1_000_000;
        assert_eq!(relative(&iso(now - 13_140), now), "3h39m ago");
        assert_eq!(relative(&iso(now + 7_500), now), "in 2h05m");
        assert_eq!(relative(&iso(now + 4 * 86400 + 3 * 3600), now), "in 4d3h");
        assert_eq!(relative(&iso(now - 300), now), "5m ago");
        assert_eq!(relative("nope", now), "nope");
    }

    #[test]
    fn the_report_names_its_span_and_counts_rejected_runs_apart() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log {
            dir: dir.path().into(),
            repo: "lens".into(),
        };
        let used = json!({"modelUsage": {"claude-sonnet-5-5": {"inputTokens": 1, "costUSD": 3.0}}});
        log.record_result("reviewer-a", "sonnet", &iso(1000), &iso(2000), &used);
        let none = json!({"usage": {"input_tokens": 0, "output_tokens": 0}});
        log.record_result("reviewer-b", "sonnet", &iso(2100), &iso(2100), &none);
        let event = |u: f64, resets: u64| {
            json!({"rate_limit_info": {"unifiedWindows": {
            "five_hour": {"utilization": u, "resetsAt": resets}}}})
        };
        log.record_readings("x", &iso(1000), &event(0.40, 9000));
        log.record_readings("x", &iso(2000), &event(0.50, 9000));
        let text = report(dir.path(), None, 3000);
        assert!(
            text.contains(&format!("spend from {}", iso(1000))),
            "{text}"
        );
        assert!(text.contains("to 1970-01-01T00:35:00Z (15m ago)"), "{text}");
        assert!(
            text.contains("rejected runs (no tokens, not in the model table): 1"),
            "{text}"
        );
        let model = text
            .split("\nmodel")
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap();
        assert_eq!(model.matches("sonnet").count(), 1, "{text}");
        assert!(
            text.contains("resets 1970-01-01T02:30:00Z (in 1h40m)"),
            "{text}"
        );
        assert!(text.contains("pooled over 1 stretch)"), "{text}");
    }

    #[test]
    fn a_report_of_only_rejected_runs_has_no_model_table() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log {
            dir: dir.path().into(),
            repo: "lens".into(),
        };
        let none = json!({"usage": {"input_tokens": 0, "output_tokens": 0}});
        log.record_result("reviewer-a", "sonnet", &iso(1000), &iso(1000), &none);
        log.record_result("reviewer-b", "sonnet", &iso(2000), &iso(2000), &none);
        let text = report(dir.path(), None, 3000);
        assert!(
            text.contains("rejected runs (no tokens, not in the model table): 2"),
            "{text}"
        );
        assert!(!text.contains("\nmodel"), "{text}");
    }

    #[test]
    fn the_report_pluralises_stretches() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log {
            dir: dir.path().into(),
            repo: "lens".into(),
        };
        let used = json!({"modelUsage": {"claude-sonnet-5-5": {"inputTokens": 1, "costUSD": 1.0}}});
        log.record_result("reviewer-a", "sonnet", &iso(0), &iso(100), &used);
        log.record_result("reviewer-a", "sonnet", &iso(200), &iso(300), &used);
        let event = |u: f64, resets: u64| {
            json!({"rate_limit_info": {"unifiedWindows": {
            "five_hour": {"utilization": u, "resetsAt": resets}}}})
        };
        log.record_readings("x", &iso(0), &event(0.10, 9000));
        log.record_readings("x", &iso(100), &event(0.20, 9000));
        log.record_readings("x", &iso(200), &event(0.05, 18000));
        log.record_readings("x", &iso(300), &event(0.15, 18000));
        let text = report(dir.path(), None, 400);
        assert!(text.contains("pooled over 2 stretches)"), "{text}");
    }

    #[test]
    fn per_one_percent_and_left_agree_when_rounded_once() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log {
            dir: dir.path().into(),
            repo: "lens".into(),
        };
        let result = json!({"modelUsage": {"m": {"inputTokens": 1, "costUSD": 1.21}}});
        log.record_result("reviewer-a", "m", &iso(1000), &iso(2000), &result);
        let event = |u: f64| {
            json!({"rate_limit_info": {"unifiedWindows": {
            "five_hour": {"utilization": u, "resetsAt": 9000}}}})
        };
        log.record_readings("x", &iso(1000), &event(0.10));
        log.record_readings("x", &iso(2000), &event(0.12));
        let text = report(dir.path(), None, 3000);
        assert!(text.contains("holds at least $0.605 "), "{text}");
        assert!(text.contains("88% left holds at least $53.24"), "{text}");
    }
}
