//! Runs review agents as headless `claude -p` sessions, each as lean as a review allows: no user or
//! project settings (so no CLAUDE.md, memory, hooks, plugins or skills), no MCP servers, only the
//! tools its role needs, and the 5-minute prompt cache. Every agent of a pass shares one system
//! prompt that holds the change itself; its role's instructions come in its first message. Agents
//! with the same model and tools then send the same cached prefix, so the first one's request
//! caches it and the rest read it at a tenth of the price: a batch starts one agent per (model,
//! tools) group and holds the rest of that group until its first response.
//!
//! When the plan turns an agent away (a `rate_limit_event` whose status is `rejected`), no agent
//! starts again until that window resets, in this lens or any other: see [`Gate`].

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::review::{iso, iso_now};
use crate::usage;

/// What an agent may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read files and run read-only git commands; everything else is denied.
    ReadOnly,
}

/// One agent to run.
#[derive(Debug, Clone)]
pub struct Spec {
    /// A short name for logs and the saved reply, e.g. `reviewer-correctness`.
    pub label: String,
    /// The system prompt file, shared by every agent of a pass.
    pub system: PathBuf,
    /// The first message: the role's instructions and this agent's job.
    pub prompt: String,
    pub model: String,
    pub effort: Option<String>,
    pub access: Access,
    /// The working directory: the repo (or the PR's tree).
    pub cwd: PathBuf,
    /// Folders outside `cwd` the agent may read, e.g. the work folder of a PR pass.
    pub add_dirs: Vec<PathBuf>,
}

/// How an agent run went.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub label: String,
    pub model: String,
    /// The tools it had, e.g. `Read,Grep,Glob,Bash`: with the model, its cache group.
    pub tools: String,
    /// The agent's final reply.
    pub text: String,
    pub cost_usd: f64,
    pub turns: u64,
    pub seconds: u64,
    /// The first request's `cache_creation_input_tokens` and `cache_read_input_tokens`: the
    /// shared prefix shows under one or the other, by whether its group was warm.
    pub cache_creation: u64,
    pub cache_read: u64,
    /// Why the run failed (the process died, timed out or reported an error).
    pub error: Option<String>,
    /// The plan's window that turned the agent away, or kept it from starting.
    pub full: Option<Full>,
}

/// A window of the plan that turned an agent away, and when it takes requests again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Full {
    /// `five_hour` or `seven_day`.
    pub window: String,
    /// Seconds since the epoch.
    pub resets_at: u64,
}

/// Whether the plan takes new agents. Once an agent's reading is rejected, no agent starts until
/// that window resets: not on any runner cloned from this one, and not in another lens, which reads
/// the hold from `full.json` beside the usage logs.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    held: Arc<Mutex<Option<Full>>>,
    file: Option<PathBuf>,
}

/// How agents run: the `claude` program and its limits.
#[derive(Debug, Clone)]
pub struct Runner {
    pub claude: PathBuf,
    pub parallel: usize,
    pub timeout: Duration,
    /// Where each agent's reply is saved, as `<label>.md`.
    pub log_dir: PathBuf,
    /// When each cache group last sent a request, so a later batch knows its prefix is cached.
    pub cached: Arc<Mutex<HashMap<String, Instant>>>,
    /// Where each agent's tokens and the plan's window readings are recorded, if anywhere.
    pub usage: Option<usage::Log>,
    pub gate: Gate,
}

/// How long after a group's last request its prefix counts as cached: under the 5-minute TTL.
const CACHED_FOR: Duration = Duration::from_secs(4 * 60);

/// How long to hold agents when a rejected reading gives no reset time still ahead.
const HOLD_WITHOUT_RESET: u64 = 5 * 60;

const READ_ONLY_BASH: &[&str] = &[
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(git blame:*)",
    "Bash(git grep:*)",
    "Bash(git diff:*)",
    "Bash(git ls-files:*)",
    "Bash(ls:*)",
    "Bash(wc:*)",
];

