//! `lens sweep`: reviews everything pushed to origin's default branch since the last sweep, in
//! one batch per repo instead of inside every task.
//!
//! The range runs from the watermark ref `refs/lens/swept` to the default branch. The lead
//! reviewer splits it into areas of at most `[sweep] area_lines` changed lines; each area is one
//! shared change block (one cache group) for its reviewers and verifiers. Confirmed blockers go
//! through the fix loop in a worktree on `lens/sweep-<stamp>`, one commit per fix, and the branch
//! is opened as one PR whose body is the digest. The watermark moves only after a pass no
//! reviewer failed.
//!
//! When the plan turns an agent away, the sweep stops before writing a report: it doesn't count
//! as a sweep, and the watermark stays for the next one after the window resets.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::agent::{Access, Gate};
use crate::config::{self, SweepConfig};
use crate::diff::git;
use crate::review::{self, Finding, Pass, LEAD};
use crate::{map, pr, profile, select};

/// The last commit a sweep reviewed, per repo.
pub const WATERMARK: &str = "refs/lens/swept";

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub repo: PathBuf,
    /// Review from this ref instead of the watermark.
    pub since: Option<String>,
    /// Review and write the report, but fix nothing, open no PR and leave the watermark.
    pub dry_run: bool,
    /// Run even when the sweep isn't due or today's budget is spent.
    pub now: bool,
}

/// One part of the range, reviewed with its own change block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Area {
    pub title: String,
    pub files: Vec<String>,
    pub why: String,
    pub lines: u32,
}

