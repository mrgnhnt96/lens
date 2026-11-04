//! [`Forge`] for GitHub, through the `gh` CLI.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{Child, Forge, PrView};
use crate::config::{PostAs, PrConfig};
use crate::outcome::{self, Discussion};
use crate::pr::{earlier_from, Earlier, Markup, Review};

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

    fn children(&self, branch: &str) -> Result<Vec<Child>> {
        let endpoint = format!(
            "repos/{{owner}}/{{repo}}/pulls?base={}&state=all&per_page=100",
            query_escape(branch)
        );
        Ok(children_from(&self.list(&endpoint)?))
    }

    /// One GraphQL query per [`outcome::GITHUB_BATCH`] PRs, then one fetch of all their heads.
    fn discussions(&self, prs: &[u64]) -> Result<BTreeMap<u64, Discussion>> {
        let mut all = BTreeMap::new();
        for batch in prs.chunks(outcome::GITHUB_BATCH) {
            let query = format!("query={}", outcome::github_query(batch));
            let owner = ["-F", "owner={owner}", "-F", "name={repo}"];
            let text = self.api(&[&["graphql"][..], &owner, &["-f", &query]].concat(), None)?;
            let answer: Value =
                serde_json::from_str(&text).context("parsing gh api graphql's answer")?;
            all.extend(outcome::github_discussions(&answer, batch));
        }
        if all.is_empty() {
            return Ok(all);
        }
        // A missing head only leaves a finding's code uncompared, so a failed fetch is no error.
        let _ = Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .args(
                all.keys()
                    .map(|n| format!("+refs/pull/{n}/head:refs/lens/pr/{n}")),
            )
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(all)
    }

    fn markup(&self) -> Markup {
        Markup::GitHub
    }

    fn payload(&self, _pr: u64, review: &Review) -> Value {
        payload(review)
    }

    /// GitHub won't let anyone approve or request changes on their own PR. lens never posts a
    /// review without a verdict, so posting as its author posts nothing: a clean approval quietly,
    /// a review with findings as an error that says to post as a bot.
    fn post(&self, pr: u64, review: &Review) -> Result<Option<String>> {
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews");
        let out = match self.api(
            &["--method", "POST", &endpoint, "--input", "-"],
            Some(&payload(review).to_string()),
        ) {
            Err(e) if format!("{e:#}").contains("your own pull request") => {
                if review.event == "APPROVE" && !review.raises_findings() {
                    return Ok(None);
                }
                bail!(
                    "GitHub wouldn't take lens's verdict on PR {pr}, so nothing was posted (lens never posts a review without one). On your own PR, post as a bot (post_as = \"bot\") to give it one"
                );
            }
            out => out?,
        };
        let v: Value = serde_json::from_str(&out).context("parsing the posted review")?;
        Ok(v["html_url"].as_str().map(str::to_string))
    }
}

/// The open and merged PRs among `pulls`, as `GET /repos/{owner}/{repo}/pulls` lists them.
fn children_from(pulls: &[Value]) -> Vec<Child> {
    pulls
        .iter()
        .filter_map(|p| {
            let merged = p["merged_at"].is_string();
            if !merged && p["state"].as_str() != Some("open") {
                return None;
            }
            Some(Child {
                number: p["number"].as_u64()?,
                url: p["html_url"].as_str().unwrap_or_default().to_string(),
                merged,
                merge_commit: p["merge_commit_sha"]
                    .as_str()
                    .filter(|_| merged)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// `text` percent-encoded for a URL's query string.
pub(crate) fn query_escape(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `gh` that logs its arguments to `log` and prints `out`.
    fn fake_gh(dir: &Path, out: &str) -> GitHub {
        let script = dir.join("gh");
        std::fs::write(dir.join("out"), out).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{0}/log'\ncat '{0}/out'\n",
                dir.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        GitHub {
            program: script,
            repo: dir.to_path_buf(),
            token: None,
        }
    }

    #[test]
    fn children_lists_open_and_merged_prs_on_the_branch_and_drops_closed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let pages = [
            json!([
                {"number": 8, "html_url": "u8", "state": "closed",
                 "merged_at": "2026-11-01T00:00:00Z", "merge_commit_sha": "m8"},
                {"number": 9, "html_url": "u9", "state": "open",
                 "merged_at": null, "merge_commit_sha": "test-merge"}
            ]),
            json!([
                {"number": 10, "html_url": "u10", "state": "closed",
                 "merged_at": null, "merge_commit_sha": "m10"}
            ]),
        ];
        let gh = fake_gh(dir.path(), &format!("{}\n{}", pages[0], pages[1]));
        let children = gh.children("feat/a b").unwrap();
        assert_eq!(
            children,
            vec![
                Child {
                    number: 8,
                    url: "u8".into(),
                    merged: true,
                    merge_commit: Some("m8".into()),
                },
                Child {
                    number: 9,
                    url: "u9".into(),
                    merged: false,
                    merge_commit: None,
                },
            ]
        );
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(
            log.trim(),
            "api --paginate repos/{owner}/{repo}/pulls?base=feat%2Fa%20b&state=all&per_page=100"
        );
    }
}