enum Event {
    /// The agent's first response is back, so its group's prefix is cached.
    Warm(usize),
    Done(usize, Outcome),
}

impl Full {
    /// The window a `rate_limit_event` says is full, or `None` when the plan let the request
    /// through.
    pub fn of(event: &Value, now: u64) -> Option<Full> {
        let info = &event["rate_limit_info"];
        // The request's own status decides; a full window under an allowed request (extra usage)
        // still runs agents.
        if info["status"].as_str().is_some_and(|s| s != "rejected") {
            return None;
        }
        let window = info["unifiedWindows"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, w)| w["status"] == "rejected")
            .map(|(name, w)| Full {
                window: name.clone(),
                resets_at: w["resetsAt"].as_u64().unwrap_or(0),
            })
            .max_by_key(|f| f.resets_at);
        let full = window.or_else(|| {
            (info["status"] == "rejected").then(|| Full {
                window: info["rateLimitType"]
                    .as_str()
                    .unwrap_or("usage")
                    .to_string(),
                resets_at: info["resetsAt"].as_u64().unwrap_or(0),
            })
        })?;
        Some(Full {
            resets_at: if full.resets_at > now {
                full.resets_at
            } else {
                now + HOLD_WITHOUT_RESET
            },
            ..full
        })
    }

    /// E.g. `the plan's five_hour window is full; resumes at 18:30Z`.
    pub fn message(&self, now: u64) -> String {
        format!(
            "the plan's {} window is full; resumes at {}",
            self.window,
            clock(self.resets_at, now)
        )
    }
}

impl Gate {
    pub fn new(file: Option<PathBuf>) -> Gate {
        Gate {
            held: Default::default(),
            file,
        }
    }

    /// The gate every lens shares, kept in `full.json` beside the usage logs.
    pub fn shared() -> Gate {
        Gate::new(usage::dir().map(|d| d.join("full.json")))
    }

    /// The window holding agents back at `now`, if any.
    pub fn full(&self, now: u64) -> Option<Full> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if held.as_ref().is_none_or(|f| f.resets_at <= now) {
            *held = self
                .file
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| serde_json::from_str(&s).ok());
        }
        held.clone().filter(|f| f.resets_at > now)
    }

    /// Holds new agents until `full`'s window resets, unless a later reset already holds them.
    pub fn hold(&self, full: Full) {
        if self
            .full(now_secs())
            .is_some_and(|h| h.resets_at >= full.resets_at)
        {
            return;
        }
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(full.clone());
        let Some(path) = &self.file else { return };
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        // Written aside and renamed, so another lens never reads half of it.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if let Ok(text) = serde_json::to_string(&full) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

impl Access {
    fn tools(self) -> &'static str {
        match self {
            Access::ReadOnly => "Read,Grep,Glob,Bash",
        }
    }
}

