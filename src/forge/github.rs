//! [`Forge`] for GitHub, through the `gh` CLI.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{Forge, PrView};
use crate::config::{PostAs, PrConfig};
use crate::pr::{earlier_from, Earlier, Review};

/// GitHub, through `gh` (or `$LENS_GH`) run in the repo with the identity `.lens.toml` picks.
pub struct GitHub {
    program: PathBuf,
    repo: PathBuf,
    token: Option<String>,
}

impl GitHub {
    pub fn new(repo: &Path, config: &PrConfig) -> Result<GitHub> {
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
        Ok(GitHub {
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
}

impl Forge for GitHub {
    fn view(&self, pr: u64) -> Result<PrView> {
        let fields = "number,url,title,body,baseRefName,headRefName,headRefOid";
        let out = self.run(&["pr", "view", &pr.to_string(), "--json", fields], None)?;
        serde_json::from_str(&out).with_context(|| format!("parsing gh pr view {pr}"))
    }

    fn fetch(&self, pr: &PrView) -> Result<()> {
        let base = &pr.base_ref_name;
        let n = pr.number;
        let out = Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .arg(format!("+refs/heads/{base}:refs/remotes/origin/{base}"))
            .arg(format!("+refs/pull/{n}/head:refs/lens/pr/{n}"))
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running git fetch")?;
        if !out.status.success() {
            bail!(
                "fetching PR {n} from origin: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The findings earlier lens runs posted on PR `pr`, inline or in a review body.
    fn earlier(&self, pr: u64) -> Result<Earlier> {
        let comments = self.list(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}/comments"))?;
        let reviews = self.list(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews"))?;
        Ok(earlier_from(&comments, &reviews))
    }

    fn payload(&self, review: &Review) -> Value {
        payload(review)
    }

    /// GitHub won't let anyone approve or request changes on their own PR, so posting as its
    /// author falls back to a plain review: the findings still land, and an approval with none
    /// posts nothing.
    fn post(&self, pr: u64, review: &Review) -> Result<Option<String>> {
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews");
        let send = |review: &Value| {
            self.api(
                &["--method", "POST", &endpoint, "--input", "-"],
                Some(&review.to_string()),
            )
        };
        let request = payload(review);
        let out = match send(&request) {
            Err(e) if format!("{e:#}").contains("your own pull request") => {
                if review.event == "APPROVE" && !review.raises_findings() {
                    return Ok(None);
                }
                let mut plain = request.clone();
                plain["event"] = "COMMENT".into();
                send(&plain)?
            }
            out => out?,
        };
        let v: Value = serde_json::from_str(&out).context("parsing the posted review")?;
        Ok(v["html_url"].as_str().map(str::to_string))
    }
}

/// The body of `POST /repos/{owner}/{repo}/pulls/{n}/reviews` for `review`.
pub(crate) fn payload(review: &Review) -> Value {
    let comments: Vec<Value> = review
        .comments
        .iter()
        .map(|c| {
            json!({
                "path": c.path,
                "line": c.line,
                "side": "RIGHT",
                "body": c.body,
            })
        })
        .collect();
    json!({
        "commit_id": review.head,
        "event": review.event,
        "body": review.body,
        "comments": comments,
    })
}
