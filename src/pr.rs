//! What the PR pass posts, whatever the forge: reading the markers earlier lens runs left on a
//! PR, and planning one review of the verified blocker, major and minor findings. Reading and posting
//! go through [`crate::forge`]. `[pr] comments` picks where they
//! go: all in the review's post (`single`, the default) or inline on the lines the diff shows.
//! Nits are never posted; the review says how many it left out. The post is terse: an agent
//! reads it, so it holds the findings and nothing else.
//!
//! A review is never just comments: it requests changes while a blocker or major is unfixed and
//! approves otherwise.
//!
//! Every finding lens posts ends in a marker, `<!-- lens:finding key=<key> -->`, and the review
//! body in `<!-- lens:review head=<sha> verdict=<event> -->`. The key is a hash of the file and the normalized
//! title, so a later run can tell which findings it already posted. Bitbucket shows HTML comments,
//! so there the markers are link-reference lines it hides instead (see [`Markup`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use sha2::{Digest, Sha256};

use crate::config::Comments;

/// A finding within this many lines of an earlier lens comment on the same file counts as the
/// same finding.
pub const NEAR_LINES: u32 = 3;

const FINDING_MARK: &str = "<!-- lens:finding key=";
const REVIEW_MARK: &str = "<!-- lens:review";

/// Every form a marker is read in: the HTML comment GitHub hides, and the link-reference line
/// (`[//]: # (lens:…)`) Bitbucket hides, since it shows HTML comments.
const FINDING_MARKS: [&str; 2] = [FINDING_MARK, "[//]: # (lens:finding key="];
const REVIEW_MARKS: [&str; 2] = [REVIEW_MARK, "[//]: # (lens:review"];

fn has_mark(body: &str, marks: &[&str]) -> bool {
    marks.iter().any(|m| body.contains(m))
}

/// How a forge renders what lens posts: its hidden markers and file links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Markup {
    /// HTML-comment markers, links into the PR's diff.
    GitHub,
    /// `[//]: # (lens:…)` markers on their own line, links to the file at the reviewed commit.
    Bitbucket,
}

impl Markup {
    fn finding_mark(self, key: &str) -> String {
        match self {
            Markup::GitHub => format!("{FINDING_MARK}{key} -->"),
            Markup::Bitbucket => format!("{}{key})", FINDING_MARKS[1]),
        }
    }

