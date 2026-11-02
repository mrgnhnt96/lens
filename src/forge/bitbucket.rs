//! [`Forge`] for Bitbucket Cloud, through its REST API (`https://api.bitbucket.org/2.0`, or
//! `$LENS_BITBUCKET_API`).
//!
//! `post_as = "owner"` signs in with an Atlassian API token (`$LENS_BITBUCKET_EMAIL` and
//! `$LENS_BITBUCKET_TOKEN`, Basic auth); `post_as = "bot"` with a repository access token from
//! `bot_token_env` (Bearer auth). Git fetches use git's own credentials for origin.
//!
//! A review is a verdict (approve or request changes) and comments: the verdict first, so lens
//! never leaves comments without one, then a comment on each inline finding's line, then the
//! summary comment, whose marker records the review for later runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Forge, PrView};
use crate::config::{PostAs, PrConfig};
use crate::pr::{earlier_from, Earlier, Markup, Review};

pub const DEFAULT_API: &str = "https://api.bitbucket.org/2.0";
pub const EMAIL_ENV: &str = "LENS_BITBUCKET_EMAIL";
pub const TOKEN_ENV: &str = "LENS_BITBUCKET_TOKEN";

/// The most pages of comments `earlier` reads, so a misbehaving `next` can't loop forever.
const MAX_PAGES: usize = 100;

const PR_FIELDS: &str = "id,title,description,links.html.href,source.branch.name,source.commit.hash,source.repository.full_name,destination.branch.name";
const COMMENT_FIELDS: &str =
    "next,values.id,values.deleted,values.inline,values.content.raw,values.links.html.href";

/// One API request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub body: Option<Value>,
}

impl Request {
    fn get(url: String) -> Request {
        Request {
            method: "GET",
            url,
            body: None,
        }
    }

    fn post(url: String, body: Option<Value>) -> Request {
        Request {
            method: "POST",
            url,
            body,
        }
    }

    fn describe(&self) -> String {
        format!("{} {}", self.method, self.url)
    }
}

/// What the API answered, whatever its status.
pub struct Reply {
    pub status: u16,
    pub text: String,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON when the status is a success; `Null` when there is none.
    fn json(self, req: &Request) -> Result<Value> {
        if !self.ok() {
            let why = serde_json::from_str::<Value>(&self.text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| self.text.trim().chars().take(300).collect());
            let hint = match self.status {
                401 | 403 => "; check the token and its scopes (README: Bitbucket)",
                _ => "",
            };
            bail!(
                "Bitbucket answered {} to {}: {why}{hint}",
                self.status,
                req.describe()
            );
        }
        if self.text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&self.text).with_context(|| format!("parsing {}", req.describe()))
    }
}

/// Sends a request to the API; only a failure to reach it is an error.
pub trait Http {
    fn send(&self, req: &Request) -> Result<Reply>;
}

struct Ureq {
    agent: ureq::Agent,
    auth: String,
}

impl Http for Ureq {
    fn send(&self, req: &Request) -> Result<Reply> {
        let call = self
            .agent
            .request(req.method, &req.url)
            .set("Authorization", &self.auth)
            .set("Accept", "application/json");
        let resp = match &req.body {
            Some(body) => call
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
            None => call.call(),
        };
        let (status, text) = match resp {
            Ok(r) => (r.status(), r.into_string()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string()),
            Err(e) => return Err(e).with_context(|| req.describe()),
        };
        let text = text.with_context(|| format!("reading {}", req.describe()))?;
        Ok(Reply { status, text })
    }
}

/// Bitbucket Cloud, for the repo origin points at, with the identity `.lens.toml` picks.
pub struct Bitbucket {
    http: Box<dyn Http>,
    api: String,
    repo: PathBuf,
    /// `workspace/repo`.
    slug: String,
}

impl Bitbucket {
    pub fn new(repo: &Path, config: &PrConfig) -> Result<Bitbucket> {
        let auth = auth(config, |k| std::env::var(k).ok())?;
        let http = Ureq {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
            auth,
        };
        let api = std::env::var("LENS_BITBUCKET_API").unwrap_or_else(|_| DEFAULT_API.into());
        Bitbucket::with_http(repo, Box::new(http), &api)
    }

