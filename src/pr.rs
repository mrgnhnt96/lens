//! The PR pass's GitHub side: reading the comments earlier lens runs left on a PR, and posting a
//! review's verified findings as one review with inline comments.
//!
//! Every comment lens posts ends in a marker, `<!-- lens:finding key=<key> -->`, and the review
//! body in `<!-- lens:review head=<sha> -->`. The key is a hash of the file and the normalized
//! title, so a later run can tell which findings it already posted.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{PostAs, PrConfig};

/// A finding within this many lines of an earlier lens comment on the same file counts as the
/// same finding.
pub const NEAR_LINES: u32 = 3;

const FINDING_MARK: &str = "<!-- lens:finding key=";
const REVIEW_MARK: &str = "<!-- lens:review";

/// The parts of `.review/pr-<n>.json` posting needs (the format is in docs/REVIEW.md).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub head: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub assessment: Option<String>,
    #[serde(default)]
    pub profiles: Vec<ProfileRun>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub dropped: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ProfileRun {
    pub name: String,
    #[serde(default)]
    pub ran: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    pub file: String,
    pub line: u32,
    pub severity: String,
    pub title: String,
    #[serde(default)]
    pub failure_scenario: String,
    #[serde(default)]
    pub how_it_fails: Option<String>,
}

impl Finding {
    pub fn key(&self) -> String {
        key(&self.file, &self.title)
    }

    fn collapsed(&self) -> bool {
        matches!(self.severity.as_str(), "minor" | "nit")
    }
}

/// A finding an earlier lens run left on the PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Posted {
    pub key: String,
    /// `None` for a finding listed in a review body rather than commented inline.
    pub path: Option<String>,
    /// The comment's line on the PR's current head; `None` once GitHub marks it outdated.
    pub line: Option<u32>,
    pub url: Option<String>,
}

/// Everything earlier lens runs posted on a PR.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Earlier {
    pub reviews: usize,
    pub findings: Vec<Posted>,
}

/// `gh`, or `$LENS_GH`, run in the repo with the identity `.lens.toml` picks.
pub struct Gh {
    program: PathBuf,
    repo: PathBuf,
    token: Option<String>,
}

impl Gh {
    pub fn new(repo: &Path, config: &PrConfig) -> Result<Gh> {
        let token = match config.post_as {
            PostAs::Owner => None,
            PostAs::Bot => {
                let var = config.bot_token_env();
                match std::env::var(var) {
                    Ok(t) if !t.is_empty() => Some(t),
                    _ => bail!(
                        ".lens.toml posts as a bot, but ${var} holds no token; set it or use post_as = \"owner\""
                    ),
                }
            }
        };
        Ok(Gh {
            program: std::env::var_os("LENS_GH")
                .map(PathBuf::from)
                .unwrap_or_else(|| "gh".into()),
            repo: repo.to_path_buf(),
            token,
        })
    }

