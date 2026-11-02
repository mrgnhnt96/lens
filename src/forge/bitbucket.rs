//! [`Forge`] for Bitbucket Cloud, through its REST API (`https://api.bitbucket.org/2.0`, or
//! `$LENS_BITBUCKET_API`).
//!
//! `post_as = "owner"` signs in with an Atlassian API token (`$LENS_BITBUCKET_EMAIL` and
//! `$LENS_BITBUCKET_TOKEN`, Basic auth); `post_as = "bot"` with a repository access token from
//! `bot_token_env` (Bearer auth). Git fetches use git's own credentials for origin.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Forge, PrView};
use crate::config::{PostAs, PrConfig};
use crate::pr::{earlier_from, Earlier, Review};

pub const DEFAULT_API: &str = "https://api.bitbucket.org/2.0";
pub const EMAIL_ENV: &str = "LENS_BITBUCKET_EMAIL";
pub const TOKEN_ENV: &str = "LENS_BITBUCKET_TOKEN";

/// The most pages of comments `earlier` reads, so a misbehaving `next` can't loop forever.
const MAX_PAGES: usize = 100;

const PR_FIELDS: &str = "id,title,description,links.html.href,source.branch.name,source.commit.hash,source.repository.full_name,destination.branch.name";
const COMMENT_FIELDS: &str =
    "next,values.id,values.deleted,values.inline,values.content.raw,values.links.html.href";

/// A GET against the API, answering its JSON.
pub trait Http {
    fn get(&self, url: &str) -> Result<Value>;
}

struct Ureq {
    agent: ureq::Agent,
    auth: String,
}

impl Http for Ureq {
    fn get(&self, url: &str) -> Result<Value> {
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .call();
        let (status, text) = match resp {
            Ok(r) => (r.status(), r.into_string()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string()),
            Err(e) => return Err(e).with_context(|| format!("GET {url}")),
        };
        let text = text.with_context(|| format!("reading GET {url}"))?;
        if !(200..300).contains(&status) {
            let why = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.trim().chars().take(300).collect());
            let hint = match status {
                401 | 403 => "; check the token and its scopes (README: Bitbucket)",
                _ => "",
            };
            bail!("Bitbucket answered {status} to GET {url}: {why}{hint}");
        }
        serde_json::from_str(&text).with_context(|| format!("parsing GET {url}"))
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
        let v = self.http.get(&format!(
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
        let v = self
            .http
            .get(&format!("{}?fields={PR_FIELDS}", self.pr_url(pr)))?;
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
            let page = self.http.get(&url)?;
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

    fn payload(&self, review: &Review) -> Value {
        json!({
            "summary": review.body,
            "inline": review.comments.iter().map(|c| json!({
                "path": c.path,
                "to": c.line,
                "body": c.body,
            })).collect::<Vec<_>>(),
            "verdict": review.event,
        })
    }

    fn post(&self, _pr: u64, _review: &Review) -> Result<Option<String>> {
        bail!("lens can't post on Bitbucket pull requests yet; --dry-run shows what it would post")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    struct Fake {
        pages: BTreeMap<String, Value>,
        asked: RefCell<Vec<String>>,
    }

    impl Http for Fake {
        fn get(&self, url: &str) -> Result<Value> {
            self.asked.borrow_mut().push(url.to_string());
            self.pages
                .iter()
                .find(|(prefix, _)| url.starts_with(prefix.as_str()))
                .map(|(_, v)| v.clone())
                .with_context(|| format!("no page for {url}"))
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
        let fake = Fake {
            pages: pages
                .iter()
                .map(|(k, v)| (format!("http://bb{k}"), v.clone()))
                .collect(),
            asked: RefCell::new(Vec::new()),
        };
        Bitbucket::with_http(dir, Box::new(fake), "http://bb/").unwrap()
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
}
