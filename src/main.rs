use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::Value;

use lens::{config, cost, diff, forge, map, outcome, pr, profile, review, select, usage};

/// Agent code review for Claude Code.
#[derive(Parser)]
#[command(name = "lens", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Review the change: run the reviewers, verify their findings, write the report to
    /// <state>/<branch>.json (outside the repo). Each agent
    /// runs as its own headless `claude -p`. Progress goes to stderr, the summary to stdout. With
    /// TASKBOARD_RESULT set it runs as a review-protocol reviewer: it writes the
    /// protocol's result there (docs/REVIEW.md).
    Review {
        /// The base ref. Defaults to origin's default branch, then main, then master.
        base: Option<String>,
        /// A ticket or spec the change implements, for intent-alignment.
        #[arg(long)]
        intent: Option<PathBuf>,
        /// Review the whole change, even when only part of it changed since the last review. With
        /// --pr: review again even when the PR's head hasn't changed since the last review.
        #[arg(long)]
        full: bool,
        /// The per-PR floor: one Sonnet correctness reviewer, only when the change touches a
        /// `[floor] risky` glob of .lens.toml.
        #[arg(long, conflicts_with_all = ["pr", "full"])]
        floor: bool,
        /// Run the PR pass on this PR: every profile, all severities, posted on the PR.
        #[arg(long, conflicts_with_all = ["base"])]
        pr: Option<u64>,
        /// With --pr: build the review but don't post it.
        #[arg(long, requires = "pr")]
        dry_run: bool,
        /// With --pr --dry-run: run only these reviewer profiles (comma-separated or repeated)
        /// and the verifiers of their findings, to tune one reviewer for a fraction of a full
        /// run. Implies --full, and keeps its report apart from the PR's own.
        #[arg(long, requires = "dry_run", value_delimiter = ',')]
        only: Vec<String>,
        /// The repo to review. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Pick the reviewer profiles that apply to the change since <base> and print them as JSON.
    Profiles {
        /// The ref the change is measured from (its merge base with HEAD).
        #[arg(long)]
        base: String,
        /// The repo to review. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The default profiles. Defaults to $LENS_PROFILES_DIR, then the ones built into lens.
        #[arg(long)]
        defaults: Option<PathBuf>,
        /// Keep only these picks (comma-separated or repeated) and list the rest as skipped.
        /// Defaults to $LENS_ONLY_PROFILES, which `lens review --only` sets.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Map the change since <base>: changed files and symbols, their callers and covering tests.
    Map {
        /// The ref the change is measured from (its merge base with HEAD).
        #[arg(long)]
        base: String,
        /// The repo to map. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The provider to use instead of .lens.toml's: "grep" (built in) or a program.
        #[arg(long)]
        provider: Option<String>,
    },
    /// What reviews spent: totals of cost, agents and findings per repo and per day, from the
    /// reports of reviews run by hand (in lens's state folder) and any `.review/**/*.json` an
    /// older lens left under <root> (cache tokens too, when reports carry them).
    Cost {
        /// Only reports started since a span like 1d, or a date like 2026-10-19.
        #[arg(long)]
        since: Option<String>,
        /// Where to look for repos. Defaults to ~/Development.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Count the `prev/` archives of earlier passes too.
        #[arg(long)]
        with_prev: bool,
    },
    /// What lens's agents took of the Claude plan, from the usage log (~/.lens/usage, or
    /// $LENS_USAGE_DIR): spend per repo, role and model, how far the 5-hour and 7-day windows
    /// moved, and the least lens work 1% of each window holds.
    Usage {
        /// Only usage since a span like 1d, or a date like 2026-10-19.
        #[arg(long)]
        since: Option<String>,
    },
    /// The PR pass's forge side: GitHub through `gh` or Bitbucket Cloud through its API, picked by
    /// `[pr] forge` or origin's URL (posting as .lens.toml's `[pr] post_as`).
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
}

#[derive(Subcommand)]
enum PrCommand {
    /// Fetch a PR's base branch to origin/<base> and its head to refs/lens/pr/<n>, and print its
    /// metadata as JSON (number, url, title, body, baseRefName, headRefName, headRefOid, and
    /// headRepo for a Bitbucket fork's PR).
    View {
        /// The PR number.
        #[arg(long)]
        pr: u64,
        /// The repo the PR belongs to. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Also write the metadata to <dir>/pr.json and the PR's title and description to
        /// <dir>/intent.md.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print just these fields' values, one a line, instead of the JSON.
        #[arg(long)]
        field: Vec<String>,
    },
    /// Print the findings earlier lens runs posted on a PR, as JSON.
    Earlier {
        /// The PR number.
        #[arg(long)]
        pr: u64,
        /// The repo the PR belongs to. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Post a PR pass's verified findings as one review with inline comments, skipping the ones
    /// an earlier run already posted. Prints what it posted and skipped, as JSON.
    Post {
        /// The PR number.
        #[arg(long)]
        pr: u64,
        /// The pass's report, `<state>/pr-<n>.json`.
        #[arg(long)]
        report: PathBuf,
        /// The PR's diff, to tell which lines can take an inline comment.
        #[arg(long)]
        diff: PathBuf,
        /// The repo the PR belongs to. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Print the review instead of posting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check what became of the findings lens posted on PRs that aren't settled yet (the check
    /// every review starts with), then sum up the stored outcomes by reviewer profile and
    /// severity: fixed, dismissed, ignored or still open.
    Outcomes {
        /// Check these PRs too, however they stand, and sum up only them. Repeatable.
        #[arg(long)]
        pr: Vec<u64>,
        /// The repo the PRs belong to. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The state folder holding the PRs' reports. Defaults to the repo's.
        #[arg(long)]
        state: Option<PathBuf>,
        /// Print every PR's outcomes as JSON instead of the summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfilesOut<'a> {
    base: &'a str,
    merge_base: &'a str,
    root: &'a Path,
    changed_files: &'a [String],
    languages: Vec<&'static str>,
    #[serde(flatten)]
    selection: select::Selection,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Profiles {
            base,
            repo,
            defaults,
            only,
        } => {
            let defaults = match defaults {
                Some(dir) => dir,
                None => default_profiles_dir()?,
            };
            let only = match only.is_empty() {
                true => only_from_env(env::var(ONLY_VAR).ok().as_deref()),
                false => only,
            };
            let change = diff::changed_files(&repo, &base)?;
            let profiles = profile::load_all(&defaults, &change.root)?;
            let mut selection = select::select(&profiles, &change.files)?;
            keep_only(&mut selection, &only, &profiles)?;
            let out = ProfilesOut {
                base: &base,
                merge_base: &change.merge_base,
                root: &change.root,
                changed_files: &change.files,
                languages: select::languages(&change.files).into_iter().collect(),
                selection,
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Command::Map {
            base,
            repo,
            provider,
        } => {
            let out = map::build(&repo, &base, provider.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Command::Review {
            base,
            intent,
            full,
            floor,
            pr,
            dry_run,
            only,
            repo,
        } => {
            let state = match only.is_empty() {
                true => {
                    env::remove_var(ONLY_VAR);
                    None
                }
                false => {
                    env::set_var(ONLY_VAR, only.join(","));
                    Some(outcome::state_of(&repo)?.join(ONLY_STATE))
                }
            };
            let mut opts = review::Options {
                full: full || state.is_some(),
                repo,
                base,
                intent,
                floor,
                pr,
                dry_run,
                state,
                protocol: None,
            }
            .with_protocol(|k| std::env::var(k).ok());
            opts.intent = opts.intent.map(|p| p.canonicalize()).transpose()?;
            let done = review::run(opts)?;
            println!("{}", done.summary);
        }
        Command::Cost {
            since,
            root,
            with_prev,
        } => {
            let root = match root {
                Some(r) => r,
                None => PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
                    .join("Development"),
            };
            let state_root = lens::assets::state_root().ok();
            println!(
                "{}",
                cost::run(&root, state_root.as_deref(), since.as_deref(), with_prev)?
            );
        }
        Command::Usage { since } => {
            let dir = usage::dir().context("HOME is not set")?;
            let now = lens::agent::now_secs();
            println!("{}", usage::run(&dir, since.as_deref(), now)?);
        }
        Command::Pr { command } => run_pr(command)?,
    }
    Ok(())
}

fn run_pr(command: PrCommand) -> Result<()> {
    match command {
        PrCommand::View {
            pr,
            repo,
            out,
            field,
        } => {
            let forge = forge::open(&repo, &config::load(&repo)?.pr)?;
            let view = forge.view(pr)?;
            forge.fetch(&view)?;
            let json = serde_json::to_value(&view)?;
            let pretty = serde_json::to_string_pretty(&json)?;
            if let Some(dir) = out {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                std::fs::write(dir.join("pr.json"), format!("{pretty}\n"))?;
                std::fs::write(dir.join("intent.md"), view.intent())?;
            }
            if field.is_empty() {
                println!("{pretty}");
            }
            for name in field {
                match &json[&name] {
                    Value::String(s) => println!("{s}"),
                    Value::Null => anyhow::bail!("a PR has no field {name}"),
                    v => println!("{v}"),
                }
            }
        }
        PrCommand::Earlier { pr, repo } => {
            let forge = forge::open(&repo, &config::load(&repo)?.pr)?;
            println!("{}", serde_json::to_string_pretty(&forge.earlier(pr)?)?);
        }
        PrCommand::Post {
            pr: number,
            report,
            diff,
            repo,
            dry_run,
        } => {
            let cfg = config::load(&repo)?.pr;
            let forge = forge::open(&repo, &cfg)?;
            let text = std::fs::read_to_string(&report)
                .with_context(|| format!("reading {}", report.display()))?;
            let report: pr::Report = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", report.display()))?;
            let patch = std::fs::read_to_string(&diff)
                .with_context(|| format!("reading {}", diff.display()))?;
            let plan = pr::plan(
                &report,
                &forge.earlier(number)?,
                &pr::commentable(&patch),
                cfg.comments,
                forge.markup(),
            );
            let url = match (&plan.review, dry_run) {
                (Some(review), false) => forge.post(number, review)?,
                _ => None,
            };
            let out = serde_json::json!({
                "pr": number,
                "postAs": cfg.post_as,
                "comments": cfg.comments,
                "dryRun": dry_run,
                "posted": url.is_some(),
                "event": plan.event,
                "review": url,
                "inline": plan.inline,
                "inBody": plan.in_body,
                "skipped": plan.skipped,
                "leftOut": plan.left_out,
                "payload": plan.review.as_ref().filter(|_| dry_run).map(|r| forge.payload(number, r)),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        PrCommand::Outcomes {
            pr: prs,
            repo,
            state,
            json,
        } => {
            let state = match state {
                Some(dir) => dir,
                None => outcome::state_of(&repo)?,
            };
            let forge = forge::open(&repo, &config::load(&repo)?.pr)?;
            outcome::check(&repo, &state, &*forge, &prs)?;
            let mut all = outcome::stored(&state)?;
            if !prs.is_empty() {
                all.retain(|o| prs.contains(&o.pr));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else {
                let heading = match all.as_slice() {
                    [one] => format!("PR #{}", one.pr),
                    _ => format!("{} PRs", all.len()),
                };
                let summary = outcome::Summary::of(all.iter().flat_map(|o| &o.findings));
                print!("{}", outcome::render(&heading, &summary));
            }
        }
    }
    Ok(())
}

/// Carries `lens review --only` to the `lens profiles` its prepare script runs.
const ONLY_VAR: &str = "LENS_ONLY_PROFILES";

/// The folder, inside the repo's state folder, an `--only` run keeps its work and report in, so
/// a later full review neither reuses nor builds a delta on a partial one. Git allows no branch
/// named like it, so it never meets a local pass's `<branch>/` folder.
const ONLY_STATE: &str = ".only";

fn only_from_env(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
        .collect()
}

/// Moves every pick not named in `only` to the skipped list. An empty `only` keeps them all; a
/// name no profile has is an error, so a typo doesn't silently review nothing.
fn keep_only(
    selection: &mut select::Selection,
    only: &[String],
    profiles: &[profile::Profile],
) -> Result<()> {
    if only.is_empty() {
        return Ok(());
    }
    let unknown: Vec<&str> = only
        .iter()
        .filter(|n| !profiles.iter().any(|p| &p.meta.name == *n))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let mut known: Vec<&str> = profiles.iter().map(|p| p.meta.name.as_str()).collect();
        known.sort_unstable();
        anyhow::bail!(
            "no reviewer profile named {}; the profiles are {}",
            unknown.join(", "),
            known.join(", ")
        );
    }
    let (kept, left) = std::mem::take(&mut selection.picked)
        .into_iter()
        .partition(|p| only.contains(&p.name));
    selection.picked = kept;
    selection
        .skipped
        .extend(left.into_iter().map(|p: select::Picked| select::Skipped {
            name: p.name,
            reason: "not named in --only".into(),
        }));
    Ok(())
}

/// `$LENS_PROFILES_DIR`, else the profiles built into lens.
fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LENS_PROFILES_DIR") {
        return Ok(dir.into());
    }
    Ok(lens::assets::root()?.join("profiles"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens::profile::{Profile, Source};

    fn profiles(names: &[&str]) -> Vec<Profile> {
        names
            .iter()
            .map(|n| {
                let text = format!("---\nname: {n}\ndescription: d\nmodel: sonnet\neffort: high\nseverityGuidance: g\nenabled: true\n---\nprompt\n");
                Profile::parse(
                    &text,
                    PathBuf::from(format!("{n}/profile.md")),
                    Source::Default,
                )
                .unwrap()
            })
            .collect()
    }

    fn names<T>(items: &[T], name: impl Fn(&T) -> &str) -> Vec<&str> {
        items.iter().map(name).collect()
    }

    #[test]
    fn keeps_only_the_named_picks_and_skips_the_rest() {
        let all = profiles(&["correctness", "testing", "security"]);
        let mut sel = select::select(&all, &["src/a.rs".into()]).unwrap();
        keep_only(&mut sel, &["testing".into()], &all).unwrap();
        assert_eq!(names(&sel.picked, |p| &p.name), ["testing"]);
        assert_eq!(
            names(&sel.skipped, |s| &s.name),
            ["correctness", "security"]
        );
        assert!(sel
            .skipped
            .iter()
            .all(|s| s.reason == "not named in --only"));
    }

    #[test]
    fn no_names_keeps_every_pick() {
        let all = profiles(&["correctness", "testing"]);
        let mut sel = select::select(&all, &["src/a.rs".into()]).unwrap();
        keep_only(&mut sel, &[], &all).unwrap();
        assert_eq!(names(&sel.picked, |p| &p.name), ["correctness", "testing"]);
        assert!(sel.skipped.is_empty());
    }

    #[test]
    fn an_unknown_name_is_an_error_listing_the_profiles() {
        let all = profiles(&["testing", "correctness"]);
        let mut sel = select::select(&all, &["src/a.rs".into()]).unwrap();
        let err = keep_only(&mut sel, &["tesing".into()], &all).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no reviewer profile named tesing; the profiles are correctness, testing"
        );
    }

    #[test]
    fn reads_the_names_lens_review_passes_down() {
        assert_eq!(
            only_from_env(Some("testing, security,")),
            ["testing", "security"]
        );
        assert!(only_from_env(Some("")).is_empty());
        assert!(only_from_env(None).is_empty());
    }

    #[test]
    fn only_needs_a_dry_run_of_a_pr() {
        let parse = |args: &[&str]| Cli::try_parse_from(["lens", "review"].iter().chain(args));
        assert!(parse(&["--pr", "15", "--only", "testing"]).is_err());
        let Command::Review { only, .. } =
            parse(&["--pr", "15", "--dry-run", "--only", "testing,security"])
                .unwrap()
                .command
        else {
            panic!("not a review");
        };
        assert_eq!(only, ["testing", "security"]);
    }
}