/// Runs a sweep and returns what to print.
pub fn run(opts: Options) -> Result<String> {
    let root = PathBuf::from(git(&opts.repo, &["rev-parse", "--show-toplevel"])?.trim());
    let cfg = config::load(&root)?;
    git(&root, &["fetch", "--quiet", "origin"]).context("fetching origin")?;
    let target = target_branch(&root)?;
    let head = rev(&root, &target)?;
    let start = match &opts.since {
        Some(r) => rev(&root, r).with_context(|| format!("--since {r} isn't a commit"))?,
        None => match rev(&root, WATERMARK) {
            Ok(s) => s,
            Err(_) if opts.dry_run => {
                return Ok(format!(
                    "lens: no sweep has run here yet; the first one sets {WATERMARK} to {target} ({}) and reviews what lands after it.",
                    short(&head)
                ))
            }
            Err(_) => {
                git(&root, &["update-ref", WATERMARK, &head])?;
                return Ok(format!(
                    "lens: first sweep: {WATERMARK} set to {target} ({}). The next sweep reviews what lands after it.",
                    short(&head)
                ));
            }
        },
    };
    let commits: u64 = git(&root, &["rev-list", "--count", &format!("{start}..{head}")])?
        .trim()
        .parse()
        .unwrap_or(0);
    if commits == 0 {
        return Ok(format!(
            "lens: nothing new on {target} since the last sweep ({}).",
            short(&start)
        ));
    }
    let changed = numstat(&root, &start, &head)?;
    let total: u32 = changed.iter().map(|(_, n)| n).sum();

    let review_dir = root.join(".review");
    let earlier = earlier_sweeps(&review_dir);
    if !opts.now {
        if let Some(why) = defer(&cfg.sweep, &earlier, total, now_secs())? {
            return Ok(format!(
                "lens: sweep deferred: {why}. {commits} commit(s), {total} changed line(s) wait for the next one (--now runs it anyway)."
            ));
        }
    }

    // `--now` doesn't run it anyway: the plan would only turn the agents away.
    if let Some(full) = Gate::shared().full(now_secs()) {
        return Ok(format!(
            "lens sweep: paused: {}. {commits} commit(s), {total} changed line(s) wait for the next sweep.",
            full.message(now_secs())
        ));
    }

    fs::create_dir_all(&review_dir)?;
    if !review_dir.join(".gitignore").exists() {
        fs::write(review_dir.join(".gitignore"), "*\n")?;
    }
    let stamp = free_stamp(&review_dir, &stamp_of(&review::iso_now()));
    let work = review_dir.join(format!("sweep-{stamp}"));
    let report_path = review_dir.join(format!("sweep-{stamp}.json"));
    fs::create_dir_all(&work)?;
    let branch = format!("lens/sweep-{stamp}");
    let tree = Tree::add(
        &root,
        &work.join("tree"),
        (!opts.dry_run).then_some(branch.as_str()),
        &head,
    )?;

    let files: Vec<String> = changed.iter().map(|(f, _)| f.clone()).collect();
    write_change(&tree.path, &work, &start, &head, &files, None)?;
    let whole_map = fs::read_to_string(work.join("map.json"))
        .ok()
        .and_then(|m| serde_json::from_str::<Value>(&m).ok());

    let prepared = json!({ "base": start, "mergeBase": start, "head": head });
    let mut pass = Pass::new(
        review::Options {
            repo: root.clone(),
            dry_run: opts.dry_run,
            ..Default::default()
        },
        root.clone(),
        tree.path.clone(),
        work.clone(),
        report_path.clone(),
        prepared,
    )?;
    let scope = format!(
        "\nThis is a sweep: the {commits} commit(s) pushed to {target} since {}. Raise only problems these commits caused; what was there before them is out of scope.\n",
        short(&start)
    );
    let profiles: Value = read_json(&work.join("profiles.json"))?;
    let plans = review::plan_pr(&profiles, &files);
    let mut failed: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut raised: Vec<Finding> = Vec::new();
    let mut risk = Value::Null;
    let mut assessment = Value::Null;
    let mut proposed: Vec<Value> = Vec::new();

    // The lead reads the whole range once: its assessment opens the digest, its areas split it.
    let t = Instant::now();
    if let Some(lead) = plans.iter().find(|p| p.name == LEAD && p.run) {
        pass.log("running the lead reviewer (impact-assessment) over the whole range");
        let system = pass.system_prompt(&pass.context(None)?)?;
        let sizes: String = changed
            .iter()
            .map(|(f, n)| format!("  {n}\t{f}\n"))
            .collect();
        let extra = format!(
            "{scope}\nYou are the lead reviewer. Return the assessment, risk and findings, and in place of focusAreas an `areas` list that splits the changed files for review: [{{\"title\", \"files\": [\"...\"], \"why\"}}], related files together, each area at most {} changed lines. Changed lines per file:\n{sizes}",
            cfg.sweep.area_lines
        );
        let prompt = pass.reviewer_prompt(lead, &extra)?;
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
        match crate::fix::parse_reply(&o.text) {
            Ok(r) => {
                risk = r["risk"].clone();
                assessment = r["assessment"].clone();
                proposed = r["areas"].as_array().cloned().unwrap_or_default();
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
    }
    pass.lap("lead", t);
    if let Some(why) = pass.parked() {
        return Ok(park(tree, &work, &why));
    }

    let (areas, doc_areas): (Vec<Area>, Vec<Area>) =
        split_areas(&changed, &proposed, cfg.sweep.area_lines)
            .into_iter()
            .partition(|a| !a.files.iter().all(|f| review::is_doc(f)));
    let area_of: BTreeMap<&str, usize> = areas
        .iter()
        .enumerate()
        .flat_map(|(i, a)| a.files.iter().map(move |f| (f.as_str(), i)))
        .collect();

    // Each area gets its own change block; every area's reviewers run in one batch.
    let mut systems = Vec::new();
    let mut runs = Vec::new();
    let mut specs = Vec::new();
    for (i, area) in areas.iter().enumerate() {
        let dir = work.join(format!("area-{}", i + 1));
        write_change(
            &tree.path,
            &dir,
            &start,
            &head,
            &area.files,
            whole_map.as_ref(),
        )?;
        pass.work = dir;
        let system = pass.system_prompt(&pass.context(None)?)?;
        let area_profiles = read_json(&pass.work.join("profiles.json"))?;
        let extra = format!(
            "{scope}\nThis area ({}/{}): {} — {}.{}\nThe diff above holds only this area's files; review those.\n",
            i + 1,
            areas.len(),
            area.title,
            area.files.join(", "),
            if area.why.is_empty() {
                String::new()
            } else {
                format!(" Why: {}", area.why)
            }
        );
        for plan in review::plan_pr(&area_profiles, &area.files)
            .into_iter()
            .filter(|p| p.name != LEAD)
        {
            if !plan.run {
                let mut row = plan.json(None);
                row["area"] = json!(i + 1);
                rows.push(row);
                continue;
            }
            specs.push(pass.spec(
                format!("a{}-reviewer-{}", i + 1, review::slug(&plan.name)),
                "reviewer",
                &system,
                pass.reviewer_prompt(&plan, &extra)?,
                &plan.model,
                &plan.effort,
                Access::ReadOnly,
            ));
            runs.push((i, plan));
        }
        systems.push(system);
    }
    pass.work = work.clone();
    let t = Instant::now();
    pass.log(&format!(
        "running {} reviewer(s) over {} area(s)",
        specs.len(),
        areas.len()
    ));
    let outcomes = pass.runner.run_all(specs);
    pass.tally(&outcomes);
    pass.lap("reviewers", t);
    if let Some(why) = pass.parked() {
        return Ok(park(tree, &work, &why));
    }
    for ((i, plan), o) in runs.iter().zip(&outcomes) {
        let mut row = plan.json(None);
        row["area"] = json!(i + 1);
        match crate::fix::parse_reply(&o.text) {
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
                failed.push(format!("area {}: {}", i + 1, plan.name));
            }
        }
        rows.push(row);
    }

    // Findings land in the area of their file; ones outside the range, or that an earlier sweep
    // already reported, go no further.
    let prior = prior_findings(&earlier);
    let mut by_area: Vec<Vec<Finding>> = vec![Vec::new(); areas.len()];
    let mut skipped = Vec::new();
    let mut dropped = Vec::new();
    for f in raised {
        let Some(&i) = area_of.get(f.file.as_str()) else {
            let mut entry = f.json();
            entry["reason"] = json!("outside the swept range");
            dropped.push(entry);
            continue;
        };
        by_area[i].push(f);
    }
    let mut confirmed = Vec::new();
    let mut minors = Vec::new();
    let t = Instant::now();
    for (i, found) in by_area.into_iter().enumerate() {
        let mut serious = Vec::new();
        for f in review::merge(found) {
            if let Some(by) = reported(&f.file, f.line, &f.title, &prior) {
                let mut entry = f.json();
                entry["area"] = json!(i + 1);
                entry["reason"] = json!(format!("reported by {by}"));
                skipped.push(entry);
            } else if review::rank(&f.severity) <= 1 {
                serious.push(f);
            } else {
                let mut entry = f.json();
                entry["area"] = json!(i + 1);
                minors.push(entry);
            }
        }
        let dir = work.join(format!("area-{}", i + 1));
        pass.work = dir.clone();
        pass.runner.log_dir = dir.join("agents");
        let (ok, out) = pass.verify(&systems[i], &serious, |s| review::rank(s) <= 1);
        for (f, mut entry) in ok {
            entry["area"] = json!(i + 1);
            confirmed.push((f, entry));
        }
        for mut entry in out {
            entry["area"] = json!(i + 1);
            if entry["reason"] == "below major" {
                entry.as_object_mut().unwrap().remove("reason");
                minors.push(entry);
            } else {
                dropped.push(entry);
            }
        }
    }
    pass.work = work.clone();
    pass.runner.log_dir = work.join("agents");
    pass.lap("verify", t);
    if let Some(why) = pass.parked() {
        return Ok(park(tree, &work, &why));
    }

    let mut report = json!({
        "version": 1,
        "pass": "sweep",
        "dryRun": opts.dry_run,
        "target": target,
        "since": start,
        "base": start,
        "mergeBase": start,
        "head": head,
        "commits": commits,
        "changedLines": total,
        "branch": (!opts.dry_run).then_some(&branch),
        "worktree": tree.path,
        "startedAt": pass.started_at,
        "map": review::map_status(&work),
        "risk": risk,
        "assessment": assessment,
        "areas": areas,
        "docAreas": doc_areas,
        "profiles": rows,
        "findings": review::number_all(confirmed),
        "minors": number_minors(minors),
        "skipped": skipped,
        "dropped": dropped,
        "failed": failed,
    });
    review::finish(&mut pass, &mut report)?;

    // One fix at a time, each committed on the sweep's branch or thrown away.
    let mut fixed: BTreeMap<String, String> = BTreeMap::new();
    // The review stands once written; blockers the plan left no room to fix stay open in it.
    let mut unfixed: Vec<String> = Vec::new();
    if !opts.dry_run {
        let blockers: Vec<(String, usize)> = report["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["severity"] == "blocker")
            .map(|f| {
                (
                    f["id"].as_str().unwrap_or("").to_string(),
                    f["area"].as_u64().unwrap_or(1) as usize,
                )
            })
            .collect();
        for (id, area) in blockers {
            if pass.parked().is_some() {
                unfixed.push(id);
                continue;
            }
            let dir = work.join(format!("area-{area}"));
            pass.work = dir.clone();
            pass.runner.log_dir = dir.join("agents");
            let current = read_json(&report_path)?;
            if let Err(e) = review::fix_loop(&mut pass, &current, Some(&id)) {
                pass.log(&format!("fixing {id} failed: {e:#}"));
            }
            let ledger = read_json(&work.join("fix").join(format!("{id}.json")));
            let finding = current["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["id"] == id.as_str())
                .cloned()
                .unwrap_or(Value::Null);
            if pass.parked().is_some() && !ledger.as_ref().is_ok_and(|l| l["status"] == "resolved")
            {
                tree.reset();
                unfixed.push(id);
                continue;
            }
            match ledger {
                Ok(l) if l["status"] == "resolved" => {
                    match commit_fix(&tree.path, &stamp, &id, &finding, &l) {
                        Ok(Some(sha)) => {
                            fixed.insert(id, sha);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            pass.log(&format!("committing the fix for {id} failed: {e:#}"));
                            tree.reset();
                        }
                    }
                }
                _ => tree.reset(),
            }
        }
        pass.work = work.clone();
        pass.runner.log_dir = work.join("agents");
    }

    let mut report = read_json(&report_path)?;
    for f in report["findings"].as_array_mut().into_iter().flatten() {
        if let Some(sha) = f["id"].as_str().and_then(|id| fixed.get(id)) {
            f["commit"] = json!(sha);
        }
    }
    let digest_path = review_dir.join(format!("sweep-{stamp}.md"));
    fs::write(&digest_path, digest(&report, &stamp))?;

    let mut pr_line = if opts.dry_run {
        "Dry run: fixed nothing, opened no PR.".to_string()
    } else if fixed.is_empty() {
        "No fixes to open a PR for.".to_string()
    } else {
        let base = target.strip_prefix("origin/").unwrap_or(&target);
        let title = format!("lens sweep {stamp}: {} fix(es)", fixed.len());
        let opened =
            git(&tree.path, &["push", "--quiet", "-u", "origin", &branch]).and_then(|_| {
                pr::Gh::new(&root, &cfg.pr)?.create_pr(base, &branch, &title, &digest_path)
            });
        match opened {
            Ok(url) => {
                report["pr"] = json!(url);
                format!("PR: {url}")
            }
            Err(e) => format!(
                "Opening the PR failed: {e:#}. Retry with: git push -u origin {branch} && gh pr create --base {base} --head {branch} --title {title:?} --body-file {}",
                digest_path.display()
            ),
        }
    };
    let failed = report["failed"].as_array().map_or(0, Vec::len);
    let moved = !opts.dry_run && failed == 0;
    if moved {
        git(&root, &["update-ref", WATERMARK, &head])?;
    }
    report["watermark"] = json!({ "moved": moved, "to": moved.then_some(&head) });
    review::finish(&mut pass, &mut report)?;

    if !opts.dry_run && !fixed.is_empty() && report["pr"].is_null() {
        // Keep the branch so the PR can still be opened by hand.
        pr_line.push_str(" The branch is kept.");
    }
    let count = |key: &str, sev: &str| {
        report[key]
            .as_array()
            .map_or(0, |a| a.iter().filter(|f| f["severity"] == sev).count())
    };
    let mut out = format!(
        "lens sweep: {commits} commit(s), {total} changed line(s) in {} area(s) on {target}.\nBlockers: {} ({} fixed), majors: {}, minors: {}; {} skipped as already reported.\nReport: {}\nDigest: {}\n{pr_line}",
        areas.len(),
        count("findings", "blocker"),
        fixed.len(),
        count("findings", "major"),
        report["minors"].as_array().map_or(0, Vec::len),
        report["skipped"].as_array().map_or(0, Vec::len),
        report_path.display(),
        digest_path.display(),
    );
    if let (Some(why), false) = (pass.parked(), unfixed.is_empty()) {
        out.push_str(&format!(
            "\nFixing paused: {why}. Left open: {}.",
            unfixed.join(", ")
        ));
    }
    if moved {
        out.push_str(&format!(
            "\nWatermark: {WATERMARK} moved to {}.",
            short(&head)
        ));
    } else if opts.dry_run {
        out.push_str("\nWatermark: left where it was (dry run).");
    } else {
        out.push_str(&format!(
            "\nWatermark: not moved; {failed} reviewer(s) failed, so the next sweep reviews this range again."
        ));
    }
    Ok(out)
}

/// Stops a sweep the plan turned away part way: no report, so it isn't counted as a sweep, and
/// the watermark stays for the next one.
fn park(tree: Tree, work: &Path, why: &str) -> String {
    drop(tree);
    let _ = fs::remove_dir_all(work);
    format!("lens sweep: paused: {why}. Nothing was reported and the watermark stays; the next sweep after then reviews this range.")
}

/// The sweep's worktree, removed when the sweep ends; its branch too unless it got commits.
struct Tree {
    root: PathBuf,
    path: PathBuf,
    branch: Option<String>,
    head: String,
}

impl Tree {
    fn add(root: &Path, path: &Path, branch: Option<&str>, head: &str) -> Result<Tree> {
        let _ = git(
            root,
            &["worktree", "remove", "--force", &path.display().to_string()],
        );
        let _ = fs::remove_dir_all(path);
        git(root, &["worktree", "prune"])?;
        let p = path.display().to_string();
        match branch {
            Some(b) => git(root, &["worktree", "add", "--quiet", "-B", b, &p, head])?,
            None => git(root, &["worktree", "add", "--quiet", "--detach", &p, head])?,
        };
        Ok(Tree {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            branch: branch.map(String::from),
            head: head.to_string(),
        })
    }

    /// Throws away what a failed fix left in the tree.
    fn reset(&self) {
        let _ = git(&self.path, &["reset", "--hard", "--quiet"]);
        let _ = git(&self.path, &["clean", "-fdq"]);
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = git(
            &self.root,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.display().to_string(),
            ],
        );
        if let Some(b) = &self.branch {
            let ahead = git(
                &self.root,
                &["rev-list", "--count", &format!("{}..{b}", self.head)],
            );
            if ahead.is_ok_and(|n| n.trim() == "0") {
                let _ = git(&self.root, &["branch", "-D", "--quiet", b]);
            }
        }
    }
}

/// Commits a resolved fix; `None` when it left nothing to commit.
fn commit_fix(
    tree: &Path,
    stamp: &str,
    id: &str,
    finding: &Value,
    ledger: &Value,
) -> Result<Option<String>> {
    git(tree, &["add", "-A"])?;
    if git(tree, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(None);
    }
    let tests = ledger["attempts"]
        .as_array()
        .and_then(|a| a.last())
        .map(|a| &a["fixer"]["tests"])
        .map(|t| match t {
            Value::Array(items) => items
                .iter()
                .map(|i| i.as_str().map_or_else(|| i.to_string(), String::from))
                .collect::<Vec<_>>()
                .join("; "),
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let msg = format!(
        "fix: {}\n\nlens sweep {stamp}, finding {id} ({}:{}).\n\nTests: {}\n",
        finding["title"].as_str().unwrap_or("lens finding"),
        finding["file"].as_str().unwrap_or("?"),
        finding["line"],
        if tests.is_empty() {
            "none reported"
        } else {
            &tests
        },
    );
    git(tree, &["commit", "--quiet", "--no-verify", "-m", &msg])?;
    Ok(Some(rev(tree, "HEAD")?))
}

/// Writes what a change block reads into `dir`: diff.patch, profiles.json and map.json (map.err
/// when the provider fails). With `whole`, the map is the range's, cut down to `files`.
fn write_change(
    tree: &Path,
    dir: &Path,
    start: &str,
    head: &str,
    files: &[String],
    whole: Option<&Value>,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut args = vec!["diff", start, head, "--"];
    args.extend(files.iter().map(String::as_str));
    fs::write(dir.join("diff.patch"), git(tree, &args)?)?;

    let profiles = profile::load_all(&default_profiles_dir()?, tree)?;
    let selection = serde_json::to_value(select::select(&profiles, files)?)?;
    let mut out = json!({
        "base": start,
        "mergeBase": start,
        "root": tree,
        "changedFiles": files,
        "languages": select::languages(files),
    });
    for key in ["picked", "skipped"] {
        out[key] = selection[key].clone();
    }
    fs::write(
        dir.join("profiles.json"),
        serde_json::to_string_pretty(&out)? + "\n",
    )?;

    let _ = fs::remove_file(dir.join("map.err"));
    let map = match whole {
        Some(m) => Ok(cut_map(m, files)),
        None => map::build(tree, start, None).and_then(|m| Ok(serde_json::to_value(m)?)),
    };
    match map {
        Ok(m) => fs::write(
            dir.join("map.json"),
            serde_json::to_string_pretty(&m)? + "\n",
        )?,
        Err(e) => fs::write(dir.join("map.err"), format!("{e:#}\n"))?,
    }
    Ok(())
}

/// The part of a change map about `files`.
fn cut_map(map: &Value, files: &[String]) -> Value {
    let keep: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let in_area = |v: &Value, key: &str| v[key].as_str().is_some_and(|f| keep.contains(f));
    let mut out = map.clone();
    let filter = |key: &str, field: &str| -> Value {
        map[key]
            .as_array()
            .map(|a| a.iter().filter(|v| in_area(v, field)).cloned().collect())
            .unwrap_or_default()
    };
    out["changedFiles"] = filter("changedFiles", "path");
    out["symbols"] = filter("symbols", "file");
    let names: BTreeSet<&str> = out["symbols"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str())
        .collect();
    out["tests"] = map["tests"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| {
                    in_area(t, "file")
                        || t["symbols"].as_array().is_some_and(|s| {
                            s.iter()
                                .any(|n| n.as_str().is_some_and(|n| names.contains(n)))
                        })
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    out
}

/// Splits the range into areas of at most `cap` changed lines. The lead's `proposed` areas come
/// first, cut down to files in the range that no earlier area took and split when too big; the
/// files it left out are packed in path order. A file bigger than `cap` gets an area to itself.
pub fn split_areas(changed: &[(String, u32)], proposed: &[Value], cap: u32) -> Vec<Area> {
    let cap = cap.max(1);
    let lines: BTreeMap<&str, u32> = changed.iter().map(|(f, n)| (f.as_str(), *n)).collect();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for p in proposed {
        let files: Vec<String> = p["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|f| lines.contains_key(f))
            .filter(|f| taken.insert(f.to_string()))
            .map(String::from)
            .collect();
        let title = p["title"].as_str().unwrap_or("area");
        let why = p["why"].as_str().unwrap_or("");
        out.extend(pack(&files, &lines, cap, |_| title.to_string(), why));
    }
    let rest: Vec<String> = changed
        .iter()
        .map(|(f, _)| f.clone())
        .filter(|f| !taken.contains(f))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    out.extend(pack(&rest, &lines, cap, common_dir, ""));
    out
}

fn pack(
    files: &[String],
    lines: &BTreeMap<&str, u32>,
    cap: u32,
    title: impl Fn(&[String]) -> String,
    why: &str,
) -> Vec<Area> {
    let mut chunks: Vec<(Vec<String>, u32)> = Vec::new();
    for f in files {
        let n = lines.get(f.as_str()).copied().unwrap_or(0);
        match chunks.last_mut() {
            Some((fs, total)) if *total + n <= cap => {
                fs.push(f.clone());
                *total += n;
            }
            _ => chunks.push((vec![f.clone()], n)),
        }
    }
    let count = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, (files, n))| {
            let mut t = title(&files);
            if count > 1 {
                t = format!("{t} ({}/{count})", i + 1);
            }
            Area {
                title: t,
                files,
                why: why.to_string(),
                lines: n,
            }
        })
        .collect()
}

/// The folder the files share, or `(repo root)`.
fn common_dir(files: &[String]) -> String {
    let mut parts: Vec<&str> = files
        .first()
        .map(|f| f.split('/').collect::<Vec<_>>())
        .unwrap_or_default();
    parts.pop();
    for f in files {
        let dirs: Vec<&str> = f.split('/').collect();
        let dirs = &dirs[..dirs.len() - 1];
        let same = parts.iter().zip(dirs).take_while(|(a, b)| a == b).count();
        parts.truncate(same);
    }
    if parts.is_empty() {
        "(repo root)".into()
    } else {
        parts.join("/")
    }
}

/// When the sweep should wait: today's budget is spent, or it isn't due yet (the last sweep was
/// less than `every` ago and fewer than `max_lines` lines are waiting).
pub fn defer(
    cfg: &SweepConfig,
    earlier: &[Value],
    pending: u32,
    now: u64,
) -> Result<Option<String>> {
    let today = &review::iso(now)[..10];
    let spent: f64 = earlier
        .iter()
        .filter(|r| {
            r["startedAt"]
                .as_str()
                .is_some_and(|s| s.starts_with(today))
        })
        .filter_map(|r| r["costUsd"].as_f64())
        .sum();
    if let Some(budget) = cfg.daily_budget_usd {
        if spent >= budget {
            return Ok(Some(format!(
                "today's sweeps spent ${spent:.2} of the ${budget:.2} daily budget"
            )));
        }
    }
    let every = span_secs(&cfg.every)?;
    let last = earlier
        .iter()
        .filter(|r| r["dryRun"] != true)
        .filter_map(|r| r["startedAt"].as_str())
        .max();
    match last {
        Some(last) if pending < cfg.max_lines && last >= review::iso(now.saturating_sub(every)).as_str() => {
            Ok(Some(format!(
                "the last sweep started at {last} and the next is due {} later, or once {} lines are waiting ({pending} so far)",
                cfg.every, cfg.max_lines
            )))
        }
        _ => Ok(None),
    }
}

/// Seconds in a span like `90m`, `6h` or `1d`.
pub fn span_secs(span: &str) -> Result<u64> {
    let unit = match span.chars().last() {
        Some('m') => 60,
        Some('h') => 3600,
        Some('d') => 86400,
        _ => bail!("[sweep] every takes a span like 90m, 6h or 1d, not {span:?}"),
    };
    let n: u64 = span[..span.len() - 1]
        .parse()
        .with_context(|| format!("[sweep] every takes a span like 90m, 6h or 1d, not {span:?}"))?;
    Ok(n * unit)
}

/// A finding an earlier sweep reported.
#[derive(Debug, Clone)]
pub struct Prior {
    pub key: String,
    pub file: String,
    pub line: u64,
    pub sweep: String,
}

/// The findings and minors of every earlier sweep that wasn't a dry run.
fn prior_findings(earlier: &[Value]) -> Vec<Prior> {
    let mut out = Vec::new();
    for r in earlier.iter().filter(|r| r["dryRun"] != true) {
        let sweep = r["startedAt"].as_str().unwrap_or("an earlier sweep");
        for f in ["findings", "minors"]
            .iter()
            .flat_map(|k| r[*k].as_array().into_iter().flatten())
        {
            let file = f["file"].as_str().unwrap_or("");
            out.push(Prior {
                key: pr::key(file, f["title"].as_str().unwrap_or("")),
                file: file.to_string(),
                line: f["line"].as_u64().unwrap_or(0),
                sweep: format!("the sweep of {sweep}"),
            });
        }
    }
    out
}

/// Which earlier sweep reported this finding: the same file and title, or the same file within
/// `pr::NEAR_LINES` lines.
pub fn reported<'a>(file: &str, line: u64, title: &str, prior: &'a [Prior]) -> Option<&'a str> {
    let key = pr::key(file, title);
    prior
        .iter()
        .find(|p| {
            p.key == key || (p.file == file && p.line.abs_diff(line) <= u64::from(pr::NEAR_LINES))
        })
        .map(|p| p.sweep.as_str())
}

/// Ids M1, M2… for the minors and nits, minors first.
fn number_minors(mut minors: Vec<Value>) -> Vec<Value> {
    minors.sort_by_key(|v| review::rank(v["severity"].as_str().unwrap_or("nit")));
    minors
        .into_iter()
        .enumerate()
        .map(|(i, mut v)| {
            let mut out = serde_json::Map::new();
            out.insert("id".into(), json!(format!("M{}", i + 1)));
            out.extend(std::mem::take(v.as_object_mut().unwrap()));
            Value::Object(out)
        })
        .collect()
}

/// The sweep in markdown, for the PR body and `.review/sweep-<stamp>.md`.
pub fn digest(report: &Value, stamp: &str) -> String {
    let list = |key: &str| report[key].as_array().cloned().unwrap_or_default();
    let findings = list("findings");
    let at = |f: &Value| {
        format!(
            "**{}** `{}:{}` {}",
            f["id"].as_str().unwrap_or(""),
            f["file"].as_str().unwrap_or("?"),
            f["line"],
            f["title"].as_str().unwrap_or("")
        )
    };
    let mut s = format!(
        "## lens sweep {stamp}\n\nReviewed {} commit(s) on {} ({}..{}): {} changed line(s) in {} area(s).",
        report["commits"],
        report["target"].as_str().unwrap_or("the default branch"),
        short(report["since"].as_str().unwrap_or("")),
        short(report["head"].as_str().unwrap_or("")),
        report["changedLines"],
        list("areas").len()
    );
    if let Some(risk) = report["risk"].as_str() {
        s.push_str(&format!(" Risk: {risk}."));
    }
    s.push('\n');
    if let Some(a) = report["assessment"]
        .as_str()
        .filter(|a| !a.trim().is_empty())
    {
        s.push_str(&format!("\n{}\n", a.trim()));
    }
    let blockers: Vec<&Value> = findings
        .iter()
        .filter(|f| f["severity"] == "blocker")
        .collect();
    let fixed: Vec<&&Value> = blockers
        .iter()
        .filter(|f| f["commit"].is_string())
        .collect();
    if !fixed.is_empty() {
        s.push_str("\n### Fixed\n\nOne commit each, the covering tests run.\n\n");
        for f in fixed {
            s.push_str(&format!(
                "- {} ({})\n",
                at(f),
                short(f["commit"].as_str().unwrap_or(""))
            ));
        }
    }
    let open: Vec<&&Value> = blockers
        .iter()
        .filter(|f| !f["commit"].is_string())
        .collect();
    if !open.is_empty() {
        s.push_str("\n### Blockers that need a human\n\n");
        for f in open {
            let why = match f["fix"]["status"].as_str() {
                Some(status) => format!(" — fix {status}"),
                None => String::new(),
            };
            s.push_str(&format!("- {}{why}\n", at(f)));
        }
    }
    let majors: Vec<&Value> = findings
        .iter()
        .filter(|f| f["severity"] == "major")
        .collect();
    if !majors.is_empty() {
        s.push_str("\n### Majors\n\n");
        for f in majors {
            s.push_str(&format!("- {}\n", at(f)));
        }
    }
    let minors = list("minors");
    if !minors.is_empty() {
        s.push_str("\n### Minors\n\nNot verified or fixed; listed here only.\n\n");
        for f in &minors {
            s.push_str(&format!(
                "- {} ({})\n",
                at(f),
                f["severity"].as_str().unwrap_or("minor")
            ));
        }
    }
    if findings.is_empty() && minors.is_empty() {
        s.push_str("\nNo findings.\n");
    }
    let mut notes = Vec::new();
    let skipped = list("skipped").len();
    if skipped > 0 {
        notes.push(format!(
            "{skipped} finding(s) skipped: an earlier sweep already reported them."
        ));
    }
    let failed = list("failed");
    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().filter_map(Value::as_str).collect();
        notes.push(format!(
            "Reviewers with no readable reply: {}. The next sweep reviews this range again.",
            names.join(", ")
        ));
    }
    notes.push(format!(
        "Cost: ${:.2} over {} agent(s).",
        report["costUsd"].as_f64().unwrap_or(0.0),
        report["totals"]["agents"].as_u64().unwrap_or(0)
    ));
    s.push('\n');
    s.push_str(&notes.join("\n"));
    s.push('\n');
    s
}

/// Earlier sweep reports, `.review/sweep-*.json`, oldest first.
fn earlier_sweeps(review_dir: &Path) -> Vec<Value> {
    let Ok(read) = fs::read_dir(review_dir) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("sweep-") && n.ends_with(".json"))
        })
        .filter_map(|p| read_json(&p).ok())
        .filter(|r| r["pass"] == "sweep")
        .collect();
    out.sort_by(|a, b| {
        a["startedAt"]
            .as_str()
            .unwrap_or("")
            .cmp(b["startedAt"].as_str().unwrap_or(""))
    });
    out
}

