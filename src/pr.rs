//! The PR pass's GitHub side: reading the comments earlier lens runs left on a PR, and posting a
//! review's verified blocker and major findings as one review. `[pr] comments` picks where they
//! go: all in the review's post (`single`, the default) or inline on the lines the diff shows.
//! Minor and nit findings are never posted; the review says how many it left out.
//!
//! A review is never just comments: it requests changes while a blocker or major is unfixed and
//! approves otherwise.
//!
//! Every finding lens posts ends in a marker, `<!-- lens:finding key=<key> -->`, and the review
//! body in `<!-- lens:review head=<sha> verdict=<event> -->`. The key is a hash of the file and the normalized
//! title, so a later run can tell which findings it already posted.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use sha2::{Digest, Sha256};

use crate::config::{Comments, PostAs, PrConfig};

/// A finding within this many lines of an earlier lens comment on the same file counts as the
/// same finding.
pub const NEAR_LINES: u32 = 3;

const FINDING_MARK: &str = "<!-- lens:finding key=";
const REVIEW_MARK: &str = "<!-- lens:review";

/// The parts of a PR pass's report (`<state>/pr-<n>.json`) posting needs (the format is in docs/REVIEW.md).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    #[serde(default)]
    pub pr: Option<PrRef>,
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

#[derive(Debug, Default, Deserialize)]
pub struct PrRef {
    #[serde(default)]
    pub url: Option<String>,
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
    #[serde(default)]
    pub fix: Option<Value>,
}

impl Finding {
    pub fn key(&self) -> String {
        key(&self.file, &self.title)
    }

    /// A minor or nit: never posted, and never blocks.
    fn left_out(&self) -> bool {
        matches!(self.severity.as_str(), "minor" | "nit")
    }

