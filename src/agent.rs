//! Runs review agents as headless `claude -p` sessions, each as lean as a review allows: no user or
//! project settings (so no CLAUDE.md, memory, hooks, plugins or skills), no MCP servers, only the
//! tools its role needs, and the 5-minute prompt cache. Agents of one role share a system prompt
//! that holds the change itself, so the first agent's request caches it and the rest read it at a
//! tenth of the price: a batch starts one agent and holds the others until its first response.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
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
    /// The system prompt file, shared by every agent of a role in a pass.
    pub system: PathBuf,
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
    /// The agent's final reply.
    pub text: String,
    pub cost_usd: f64,
    pub turns: u64,
    pub seconds: u64,
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
}

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
    /// The agent's first response is back, so the shared system prompt is cached.
    Warm,
    Done(usize, Outcome),
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
        let (tools, mode, allowed): (&str, &str, Vec<&str>) = match spec.access {
            Access::ReadOnly => (
                "Read,Grep,Glob,Bash",
                "dontAsk",
                ["Read", "Grep", "Glob"]
                    .into_iter()
                    .chain(READ_ONLY_BASH.iter().copied())
                    .collect(),
            ),
            Access::ReadFiles => ("Read,Grep,Glob", "dontAsk", vec!["Read", "Grep", "Glob"]),
            Access::Edit => (
                "Read,Grep,Glob,Bash,Edit,Write",
                self.fixer_permission_mode.as_str(),
                vec![],
            ),
        };
        args.extend([
            "--tools".into(),
            tools.into(),
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
                Ok(Event::Warm) => continue,
                Err(_) => return Outcome::default(),
            }
        }
    }

    /// Runs every agent, at most `parallel` at once, and returns their outcomes in order. The
    /// first one runs alone until its first response, so the others find their shared system
    /// prompt cached.
    pub fn run_all(&self, specs: Vec<Spec>) -> Vec<Outcome> {
        let n = specs.len();
        let mut out: Vec<Option<Outcome>> = vec![None; n];
        let (tx, rx) = mpsc::channel();
        let mut queue = specs.into_iter().enumerate();
        let mut running = 0;
        if let Some((i, spec)) = queue.next() {
            self.spawn(i, spec, tx.clone());
            running += 1;
        }
        // Any event (the first response, or a finished agent) means the prompt is cached.
        while running > 0 {
            match rx.recv() {
                Ok(Event::Warm) => {}
                Ok(Event::Done(i, outcome)) => {
                    out[i] = Some(outcome);
                    running -= 1;
                }
                Err(_) => break,
            }
            while running < self.parallel.max(1) {
                let Some((i, spec)) = queue.next() else { break };
                self.spawn(i, spec, tx.clone());
                running += 1;
            }
        }
        out.into_iter().map(Option::unwrap_or_default).collect()
    }

    fn spawn(&self, i: usize, spec: Spec, tx: mpsc::Sender<Event>) {
        let runner = self.clone();
        thread::spawn(move || {
            let label = spec.label.clone();
            let started = Instant::now();
            let mut outcome = runner.exec(&spec, &tx).unwrap_or_else(|e| Outcome {
                error: Some(format!("{e:#}")),
                ..Outcome::default()
            });
            outcome.label = label;
            outcome.seconds = started.elapsed().as_secs();
            runner.save(&outcome);
            let _ = tx.send(Event::Done(i, outcome));
        });
    }

    fn exec(&self, spec: &Spec, tx: &mpsc::Sender<Event>) -> Result<Outcome> {
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
        let mut warmed = false;
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
                            if !warmed {
                                warmed = true;
                                let _ = tx.send(Event::Warm);
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
                        ..Outcome::default()
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
                ..Outcome::default()
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
            ..Outcome::default()
        })
    }

    fn save(&self, outcome: &Outcome) {
        let _ = std::fs::create_dir_all(&self.log_dir);
        let mut text = format!(
            "<!-- {} · ${:.4} · {} turns · {}s{} -->\n\n",
            outcome.label,
            outcome.cost_usd,
            outcome.turns,
            outcome.seconds,
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
}