/// Changed lines per file between two commits, in path order.
fn numstat(root: &Path, start: &str, head: &str) -> Result<Vec<(String, u32)>> {
    let out = git(
        root,
        &["diff", "--numstat", "--no-renames", "-z", start, head],
    )?;
    let mut files: Vec<(String, u32)> = out
        .split('\0')
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let mut parts = r.splitn(3, '\t');
            let added = parts.next()?.parse::<u32>().unwrap_or(0);
            let deleted = parts.next()?.parse::<u32>().unwrap_or(0);
            Some((parts.next()?.to_string(), added + deleted))
        })
        .collect();
    files.sort();
    Ok(files)
}

/// origin's default branch, then origin/main, then origin/master.
fn target_branch(root: &Path) -> Result<String> {
    if let Ok(r) = git(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let r = r.trim();
        if !r.is_empty() && rev(root, r).is_ok() {
            return Ok(r.to_string());
        }
    }
    for r in ["origin/main", "origin/master"] {
        if rev(root, r).is_ok() {
            return Ok(r.to_string());
        }
    }
    bail!("origin has no default branch, main or master to sweep")
}

fn rev(dir: &Path, r: &str) -> Result<String> {
    Ok(git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ],
    )?
    .trim()
    .to_string())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// `2026-10-20T13:05:22Z` → `2026-10-20-1305`: sweeps run several times a day.