    /// A blocker or major the fix loop hasn't fixed: the PR can't merge with it.
    fn blocking(&self) -> bool {
        !self.left_out() && self.fix.as_ref().is_none_or(|f| f["status"] != "resolved")
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
    /// The verdict the latest lens review gave, `APPROVE` or `REQUEST_CHANGES`, from its marker.
    pub verdict: Option<String>,
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
    ///
    /// GitHub won't let anyone approve or request changes on their own PR, so posting as its
    /// author falls back to a plain review: the findings still land, and an approval with none
    /// posts nothing.
    pub fn post(&self, pr: u64, review: &Value) -> Result<Option<String>> {
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews");
        let send = |review: &Value| {
            self.api(
                &["--method", "POST", &endpoint, "--input", "-"],
                Some(&review.to_string()),
            )
        };
        let out = match send(review) {
            Err(e) if format!("{e:#}").contains("your own pull request") => {
                if review["event"] == "APPROVE"
                    && review["comments"].as_array().is_none_or(Vec::is_empty)
                    && !review["body"].as_str().unwrap_or("").contains(FINDING_MARK)
                {
                    return Ok(None);
                }
                let mut plain = review.clone();
                plain["event"] = "COMMENT".into();
                send(&plain)?
            }
            out => out?,
        };
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
        out.verdict = verdict_in(body);
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

fn verdict_in(body: &str) -> Option<String> {
    let mark = &body[body.rfind(REVIEW_MARK)?..];
    let rest = &mark[mark.find(" verdict=")? + " verdict=".len()..];
    let v: String = rest
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
        .collect();
    (!v.is_empty()).then_some(v)
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
    /// Findings commented inline, by id; always empty with `[pr] comments = "single"`.
    pub inline: Vec<String>,
    /// Findings listed in the review body: every new one in single mode, those on lines outside
    /// the diff in inline mode.
    pub in_body: Vec<String>,
    pub skipped: Vec<Skipped>,
    /// Minor and nit findings, by id: counted in the review, never posted.
    pub left_out: Vec<String>,
    /// `REQUEST_CHANGES` while a blocker or major is unfixed, else `APPROVE`.
    pub event: &'static str,
    /// The review to post; `None` when there's nothing new to raise and an earlier lens review
    /// already gave this verdict.
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

/// A finding the review body lists, with whether the diff shows its line.
pub struct InBody<'a> {
    pub finding: &'a Finding,
    pub key: String,
    pub on_diff: bool,
}

pub fn plan(
    report: &Report,
    earlier: &Earlier,
    lines: &BTreeMap<String, BTreeSet<u32>>,
    mode: Comments,
) -> Plan {
    let mut inline = Vec::new();
    let mut in_body = Vec::new();
    let mut skipped = Vec::new();
    let mut left_out = Vec::new();
    let mut comments = Vec::new();
    let mut body_findings = Vec::new();
    let mut seen = Vec::new();
    for f in &report.findings {
        if f.left_out() {
            left_out.push(f.id.clone());
            continue;
        }
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
        let on_diff = lines.get(&f.file).is_some_and(|ls| ls.contains(&f.line));
        if on_diff && mode == Comments::Inline {
            comments.push(json!({
                "path": f.file,
                "line": f.line,
                "side": "RIGHT",
                "body": comment_body(f, &key),
            }));
            inline.push(f.id.clone());
        } else {
            body_findings.push(InBody {
                finding: f,
                key,
                on_diff,
            });
            in_body.push(f.id.clone());
        }
    }
    let new = !inline.is_empty() || !in_body.is_empty();
    let event = if report.findings.iter().any(Finding::blocking) {
        "REQUEST_CHANGES"
    } else {
        "APPROVE"
    };
    let changed = earlier.verdict.as_deref() != Some(event);
    let review = (new || changed).then(|| {
        // An approval with nothing new to raise says nothing: the marker alone renders empty.
        let body = if new || event == "REQUEST_CHANGES" || !left_out.is_empty() {
            review_body(
                report,
                &BodyParts {
                    findings: &body_findings,
                    skipped: skipped.len(),
                    left_out: left_out.len(),
                    mode,
                },
                event,
            )
        } else {
            review_mark(report, event)
        };
        json!({
            "commit_id": report.head,
            "event": event,
            "body": body,
            "comments": comments,
        })
    });
    Plan {
        inline,
        in_body,
        skipped,
        left_out,
        event,
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
    finding_text(f, key, None)
}

/// A finding as lens posts it: severity and title, where it is when that isn't the comment's own
/// line, the failure scenario, how it fails folded away, and its marker.
fn finding_text(f: &Finding, key: &str, location: Option<&str>) -> String {
    let mut s = format!("**{}** · {}{}\n\n", f.severity, f.title, by(f));
    if let Some(at) = location {
        s.push_str(&format!("{at}\n\n"));
    }
    if !f.failure_scenario.is_empty() {
        s.push_str(&f.failure_scenario);
        s.push_str("\n\n");
    }
    let how = f.how_it_fails.as_deref().unwrap_or("").trim();
    if !how.is_empty() {
        s.push_str(&format!(
            "<details><summary>How it fails</summary>\n\n{how}\n\n</details>\n\n"
        ));
    }
    s.push_str(&format!("{FINDING_MARK}{key} -->"));
    s
}

/// `file:line`, linked to the line in the PR's diff when the diff shows it, else to the file at
/// the reviewed commit. Unlinked when the report doesn't name the PR's URL.
fn location(report: &Report, f: &Finding, on_diff: bool) -> String {
    let text = format!("`{}:{}`", f.file, f.line);
    let Some(pr) = report
        .pr
        .as_ref()
        .and_then(|p| p.url.as_deref())
        .map(|u| u.trim_end_matches('/'))
    else {
        return text;
    };
    let url = if on_diff {
        let anchor = hex(&Sha256::digest(f.file.as_bytes()));
        Some(format!("{pr}/files#diff-{anchor}R{}", f.line))
    } else {
        pr.rfind("/pull/")
            .map(|i| format!("{}/blob/{}/{}#L{}", &pr[..i], report.head, f.file, f.line))
    };
    match url {
        Some(url) => format!("[{text}]({url})"),
        None => text,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

/// What a review body lists besides the report's own summary.
pub struct BodyParts<'a> {
    pub findings: &'a [InBody<'a>],
    /// New findings an earlier run already posted.
    pub skipped: usize,
    /// Minor and nit findings, counted but not posted.
    pub left_out: usize,
    pub mode: Comments,
}

pub fn review_body(report: &Report, parts: &BodyParts, event: &str) -> String {
    let posted: Vec<&Finding> = report.findings.iter().filter(|f| !f.left_out()).collect();
    let n = posted.len();
    let mut s = format!(
        "### lens review · {n} finding{}",
        if n == 1 { "" } else { "s" }
    );
    if n > 0 {
        s.push_str(&format!(" ({})", counts(&posted)));
    }
    if parts.left_out > 0 {
        s.push_str(&format!(" · {} minor/nit left out", parts.left_out));
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
    s.push_str(" Every finding was confirmed by a verifier that traced the failure; only blockers and majors are posted.\n\n");
    if parts.skipped > 0 {
        s.push_str(&format!(
            "{} of them already posted on an earlier run, not repeated.\n\n",
            parts.skipped
        ));
    }
    if !parts.findings.is_empty() {
        s.push_str(match parts.mode {
            Comments::Single => "Findings:\n\n",
            Comments::Inline => "Findings on lines outside the diff:\n\n",
        });
        for b in parts.findings {
            let at = location(report, b.finding, b.on_diff);
            s.push_str(&finding_text(b.finding, &b.key, Some(&at)));
            s.push_str("\n\n");
        }
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
    s.push_str(&format!("\n\n{}", review_mark(report, event)));
    s
}

fn review_mark(report: &Report, event: &str) -> String {
    format!("{REVIEW_MARK} head={} verdict={event} -->", report.head)
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
            fix: None,
        }
    }

    fn report(findings: Vec<Finding>) -> Report {
        Report {
            pr: Some(PrRef {
                url: Some(PR_URL.into()),
            }),
            head: "abcdef1234".into(),
            base: "origin/main".into(),
            risk: Some("low".into()),
            assessment: None,
            profiles: vec![],
            findings,
            dropped: vec![],
        }
    }

    const PR_URL: &str = "https://github.com/o/r/pull/7";

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
            json!({"body": format!("- y {FINDING_MARK}ffff -->\n\n{REVIEW_MARK} head=1 verdict=REQUEST_CHANGES -->"), "html_url": "u2"}),
            json!({"body": format!("someone quoting {FINDING_MARK}eeee -->")}),
        ];
        let e = earlier_from(&comments, &reviews);
        assert_eq!(e.reviews, 1);
        assert_eq!(e.verdict.as_deref(), Some("REQUEST_CHANGES"));
        let keys: Vec<&str> = e.findings.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec![k.as_str(), "ffff"]);
        assert_eq!(e.findings[0].line, Some(3));
    }

    fn sample() -> Report {
        report(vec![
            finding("F1", "src/a.rs", 2, "blocker", "breaks"),
            finding("F2", "src/a.rs", 40, "major", "far away"),
            finding("F3", "src/a.rs", 22, "nit", "tiny"),
            finding("F4", "src/a.rs", 3, "minor", "edge"),
        ])
    }

    #[test]
    fn single_puts_every_finding_in_the_one_post_linked_to_its_line() {
        let p = plan(
            &sample(),
            &Earlier::default(),
            &commentable(PATCH),
            Comments::Single,
        );
        assert!(p.inline.is_empty());
        assert_eq!(p.in_body, vec!["F1", "F2"]);
        assert_eq!(p.left_out, vec!["F3", "F4"]);
        let review = p.review.unwrap();
        assert_eq!(review["commit_id"], "abcdef1234");
        assert_eq!(review["event"], "REQUEST_CHANGES");
        assert!(review["comments"].as_array().unwrap().is_empty());
        let body = review["body"].as_str().unwrap();
        assert!(
            body.contains("2 findings (1 blocker, 1 major) · 2 minor/nit left out · risk low"),
            "{body}"
        );
        let anchor = hex(&Sha256::digest(b"src/a.rs"));
        assert!(
            body.contains(&format!(
                "**blocker** · breaks · _correctness_\n\n[`src/a.rs:2`]({PR_URL}/files#diff-{anchor}R2)\n\nx happens"
            )),
            "{body}"
        );
        assert!(
            body.contains("[`src/a.rs:40`](https://github.com/o/r/blob/abcdef1234/src/a.rs#L40)"),
            "{body}"
        );
        assert!(body.contains(&format!("{FINDING_MARK}{} -->", key("src/a.rs", "breaks"))));
        assert!(body.contains(&format!(
            "{FINDING_MARK}{} -->",
            key("src/a.rs", "far away")
        )));
        assert!(!body.contains("tiny") && !body.contains("edge"), "{body}");
        assert!(body.contains("<!-- lens:review head=abcdef1234 verdict=REQUEST_CHANGES -->"));
    }

    #[test]
    fn inline_comments_on_the_diff_and_lists_the_rest_in_the_body() {
        let p = plan(
            &sample(),
            &Earlier::default(),
            &commentable(PATCH),
            Comments::Inline,
        );
        assert_eq!(p.inline, vec!["F1"]);
        assert_eq!(p.in_body, vec!["F2"]);
        assert_eq!(p.left_out, vec!["F3", "F4"]);
        let review = p.review.unwrap();
        assert_eq!(review["event"], "REQUEST_CHANGES");
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["line"], 2);
        assert_eq!(comments[0]["side"], "RIGHT");
        let body = review["body"].as_str().unwrap();
        assert!(
            body.contains("Findings on lines outside the diff:"),
            "{body}"
        );
        assert!(body.contains(&key("src/a.rs", "far away")));
        assert!(!body.contains(&key("src/a.rs", "breaks")));
        assert!(body.contains("2 minor/nit left out"), "{body}");
    }