impl Runner {
    /// The arguments for one agent's `claude -p`.
    pub fn args(&self, spec: &Spec) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--model".into(),
            spec.model.clone(),
            "--system-prompt-file".into(),
            spec.system.display().to_string(),
            "--setting-sources".into(),
            String::new(),
            "--strict-mcp-config".into(),
            "--disable-slash-commands".into(),
            "--no-session-persistence".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];
        if let Some(effort) = spec.effort.as_ref().filter(|e| !e.is_empty()) {
            if !spec.model.contains("haiku") {
                args.extend(["--effort".into(), effort.clone()]);
            }
        }
        let (mode, allowed): (&str, Vec<&str>) = match spec.access {
            Access::ReadOnly => (
                "dontAsk",
                ["Read", "Grep", "Glob"]
                    .into_iter()
                    .chain(READ_ONLY_BASH.iter().copied())
                    .collect(),
            ),
        };
        args.extend([
            "--tools".into(),
            spec.access.tools().into(),
            "--permission-mode".into(),
            mode.into(),
        ]);
        if !allowed.is_empty() {
            args.extend(["--allowedTools".into(), allowed.join(",")]);
        }
        for dir in &spec.add_dirs {
            args.extend(["--add-dir".into(), dir.display().to_string()]);
        }
        args
    }

    /// Runs one agent to the end.
    pub fn run(&self, spec: Spec) -> Outcome {
        let (tx, rx) = mpsc::channel();
        self.spawn(0, spec, tx);
        loop {
            match rx.recv() {
                Ok(Event::Done(_, outcome)) => return outcome,
                Ok(Event::Warm(_)) => continue,
                Err(_) => return Outcome::default(),
            }
        }
    }

    /// Runs every agent, at most `parallel` at once, and returns their outcomes in order. In a
    /// group whose prefix isn't cached yet, the first agent runs alone until its first response,
    /// so the rest of the group finds it cached.
    pub fn run_all(&self, specs: Vec<Spec>) -> Vec<Outcome> {
        let n = specs.len();
        let mut out: Vec<Option<Outcome>> = vec![None; n];
        let groups: Vec<String> = specs.iter().map(group).collect();
        let mut queue: Vec<Option<Spec>> = specs.into_iter().map(Some).collect();
        // Groups whose first agent is running and hasn't had a response yet.
        let mut warming: HashSet<String> = HashSet::new();
        let (tx, rx) = mpsc::channel();
        let mut running = 0;
        loop {
            // Once the plan turns an agent away, the rest of the batch doesn't start.
            if let Some(full) = self.gate.full(now_secs()) {
                for (i, spec) in queue.iter_mut().enumerate() {
                    if let Some(spec) = spec.take() {
                        out[i] = Some(refused(&spec, full.clone()));
                    }
                }
            }
            for i in 0..n {
                if running >= self.parallel.max(1) {
                    break;
                }
                if queue[i].is_none() || warming.contains(&groups[i]) {
                    continue;
                }
                if !self.is_cached(&groups[i]) {
                    warming.insert(groups[i].clone());
                }
                self.spawn(i, queue[i].take().unwrap(), tx.clone());
                running += 1;
            }
            if running == 0 {
                break;
            }
            // The first response, or the agent ending without one: either way the group goes on.
            match rx.recv() {
                Ok(Event::Warm(i)) => {
                    warming.remove(&groups[i]);
                }
                Ok(Event::Done(i, outcome)) => {
                    warming.remove(&groups[i]);
                    out[i] = Some(outcome);
                    running -= 1;
                }
                Err(_) => break,
            }
        }
        out.into_iter().map(Option::unwrap_or_default).collect()
    }

    fn is_cached(&self, group: &str) -> bool {
        self.cached
            .lock()
            .unwrap()
            .get(group)
            .is_some_and(|t| t.elapsed() < CACHED_FOR)
    }

    fn spawn(&self, i: usize, spec: Spec, tx: mpsc::Sender<Event>) {
        let runner = self.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let mut outcome = runner.exec(i, &spec, &tx).unwrap_or_else(|e| Outcome {
                error: Some(format!("{e:#}")),
                ..Outcome::default()
            });
            outcome.label = spec.label.clone();
            outcome.model = spec.model.clone();
            outcome.tools = spec.access.tools().to_string();
            outcome.seconds = started.elapsed().as_secs();
            runner.save(&outcome);
            let _ = tx.send(Event::Done(i, outcome));
        });
    }

    fn exec(&self, i: usize, spec: &Spec, tx: &mpsc::Sender<Event>) -> Result<Outcome> {
        if let Some(full) = self.gate.full(now_secs()) {
            return Ok(refused(spec, full));
        }
        let group = group(spec);
        let started_at = iso_now();
        let mut child = Command::new(&self.claude)
            .args(self.args(spec))
            .current_dir(&spec.cwd)
            .env("CLAUDE_CODE_PROMPT_CACHE_TTL", "5m")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.claude.display()))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(spec.prompt.as_bytes())?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (lines_tx, lines_rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let err_reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut text);
            text
        });
        let deadline = Instant::now() + self.timeout;
        let mut first_usage: Option<(u64, u64)> = None;
        let mut turned_away: Option<Full> = None;
        let mut result: Option<Value> = None;
        let mut last_text = String::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match lines_rx.recv_timeout(left) {
                Ok(line) => {
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match event["type"].as_str() {
                        Some("assistant") => {
                            // Every request reads the prefix again, which keeps it cached.
                            self.cached
                                .lock()
                                .unwrap()
                                .insert(group.clone(), Instant::now());
                            if first_usage.is_none() {
                                let usage = &event["message"]["usage"];
                                first_usage = Some((
                                    usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                                    usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
                                ));
                                let _ = tx.send(Event::Warm(i));
                            }
                            if let Some(text) = assistant_text(&event) {
                                last_text = text;
                            }
                        }
                        Some("rate_limit_event") => {
                            if let Some(log) = &self.usage {
                                log.record_readings(&spec.label, &iso_now(), &event);
                            }
                            if let Some(full) = Full::of(&event, now_secs()) {
                                self.gate.hold(full.clone());
                                turned_away = Some(full);
                            }
                        }
                        Some("result") => {
                            // A run the plan turned away before it spent anything isn't one.
                            let spent = event["total_cost_usd"].as_f64().unwrap_or(0.0) > 0.0;
                            if let (Some(log), true) = (&self.usage, turned_away.is_none() || spent)
                            {
                                log.record_result(
                                    &spec.label,
                                    &spec.model,
                                    &started_at,
                                    &iso_now(),
                                    &event,
                                );
                            }
                            result = Some(event);
                        }
                        _ => {}
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Outcome {
                        text: last_text,
                        error: Some(format!("timed out after {}s", self.timeout.as_secs())),
                        full: turned_away,
                        ..usage_outcome(first_usage)
                    });
                }
            }
        }
        let status = child.wait()?;
        let stderr = err_reader.join().unwrap_or_default();
        if let Some(full) = turned_away {
            return Ok(Outcome {
                text: last_text,
                cost_usd: result
                    .as_ref()
                    .and_then(|r| r["total_cost_usd"].as_f64())
                    .unwrap_or(0.0),
                error: Some(full.message(now_secs())),
                full: Some(full),
                ..usage_outcome(first_usage)
            });
        }
        let Some(result) = result else {
            return Ok(Outcome {
                text: last_text,
                error: Some(format!(
                    "claude exited ({status}) without a result: {}",
                    tail(&stderr)
                )),
                ..usage_outcome(first_usage)
            });
        };
        let text = result["result"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(last_text);
        let error = (result["is_error"] == Value::Bool(true))
            .then(|| result["subtype"].as_str().unwrap_or("error").to_string());
        Ok(Outcome {
            text,
            cost_usd: result["total_cost_usd"].as_f64().unwrap_or(0.0),
            turns: result["num_turns"].as_u64().unwrap_or(0),
            error,
            ..usage_outcome(first_usage)
        })
    }

    fn save(&self, outcome: &Outcome) {
        let _ = std::fs::create_dir_all(&self.log_dir);
        let mut text = format!(
            "<!-- {} · ${:.4} · {} turns · {}s · first turn: {} cache write, {} cache read{} -->\n\n",
            outcome.label,
            outcome.cost_usd,
            outcome.turns,
            outcome.seconds,
            outcome.cache_creation,
            outcome.cache_read,
            outcome
                .error
                .as_ref()
                .map(|e| format!(" · error: {e}"))
                .unwrap_or_default()
        );
        text.push_str(&outcome.text);
        let _ = std::fs::write(self.log_dir.join(format!("{}.md", outcome.label)), text);
    }
}