fn stamp_of(iso: &str) -> String {
    format!("{}-{}{}", &iso[..10], &iso[11..13], &iso[14..16])
}

/// `stamp`, or `stamp-2`, `stamp-3`… when a sweep already used it.
fn free_stamp(review_dir: &Path, stamp: &str) -> String {
    let used = |s: &str| {
        review_dir.join(format!("sweep-{s}.json")).exists()
            || review_dir.join(format!("sweep-{s}")).exists()
    };
    if !used(stamp) {
        return stamp.to_string();
    }
    (2..)
        .map(|n| format!("{stamp}-{n}"))
        .find(|s| !used(s))
        .unwrap()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// `$LENS_PROFILES_DIR`, else the profiles built into lens.
fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LENS_PROFILES_DIR") {
        return Ok(dir.into());
    }
    Ok(crate::assets::root()?.join("profiles"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(files: &[(&str, u32)]) -> Vec<(String, u32)> {
        files.iter().map(|(f, n)| (f.to_string(), *n)).collect()
    }

    #[test]
    fn areas_follow_the_lead_and_keep_under_the_cap() {
        let c = changed(&[
            ("README.md", 10),
            ("src/a.rs", 500),
            ("src/b.rs", 400),
            ("src/c.rs", 100),
            ("tests/t.rs", 50),
        ]);
        let proposed = vec![json!({
            "title": "parser",
            "files": ["src/a.rs", "src/b.rs", "src/gone.rs", "src/a.rs"],
            "why": "new grammar"
        })];
        let areas = split_areas(&c, &proposed, 800);
        assert_eq!(areas[0].title, "parser (1/2)");
        assert_eq!(areas[0].files, vec!["src/a.rs"]);
        assert_eq!(areas[0].why, "new grammar");
        assert_eq!(areas[1].title, "parser (2/2)");
        assert_eq!(areas[1].files, vec!["src/b.rs"]);
        // What the lead left out is packed in path order.
        assert_eq!(areas[2].files, vec!["README.md", "src/c.rs", "tests/t.rs"]);
        assert_eq!(areas[2].title, "(repo root)");
        assert_eq!(areas[2].lines, 160);
        assert!(areas.iter().all(|a| a.lines <= 800));
    }

    #[test]
    fn a_file_over_the_cap_gets_its_own_area() {
        let c = changed(&[("src/big.rs", 2000), ("src/x/y.rs", 5), ("src/x/z.rs", 5)]);
        let areas = split_areas(&c, &[], 800);
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].files, vec!["src/big.rs"]);
        assert_eq!(areas[0].title, "src (1/2)");
        assert_eq!(areas[1].files, vec!["src/x/y.rs", "src/x/z.rs"]);
        assert_eq!(areas[1].title, "src/x (2/2)");
    }

    #[test]
    fn spans_take_minutes_hours_and_days() {
        assert_eq!(span_secs("90m").unwrap(), 5400);
        assert_eq!(span_secs("6h").unwrap(), 21600);
        assert_eq!(span_secs("1d").unwrap(), 86400);
        assert!(span_secs("6").is_err());
        assert!(span_secs("h").is_err());
    }

    #[test]
    fn a_sweep_waits_for_every_unless_lines_pile_up() {
        let cfg = SweepConfig::default();
        let now = 1_792_500_000; // 2026-10-20
        let at =
            |secs: u64| json!({ "pass": "sweep", "startedAt": review::iso(secs), "costUsd": 1.0 });
        assert_eq!(defer(&cfg, &[], 10, now).unwrap(), None);
        let recent = [at(now - 3600)];
        assert!(defer(&cfg, &recent, 10, now).unwrap().is_some());
        assert_eq!(defer(&cfg, &recent, 1500, now).unwrap(), None);
        assert_eq!(defer(&cfg, &[at(now - 7 * 3600)], 10, now).unwrap(), None);
        // A dry run doesn't count as the last sweep.
        let mut dry = at(now - 60);
        dry["dryRun"] = json!(true);
        assert_eq!(defer(&cfg, &[dry], 10, now).unwrap(), None);
    }

    #[test]
    fn a_spent_budget_defers_to_the_next_sweep() {
        let cfg = SweepConfig {
            daily_budget_usd: Some(2.0),
            ..Default::default()
        };
        let now = 1_792_500_000;
        let at = |secs: u64, cost: f64| json!({ "pass": "sweep", "startedAt": review::iso(secs), "costUsd": cost, "dryRun": true });
        assert_eq!(defer(&cfg, &[at(now - 60, 1.5)], 10, now).unwrap(), None);
        let why = defer(&cfg, &[at(now - 60, 1.5), at(now - 120, 0.5)], 10, now).unwrap();
        assert!(why.unwrap().contains("daily budget"));
        // Yesterday's spend doesn't count.
        assert_eq!(defer(&cfg, &[at(now - 86400, 5.0)], 10, now).unwrap(), None);
    }

    #[test]
    fn earlier_sweeps_suppress_the_same_finding_or_a_near_line() {
        let earlier = vec![json!({
            "pass": "sweep",
            "startedAt": "2026-10-20T06:00:00Z",
            "findings": [{ "file": "src/a.rs", "line": 10, "title": "Off by one" }],
            "minors": [{ "file": "src/b.rs", "line": 50, "title": "unclear name" }],
        })];
        let prior = prior_findings(&earlier);
        assert!(reported("src/a.rs", 90, "off-by-one", &prior).is_some());
        assert!(reported("src/a.rs", 13, "something else", &prior).is_some());
        assert!(reported("src/a.rs", 14, "something else", &prior).is_none());
        assert!(reported("src/b.rs", 48, "x", &prior).is_some());
        assert!(reported("src/c.rs", 10, "Off by one", &prior).is_none());
        let mut dry = earlier[0].clone();
        dry["dryRun"] = json!(true);
        assert!(prior_findings(&[dry]).is_empty());
    }

    #[test]
    fn the_map_is_cut_to_the_area() {
        let map = json!({
            "changedFiles": [{ "path": "a.rs" }, { "path": "b.rs" }],
            "symbols": [{ "name": "f", "file": "a.rs" }, { "name": "g", "file": "b.rs" }],
            "tests": [{ "file": "t_a.rs", "symbols": ["f"] }, { "file": "t_b.rs", "symbols": ["g"] }],
        });
        let cut = cut_map(&map, &["a.rs".into()]);
        assert_eq!(cut["changedFiles"], json!([{ "path": "a.rs" }]));
        assert_eq!(cut["symbols"], json!([{ "name": "f", "file": "a.rs" }]));
        assert_eq!(
            cut["tests"],
            json!([{ "file": "t_a.rs", "symbols": ["f"] }])
        );
    }

    #[test]
    fn stamps_carry_the_time() {
        assert_eq!(stamp_of("2026-10-20T13:05:22Z"), "2026-10-20-1305");
    }

    #[test]
    fn the_digest_lists_fixes_open_blockers_majors_and_minors() {
        let report = json!({
            "commits": 3,
            "target": "origin/main",
            "since": "aaaaaaaaaa",
            "head": "bbbbbbbbbb",
            "changedLines": 120,
            "areas": [{}],
            "risk": "moderate",
            "assessment": "Touches the parser.",
            "findings": [
                { "id": "F1", "severity": "blocker", "file": "a.rs", "line": 3, "title": "crash", "commit": "ccccccccc" },
                { "id": "F2", "severity": "blocker", "file": "a.rs", "line": 9, "title": "leak", "fix": { "status": "escalated" } },
                { "id": "F3", "severity": "major", "file": "b.rs", "line": 1, "title": "slow" },
            ],
            "minors": [{ "id": "M1", "severity": "minor", "file": "c.rs", "line": 2, "title": "name" }],
            "skipped": [{}],
            "failed": [],
            "costUsd": 1.234,
            "totals": { "agents": 7 },
        });
        let d = digest(&report, "2026-10-20-1305");
        assert!(d.contains("Reviewed 3 commit(s) on origin/main (aaaaaaa..bbbbbbb)"));
        assert!(d.contains("### Fixed\n\nOne commit each, the covering tests run.\n\n- **F1** `a.rs:3` crash (ccccccc)"));
        assert!(
            d.contains("### Blockers that need a human\n\n- **F2** `a.rs:9` leak — fix escalated")
        );
        assert!(d.contains("### Majors\n\n- **F3** `b.rs:1` slow"));
        assert!(d.contains("- **M1** `c.rs:2` name (minor)"));
        assert!(d.contains("1 finding(s) skipped"));
        assert!(d.contains("Cost: $1.23 over 7 agent(s)."));
    }
}