    pub fn with_http(repo: &Path, http: Box<dyn Http>, api: &str) -> Result<Bitbucket> {
        let origin = origin_url(repo)?;
        let slug = slug_of(&origin).with_context(|| {
            format!("telling the Bitbucket workspace and repo from origin {origin}")
        })?;
        Ok(Bitbucket {
            http,
            api: api.trim_end_matches('/').to_string(),
            repo: repo.to_path_buf(),
            slug,
        })
    }

    fn pr_url(&self, pr: u64) -> String {
        format!("{}/repositories/{}/pullrequests/{pr}", self.api, self.slug)
    }

    fn get(&self, url: String) -> Result<Value> {
        let req = Request::get(url);
        self.http.send(&req)?.json(&req)
    }

    /// What posting `review` on PR `pr` sends, in order: clearing the opposite verdict, giving
    /// this one, a comment on each inline finding's line, then the summary.
    pub fn requests(&self, pr: u64, review: &Review) -> Vec<Request> {
        let at = self.pr_url(pr);
        let (verdict, opposite) = match review.event {
            "APPROVE" => ("approve", "request-changes"),
            _ => ("request-changes", "approve"),
        };
        let mut out = vec![
            Request {
                method: "DELETE",
                url: format!("{at}/{opposite}"),
                body: None,
            },
            Request::post(format!("{at}/{verdict}"), None),
        ];
        for c in &review.comments {
            out.push(Request::post(
                format!("{at}/comments"),
                Some(json!({
                    "content": {"raw": c.body},
                    "inline": {"path": c.path, "to": c.line},
                })),
            ));
        }
        out.push(Request::post(
            format!("{at}/comments"),
            Some(json!({"content": {"raw": review.body}})),
        ));
        out
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The full hash of `rev` in `repo` (`workspace/repo`); the PR endpoint gives a short one.
    fn full_hash(&self, repo: &str, rev: &str) -> Result<String> {
        if rev.len() == 40 {
            return Ok(rev.to_string());
        }
        let v = self.get(format!(
            "{}/repositories/{repo}/commit/{rev}?fields=hash",
            self.api
        ))?;
        match v["hash"].as_str() {
            Some(h) if h.starts_with(rev) => Ok(h.to_string()),
            _ => bail!("Bitbucket gave no full hash for commit {rev} in {repo}"),
        }
    }
}

/// The `Authorization` header for `post_as`, from the environment `var` reads.
pub fn auth(config: &PrConfig, var: impl Fn(&str) -> Option<String>) -> Result<String> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty());
    match config.post_as {
        PostAs::Owner => match (set(EMAIL_ENV), set(TOKEN_ENV)) {
            (Some(email), Some(token)) => Ok(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            )),
            _ => bail!(
                "lens reads Bitbucket as you through an Atlassian API token: set ${EMAIL_ENV} to your Atlassian email and ${TOKEN_ENV} to a token made at id.atlassian.com (Security, API tokens) with read:repository:bitbucket, read:pullrequest:bitbucket and write:pullrequest:bitbucket; or post as a bot with post_as = \"bot\""
            ),
        },
        PostAs::Bot => {
            let env = config.bot_token_env();
            match set(env) {
                Some(token) => Ok(format!("Bearer {token}")),
                None => bail!(
                    ".lens.toml posts as a bot, but ${env} holds no token; set it to a repository access token (Repository settings, Access tokens) with pullrequest:write, or use post_as = \"owner\""
                ),
            }
        }
    }
}

