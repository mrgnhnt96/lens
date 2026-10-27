use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use lens::{config, cost, diff, map, pr, profile, review, select, usage};

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
        /// Review the whole change, even when only part of it changed since the last review.
        #[arg(long)]
        full: bool,
        /// The per-PR floor: one Sonnet correctness reviewer, only when the change touches a
        /// `[floor] risky` glob of .lens.toml.
        #[arg(long, conflicts_with_all = ["pr", "full"])]
        floor: bool,
        /// Run the PR pass on this PR: every profile, all severities, posted on the PR.
        #[arg(long, conflicts_with_all = ["base", "full"])]
        pr: Option<u64>,
        /// With --pr: build the review but don't post it.
        #[arg(long, requires = "pr")]
        dry_run: bool,
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
    /// The PR pass's GitHub side, through `gh` (posting as .lens.toml's `[pr] post_as`).
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
}

#[derive(Subcommand)]
enum PrCommand {
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
        } => {
            let defaults = match defaults {
                Some(dir) => dir,
                None => default_profiles_dir()?,
            };
            let change = diff::changed_files(&repo, &base)?;
            let profiles = profile::load_all(&defaults, &change.root)?;
            let out = ProfilesOut {
                base: &base,
                merge_base: &change.merge_base,
                root: &change.root,
                changed_files: &change.files,
                languages: select::languages(&change.files).into_iter().collect(),
                selection: select::select(&profiles, &change.files)?,
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
            repo,
        } => {
            let mut opts = review::Options {
                repo,
                base,
                intent,
                full,
                floor,
                pr,
                dry_run,
                state: None,
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
        PrCommand::Earlier { pr, repo } => {
            let gh = pr::Gh::new(&repo, &config::load(&repo)?.pr)?;
            println!("{}", serde_json::to_string_pretty(&gh.earlier(pr)?)?);
        }
        PrCommand::Post {
            pr: number,
            report,
            diff,
            repo,
            dry_run,
        } => {
            let cfg = config::load(&repo)?.pr;
            let gh = pr::Gh::new(&repo, &cfg)?;
            let text = std::fs::read_to_string(&report)
                .with_context(|| format!("reading {}", report.display()))?;
            let report: pr::Report = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", report.display()))?;
            let patch = std::fs::read_to_string(&diff)
                .with_context(|| format!("reading {}", diff.display()))?;
            let plan = pr::plan(&report, &gh.earlier(number)?, &pr::commentable(&patch));
            let url = match (&plan.review, dry_run) {
                (Some(review), false) => gh.post(number, review)?,
                _ => None,
            };
            let out = serde_json::json!({
                "pr": number,
                "postAs": cfg.post_as,
                "dryRun": dry_run,
                "posted": url.is_some(),
                "event": plan.event,
                "review": url,
                "inline": plan.inline,
                "inBody": plan.in_body,
                "skipped": plan.skipped,
                "payload": if dry_run { plan.review } else { None },
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
    }
    Ok(())
}

/// `$LENS_PROFILES_DIR`, else the profiles built into lens.
fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LENS_PROFILES_DIR") {
        return Ok(dir.into());
    }
    Ok(lens::assets::root()?.join("profiles"))
}
