use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use lens::{config, cost, diff, fix, map, pr, profile, review, select, sweep};

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
    /// .review/<branch>.json and take blockers through the fix loop. Each agent runs as its own
    /// headless `claude -p`. Progress goes to stderr, the summary to stdout.
    Review {
        /// The base ref. Defaults to origin's default branch, then main, then master.
        base: Option<String>,
        /// A ticket or spec the change implements, for intent-alignment.
        #[arg(long)]
        intent: Option<PathBuf>,
        /// Report blockers without running the fix loop.
        #[arg(long)]
        no_fix: bool,
        /// Review the whole change, even when only part of it changed since the last review.
        #[arg(long)]
        full: bool,
        /// The per-PR floor: one Sonnet correctness reviewer, only when the change touches a
        /// `[floor] risky` glob of .lens.toml; no fix loop.
        #[arg(long, conflicts_with_all = ["pr", "full"])]
        floor: bool,
        /// Run the PR pass on this PR: every profile, all severities, posted on the PR.
        #[arg(long, conflicts_with_all = ["base", "no_fix", "full"])]
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
    /// The blocker fix loop's ledger: start it, record attempts and judgments, summarize it.
    Fix {
        #[command(subcommand)]
        command: FixCommand,
    },
    /// Review everything pushed to origin's default branch since the last sweep (the
    /// refs/lens/swept watermark): reviewers per area, the verifier, the fix loop on blockers in
    /// a worktree on lens/sweep-<stamp>, and one PR with the digest. The report goes to
    /// .review/sweep-<stamp>.json.
    Sweep {
        /// Review from this ref instead of the watermark.
        #[arg(long)]
        since: Option<String>,
        /// Review and report, but fix nothing, open no PR and leave the watermark.
        #[arg(long)]
        dry_run: bool,
        /// Run even when [sweep] says it isn't due or today's budget is spent.
        #[arg(long)]
        now: bool,
        /// The repo to sweep. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// What reviews spent: totals of cost, agents and findings per repo and per day, from the
    /// `.review/**/*.json` reports under <root> (cache tokens too, when reports carry them).
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
    /// The PR pass's GitHub side, through `gh` (posting as .lens.toml's `[pr] post_as`).
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
}

#[derive(Subcommand)]
enum FixCommand {
    /// Open the loop for one finding of a `lens review` report and snapshot the working tree.
    Start {
        /// The report, `.review/<branch>.json`.
        #[arg(long)]
        report: PathBuf,
        /// The finding's id, e.g. F1.
        #[arg(long)]
        finding: String,
        /// Attempts before the loop escalates whatever the judge says.
        #[arg(long, default_value_t = fix::DEFAULT_CAP)]
        cap: u32,
    },
    /// Record the attempt the fixer just made and its re-review.
    Record {
        #[arg(long)]
        ledger: PathBuf,
        /// The fixer's reply (JSON, or text ending in a ```json block).
        #[arg(long)]
        fixer: Option<PathBuf>,
        /// The re-review: {"original": <verifier reply>, "newFindings": [...]}. Leave it out
        /// only when the fixer changed nothing.
        #[arg(long)]
        review: Option<PathBuf>,
    },
    /// Print the diff of the attempt in progress (empty when the fixer changed nothing).
    Diff {
        #[arg(long)]
        ledger: PathBuf,
    },
    /// Record the escalation judge's call on the last failed attempt.
    Judge {
        #[arg(long)]
        ledger: PathBuf,
        /// The judge's reply: {"decision": "continue"|"escalate", "reason", "needs"}.
        #[arg(long)]
        decision: PathBuf,
    },
    /// Print the loop's summary for the human.
    Summary {
        #[arg(long)]
        ledger: PathBuf,
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
        /// The pass's report, `.review/pr-<n>.json`.
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
            no_fix,
            full,
            floor,
            pr,
            dry_run,
            repo,
        } => {
            let intent = intent.map(|p| p.canonicalize()).transpose()?;
            let done = review::run(review::Options {
                repo,
                base,
                intent,
                no_fix,
                full,
                floor,
                pr,
                dry_run,
            })?;
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
            println!("{}", cost::run(&root, since.as_deref(), with_prev)?);
        }
        Command::Sweep {
            since,
            dry_run,
            now,
            repo,
        } => {
            println!(
                "{}",
                sweep::run(sweep::Options {
                    repo,
                    since,
                    dry_run,
                    now,
                })?
            );
        }
        Command::Fix { command } => run_fix(command)?,
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
                "posted": plan.review.is_some() && !dry_run,
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

fn run_fix(command: FixCommand) -> Result<()> {
    let step = match command {
        FixCommand::Start {
            report,
            finding,
            cap,
        } => fix::start(&report, &finding, cap)?,
        FixCommand::Record {
            ledger,
            fixer,
            review,
        } => {
            let fixer = fixer.map(|p| read_reply(&p)).transpose()?;
            let review = review
                .map(|p| Ok::<_, anyhow::Error>(serde_json::from_value(read_reply(&p)?)?))
                .transpose()?;
            fix::record(&ledger, fixer, review)?
        }
        FixCommand::Judge { ledger, decision } => {
            fix::judge(&ledger, serde_json::from_value(read_reply(&decision)?)?)?
        }
        FixCommand::Diff { ledger } => {
            print!("{}", fix::pending_diff(&ledger)?);
            return Ok(());
        }
        FixCommand::Summary { ledger } => {
            print!("{}", fix::summary(&ledger)?);
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&step)?);
    Ok(())
}

fn read_reply(path: &Path) -> Result<serde_json::Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    fix::parse_reply(&text).with_context(|| format!("in {}", path.display()))
}

/// `$LENS_PROFILES_DIR`, else the profiles built into lens.
fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LENS_PROFILES_DIR") {
        return Ok(dir.into());
    }
    Ok(lens::assets::root()?.join("profiles"))
}
