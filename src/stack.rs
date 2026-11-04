//! Stacked PRs: child PRs merged into a PR's head branch. lens reviewed each child on its own
//! PR, so the parent's next PR pass takes their reviewed patches out of its delta and reviews
//! only what's left (docs/REVIEW.md, "Stacked PRs").

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::diff::git;
use crate::forge::Child;

/// A merged child PR whose reviewed patch came out of the parent's delta.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Taken {
    pub(crate) number: u64,
    pub(crate) url: String,
    /// The merge base and head the child's last lens review was for.
    pub(crate) base: String,
    pub(crate) head: String,
    /// The files the child's reviewed patch touches.
    pub(crate) files: Vec<String>,
    /// The child report's findings, their lines moved to the parent's head.
    pub(crate) findings: Vec<Value>,
}

/// What's left of a delta once the reviewed children come out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Leftover {
    pub(crate) patch: String,
    pub(crate) files: Vec<String>,
}

/// The PRs stacked on a PR, as one PR pass sorted them.
#[derive(Debug, Default)]
pub(crate) struct Stack {
    pub(crate) taken: Vec<Taken>,
    /// Merged children whose lines stay in the delta, each with why.
    pub(crate) kept: Vec<(u64, String)>,
    /// Children not merged yet.
    pub(crate) open: Vec<u64>,
    /// The delta with the taken children out; `None` when none came out.
    pub(crate) leftover: Option<Leftover>,
}

/// Takes the merged `children` lens reviewed (a PR report in `state`) out of the delta
/// `from..head`: their reviewed changes are merged onto `from`'s tree, and what still differs
/// from `head` is the leftover. A child merged before `from` is left alone; one with no report,
/// or whose change won't merge, keeps its lines.
pub(crate) fn subtract(
    tree: &Path,
    state: &Path,
    from: &str,
    head: &str,
    children: &[Child],
) -> Stack {
    let mut stack = Stack::default();
    let mut children = children.to_vec();
    children.sort_by_key(|c| c.number);
    let Ok(mut at) = git(tree, &["rev-parse", &format!("{from}^{{tree}}")]) else {
        return stack;
    };
    at = at.trim().to_string();
    for child in &children {
        if !child.merged {
            stack.open.push(child.number);
            continue;
        }
        if merged_before(tree, child.merge_commit.as_deref(), from) {
            continue;
        }
        let Some((base, child_head, findings)) = reviewed(state, child.number) else {
            stack.kept.push((child.number, "no lens review".into()));
            continue;
        };
        if child.merge_commit.is_none() && merged_before(tree, Some(&child_head), from) {
            continue;
        }
        if !present(tree, &base) || !present(tree, &child_head) {
            stack
                .kept
                .push((child.number, "its reviewed commits can't be fetched".into()));
            continue;
        }
        let files = names(tree, &base, &child_head);
        if files.is_empty() {
            continue;
        }
        match merge_onto(tree, &at, &base, &child_head) {
            Ok(next) => at = next,
            Err(_) => {
                stack.kept.push((
                    child.number,
                    "its reviewed change doesn't merge onto the last-reviewed head".into(),
                ));
                continue;
            }
        }
        let mut findings = findings;
        crate::review::move_findings(tree, &child_head, head, &mut findings);
        stack.taken.push(Taken {
            number: child.number,
            url: child.url.clone(),
            base,
            head: child_head,
            files,
            findings,
        });
    }
    if stack.taken.is_empty() {
        return stack;
    }
    if let Ok(patch) = git(tree, &["diff", "--full-index", &at, head]) {
        stack.leftover = Some(Leftover {
            patch,
            files: names(tree, &at, head),
        });
    }
    stack
}

/// The files touched by more than one of the stack's parts, each with who touches it: the taken
/// children (`#n`), the PR's own changes before the review (`parent`) and the leftover.
pub(crate) fn meeting(
    stack: &Stack,
    parent: &[String],
    leftover: &[String],
) -> BTreeMap<String, Vec<String>> {
    let mut who: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |files: &[String], name: String| {
        for f in files {
            let names = who.entry(f.clone()).or_default();
            if !names.contains(&name) {
                names.push(name.clone());
            }
        }
    };
    add(parent, "parent".into());
    for t in &stack.taken {
        add(&t.files, format!("#{}", t.number));
    }
    add(leftover, "leftover".into());
    who.retain(|_, names| names.len() > 1);
    who
}

