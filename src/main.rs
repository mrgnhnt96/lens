use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use lens::{diff, map, profile, select};

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
    }
    Ok(())
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
