//! The built-in provider: `git diff` for what changed, line patterns ([`super::symbols`]) for
//! which symbols that touches, and `git grep` for who mentions them.
//!
//! A changed symbol is one defined on a changed line, or the nearest definition enclosing a
//! changed line (found by walking up to the first less-indented line that defines something).
//! Callers are whole-word mentions of the name anywhere in the repo, so they over-report: a
//! common name matches unrelated code. A mention counts as a test when its file looks like a test
//! (see [`is_test_path`]) or sits below `#[cfg(test)]` in a Rust file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use super::symbols::{definition, Def};
use super::{ChangedFile, Location, ProviderOutput, Symbol, SymbolChange, TestFile};
use crate::diff::{self, git, split_nul, Status};

/// The most callers, and the most tests, kept per symbol.
pub const MAX_REFS: usize = 50;
/// The most symbols looked up; the rest are dropped.
pub const MAX_SYMBOLS: usize = 200;
const MAX_TEXT: usize = 200;

pub fn run(root: &Path, merge_base: &str, files: &[ChangedFile]) -> Result<ProviderOutput> {
    let mut symbols = Vec::new();
    for file in files {
        let Some(lang) = file.language else { continue };
        symbols.extend(changed_symbols(root, merge_base, file, lang)?);
    }
    symbols.truncate(MAX_SYMBOLS);

    let mut rust_test_start = HashMap::new();
    for symbol in &mut symbols {
        for hit in references(root, &symbol.name)? {
            if symbol.change != SymbolChange::Removed
                && hit.file == symbol.file
                && hit.line == symbol.line
            {
                continue;
            }
            let list = if is_test_hit(root, &hit, &mut rust_test_start) {
                &mut symbol.tests
            } else {
                &mut symbol.callers
            };
            if list.len() < MAX_REFS {
                list.push(hit);
            } else {
                symbol.truncated = true;
            }
        }
    }

    let tests = covering_tests(root, files, &symbols)?;
    Ok(ProviderOutput { symbols, tests })
}

/// The symbols one file's change adds, edits or removes.
fn changed_symbols(
    root: &Path,
    merge_base: &str,
    file: &ChangedFile,
    lang: &str,
) -> Result<Vec<Symbol>> {
    let changes = diff::line_changes(root, merge_base, &file.path)?;
    let lines: Vec<String> = if file.status == Status::Deleted {
        Vec::new()
    } else {
        let bytes = std::fs::read(root.join(&file.path)).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect()
    };
    let removed_defs: Vec<(u32, Def)> = changes
        .removed
        .iter()
        .filter_map(|(line, text)| definition(lang, text).map(|d| (*line, d)))
        .collect();
    let removed_names: HashSet<&str> = removed_defs.iter().map(|(_, d)| d.name.as_str()).collect();

    // Keyed by (name, line) so a symbol touched on many lines shows up once, in file order.
    let mut found: BTreeMap<(u32, String), Symbol> = BTreeMap::new();
    let mut add = |line: u32, def: Def, change: SymbolChange| {
        found
            .entry((line, def.name.clone()))
            .or_insert_with(|| Symbol {
                name: def.name,
                kind: def.kind,
                file: file.path.clone(),
                line,
                change,
                callers: Vec::new(),
                tests: Vec::new(),
                truncated: false,
            });
    };

    for (line, text) in &changes.added {
        if let Some(def) = definition(lang, text) {
            let change = if removed_names.contains(def.name.as_str()) {
                SymbolChange::Modified
            } else {
                SymbolChange::Added
            };
            add(*line, def, change);
        } else if let Some((at, def)) = enclosing(&lines, lang, *line, text) {
            add(at, def, SymbolChange::Modified);
        }
    }
    // A removed line belongs to whatever enclosed it at the merge base. When that symbol still
    // exists it was modified; when it doesn't, its removed definition line reports it below.
    let old_lines = if changes.removed.is_empty() {
        Vec::new()
    } else {
        diff::lines_at(root, merge_base, &file.path)
    };
    for (line, text) in &changes.removed {
        if definition(lang, text).is_some() {
            continue;
        }
        let Some((_, def)) = enclosing(&old_lines, lang, *line, text) else {
            continue;
        };
        if let Some((at, now)) = defined_in(&lines, lang, &def.name) {
            add(at, now, SymbolChange::Modified);
        }
    }
    let mut symbols: Vec<Symbol> = found.into_values().collect();
    for (line, def) in removed_defs {
        if symbols.iter().any(|s| s.name == def.name) {
            continue;
        }
        // Still defined elsewhere in the file: it moved or its signature changed.
        let (line, change) = match defined_in(&lines, lang, &def.name) {
            Some((at, _)) => (at, SymbolChange::Modified),
            None => (line, SymbolChange::Removed),
        };
        symbols.push(Symbol {
            name: def.name,
            kind: def.kind,
            file: file.path.clone(),
            line,
            change,
            callers: Vec::new(),
            tests: Vec::new(),
            truncated: false,
        });
    }
    Ok(symbols)
}