/// Whether `commit` (a child's merge commit, or its head when the forge doesn't give the merge
/// commit) was already in the head lens last reviewed, `from`.
fn merged_before(tree: &Path, commit: Option<&str>, from: &str) -> bool {
    commit.is_some_and(|c| git(tree, &["merge-base", "--is-ancestor", c, from]).is_ok())
}

/// The merge base, head and findings of child `n`'s last lens PR review in `state`.
fn reviewed(state: &Path, n: u64) -> Option<(String, String, Vec<Value>)> {
    let text = std::fs::read_to_string(state.join(format!("pr-{n}.json"))).ok()?;
    let report: Value = serde_json::from_str(&text).ok()?;
    if report["pass"] != "pr" {
        return None;
    }
    Some((
        report["mergeBase"].as_str()?.to_string(),
        report["head"].as_str()?.to_string(),
        report["findings"].as_array().cloned().unwrap_or_default(),
    ))
}

/// Whether `commit` is in `tree`'s repo, fetching it from origin when it isn't.
fn present(tree: &Path, commit: &str) -> bool {
    let spec = format!("{commit}^{{commit}}");
    if git(tree, &["cat-file", "-e", &spec]).is_ok() {
        return true;
    }
    let _ = git(tree, &["fetch", "--quiet", "origin", commit]);
    git(tree, &["cat-file", "-e", &spec]).is_ok()
}

