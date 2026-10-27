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

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::review::iso;

/// Where an agent's usage goes, and the repo it ran for.
#[derive(Debug, Clone)]
pub struct Log {
    pub dir: PathBuf,
    pub repo: String,
}

/// One agent run's use of one model.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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

/// Agents run on threads of one process; this keeps their lines whole.
static WRITE: Mutex<()> = Mutex::new(());

impl Log {
    /// `$LENS_USAGE_DIR`, else `~/.lens/usage`; `None` without a home.
    pub fn new(repo: &str) -> Option<Log> {
        let dir = match std::env::var_os("LENS_USAGE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".lens/usage"),
        };
        Some(Log {
            dir,
            repo: repo.to_string(),
        })
    }

    pub fn ledger(&self) -> PathBuf {
        self.dir.join("agents.jsonl")
    }

    pub fn readings(&self) -> PathBuf {
        self.dir.join("readings.jsonl")
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
}