/// Where `name` is defined in `lines`, if anywhere.
fn defined_in(lines: &[String], lang: &str, name: &str) -> Option<(u32, Def)> {
    (1..).zip(lines).find_map(|(n, l)| {
        definition(lang, l)
            .filter(|d| d.name == name)
            .map(|d| (n, d))
    })
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The definition enclosing a changed line: walking up from `from` (1-based, inclusive), the
/// first line indented less than everything seen so far that defines something. Closing
/// brackets and `where` clauses are skipped so a multi-line signature doesn't end the walk.
fn enclosing(lines: &[String], lang: &str, from: u32, changed: &str) -> Option<(u32, Def)> {
    if changed.trim().is_empty() {
        return None;
    }
    let mut target = indent(changed);
    let from = (from as usize).min(lines.len());
    for i in (0..from).rev() {
        let line = &lines[i];
        let trimmed = line.trim_start();
        if trimmed.is_empty() || indent(line) >= target {
            continue;
        }
        if trimmed.starts_with([')', ']', '}']) || trimmed.starts_with("where") {
            continue;
        }
        if let Some(def) = definition(lang, line) {
            return Some((i as u32 + 1, def));
        }
        target = indent(line);
        if target == 0 {
            return None;
        }
    }
    None
}

/// Whole-word mentions of `name` in tracked and untracked (not ignored) files.
fn references(root: &Path, name: &str) -> Result<Vec<Location>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "grep",
            "-n",
            "-z",
            "-I",
            "-w",
            "-F",
            "--untracked",
            "-e",
            name,
        ])
        .output()?;
    // Exit 1 is "no matches".
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\0');
            let file = parts.next()?.to_string();
            let line = parts.next()?.parse().ok()?;
            let mut text = parts.next().unwrap_or("").trim().to_string();
            if text.len() > MAX_TEXT {
                let cut = (0..=MAX_TEXT)
                    .rev()
                    .find(|&i| text.is_char_boundary(i))
                    .unwrap_or(0);
                text.truncate(cut);
            }
            Some(Location { file, line, text })
        })
        .collect())
}

/// Paths that hold tests: a `test`/`tests`/`spec`/`__tests__`/`integration_test` folder, or a
/// name like `foo_test.go`, `test_foo.py`, `foo.test.ts`, `foo_spec.rb` or `FooTest.java`.
pub fn is_test_path(path: &str) -> bool {
    let p = Path::new(path);
    let in_test_dir = p.parent().is_some_and(|dir| {
        dir.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("test" | "tests" | "spec" | "__tests__" | "integration_test" | "test_driver")
            )
        })
    });
    in_test_dir || test_stem(path).is_some()
}

