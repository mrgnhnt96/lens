//! Repo guideline files (`CLAUDE.md`, `AGENTS.md`) with their `@path` imports resolved, the way
//! Claude Code reads them, so a reviewer sees the rules a line like `@CODING_GUIDELINES.md` pulls
//! in rather than the line itself.
//!
//! An import is a line whose first word (after indentation and an optional `-`, `*` or `+` list
//! marker) is `@path`. The path is relative to the importing file's folder, or `~/…` or absolute.
//! Whatever it names must be a file inside the reviewed tree, symlinks resolved: anything outside
//! it is never read. The imported file's text goes right after the import line, between markers
//! naming it, and its own imports are resolved the same way, up to [`MAX_DEPTH`] hops from the
//! guideline file. A file already shown (a cycle, or imported twice) isn't shown again. Lines
//! inside fenced code blocks aren't imports.
//!
//! The expanded text is cut at a character cap, and a note at the end names the files that didn't
//! fit, so a reviewer knows what to read for the rest.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// How many imports deep a chain is followed: Claude Code's limit.
pub const MAX_DEPTH: usize = 5;

/// The files a pass has already shown, shared across a pass's guideline files so a file two of
/// them import (or one that is also a guideline itself) appears once.
#[derive(Debug, Default)]
pub struct Seen(HashSet<PathBuf>);

impl Seen {
    /// Marks `file` as shown; false when it already was.
    pub fn insert(&mut self, tree: &Path, file: &Path) -> bool {
        self.0.insert(canonical(tree, file))
    }
}

/// `file` (a path inside `tree`) with its imports inlined, cut at `cap` characters. `file` is
/// marked as seen; the caller decides whether to show it at all with [`Seen::insert`] first.
pub fn expand(tree: &Path, file: &Path, cap: usize, seen: &mut Seen) -> String {
    let root = tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf());
    seen.0.insert(canonical(tree, file));
    let mut e = Expansion {
        root,
        cap,
        out: String::new(),
        len: 0,
        unshown: Vec::new(),
        seen,
    };
    let text = fs::read_to_string(tree.join(file)).unwrap_or_default();
    e.push_file(&text, &tree.join(file), 0);
    e.finish()
}

/// A `## Repo guideline:` section per file of `paths` (relative to `tree`), each expanded and
/// capped by [`expand`]. A guideline another one already showed (it imported it, or it is a
/// symlink to one) gets a one-line section saying so.
pub fn sections(tree: &Path, paths: &[String], cap: usize) -> String {
    let mut seen = Seen::default();
    let mut s = String::new();
    for path in paths {
        let text = if seen.insert(tree, Path::new(path)) {
            expand(tree, Path::new(path), cap, &mut seen)
        } else {
            "Already included above.".to_string()
        };
        s.push_str(&format!("\n## Repo guideline: {path}\n\n{}\n", text.trim()));
    }
    s
}

fn canonical(tree: &Path, file: &Path) -> PathBuf {
    let p = tree.join(file);
    p.canonicalize().unwrap_or(p)
}

struct Expansion<'a> {
    root: PathBuf,
    cap: usize,
    out: String,
    /// `out`'s length in characters.
    len: usize,
    /// Files, relative to the tree, whose text was cut or never reached because of the cap.
    unshown: Vec<String>,
    seen: &'a mut Seen,
}

