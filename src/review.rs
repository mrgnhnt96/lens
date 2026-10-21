//! `lens review`: a whole review pass, orchestrated here rather than by a model. Every judgment
//! about the code still comes from an agent with a fresh context (see `agent.rs`); what this module
//! decides is plumbing that doesn't need one: which reviewers run, merging duplicate findings,
//! which findings get verified, the report, and the blocker fix loop.
//!
//! The local pass keeps cost down three ways:
//! - **Tiers.** A change that touches only docs isn't reviewed. A small change (`[review]
//!   small_lines`) gets the correctness reviewer only. Otherwise correctness, testing and, when
//!   there's a task, intent-alignment. No lead reviewer and no focus-area reviewers locally: those
//!   are the PR pass's.
//! - **Deltas.** When the last report reviewed this branch from the same merge base, only what
//!   changed since then is reviewed, and blockers it left unfixed are verified again.
//! - **Fresh reports** aren't redone: an unchanged tree only resumes the fix loop.
//!
//! Every agent of a pass gets the same system prompt (a short preamble and the change), and its
//! role's instructions in its first message, so agents with the same model and tools share one
//! cached copy of the change.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::agent::{self, Access, Outcome, Runner, Spec};
use crate::config::{self, Config};
use crate::diff::git;
use crate::fix;
use crate::pr;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub repo: PathBuf,
    /// The base ref; `prepare.sh` picks origin's default branch, then main, then master.
    pub base: Option<String>,
    /// A ticket or spec the change implements, for intent-alignment.
    pub intent: Option<PathBuf>,
    /// Report blockers without running the fix loop.
    pub no_fix: bool,
    /// Review the whole change even when an earlier report would allow a delta.
    pub full: bool,
    /// Run the PR pass on this PR instead of the local pass.
    pub pr: Option<u64>,
    /// PR pass: build the review but don't post it.
    pub dry_run: bool,
    /// The per-PR floor: one Sonnet correctness reviewer, only when the change touches a
    /// `[floor] risky` path.
    pub floor: bool,
}

/// What `lens review` did, for the caller to print.
pub struct Done {
    pub summary: String,
    pub report: Option<PathBuf>,
}

/// The floor's scope note: a cheap per-PR check reports only what this change caused.
const FLOOR_SCOPE: &str = "\nThis is the per-PR floor check on paths the repo marked risky. Raise a finding only when this change caused it: code the diff adds or alters, or code it newly breaks. Leave problems that were already there.\n";

/// The changed files that match a `[floor] risky` glob.
pub fn risky_files<'a>(globs: &[String], files: &'a [String]) -> Result<Vec<&'a str>> {
    let mut set = globset::GlobSetBuilder::new();
    for g in globs {
        set.add(globset::Glob::new(g).with_context(|| format!("[floor] risky glob {g:?}"))?);
    }
    let set = set.build()?;
    Ok(files
        .iter()
        .map(String::as_str)
        .filter(|f| set.is_match(f))
        .collect())
}

const LOCAL_PROFILES: &[&str] = &["correctness", "testing", "intent-alignment"];
pub(crate) const LEAD: &str = "impact-assessment";
const ROLES: &[&str] = &["reviewer", "verifier", "fixer", "escalator"];
/// Above this many bytes the diff isn't put in the prompt; agents read the file instead.
const INLINE_DIFF_MAX: usize = 150_000;
const INLINE_MAP_MAX: usize = 60_000;
const INLINE_GUIDELINE_MAX: usize = 20_000;

/// A finding as a reviewer raised it, after merging duplicates.
#[derive(Debug, Clone)]
pub(crate) struct Finding {
    pub(crate) profiles: Vec<String>,
    pub(crate) file: String,
    pub(crate) line: u64,
    pub(crate) severity: String,
    pub(crate) title: String,
    pub(crate) failure_scenario: String,
    pub(crate) confidence: f64,
    /// A blocker the last report left unfixed, verified again in a delta review.
    pub(crate) carried: bool,
}

impl Finding {
    pub(crate) fn from_reply(profile: &str, v: &Value) -> Option<Finding> {
        Some(Finding {
            profiles: vec![profile.to_string()],
            file: v["file"].as_str()?.to_string(),
            line: v["line"].as_u64().unwrap_or(0),
            severity: v["severity"].as_str().unwrap_or("minor").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            failure_scenario: v["failureScenario"].as_str().unwrap_or("").to_string(),
            confidence: v["confidence"].as_f64().unwrap_or(0.5),
            carried: false,
        })
    }

    pub(crate) fn json(&self) -> Value {
        json!({
            "profiles": self.profiles,
            "file": self.file,
            "line": self.line,
            "severity": self.severity,
            "title": self.title,
            "failureScenario": self.failure_scenario,
            "confidence": self.confidence,
        })
    }

    fn for_verifier(&self) -> String {
        format!(
            "Finding (from the {} reviewer{}):\n  file: {}\n  line: {}\n  severity: {}\n  title: {}\n  failureScenario: {}\n",
            self.profiles.join(", "),
            if self.carried {
                "; an earlier review raised it and it may be fixed by now"
            } else {
                ""
            },
            self.file,
            self.line,
            self.severity,
            self.title,
            self.failure_scenario
        )
    }
}

pub fn rank(severity: &str) -> u8 {
    match severity {
        "blocker" => 0,
        "major" => 1,
        "minor" => 2,
        _ => 3,
    }
}

/// Merges findings several reviewers raised: same file, lines within 3 and the same line or a
/// title in common. Keeps the highest severity and confidence and every profile that raised it.
pub(crate) fn merge(findings: Vec<Finding>) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for f in findings {
        let dup = out.iter_mut().find(|o| same(o, &f));
        match dup {
            Some(o) => {
                if rank(&f.severity) < rank(&o.severity) {
                    o.severity = f.severity.clone();
                    o.title = f.title.clone();
                    o.failure_scenario = f.failure_scenario.clone();
                }
                o.confidence = o.confidence.max(f.confidence);
                o.carried &= f.carried;
                for p in f.profiles {
                    if !o.profiles.contains(&p) {
                        o.profiles.push(p);
                    }
                }
            }
            None => out.push(f),
        }
    }
    out
}

/// The same failure: same file, lines within 3, and the same line or a title in common.
fn same(a: &Finding, b: &Finding) -> bool {
    a.file == b.file
        && a.line.abs_diff(b.line) <= 3
        && (a.line == b.line || similar(&a.title, &b.title))
}

fn similar(a: &str, b: &str) -> bool {
    let words = |s: &str| -> BTreeSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(str::to_lowercase)
            .collect()
    };
    let (a, b) = (words(a), words(b));
    let common = a.intersection(&b).count();
    let all = a.union(&b).count();
    all > 0 && common * 10 >= all * 3
}

/// A path that holds no code: docs and images. A change of only these isn't reviewed.
pub fn is_doc(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ["license", "licence", "changelog", "notice", "authors"]
        .iter()
        .any(|n| name.starts_with(n))
    {
        return true;
    }
    matches!(
        Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "md" | "markdown"
            | "mdx"
            | "txt"
            | "rst"
            | "adoc"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "svg"
            | "webp"
            | "ico"
            | "pdf"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Docs only: nothing to review.
    Skip,
    /// Correctness only.
    Small,
    Full,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Tier::Skip => "skip",
            Tier::Small => "small",
            Tier::Full => "full",
        }
    }
}

