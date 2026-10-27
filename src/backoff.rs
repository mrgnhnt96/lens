//! `[backoff]`: lens reviewing less as the plan's usage windows fill. It goes by the window
//! readings the usage log keeps, so it acts on what the plan last reported.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::agent::clock;
use crate::config::{BackoffConfig, WindowBackoff};
use crate::usage::{self, Reading};

/// Printed by the first review after every step lifts.
pub const LIFTED: &str =
    "backoff lifted: the plan's windows are under their [backoff] limits again; reviewing as usual";

/// Which steps held at the last check, per repo, so the next can say when they lift.
const STATE: &str = "backoff.json";

/// Where a window stands: how much of it is used and when it resets.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub utilization: f64,
    /// Seconds since the epoch.
    pub resets_at: u64,
}

/// How lens holds back now.
#[derive(Debug, Default, PartialEq)]
pub struct Throttle {
    /// Why lens starts no review, and when the window that says so resets.
    pub pause: Option<(String, u64)>,
    /// Why a review runs the correctness reviewer only.
    pub lighter: Option<String>,
    /// The share of PRs `lens review --pr` still reviews, and why.
    pub share: Option<(f64, String)>,
    /// Whether this check found nothing holding where the last one found a step.
    pub lifted: bool,
}

impl Throttle {
    fn steps(&self) -> Vec<String> {
        [
            ("pause", self.pause.is_some()),
            ("lighter", self.lighter.is_some()),
            ("share", self.share.is_some()),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(step, _)| step.to_string())
        .collect()
    }
}

/// Window `name` now. Every reading that hasn't reset yet is of the current window; the most any
/// of them read is where it stands, since a later reading can dip by rounding.
pub fn level(readings: &[Reading], name: &str, now: u64) -> Option<Level> {
    readings
        .iter()
        .filter(|r| r.window == name)
        .filter_map(|r| {
            Some(Level {
                utilization: r.utilization,
                resets_at: usage::secs(&r.resets_at)?,
            })
        })
        .filter(|l| l.resets_at > now)
        .reduce(|a, b| Level {
            utilization: a.utilization.max(b.utilization),
            resets_at: a.resets_at.max(b.resets_at),
        })
}

/// How far `level` runs ahead of an even pace through a window `len` seconds long.
fn ahead(level: &Level, len: u64, now: u64) -> f64 {
    let left = level.resets_at.saturating_sub(now).min(len);
    level.utilization - (len - left) as f64 / len as f64
}

fn pct(share: f64) -> i64 {
    (share * 100.0).round() as i64
}

/// The steps `cfg` calls for, given `readings`. A pause outranks the rest; across windows the
/// pause that lasts longest and the smallest share win.
pub fn throttle(cfg: &BackoffConfig, readings: &[Reading], now: u64) -> Throttle {
    let windows: [(&str, u64, &WindowBackoff); 2] = [
        ("five_hour", 5 * 3600, &cfg.five_hour),
        ("seven_day", 7 * 86400, &cfg.seven_day),
    ];
    let mut t = Throttle::default();
    for (name, len, limits) in windows {
        let Some(level) = level(readings, name, now) else {
            continue;
        };
        let used = format!(
            "the plan's {name} window is {}% used",
            pct(level.utilization)
        );
        if let Some(at) = limits.pause_at.filter(|&at| level.utilization >= at) {
            if t.pause
                .as_ref()
                .is_none_or(|(_, until)| level.resets_at > *until)
            {
                t.pause = Some((
                    format!(
                        "backoff: {used} (pause_at {}%); resumes at {}",
                        pct(at),
                        clock(level.resets_at, now)
                    ),
                    level.resets_at,
                ));
            }
        }
        if let Some(at) = limits.lighter_at.filter(|&at| level.utilization >= at) {
            t.lighter
                .get_or_insert_with(|| format!("{used} (lighter_at {}%)", pct(at)));
        }
        let by_far = ahead(&level, len, now);
        if let Some(by) = limits.ahead_by.filter(|&by| by_far >= by) {
            let share = limits.sample();
            if t.share.as_ref().is_none_or(|(s, _)| share < *s) {
                t.share = Some((
                    share,
                    format!(
                        "{used}, {} points ahead of an even pace (ahead_by {}%)",
                        pct(by_far),
                        pct(by)
                    ),
                ));
            }
        }
    }
    t
}

fn on(cfg: &BackoffConfig) -> bool {
    [&cfg.five_hour, &cfg.seven_day]
        .iter()
        .any(|w| w.lighter_at.is_some() || w.pause_at.is_some() || w.ahead_by.is_some())
}

/// The backoff for a review of `repo` now, from the readings in the usage log. Off, or without a
/// usage log, nothing holds.
pub fn check(cfg: &BackoffConfig, repo: &str, now: u64) -> Throttle {
    match usage::dir() {
        Some(dir) if on(cfg) => check_in(&dir, cfg, repo, now),
        _ => Throttle::default(),
    }
}

