use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use lens::{diff, fix, map, profile, select};

/// Agent code review for Claude Code.
#[derive(Parser)]
#[command(name = "lens", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pick the reviewer profiles that apply to the change since <base> and print them as JSON.
    Profiles {
        /// The ref the change is measured from (its merge base with HEAD).
        #[arg(long)]
        base: String,
        /// The repo to review. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The default profiles. Defaults to $LENS_PROFILES_DIR, then the plugin's profiles/.
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
}

#[derive(Subcommand)]
enum FixCommand {
    /// Open the loop for one finding of a /review report and snapshot the working tree.
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
        Command::Fix { command } => run_fix(command)?,
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

/// `$LENS_PROFILES_DIR`, then `$CLAUDE_PLUGIN_ROOT/profiles`, then a `profiles/` folder beside a
/// `.claude-plugin/` in one of the binary's ancestors (a cargo build inside the plugin checkout).
fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LENS_PROFILES_DIR") {
        return Ok(dir.into());
    }
    if let Some(root) = env::var_os("CLAUDE_PLUGIN_ROOT") {
        let dir = PathBuf::from(root).join("profiles");
        if dir.is_dir() {
            return Ok(dir);
        }
    }
    let exe = env::current_exe()?.canonicalize()?;
    for dir in exe.ancestors().skip(1) {
        if dir.join(".claude-plugin").is_dir() && dir.join("profiles").is_dir() {
            return Ok(dir.join("profiles"));
        }
    }
    bail!("can't find the default profiles; pass --defaults or set LENS_PROFILES_DIR")
}
