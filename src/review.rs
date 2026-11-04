//! `lens review`: a whole review pass, orchestrated here rather than by a model. Every judgment
//! about the code still comes from an agent with a fresh context (see `agent.rs`); what this module
//! decides is plumbing that doesn't need one: which reviewers run, merging duplicate findings,
//! which findings get verified, and the report.
//!
//! The local pass keeps cost down three ways:
//! - **Tiers.** A change that touches only docs isn't reviewed. A small change (`[review]
//!   small_lines`) gets the correctness reviewer only. Otherwise correctness, testing and, when
//!   there's a task, intent-alignment. No lead reviewer and no focus-area reviewers locally: those
//!   are the PR pass's.
//! - **Deltas.** When the last report reviewed this branch from the same merge base, only what
//!   changed since then is reviewed, and blockers it left unfixed are verified again.
//! - **Fresh reports** aren't redone: an unchanged tree isn't reviewed again.
//!
//! Every agent of a pass gets the same system prompt (a short preamble and the change), and its
//! role's instructions in its first message, so agents with the same model and tools share one
//! cached copy of the change.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::agent::{self, Access, Full, Gate, Outcome, Runner, Spec};
use crate::backoff;
use crate::config::{self, Config};
use crate::diff::git;
use crate::forge;
use crate::pr;
use crate::reply;
use crate::usage;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub repo: PathBuf,
    /// The base ref; `prepare.sh` picks origin's default branch, then main, then master.
    pub base: Option<String>,
    /// A ticket or spec the change implements, for intent-alignment.
    pub intent: Option<PathBuf>,
    /// Review the whole change even when an earlier report would allow a delta.
    pub full: bool,
    /// Run the PR pass on this PR instead of the local pass.
    pub pr: Option<u64>,
    /// PR pass: build the review but don't post it.
    pub dry_run: bool,
    /// The per-PR floor: one Sonnet correctness reviewer, only when the change touches a
    /// `[floor] risky` path.
    pub floor: bool,
    /// Where the pass keeps its work folders and reports; defaults to a folder for the repo under
    /// `assets::state_root`.
    pub state: Option<PathBuf>,
    /// Set when the task board runs lens as its pre-PR reviewer.
    pub protocol: Option<Protocol>,
}

/// A run under the pre-PR review protocol (docs/REVIEW.md): the board sets `TASKBOARD_*`
/// variables, lens writes its verdict to `result` and exits 0, or fails without one.
#[derive(Debug, Clone, Default)]
pub struct Protocol {
    pub result: PathBuf,
    /// The last round's result, each finding with the triage's `outcome` and `reason`.
    pub previous: Option<PathBuf>,
}

impl Options {
    /// Takes the protocol's variables, when `TASKBOARD_RESULT` is set and this isn't a PR pass:
    /// the repo, base and intent the board names (flags given win) and its state folder.
    pub fn with_protocol(mut self, var: impl Fn(&str) -> Option<String>) -> Options {
        let var = |k: &str| var(k).filter(|v| !v.is_empty());
        let Some(result) = var("TASKBOARD_RESULT") else {
            return self;
        };
        if self.pr.is_some() {
            return self;
        }
        if self.repo == Path::new(".") {
            if let Some(repo) = var("TASKBOARD_REPO") {
                self.repo = repo.into();
            }
        }
        self.base = self.base.or_else(|| var("TASKBOARD_BASE"));
        self.intent = self
            .intent
            .or_else(|| var("TASKBOARD_INTENT").map(PathBuf::from));
        self.state = self
            .state
            .or_else(|| var("TASKBOARD_STATE_DIR").map(PathBuf::from));
        self.protocol = Some(Protocol {
            result: result.into(),
            previous: var("TASKBOARD_PREVIOUS").map(PathBuf::from),
        });
        self
    }
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
pub(crate) const ROLES: &[&str] = &["reviewer", "verifier"];
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
    /// The reviewer's one-line fix, a hint for the verifier: only the verifier's reaches a post.
    pub(crate) suggested_fix: Option<String>,
    pub(crate) confidence: f64,
    /// A blocker the last report left unfixed, verified again in a delta review.
    pub(crate) carried: bool,
    /// The id an earlier report gave it, kept so a finding has one id across rounds.
    pub(crate) id: Option<String>,
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
            suggested_fix: one_line(&v["suggestedFix"]),
            confidence: v["confidence"].as_f64().unwrap_or(0.5),
            carried: false,
            id: None,
        })
    }

    pub(crate) fn json(&self) -> Value {
        let mut v = json!({
            "profiles": self.profiles,
            "file": self.file,
            "line": self.line,
            "severity": self.severity,
            "title": self.title,
            "failureScenario": self.failure_scenario,
            "confidence": self.confidence,
        });
        if let Some(fix) = &self.suggested_fix {
            v["suggestedFix"] = json!(fix);
        }
        v
    }

    fn for_verifier(&self) -> String {
        format!(
            "Finding (from the {} reviewer{}):\n  file: {}\n  line: {}\n  severity: {}\n  title: {}\n  failureScenario: {}\n{}",
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
            self.failure_scenario,
            self.suggested_fix
                .as_ref()
                .map(|fix| format!("  suggestedFix: {fix}\n"))
                .unwrap_or_default()
        )
    }
}