fn names(tree: &Path, a: &str, b: &str) -> Vec<String> {
    git(tree, &["diff", "--name-only", "--no-renames", a, b])
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// The tree that merging a child's reviewed change `base..head` onto tree `at` makes. Where the
/// two conflict it takes the child's side: the parent's head resolved that conflict, and so its
/// resolution shows in the leftover.
fn merge_onto(tree: &Path, at: &str, base: &str, head: &str) -> Result<String> {
    let out = git(
        tree,
        &[
            "merge-tree",
            "--write-tree",
            "--no-messages",
            "-X",
            "theirs",
            &format!("--merge-base={base}"),
            at,
            head,
        ],
    )?;
    Ok(out.lines().next().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::process::Command;

    /// A repo whose `parent` branch lens last reviewed at `from`, with child branches cut there.
    struct Repo {
        dir: tempfile::TempDir,
        state: tempfile::TempDir,
        from: String,
    }

    impl Repo {
        fn new() -> Repo {
            let dir = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let r = Repo {
                dir,
                state,
                from: String::new(),
            };
            r.git(&["init", "-q", "-b", "main"]);
            r.write("src/a.rs", "a1\na2\na3\n");
            r.write("src/b.rs", "b1\nb2\nb3\n");
            r.write("src/c.rs", "c1\n");
            r.commit("base");
            r.git(&["switch", "-q", "-c", "parent"]);
            r.write("src/a.rs", "a1\na2 parent\na3\n");
            r.commit("parent's own change");
            Repo {
                from: r.rev("HEAD"),
                ..r
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(self.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        }

        fn rev(&self, r: &str) -> String {
            self.git(&["rev-parse", r]).trim().to_string()
        }

        fn write(&self, file: &str, text: &str) {
            let p = self.path().join(file);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }

        fn commit(&self, msg: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", msg]);
        }

        /// Child `name` cut from the parent's reviewed head, writing `file`.
        fn child(&self, name: &str, file: &str, text: &str) -> String {
            self.git(&["switch", "-q", "-c", name, &self.from]);
            self.write(file, text);
            self.commit(name);
            let head = self.rev("HEAD");
            self.git(&["switch", "-q", "parent"]);
            head
        }

        /// lens's report on child PR `n`, which reviewed `head` from the parent's reviewed head.
        fn report(&self, n: u64, head: &str, findings: Value) {
            fs::write(
                self.state.path().join(format!("pr-{n}.json")),
                json!({"pass": "pr", "mergeBase": self.from, "head": head, "findings": findings})
                    .to_string(),
            )
            .unwrap();
        }

        fn squash(&self, branch: &str) -> String {
            self.git(&["merge", "-q", "--squash", branch]);
            self.git(&["commit", "-q", "-m", &format!("{branch} (squashed)")]);
            self.rev("HEAD")
        }

        fn merge(&self, branch: &str) -> String {
            self.git(&[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {branch}"),
                branch,
            ]);
            self.rev("HEAD")
        }

        fn subtract(&self, children: &[Child]) -> Stack {
            subtract(
                self.path(),
                self.state.path(),
                &self.from,
                &self.rev("HEAD"),
                children,
            )
        }
    }

    fn merged(number: u64, merge_commit: &str) -> Child {
        Child {
            number,
            url: format!("u{number}"),
            merged: true,
            merge_commit: Some(merge_commit.into()),
        }
    }

    fn leftover(stack: &Stack) -> &Leftover {
        stack.leftover.as_ref().expect("a leftover")
    }

    #[test]
    fn a_squash_merged_reviewed_child_leaves_nothing() {
        let repo = Repo::new();
        let head = repo.child("c8", "src/b.rs", "b1\nb2 child\nb3\n");
        repo.report(8, &head, json!([]));
        let squash = repo.squash("c8");
        let stack = repo.subtract(&[merged(8, &squash)]);
        assert_eq!(stack.taken.len(), 1);
        assert_eq!(stack.taken[0].files, vec!["src/b.rs"]);
        assert_eq!(stack.taken[0].head, head);
        assert!(stack.kept.is_empty());
        assert_eq!(leftover(&stack).patch, "");
        assert!(leftover(&stack).files.is_empty());
    }

    #[test]
    fn a_merge_commit_child_leaves_nothing() {
        let repo = Repo::new();
        let head = repo.child("c8", "src/b.rs", "b1\nb2 child\nb3\n");
        repo.report(8, &head, json!([]));
        let merge = repo.merge("c8");
        let stack = repo.subtract(&[merged(8, &merge)]);
        assert_eq!(stack.taken.len(), 1);
        assert_eq!(leftover(&stack).patch, "");
    }

    #[test]
    fn a_conflict_resolution_and_a_later_commit_are_left_over() {
        let repo = Repo::new();
        let eight = repo.child("c8", "src/b.rs", "b1\nb2 eight\nb3\n");
        let nine = repo.child("c9", "src/b.rs", "b1\nb2 nine\nb3\n");
        repo.report(8, &eight, json!([]));
        repo.report(9, &nine, json!([]));
        let m8 = repo.merge("c8");
        let conflict = Command::new("git")
            .args(["merge", "-q", "--no-ff", "c9"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(!conflict.success());
        repo.write("src/b.rs", "b1\nb2 eight and nine\nb3\n");
        repo.commit("merge c9, resolved");
        let m9 = repo.rev("HEAD");
        repo.write("src/c.rs", "c1 parent again\n");
        repo.commit("another commit on the parent");
        let stack = repo.subtract(&[merged(9, &m9), merged(8, &m8)]);
        assert_eq!(
            stack.taken.iter().map(|t| t.number).collect::<Vec<_>>(),
            vec![8, 9]
        );
        let left = leftover(&stack);
        assert_eq!(left.files, vec!["src/b.rs", "src/c.rs"]);
        assert!(left.patch.contains("+b2 eight and nine"), "{}", left.patch);
        assert!(left.patch.contains("+c1 parent again"), "{}", left.patch);
        assert_eq!(
            meeting(&stack, &["src/a.rs".into()], &left.files),
            BTreeMap::from([(
                "src/b.rs".to_string(),
                vec!["#8".to_string(), "#9".into(), "leftover".into()]
            )])
        );
    }

    #[test]
    fn a_child_with_no_lens_review_keeps_its_lines() {
        let repo = Repo::new();
        let eight = repo.child("c8", "src/b.rs", "b1\nb2 eight\nb3\n");
        repo.child("c9", "src/c.rs", "c1 nine\n");
        repo.report(8, &eight, json!([]));
        let m8 = repo.merge("c8");
        let m9 = repo.merge("c9");
        let stack = repo.subtract(&[merged(8, &m8), merged(9, &m9)]);
        assert_eq!(stack.taken.len(), 1);
        assert_eq!(stack.kept, vec![(9, "no lens review".to_string())]);
        let left = leftover(&stack);
        assert_eq!(left.files, vec!["src/c.rs"]);
        assert!(left.patch.contains("+c1 nine"));
    }

    #[test]
    fn a_child_reviewed_only_up_to_an_earlier_commit_leaves_its_later_ones() {
        let repo = Repo::new();
        let reviewed = repo.child("c8", "src/b.rs", "b1\nb2 eight\nb3\n");
        repo.git(&["switch", "-q", "c8"]);
        repo.write("src/c.rs", "c1 unreviewed\n");
        repo.commit("after the review");
        repo.git(&["switch", "-q", "parent"]);
        repo.report(8, &reviewed, json!([]));
        let squash = repo.squash("c8");
        let stack = repo.subtract(&[merged(8, &squash)]);
        assert_eq!(leftover(&stack).files, vec!["src/c.rs"]);
    }

    #[test]
    fn open_children_and_ones_merged_before_the_review_change_nothing() {
        let repo = Repo::new();
        let early = repo.child("c7", "src/c.rs", "c1 seven\n");
        repo.report(7, &early, json!([]));
        let m7 = repo.merge("c7");
        let repo = Repo {
            from: m7.clone(),
            ..repo
        };
        repo.write("src/a.rs", "a1\na2\na3 later\n");
        repo.commit("a later commit");
        let open = Child {
            number: 10,
            url: "u10".into(),
            merged: false,
            merge_commit: None,
        };
        let stack = repo.subtract(&[merged(7, &m7), open]);
        assert!(stack.taken.is_empty());
        assert!(stack.kept.is_empty());
        assert_eq!(stack.open, vec![10]);
        assert_eq!(stack.leftover, None);
    }

    #[test]
    fn no_children_leave_the_delta_as_it_is() {
        let repo = Repo::new();
        repo.write("src/c.rs", "c1 more\n");
        repo.commit("more");
        let stack = repo.subtract(&[]);
        assert!(stack.taken.is_empty() && stack.kept.is_empty() && stack.open.is_empty());
        assert_eq!(stack.leftover, None);
    }

    #[test]
    fn a_child_change_that_wont_merge_keeps_its_lines() {
        let repo = Repo::new();
        repo.git(&["rm", "-q", "src/c.rs"]);
        repo.commit("the parent drops c.rs");
        let repo = Repo {
            from: repo.rev("HEAD"),
            ..repo
        };
        repo.git(&["switch", "-q", "-c", "c8", "main"]);
        repo.write("src/c.rs", "c1 child\n");
        repo.commit("c8");
        let head = repo.rev("HEAD");
        repo.git(&["switch", "-q", "parent"]);
        fs::write(
            repo.state.path().join("pr-8.json"),
            json!({"pass": "pr", "mergeBase": repo.rev("main"), "head": head}).to_string(),
        )
        .unwrap();
        let conflict = Command::new("git")
            .args(["merge", "-q", "--no-ff", "c8"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(!conflict.success());
        repo.git(&["rm", "-q", "src/c.rs"]);
        repo.commit("merge c8, keeping c.rs dropped");
        let stack = repo.subtract(&[merged(8, &repo.rev("HEAD"))]);
        assert!(stack.taken.is_empty());
        assert_eq!(stack.kept.len(), 1, "{:?}", stack.kept);
        assert_eq!(stack.leftover, None);
    }

    #[test]
    fn child_findings_move_to_the_parent_head() {
        let repo = Repo::new();
        let head = repo.child("c8", "src/b.rs", "b1\nb2 child\nb3\n");
        repo.report(
            8,
            &head,
            json!([{"id": "F1", "file": "src/b.rs", "line": 3, "title": "x"}]),
        );
        repo.write("src/b.rs", "b0\nb1\nb2\nb3\n");
        repo.commit("a line above");
        let m = repo.merge("c8");
        let stack = repo.subtract(&[merged(8, &m)]);
        assert_eq!(stack.taken[0].findings[0]["line"], 4);
    }
}