    fn api(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        self.run(&[&["api"], args].concat(), stdin)
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .current_dir(&self.repo)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = &self.token {
            cmd.env("GH_TOKEN", token);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {}", self.program.display()))?;
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "gh {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        String::from_utf8(out.stdout).context("gh printed non-UTF-8")
    }

    /// Opens a PR from `head` into `base` and returns its URL.
    pub fn create_pr(&self, base: &str, head: &str, title: &str, body: &Path) -> Result<String> {
        let body = body.display().to_string();
        let out = self.run(
            &[
                "pr",
                "create",
                "--base",
                base,
                "--head",
                head,
                "--title",
                title,
                "--body-file",
                &body,
            ],
            None,
        )?;
        Ok(out.trim().lines().last().unwrap_or("").to_string())
    }

    /// Every page of a list endpoint.
    fn list(&self, endpoint: &str) -> Result<Vec<Value>> {
        let text = self.api(&["--paginate", endpoint], None)?;
        // --paginate prints one JSON array per page, back to back.
        let mut all = Vec::new();
        for page in serde_json::Deserializer::from_str(&text).into_iter::<Vec<Value>>() {
            all.extend(page.with_context(|| format!("parsing gh api {endpoint}"))?);
        }
        Ok(all)
    }

    /// The findings earlier lens runs posted on PR `pr`, inline or in a review body.
    pub fn earlier(&self, pr: u64) -> Result<Earlier> {
        let comments = self.list(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}/comments"))?;
        let reviews = self.list(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews"))?;
        Ok(earlier_from(&comments, &reviews))
    }

    /// Posts `review` (from [`plan`]) on PR `pr` and returns the review's URL.
    pub fn post(&self, pr: u64, review: &Value) -> Result<Option<String>> {
        let out = self.api(
            &[
                "--method",
                "POST",
                &format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews"),
                "--input",
                "-",
            ],
            Some(&review.to_string()),
        )?;
        let v: Value = serde_json::from_str(&out).context("parsing the posted review")?;
        Ok(v["html_url"].as_str().map(str::to_string))
    }
}

pub fn earlier_from(comments: &[Value], reviews: &[Value]) -> Earlier {
    let mut out = Earlier::default();
    for c in comments {
        let body = c["body"].as_str().unwrap_or("");
        for key in keys_in(body) {
            out.findings.push(Posted {
                key,
                path: c["path"].as_str().map(str::to_string),
                line: c["line"].as_u64().map(|l| l as u32),
                url: c["html_url"].as_str().map(str::to_string),
            });
        }
    }
    for r in reviews {
        let body = r["body"].as_str().unwrap_or("");
        if !body.contains(REVIEW_MARK) {
            continue;
        }
        out.reviews += 1;
        for key in keys_in(body) {
            out.findings.push(Posted {
                key,
                path: None,
                line: None,
                url: r["html_url"].as_str().map(str::to_string),
            });
        }
    }
    out
}

fn keys_in(body: &str) -> Vec<String> {
    body.match_indices(FINDING_MARK)
        .filter_map(|(i, _)| {
            let rest = &body[i + FINDING_MARK.len()..];
            let key: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            (!key.is_empty()).then_some(key)
        })
        .collect()
}

/// The finding's identity across runs: FNV-1a over the file and the title, lowercased with
/// everything but letters and digits removed.
pub fn key(file: &str, title: &str) -> String {
    let norm: String = title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in file.bytes().chain([0]).chain(norm.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The lines of each file a PR review can comment on: the new side of every hunk (added and
/// context lines).
pub fn commentable(patch: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut file: Option<String> = None;
    let mut line = 0u32;
    let mut in_hunk = false;
    for l in patch.lines() {
        if l.starts_with("diff ") {
            in_hunk = false;
            file = None;
        } else if !in_hunk && l.starts_with("+++ ") {
            let p = &l[4..];
            file = p.strip_prefix("b/").map(str::to_string);
        } else if let Some(rest) = l.strip_prefix("@@ ") {
            in_hunk = true;
            // @@ -a,b +c,d @@
            line = rest
                .split_whitespace()
                .find_map(|t| t.strip_prefix('+'))
                .and_then(|t| t.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if in_hunk {
            match l.as_bytes().first() {
                Some(b'+') | Some(b' ') => {
                    if let Some(f) = &file {
                        out.entry(f.clone()).or_default().insert(line);
                    }
                    line += 1;
                }
                Some(b'-') | Some(b'\\') => {}
                // An empty context line some tools write without its leading space.
                None => {
                    if let Some(f) = &file {
                        out.entry(f.clone()).or_default().insert(line);
                    }
                    line += 1;
                }
                _ => in_hunk = false,
            }
        }
    }
    out
}

/// Why a finding isn't posted again.
#[derive(Debug, Serialize)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

/// What a run will post.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Findings commented inline, by id.
    pub inline: Vec<String>,
    /// Findings on lines outside the diff, listed in the review body instead.
    pub in_body: Vec<String>,
    pub skipped: Vec<Skipped>,
    /// The review to post; `None` when there's nothing new to raise: lens comments only on what
    /// needs changing.
    pub review: Option<Value>,
}

fn already_posted(f: &Finding, key: &str, earlier: &[Posted]) -> Option<String> {
    earlier.iter().find_map(|p| {
        let same_key = p.key == key;
        let near = p.path.as_deref() == Some(f.file.as_str())
            && p.line.is_some_and(|l| l.abs_diff(f.line) <= NEAR_LINES);
        (same_key || near).then(|| match &p.url {
            Some(url) => format!("already posted: {url}"),
            None => "already posted".to_string(),
        })
    })
}

pub fn plan(report: &Report, earlier: &Earlier, lines: &BTreeMap<String, BTreeSet<u32>>) -> Plan {
    let mut inline = Vec::new();
    let mut in_body = Vec::new();
    let mut skipped = Vec::new();
    let mut comments = Vec::new();
    let mut body_findings = Vec::new();
    let mut seen = Vec::new();
    for f in &report.findings {
        let key = f.key();
        if let Some(reason) = already_posted(f, &key, &earlier.findings) {
            skipped.push(Skipped {
                id: f.id.clone(),
                reason,
            });
            continue;
        }
        if seen.contains(&key) {
            skipped.push(Skipped {
                id: f.id.clone(),
                reason: "duplicate in this run".into(),
            });
            continue;
        }
        seen.push(key.clone());
        if lines.get(&f.file).is_some_and(|ls| ls.contains(&f.line)) {
            comments.push(json!({
                "path": f.file,
                "line": f.line,
                "side": "RIGHT",
                "body": comment_body(f, &key),
            }));
            inline.push(f.id.clone());
        } else {
            body_findings.push((f, key));
            in_body.push(f.id.clone());
        }
    }
    let new = !inline.is_empty() || !in_body.is_empty();
    let review = new.then(|| {
        json!({
            "commit_id": report.head,
            "event": "COMMENT",
            "body": review_body(report, &body_findings, skipped.len()),
            "comments": comments,
        })
    });
    Plan {
        inline,
        in_body,
        skipped,
        review,
    }
}

fn by(f: &Finding) -> String {
    if f.profiles.is_empty() {
        String::new()
    } else {
        format!(" · _{}_", f.profiles.join(", "))
    }
}

pub fn comment_body(f: &Finding, key: &str) -> String {
    let mut details = String::new();
    if !f.failure_scenario.is_empty() {
        details.push_str(&f.failure_scenario);
        details.push_str("\n\n");
    }
    let how = f.how_it_fails.as_deref().unwrap_or("").trim();
    let mark = format!("{FINDING_MARK}{key} -->");
    if f.collapsed() {
        if !how.is_empty() {
            details.push_str(&format!("How it fails:\n\n{how}\n\n"));
        }
        format!(
            "<details><summary><b>{}</b> · {}{}</summary>\n\n{}</details>\n\n{mark}",
            f.severity,
            f.title,
            if f.profiles.is_empty() {
                String::new()
            } else {
                format!(" · <i>{}</i>", f.profiles.join(", "))
            },
            details
        )
    } else {
        let mut s = format!("**{}** · {}{}\n\n{details}", f.severity, f.title, by(f));
        if !how.is_empty() {
            s.push_str(&format!(
                "<details><summary>How it fails</summary>\n\n{how}\n\n</details>\n\n"
            ));
        }
        s.push_str(&mark);
        s
    }
}

fn counts(findings: &[&Finding]) -> String {
    let mut parts = Vec::new();
    for sev in ["blocker", "major", "minor", "nit"] {
        let n = findings.iter().filter(|f| f.severity == sev).count();
        if n > 0 {
            parts.push(format!("{n} {sev}"));
        }
    }
    parts.join(", ")
}

pub fn review_body(report: &Report, outside: &[(&Finding, String)], skipped: usize) -> String {
    let posted: Vec<&Finding> = report.findings.iter().collect();
    let n = posted.len();
    let mut s = format!(
        "### lens review · {n} finding{}",
        if n == 1 { "" } else { "s" }
    );
    if n > 0 {
        s.push_str(&format!(" ({})", counts(&posted)));
    }
    if let Some(risk) = &report.risk {
        s.push_str(&format!(" · risk {risk}"));
    }
    s.push_str("\n\n");
    let short = &report.head[..report.head.len().min(7)];
    if report.base.is_empty() {
        s.push_str(&format!("Reviewed `{short}`."));
    } else {
        s.push_str(&format!("Reviewed `{short}` against `{}`.", report.base));
    }
    s.push_str(" Every finding was confirmed by a verifier that traced the failure; minor and nit findings are collapsed.\n\n");
    if skipped > 0 {
        s.push_str(&format!(
            "{skipped} of them already posted on an earlier run, not repeated.\n\n"
        ));
    }
    if !outside.is_empty() {
        s.push_str("Findings on lines outside the diff:\n\n");
        for (f, key) in outside {
            s.push_str(&format!(
                "- **{}** `{}:{}` {}{}: {} {FINDING_MARK}{key} -->\n",
                f.severity,
                f.file,
                f.line,
                f.title,
                by(f),
                f.failure_scenario
            ));
        }
        s.push('\n');
    }
    if let Some(a) = report
        .assessment
        .as_deref()
        .filter(|a| !a.trim().is_empty())
    {
        s.push_str(&format!(
            "<details><summary>Impact assessment</summary>\n\n{}\n\n</details>\n\n",
            a.trim()
        ));
    }
    let ran: Vec<&str> = report
        .profiles
        .iter()
        .filter(|p| p.ran)
        .map(|p| p.name.as_str())
        .collect();
    let skipped_profiles: Vec<String> = report
        .profiles
        .iter()
        .filter(|p| !p.ran)
        .map(|p| match &p.reason {
            Some(r) => format!("{} ({r})", p.name),
            None => p.name.clone(),
        })
        .collect();
    if !ran.is_empty() {
        s.push_str(&format!("Ran: {}.", ran.join(", ")));
        if !skipped_profiles.is_empty() {
            s.push_str(&format!(" Skipped: {}.", skipped_profiles.join(", ")));
        }
        s.push(' ');
    }
    if !report.dropped.is_empty() {
        s.push_str(&format!(
            "Dropped {} the verifier couldn't show failing.",
            report.dropped.len()
        ));
    }
    s.push_str(&format!("\n\n{REVIEW_MARK} head={} -->", report.head));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str, file: &str, line: u32, severity: &str, title: &str) -> Finding {
        Finding {
            id: id.into(),
            profiles: vec!["correctness".into()],
            file: file.into(),
            line,
            severity: severity.into(),
            title: title.into(),
            failure_scenario: "x happens".into(),
            how_it_fails: Some("1. step".into()),
        }
    }

    fn report(findings: Vec<Finding>) -> Report {
        Report {
            head: "abcdef1234".into(),
            base: "origin/main".into(),
            risk: Some("low".into()),
            assessment: None,
            profiles: vec![],
            findings,
            dropped: vec![],
        }
    }

    const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 fn a() {
-    old();
+    new();
+    more();
 }
@@ -20,2 +21,2 @@ fn b() {
 x
+y
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";

    #[test]
    fn commentable_lines_are_the_new_side_of_each_hunk() {
        let lines = commentable(PATCH);
        assert_eq!(
            lines["src/a.rs"].iter().copied().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 21, 22]
        );
        assert!(!lines.contains_key("gone.rs"));
    }

    #[test]
    fn key_ignores_case_and_punctuation_but_not_the_file() {
        assert_eq!(
            key("a.rs", "Empty input, parses!"),
            key("a.rs", "empty input parses")
        );
        assert_ne!(
            key("a.rs", "empty input parses"),
            key("b.rs", "empty input parses")
        );
    }

    #[test]
    fn earlier_reads_markers_from_comments_and_lens_reviews() {
        let k = key("src/a.rs", "t");
        let comments = vec![
            json!({"path": "src/a.rs", "line": 3, "body": format!("x\n\n{FINDING_MARK}{k} -->"), "html_url": "u1"}),
            json!({"path": "src/a.rs", "line": null, "body": "a human comment"}),
        ];
        let reviews = vec![
            json!({"body": format!("- y {FINDING_MARK}ffff -->\n\n{REVIEW_MARK} head=1 -->"), "html_url": "u2"}),
            json!({"body": format!("someone quoting {FINDING_MARK}eeee -->")}),
        ];
        let e = earlier_from(&comments, &reviews);
        assert_eq!(e.reviews, 1);
        let keys: Vec<&str> = e.findings.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec![k.as_str(), "ffff"]);
        assert_eq!(e.findings[0].line, Some(3));
    }

    #[test]
    fn plan_posts_inline_in_the_diff_and_in_the_body_outside_it() {
        let r = report(vec![
            finding("F1", "src/a.rs", 2, "blocker", "breaks"),
            finding("F2", "src/a.rs", 40, "major", "far away"),
            finding("F3", "src/a.rs", 22, "nit", "tiny"),
        ]);
        let p = plan(&r, &Earlier::default(), &commentable(PATCH));
        assert_eq!(p.inline, vec!["F1", "F3"]);
        assert_eq!(p.in_body, vec!["F2"]);
        let review = p.review.unwrap();
        assert_eq!(review["commit_id"], "abcdef1234");
        assert_eq!(review["event"], "COMMENT");
        assert_eq!(review["comments"][0]["line"], 2);
        assert_eq!(review["comments"][0]["side"], "RIGHT");
        let body = review["body"].as_str().unwrap();
        assert!(
            body.contains("3 findings (1 blocker, 1 major, 1 nit)"),
            "{body}"
        );
        assert!(body.contains(&key("src/a.rs", "far away")));
        assert!(body.contains("<!-- lens:review head=abcdef1234 -->"));
        let nit = review["comments"][1]["body"].as_str().unwrap();
        assert!(
            nit.starts_with("<details><summary><b>nit</b> · tiny"),
            "{nit}"
        );
    }

    #[test]
    fn plan_skips_what_an_earlier_run_posted() {
        let r = report(vec![
            finding("F1", "src/a.rs", 2, "blocker", "breaks"),
            finding("F2", "src/a.rs", 21, "major", "reworded since last time"),
            finding("F3", "src/a.rs", 3, "major", "different spot, same key? no"),
        ]);
        let earlier = Earlier {
            reviews: 1,
            findings: vec![
                Posted {
                    key: key("src/a.rs", "BREAKS"),
                    path: None,
                    line: None,
                    url: Some("u".into()),
                },
                Posted {
                    key: "other".into(),
                    path: Some("src/a.rs".into()),
                    line: Some(23),
                    url: None,
                },
            ],
        };
        let p = plan(&r, &earlier, &commentable(PATCH));
        let skipped: Vec<&str> = p.skipped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(skipped, vec!["F1", "F2"]);
        assert_eq!(p.skipped[0].reason, "already posted: u");
        assert_eq!(p.inline, vec!["F3"]);
    }

    #[test]
    fn nothing_new_posts_nothing() {
        let r = report(vec![]);
        assert!(plan(&r, &Earlier::default(), &BTreeMap::new())
            .review
            .is_none());
        let r = report(vec![finding("F1", "src/a.rs", 2, "major", "breaks")]);
        let earlier = Earlier {
            reviews: 1,
            findings: vec![Posted {
                key: key("src/a.rs", "breaks"),
                path: Some("src/a.rs".into()),
                line: Some(2),
                url: None,
            }],
        };
        assert!(plan(&r, &earlier, &commentable(PATCH)).review.is_none());
    }

    #[test]
    fn blocker_comments_lead_with_severity_and_hide_the_trace() {
        let f = finding("F1", "a.rs", 1, "blocker", "breaks");
        let b = comment_body(&f, "k");
        assert!(
            b.starts_with("**blocker** · breaks · _correctness_\n\nx happens"),
            "{b}"
        );
        assert!(b.contains("<details><summary>How it fails</summary>\n\n1. step"));
        assert!(b.ends_with("<!-- lens:finding key=k -->"));
    }
}