impl Expansion<'_> {
    fn full(&self) -> bool {
        self.len >= self.cap
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
        self.len += s.chars().count();
    }

    /// Pushes `text`, the contents of `path` at import depth `depth`, resolving its imports.
    fn push_file(&mut self, text: &str, path: &Path, depth: usize) {
        let dir = path.parent().unwrap_or(Path::new(""));
        let mut fence: Option<(char, usize)> = None;
        for line in text.split_inclusive('\n') {
            self.push(line);
            if !line.ends_with('\n') {
                self.push("\n");
            }
            if let Some(f) = fence_marker(line) {
                fence = match fence {
                    None => Some(f),
                    Some((c, n))
                        if f.0 == c && f.1 >= n && line.trim()[f.1..].trim().is_empty() =>
                    {
                        None
                    }
                    open => open,
                };
                continue;
            }
            if fence.is_some() {
                continue;
            }
            let Some(target) = import(line) else { continue };
            self.push_import(target, dir, depth);
        }
    }

    fn push_import(&mut self, target: &str, dir: &Path, depth: usize) {
        let Some(path) = self.resolve(target, dir) else {
            self.note(&format!("@{target} not inlined: not a file in the repo"));
            return;
        };
        let rel = self.rel(&path);
        if depth + 1 > MAX_DEPTH {
            self.note(&format!(
                "@{target} not inlined: imports nest more than {MAX_DEPTH} deep"
            ));
            return;
        }
        // A file that doesn't fit stays unseen, so a later guideline can still show it.
        if self.full() {
            if !self.seen.0.contains(&path) && !self.unshown.contains(&rel) {
                self.unshown.push(rel);
            }
            return;
        }
        if !self.seen.0.insert(path.clone()) {
            self.note(&format!("{rel} is already included above"));
            return;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        // Its own imports are listed after it.
        let slot = self.unshown.len();
        self.push(&format!("<!-- lens: begin {rel} -->\n"));
        self.push_file(&text, &path, depth + 1);
        // Cut only if its text is: losing the end marker loses nothing.
        if self.len > self.cap {
            self.seen.0.remove(&path);
            self.unshown.insert(slot, rel.clone());
        }
        self.push(&format!("<!-- lens: end {rel} -->\n"));
    }

    fn note(&mut self, s: &str) {
        if !self.full() {
            self.push(&format!("<!-- lens: {s} -->\n"));
        }
    }

    /// The file `target` names, if it is one inside the tree.
    fn resolve(&self, target: &str, dir: &Path) -> Option<PathBuf> {
        let p = if let Some(rest) = target.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME")?).join(rest)
        } else {
            dir.join(target)
        };
        let p = p.canonicalize().ok()?;
        (p.is_file() && p.starts_with(&self.root)).then_some(p)
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .unwrap_or(p)
            .display()
            .to_string()
    }

    fn finish(self) -> String {
        let text = self.out.trim_end();
        if text.chars().count() <= self.cap {
            return text.to_string();
        }
        let mut s: String = text.chars().take(self.cap).collect();
        s.push_str(&format!("\n\n[lens: cut at {} characters.", self.cap));
        if !self.unshown.is_empty() {
            s.push_str(&format!(" Not shown in full: {}.", self.unshown.join(", ")));
        }
        s.push_str(" Read the files for the rest.]");
        s
    }
}

/// The path a line imports, if it is an import line.
fn import(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let t = ["- ", "* ", "+ "]
        .iter()
        .find_map(|m| t.strip_prefix(m))
        .map_or(t, str::trim_start);
    let word = t.split_whitespace().next()?;
    let path = word.strip_prefix('@')?;
    (!path.is_empty()).then_some(path)
}

