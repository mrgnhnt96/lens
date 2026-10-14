//! The change map: the files a change touches, the symbols it adds, edits or removes, who calls
//! them and which tests cover them. A provider builds it; the built-in one is [`grep`], and a repo
//! can name its own in `.lens.toml` (the contract is in docs/PROVIDERS.md).

pub mod grep;
pub mod symbols;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, Input, ProviderSpec};
use crate::diff::{self, Status};
use crate::lang::language_of;

/// The version of the provider contract. Bumped on breaking changes to [`ProviderOutput`].
pub const VERSION: u32 = 1;

/// The name of the built-in provider.
pub const BUILTIN: &str = "grep";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeMap {
    pub version: u32,
    pub base: String,
    pub merge_base: String,
    pub root: PathBuf,
    pub provider: String,
    pub changed_files: Vec<ChangedFile>,
    #[serde(flatten)]
    pub output: ProviderOutput,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<&'static str>,
}

/// What a provider prints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOutput {
    #[serde(default)]
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub tests: Vec<TestFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolChange {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// Where it's defined: in the working tree, or at the merge base when `removed`.
    pub file: String,
    pub line: u32,
    pub change: SymbolChange,
    #[serde(default)]
    pub callers: Vec<Location>,
    #[serde(default)]
    pub tests: Vec<Location>,
    /// Some callers or tests were left out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// A test file that covers the change, and why lens thinks so.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFile {
    pub file: String,
    /// The change edits the test itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed: bool,
    /// Changed symbols it references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    /// Changed files it's named after (`foo.rs` → `foo_test.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named_after: Vec<String>,
}

/// Builds the change map for everything that changed since the merge base of `base` and `HEAD`.
/// `provider` overrides `.lens.toml`'s `[map] provider`.
pub fn build(repo: &Path, base: &str, provider: Option<&str>) -> Result<ChangeMap> {
    let change = diff::changed_files(repo, base)?;
    let changed_files: Vec<ChangedFile> = diff::statuses(&change)?
        .into_iter()
        .map(|(path, status)| ChangedFile {
            language: language_of(&path),
            path,
            status,
        })
        .collect();
    let cfg = config::load(&change.root)?.map;
    let spec = match provider {
        Some(p) => ProviderSpec::Program(p.to_string()),
        None => cfg
            .provider
            .unwrap_or_else(|| ProviderSpec::Program(BUILTIN.into())),
    };
    let (name, output) = match spec {
        ProviderSpec::Program(p) if p == BUILTIN => (
            BUILTIN.to_string(),
            grep::run(&change.root, &change.merge_base, &changed_files)?,
        ),
        ProviderSpec::Program(p) => (p.clone(), external(&[p], cfg.input, base, &change)?),
        ProviderSpec::Argv(argv) if argv.is_empty() => {
            bail!("{}: [map] provider is an empty list", config::FILE)
        }
        ProviderSpec::Argv(argv) => (argv.join(" "), external(&argv, cfg.input, base, &change)?),
    };
    Ok(ChangeMap {
        version: VERSION,
        base: base.to_string(),
        merge_base: change.merge_base,
        root: change.root,
        provider: name,
        changed_files,
        output,
    })
}

/// Runs a repo's provider from the repo root and reads its JSON.
fn external(
    argv: &[String],
    input: Input,
    base: &str,
    change: &diff::Change,
) -> Result<ProviderOutput> {
    let program = Path::new(&argv[0]);
    let program = if program.is_relative() && argv[0].contains('/') {
        change.root.join(program)
    } else {
        program.to_path_buf()
    };
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .current_dir(&change.root)
        .env("LENS_MAP_VERSION", VERSION.to_string())
        .env("LENS_BASE", base)
        .env("LENS_MERGE_BASE", &change.merge_base)
        .env("LENS_ROOT", &change.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input == Input::Args {
        cmd.args(&change.files);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting map provider {}", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let list = match input {
        Input::Stdin => change.files.iter().map(|f| format!("{f}\n")).collect(),
        Input::Args => String::new(),
    };
    // Written from a thread so a provider that prints before reading all of stdin can't deadlock.
    let writer = std::thread::spawn(move || stdin.write_all(list.as_bytes()));
    let out = child.wait_with_output()?;
    // A provider that takes its input from args may exit without reading stdin.
    let _ = writer.join();
    if !out.status.success() {
        bail!(
            "map provider {} failed ({}): {}",
            program.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_output(&out.stdout)
        .with_context(|| format!("reading map provider {}'s output", program.display()))
}

fn parse_output(stdout: &[u8]) -> Result<ProviderOutput> {
    #[derive(Deserialize)]
    struct Versioned {
        version: Option<u32>,
    }
    let v: Versioned = serde_json::from_slice(stdout).context("not a JSON object")?;
    if let Some(version) = v.version {
        if version != VERSION {
            bail!("it speaks change-map version {version}; lens speaks {VERSION}");
        }
    }
    Ok(serde_json::from_slice(stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_output_defaults_missing_lists() {
        let out = parse_output(br#"{"symbols":[{"name":"a","kind":"function","file":"x.rs","line":1,"change":"added"}]}"#).unwrap();
        assert_eq!(out.symbols.len(), 1);
        assert!(out.symbols[0].callers.is_empty());
        assert!(out.tests.is_empty());
    }

    #[test]
    fn provider_output_rejects_other_versions_and_bad_shapes() {
        assert!(parse_output(br#"{"version":2}"#).is_err());
        assert!(parse_output(br#"{"symbols":[{"name":"a"}]}"#).is_err());
        assert!(parse_output(b"not json").is_err());
    }
}