fn origin_url(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .stdin(Stdio::null())
        .output()
        .context("running git remote get-url origin")?;
    if !out.status.success() {
        bail!("the repo has no origin remote to tell its Bitbucket workspace and repo from");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `workspace/repo` from an origin URL: its path's last two segments, without `.git`.
pub fn slug_of(url: &str) -> Option<String> {
    let path = url.split_once("://").map_or_else(
        || url.split_once(':').map_or(url, |(_, p)| p),
        |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p),
    );
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplit('/').filter(|s| !s.is_empty());
    let repo = parts.next()?;
    let ws = parts.next()?;
    Some(format!("{ws}/{repo}"))
}

/// The fork's clone URL in the style of origin's (ssh or https), with its path swapped for
/// `full_name`.
pub fn fork_url(origin: &str, slug: &str, full_name: &str) -> Option<String> {
    let at = origin.rfind(slug)?;
    Some(format!(
        "{}{full_name}{}",
        &origin[..at],
        &origin[at + slug.len()..]
    ))
}

/// The PR as [`PrView`] from `GET …/pullrequests/<n>`, with the head's full hash and, for a fork,
/// where its branch lives.
pub fn view_from(v: &Value, slug: &str, head_oid: String) -> Result<PrView> {
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_string);
    let need = |p: &str| s(p).with_context(|| format!("Bitbucket's PR has no {p}"));
    let source = s("/source/repository/full_name");
    Ok(PrView {
        number: v["id"].as_u64().context("Bitbucket's PR has no id")?,
        url: need("/links/html/href")?,
        title: need("/title")?,
        body: s("/description").unwrap_or_default(),
        base_ref_name: need("/destination/branch/name")?,
        head_ref_name: need("/source/branch/name")?,
        head_ref_oid: head_oid,
        head_repo: source.filter(|f| !f.eq_ignore_ascii_case(slug)),
    })
}

/// [`Earlier`] from every page of `GET …/pullrequests/<n>/comments`: inline comments carry
/// findings, general ones lens's review summaries. Deleted comments don't count.
pub fn earlier_from_comments(mut values: Vec<Value>) -> Earlier {
    values.retain(|c| c["deleted"].as_bool() != Some(true));
    values.sort_by_key(|c| c["id"].as_u64().unwrap_or(0));
    let mut inline = Vec::new();
    let mut general = Vec::new();
    for c in &values {
        let body = c.pointer("/content/raw").cloned().unwrap_or(Value::Null);
        let url = c
            .pointer("/links/html/href")
            .cloned()
            .unwrap_or(Value::Null);
        match c.get("inline").filter(|i| i.is_object()) {
            Some(i) => {
                let line = if i["outdated"].as_bool() == Some(true) {
                    Value::Null
                } else {
                    i["to"].clone()
                };
                inline
                    .push(json!({"body": body, "path": i["path"], "line": line, "html_url": url}));
            }
            None => general.push(json!({"body": body, "html_url": url})),
        }
    }
    earlier_from(&inline, &general)
}

impl Forge for Bitbucket {
    fn view(&self, pr: u64) -> Result<PrView> {
        let v = self.get(format!("{}?fields={PR_FIELDS}", self.pr_url(pr)))?;
        let short = v
            .pointer("/source/commit/hash")
            .and_then(Value::as_str)
            .context("Bitbucket's PR has no source commit")?;
        let source = v
            .pointer("/source/repository/full_name")
            .and_then(Value::as_str)
            .unwrap_or(&self.slug);
        let oid = self.full_hash(source, short)?;
        view_from(&v, &self.slug, oid)
    }

    /// Bitbucket has no pull refs: the head is the source branch, from the fork when the PR
    /// comes from one. Stops when the fetched tip isn't the commit the PR was read at.
    fn fetch(&self, pr: &PrView) -> Result<()> {
        let n = pr.number;
        let base = &pr.base_ref_name;
        let head = &pr.head_ref_name;
        let base_spec = format!("+refs/heads/{base}:refs/remotes/origin/{base}");
        let head_spec = format!("+refs/heads/{head}:refs/lens/pr/{n}");
        let fetched = match &pr.head_repo {
            None => self.git(&["fetch", "--quiet", "origin", &base_spec, &head_spec]),
            Some(fork) => {
                let origin = origin_url(&self.repo)?;
                let url = fork_url(&origin, &self.slug, fork).with_context(|| {
                    format!("telling the fork {fork}'s URL from origin {origin}")
                })?;
                self.git(&["fetch", "--quiet", "origin", &base_spec])
                    .and_then(|_| self.git(&["fetch", "--quiet", &url, &head_spec]))
            }
        };
        fetched.with_context(|| format!("fetching PR {n}"))?;
        let tip = self.git(&["rev-parse", &format!("refs/lens/pr/{n}")])?;
        if tip != pr.head_ref_oid {
            bail!(
                "PR {n}'s branch {head} is at {tip}, not {} as Bitbucket said; it moved, run again",
                pr.head_ref_oid
            );
        }
        Ok(())
    }