pub fn tier(files: &[String], changed_lines: u32, small_lines: u32) -> Tier {
    if files.iter().all(|f| is_doc(f)) {
        Tier::Skip
    } else if changed_lines <= small_lines {
        Tier::Small
    } else {
        Tier::Full
    }
}

pub fn changed_lines(diff: &str) -> u32 {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++ ")
                && !l.starts_with("--- ")
        })
        .count() as u32
}

/// One profile's place in the pass: run, with its file and model, or skipped with why.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) reason: String,
    pub(crate) run: bool,
    pub(crate) max_minions: u32,
}

impl Plan {
    fn from_picked(p: &Value, run: bool, reason: String) -> Plan {
        Plan {
            name: p["name"].as_str().unwrap_or("").to_string(),
            path: PathBuf::from(p["path"].as_str().unwrap_or("")),
            model: p["model"].as_str().unwrap_or("sonnet").to_string(),
            effort: p["effort"].as_str().unwrap_or("high").to_string(),
            reason,
            run,
            max_minions: p["maxMinions"].as_u64().unwrap_or(0) as u32,
        }
    }

    pub(crate) fn json(&self, findings: Option<usize>) -> Value {
        let mut v = json!({ "name": self.name, "ran": self.run, "reason": self.reason });
        if self.run {
            v["model"] = json!(self.model);
            if let Some(n) = findings {
                v["findings"] = json!(n);
            }
        }
        v
    }
}

/// The local pass's profiles: correctness always; testing and intent-alignment on a full tier,
/// intent-alignment only with a task or intent. Others are left to the PR pass.
fn plan_local(profiles: &Value, tier: Tier, has_intent: bool) -> Vec<Plan> {
    let picked = profiles["picked"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for name in LOCAL_PROFILES {
        let Some(p) = picked.iter().find(|p| p["name"] == *name) else {
            continue;
        };
        let (run, reason) = match (*name, tier) {
            (_, Tier::Skip) => (false, "docs only".to_string()),
            ("correctness", _) => (true, "always".to_string()),
            (_, Tier::Small) => (false, "small change".to_string()),
            ("intent-alignment", _) if !has_intent => (false, "no task context".to_string()),
            _ => (true, reason_of(p)),
        };
        out.push(Plan::from_picked(p, run, reason));
    }
    out
}

/// The PR pass's profiles: every one lens picked. An `infer` profile runs too (testing only when
/// the change has code), since nothing here asks a model to judge its rules.
pub(crate) fn plan_pr(profiles: &Value, files: &[String]) -> Vec<Plan> {
    let has_code = files.iter().any(|f| !is_doc(f));
    profiles["picked"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let skip = !has_code && p["name"] != "intent-alignment";
            if skip {
                Plan::from_picked(p, false, "docs only".into())
            } else {
                Plan::from_picked(p, true, reason_of(p))
            }
        })
        .collect()
}

fn reason_of(p: &Value) -> String {
    if p["decision"] == "infer" {
        "inferred".into()
    } else {
        p["reasons"]
            .as_array()
            .and_then(|r| r.first())
            .and_then(Value::as_str)
            .unwrap_or("selected")
            .to_string()
    }
}

/// Everything one pass works from.
pub(crate) struct Pass {
    pub(crate) opts: Options,
    /// lens's own files (`assets::root`).
    pub(crate) assets: PathBuf,
    pub(crate) root: PathBuf,
    /// Where agents read the code: the repo, the PR's tree or the sweep's.
    pub(crate) tree_dir: PathBuf,
    /// The folder `context` reads the change from (diff.patch, map.json, profiles.json).
    pub(crate) work: PathBuf,
    pub(crate) report_path: PathBuf,
    pub(crate) prepared: Value,
    pub(crate) cfg: Config,
    pub(crate) runner: Runner,
    /// Each role's instructions (`prompts/<role>.md`), sent as an agent's first message.
    roles: HashMap<&'static str, String>,
    started: Instant,
    pub(crate) started_at: String,
    cost: f64,
    /// One row per agent run, for the report's `agents`.
    agents: Vec<Value>,
    seconds: serde_json::Map<String, Value>,
}

impl Pass {
    /// A pass over the change `prepared` describes, its agents logged under `work/agents`.
    pub(crate) fn new(
        opts: Options,
        root: PathBuf,
        tree_dir: PathBuf,
        work: PathBuf,
        report_path: PathBuf,
        prepared: Value,
    ) -> Result<Pass> {
        let assets = crate::assets::root()?;
        let cfg = config::load(&root)?;
        let runner = Runner {
            claude: agent::claude_program(),
            parallel: cfg.review.parallel,
            timeout: Duration::from_secs(cfg.review.timeout_mins.max(1) * 60),
            fixer_permission_mode: cfg.review.fixer_permission_mode.clone(),
            log_dir: work.join("agents"),
            cached: Default::default(),
        };
        let mut roles = HashMap::new();
        for role in ROLES {
            roles.insert(*role, agent::role_prompt(&assets, role)?);
        }
        Ok(Pass {
            opts,
            assets,
            root,
            tree_dir,
            work,
            report_path,
            prepared,
            cfg,
            runner,
            roles,
            started: Instant::now(),
            started_at: iso_now(),
            cost: 0.0,
            agents: Vec::new(),
            seconds: serde_json::Map::new(),
        })
    }

    pub(crate) fn log(&self, msg: &str) {
        eprintln!("lens: {msg}");
    }

    pub(crate) fn lap(&mut self, name: &str, since: Instant) {
        self.seconds
            .insert(name.into(), json!(since.elapsed().as_secs()));
    }

    pub(crate) fn tally(&mut self, outcomes: &[Outcome]) {
        for o in outcomes {
            self.cost += o.cost_usd;
            let mut row = json!({
                "label": o.label,
                "model": o.model,
                "tools": o.tools,
                "costUsd": o.cost_usd,
                "turns": o.turns,
                "seconds": o.seconds,
                "cacheCreationInputTokens": o.cache_creation,
                "cacheReadInputTokens": o.cache_read,
            });
            if let Some(e) = &o.error {
                row["error"] = json!(e);
            }
            self.agents.push(row);
        }
    }

    fn base(&self) -> &str {
        self.prepared["base"].as_str().unwrap_or("")
    }

    fn merge_base(&self) -> &str {
        self.prepared["mergeBase"].as_str().unwrap_or("")
    }