/// The fence a line opens or closes: its character and length.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let t = line.trim();
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let p = tmp.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        tmp
    }

    fn run(t: &tempfile::TempDir, file: &str, cap: usize) -> String {
        expand(t.path(), Path::new(file), cap, &mut Seen::default())
    }

    #[test]
    fn a_file_without_imports_is_unchanged() {
        let t = tree(&[(
            "CLAUDE.md",
            "# Rules\n\nWrite tests.\nMail me@example.com.\n",
        )]);
        assert_eq!(
            run(&t, "CLAUDE.md", 1000),
            "# Rules\n\nWrite tests.\nMail me@example.com."
        );
    }

    #[test]
    fn nested_imports_are_inlined_where_imported_relative_to_their_file() {
        let t = tree(&[
            ("CLAUDE.md", "top\n@docs/style.md\nbottom\n"),
            ("docs/style.md", "style\n- @sub/comments.md\n"),
            ("docs/sub/comments.md", "no TODOs"),
        ]);
        assert_eq!(
            run(&t, "CLAUDE.md", 1000),
            "top\n@docs/style.md\n<!-- lens: begin docs/style.md -->\nstyle\n- @sub/comments.md\n\
             <!-- lens: begin docs/sub/comments.md -->\nno TODOs\n\
             <!-- lens: end docs/sub/comments.md -->\n<!-- lens: end docs/style.md -->\nbottom"
        );
    }

    #[test]
    fn a_guideline_in_a_subfolder_imports_relative_to_its_own_folder() {
        let t = tree(&[
            ("app/CLAUDE.md", "@RULES.md\n"),
            ("app/RULES.md", "app rules"),
            ("RULES.md", "root rules"),
        ]);
        let out = run(&t, "app/CLAUDE.md", 1000);
        assert!(out.contains("app rules"), "{out}");
        assert!(!out.contains("root rules"), "{out}");
    }

    #[test]
    fn a_cycle_is_followed_once() {
        let t = tree(&[
            ("CLAUDE.md", "@A.md\n"),
            ("A.md", "a\n@B.md\n"),
            ("B.md", "b\n@A.md\n@CLAUDE.md\n"),
        ]);
        let out = run(&t, "CLAUDE.md", 1000);
        assert_eq!(out.matches("\na\n").count(), 1, "{out}");
        assert_eq!(out.matches("\nb\n").count(), 1, "{out}");
        assert!(
            out.contains("<!-- lens: A.md is already included above -->"),
            "{out}"
        );
        assert!(
            out.contains("<!-- lens: CLAUDE.md is already included above -->"),
            "{out}"
        );
    }

    #[test]
    fn a_file_imported_twice_or_already_shown_by_another_guideline_appears_once() {
        let t = tree(&[
            ("CLAUDE.md", "@AGENTS.md\n@AGENTS.md\n"),
            ("AGENTS.md", "agents"),
            ("app/CLAUDE.md", "@../AGENTS.md\n"),
        ]);
        let mut seen = Seen::default();
        let first = expand(t.path(), Path::new("CLAUDE.md"), 1000, &mut seen);
        assert_eq!(first.matches("agents").count(), 1, "{first}");
        let second = expand(t.path(), Path::new("app/CLAUDE.md"), 1000, &mut seen);
        assert!(
            second.contains("AGENTS.md is already included above"),
            "{second}"
        );
        assert!(!seen.insert(t.path(), Path::new("AGENTS.md")));
    }

    #[test]
    fn imports_stop_past_the_depth_limit() {
        let mut files = vec![("CLAUDE.md".to_string(), "@1.md\n".to_string())];
        for i in 1..=7 {
            files.push((format!("{i}.md"), format!("level {i}\n@{}.md\n", i + 1)));
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let t = tree(&files);
        let out = run(&t, "CLAUDE.md", 10_000);
        assert!(out.contains("level 5"), "{out}");
        assert!(!out.contains("level 6"), "{out}");
        assert!(
            out.contains("<!-- lens: @6.md not inlined: imports nest more than 5 deep -->"),
            "{out}"
        );
    }

    #[test]
    fn a_missing_target_is_left_with_a_note() {
        let t = tree(&[
            ("CLAUDE.md", "@gone.md\n@docs\nafter\n"),
            ("docs/x.md", "x"),
        ]);
        assert_eq!(
            run(&t, "CLAUDE.md", 1000),
            "@gone.md\n<!-- lens: @gone.md not inlined: not a file in the repo -->\n\
             @docs\n<!-- lens: @docs not inlined: not a file in the repo -->\nafter"
        );
    }

    #[test]
    fn imports_inside_code_fences_or_mid_line_are_not_followed() {
        let t = tree(&[
            (
                "CLAUDE.md",
                "````md\n@A.md\n```\n@A.md\n```` \n~~~\n@A.md\n~~~\nsee @A.md or `@A.md`\n",
            ),
            ("A.md", "imported"),
        ]);
        let out = run(&t, "CLAUDE.md", 1000);
        assert!(!out.contains("imported"), "{out}");
        assert!(!out.contains("lens:"), "{out}");
    }

    #[test]
    fn an_import_after_a_closed_fence_is_followed() {
        let t = tree(&[
            ("CLAUDE.md", "```\ncode\n```\n@A.md\n"),
            ("A.md", "imported"),
        ]);
        assert!(run(&t, "CLAUDE.md", 1000).contains("imported"));
    }

    #[test]
    fn paths_escaping_the_tree_are_never_read() {
        let outer = tree(&[("secret.md", "secret"), ("repo/A.md", "inside")]);
        let repo = outer.path().join("repo");
        let secret = outer.path().join("secret.md");
        fs::write(
            repo.join("CLAUDE.md"),
            format!(
                "@../secret.md\n@{}\n@link.md\n@A.md/../../secret.md\n@~/.lens-no-such-file.md\n",
                secret.display()
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(&secret, repo.join("link.md")).unwrap();
        let out = expand(&repo, Path::new("CLAUDE.md"), 1000, &mut Seen::default());
        assert!(!out.contains("\nsecret"), "{out}");
        assert_eq!(out.matches("not a file in the repo").count(), 5, "{out}");
    }

    #[test]
    fn a_nested_import_cannot_escape_the_tree_either() {
        let outer = tree(&[
            ("secret.md", "secret"),
            ("repo/CLAUDE.md", "@docs/a.md\n"),
            ("repo/docs/a.md", "a\n@../../secret.md\n"),
        ]);
        let out = expand(
            &outer.path().join("repo"),
            Path::new("CLAUDE.md"),
            1000,
            &mut Seen::default(),
        );
        assert!(out.contains("\na\n"), "{out}");
        assert!(!out.contains("\nsecret"), "{out}");
        assert!(out.contains("@../../secret.md not inlined"), "{out}");
    }

    #[test]
    fn an_absolute_path_inside_the_tree_is_followed() {
        let t = tree(&[("A.md", "imported")]);
        let abs = t.path().canonicalize().unwrap().join("A.md");
        fs::write(t.path().join("CLAUDE.md"), format!("@{}\n", abs.display())).unwrap();
        let out = run(&t, "CLAUDE.md", 1000);
        assert!(
            out.contains("<!-- lens: begin A.md -->\nimported\n"),
            "{out}"
        );
    }

    #[test]
    fn the_cap_covers_imports_and_names_what_did_not_fit() {
        let t = tree(&[
            ("CLAUDE.md", "@A.md\n@B.md\n@C.md\n"),
            ("A.md", &"a".repeat(50)),
            ("B.md", &"b".repeat(50)),
            ("C.md", "c"),
        ]);
        let out = run(&t, "CLAUDE.md", 100);
        let (body, note) = out.split_once("\n\n[lens:").unwrap();
        assert_eq!(body.chars().count(), 100, "{out}");
        assert!(body.contains(&"a".repeat(50)), "{out}");
        assert!(!body.contains(&"b".repeat(50)), "{out}");
        assert_eq!(
            note,
            " cut at 100 characters. Not shown in full: B.md, C.md. Read the files for the rest.]"
        );
    }

    #[test]
    fn a_file_the_cap_kept_out_is_shown_by_a_later_guideline() {
        let t = tree(&[
            ("CLAUDE.md", "@A.md\n@B.md\n@B.md\n"),
            ("A.md", &"a".repeat(200)),
            ("B.md", "bee"),
            ("app/CLAUDE.md", "@../B.md\n@../A.md\n"),
        ]);
        let paths = ["CLAUDE.md", "app/CLAUDE.md"].map(String::from);
        let out = sections(t.path(), &paths, 100);
        let (first, second) = out.split_once("## Repo guideline: app/CLAUDE.md").unwrap();
        assert!(first.contains("Not shown in full: A.md, B.md."), "{out}");
        assert!(!first.contains("bee"), "{out}");
        assert!(second.contains("<!-- lens: begin B.md -->\nbee\n"), "{out}");
        assert!(second.contains("Not shown in full: A.md."), "{out}");
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let t = tree(&[("CLAUDE.md", &"é".repeat(30))]);
        let out = run(&t, "CLAUDE.md", 10);
        assert!(
            out.starts_with(&format!("{}\n\n[lens: cut at 10", "é".repeat(10))),
            "{out}"
        );
    }

    #[test]
    fn each_guideline_gets_a_section_and_one_already_shown_says_so() {
        let t = tree(&[
            ("CLAUDE.md", "root\n@AGENTS.md\n"),
            ("AGENTS.md", "agents"),
            ("app/AGENTS.md", "app"),
        ]);
        std::os::unix::fs::symlink("AGENTS.md", t.path().join("app/CLAUDE.md")).unwrap();
        let paths = ["CLAUDE.md", "AGENTS.md", "app/CLAUDE.md", "app/AGENTS.md"].map(String::from);
        assert_eq!(
            sections(t.path(), &paths, 1000),
            "\n## Repo guideline: CLAUDE.md\n\nroot\n@AGENTS.md\n<!-- lens: begin AGENTS.md -->\n\
             agents\n<!-- lens: end AGENTS.md -->\n\
             \n## Repo guideline: AGENTS.md\n\nAlready included above.\n\
             \n## Repo guideline: app/CLAUDE.md\n\napp\n\
             \n## Repo guideline: app/AGENTS.md\n\nAlready included above.\n"
        );
    }

    #[test]
    fn import_lines_are_recognised() {
        assert_eq!(import("@a.md"), Some("a.md"));
        assert_eq!(import("  - @docs/a.md rules for x\n"), Some("docs/a.md"));
        assert_eq!(import("* @a.md"), Some("a.md"));
        assert_eq!(import("@"), None);
        assert_eq!(import("see @a.md"), None);
        assert_eq!(import("`@a.md`"), None);
    }
}
