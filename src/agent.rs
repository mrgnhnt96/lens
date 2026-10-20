//! Runs review agents as headless `claude -p` sessions, each as lean as a review allows: no user or
//! project settings (so no CLAUDE.md, memory, hooks, plugins or skills), no MCP servers, only the
//! tools its role needs, and the 5-minute prompt cache. Every agent of a pass shares one system
//! prompt that holds the change itself; its role's instructions come in its first message. Agents
//! with the same model and tools then send the same cached prefix, so the first one's request
//! caches it and the rest read it at a tenth of the price: a batch starts one agent per (model,
//! tools) group and holds the rest of that group until its first response.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

/// What an agent may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read files and run read-only git commands; everything else is denied.
    ReadOnly,
    /// Read only, without Bash (the escalation judge).
    ReadFiles,
    /// Edit the working tree and run builds and tests (the fixer), in the given permission mode.
    Edit,
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
}

/// How agents run: the `claude` program, limits and the fixer's permission mode.
#[derive(Debug, Clone)]
pub struct Runner {
    pub claude: PathBuf,
    pub parallel: usize,
    pub timeout: Duration,
    pub fixer_permission_mode: String,
    /// Where each agent's reply is saved, as `<label>.md`.
    pub log_dir: PathBuf,
    /// When each cache group last sent a request, so a later batch knows its prefix is cached.
    pub cached: Arc<Mutex<HashMap<String, Instant>>>,
}

/// How long after a group's last request its prefix counts as cached: under the 5-minute TTL.
const CACHED_FOR: Duration = Duration::from_secs(4 * 60);

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

impl Access {
    fn tools(self) -> &'static str {
        match self {
            Access::ReadOnly => "Read,Grep,Glob,Bash",
            Access::ReadFiles => "Read,Grep,Glob",
            Access::Edit => "Read,Grep,Glob,Bash,Edit,Write",
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
            Access::ReadFiles => ("dontAsk", vec!["Read", "Grep", "Glob"]),
            Access::Edit => (self.fixer_permission_mode.as_str(), vec![]),
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
        let group = group(spec);
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
                        Some("result") => result = Some(event),
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
                        ..usage_outcome(first_usage)
                    });
                }
            }
        }
        let status = child.wait()?;
        let stderr = err_reader.join().unwrap_or_default();
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

    fn runner() -> Runner {
        Runner {
            claude: "claude".into(),
            parallel: 4,
            timeout: Duration::from_secs(60),
            fixer_permission_mode: "bypassPermissions".into(),
            log_dir: "/tmp".into(),
            cached: Default::default(),
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
    fn reviewers_read_and_fixers_edit() {
        let r = runner();
        let args = r.args(&spec(Access::ReadOnly, "opus"));
        assert_eq!(flag(&args, "--permission-mode"), Some("dontAsk"));
        assert!(flag(&args, "--allowedTools")
            .unwrap()
            .contains("Bash(git show:*)"));
        assert_eq!(flag(&args, "--effort"), Some("high"));
        let args = r.args(&spec(Access::Edit, "opus"));
        assert_eq!(flag(&args, "--permission-mode"), Some("bypassPermissions"));
        assert!(flag(&args, "--tools").unwrap().contains("Edit"));
        assert_eq!(flag(&args, "--allowedTools"), None);
        let args = r.args(&spec(Access::ReadFiles, "haiku"));
        assert_eq!(flag(&args, "--tools"), Some("Read,Grep,Glob"));
        assert_eq!(flag(&args, "--effort"), None);
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