    fn earlier(&self, pr: u64) -> Result<Earlier> {
        let mut values = Vec::new();
        let mut next = Some(format!(
            "{}/comments?pagelen=100&fields={COMMENT_FIELDS}",
            self.pr_url(pr)
        ));
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else { break };
            let page = self.get(url)?;
            if let Some(vs) = page["values"].as_array() {
                values.extend(vs.iter().cloned());
            }
            next = page["next"].as_str().map(str::to_string);
        }
        if next.is_some() {
            bail!("PR {pr} has more than {MAX_PAGES} pages of comments");
        }
        Ok(earlier_from_comments(values))
    }

    fn markup(&self) -> Markup {
        Markup::Bitbucket
    }

    fn payload(&self, pr: u64, review: &Review) -> Value {
        let requests: Vec<Value> = self
            .requests(pr, review)
            .into_iter()
            .map(|r| match r.body {
                Some(body) => json!({"method": r.method, "url": r.url, "body": body}),
                None => json!({"method": r.method, "url": r.url}),
            })
            .collect();
        json!({"requests": requests})
    }

    /// Clearing a verdict lens never gave is no error; giving one it already gave (409) isn't
    /// either. A verdict Bitbucket refuses stops the post before any comment goes up.
    fn post(&self, pr: u64, review: &Review) -> Result<Option<String>> {
        let mut summary = None;
        for req in self.requests(pr, review) {
            let reply = self.http.send(&req)?;
            match req.method {
                "DELETE" if reply.ok() || matches!(reply.status, 400 | 404 | 409) => {}
                "POST" if req.body.is_none() && reply.status == 409 => {}
                "POST" if req.body.is_none() => {
                    reply.json(&req).with_context(|| {
                        format!(
                            "Bitbucket wouldn't take lens's verdict on PR {pr}, so nothing was posted (lens never leaves comments without one). On your own PR, post as a bot (post_as = \"bot\") to give it one"
                        )
                    })?;
                }
                _ => {
                    let v = reply.json(&req)?;
                    summary = v
                        .pointer("/links/html/href")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
        }
        Ok(Some(summary.unwrap_or_else(|| self.pr_url(pr))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Comments;
    use crate::pr::{commentable, plan, Finding, PrRef, Report};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    type Sent = Rc<RefCell<Vec<Request>>>;

    /// The API: GETs answered from `pages`, anything else from the first of `replies` whose
    /// method and URL prefix match, else 200 with a link to what was posted. Keeps each request.
    struct Fake {
        pages: BTreeMap<String, Value>,
        replies: Vec<(&'static str, String, u16)>,
        sent: Sent,
    }

    impl Http for Fake {
        fn send(&self, req: &Request) -> Result<Reply> {
            self.sent.borrow_mut().push(req.clone());
            if req.method == "GET" {
                return Ok(
                    match self
                        .pages
                        .iter()
                        .find(|(prefix, _)| req.url.starts_with(prefix.as_str()))
                    {
                        Some((_, v)) => Reply {
                            status: 200,
                            text: v.to_string(),
                        },
                        None => Reply {
                            status: 404,
                            text: json!({"error": {"message": "no page"}}).to_string(),
                        },
                    },
                );
            }
            let status = self
                .replies
                .iter()
                .find(|(m, prefix, _)| *m == req.method && req.url.starts_with(prefix.as_str()))
                .map_or(200, |(_, _, s)| *s);
            let n = self.sent.borrow().len();
            Ok(Reply {
                status,
                text: json!({"links": {"html": {"href": format!("posted-{n}")}}}).to_string(),
            })
        }
    }

    fn repo_with_origin(origin: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["remote", "add", "origin", origin],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        dir
    }

    fn bitbucket(dir: &Path, pages: &[(&str, Value)]) -> Bitbucket {
        bitbucket_with(dir, pages, &[]).0
    }

    fn bitbucket_with(
        dir: &Path,
        pages: &[(&str, Value)],
        replies: &[(&'static str, &str, u16)],
    ) -> (Bitbucket, Sent) {
        let sent = Sent::default();
        let fake = Fake {
            pages: pages
                .iter()
                .map(|(k, v)| (format!("http://bb{k}"), v.clone()))
                .collect(),
            replies: replies
                .iter()
                .map(|(m, k, s)| (*m, format!("http://bb{k}"), *s))
                .collect(),
            sent: sent.clone(),
        };
        let bb = Bitbucket::with_http(dir, Box::new(fake), "http://bb/").unwrap();
        (bb, sent)
    }

    const AT: &str = "http://bb/repositories/team/repo/pullrequests/7";
    const PR_PATH: &str = "/repositories/team/repo/pullrequests/7";
    const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {
+    new();
 }
";

    fn finding(id: &str, file: &str, line: u32, severity: &str, title: &str) -> Finding {
        Finding {
            id: id.into(),
            profiles: vec!["correctness".into()],
            file: file.into(),
            line,
            severity: severity.into(),
            title: title.into(),
            failure_scenario: "x happens".into(),
            how_it_fails: None,
            fix: None,
        }
    }

    fn report(findings: Vec<Finding>) -> Report {
        Report {
            pr: Some(PrRef {
                url: Some("https://bitbucket.org/team/repo/pull-requests/7".into()),
            }),
            head: "abcdef1234".into(),
            base: "origin/main".into(),
            risk: None,
            assessment: None,
            profiles: vec![],
            findings,
            dropped: vec![],
        }
    }

    /// A blocker on a line the diff shows and a major on one it doesn't.
    fn sample() -> Report {
        report(vec![
            finding("F1", "src/a.rs", 2, "blocker", "breaks"),
            finding("F2", "src/a.rs", 40, "major", "far away"),
            finding("F3", "src/a.rs", 3, "nit", "tiny"),
        ])
    }

    fn review_of(r: &Report, earlier: &Earlier, mode: Comments) -> Option<Review> {
        plan(r, earlier, &commentable(PATCH), mode, Markup::Bitbucket).review
    }

    fn methods(sent: &Sent) -> Vec<String> {
        sent.borrow()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.trim_start_matches(AT)))
            .collect()
    }

    /// The comments as Bitbucket lists them back, numbered in the order they went up.
    fn as_listed(sent: &Sent) -> Vec<Value> {
        sent.borrow()
            .iter()
            .filter_map(|r| r.body.as_ref())
            .enumerate()
            .map(|(i, b)| {
                let mut c = json!({"id": i + 1, "content": b["content"]});
                if b["inline"].is_object() {
                    c["inline"] = b["inline"].clone();
                }
                c
            })
            .collect()
    }

    fn pr_json(source_repo: &str) -> Value {
        json!({
            "id": 7,
            "title": "Add a thing",
            "description": "Why it matters",
            "links": {"html": {"href": "https://bitbucket.org/team/repo/pull-requests/7"}},
            "source": {
                "branch": {"name": "feat/thing"},
                "commit": {"hash": "0123456789ab"},
                "repository": {"full_name": source_repo}
            },
            "destination": {"branch": {"name": "main"}}
        })
    }

    #[test]
    fn slug_is_the_workspace_and_repo_of_any_origin_form() {
        for url in [
            "git@bitbucket.org:team/repo.git",
            "ssh://git@bitbucket.org/team/repo.git",
            "https://me@bitbucket.org/team/repo.git",
            "https://bitbucket.org/team/repo",
            "https://bitbucket.org/team/repo/",
            "/srv/git/team/repo.git",
        ] {
            assert_eq!(slug_of(url).as_deref(), Some("team/repo"), "{url}");
        }
        assert_eq!(slug_of("https://bitbucket.org/repo"), None);
    }

    #[test]
    fn a_fork_is_fetched_the_way_origin_is() {
        assert_eq!(
            fork_url(
                "git@bitbucket.org:team/repo.git",
                "team/repo",
                "them/repo-fork"
            )
            .as_deref(),
            Some("git@bitbucket.org:them/repo-fork.git")
        );
        assert_eq!(
            fork_url("https://me@bitbucket.org/team/repo", "team/repo", "them/r").as_deref(),
            Some("https://me@bitbucket.org/them/r")
        );
    }

    #[test]
    fn owner_signs_in_with_email_and_api_token_and_bot_with_its_access_token() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let owner = PrConfig::default();
        let basic = auth(&owner, env(&[(EMAIL_ENV, "me@x.io"), (TOKEN_ENV, "tok")])).unwrap();
        let encoded = basic.strip_prefix("Basic ").expect(&basic);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "me@x.io:tok");
        let err = auth(&owner, env(&[(TOKEN_ENV, "tok")])).unwrap_err();
        assert!(format!("{err}").contains(EMAIL_ENV), "{err}");

        let bot: PrConfig =
            toml::from_str("post_as = \"bot\"\nbot_token_env = \"BB_BOT\"\n").unwrap();
        assert_eq!(auth(&bot, env(&[("BB_BOT", "abc")])).unwrap(), "Bearer abc");
        let err = auth(&bot, env(&[(TOKEN_ENV, "tok")])).unwrap_err();
        assert!(format!("{err}").contains("$BB_BOT"), "{err}");
    }

    #[test]
    fn view_maps_the_pr_with_the_heads_full_hash() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let full = "0123456789abcdef0123456789abcdef01234567";
        let bb = bitbucket(
            dir.path(),
            &[
                (
                    "/repositories/team/repo/pullrequests/7",
                    pr_json("team/repo"),
                ),
                (
                    "/repositories/team/repo/commit/0123456789ab",
                    json!({"hash": full}),
                ),
            ],
        );
        let view = bb.view(7).unwrap();
        assert_eq!(
            view,
            PrView {
                number: 7,
                url: "https://bitbucket.org/team/repo/pull-requests/7".into(),
                title: "Add a thing".into(),
                body: "Why it matters".into(),
                base_ref_name: "main".into(),
                head_ref_name: "feat/thing".into(),
                head_ref_oid: full.into(),
                head_repo: None,
            }
        );
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("headRepo").is_none(), "{json}");
    }

    #[test]
    fn view_of_a_fork_pr_names_the_fork_and_reads_its_commit_there() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let full = "0123456789abcdef0123456789abcdef01234567";
        let bb = bitbucket(
            dir.path(),
            &[
                (
                    "/repositories/team/repo/pullrequests/7",
                    pr_json("them/fork"),
                ),
                (
                    "/repositories/them/fork/commit/0123456789ab",
                    json!({"hash": full}),
                ),
            ],
        );
        let view = bb.view(7).unwrap();
        assert_eq!(view.head_repo.as_deref(), Some("them/fork"));
        assert_eq!(view.head_ref_oid, full);
    }

    #[test]
    fn earlier_reads_every_page_and_skips_deleted_comments() {
        let dir = repo_with_origin("https://bitbucket.org/team/repo.git");
        let finding = |key: &str| format!("x\n\n[//]: # (lens:finding key={key})");
        let bb = bitbucket(
            dir.path(),
            &[
                (
                    "/repositories/team/repo/pullrequests/7/comments?pagelen",
                    json!({
                        "values": [
                            {"id": 3, "content": {"raw": finding("aaaa")},
                             "inline": {"path": "src/a.rs", "to": 12},
                             "links": {"html": {"href": "u3"}}},
                            {"id": 4, "deleted": true, "content": {"raw": finding("dead")},
                             "inline": {"path": "src/a.rs", "to": 40}}
                        ],
                        "next": "http://bb/page2"
                    }),
                ),
                (
                    "/page2",
                    json!({"values": [
                        {"id": 9, "content": {"raw": format!("- y {}\n\n[//]: # (lens:review head=1 verdict=APPROVE)", finding("cccc"))},
                         "links": {"html": {"href": "u9"}}},
                        {"id": 5, "content": {"raw": "<!-- lens:finding key=bbbb -->\n\n<!-- lens:review head=0 verdict=REQUEST_CHANGES -->"}},
                        {"id": 6, "content": {"raw": "quoting [//]: # (lens:finding key=eeee)"}},
                        {"id": 8, "content": {"raw": "outdated [//]: # (lens:finding key=dddd)"},
                         "inline": {"path": "src/b.rs", "to": 3, "outdated": true}}
                    ]}),
                ),
            ],
        );
        let earlier = bb.earlier(7).unwrap();
        assert_eq!(earlier.reviews, 2);
        assert_eq!(earlier.verdict.as_deref(), Some("APPROVE"));
        let keys: Vec<_> = earlier
            .findings
            .iter()
            .map(|p| (p.key.as_str(), p.path.as_deref(), p.line))
            .collect();
        assert_eq!(
            keys,
            [
                ("aaaa", Some("src/a.rs"), Some(12)),
                ("dddd", Some("src/b.rs"), None),
                ("bbbb", None, None),
                ("cccc", None, None),
            ]
        );
    }

    #[test]
    fn single_gives_the_verdict_then_one_comment_with_every_finding() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let (bb, sent) = bitbucket_with(dir.path(), &[], &[]);
        let review = review_of(&sample(), &Earlier::default(), Comments::Single).unwrap();
        assert_eq!(bb.post(7, &review).unwrap().as_deref(), Some("posted-3"));
        assert_eq!(
            methods(&sent),
            ["DELETE /approve", "POST /request-changes", "POST /comments"]
        );
        let sent = sent.borrow();
        assert!(sent[1].body.is_none());
        let raw = sent[2].body.as_ref().unwrap()["content"]["raw"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sent[2].body.as_ref().unwrap().get("inline").is_none());
        assert!(
            raw.contains(
                "[`src/a.rs:2`](https://bitbucket.org/team/repo/src/abcdef1234/src/a.rs#lines-2)"
            ),
            "{raw}"
        );
        assert!(raw.contains("#lines-40)"), "{raw}");
        assert!(
            raw.contains(&format!(
                "\n\n[//]: # (lens:finding key={})",
                crate::pr::key("src/a.rs", "breaks")
            )),
            "{raw}"
        );
        assert!(
            raw.ends_with("[//]: # (lens:review head=abcdef1234 verdict=REQUEST_CHANGES)"),
            "{raw}"
        );
        assert!(!raw.contains("<!--") && !raw.contains("<details>"), "{raw}");
        assert!(!raw.contains("tiny"), "{raw}");
    }

    #[test]
    fn inline_comments_on_the_diffs_lines_and_sums_up_last() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let (bb, sent) = bitbucket_with(dir.path(), &[], &[]);
        let review = review_of(&sample(), &Earlier::default(), Comments::Inline).unwrap();
        bb.post(7, &review).unwrap();
        assert_eq!(
            methods(&sent),
            [
                "DELETE /approve",
                "POST /request-changes",
                "POST /comments",
                "POST /comments"
            ]
        );
        let sent = sent.borrow();
        let inline = sent[2].body.as_ref().unwrap();
        assert_eq!(inline["inline"], json!({"path": "src/a.rs", "to": 2}));
        let raw = inline["content"]["raw"].as_str().unwrap();
        assert!(raw.contains("[//]: # (lens:finding key="), "{raw}");
        let summary = sent[3].body.as_ref().unwrap()["content"]["raw"]
            .as_str()
            .unwrap();
        assert!(
            summary.contains("#lines-40)") && !summary.contains("#lines-2)"),
            "{summary}"
        );
    }

    #[test]
    fn an_approval_clears_a_request_for_changes_and_still_says_why() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let (bb, sent) = bitbucket_with(dir.path(), &[], &[]);
        let earlier = Earlier {
            reviews: 1,
            verdict: Some("REQUEST_CHANGES".into()),
            head: None,
            findings: vec![],
        };
        let review = review_of(&report(vec![]), &earlier, Comments::Single).unwrap();
        assert_eq!(review.event, "APPROVE");
        bb.post(7, &review).unwrap();
        assert_eq!(
            methods(&sent),
            ["DELETE /request-changes", "POST /approve", "POST /comments"]
        );
        let raw = sent.borrow()[2].body.as_ref().unwrap()["content"]["raw"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(raw.starts_with("### lens · 0 findings"), "{raw}");
        assert!(
            raw.ends_with("[//]: # (lens:review head=abcdef1234 verdict=APPROVE)"),
            "{raw}"
        );
    }

    #[test]
    fn a_verdict_it_never_gave_or_already_gave_is_no_error() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        for (clear, give) in [(404, 409), (400, 200), (409, 409)] {
            let (bb, sent) = bitbucket_with(
                dir.path(),
                &[],
                &[
                    ("DELETE", &format!("{PR_PATH}/approve"), clear),
                    ("POST", &format!("{PR_PATH}/request-changes"), give),
                ],
            );
            let review = review_of(&sample(), &Earlier::default(), Comments::Single).unwrap();
            bb.post(7, &review).unwrap();
            assert_eq!(sent.borrow().len(), 3, "{clear} {give}");
        }
    }

    #[test]
    fn a_refused_verdict_posts_no_comments() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let (bb, sent) = bitbucket_with(
            dir.path(),
            &[],
            &[("POST", &format!("{PR_PATH}/request-changes"), 400)],
        );
        let review = review_of(&sample(), &Earlier::default(), Comments::Inline).unwrap();
        let err = format!("{:#}", bb.post(7, &review).unwrap_err());
        assert!(err.contains("nothing was posted"), "{err}");
        assert!(err.contains("post_as = \"bot\""), "{err}");
        assert_eq!(methods(&sent), ["DELETE /approve", "POST /request-changes"]);
    }

    #[test]
    fn a_failed_comment_says_what_bitbucket_answered() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let (bb, _) = bitbucket_with(
            dir.path(),
            &[],
            &[("POST", &format!("{PR_PATH}/comments"), 403)],
        );
        let review = review_of(&sample(), &Earlier::default(), Comments::Single).unwrap();
        let err = format!("{:#}", bb.post(7, &review).unwrap_err());
        assert!(err.contains("403 to POST http://bb/"), "{err}");
        assert!(err.contains("check the token and its scopes"), "{err}");
    }

    #[test]
    fn the_dry_run_payload_is_the_requests_in_order() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let bb = bitbucket(dir.path(), &[]);
        let review = review_of(&sample(), &Earlier::default(), Comments::Inline).unwrap();
        let payload = bb.payload(7, &review);
        let requests = payload["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0],
            json!({"method": "DELETE", "url": format!("{AT}/approve")})
        );
        assert_eq!(
            requests[1],
            json!({"method": "POST", "url": format!("{AT}/request-changes")})
        );
        assert_eq!(requests[2]["body"]["inline"]["to"], 2);
        assert_eq!(requests[3]["body"]["content"]["raw"], review.body);
    }

    #[test]
    fn a_rerun_posts_nothing_in_either_mode() {
        let dir = repo_with_origin("git@bitbucket.org:team/repo.git");
        let modes = [Comments::Single, Comments::Inline];
        for first in modes {
            for second in modes {
                let (bb, sent) = bitbucket_with(dir.path(), &[], &[]);
                let review = review_of(&sample(), &Earlier::default(), first).unwrap();
                bb.post(7, &review).unwrap();
                let earlier = earlier_from_comments(as_listed(&sent));
                assert_eq!(earlier.verdict.as_deref(), Some("REQUEST_CHANGES"));
                let mut moved = sample();
                moved.findings[0].line = 4;
                let again = plan(
                    &moved,
                    &earlier,
                    &commentable(PATCH),
                    second,
                    Markup::Bitbucket,
                );
                let skipped: Vec<&str> = again.skipped.iter().map(|s| s.id.as_str()).collect();
                assert_eq!(skipped, ["F1", "F2"], "{first:?} then {second:?}");
                assert!(again.review.is_none(), "{first:?} then {second:?}");
            }
        }
    }
}