    /// The change as every agent of the pass sees it: the diff, its map, the task and the repo's
    /// guidelines.
    pub(crate) fn context(&self, delta: Option<&str>) -> Result<String> {
        let mut s = format!(
            "# The change under review\n\nRepo: {} (your working directory). Base: {}, merge base {}.\nWork folder: {}\n",
            self.tree_dir.display(),
            self.base(),
            self.merge_base(),
            self.work.display()
        );
        let diff = fs::read_to_string(self.work.join("diff.patch")).unwrap_or_default();
        if diff.len() <= INLINE_DIFF_MAX {
            s.push_str(&format!(
                "\n## The diff (diff.patch)\n\n```diff\n{diff}```\n"
            ));
        } else {
            s.push_str(&format!(
                "\n## The diff\n\nIt's too large to include here ({} KB): read {} in parts.\n",
                diff.len() / 1024,
                self.work.join("diff.patch").display()
            ));
        }
        if let Some(delta) = delta {
            s.push_str(&format!(
                "\n## What changed since the last review (delta.patch)\n\nThe change was reviewed before. Review only what this delta causes or exposes; the rest was already reviewed.\n\n```diff\n{delta}```\n"
            ));
        }
        match fs::read_to_string(self.work.join("map.json")) {
            Ok(map) if map.len() <= INLINE_MAP_MAX => {
                s.push_str(&format!("\n## The change map (map.json)\n\nChanged symbols, their callers and the tests that cover them.\n\n```json\n{map}\n```\n"))
            }
            Ok(_) => s.push_str(&format!(
                "\n## The change map\n\nToo large to include: read {}.\n",
                self.work.join("map.json").display()
            )),
            Err(_) => s.push_str("\n## The change map\n\nNone: the map provider failed. Grep for callers and tests yourself.\n"),
        }
        for name in ["task.json", "intent.md"] {
            if let Ok(text) = fs::read_to_string(self.work.join(name)) {
                s.push_str(&format!("\n## The task ({name})\n\n{}\n", text.trim()));
            }
        }
        for path in self.guidelines()? {
            let text = fs::read_to_string(self.tree_dir.join(&path)).unwrap_or_default();
            let text: String = text.chars().take(INLINE_GUIDELINE_MAX).collect();
            s.push_str(&format!("\n## Repo guideline: {path}\n\n{}\n", text.trim()));
        }
        Ok(s)
    }

    /// `CLAUDE.md` and `AGENTS.md` at the repo root and in the changed files' folders.
    fn guidelines(&self) -> Result<Vec<String>> {
        let files: Vec<String> =
            serde_json::from_str::<Value>(&fs::read_to_string(self.work.join("profiles.json"))?)?
                ["changedFiles"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect();
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::from([PathBuf::new()]);
        for f in &files {
            for d in Path::new(f).ancestors().skip(1) {
                dirs.insert(d.to_path_buf());
            }
        }
        let mut out = Vec::new();
        for d in dirs {
            for name in ["CLAUDE.md", "AGENTS.md"] {
                let p = d.join(name);
                if self.tree_dir.join(&p).is_file() {
                    out.push(p.display().to_string());
                }
            }
        }
        Ok(out)
    }

    /// Writes the system prompt every agent of the pass shares: the preamble and `context`. The
    /// file is named after its text, so a different change gets a different file (and cache
    /// group).
    pub(crate) fn system_prompt(&self, context: &str) -> Result<PathBuf> {
        let text = format!(
            "{}\n\n{}",
            agent::role_prompt(&self.assets, "preamble")?,
            context
        );
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let path = self
            .work
            .join("agents")
            .join(format!("change-{:08x}.system.md", hasher.finish() as u32));
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, text)?;
        Ok(path)
    }

    /// An agent of `role`: its first message is the role's instructions, then `prompt`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spec(
        &self,
        label: String,
        role: &str,
        system: &Path,
        prompt: String,
        model: &str,
        effort: &str,
        access: Access,
    ) -> Spec {
        Spec {
            label,
            system: system.to_path_buf(),
            prompt: format!(
                "<role name=\"{role}\">\n{}\n</role>\n\n{prompt}",
                self.roles[role]
            ),
            model: model.to_string(),
            effort: Some(effort.to_string()),
            access,
            cwd: self.tree_dir.clone(),
            add_dirs: if self.work.starts_with(&self.tree_dir) {
                vec![]
            } else {
                vec![self.work.clone()]
            },
        }
    }

    pub(crate) fn reviewer_prompt(&self, plan: &Plan, extra: &str) -> Result<String> {
        let profile = fs::read_to_string(&plan.path)
            .with_context(|| format!("reading {}", plan.path.display()))?;
        Ok(format!(
            "Review the change through this profile.\n\n<profile name=\"{}\">\n{}\n</profile>\n{extra}",
            plan.name,
            profile.trim()
        ))
    }

    /// Verifies `findings`; returns `(confirmed, dropped)`, each as report JSON.
    pub(crate) fn verify(
        &mut self,
        system: &Path,
        findings: &[Finding],
        keep: impl Fn(&str) -> bool,
    ) -> (Vec<(Finding, Value)>, Vec<Value>) {
        if findings.is_empty() {
            return (vec![], vec![]);
        }
        let specs = findings
            .iter()
            .enumerate()
            .map(|(i, f)| {
                self.spec(
                    format!("verifier-{}", i + 1),
                    "verifier",
                    system,
                    f.for_verifier(),
                    "sonnet",
                    "high",
                    Access::ReadOnly,
                )
            })
            .collect();
        let t = Instant::now();
        self.log(&format!("verifying {} finding(s)", findings.len()));
        let outcomes = self.runner.run_all(specs);
        self.tally(&outcomes);
        self.lap("verify", t);
        let mut confirmed = Vec::new();
        let mut dropped = Vec::new();
        for (f, o) in findings.iter().zip(outcomes) {
            let reply = fix::parse_reply(&o.text).ok();
            let verdict = reply.as_ref().and_then(|r| r["verdict"].as_str());
            let mut entry = f.json();
            match (verdict, &reply) {
                (Some("confirmed"), Some(r)) => {
                    let severity = r["severity"].as_str().unwrap_or(&f.severity).to_string();
                    if !keep(&severity) {
                        entry["severity"] = json!(severity);
                        entry["reason"] = json!("below major");
                        dropped.push(entry);
                        continue;
                    }
                    entry["severity"] = json!(severity);
                    if let Some(line) = r["line"].as_u64() {
                        entry["line"] = json!(line);
                    }
                    entry["howItFails"] = r["howItFails"].clone();
                    confirmed.push((f.clone(), entry));
                }
                (Some(_), Some(r)) => {
                    entry["reason"] = json!(format!(
                        "unverified: {}",
                        r["reason"].as_str().unwrap_or("dropped")
                    ));
                    dropped.push(entry);
                }
                _ => {
                    entry["reason"] = json!(format!(
                        "unverified: the verifier's reply couldn't be read{}",
                        o.error.map(|e| format!(" ({e})")).unwrap_or_default()
                    ));
                    dropped.push(entry);
                }
            }
        }
        (confirmed, dropped)
    }
}