/// [`check`] against the usage log in `dir`.
pub fn check_in(dir: &Path, cfg: &BackoffConfig, repo: &str, now: u64) -> Throttle {
    let mut t = throttle(cfg, &usage::recorded(dir), now);
    let path = dir.join(STATE);
    let mut held: BTreeMap<String, Vec<String>> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let steps = t.steps();
    let before = held.get(repo).cloned().unwrap_or_default();
    if before != steps {
        t.lifted = steps.is_empty();
        if steps.is_empty() {
            held.remove(repo);
        } else {
            held.insert(repo.to_string(), steps);
        }
        // Losing this only loses the "lifted" line.
        let _ = fs::create_dir_all(dir);
        let _ = fs::write(
            &path,
            serde_json::to_string_pretty(&held).unwrap_or_default(),
        );
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::iso;

    /// 2026-10-27T12:00:00Z.
    const NOW: u64 = 1_793_102_400;

    fn reading(window: &str, utilization: f64, resets_at: u64) -> Reading {
        Reading {
            at: iso(NOW - 60),
            window: window.into(),
            utilization,
            resets_at: iso(resets_at),
            status: "allowed".into(),
            ..Default::default()
        }
    }

    fn cfg(toml: &str) -> BackoffConfig {
        toml::from_str(toml).unwrap()
    }

    const FIVE: &str = "[five_hour]\nlighter_at = 0.7\npause_at = 0.85\n";

    #[test]
    fn below_every_limit_nothing_holds() {
        let r = [reading("five_hour", 0.5, NOW + 3600)];
        assert_eq!(throttle(&cfg(FIVE), &r, NOW), Throttle::default());
    }

    #[test]
    fn off_by_default_even_with_a_full_window() {
        let r = [reading("five_hour", 1.0, NOW + 3600)];
        assert_eq!(throttle(&cfg(""), &r, NOW), Throttle::default());
    }

    #[test]
    fn lighter_at_runs_correctness_only() {
        let r = [reading("five_hour", 0.72, NOW + 3600)];
        let t = throttle(&cfg(FIVE), &r, NOW);
        assert_eq!(
            t.lighter.as_deref(),
            Some("the plan's five_hour window is 72% used (lighter_at 70%)")
        );
        assert_eq!((t.pause, t.share), (None, None));
    }

    #[test]
    fn pause_at_holds_until_the_window_resets() {
        let r = [reading("five_hour", 0.87, NOW + 3600)];
        let t = throttle(&cfg(FIVE), &r, NOW);
        assert_eq!(
            t.pause,
            Some((
                "backoff: the plan's five_hour window is 87% used (pause_at 85%); resumes at 13:00Z"
                    .into(),
                NOW + 3600
            ))
        );
        // Once it resets, its readings no longer count.
        assert_eq!(throttle(&cfg(FIVE), &r, NOW + 3600), Throttle::default());
    }

    #[test]
    fn a_dip_doesnt_lift_a_step() {
        let r = [
            reading("five_hour", 0.86, NOW + 3600),
            reading("five_hour", 0.84, NOW + 3600),
        ];
        assert!(throttle(&cfg(FIVE), &r, NOW).pause.is_some());
    }

    #[test]
    fn a_past_window_is_ignored_for_the_new_one() {
        let r = [
            reading("five_hour", 0.95, NOW - 60),
            reading("five_hour", 0.1, NOW + 4 * 3600),
        ];
        assert_eq!(throttle(&cfg(FIVE), &r, NOW), Throttle::default());
    }

    #[test]
    fn ahead_of_pace_reviews_a_share_of_prs() {
        let week = "[seven_day]\nahead_by = 0.1\nsample = 0.25\n";
        // Two days into the week: an even pace is 2/7, about 29%.
        let resets = NOW + 5 * 86400;
        let on_pace = [reading("seven_day", 0.35, resets)];
        assert_eq!(throttle(&cfg(week), &on_pace, NOW), Throttle::default());
        let ahead = [reading("seven_day", 0.45, resets)];
        let t = throttle(&cfg(week), &ahead, NOW);
        assert_eq!(
            t.share,
            Some((
                0.25,
                "the plan's seven_day window is 45% used, 16 points ahead of an even pace (ahead_by 10%)"
                    .into()
            ))
        );
        assert_eq!((t.pause, t.lighter), (None, None));
    }

    #[test]
    fn the_longest_pause_wins() {
        let both = "[five_hour]\npause_at = 0.8\n[seven_day]\npause_at = 0.9\n";
        let r = [
            reading("five_hour", 0.9, NOW + 3600),
            reading("seven_day", 0.95, NOW + 86400),
        ];
        let (why, until) = throttle(&cfg(both), &r, NOW).pause.unwrap();
        assert_eq!(until, NOW + 86400);
        assert!(why.contains("seven_day"), "{why}");
    }

    #[test]
    fn the_review_after_the_last_step_says_it_lifted() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(FIVE);
        let write = |u: f64| {
            let line = serde_json::to_string(&reading("five_hour", u, NOW + 3600)).unwrap();
            fs::write(dir.path().join("readings.jsonl"), line + "\n").unwrap();
        };
        write(0.9);
        let t = check_in(dir.path(), &c, "lens", NOW);
        assert!(t.pause.is_some() && !t.lifted);
        // Another repo held nothing, so nothing lifts for it.
        write(0.2);
        assert!(!check_in(dir.path(), &c, "other", NOW).lifted);
        assert!(check_in(dir.path(), &c, "lens", NOW).lifted);
        assert!(!check_in(dir.path(), &c, "lens", NOW).lifted);
    }
}