    #[test]
    fn a_finding_is_unlinked_when_the_report_names_no_pr_url() {
        let mut r = sample();
        r.pr = None;
        let p = plan(
            &r,
            &Earlier::default(),
            &commentable(PATCH),
            Comments::Single,
        );
        let body = p.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("\n\n`src/a.rs:2`\n\n"), "{body}");
        assert!(!body.contains("]("), "{body}");
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
            verdict: Some("REQUEST_CHANGES".into()),
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
        for mode in [Comments::Single, Comments::Inline] {
            let p = plan(&r, &earlier, &commentable(PATCH), mode);
            let skipped: Vec<&str> = p.skipped.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(skipped, vec!["F1", "F2"]);
            assert_eq!(p.skipped[0].reason, "already posted: u");
            let posted = [p.inline, p.in_body].concat();
            assert_eq!(posted, vec!["F3"]);
        }
    }

    /// What GitHub hands back for a posted review: its body, and each inline comment.
    fn as_posted(review: &Value) -> Earlier {
        let comments: Vec<Value> = review["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| json!({"path": c["path"], "line": c["line"], "body": c["body"]}))
            .collect();
        earlier_from(&comments, &[json!({"body": review["body"]})])
    }

    #[test]
    fn a_rerun_posts_nothing_in_either_mode_or_after_a_mode_change() {
        let modes = [Comments::Single, Comments::Inline];
        for first in modes {
            for second in modes {
                let lines = commentable(PATCH);
                let review = plan(&sample(), &Earlier::default(), &lines, first)
                    .review
                    .unwrap();
                let earlier = as_posted(&review);
                assert_eq!(earlier.verdict.as_deref(), Some("REQUEST_CHANGES"));
                let again = plan(&sample(), &earlier, &lines, second);
                let skipped: Vec<&str> = again.skipped.iter().map(|s| s.id.as_str()).collect();
                assert_eq!(skipped, vec!["F1", "F2"], "{first:?} then {second:?}");
                assert!(again.review.is_none(), "{first:?} then {second:?}");
            }
        }
    }

    #[test]
    fn minors_and_nits_are_left_out_and_approve() {
        let r = report(vec![
            finding("F1", "src/a.rs", 2, "minor", "edge"),
            finding("F2", "src/a.rs", 3, "nit", "tiny"),
        ]);
        for mode in [Comments::Single, Comments::Inline] {
            let p = plan(&r, &Earlier::default(), &commentable(PATCH), mode);
            assert_eq!(p.event, "APPROVE");
            assert!(p.inline.is_empty() && p.in_body.is_empty());
            assert_eq!(p.left_out, vec!["F1", "F2"]);
            let review = p.review.unwrap();
            assert!(review["comments"].as_array().unwrap().is_empty());
            let body = review["body"].as_str().unwrap();
            assert!(
                body.starts_with("### lens review · 0 findings · 2 minor/nit left out"),
                "{body}"
            );
            assert!(!body.contains(FINDING_MARK), "{body}");
        }
    }

    #[test]
    fn a_blocker_the_fix_loop_fixed_does_not_block() {
        let mut f = finding("F1", "src/a.rs", 2, "blocker", "breaks");
        f.fix = Some(json!({"status": "resolved"}));
        let p = plan(
            &report(vec![f.clone()]),
            &Earlier::default(),
            &BTreeMap::new(),
            Comments::Single,
        );
        assert_eq!(p.event, "APPROVE");
        f.fix = Some(json!({"status": "escalated"}));
        let p = plan(
            &report(vec![f]),
            &Earlier::default(),
            &BTreeMap::new(),
            Comments::Single,
        );
        assert_eq!(p.event, "REQUEST_CHANGES");
    }

    #[test]
    fn a_clean_review_approves_with_an_empty_body() {
        let review = plan(
            &report(vec![]),
            &Earlier::default(),
            &BTreeMap::new(),
            Comments::Single,
        )
        .review
        .unwrap();
        assert_eq!(review["event"], "APPROVE");
        assert_eq!(
            review["body"],
            "<!-- lens:review head=abcdef1234 verdict=APPROVE -->"
        );
        assert!(review["comments"].as_array().unwrap().is_empty());
    }

    #[test]
    fn approves_once_the_blockers_it_requested_changes_for_are_gone() {
        let earlier = Earlier {
            reviews: 1,
            verdict: Some("REQUEST_CHANGES".into()),
            findings: vec![],
        };
        let review = plan(
            &report(vec![]),
            &earlier,
            &BTreeMap::new(),
            Comments::Single,
        )
        .review
        .unwrap();
        assert_eq!(review["event"], "APPROVE");
    }

    #[test]
    fn nothing_new_posts_nothing_once_the_verdict_stands() {
        let earlier = Earlier {
            reviews: 1,
            verdict: Some("APPROVE".into()),
            findings: vec![],
        };
        assert!(plan(
            &report(vec![]),
            &earlier,
            &BTreeMap::new(),
            Comments::Single
        )
        .review
        .is_none());
        let r = report(vec![finding("F1", "src/a.rs", 2, "major", "breaks")]);
        let earlier = Earlier {
            reviews: 1,
            verdict: Some("REQUEST_CHANGES".into()),
            findings: vec![Posted {
                key: key("src/a.rs", "breaks"),
                path: Some("src/a.rs".into()),
                line: Some(2),
                url: None,
            }],
        };
        for mode in [Comments::Single, Comments::Inline] {
            assert!(plan(&r, &earlier, &commentable(PATCH), mode)
                .review
                .is_none());
        }
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