/// A suggested fix as one trimmed line, or `None` when it's missing or blank.
fn one_line(v: &Value) -> Option<String> {
    let line = v.as_str()?.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.to_string())
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
                    o.suggested_fix = f.suggested_fix.clone();
                }
                o.confidence = o.confidence.max(f.confidence);
                o.carried &= f.carried;
                if o.id.is_none() {
                    o.id = f.id;
                }
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
    /// Where agents read the code: the repo or the PR's tree.
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
    /// The plan's window that turned one of this pass's agents away, if one did.
    turned_away: Option<Full>,
    /// Why `[backoff]` has this pass run the correctness reviewer only, when it does.
    pub(crate) lighter: Option<String>,
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
            log_dir: work.join("agents"),
            usage: usage_log(&root),
            gate: Gate::shared(),
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
            turned_away: None,
            lighter: None,
        })
    }

    pub(crate) fn log(&self, msg: &str) {
        eprintln!("lens: {msg}");
    }

    pub(crate) fn lap(&mut self, name: &str, since: Instant) {
        self.seconds
            .insert(name.into(), json!(since.elapsed().as_secs()));
    }

    /// With `[backoff]` saying the plan's usage runs high, every reviewer but correctness sits
    /// this pass out.
    pub(crate) fn lighten(&self, plans: &mut [Plan]) {
        let Some(why) = &self.lighter else { return };
        let mut skipped = Vec::new();
        for p in plans
            .iter_mut()
            .filter(|p| p.run && p.name != "correctness")
        {
            p.run = false;
            p.reason = "backoff".into();
            skipped.push(p.name.clone());
        }
        if !skipped.is_empty() {
            self.log(&format!(
                "backoff: {why}; running correctness only, skipping {}",
                skipped.join(", ")
            ));
        }
    }

    /// Why the pass stops here, once the plan has turned one of its agents away.
    pub(crate) fn parked(&self) -> Option<String> {
        self.turned_away
            .as_ref()
            .map(|f| f.message(agent::now_secs()))
    }

    pub(crate) fn tally(&mut self, outcomes: &[Outcome]) {
        for o in outcomes {
            if let Some(full) = &o.full {
                self.turned_away.get_or_insert_with(|| full.clone());
                // An agent the plan turned away before it spent anything didn't run.
                if o.cost_usd == 0.0 {
                    continue;
                }
            }
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
                "\n## What changed since the last review (delta.patch)\n\nThe change was reviewed before. Review only what this delta causes or exposes; the rest was already reviewed. Raise nothing about code the delta doesn't touch or newly depend on, unless it is a blocker: it crashes, corrupts data or breaks the build in normal use.\n\n```diff\n{delta}```\n"
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
        // A re-review looks only at the delta, and the list covers the whole change.
        if let Some(list) = fs::read_to_string(self.work.join("map.json"))
            .ok()
            .filter(|_| delta.is_none())
            .and_then(|m| untested(&m))
        {
            s.push_str(&format!("\n## Changed code no test names\n\nChanged symbols outside test files that no test file mentions. Each is a likely coverage gap; a symbol missing here may still be untested, since a mention isn't an assertion.\n\n{list}"));
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

    /// A stable hash of everything that instructs the lead: the preamble, the reviewer role and
    /// its profile. An earlier assessment is reused only while this is the same.
    fn lead_hash(&self, lead: &Plan) -> Result<String> {
        let text = format!(
            "{}\n{}\n{}",
            agent::role_prompt(&self.assets, "preamble")?,
            self.roles["reviewer"],
            fs::read_to_string(&lead.path)
                .with_context(|| format!("reading {}", lead.path.display()))?
        );
        Ok(format!("{:016x}", fnv1a(text.as_bytes())))
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
            let reply = reply::parse_reply(&o.text).ok();
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
                    // The scenario with any failure the verifier couldn't reach taken out.
                    if let Some(s) = r["failureScenario"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                    {
                        entry["failureScenario"] = json!(s);
                    }
                    // Only a fix the verifier stands behind is posted; the reviewer's was a hint.
                    match one_line(&r["suggestedFix"]) {
                        Some(fix) => entry["suggestedFix"] = json!(fix),
                        None => {
                            if let Some(o) = entry.as_object_mut() {
                                o.remove("suggestedFix");
                            }
                        }
                    }
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
    let cfg = config::load(&root)?;
    if let Some(n) = opts.pr {
        if !cfg.pr.samples(n) {
            return Ok(Done {
                summary: format!(
                    "lens: PR #{n} isn't in the sample ([pr] sample = {}), nothing to review.",
                    cfg.pr.sample
                ),
                report: None,
            });
        }
    }
    // A held plan window isn't checked here: a review that starts no agent (the last one still
    // matches the tree) shows the stored report, and one that does is turned away by the runner.
    let now = agent::now_secs();
    let backoff = backoff::check(&cfg.backoff, &root.to_string_lossy(), now);
    if backoff.lifted {
        eprintln!("lens: {}", backoff::LIFTED);
    }
    if let Some((why, _)) = &backoff.pause {
        if opts.protocol.is_some() {
            bail!("paused: {why}");
        }
        return Ok(parked(why));
    }
    if let (Some(n), Some((share, why))) = (opts.pr, &backoff.share) {
        if !config::in_share(n, cfg.pr.sample * share) {
            return Ok(Done {
                summary: format!(
                    "lens: PR #{n} skipped: backoff: {why}, so lens reviews {}% of PRs until it eases. Nothing was reviewed.",
                    (share * 100.0).round()
                ),
                report: None,
            });
        }
    }
    let state = match &opts.state {
        Some(dir) => dir.clone(),
        None => state_dir(&crate::assets::state_root()?, &root),
    };
    fs::create_dir_all(&state).with_context(|| format!("making {}", state.display()))?;
    let state = state.canonicalize()?;
    let repo_state = crate::outcome::state_of(&root).ok();
    let states: Vec<PathBuf> = repo_state.into_iter().chain([state.clone()]).collect();
    crate::outcome::before_review(&root, &states, &cfg);
    let prepared = prepare(&assets, &root, &state, &opts)?;
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
    pass.lighter = backoff.lighter;
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

/// A repo's state folder under `state_root`: its name, then a hash of its path, so two checkouts
/// of one repo keep apart. The hash is FNV-1a, which no toolchain update changes, so a rebuilt
/// lens still finds the last report.
pub(crate) fn state_dir(state_root: &Path, root: &Path) -> PathBuf {
    let hash = fnv1a(root.as_os_str().as_encoded_bytes());
    state_root.join(format!("{}-{hash:016x}", repo_name(root)))
}

/// FNV-1a: a hash a later build of lens computes the same way, for anything kept on disk.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

fn prepare(assets: &Path, root: &Path, state: &Path, opts: &Options) -> Result<Value> {
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
        .env("LENS_STATE", state)
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

/// The usage log for a run in `root`, filed under its repo's name.
fn usage_log(root: &Path) -> Option<usage::Log> {
    usage::Log::new(&repo_name(root))
}

/// The name usage rows are filed under: the main repository's folder, so a review run in a
/// worktree slot counts toward its repo. The git common dir is `<repo>/.git` for a normal
/// repo, `<repo>.git` for a bare one and `<super>/.git/modules/<repo>` for a submodule; the
/// last two are named by the dir itself. Falls back to the folder name outside a git repo.
fn repo_name(root: &Path) -> String {
    let named = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .and_then(|out| {
        let common = Path::new(out.trim());
        match named(common)? {
            dot_git if dot_git == ".git" => named(common.parent()?),
            dir => Some(dir.strip_suffix(".git").unwrap_or(&dir).to_string()),
        }
    })
    .or_else(|| named(root))
    .unwrap_or_else(|| "?".into())
}

/// The tree an earlier report reviewed: the one it recorded, else
/// `head`'s when the tree was clean.
fn reviewed_tree(root: &Path, report: &Value) -> Option<String> {
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
    x.id = f["id"].as_str().map(String::from);
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
        let summary = "lens: nothing to review: the branch has no changes against its base.";
        respond(pass, None, summary, 0.0)?;
        return Ok(Done {
            summary: summary.into(),
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
            let summary = "lens: floor: no risky paths changed, nothing to review.";
            respond(pass, None, summary, 0.0)?;
            return Ok(Done {
                summary: summary.into(),
                report: None,
            });
        }
        floor_reason = Some(format!("risky path: {}", risky.join(", ")));
    }
    let now = reply::snapshot(&pass.root)?;
    let prior: Option<Value> = fs::read_to_string(&pass.report_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let triaged = triaged(pass)?;
    let settled = settle(prior.as_ref(), triaged.as_ref());
    let last = prior.as_ref().map_or(0, last_id);
    let previous = prior.filter(|r: &Value| r["pass"] == "local" && !pass.opts.floor);
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

    if let Mode::Fresh(mut prev) = mode {
        pass.log("the last review still matches the tree; nothing to review again");
        if !settled.is_empty() {
            if let Some(fs) = prev["findings"].as_array_mut() {
                fs.retain(|f| !settled.iter().any(|s| s["id"] == f["id"]));
            }
            prev["settled"] = json!(settled);
            fs::write(
                &pass.report_path,
                serde_json::to_string_pretty(&prev)? + "\n",
            )?;
        }
        let report: Value = serde_json::from_str(&fs::read_to_string(&pass.report_path)?)?;
        respond(
            pass,
            Some(&report),
            &summarize(&report, &pass.report_path, true),
            0.0,
        )?;
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
    // A delta carries the last report's findings over. Unfixed blockers, majors in files the
    // delta touched and findings the triage sent to be fixed are verified again; other majors are
    // kept as they were. What the triage dismissed or backlogged stays out.
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
                if settled.iter().any(|s| s["id"] == f["id"]) {
                    continue;
                }
                let touched = files.iter().any(|p| f["file"] == *p.as_str());
                let to_fix = outcome(triaged.as_ref(), &f["id"]) == Some("fix");
                let Some(x) = carry(&f) else { continue };
                if f["severity"] == "blocker" || touched || to_fix {
                    carried.push(x);
                } else {
                    kept.push((x, f.clone()));
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
    pass.lighten(&mut plans);
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
    if let Some(why) = pass.parked() {
        return unpark_previous(pass, &prev_dir, &why);
    }

    let mut raised = Vec::new();
    let mut profile_rows = Vec::new();
    let mut failed = Vec::new();
    // Reviewers backoff skipped: the report counts as incomplete, so a later review redoes them.
    let mut lightened = Vec::new();
    let mut it = outcomes.iter();
    for plan in &plans {
        if !plan.run {
            profile_rows.push(plan.json(None));
            if plan.reason == "backoff" {
                lightened.push(plan.name.clone());
            }
            continue;
        }
        let o = it.next().unwrap();
        match reply::parse_reply(&o.text) {
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
            if let Some(s) = settled_as(&settled, &f) {
                let mut e = f.json();
                e["reason"] = json!(format!(
                    "{} in triage{}",
                    if s["outcome"] == "dismiss" {
                        "dismissed"
                    } else {
                        "backlogged"
                    },
                    s["reason"]
                        .as_str()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ));
                drop.push(e);
            } else if rank(&f.severity) <= 1 {
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
    if let Some(why) = pass.parked() {
        return unpark_previous(pass, &prev_dir, &why);
    }
    dropped.extend(unverified);
    confirmed.extend(kept);

    let mut report = json!({
        "version": 1,
        "pass": "local",
        "root": pass.root,
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
        "findings": number(confirmed, last),
        "dropped": dropped,
    });
    if let Some(d) = delta_info {
        report["delta"] = d;
    }
    if !settled.is_empty() {
        report["settled"] = json!(settled);
    }
    report["lastId"] = json!(last_id(&report));
    if !failed.is_empty() || !lightened.is_empty() {
        report["incomplete"] = json!(failed.iter().chain(&lightened).collect::<Vec<_>>());
    }
    finish(pass, &mut report)?;
    if pass.opts.protocol.is_some() && !failed.is_empty() {
        bail!(
            "{} reviewer(s) gave no readable reply ({}); the review is incomplete. Report: {}",
            failed.len(),
            failed.join(", "),
            pass.report_path.display()
        );
    }
    respond(
        pass,
        Some(&report),
        &summarize(&report, &pass.report_path, false),
        report["costUsd"].as_f64().unwrap_or(0.0),
    )?;

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

/// What a pass the plan stopped prints: no report, and when it can run again.
pub(crate) fn parked(why: &str) -> Done {
    Done {
        summary: format!(
            "lens: paused: {why}. Nothing was reviewed; run lens again then to pick it up."
        ),
        report: None,
    }
}

/// Parks a local pass the plan stopped part way: the last review stays the current one, so the
/// next run starts from it as if this one never ran.
fn unpark_previous(pass: &Pass, prev_dir: &Path, why: &str) -> Result<Done> {
    if prev_dir.join("report.json").is_file() {
        fs::rename(prev_dir.join("report.json"), &pass.report_path)?;
    }
    if pass.opts.protocol.is_some() {
        bail!("paused: {why}");
    }
    Ok(parked(why))
}

/// The last round's result the board passed in `TASKBOARD_PREVIOUS`, with the triage's outcomes.
fn triaged(pass: &Pass) -> Result<Option<Value>> {
    let Some(path) = pass
        .opts
        .protocol
        .as_ref()
        .and_then(|p| p.previous.as_ref())
    else {
        return Ok(None);
    };
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("reading {}", path.display()))
}

/// What the triage made of the finding with `id`: fix, backlog or dismiss.
fn outcome<'a>(triaged: Option<&'a Value>, id: &Value) -> Option<&'a str> {
    triaged?["findings"]
        .as_array()?
        .iter()
        .find(|f| f["id"] == *id)?["outcome"]
        .as_str()
}

/// Every finding the triage dismissed or backlogged so far: the last report's `settled` and the
/// last round's. They're never raised again.
fn settle(prior: Option<&Value>, triaged: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = prior
        .and_then(|r| r["settled"].as_array().cloned())
        .unwrap_or_default();
    let findings = triaged.and_then(|t| t["findings"].as_array());
    for f in findings.into_iter().flatten() {
        if !matches!(f["outcome"].as_str(), Some("dismiss" | "backlog"))
            || out.iter().any(|s| s["id"] == f["id"])
        {
            continue;
        }
        out.push(json!({
            "id": f["id"],
            "file": f["file"],
            "line": f["line"],
            "severity": f["severity"],
            "title": f["title"],
            "outcome": f["outcome"],
            "reason": f["reason"],
        }));
    }
    out
}

/// The settled finding `f` raises again: the same id, or the same failure.
fn settled_as<'a>(settled: &'a [Value], f: &Finding) -> Option<&'a Value> {
    settled.iter().find(|s| {
        f.id.as_deref().is_some_and(|id| s["id"] == id) || carry(s).is_some_and(|x| same(&x, f))
    })
}

fn id_number(id: &Value) -> u32 {
    id.as_str()
        .and_then(|s| s.strip_prefix('F'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// The highest id a report has handed out, so a new finding never takes an old one's.
fn last_id(report: &Value) -> u32 {
    let ids = ["findings", "settled"]
        .iter()
        .flat_map(|k| report[*k].as_array().cloned().unwrap_or_default())
        .map(|f| id_number(&f["id"]));
    ids.chain([report["lastId"].as_u64().unwrap_or(0) as u32])
        .max()
        .unwrap_or(0)
}

/// Ids in report order, blockers first. A finding an earlier report raised keeps its id; a new
/// one takes the next after `last`.
fn number(mut confirmed: Vec<(Finding, Value)>, last: u32) -> Vec<Value> {
    confirmed.sort_by_key(|(_, v)| rank(v["severity"].as_str().unwrap_or("nit")));
    let old = |f: &Finding, v: &Value| f.id.clone().or_else(|| v["id"].as_str().map(String::from));
    let mut next = confirmed
        .iter()
        .filter_map(|(f, v)| old(f, v))
        .map(|id| id_number(&json!(id)))
        .fold(last, u32::max);
    let mut used = BTreeSet::new();
    confirmed
        .into_iter()
        .map(|(f, mut v)| {
            let id = match old(&f, &v) {
                Some(id) if used.insert(id.clone()) => id,
                _ => {
                    next += 1;
                    format!("F{next}")
                }
            };
            let obj = v.as_object_mut().unwrap();
            obj.remove("id");
            let mut out = serde_json::Map::new();
            out.insert("id".into(), json!(id));
            out.extend(std::mem::take(obj));
            Value::Object(out)
        })
        .collect()
}

/// Under the protocol, writes the round's result to `TASKBOARD_RESULT`: fail with the findings,
/// pass when a review found none, skip when there was nothing to review.
fn respond(pass: &Pass, report: Option<&Value>, summary: &str, cost: f64) -> Result<()> {
    let Some(protocol) = &pass.opts.protocol else {
        return Ok(());
    };
    let result = protocol_result(report, summary, cost);
    fs::write(
        &protocol.result,
        serde_json::to_string_pretty(&result)? + "\n",
    )
    .with_context(|| format!("writing {}", protocol.result.display()))
}

fn protocol_result(report: Option<&Value>, summary: &str, cost: f64) -> Value {
    let findings: Vec<Value> = report
        .and_then(|r| r["findings"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|f| {
            let mut detail = f["failureScenario"].as_str().unwrap_or("").to_string();
            if let Some(fix) = f["suggestedFix"].as_str() {
                detail = format!("{detail}\n\nFix: {fix}");
            }
            if let Some(how) = f["howItFails"].as_str().filter(|h| !h.is_empty()) {
                detail = format!("{detail}\n\n{how}");
            }
            json!({
                "id": f["id"],
                "title": f["title"],
                "severity": f["severity"],
                "file": f["file"],
                "line": f["line"],
                "detail": detail.trim(),
                "url": null,
            })
        })
        .collect();
    let reviewed = report.is_some_and(|r| r["totals"]["agents"].as_u64().unwrap_or(0) > 0);
    let verdict = if !findings.is_empty() {
        "fail"
    } else if reviewed {
        "pass"
    } else {
        "skip"
    };
    let headline = summary.lines().next().unwrap_or("");
    json!({
        "protocol": 1,
        "reviewer": "lens",
        "verdict": verdict,
        "headline": headline.strip_prefix("lens: ").unwrap_or(headline),
        "findings": findings,
        "cost": { "usd": (cost * 10000.0).round() / 10000.0 },
    })
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
    let text = serde_json::to_string_pretty(report)? + "\n";
    fs::write(&pass.report_path, &text)
        .with_context(|| format!("writing {}", pass.report_path.display()))?;
    if pass.cfg.review.keep_reports {
        let kept = pass.work.join("reports").join(kept_name(report));
        fs::create_dir_all(kept.parent().unwrap())?;
        fs::write(&kept, &text).with_context(|| format!("writing {}", kept.display()))?;
    }
    Ok(())
}

/// The file name a kept report gets: its start time and the commit it reviewed, so the
/// `reports/` folder lists runs in order, e.g. `20261102T204105Z-3cd14fc.json`.
fn kept_name(report: &Value) -> String {
    let started: String = report["startedAt"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    let head = report["head"].as_str().unwrap_or("");
    format!("{started}-{}.json", &head[..head.len().min(7)])
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
    if !pass.opts.full {
        if let Some(prev) = reusable(&pass.report_path, &prepared) {
            pass.log(&format!(
                "PR #{number}'s head is unchanged since the last review; reusing it (--full reviews again)"
            ));
            let summary = summarize(&prev, &pass.report_path, true);
            return Ok(post_pr(pass, number, &prev, summary, &[]));
        }
    }
    let _ = fs::remove_dir_all(pass.work.join("agents"));
    // A PR lens reviewed before is reviewed again only for what changed since: the findings it
    // already raised were raised, and the code it already read was read.
    let since = if pass.opts.full { None } else { pr_delta(pass) };
    let delta = match since {
        Some(Since::Rebased { from, report }) => {
            return reuse_rebased(pass, number, &from, report);
        }
        Some(Since::Delta(d)) => Some(d),
        None => None,
    };
    if let Some(d) = &delta {
        pass.log(&format!(
            "reviewing the {} changed lines since the last review of {}",
            changed_lines(&d.patch),
            &d.from[..d.from.len().min(7)]
        ));
    }
    let carried: Vec<Finding> = delta
        .as_ref()
        .and_then(|d| d.previous.as_ref())
        .and_then(|p| p["findings"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|f| matches!(f["severity"].as_str(), Some("blocker" | "major")))
        .filter_map(carry)
        .collect();
    let profiles: Value =
        serde_json::from_str(&fs::read_to_string(pass.work.join("profiles.json"))?)?;
    let files: Vec<String> = profiles["changedFiles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();
    let mut plans = plan_pr(&profiles, &files);
    pass.lighten(&mut plans);
    if delta.as_ref().is_some_and(|d| d.patch.trim().is_empty()) {
        for p in &mut plans {
            p.run = false;
            p.reason = "nothing changed since the last review".into();
        }
    }
    let extra = format!(
        "\nRepo: {}  (the PR's head; read files there, not in the current directory)\nAlready posted on this PR (don't raise these again): {}\n",
        pass.tree_dir.display(),
        pass.work.join("posted.json").display()
    );
    let system = pass.system_prompt(&pass.context(delta.as_ref().map(|d| d.patch.as_str()))?)?;

    // The lead first: its assessment goes in the review body and its focus areas get reviewers.
    let mut raised = Vec::new();
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    let mut risk = Value::Null;
    let mut assessment = Value::Null;
    let mut focus: Vec<Value> = Vec::new();
    let mut lead_record = Value::Null;
    let mut correctness: Option<Plan> = None;
    let t = Instant::now();
    let lead = plans.iter().find(|p| p.name == LEAD && p.run);
    let lead_hash = lead.map(|l| pass.lead_hash(l)).transpose()?;
    let prior_lead = match (&lead_hash, pass.opts.full) {
        (Some(hash), false) => delta
            .as_ref()
            .map_or_else(|| last_report(&pass.report_path), |d| d.previous.clone())
            .filter(|prev| reusable_lead(prev, &prepared, hash, &files)),
        _ => None,
    };
    if let (Some(lead), Some(prev)) = (lead, &prior_lead) {
        let from = prev["head"].as_str().unwrap_or("");
        let short = &from[..from.len().min(7)];
        risk = prev["risk"].clone();
        assessment = prev["assessment"].clone();
        let all = prev["lead"]["focusAreas"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        focus = match &delta {
            Some(d) => all
                .iter()
                .filter(|f| touches(f, &d.files))
                .cloned()
                .collect(),
            None => all.clone(),
        };
        pass.log(&format!(
            "reusing the lead's assessment of {short}: same base and files (--full runs it again); {} of {} focus area(s) to review again",
            focus.len(),
            all.len()
        ));
        // A full review re-raises what every other reviewer found; the lead's own findings come
        // back only this way.
        if delta.is_none() {
            raised.extend(
                prev["findings"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter(|f| {
                        f["profiles"]
                            .as_array()
                            .is_some_and(|p| p.contains(&json!(LEAD)))
                    })
                    .filter_map(carry),
            );
        }
        let mut reused = lead.clone();
        reused.run = false;
        reused.reason = format!("reused from {short}");
        rows.push(reused.json(None));
        lead_record =
            json!({ "profile": lead_hash, "files": files, "focusAreas": all, "reusedFrom": from });
        fs::write(
            pass.work.join("impact.md"),
            assessment.as_str().unwrap_or(""),
        )?;
    } else if let Some(lead) = lead {
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
        match reply::parse_reply(&o.text) {
            Ok(r) => {
                risk = r["risk"].clone();
                assessment = r["assessment"].clone();
                focus = r["focusAreas"].as_array().cloned().unwrap_or_default();
                focus.truncate(lead.max_minions as usize);
                lead_record = json!({ "profile": lead_hash, "files": files, "focusAreas": focus });
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
    if let Some(why) = pass.parked() {
        return Ok(parked(&why));
    }

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
    if let Some(why) = pass.parked() {
        return Ok(parked(&why));
    }
    for ((name, plan, _), o) in runs.iter().zip(&outcomes) {
        let mut row = plan.json(None);
        row["name"] = json!(name);
        match reply::parse_reply(&o.text) {
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

    let (to_verify, nits) = skip_nits(merge(raised.into_iter().chain(carried).collect()));
    let (confirmed, mut dropped) = pass.verify(&system, &to_verify, |_| true);
    if let Some(why) = pass.parked() {
        return Ok(parked(&why));
    }
    dropped.extend(nits);
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
        "lead": lead_record,
        "profiles": rows,
        "findings": number_all(confirmed),
        "dropped": dropped,
        "lens": crate::assets::fingerprint(),
        "mode": if delta.is_some() { "delta" } else { "full" },
    });
    if let Some(d) = &delta {
        report["delta"] = json!({ "from": d.from, "changedLines": changed_lines(&d.patch) });
    }
    finish(pass, &mut report)?;

    let summary = summarize(&report, &pass.report_path, false);
    Ok(post_pr(pass, number, &report, summary, &failed))
}

/// Reuses the review of `from` for a PR rebased since with the same patches: its findings, moved
/// to the new head, are posted for that head.
fn reuse_rebased(pass: &mut Pass, number: u64, from: &str, mut report: Value) -> Result<Done> {
    pass.log(&format!(
        "PR #{number} was rebased since the last review of {} and every commit's patch is the same; reusing that review (--full reviews again)",
        &from[..from.len().min(7)]
    ));
    let prepared = pass.prepared.clone();
    for key in ["branch", "base", "mergeBase", "head", "changedLines"] {
        report[key] = prepared[key].clone();
    }
    report["startedAt"] = json!(pass.started_at);
    report["mode"] = json!("rebased");
    report["rebased"] = json!({ "from": from });
    if let Some(r) = report.as_object_mut() {
        r.remove("delta");
    }
    finish(pass, &mut report)?;
    let summary = summarize(&report, &pass.report_path, true);
    Ok(post_pr(pass, number, &report, summary, &[]))
}

/// Why a nit the PR pass never verified is in the report's `dropped`.
const NIT_UNVERIFIED: &str = "nit: never posted, so not verified";

/// Splits off the nits: a PR never gets one (`pr::Finding::left_out`), so a verifier on one is
/// spent for nothing. They go to `dropped`, not `findings`, so the report counts only what a
/// verifier checked.
fn skip_nits(findings: Vec<Finding>) -> (Vec<Finding>, Vec<Value>) {
    let (nits, rest): (Vec<Finding>, Vec<Finding>) =
        findings.into_iter().partition(|f| f.severity == "nit");
    let dropped = nits
        .iter()
        .map(|f| {
            let mut e = f.json();
            e["reason"] = json!(NIT_UNVERIFIED);
            e
        })
        .collect();
    (rest, dropped)
}

/// What a PR changed since lens last reviewed it.
struct PrDelta {
    /// The commit the last lens review on the PR was for.
    from: String,
    /// `git diff <from> <head>`.
    patch: String,
    /// The files `patch` touches.
    files: Vec<String>,
    /// That review's report, when lens kept it: its blockers and majors are verified again.
    previous: Option<Value>,
}

/// What became of the PR since its last lens review.
enum Since {
    /// The PR was rebased and every commit's patch is the same: `report` is the last review's,
    /// moved to the new head.
    Rebased { from: String, report: Value },
    /// Review only this.
    Delta(PrDelta),
}

/// The PR's change since the commit its last lens review was for (read from that review's
/// marker in `posted.json`). When that commit is an ancestor of the head, the delta is
/// everything since it; when the PR was rebased since, it's what follows the commits whose
/// patches stayed the same (`rebased_since`). `None` means a full review: a first review, the
/// same head, a reviewed commit lens can't fetch, or a rewrite it can't match.
fn pr_delta(pass: &Pass) -> Option<Since> {
    let posted: Value =
        serde_json::from_str(&fs::read_to_string(pass.work.join("posted.json")).ok()?).ok()?;
    let mut from = posted["head"].as_str()?.to_string();
    // A rebase whose review was reused may post nothing new, so the marker can still name the
    // head from before it: that review covered the head it was moved to.
    if let Some(last) = last_report(&pass.report_path) {
        if last["mode"] == "rebased" && last["rebased"]["from"] == from.as_str() {
            if let Some(h) = last["head"].as_str() {
                from = h.to_string();
            }
        }
    }
    let head = pass.prepared["head"].as_str()?;
    if from == head {
        return None;
    }
    let tree = &pass.tree_dir;
    if crate::diff::git(tree, &["cat-file", "-e", &format!("{from}^{{commit}}")]).is_err() {
        let _ = crate::diff::git(tree, &["fetch", "--quiet", "origin", &from]);
    }
    crate::diff::git(tree, &["cat-file", "-e", &format!("{from}^{{commit}}")]).ok()?;
    let previous = report_for(pass, &from);
    if crate::diff::git(tree, &["merge-base", "--is-ancestor", &from, head]).is_ok() {
        return delta_between(tree, &from, head, previous).map(Since::Delta);
    }
    rebased_since(pass, &from, previous?)
}

/// The delta for a PR rebased since `previous` reviewed `from`, matching its old commits to the
/// new ones by patch-id. The new commits up to the first whose patch the old ones don't have
/// were reviewed already. All of them: the review is reused when nothing was dropped either (and
/// the same lens made it), and otherwise the delta is empty and only the carried findings are
/// verified again. None of them, or a merge commit in either range: a full review.
fn rebased_since(pass: &Pass, from: &str, mut previous: Value) -> Option<Since> {
    let tree = &pass.tree_dir;
    let head = pass.prepared["head"].as_str()?;
    let old_base = previous["mergeBase"].as_str()?;
    let new_base = pass.prepared["mergeBase"].as_str()?;
    let old = crate::diff::patch_ids(tree, &format!("{old_base}..{from}")).ok()?;
    let new = crate::diff::patch_ids(tree, &format!("{new_base}..{head}")).ok()?;
    let reviewed = reviewed_prefix(&old, &new);
    if reviewed == 0 {
        return None;
    }
    if let Some(findings) = previous["findings"].as_array_mut() {
        move_findings(tree, from, head, findings);
    }
    let all_kept = old
        .iter()
        .filter_map(|(_, id)| id.as_ref())
        .all(|id| new.iter().any(|(_, n)| n.as_ref() == Some(id)));
    if reviewed == new.len() && all_kept {
        // Like an unchanged head, a review by another lens isn't reused.
        if previous["lens"] != crate::assets::fingerprint().as_str() {
            return None;
        }
        return Some(Since::Rebased {
            from: from.to_string(),
            report: previous,
        });
    }
    let last = &new[reviewed - 1].0;
    delta_between(tree, last, head, Some(previous)).map(Since::Delta)
}

/// How many of `new`'s commits, from the oldest, have a patch one of `old`'s has. A commit that
/// changes nothing counts as having one.
fn reviewed_prefix(old: &[(String, Option<String>)], new: &[(String, Option<String>)]) -> usize {
    new.iter()
        .take_while(|(_, id)| {
            id.as_ref()
                .is_none_or(|id| old.iter().any(|(_, o)| o.as_ref() == Some(id)))
        })
        .count()
}

/// `git diff <from> <head>` in `tree` as a delta over `previous`.
fn delta_between(tree: &Path, from: &str, head: &str, previous: Option<Value>) -> Option<PrDelta> {
    let patch = crate::diff::git(tree, &["diff", "--full-index", from, head]).ok()?;
    let files = crate::diff::git(tree, &["diff", "--name-only", "--no-renames", from, head])
        .ok()?
        .lines()
        .map(String::from)
        .collect();
    Some(PrDelta {
        from: from.to_string(),
        patch,
        files,
        previous,
    })
}

/// Moves each finding's line from where it was at `from` to where that line is at `to`.
fn move_findings(tree: &Path, from: &str, to: &str, findings: &mut [Value]) {
    let mut diffs: HashMap<String, Option<String>> = HashMap::new();
    for f in findings {
        let (Some(file), Some(line)) = (f["file"].as_str(), f["line"].as_u64()) else {
            continue;
        };
        let diff = diffs.entry(file.to_string()).or_insert_with(|| {
            crate::diff::git(
                tree,
                &[
                    "diff",
                    "-U0",
                    "--no-renames",
                    "--no-color",
                    "--no-ext-diff",
                    from,
                    to,
                    "--",
                    file,
                ],
            )
            .ok()
        });
        if let (Some(diff), Ok(line)) = (diff, u32::try_from(line)) {
            f["line"] = json!(crate::diff::moved_line(diff, line));
        }
    }
}

/// The kept report (or the latest one) that reviewed `head`.
fn report_for(pass: &Pass, head: &str) -> Option<Value> {
    let read = |p: &Path| -> Option<Value> {
        let r: Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
        (r["head"] == head).then_some(r)
    };
    if let Some(r) = read(&pass.report_path) {
        return Some(r);
    }
    let mut kept: Vec<PathBuf> = fs::read_dir(pass.work.join("reports"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    kept.sort();
    kept.iter().rev().find_map(|p| read(p))
}

/// The last PR pass's report, when it reviewed the same head against the same base with the same
/// lens: running the agents again would only add noise.
fn reusable(path: &Path, prepared: &Value) -> Option<Value> {
    let prev: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (prev["pass"] == "pr"
        && prev["head"] == prepared["head"]
        && prev["base"] == prepared["base"]
        && prev["lens"] == crate::assets::fingerprint().as_str())
    .then_some(prev)
}

/// The last pass's report, whatever head it reviewed.
fn last_report(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Whether `prev`'s lead assessment still describes the PR: same base and merge base, the same
/// lead instructions (`hash`), and the PR touches the same `files`. A re-review then skips the
/// lead and reruns only the focus areas its delta touches.
fn reusable_lead(prev: &Value, prepared: &Value, hash: &str, files: &[String]) -> bool {
    let lead = &prev["lead"];
    let same_files = || {
        let before: BTreeSet<&str> = lead["files"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        before == files.iter().map(String::as_str).collect()
    };
    prev["pass"] == "pr"
        && prev["base"] == prepared["base"]
        && prev["mergeBase"] == prepared["mergeBase"]
        && prev["assessment"]
            .as_str()
            .is_some_and(|a| !a.trim().is_empty())
        && lead["profile"] == hash
        && lead["focusAreas"].is_array()
        && same_files()
}

/// Whether a lead focus area names one of `files` (or a folder holding one). An area naming no
/// files counts as touched.
fn touches(area: &Value, files: &[String]) -> bool {
    let named: Vec<&str> = area["files"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    named.is_empty()
        || named.iter().any(|n| {
            let n = n.trim_end_matches('/');
            files.iter().any(|f| {
                f == n
                    || n.strip_prefix(f.as_str())
                        .is_some_and(|rest| rest.starts_with(':'))
                    || f.strip_prefix(n).is_some_and(|rest| rest.starts_with('/'))
            })
        })
}

/// Posts a PR pass's report (unless it's a dry run) and adds what happened to `summary`.
fn post_pr(
    pass: &Pass,
    number: u64,
    report: &Value,
    mut summary: String,
    failed: &[String],
) -> Done {
    let cfg = match config::load(&pass.root) {
        Ok(c) => c.pr,
        Err(e) => {
            summary.push_str(&format!("\nPosting failed: {e:#}."));
            return Done {
                summary,
                report: Some(pass.report_path.clone()),
            };
        }
    };
    let posted = (|| -> Result<String> {
        let forge = forge::open(&pass.root, &cfg)?;
        let parsed: pr::Report = serde_json::from_value(report.clone())?;
        let patch = fs::read_to_string(pass.work.join("diff.patch"))?;
        let plan = pr::plan(
            &parsed,
            &forge.earlier(number)?,
            &pr::commentable(&patch),
            cfg.comments,
            forge.markup(),
        );
        let left_out = match plan.left_out.len() {
            0 => String::new(),
            n => format!("; {n} nit left out"),
        };
        let url = match (&plan.review, pass.opts.dry_run) {
            (Some(review), false) => forge.post(number, review)?,
            _ => None,
        };
        let verdict = if plan.event == "APPROVE" {
            "approve"
        } else {
            "request changes"
        };
        Ok(match (url, &plan.review) {
            (Some(url), _) => format!(
                "Posted ({verdict}): {url} ({} inline, {} in the body; {} already posted{left_out})",
                plan.inline.len(),
                plan.in_body.len(),
                plan.skipped.len()
            ),
            (None, Some(_)) if pass.opts.dry_run => format!(
                "Dry run: would {verdict}, {} inline, {} in the body{left_out}.",
                plan.inline.len(),
                plan.in_body.len()
            ),
            (None, Some(_)) => {
                "Nothing posted: lens would approve, but GitHub won't let a PR's author approve it."
                    .into()
            }
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
    Done {
        summary,
        report: Some(pass.report_path.clone()),
    }
}

/// The change map's added and modified symbols outside test files that no test mentions, one
/// `- name (kind, file:line)` line each, or `None` when there are none.
fn untested(map: &str) -> Option<String> {
    let map: Value = serde_json::from_str(map).ok()?;
    let lines: Vec<String> = map["symbols"]
        .as_array()?
        .iter()
        .filter(|sym| sym["change"] != "removed")
        .filter(|sym| sym["tests"].as_array().is_none_or(Vec::is_empty))
        .filter_map(|sym| {
            let file = sym["file"].as_str()?;
            (!crate::map::grep::is_test_path(file)).then(|| {
                format!(
                    "- `{}` ({}, {file}:{})\n",
                    sym["name"].as_str().unwrap_or("?"),
                    sym["kind"].as_str().unwrap_or("symbol"),
                    sym["line"]
                )
            })
        })
        .collect();
    (!lines.is_empty()).then(|| lines.concat())
}

pub(crate) fn number_all(confirmed: Vec<(Finding, Value)>) -> Vec<Value> {
    number(confirmed, 0)
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
    if fresh && report["mode"] == "rebased" {
        head.push_str(" · rebased with the same patches as the last review");
    } else if fresh {
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
        out.push(String::new());
        out.push(format!(
            "{}  {}  {}:{}  {}  [{profiles}]",
            f["id"].as_str().unwrap_or("?"),
            f["severity"].as_str().unwrap_or("?"),
            f["file"].as_str().unwrap_or("?"),
            f["line"],
            f["title"].as_str().unwrap_or("")
        ));
        out.push(format!("  {}", f["failureScenario"].as_str().unwrap_or("")));
        if let Some(fix) = f["suggestedFix"].as_str() {
            out.push(format!("  fix: {fix}"));
        }
        if let Some(how) = f["howItFails"].as_str() {
            for line in how.lines() {
                out.push(format!("  {line}"));
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
    let nits = dropped
        .iter()
        .filter(|d| d["reason"] == NIT_UNVERIFIED)
        .count();
    let mut line = format!(
        "Dropped {} finding(s) the verifier couldn't show failing; {below} below major",
        dropped.len() - below - nits
    );
    if nits > 0 {
        line.push_str(&format!("; {nits} nit(s) left out unverified"));
    }
    line.push('.');
    out.push(line);
    if let Some(err) = report["map"]["error"].as_str() {
        out.push(format!(
            "The change map failed, so reviewers ran without it: {err}"
        ));
    }
    out.push(format!("Report: {}", path.display()));
    out.join("\n")
}

pub(crate) fn iso_now() -> String {
    iso(agent::now_secs())
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

    fn git_in(dir: &Path, args: &[&str]) {
        git(dir, args).unwrap();
    }

    #[test]
    fn untested_lists_changed_code_outside_tests_that_no_test_names() {
        let map = json!({"symbols": [
            {"name": "covered", "kind": "function", "file": "src/a.go", "line": 3, "change": "modified", "tests": [{"file": "a_test.go", "line": 1}]},
            {"name": "bare", "kind": "function", "file": "src/a.go", "line": 9, "change": "added", "tests": []},
            {"name": "gone", "kind": "function", "file": "src/a.go", "line": 12, "change": "removed"},
            {"name": "TestBare", "kind": "function", "file": "src/a_test.go", "line": 5, "change": "added"}
        ]});
        assert_eq!(
            untested(&map.to_string()).as_deref(),
            Some("- `bare` (function, src/a.go:9)\n")
        );
        assert_eq!(untested(&json!({"symbols": []}).to_string()), None);
    }

    #[test]
    fn usage_is_filed_under_the_repo_not_the_worktree_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("taskboard");
        fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        git_in(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        let slot = tmp.path().join("slot-1");
        git_in(
            &repo,
            &["worktree", "add", "-q", slot.to_str().unwrap(), "-b", "w"],
        );
        assert_eq!(repo_name(&repo), "taskboard");
        assert_eq!(repo_name(&slot), "taskboard");
    }

    #[test]
    fn pass_files_usage_under_the_repo_name_from_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("taskboard");
        fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        git_in(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        let slot = tmp.path().join("slot-1");
        git_in(
            &repo,
            &["worktree", "add", "-q", slot.to_str().unwrap(), "-b", "w"],
        );
        assert_eq!(usage_log(&slot).unwrap().repo, "taskboard");
    }

    #[test]
    fn usage_name_of_a_bare_repo_is_its_folder_without_dot_git() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("srv").join("foo.git");
        fs::create_dir_all(&bare).unwrap();
        git_in(&bare, &["init", "-q", "--bare"]);
        assert_eq!(repo_name(&bare), "foo");
    }

    #[test]
    fn usage_name_of_a_submodule_is_the_submodule() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp
            .path()
            .join("sup")
            .join(".git")
            .join("modules")
            .join("lib");
        fs::create_dir_all(&sub).unwrap();
        git_in(&sub, &["init", "-q", "--bare"]);
        assert_eq!(repo_name(&sub), "lib");
    }

    #[test]
    fn usage_name_falls_back_to_the_folder_outside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plain");
        fs::create_dir(&dir).unwrap();
        assert_eq!(repo_name(&dir), "plain");
    }

    fn f(file: &str, line: u64, sev: &str, title: &str, profile: &str) -> Finding {
        Finding {
            profiles: vec![profile.into()],
            file: file.into(),
            line,
            severity: sev.into(),
            title: title.into(),
            failure_scenario: String::new(),
            suggested_fix: None,
            confidence: 0.5,
            carried: false,
            id: None,
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
        let v = number(
            vec![
                (f("a", 1, "major", "x", "c"), json!({ "severity": "major" })),
                (
                    f("a", 1, "blocker", "y", "c"),
                    json!({ "severity": "blocker" }),
                ),
            ],
            0,
        );
        assert_eq!(v[0]["id"], "F1");
        assert_eq!(v[0]["severity"], "blocker");
        assert_eq!(v[1]["id"], "F2");
    }

    #[test]
    fn keeps_ids_across_rounds() {
        let mut old = f("a", 1, "major", "x", "c");
        old.id = Some("F2".into());
        let v = number(
            vec![
                (f("b", 1, "minor", "y", "c"), json!({ "severity": "minor" })),
                (old, json!({ "severity": "major" })),
                (
                    f("c", 1, "blocker", "z", "c"),
                    json!({ "severity": "blocker" }),
                ),
            ],
            3,
        );
        let ids: Vec<_> = v.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["F4", "F2", "F5"]);
    }

    #[test]
    fn settles_dismissed_and_backlogged_findings() {
        let prior = json!({ "settled": [{ "id": "F1", "outcome": "dismiss" }] });
        let triaged = json!({ "findings": [
            { "id": "F1", "outcome": "dismiss" },
            { "id": "F2", "outcome": "fix" },
            { "id": "F3", "outcome": "backlog", "reason": "old code", "file": "a", "line": 1 },
        ]});
        let settled = settle(Some(&prior), Some(&triaged));
        let ids: Vec<_> = settled.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["F1", "F3"]);
        assert_eq!(settled[1]["reason"], "old code");
        assert_eq!(outcome(Some(&triaged), &json!("F2")), Some("fix"));
        assert_eq!(
            last_id(&json!({ "findings": [{ "id": "F2" }], "settled": settled, "lastId": 1 })),
            3
        );
    }

    #[test]
    fn writes_protocol_verdicts() {
        let findings = json!({
            "totals": { "agents": 2 },
            "findings": [{
                "id": "F1", "title": "t", "severity": "major", "file": "a.rs", "line": 3,
                "failureScenario": "boom", "howItFails": "because",
                "suggestedFix": "guard it",
            }],
        });
        let r = protocol_result(Some(&findings), "lens: 1 finding (1 major)\nmore", 0.123456);
        assert_eq!(r["protocol"], 1);
        assert_eq!(r["reviewer"], "lens");
        assert_eq!(r["verdict"], "fail");
        assert_eq!(r["headline"], "1 finding (1 major)");
        assert_eq!(
            r["findings"][0]["detail"],
            "boom\n\nFix: guard it\n\nbecause"
        );
        assert_eq!(r["findings"][0]["url"], Value::Null);
        assert_eq!(r["cost"]["usd"], 0.1235);
        let clean = json!({ "totals": { "agents": 2 }, "findings": [] });
        assert_eq!(protocol_result(Some(&clean), "", 0.0)["verdict"], "pass");
        let skipped = json!({ "totals": { "agents": 0 }, "findings": [] });
        assert_eq!(protocol_result(Some(&skipped), "", 0.0)["verdict"], "skip");
        assert_eq!(protocol_result(None, "", 0.0)["verdict"], "skip");
    }

    #[test]
    fn takes_protocol_variables() {
        let env = HashMap::from([
            ("TASKBOARD_RESULT", "/s/result.json"),
            ("TASKBOARD_REPO", "/w"),
            ("TASKBOARD_BASE", "origin/main"),
            ("TASKBOARD_INTENT", "/s/intent.md"),
            ("TASKBOARD_STATE_DIR", "/s/state"),
            ("TASKBOARD_PREVIOUS", ""),
        ]);
        let var = |k: &str| env.get(k).map(|v| v.to_string());
        let opts = Options {
            repo: ".".into(),
            base: Some("main".into()),
            ..Default::default()
        }
        .with_protocol(var);
        assert_eq!(opts.repo, Path::new("/w"));
        assert_eq!(opts.base.as_deref(), Some("main"));
        assert_eq!(opts.intent.as_deref(), Some(Path::new("/s/intent.md")));
        assert_eq!(opts.state.as_deref(), Some(Path::new("/s/state")));
        let p = opts.protocol.unwrap();
        assert_eq!(p.result, Path::new("/s/result.json"));
        assert!(p.previous.is_none());

        let by_hand = Options::default().with_protocol(|_| None);
        assert!(by_hand.protocol.is_none());
        let pr = Options {
            pr: Some(7),
            ..Default::default()
        }
        .with_protocol(var);
        assert!(pr.protocol.is_none());
    }

    #[test]
    fn formats_iso_times() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_760_486_400), "2025-10-15T00:00:00Z");
    }

    #[test]
    fn a_rebase_was_reviewed_up_to_its_first_new_patch() {
        let commits = |ids: &[Option<&str>]| -> Vec<(String, Option<String>)> {
            ids.iter()
                .enumerate()
                .map(|(i, id)| (format!("c{i}"), id.map(String::from)))
                .collect()
        };
        let old = commits(&[Some("p1"), Some("p2"), Some("p3")]);
        assert_eq!(reviewed_prefix(&old, &old), 3);
        assert_eq!(
            reviewed_prefix(&old, &commits(&[Some("p1"), Some("p2")])),
            2
        );
        assert_eq!(
            reviewed_prefix(&old, &commits(&[Some("p1"), Some("x"), Some("p3")])),
            1,
            "an edited commit and everything after it are reviewed again"
        );
        assert_eq!(reviewed_prefix(&old, &commits(&[Some("x"), Some("p1")])), 0);
        assert_eq!(
            reviewed_prefix(&old, &commits(&[Some("p2"), Some("p1"), Some("p3")])),
            3,
            "reordered commits with the same patches were reviewed"
        );
        assert_eq!(
            reviewed_prefix(&old, &commits(&[None, Some("p1"), Some("p4")])),
            2,
            "a commit that changes nothing needs no review"
        );
        assert_eq!(reviewed_prefix(&[], &commits(&[Some("p1")])), 0);
    }

    #[test]
    fn reuses_the_lead_only_while_base_instructions_and_files_hold() {
        let prepared = json!({"base": "main", "mergeBase": "m1"});
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let prev = json!({
            "pass": "pr", "base": "main", "mergeBase": "m1", "assessment": "## Impact\n...",
            "lead": {"profile": "h1", "files": ["src/b.rs", "src/a.rs"], "focusAreas": []}
        });
        assert!(reusable_lead(&prev, &prepared, "h1", &files));

        let with = |path: &str, value: Value| {
            let mut p = prev.clone();
            *p.pointer_mut(path).unwrap() = value;
            p
        };
        for changed in [
            with("/pass", json!("local")),
            with("/base", json!("develop")),
            with("/mergeBase", json!("m2")),
            with("/assessment", json!("  ")),
            with("/lead/profile", json!("h2")),
            with("/lead/focusAreas", Value::Null),
            with("/lead/files", json!(["src/a.rs"])),
            with("/lead", Value::Null),
        ] {
            assert!(
                !reusable_lead(&changed, &prepared, "h1", &files),
                "{changed}"
            );
        }
        assert!(!reusable_lead(&prev, &prepared, "h1", &files[..1]));
    }

    #[test]
    fn a_focus_area_is_touched_by_its_files_lines_or_folders() {
        let files = vec!["src/review.rs".to_string(), "docs/a.md".to_string()];
        let area = |named: Value| json!({"title": "t", "files": named});
        assert!(touches(&area(json!(["src/review.rs"])), &files));
        assert!(touches(&area(json!(["src/review.rs:120"])), &files));
        assert!(touches(&area(json!(["docs/"])), &files));
        assert!(touches(&area(json!(["docs"])), &files));
        assert!(touches(&area(json!([])), &files));
        assert!(touches(&json!({"title": "t"}), &files));
        assert!(!touches(&area(json!(["src/review.rs.bak"])), &files));
        assert!(!touches(&area(json!(["src/main.rs", "doc"])), &files));
    }
}