    fn review_mark(self, head: &str, event: &str) -> String {
        match self {
            Markup::GitHub => format!("{REVIEW_MARK} head={head} verdict={event} -->"),
            Markup::Bitbucket => format!("{} head={head} verdict={event})", REVIEW_MARKS[1]),
        }
    }
}

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

    /// A nit: never posted, and never blocks.
    fn left_out(&self) -> bool {
        self.severity == "nit"
    }

    /// A blocker or major the fix loop hasn't fixed: the PR can't merge with it.
    fn blocking(&self) -> bool {
        matches!(self.severity.as_str(), "blocker" | "major")
            && self.fix.as_ref().is_none_or(|f| f["status"] != "resolved")
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

/// What a run posts on a PR, whatever the forge: a verdict with a body, and comments on lines
/// the diff shows. Each forge sends it its own way.
#[derive(Debug, Clone)]
pub struct Review {
    /// The commit the review is for.
    pub head: String,
    /// `REQUEST_CHANGES` while a blocker or major is unfixed, else `APPROVE`.
    pub event: &'static str,
    pub body: String,
    pub comments: Vec<LineComment>,
}

impl Review {
    /// Whether it raises anything: comments, or a finding in its body.
    pub fn raises_findings(&self) -> bool {
        !self.comments.is_empty() || has_mark(&self.body, &FINDING_MARKS)
    }
}

/// A comment on a line of the PR's new side.
#[derive(Debug, Clone)]
pub struct LineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
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
        if !has_mark(body, &REVIEW_MARKS) {
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
    let at = REVIEW_MARKS.iter().filter_map(|m| body.rfind(m)).max()?;
    let mark = &body[at..];
    let rest = &mark[mark.find(" verdict=")? + " verdict=".len()..];
    let v: String = rest
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
        .collect();
    (!v.is_empty()).then_some(v)
}

fn keys_in(body: &str) -> Vec<String> {
    let mut at: Vec<(usize, &str)> = FINDING_MARKS
        .iter()
        .flat_map(|m| {
            body.match_indices(m)
                .map(|(i, _)| (i, &body[i + m.len()..]))
        })
        .collect();
    at.sort_by_key(|(i, _)| *i);
    at.into_iter()
        .filter_map(|(_, rest)| {
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
    #[serde(skip)]
    pub review: Option<Review>,
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
    markup: Markup,
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
            comments.push(LineComment {
                path: f.file.clone(),
                line: f.line,
                body: comment_body(f, &key, markup),
            });
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
        // On GitHub an approval with nothing new to raise says nothing: the marker alone renders
        // empty. Bitbucket's verdict carries no text, so its summary comment always says something.
        let full = new || event == "REQUEST_CHANGES" || !left_out.is_empty();
        let body = if full || markup == Markup::Bitbucket {
            review_body(
                report,
                &BodyParts {
                    findings: &body_findings,
                    skipped: skipped.len(),
                    left_out: left_out.len(),
                    mode,
                    markup,
                },
                event,
            )
        } else {
            markup.review_mark(&report.head, event)
        };
        Review {
            head: report.head.clone(),
            event,
            body,
            comments,
        }
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

pub fn comment_body(f: &Finding, key: &str, markup: Markup) -> String {
    finding_text(f, key, None, markup)
}

/// A finding as lens posts it: severity and title, where it is when that isn't the comment's own
/// line, the failure scenario and its marker. Nothing else: the verifier's trace stays in the
/// report.
fn finding_text(f: &Finding, key: &str, location: Option<&str>, markup: Markup) -> String {
    let mut s = format!("**{}** · {}", f.severity, f.title);
    if let Some(at) = location {
        s.push_str(&format!(" · {at}"));
    }
    s.push_str("\n\n");
    if !f.failure_scenario.is_empty() {
        s.push_str(f.failure_scenario.trim());
        s.push_str("\n\n");
    }
    s.push_str(&markup.finding_mark(key));
    s
}

/// `file:line`, linked to the line in the PR's diff when the diff shows it and the forge links
/// into diffs by line (GitHub), else to the file at the reviewed commit. Unlinked when the report
/// doesn't name the PR's URL.
fn location(report: &Report, f: &Finding, on_diff: bool, markup: Markup) -> String {
    let text = format!("`{}:{}`", f.file, f.line);
    let Some(pr) = report
        .pr
        .as_ref()
        .and_then(|p| p.url.as_deref())
        .map(|u| u.trim_end_matches('/'))
    else {
        return text;
    };
    let url = match markup {
        Markup::GitHub if on_diff => {
            let anchor = hex(&Sha256::digest(f.file.as_bytes()));
            Some(format!("{pr}/files#diff-{anchor}R{}", f.line))
        }
        Markup::GitHub => pr
            .rfind("/pull/")
            .map(|i| format!("{}/blob/{}/{}#L{}", &pr[..i], report.head, f.file, f.line)),
        Markup::Bitbucket => pr.rfind("/pull-requests/").map(|i| {
            format!(
                "{}/src/{}/{}#lines-{}",
                &pr[..i],
                report.head,
                f.file,
                f.line
            )
        }),
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
    pub markup: Markup,
}

pub fn review_body(report: &Report, parts: &BodyParts, event: &str) -> String {
    let posted: Vec<&Finding> = report.findings.iter().filter(|f| !f.left_out()).collect();
    let n = posted.len();
    let mut s = format!("### lens · {n} finding{}", if n == 1 { "" } else { "s" });
    if n > 0 {
        s.push_str(&format!(" ({})", counts(&posted)));
    }
    if parts.left_out > 0 {
        s.push_str(&format!(" · {} nit left out", parts.left_out));
    }
    s.push_str("\n\n");
    if parts.skipped > 0 {
        s.push_str(&format!(
            "{} already posted on an earlier run.\n\n",
            parts.skipped
        ));
    }
    if !parts.findings.is_empty() {
        if parts.mode == Comments::Inline {
            s.push_str("Outside the diff:\n\n");
        }
        for b in parts.findings {
            let at = location(report, b.finding, b.on_diff, parts.markup);
            s.push_str(&finding_text(b.finding, &b.key, Some(&at), parts.markup));
            s.push_str("\n\n");
        }
    }
    s.push_str(&parts.markup.review_mark(&report.head, event));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What GitHub is sent for a review.
    fn github(review: Review) -> Value {
        crate::forge::github_payload(&review)
    }

    /// [`super::plan`] for GitHub.
    fn plan(
        report: &Report,
        earlier: &Earlier,
        lines: &BTreeMap<String, BTreeSet<u32>>,
        mode: Comments,
    ) -> Plan {
        super::plan(report, earlier, lines, mode, Markup::GitHub)
    }

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

    #[test]
    fn earlier_reads_the_link_reference_markers_too() {
        let comments = vec![
            json!({"path": "src/a.rs", "line": 3, "body": "x\n\n[//]: # (lens:finding key=aaaa)"}),
        ];
        let reviews = vec![
            json!({"body": "- y [//]: # (lens:finding key=bbbb)\n- z <!-- lens:finding key=cccc -->\n\n[//]: # (lens:review head=1 verdict=APPROVE)"}),
            json!({"body": "quoting [//]: # (lens:finding key=eeee)"}),
        ];
        let e = earlier_from(&comments, &reviews);
        assert_eq!(e.reviews, 1);
        assert_eq!(e.verdict.as_deref(), Some("APPROVE"));
        let keys: Vec<&str> = e.findings.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["aaaa", "bbbb", "cccc"]);
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
        assert_eq!(p.in_body, vec!["F1", "F2", "F4"]);
        assert_eq!(p.left_out, vec!["F3"]);
        let review = p.review.map(github).unwrap();
        assert_eq!(review["commit_id"], "abcdef1234");
        assert_eq!(review["event"], "REQUEST_CHANGES");
        assert!(review["comments"].as_array().unwrap().is_empty());
        let body = review["body"].as_str().unwrap();
        assert!(
            body.starts_with(
                "### lens · 3 findings (1 blocker, 1 major, 1 minor) · 1 nit left out\n\n"
            ),
            "{body}"
        );
        let anchor = hex(&Sha256::digest(b"src/a.rs"));
        assert!(
            body.contains(&format!(
                "**blocker** · breaks · [`src/a.rs:2`]({PR_URL}/files#diff-{anchor}R2)\n\nx happens\n\n<!--"
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
        assert!(body.contains("**minor** · edge"), "{body}");
        assert!(!body.contains("tiny"), "{body}");
        for fluff in [
            "Impact assessment",
            "Ran:",
            "Dropped",
            "Reviewed",
            "How it fails",
            "risk",
        ] {
            assert!(!body.contains(fluff), "{fluff} in {body}");
        }
        assert!(body.ends_with("<!-- lens:review head=abcdef1234 verdict=REQUEST_CHANGES -->"));
    }

    #[test]
    fn inline_comments_on_the_diff_and_lists_the_rest_in_the_body() {
        let p = plan(
            &sample(),
            &Earlier::default(),
            &commentable(PATCH),
            Comments::Inline,
        );
        assert_eq!(p.inline, vec!["F1", "F4"]);
        assert_eq!(p.in_body, vec!["F2"]);
        assert_eq!(p.left_out, vec!["F3"]);
        let review = p.review.map(github).unwrap();
        assert_eq!(review["event"], "REQUEST_CHANGES");
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["line"], 2);
        assert_eq!(comments[0]["side"], "RIGHT");
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("Outside the diff:"), "{body}");
        assert!(body.contains(&key("src/a.rs", "far away")));
        assert!(!body.contains(&key("src/a.rs", "breaks")));
        assert!(body.contains("1 nit left out"), "{body}");
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
        let body = p.review.map(github).unwrap()["body"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            body.contains("**blocker** · breaks · `src/a.rs:2`\n\n"),
            "{body}"
        );
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
                    .map(github)
                    .unwrap();
                let earlier = as_posted(&review);
                assert_eq!(earlier.verdict.as_deref(), Some("REQUEST_CHANGES"));
                let again = plan(&sample(), &earlier, &lines, second);
                let skipped: Vec<&str> = again.skipped.iter().map(|s| s.id.as_str()).collect();
                assert_eq!(skipped, vec!["F1", "F2", "F4"], "{first:?} then {second:?}");
                assert!(again.review.is_none(), "{first:?} then {second:?}");
            }
        }
    }

    #[test]
    fn minors_are_posted_but_approve_and_nits_are_left_out() {
        let r = report(vec![
            finding("F1", "src/a.rs", 2, "minor", "edge"),
            finding("F2", "src/a.rs", 3, "nit", "tiny"),
        ]);
        for mode in [Comments::Single, Comments::Inline] {
            let p = plan(&r, &Earlier::default(), &commentable(PATCH), mode);
            assert_eq!(p.event, "APPROVE");
            assert_eq!([p.inline.clone(), p.in_body.clone()].concat(), vec!["F1"]);
            assert_eq!(p.left_out, vec!["F2"]);
            let review = p.review.map(github).unwrap();
            let body = review["body"].as_str().unwrap();
            assert!(
                body.starts_with("### lens · 1 finding (1 minor) · 1 nit left out"),
                "{body}"
            );
            assert!(!body.contains("tiny"), "{body}");
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
        .map(github)
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
        .map(github)
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
    fn a_comment_is_severity_title_and_scenario_without_the_trace() {
        let f = finding("F1", "a.rs", 1, "blocker", "breaks");
        let b = comment_body(&f, "k", Markup::GitHub);
        assert_eq!(
            b,
            "**blocker** · breaks\n\nx happens\n\n<!-- lens:finding key=k -->"
        );
    }
}