/// Runs a pass and returns what to print.
pub fn run(opts: Options) -> Result<Done> {
    let assets = crate::assets::root()?;
    let root = PathBuf::from(git(&opts.repo, &["rev-parse", "--show-toplevel"])?.trim());
    if let Some(n) = opts.pr {
        let cfg = config::load(&root)?.pr;
        if !cfg.samples(n) {
            return Ok(Done {
                summary: format!(
                    "lens: PR #{n} isn't in the sample ([pr] sample = {}), nothing to review.",
                    cfg.sample
                ),
                report: None,
            });
        }
    }
    let prepared = prepare(&assets, &root, &opts)?;
    let work = PathBuf::from(
        prepared["work"]
            .as_str()
            .context("prepare printed no work folder")?,
    );
    let report_path = PathBuf::from(
        prepared["report"]
            .as_str()
            .context("prepare printed no report path")?,
    );
    let tree_dir = prepared["tree"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let mut pass = Pass::new(opts, root, tree_dir, work, report_path, prepared)?;
    if let Some(intent) = &pass.opts.intent {
        fs::copy(intent, pass.work.join("intent.md"))
            .with_context(|| format!("copying {}", intent.display()))?;
    }
    if pass.opts.pr.is_some() {
        pr_pass(&mut pass)
    } else {
        local_pass(&mut pass)
    }
}

fn prepare(assets: &Path, root: &Path, opts: &Options) -> Result<Value> {
    let scripts = assets.join("scripts");
    let mut cmd = match opts.pr {
        Some(n) => {
            let mut c = Command::new(scripts.join("prepare-pr.sh"));
            c.arg(n.to_string());
            c
        }
        None => {
            let mut c = Command::new(scripts.join("prepare.sh"));
            c.args(opts.base.iter());
            c
        }
    };
    cmd.current_dir(root)
        .env("LENS", std::env::current_exe()?)
        .env(
            "LENS_PROFILES_DIR",
            std::env::var_os("LENS_PROFILES_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| assets.join("profiles")),
        );
    let out = cmd.output().context("running the prepare script")?;
    if !out.status.success() {
        bail!(
            "prepare failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("reading the prepare script's summary")
}

/// The tree an earlier report reviewed: its last fix attempt's, else the one it recorded, else
/// `head`'s when the tree was clean.
fn reviewed_tree(root: &Path, report: &Value) -> Option<String> {
    let fixed = report["findings"].as_array().and_then(|fs| {
        fs.iter().rev().find_map(|f| {
            let ledger = fs::read_to_string(f["fix"]["ledger"].as_str()?).ok()?;
            let ledger: Value = serde_json::from_str(&ledger).ok()?;
            ledger["attempts"].as_array()?.last()?["after"]
                .as_str()
                .map(String::from)
        })
    });
    if fixed.is_some() {
        return fixed;
    }
    if let Some(tree) = report["tree"].as_str() {
        return Some(tree.to_string());
    }
    if report["dirty"] == false {
        let head = report["head"].as_str()?;
        return git(root, &["rev-parse", &format!("{head}^{{tree}}")])
            .ok()
            .map(|t| t.trim().to_string());
    }
    None
}

/// A finding of the last report, raised again in a delta review.
fn carry(f: &Value) -> Option<Finding> {
    let profiles: Vec<String> = f["profiles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut x = Finding::from_reply(profiles.first().map_or("correctness", String::as_str), f)?;
    x.profiles = profiles;
    x.carried = true;
    Some(x)
}

enum Mode {
    /// Nothing changed since the last report.
    Fresh(Value),
    /// Review only what changed since `from`; the previous report's findings carry over.
    Delta {
        from: String,
        patch: String,
        files: Vec<String>,
        previous: Value,
    },
    Full,
}

fn local_pass(pass: &mut Pass) -> Result<Done> {
    let prepared = pass.prepared.clone();
    let changed = prepared["changedLines"].as_u64().unwrap_or(0) as u32;
    if changed == 0 {
        return Ok(Done {
            summary: "lens: nothing to review: the branch has no changes against its base.".into(),
            report: None,
        });
    }
    let mut floor_reason = None;
    if pass.opts.floor {
        let changed_files: Vec<String> = fs::read_to_string(pass.work.join("profiles.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|p| p["changedFiles"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.as_str().map(String::from))
            .collect();
        let risky = risky_files(&pass.cfg.floor.risky, &changed_files)?;
        if risky.is_empty() {
            return Ok(Done {
                summary: "lens: floor: no risky paths changed, nothing to review.".into(),
                report: None,
            });
        }
        floor_reason = Some(format!("risky path: {}", risky.join(", ")));
    }
    let now = fix::snapshot(&pass.root)?;
    let previous: Option<Value> = fs::read_to_string(&pass.report_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|r: &Value| r["pass"] == "local" && !pass.opts.floor);
    let mode = match previous {
        // A report some reviewers failed on doesn't count: the next pass reviews everything.
        Some(prev)
            if prev["base"] == prepared["base"]
                && prev["mergeBase"] == prepared["mergeBase"]
                && prev["incomplete"].as_array().is_none_or(Vec::is_empty) =>
        {
            match reviewed_tree(&pass.root, &prev) {
                Some(t) if t == now && !pass.opts.full => Mode::Fresh(prev),
                Some(t) if !pass.opts.full => match git(
                    &pass.root,
                    &["diff", "--no-color", "--no-ext-diff", &t, &now],
                ) {
                    Ok(patch) => {
                        let files = git(
                            &pass.root,
                            &["diff", "--name-only", "--no-renames", &t, &now],
                        )?
                        .lines()
                        .map(String::from)
                        .collect();
                        Mode::Delta {
                            from: t,
                            patch,
                            files,
                            previous: prev,
                        }
                    }
                    Err(_) => Mode::Full,
                },
                _ => Mode::Full,
            }
        }
        _ => Mode::Full,
    };

    if let Mode::Fresh(prev) = mode {
        pass.log("the last review still matches the tree; resuming its fix loop if it has one");
        if !pass.opts.no_fix {
            fix_loop(pass, &prev, None)?;
        }
        let report: Value = serde_json::from_str(&fs::read_to_string(&pass.report_path)?)?;
        return Ok(Done {
            summary: summarize(&report, &pass.report_path, true),
            report: Some(pass.report_path.clone()),
        });
    }

    // A new review replaces the last one: keep it beside the work folder for reference.
    let prev_dir = pass.work.join("prev");
    let _ = fs::remove_dir_all(&prev_dir);
    if pass.report_path.is_file() {
        fs::create_dir_all(&prev_dir)?;
        fs::rename(&pass.report_path, prev_dir.join("report.json"))?;
        if pass.work.join("fix").is_dir() {
            fs::rename(pass.work.join("fix"), prev_dir.join("fix"))?;
        }
    }
    let _ = fs::remove_dir_all(pass.work.join("agents"));

    let profiles: Value =
        serde_json::from_str(&fs::read_to_string(pass.work.join("profiles.json"))?)?;
    let all_files: Vec<String> = profiles["changedFiles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();
    // A delta carries the last report's findings over. Unfixed blockers, and majors in files the
    // delta touched, are verified again; other majors are kept as they were.
    let mut kept: Vec<(Finding, Value)> = Vec::new();
    let (files, lines, delta_patch, delta_info, carried) = match &mode {
        Mode::Delta {
            from,
            patch,
            files,
            previous,
        } => {
            let mut carried: Vec<Finding> = Vec::new();
            for f in previous["findings"].as_array().cloned().unwrap_or_default() {
                if f["severity"] == "blocker" && f["fix"]["status"] == "resolved" {
                    continue;
                }
                let touched = files.iter().any(|p| f["file"] == *p.as_str());
                let Some(x) = carry(&f) else { continue };
                if f["severity"] == "blocker" || touched {
                    carried.push(x);
                } else {
                    let mut entry = f.clone();
                    if let Some(o) = entry.as_object_mut() {
                        o.remove("id");
                        o.remove("fix");
                    }
                    kept.push((x, entry));
                }
            }
            let lines = changed_lines(patch);
            (
                files.clone(),
                lines,
                Some(patch.clone()),
                Some(json!({ "from": from, "changedLines": lines })),
                carried,
            )
        }
        _ => (all_files, changed, None, None, vec![]),
    };
    let tier = tier(&files, lines, pass.cfg.review.small_lines);
    let has_intent = pass.work.join("task.json").is_file() || pass.work.join("intent.md").is_file();
    let mut plans = plan_local(&profiles, tier, has_intent);
    if let Some(reason) = &floor_reason {
        plans.retain(|p| p.name == "correctness");
        for p in &mut plans {
            p.model = "sonnet".into();
            p.reason = reason.clone();
            p.run = true;
        }
    }
    if let Some(d) = &delta_info {
        pass.log(&format!(
            "reviewing the {} changed lines since the last review ({} tier)",
            d["changedLines"],
            tier.name()
        ));
    } else {
        pass.log(&format!(
            "reviewing {lines} changed lines ({} tier)",
            tier.name()
        ));
    }

    let context = pass.context(delta_patch.as_deref())?;
    let system = pass.system_prompt(&context)?;

    let running: Vec<&Plan> = plans.iter().filter(|p| p.run).collect();
    let mut specs = Vec::new();
    for plan in &running {
        specs.push(pass.spec(
            format!("reviewer-{}", plan.name),
            "reviewer",
            &system,
            pass.reviewer_prompt(plan, if pass.opts.floor { FLOOR_SCOPE } else { "" })?,
            &plan.model,
            &plan.effort,
            Access::ReadOnly,
        ));
    }
    let t = Instant::now();
    if !specs.is_empty() {
        pass.log(&format!(
            "running {}",
            running
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let outcomes = pass.runner.run_all(specs);
    pass.tally(&outcomes);
    pass.lap("reviewers", t);

    let mut raised = Vec::new();
    let mut profile_rows = Vec::new();
    let mut failed = Vec::new();
    let mut it = outcomes.iter();
    for plan in &plans {
        if !plan.run {
            profile_rows.push(plan.json(None));
            continue;
        }
        let o = it.next().unwrap();
        match fix::parse_reply(&o.text) {
            Ok(reply) => {
                let fs: Vec<Finding> = reply["findings"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|f| Finding::from_reply(&plan.name, f))
                    .collect();
                profile_rows.push(plan.json(Some(fs.len())));
                raised.extend(fs);
            }
            Err(_) => {
                let mut row = plan.json(None);
                row["failed"] = json!(o
                    .error
                    .clone()
                    .unwrap_or_else(|| "no readable findings".into()));
                profile_rows.push(row);
                failed.push(plan.name.clone());
            }
        }
    }

    let (to_verify, mut dropped): (Vec<Finding>, Vec<Value>) = {
        // A kept finding a reviewer raised again is verified with the new one instead.
        kept.retain(|(k, _)| !raised.iter().any(|r| same(k, r)));
        let merged = merge(raised.into_iter().chain(carried).collect());
        let mut keep = Vec::new();
        let mut drop = Vec::new();
        for f in merged {
            if rank(&f.severity) <= 1 {
                keep.push(f);
            } else {
                let mut e = f.json();
                e["reason"] = json!("below major");
                drop.push(e);
            }
        }
        (keep, drop)
    };
    let (mut confirmed, unverified) = pass.verify(&system, &to_verify, |s| rank(s) <= 1);
    dropped.extend(unverified);
    confirmed.extend(kept);

    let mut report = json!({
        "version": 1,
        "pass": "local",
        "branch": prepared["branch"],
        "base": prepared["base"],
        "mergeBase": prepared["mergeBase"],
        "head": prepared["head"],
        "dirty": prepared["dirty"],
        "tree": now,
        "changedLines": changed,
        "mode": if delta_info.is_some() { "delta" } else { "full" },
        "tier": if pass.opts.floor { "floor" } else { tier.name() },
        "startedAt": pass.started_at,
        "map": map_status(&pass.work),
        "profiles": profile_rows,
        "findings": number(confirmed),
        "dropped": dropped,
    });
    if let Some(d) = delta_info {
        report["delta"] = d;
    }
    if !failed.is_empty() {
        report["incomplete"] = json!(failed);
    }
    finish(pass, &mut report)?;

    if !pass.opts.no_fix && !pass.opts.floor {
        fix_loop(pass, &report, None)?;
        report = serde_json::from_str(&fs::read_to_string(&pass.report_path)?)?;
        finish(pass, &mut report)?;
    }
    let mut summary = summarize(&report, &pass.report_path, false);
    if !failed.is_empty() {
        summary.push_str(&format!(
            "\nFailed (no readable reply): {}.",
            failed.join(", ")
        ));
    }
    Ok(Done {
        summary,
        report: Some(pass.report_path.clone()),
    })
}

/// Ids F1, F2… in report order, blockers first.
fn number(mut confirmed: Vec<(Finding, Value)>) -> Vec<Value> {
    confirmed.sort_by_key(|(_, v)| rank(v["severity"].as_str().unwrap_or("nit")));
    confirmed
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut v))| {
            let obj = v.as_object_mut().unwrap();
            let mut out = serde_json::Map::new();
            out.insert("id".into(), json!(format!("F{}", i + 1)));
            out.extend(std::mem::take(obj));
            Value::Object(out)
        })
        .collect()
}

pub(crate) fn map_status(work: &Path) -> Value {
    let provider = fs::read_to_string(work.join("map.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|m| m["provider"].as_str().map(String::from));
    let error = fs::read_to_string(work.join("map.err"))
        .ok()
        .map(|e| e.trim().to_string());
    json!({ "provider": provider, "error": error })
}

/// Stamps the end time, timings and cost, and writes the report.
pub(crate) fn finish(pass: &mut Pass, report: &mut Value) -> Result<()> {
    let mut seconds = pass.seconds.clone();
    seconds.insert("total".into(), json!(pass.started.elapsed().as_secs()));
    report["finishedAt"] = json!(iso_now());
    report["seconds"] = Value::Object(seconds);
    let cost = (pass.cost * 10000.0).round() / 10000.0;
    let sum = |key: &str| -> u64 { pass.agents.iter().filter_map(|a| a[key].as_u64()).sum() };
    report["totals"] = json!({
        "agents": pass.agents.len(),
        "costUsd": cost,
        "cacheCreationInputTokens": sum("cacheCreationInputTokens"),
        "cacheReadInputTokens": sum("cacheReadInputTokens"),
    });
    report["agents"] = json!(pass.agents);
    report["costUsd"] = json!(cost);
    fs::write(
        &pass.report_path,
        serde_json::to_string_pretty(report)? + "\n",
    )
    .with_context(|| format!("writing {}", pass.report_path.display()))
}

/// Takes every blocker of `report` without a settled fix through the fix loop, one at a time;
/// with `only`, just that one.
pub(crate) fn fix_loop(pass: &mut Pass, report: &Value, only: Option<&str>) -> Result<()> {
    let blockers: Vec<Value> = report["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| {
            f["severity"] == "blocker"
                && only.is_none_or(|id| f["id"] == id)
                && !matches!(f["fix"]["status"].as_str(), Some("resolved" | "escalated"))
        })
        .collect();
    if blockers.is_empty() {
        return Ok(());
    }
    let t = Instant::now();
    let system = pass.system_prompt(&pass.context(None)?)?;
    let profiles: Value =
        serde_json::from_str(&fs::read_to_string(pass.work.join("profiles.json"))?)?;
    let guidelines = pass.guidelines()?;

    for b in blockers {
        let id = b["id"].as_str().unwrap_or("F1").to_string();
        pass.log(&format!(
            "fixing blocker {id}: {}",
            b["title"].as_str().unwrap_or("")
        ));
        let mut step = fix::start(&pass.report_path, &id, fix::DEFAULT_CAP)?;
        let finding = Finding::from_reply("correctness", &b).context("reading the blocker")?;
        let finding = Finding {
            profiles: b["profiles"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["correctness".into()]),
            ..finding
        };
        loop {
            match step.next {
                fix::Next::Fix => {
                    let n = step.attempts + 1;
                    let prompt = format!(
                        "Ledger: {}\nFinding {id} ({}): {}:{}  {}\nFailure: {}\nHow it fails:\n{}\nGuidelines: {}\n",
                        step.ledger.display(),
                        finding.profiles.join(", "),
                        finding.file,
                        b["line"],
                        finding.title,
                        finding.failure_scenario,
                        b["howItFails"].as_str().unwrap_or(""),
                        if guidelines.is_empty() { "none".into() } else { guidelines.join(", ") },
                    );
                    let spec = pass.spec(
                        format!("fixer-{id}-{n}"),
                        "fixer",
                        &system,
                        prompt,
                        "sonnet",
                        "high",
                        Access::Edit,
                    );
                    let o = pass.runner.run(spec);
                    pass.tally(std::slice::from_ref(&o));
                    let fixer = fix::parse_reply(&o.text)
                        .unwrap_or_else(|_| json!({ "summary": "(the fixer's reply couldn't be read)", "error": o.error }));
                    let pending = fix::pending_diff(&step.ledger)?;
                    if pending.trim().is_empty() {
                        step = fix::record(&step.ledger, Some(fixer), None)?;
                        continue;
                    }
                    let patch = pass.work.join("fix").join(format!("{id}-pending.patch"));
                    fs::create_dir_all(patch.parent().unwrap())?;
                    fs::write(&patch, &pending)?;
                    let review = rereview(pass, &system, &profiles, &finding, &id, &patch)?;
                    step = fix::record(&step.ledger, Some(fixer), Some(review))?;
                }
                fix::Next::Judge => {
                    let prompt = format!("Ledger: {}\n", step.ledger.display());
                    let mut decision = None;
                    for attempt in 1..=2 {
                        let spec = pass.spec(
                            format!("escalator-{id}-{}-{attempt}", step.attempts),
                            "escalator",
                            &system,
                            prompt.clone(),
                            "haiku",
                            "",
                            Access::ReadFiles,
                        );
                        let o = pass.runner.run(spec);
                        pass.tally(std::slice::from_ref(&o));
                        if let Some(d) = fix::parse_reply(&o.text)
                            .ok()
                            .and_then(|v| serde_json::from_value::<fix::Decision>(v).ok())
                        {
                            decision = Some(d);
                            break;
                        }
                    }
                    let decision = decision.unwrap_or(fix::Decision {
                        decision: fix::Verdict::Escalate,
                        reason: "the judge's reply couldn't be read".into(),
                        needs: Some("other".into()),
                    });
                    step = fix::judge(&step.ledger, decision)?;
                }
                fix::Next::Done | fix::Next::Escalate => break,
            }
        }
        let summary = fix::summary(&step.ledger)?;
        pass.log(summary.lines().next().unwrap_or(""));
    }
    pass.lap("fix", t);
    Ok(())
}

/// Re-reviews one fix attempt: the verifier on the original finding, plus a reviewer per profile
/// that raised it looking only for what the fix breaks; their blockers and majors are verified.
fn rereview(
    pass: &mut Pass,
    system: &Path,
    profiles: &Value,
    finding: &Finding,
    id: &str,
    patch: &Path,
) -> Result<fix::Review> {
    let picked = profiles["picked"].as_array().cloned().unwrap_or_default();
    let extra = format!(
        "\nThis is a re-review of a fix for finding {id} ({}). The fix: {}. Raise only what the fix itself breaks.\n",
        finding.title,
        patch.display()
    );
    let mut specs = vec![pass.spec(
        format!("fix-{id}-verifier"),
        "verifier",
        system,
        finding.for_verifier(),
        "sonnet",
        "high",
        Access::ReadOnly,
    )];
    let mut names = Vec::new();
    for name in &finding.profiles {
        if let Some(p) = picked.iter().find(|p| p["name"] == *name.as_str()) {
            let plan = Plan::from_picked(p, true, String::new());
            specs.push(pass.spec(
                format!("fix-{id}-reviewer-{name}"),
                "reviewer",
                system,
                pass.reviewer_prompt(&plan, &extra)?,
                &plan.model,
                &plan.effort,
                Access::ReadOnly,
            ));
            names.push(name.clone());
        }
    }
    let outcomes = pass.runner.run_all(specs);
    pass.tally(&outcomes);
    let original =
        fix::parse_reply(&outcomes[0].text).unwrap_or_else(|_| json!({ "verdict": "unreadable" }));
    let mut new = Vec::new();
    for (name, o) in names.iter().zip(&outcomes[1..]) {
        if let Ok(reply) = fix::parse_reply(&o.text) {
            new.extend(
                reply["findings"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|f| Finding::from_reply(name, f))
                    .filter(|f| rank(&f.severity) <= 1),
            );
        }
    }
    let (confirmed, _) = pass.verify(system, &merge(new), |s| rank(s) <= 1);
    Ok(fix::Review {
        original,
        new_findings: confirmed.into_iter().map(|(_, v)| v).collect(),
    })
}

fn pr_pass(pass: &mut Pass) -> Result<Done> {
    let number = pass.opts.pr.unwrap();
    let prepared = pass.prepared.clone();
    if prepared["changedLines"].as_u64().unwrap_or(0) == 0 {
        return Ok(Done {
            summary: format!("lens: PR #{number} has no changes to review; posted nothing."),
            report: None,
        });
    }
    let _ = fs::remove_dir_all(pass.work.join("agents"));
    let profiles: Value =
        serde_json::from_str(&fs::read_to_string(pass.work.join("profiles.json"))?)?;
    let files: Vec<String> = profiles["changedFiles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();
    let plans = plan_pr(&profiles, &files);
    let extra = format!(
        "\nRepo: {}  (the PR's head; read files there, not in the current directory)\nAlready posted on this PR (don't raise these again): {}\n",
        pass.tree_dir.display(),
        pass.work.join("posted.json").display()
    );
    let system = pass.system_prompt(&pass.context(None)?)?;

    // The lead first: its assessment goes in the review body and its focus areas get reviewers.
    let mut raised = Vec::new();
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    let mut risk = Value::Null;
    let mut assessment = Value::Null;
    let mut focus: Vec<Value> = Vec::new();
    let mut correctness: Option<Plan> = None;
    let t = Instant::now();
    if let Some(lead) = plans.iter().find(|p| p.name == LEAD && p.run) {
        pass.log("running the lead reviewer (impact-assessment)");
        let prompt = pass.reviewer_prompt(lead, &format!("{extra}\nYou are the lead reviewer. Return the assessment, risk and focusAreas as well as findings.\n"))?;
        let o = pass.runner.run(pass.spec(
            format!("reviewer-{LEAD}"),
            "reviewer",
            &system,
            prompt,
            &lead.model,
            &lead.effort,
            Access::ReadOnly,
        ));
        pass.tally(std::slice::from_ref(&o));
        match fix::parse_reply(&o.text) {
            Ok(r) => {
                risk = r["risk"].clone();
                assessment = r["assessment"].clone();
                focus = r["focusAreas"].as_array().cloned().unwrap_or_default();
                focus.truncate(lead.max_minions as usize);
                let fs: Vec<Finding> = r["findings"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|f| Finding::from_reply(LEAD, f))
                    .collect();
                rows.push(lead.json(Some(fs.len())));
                raised.extend(fs);
            }
            Err(_) => {
                let mut row = lead.json(None);
                row["failed"] = json!(o.error.unwrap_or_else(|| "no readable findings".into()));
                rows.push(row);
                failed.push(LEAD.to_string());
            }
        }
        fs::write(
            pass.work.join("impact.md"),
            assessment.as_str().unwrap_or(""),
        )?;
    }
    pass.lap("lead", t);

    let mut runs: Vec<(String, Plan, String)> = Vec::new();
    for plan in plans.iter().filter(|p| p.name != LEAD) {
        if plan.name == "correctness" {
            correctness = Some(plan.clone());
        }
        if plan.run {
            runs.push((plan.name.clone(), plan.clone(), extra.clone()));
        } else {
            rows.push(plan.json(None));
        }
    }
    if let Some(c) = &correctness {
        for f in &focus {
            let title = f["title"].as_str().unwrap_or("focus").to_string();
            let files = f["files"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let mut plan = c.clone();
            plan.effort = "high".into();
            plan.reason = "lead focus area".into();
            runs.push((
                format!("focus: {title}"),
                plan,
                format!(
                    "{extra}\nFocus only on: {title}, in {files}. Why it's risky: {}.\n",
                    f["why"].as_str().unwrap_or("")
                ),
            ));
        }
    }
    let mut specs = Vec::new();
    for (i, (name, plan, extra)) in runs.iter().enumerate() {
        specs.push(pass.spec(
            format!("reviewer-{}-{}", i + 1, slug(name)),
            "reviewer",
            &system,
            pass.reviewer_prompt(plan, extra)?,
            &plan.model,
            &plan.effort,
            Access::ReadOnly,
        ));
    }
    let t = Instant::now();
    pass.log(&format!("running {} reviewer(s)", specs.len()));
    let outcomes = pass.runner.run_all(specs);
    pass.tally(&outcomes);
    pass.lap("reviewers", t);
    for ((name, plan, _), o) in runs.iter().zip(&outcomes) {
        let mut row = plan.json(None);
        row["name"] = json!(name);
        match fix::parse_reply(&o.text) {
            Ok(r) => {
                let fs: Vec<Finding> = r["findings"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|f| Finding::from_reply(&plan.name, f))
                    .collect();
                row["findings"] = json!(fs.len());
                raised.extend(fs);
            }
            Err(_) => {
                row["failed"] = json!(o
                    .error
                    .clone()
                    .unwrap_or_else(|| "no readable findings".into()));
                failed.push(name.clone());
            }
        }
        rows.push(row);
    }

    let (confirmed, dropped) = pass.verify(&system, &merge(raised), |_| true);
    let pr_info: Value = serde_json::from_str(&fs::read_to_string(pass.work.join("pr.json"))?)
        .unwrap_or(Value::Null);
    let mut report = json!({
        "version": 1,
        "pass": "pr",
        "pr": { "number": number, "url": prepared["url"].as_str().or(pr_info["url"].as_str()) },
        "branch": prepared["branch"],
        "base": prepared["base"],
        "mergeBase": prepared["mergeBase"],
        "head": prepared["head"],
        "dirty": false,
        "changedLines": prepared["changedLines"],
        "startedAt": pass.started_at,
        "map": map_status(&pass.work),
        "risk": risk,
        "assessment": assessment,
        "profiles": rows,
        "findings": number_all(confirmed),
        "dropped": dropped,
    });
    finish(pass, &mut report)?;

    let mut summary = summarize(&report, &pass.report_path, false);
    let cfg = config::load(&pass.root)?.pr;
    let posted = (|| -> Result<String> {
        let gh = pr::Gh::new(&pass.root, &cfg)?;
        let parsed: pr::Report = serde_json::from_value(report.clone())?;
        let patch = fs::read_to_string(pass.work.join("diff.patch"))?;
        let plan = pr::plan(&parsed, &gh.earlier(number)?, &pr::commentable(&patch));
        let url = match (&plan.review, pass.opts.dry_run) {
            (Some(review), false) => gh.post(number, review)?,
            _ => None,
        };
        Ok(match (url, &plan.review) {
            (Some(url), _) => format!(
                "Posted: {url} ({} inline, {} in the body; {} already posted)",
                plan.inline.len(),
                plan.in_body.len(),
                plan.skipped.len()
            ),
            (None, Some(_)) => format!(
                "Dry run: would post {} inline, {} in the body.",
                plan.inline.len(),
                plan.in_body.len()
            ),
            (None, None) => "Nothing new to post.".into(),
        })
    })();
    match posted {
        Ok(line) => summary.push_str(&format!("\n{line}")),
        Err(e) => summary.push_str(&format!(
            "\nPosting failed: {e:#}. Retry with: lens pr post --pr {number} --report {} --diff {}",
            pass.report_path.display(),
            pass.work.join("diff.patch").display()
        )),
    }
    if !failed.is_empty() {
        summary.push_str(&format!(
            "\nFailed (no readable reply): {}.",
            failed.join(", ")
        ));
    }
    Ok(Done {
        summary,
        report: Some(pass.report_path.clone()),
    })
}

pub(crate) fn number_all(confirmed: Vec<(Finding, Value)>) -> Vec<Value> {
    number(confirmed)
}

pub(crate) fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(40)
        .collect()
}

/// The report in a few lines for the human.
pub fn summarize(report: &Value, path: &Path, fresh: bool) -> String {
    let findings = report["findings"].as_array().cloned().unwrap_or_default();
    let count = |s: &str| findings.iter().filter(|f| f["severity"] == s).count();
    let counts: Vec<String> = ["blocker", "major", "minor", "nit"]
        .iter()
        .filter(|s| count(s) > 0)
        .map(|s| format!("{} {s}", count(s)))
        .collect();
    let what = match report["pr"]["number"].as_u64() {
        Some(n) => format!("PR #{n}"),
        None => format!(
            "{} vs {}",
            report["branch"].as_str().unwrap_or("?"),
            report["base"].as_str().unwrap_or("?")
        ),
    };
    let mut head = format!(
        "lens: {} finding{}{} on {what}",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" },
        if counts.is_empty() {
            String::new()
        } else {
            format!(" ({})", counts.join(", "))
        },
    );
    if fresh {
        head.push_str(" · unchanged since the last review");
    } else {
        let secs = report["seconds"]["total"].as_u64().unwrap_or(0);
        head.push_str(&format!(" · {}m {}s", secs / 60, secs % 60));
        if let Some(mode) = report["mode"].as_str() {
            head.push_str(&format!(" · {mode}"));
        }
        if let Some(tier) = report["tier"].as_str() {
            head.push_str(&format!(" ({tier})"));
        }
        head.push_str(&format!(
            " · {} agents, ${:.2}",
            report["totals"]["agents"]
                .as_u64()
                .or(report["agents"].as_u64())
                .unwrap_or(0),
            report["costUsd"].as_f64().unwrap_or(0.0)
        ));
    }
    if let Some(risk) = report["risk"].as_str() {
        head.push_str(&format!(" · risk {risk}"));
    }
    let mut out = vec![head];
    for f in &findings {
        let profiles = f["profiles"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let fix = match f["fix"]["status"].as_str() {
            Some("resolved") => " — fixed by the fix loop",
            Some("escalated") => " — the fix loop needs you",
            Some("open") => " — fix loop open",
            _ => "",
        };
        out.push(String::new());
        out.push(format!(
            "{}  {}  {}:{}  {}  [{profiles}]{fix}",
            f["id"].as_str().unwrap_or("?"),
            f["severity"].as_str().unwrap_or("?"),
            f["file"].as_str().unwrap_or("?"),
            f["line"],
            f["title"].as_str().unwrap_or("")
        ));
        out.push(format!("  {}", f["failureScenario"].as_str().unwrap_or("")));
        if let Some(how) = f["howItFails"].as_str() {
            for line in how.lines() {
                out.push(format!("  {line}"));
            }
        }
        if f["fix"]["status"] == "escalated" {
            if let Some(ledger) = f["fix"]["ledger"].as_str() {
                if let Ok(s) = fix::summary(Path::new(ledger)) {
                    out.push(String::new());
                    out.extend(s.lines().map(|l| format!("  {l}")));
                }
            }
        }
    }
    let profiles = report["profiles"].as_array().cloned().unwrap_or_default();
    let ran: Vec<String> = profiles
        .iter()
        .filter(|p| p["ran"] == true)
        .filter_map(|p| p["name"].as_str().map(String::from))
        .collect();
    let skipped: Vec<String> = profiles
        .iter()
        .filter(|p| p["ran"] == false)
        .map(|p| {
            format!(
                "{} ({})",
                p["name"].as_str().unwrap_or("?"),
                p["reason"].as_str().unwrap_or("")
            )
        })
        .collect();
    out.push(String::new());
    let mut line = format!(
        "Ran: {}.",
        if ran.is_empty() {
            "none".into()
        } else {
            ran.join(", ")
        }
    );
    if !skipped.is_empty() {
        line.push_str(&format!(" Skipped: {}.", skipped.join(", ")));
    }
    out.push(line);
    let dropped = report["dropped"].as_array().cloned().unwrap_or_default();
    let below = dropped
        .iter()
        .filter(|d| d["reason"] == "below major")
        .count();
    out.push(format!(
        "Dropped {} finding(s) the verifier couldn't show failing; {below} below major.",
        dropped.len() - below
    ));
    if let Some(err) = report["map"]["error"].as_str() {
        out.push(format!(
            "The change map failed, so reviewers ran without it: {err}"
        ));
    }
    out.push(format!("Report: {}", path.display()));
    out.join("\n")
}

pub(crate) fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso(secs)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(file: &str, line: u64, sev: &str, title: &str, profile: &str) -> Finding {
        Finding {
            profiles: vec![profile.into()],
            file: file.into(),
            line,
            severity: sev.into(),
            title: title.into(),
            failure_scenario: String::new(),
            confidence: 0.5,
            carried: false,
        }
    }

    #[test]
    fn merges_the_same_failure_from_two_profiles() {
        let merged = merge(vec![
            f(
                "a.rs",
                10,
                "major",
                "empty input parses as a token",
                "correctness",
            ),
            f(
                "a.rs",
                12,
                "blocker",
                "empty input accepted as a token",
                "testing",
            ),
            f("a.rs", 11, "major", "leaks the file handle", "correctness"),
            f(
                "b.rs",
                10,
                "major",
                "empty input parses as a token",
                "correctness",
            ),
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].severity, "blocker");
        assert_eq!(merged[0].profiles, vec!["correctness", "testing"]);
    }

    #[test]
    fn tiers_by_what_changed() {
        let docs = vec!["README.md".to_string(), "docs/x.png".into()];
        assert_eq!(tier(&docs, 500, 40), Tier::Skip);
        let code = vec!["src/a.rs".to_string(), "README.md".into()];
        assert_eq!(tier(&code, 40, 40), Tier::Small);
        assert_eq!(tier(&code, 41, 40), Tier::Full);
        assert!(!is_doc("Cargo.toml"));
        assert!(is_doc("LICENSE"));
    }

    #[test]
    fn local_plan_keeps_three_profiles_and_skips_by_tier() {
        let profiles = json!({ "picked": [
            { "name": "impact-assessment", "path": "/p/i.md", "model": "sonnet", "effort": "high", "decision": "selected", "reasons": ["always"] },
            { "name": "correctness", "path": "/p/c.md", "model": "sonnet", "effort": "high", "decision": "selected", "reasons": ["always"] },
            { "name": "testing", "path": "/p/t.md", "model": "sonnet", "effort": "high", "decision": "infer" },
            { "name": "intent-alignment", "path": "/p/a.md", "model": "sonnet", "effort": "high", "decision": "infer" },
        ]});
        let run = |tier, intent| -> Vec<String> {
            plan_local(&profiles, tier, intent)
                .into_iter()
                .filter(|p| p.run)
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(
            run(Tier::Full, true),
            vec!["correctness", "testing", "intent-alignment"]
        );
        assert_eq!(run(Tier::Full, false), vec!["correctness", "testing"]);
        assert_eq!(run(Tier::Small, true), vec!["correctness"]);
        assert!(run(Tier::Skip, true).is_empty());
    }

    #[test]
    fn plans_default_to_sonnet_and_a_profile_can_opt_into_opus() {
        let plan = Plan::from_picked(&json!({ "name": "c" }), true, String::new());
        assert_eq!(plan.model, "sonnet");
        let plan = Plan::from_picked(
            &json!({ "name": "c", "model": "opus" }),
            true,
            String::new(),
        );
        assert_eq!(plan.model, "opus");
    }

    #[test]
    fn numbers_blockers_first() {
        let v = number(vec![
            (f("a", 1, "major", "x", "c"), json!({ "severity": "major" })),
            (
                f("a", 1, "blocker", "y", "c"),
                json!({ "severity": "blocker" }),
            ),
        ]);
        assert_eq!(v[0]["id"], "F1");
        assert_eq!(v[0]["severity"], "blocker");
        assert_eq!(v[1]["id"], "F2");
    }

    #[test]
    fn formats_iso_times() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_760_486_400), "2025-10-15T00:00:00Z");
    }
}