/// The name a test file is about, when its file name says it's a test: `foo` for
/// `foo_test.rs`, `test_foo.py`, `foo.test.ts`, `FooTest.java`.
fn test_stem(path: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let stem = name.split('.').next()?;
    let rest = &name[stem.len()..];
    if rest.starts_with(".test.") || rest.starts_with(".spec.") {
        return Some(stem.to_string());
    }
    for suffix in ["_test", "_spec", "Tests", "Test", "Spec"] {
        if let Some(s) = stem.strip_suffix(suffix) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    stem.strip_prefix("test_").map(str::to_string)
}

fn is_test_hit(
    root: &Path,
    hit: &Location,
    rust_test_start: &mut HashMap<String, Option<u32>>,
) -> bool {
    if is_test_path(&hit.file) {
        return true;
    }
    if !hit.file.ends_with(".rs") {
        return false;
    }
    let start = rust_test_start.entry(hit.file.clone()).or_insert_with(|| {
        let text = std::fs::read_to_string(root.join(&hit.file)).unwrap_or_default();
        (1..)
            .zip(text.lines())
            .find(|(_, l)| l.trim_start().starts_with("#[cfg(test)]"))
            .map(|(n, _)| n)
    });
    start.is_some_and(|s| hit.line > s)
}

/// Test files that cover the change: changed tests, tests that mention a changed symbol, and
/// tests named after a changed file.
fn covering_tests(root: &Path, files: &[ChangedFile], symbols: &[Symbol]) -> Result<Vec<TestFile>> {
    let mut tests: BTreeMap<String, TestFile> = BTreeMap::new();
    fn entry<'a>(tests: &'a mut BTreeMap<String, TestFile>, file: &str) -> &'a mut TestFile {
        tests.entry(file.to_string()).or_insert_with(|| TestFile {
            file: file.to_string(),
            ..TestFile::default()
        })
    }
    for f in files.iter().filter(|f| f.status != Status::Deleted) {
        if is_test_path(&f.path) {
            entry(&mut tests, &f.path).changed = true;
        }
    }
    for s in symbols {
        for hit in &s.tests {
            let t = entry(&mut tests, &hit.file);
            if !t.symbols.contains(&s.name) {
                t.symbols.push(s.name.clone());
            }
        }
    }

    // `src/foo.rs` → any test file whose name, minus its test affixes, is `foo`.
    let mut by_stem: HashMap<String, Vec<&str>> = HashMap::new();
    for f in files
        .iter()
        .filter(|f| f.language.is_some() && !is_test_path(&f.path))
    {
        if let Some(stem) = Path::new(&f.path).file_stem().and_then(|s| s.to_str()) {
            if !matches!(
                stem,
                "mod" | "index" | "main" | "lib" | "__init__" | "utils"
            ) {
                by_stem.entry(stem.to_string()).or_default().push(&f.path);
            }
        }
    }
    if !by_stem.is_empty() {
        let all = git(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        for path in split_nul(&all).filter(|p| is_test_path(p)) {
            let stem = test_stem(&path).or_else(|| {
                Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            });
            let Some(sources) = stem.and_then(|s| by_stem.get(&s)) else {
                continue;
            };
            let t = entry(&mut tests, &path);
            for src in sources {
                t.named_after.push(src.to_string());
            }
        }
    }
    Ok(tests.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths() {
        for p in [
            "tests/cli.rs",
            "pkg/foo_test.go",
            "app/test_models.py",
            "web/a.test.ts",
            "web/__tests__/a.ts",
            "src/main/java/FooTest.java",
            "spec/user_spec.rb",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/testing.rs", "src/contest.py", "latest.ts", "Test.java"] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn test_stems() {
        assert_eq!(test_stem("a/foo_test.dart").as_deref(), Some("foo"));
        assert_eq!(test_stem("test_foo.py").as_deref(), Some("foo"));
        assert_eq!(test_stem("foo.spec.ts").as_deref(), Some("foo"));
        assert_eq!(test_stem("FooTest.java").as_deref(), Some("Foo"));
        assert_eq!(test_stem("foo.rs"), None);
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn enclosing_walks_past_nested_blocks_and_multiline_signatures() {
        let src = lines(
            "fn a() {}\n\
             pub fn b(\n    x: u8,\n) -> u8 {\n    if x > 1 {\n        x + 1\n    } else {\n        0\n    }\n}\n",
        );
        let (at, def) = enclosing(&src, "rust", 8, "        0").unwrap();
        assert_eq!((at, def.name.as_str()), (2, "b"));
        let (at, def) = enclosing(&src, "rust", 6, "        x + 1").unwrap();
        assert_eq!((at, def.name.as_str()), (2, "b"));
    }

    #[test]
    fn top_level_lines_have_no_enclosing_symbol() {
        let src = lines("fn a() {\n    1\n}\nuse x;\n");
        assert!(enclosing(&src, "rust", 4, "use x;").is_none());
        assert!(enclosing(&src, "rust", 2, "   ").is_none());
    }
}