/// The agents whose requests start with the same cached prefix: same model, tools and system
/// prompt.
fn group(spec: &Spec) -> String {
    format!(
        "{}|{}|{}",
        spec.model,
        spec.access.tools(),
        spec.system.display()
    )
}

/// An agent that didn't start because the plan's `full` window is full.
fn refused(spec: &Spec, full: Full) -> Outcome {
    Outcome {
        label: spec.label.clone(),
        model: spec.model.clone(),
        tools: spec.access.tools().to_string(),
        error: Some(full.message(now_secs())),
        full: Some(full),
        ..Outcome::default()
    }
}

/// `secs` as `18:30Z` on the same day as `now`, else `2026-10-28T18:30Z`.
pub fn clock(secs: u64, now: u64) -> String {
    let at = iso(secs);
    if at[..10] == iso(now)[..10] {
        format!("{}Z", &at[11..16])
    } else {
        format!("{}Z", &at[..16])
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn usage_outcome(first: Option<(u64, u64)>) -> Outcome {
    let (cache_creation, cache_read) = first.unwrap_or_default();
    Outcome {
        cache_creation,
        cache_read,
        ..Outcome::default()
    }
}

fn assistant_text(event: &Value) -> Option<String> {
    let text: String = event["message"]["content"]
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!text.is_empty()).then_some(text)
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let start = s.len().saturating_sub(400);
    let start = (start..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[start..].to_string()
}

/// The `claude` program: `$LENS_CLAUDE`, or `claude` on the PATH.
pub fn claude_program() -> PathBuf {
    std::env::var_os("LENS_CLAUDE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"))
}

/// A role's prompt (`<assets>/prompts/<role>.md`) without its frontmatter.
pub fn role_prompt(assets: &Path, role: &str) -> Result<String> {
    let path = assets.join("prompts").join(format!("{role}.md"));
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(strip_frontmatter(&text).trim().to_string())
}

pub fn strip_frontmatter(text: &str) -> &str {
    text.strip_prefix("---")
        .and_then(|rest| rest.find("\n---").map(|end| &rest[end + 4..]))
        .unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runner() -> Runner {
        Runner {
            claude: "claude".into(),
            parallel: 4,
            timeout: Duration::from_secs(60),
            log_dir: "/tmp".into(),
            cached: Default::default(),
            usage: None,
            gate: Gate::default(),
        }
    }

    fn spec(access: Access, model: &str) -> Spec {
        Spec {
            label: "x".into(),
            system: "/w/sys.md".into(),
            prompt: "p".into(),
            model: model.into(),
            effort: Some("high".into()),
            access,
            cwd: "/repo".into(),
            add_dirs: vec!["/w".into()],
        }
    }

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == name)
            .map(|i| args[i + 1].as_str())
    }

    #[test]
    fn agents_load_none_of_the_users_setup() {
        let args = runner().args(&spec(Access::ReadOnly, "opus"));
        assert_eq!(flag(&args, "--setting-sources"), Some(""));
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(args.iter().any(|a| a == "--disable-slash-commands"));
        assert_eq!(flag(&args, "--system-prompt-file"), Some("/w/sys.md"));
        assert_eq!(flag(&args, "--add-dir"), Some("/w"));
    }

    #[test]
    fn reviewers_are_read_only() {
        let r = runner();
        let args = r.args(&spec(Access::ReadOnly, "opus"));
        assert_eq!(flag(&args, "--permission-mode"), Some("dontAsk"));
        assert!(flag(&args, "--allowedTools")
            .unwrap()
            .contains("Bash(git show:*)"));
        assert_eq!(flag(&args, "--effort"), Some("high"));
        assert_eq!(flag(&args, "--tools"), Some("Read,Grep,Glob,Bash"));
    }

    #[test]
    fn strips_frontmatter() {
        assert_eq!(strip_frontmatter("---\na: 1\n---\nbody"), "\nbody");
        assert_eq!(strip_frontmatter("body"), "body");
    }

    /// Runs `specs` (labelled by their prompt) through a fake claude that logs when each agent
    /// starts and when it answers, and returns that log.
    fn batch(cached: &[(&Spec, Instant)], specs: Vec<Spec>) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let claude = dir.path().join("claude");
        std::fs::write(
            &claude,
            format!(
                r#"#!/bin/sh
label=$(cat)
echo "start $label" >> {log}
sleep 0.4
echo "first $label" >> {log}
echo '{{"type":"assistant","message":{{"content":[]}}}}'
sleep 0.2
echo '{{"type":"result","result":"ok"}}'
"#,
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = Runner {
            claude,
            log_dir: dir.path().into(),
            ..runner()
        };
        for (spec, at) in cached {
            runner.cached.lock().unwrap().insert(group(spec), *at);
        }
        let specs = specs
            .into_iter()
            .map(|s| Spec {
                cwd: dir.path().into(),
                ..s
            })
            .collect();
        for outcome in runner.run_all(specs) {
            assert_eq!(outcome.error, None);
        }
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn labelled(label: &str, model: &str) -> Spec {
        Spec {
            prompt: label.into(),
            ..spec(Access::ReadOnly, model)
        }
    }

    fn at(log: &[String], line: &str) -> usize {
        log.iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("no {line:?} in {log:?}"))
    }

    #[test]
    fn a_cold_group_starts_one_agent_and_holds_the_rest_until_it_answers() {
        let log = batch(
            &[],
            vec![
                labelled("a1", "opus"),
                labelled("a2", "opus"),
                labelled("b", "sonnet"),
            ],
        );
        assert!(at(&log, "start a2") > at(&log, "first a1"), "{log:?}");
        // Another group doesn't wait for it.
        assert!(at(&log, "start b") < at(&log, "first a1"), "{log:?}");
    }

    fn limit_event(status: &str, resets_at: u64) -> Value {
        json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": status,
                "resetsAt": resets_at,
                "rateLimitType": "five_hour",
                "unifiedWindows": {
                    "five_hour": { "utilization": 1.0, "resetsAt": resets_at, "status": status },
                    "seven_day": { "utilization": 0.4, "resetsAt": resets_at + 86400, "status": "allowed" },
                },
            },
        })
    }

    #[test]
    fn a_rejected_reading_names_the_full_window_and_its_reset() {
        let now = 1_000_000;
        assert_eq!(
            Full::of(&limit_event("rejected", now + 600), now),
            Some(Full {
                window: "five_hour".into(),
                resets_at: now + 600
            })
        );
        assert_eq!(Full::of(&limit_event("allowed", now + 600), now), None);
        assert_eq!(
            Full::of(&limit_event("allowed_warning", now + 600), now),
            None
        );
        // A reset already past still holds agents for a while rather than not at all.
        let late = Full::of(&limit_event("rejected", now - 5), now).unwrap();
        assert_eq!(late.resets_at, now + HOLD_WITHOUT_RESET);
    }

    #[test]
    fn says_when_the_window_resumes() {
        let now = 1_761_586_920; // 2025-10-27T17:42:00Z
        let full = Full {
            window: "five_hour".into(),
            resets_at: now + 48 * 60,
        };
        assert_eq!(
            full.message(now),
            "the plan's five_hour window is full; resumes at 18:30Z"
        );
        let full = Full {
            window: "seven_day".into(),
            resets_at: now + 86400,
        };
        assert_eq!(
            full.message(now),
            "the plan's seven_day window is full; resumes at 2025-10-28T17:42Z"
        );
    }

    #[test]
    fn a_hold_is_shared_through_its_file_until_the_window_resets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("full.json");
        let now = now_secs();
        let full = Full {
            window: "five_hour".into(),
            resets_at: now + 600,
        };
        Gate::new(Some(file.clone())).hold(full.clone());
        // Another lens reads it.
        let other = Gate::new(Some(file.clone()));
        assert_eq!(other.full(now), Some(full.clone()));
        assert_eq!(other.full(now + 599), Some(full.clone()));
        assert_eq!(other.full(now + 600), None);
        // An earlier reset doesn't shorten a later one.
        other.hold(Full {
            window: "five_hour".into(),
            resets_at: now + 60,
        });
        assert_eq!(Gate::new(Some(file)).full(now), Some(full));
    }

    /// Runs `specs` (labelled by their prompt) through a fake claude the plan turns away for every
    /// prompt starting with `r`. Returns the start log, the outcomes and the usage folder.
    fn turned_away(
        gate: Gate,
        resets_at: u64,
        specs: Vec<Spec>,
    ) -> (Vec<String>, Vec<Outcome>, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let claude = dir.path().join("claude");
        std::fs::write(
            &claude,
            format!(
                r#"#!/bin/sh
label=$(cat)
echo "start $label" >> {log}
case "$label" in
r*)
  echo '{rejected}'
  echo '{{"type":"result","is_error":true,"result":"limit reached","total_cost_usd":0}}'
  exit 1;;
esac
sleep 0.2
echo '{{"type":"assistant","message":{{"content":[]}}}}'
echo '{{"type":"result","result":"ok","total_cost_usd":0.01}}'
"#,
                log = log.display(),
                rejected = limit_event("rejected", resets_at),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = Runner {
            claude,
            log_dir: dir.path().into(),
            usage: Some(usage::Log {
                dir: dir.path().join("usage"),
                repo: "repo".into(),
            }),
            gate,
            ..runner()
        };
        let specs = specs
            .into_iter()
            .map(|s| Spec {
                cwd: dir.path().into(),
                ..s
            })
            .collect();
        let outcomes = runner.run_all(specs);
        let log = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        (log, outcomes, dir)
    }

    #[test]
    fn a_rejected_reading_starts_no_more_agents_until_the_window_resets() {
        let resets_at = now_secs() + 600;
        let gate = Gate::default();
        let before = now_secs();
        let (log, outcomes, dir) = turned_away(
            gate.clone(),
            resets_at,
            vec![
                labelled("r1", "sonnet"),
                labelled("a2", "sonnet"),
                labelled("a3", "sonnet"),
            ],
        );
        assert_eq!(log, ["start r1"]);
        let full = Full {
            window: "five_hour".into(),
            resets_at,
        };
        for o in &outcomes {
            assert_eq!(o.full.as_ref(), Some(&full), "{o:?}");
            // The message names the day, so allow for the clock crossing midnight mid-run.
            let error = o.error.clone().unwrap();
            assert!(
                [full.message(before), full.message(now_secs())].contains(&error),
                "{error}"
            );
        }
        // The reading is kept; the refused run isn't a row of spend.
        let usage = dir.path().join("usage");
        assert!(std::fs::read_to_string(usage.join("readings.jsonl"))
            .unwrap()
            .contains("rejected"));
        assert!(!usage.join("agents.jsonl").exists());

        // A later batch on the same gate starts nothing.
        let (log, outcomes, _dir) = turned_away(gate, resets_at, vec![labelled("a4", "opus")]);
        assert!(log.is_empty(), "{log:?}");
        assert!(outcomes[0].full.is_some());
    }

    #[test]
    fn a_rejected_status_without_windows_names_the_window_from_the_top_level() {
        let now = 1_000_000;
        let event = json!({"type": "rate_limit_event", "rate_limit_info": {
            "status": "rejected", "rateLimitType": "seven_day", "resetsAt": now + 900}});
        assert_eq!(
            Full::of(&event, now),
            Some(Full {
                window: "seven_day".into(),
                resets_at: now + 900
            })
        );
        // Windows, none of them rejected: the top level still names the full window.
        let none_rejected = json!({"rate_limit_info": {
            "status": "rejected", "rateLimitType": "seven_day", "resetsAt": now + 900,
            "unifiedWindows": {"five_hour": {"status": "allowed", "resetsAt": now + 100}}}});
        assert_eq!(
            Full::of(&none_rejected, now),
            Some(Full {
                window: "seven_day".into(),
                resets_at: now + 900
            })
        );
        // No type and no reset still ahead: a generic window held for the fallback.
        let bare = json!({"rate_limit_info": {"status": "rejected", "resetsAt": now - 5}});
        assert_eq!(
            Full::of(&bare, now),
            Some(Full {
                window: "usage".into(),
                resets_at: now + HOLD_WITHOUT_RESET
            })
        );
    }

    #[test]
    fn a_reading_with_no_status_is_not_a_rejection_unless_a_window_is() {
        let now = 1_000_000;
        assert_eq!(Full::of(&json!({"type": "rate_limit_event"}), now), None);
        let windows = |a: u64, b: u64| {
            json!({"rate_limit_info": {"unifiedWindows": {
                "five_hour": {"status": "rejected", "resetsAt": a},
                "seven_day": {"status": "rejected", "resetsAt": b}}}})
        };
        // Several rejected windows: the one that resets last holds the agents.
        assert_eq!(
            Full::of(&windows(now + 100, now + 500), now),
            Some(Full {
                window: "seven_day".into(),
                resets_at: now + 500
            })
        );
        let allowed = json!({"rate_limit_info": {"status": "allowed", "unifiedWindows": {
            "five_hour": {"status": "rejected", "resetsAt": now + 100}}}});
        assert_eq!(Full::of(&allowed, now), None);
    }

    /// A fake claude that is turned away and then `tail`s; run with a `timeout`. Returns the
    /// outcome and the usage folder.
    fn rejected_then(tail: &str, timeout: Duration) -> (Outcome, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\ncat >/dev/null\necho '{}'\n{tail}\n",
                limit_event("rejected", now_secs() + 600)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = Runner {
            claude,
            timeout,
            log_dir: dir.path().into(),
            usage: Some(usage::Log {
                dir: dir.path().join("usage"),
                repo: "repo".into(),
            }),
            ..runner()
        };
        let spec = Spec {
            cwd: dir.path().into(),
            ..labelled("r1", "sonnet")
        };
        (runner.run(spec), dir)
    }

    #[test]
    fn an_agent_turned_away_that_then_times_out_still_reports_the_full_window() {
        let (o, _dir) = rejected_then("exec sleep 30", Duration::from_secs(3));
        assert!(o.full.is_some(), "{o:?}");
        assert!(o.error.unwrap().starts_with("timed out after"));
    }

    #[test]
    fn a_turned_away_run_that_spent_is_still_recorded() {
        let result = r#"{"type":"result","is_error":true,"result":"limit reached","total_cost_usd":0.05,"usage":{"input_tokens":3,"output_tokens":4}}"#;
        let (o, dir) = rejected_then(&format!("echo '{result}'"), Duration::from_secs(10));
        assert!(o.full.is_some());
        assert_eq!(o.cost_usd, 0.05);
        let ledger = std::fs::read_to_string(dir.path().join("usage/agents.jsonl")).unwrap();
        assert_eq!(ledger.lines().count(), 1, "{ledger}");
    }

    #[test]
    fn agents_start_again_once_the_window_resets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("full.json");
        std::fs::write(
            &file,
            serde_json::to_string(&Full {
                window: "five_hour".into(),
                resets_at: now_secs() - 1,
            })
            .unwrap(),
        )
        .unwrap();
        let (log, outcomes, _dir) = turned_away(
            Gate::new(Some(file)),
            now_secs() + 600,
            vec![labelled("a1", "sonnet"), labelled("a2", "sonnet")],
        );
        assert_eq!(log.len(), 2, "{log:?}");
        for o in outcomes {
            assert_eq!((o.error, o.full), (None, None));
        }
    }

    #[test]
    fn a_cached_group_starts_together_and_an_expired_one_is_held_again() {
        let (a1, a2) = (labelled("a1", "opus"), labelled("a2", "opus"));
        let log = batch(&[(&a1, Instant::now())], vec![a1.clone(), a2.clone()]);
        assert!(at(&log, "start a2") < at(&log, "first a1"), "{log:?}");

        let Some(expired) = Instant::now().checked_sub(CACHED_FOR + Duration::from_secs(1)) else {
            return;
        };
        let log = batch(&[(&a1, expired)], vec![a1.clone(), a2]);
        assert!(at(&log, "start a2") > at(&log, "first a1"), "{log:?}");
    }
}
