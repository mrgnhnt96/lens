//! `lens pr view`, `lens pr earlier` and `lens pr post` on a Bitbucket repo, against a fake API
//! (`$LENS_BITBUCKET_API`) on localhost and a local origin standing in for bitbucket.org.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;
use serde_json::{json, Value};

/// The `email:token` a `Basic` `Authorization` header carries.
fn basic_credential(header: &str) -> Option<String> {
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    String::from_utf8(decoded).ok()
}

/// A Bitbucket API stand-in: answers a GET with the first route whose prefix its path starts
/// with, and keeps each request's path and `Authorization` header.
struct Api {
    base: String,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

fn serve(routes: Vec<(String, Value)>) -> Api {
    serve_at(|_| routes)
}

/// [`serve`], with routes that may point back at the server (`base`), as a page's `next` does.
fn serve_at(routes: impl FnOnce(&str) -> Vec<(String, Value)>) -> Api {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = routes(&base);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("authorization") {
                        auth = v.trim().to_string();
                    }
                }
            }
            log.lock().unwrap().push((path.clone(), auth));
            let (status, body) = match routes.iter().find(|(p, _)| path.starts_with(p.as_str())) {
                Some((_, v)) => ("200 OK", v.to_string()),
                None => (
                    "404 Not Found",
                    json!({"error": {"message": format!("no route for {path}")}}).to_string(),
                ),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Api { base, seen }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `<root>/team/repo.git` with `main` and `feat/thing`, a clone of it at `<root>/work`, and the
/// tip of `feat/thing`. The origin's path ends in `team/repo`, so that's the Bitbucket repo.
fn repos(root: &Path) -> String {
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "--quiet", "-b", "main"]);
    fs::write(seed.join("a.txt"), "a\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "--quiet", "-m", "a"]);
    git(&seed, &["switch", "--quiet", "-c", "feat/thing"]);
    fs::write(seed.join("a.txt"), "b\n").unwrap();
    git(&seed, &["commit", "--quiet", "-am", "b"]);
    let tip = git(&seed, &["rev-parse", "HEAD"]);
    for bare in ["team/repo.git", "them/fork.git"] {
        let dir = root.join(bare);
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        git(
            root,
            &[
                "clone",
                "--quiet",
                "--bare",
                seed.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
    }
    git(
        root,
        &[
            "clone",
            "--quiet",
            "-b",
            "main",
            root.join("team/repo.git").to_str().unwrap(),
            root.join("work").to_str().unwrap(),
        ],
    );
    fs::write(
        root.join("work/.lens.toml"),
        "[pr]\nforge = \"bitbucket\"\n",
    )
    .unwrap();
    tip
}

fn pr(source: &str, head: &str) -> Value {
    json!({
        "id": 7,
        "title": "Add a thing",
        "description": "Why it matters",
        "links": {"html": {"href": "https://bitbucket.org/team/repo/pull-requests/7"}},
        "source": {
            "branch": {"name": "feat/thing"},
            "commit": {"hash": &head[..12]},
            "repository": {"full_name": source}
        },
        "destination": {"branch": {"name": "main"}}
    })
}

fn lens(work: &Path, api: &Api, env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lens"));
    cmd.args(args)
        .args(["--repo", work.to_str().unwrap()])
        .env("LENS_BITBUCKET_API", &api.base)
        .env_remove("LENS_BITBUCKET_EMAIL")
        .env_remove("LENS_BITBUCKET_TOKEN")
        .env_remove("LENS_BOT_TOKEN");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

const OWNER: &[(&str, &str)] = &[
    ("LENS_BITBUCKET_EMAIL", "me@x.io"),
    ("LENS_BITBUCKET_TOKEN", "tok"),
];

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn view_writes_the_pr_and_fetches_its_head_and_base() {
    let root = tempfile::tempdir().unwrap();
    let tip = repos(root.path());
    let api = serve(vec![
        (
            "/repositories/team/repo/pullrequests/7?".into(),
            pr("team/repo", &tip),
        ),
        (
            format!("/repositories/team/repo/commit/{}", &tip[..12]),
            json!({"hash": tip}),
        ),
    ]);
    let work = root.path().join("work");
    let out_dir = root.path().join("out");
    let printed = ok(&lens(
        &work,
        &api,
        OWNER,
        &[
            "pr",
            "view",
            "--pr",
            "7",
            "--out",
            out_dir.to_str().unwrap(),
            "--field",
            "headRefOid",
            "--field",
            "baseRefName",
        ],
    ));
    assert_eq!(printed, format!("{tip}\nmain\n"));
    assert_eq!(git(&work, &["rev-parse", "refs/lens/pr/7"]), tip);
    git(
        &work,
        &["rev-parse", "--verify", "refs/remotes/origin/main"],
    );

    let view: Value =
        serde_json::from_str(&fs::read_to_string(out_dir.join("pr.json")).unwrap()).unwrap();
    assert_eq!(view["headRefName"], "feat/thing");
    assert_eq!(
        view["url"],
        "https://bitbucket.org/team/repo/pull-requests/7"
    );
    assert!(view.get("headRepo").is_none(), "{view}");
    assert_eq!(
        fs::read_to_string(out_dir.join("intent.md")).unwrap(),
        "# Add a thing\n\nWhy it matters\n"
    );
    let seen = api.seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|(_, a)| basic_credential(a).as_deref() == Some("me@x.io:tok")),
        "{seen:?}"
    );
}

#[test]
fn view_of_a_fork_pr_fetches_the_head_from_the_fork() {
    let root = tempfile::tempdir().unwrap();
    let tip = repos(root.path());
    git(
        &root.path().join("team/repo.git"),
        &["branch", "-D", "feat/thing"],
    );
    let api = serve(vec![
        (
            "/repositories/team/repo/pullrequests/7?".into(),
            pr("them/fork", &tip),
        ),
        (
            format!("/repositories/them/fork/commit/{}", &tip[..12]),
            json!({"hash": tip}),
        ),
    ]);
    let work = root.path().join("work");
    let printed = ok(&lens(&work, &api, OWNER, &["pr", "view", "--pr", "7"]));
    let view: Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(view["headRepo"], "them/fork");
    assert_eq!(git(&work, &["rev-parse", "refs/lens/pr/7"]), tip);
}

#[test]
fn view_stops_when_the_branch_moved_since_bitbucket_was_read() {
    let root = tempfile::tempdir().unwrap();
    repos(root.path());
    let work = root.path().join("work");
    let tip = git(&work, &["rev-parse", "HEAD"]);
    let api = serve(vec![
        (
            "/repositories/team/repo/pullrequests/7?".into(),
            pr("team/repo", &tip),
        ),
        (
            format!("/repositories/team/repo/commit/{}", &tip[..12]),
            json!({"hash": tip}),
        ),
    ]);
    let out = lens(&work, &api, OWNER, &["pr", "view", "--pr", "7"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("run again"), "{err}");
}

#[test]
fn view_without_credentials_says_which_to_set() {
    let root = tempfile::tempdir().unwrap();
    repos(root.path());
    let api = serve(vec![]);
    let out = lens(
        &root.path().join("work"),
        &api,
        &[],
        &["pr", "view", "--pr", "7"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("LENS_BITBUCKET_EMAIL") && err.contains("LENS_BITBUCKET_TOKEN"),
        "{err}"
    );
    assert!(api.seen.lock().unwrap().is_empty());
}

#[test]
fn an_api_error_carries_bitbucket_s_message() {
    let root = tempfile::tempdir().unwrap();
    repos(root.path());
    let api = serve(vec![]);
    let out = lens(
        &root.path().join("work"),
        &api,
        OWNER,
        &["pr", "view", "--pr", "7"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("404") && err.contains("no route for"), "{err}");
}

#[test]
fn earlier_pages_through_comments_as_the_bot() {
    let root = tempfile::tempdir().unwrap();
    repos(root.path());
    let work = root.path().join("work");
    fs::write(
        work.join(".lens.toml"),
        "[pr]\nforge = \"bitbucket\"\npost_as = \"bot\"\n",
    )
    .unwrap();
    let api = serve_at(|base| {
        vec![
            (
                "/repositories/team/repo/pullrequests/7/comments?".into(),
                json!({
                    "values": [
                        {"id": 3, "content": {"raw": "x\n\n[//]: # (lens:finding key=aaaa)"},
                         "inline": {"path": "src/a.rs", "to": 12}, "links": {"html": {"href": "u3"}}},
                        {"id": 4, "deleted": true, "content": {"raw": "[//]: # (lens:finding key=dead)"},
                         "inline": {"path": "src/a.rs", "to": 40}}
                    ],
                    "next": format!("{base}/page2")
                }),
            ),
            (
                "/page2".into(),
                json!({"values": [
                    {"id": 9, "content": {"raw": "- y [//]: # (lens:finding key=bbbb)\n\n[//]: # (lens:review head=1 verdict=REQUEST_CHANGES)"},
                     "links": {"html": {"href": "u9"}}}
                ]}),
            ),
        ]
    });
    let printed = ok(&lens(
        &work,
        &api,
        &[("LENS_BOT_TOKEN", "bot-tok")],
        &["pr", "earlier", "--pr", "7"],
    ));
    let earlier: Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(earlier["reviews"], 1);
    assert_eq!(earlier["verdict"], "REQUEST_CHANGES");
    let keys: Vec<_> = earlier["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["key"].as_str().unwrap().to_string(), f["line"].clone()))
        .collect();
    assert_eq!(
        keys,
        [
            ("aaaa".to_string(), json!(12)),
            ("bbbb".to_string(), Value::Null)
        ]
    );
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen.iter().all(|(_, a)| a == "Bearer bot-tok"), "{seen:?}");
}

const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {
+    new();
 }
";

#[test]
fn post_shows_the_review_on_a_dry_run_and_says_it_cannot_post_yet() {
    let root = tempfile::tempdir().unwrap();
    repos(root.path());
    let work = root.path().join("work");
    let api = serve(vec![(
        "/repositories/team/repo/pullrequests/7/comments?".into(),
        json!({"values": []}),
    )]);
    let report = root.path().join("pr-7.json");
    let diff = root.path().join("diff.patch");
    fs::write(&diff, PATCH).unwrap();
    fs::write(
        &report,
        json!({
            "version": 1,
            "pass": "pr",
            "pr": {"number": 7, "url": "https://bitbucket.org/team/repo/pull-requests/7"},
            "head": "0123456789abcdef",
            "base": "origin/main",
            "risk": "low",
            "profiles": [{"name": "correctness", "ran": true}],
            "findings": [
                {"id": "F1", "profiles": ["correctness"], "file": "src/a.rs", "line": 2,
                 "severity": "major", "title": "new() panics", "failureScenario": "a() panics"}
            ],
            "dropped": []
        })
        .to_string(),
    )
    .unwrap();
    let args = [
        "pr",
        "post",
        "--pr",
        "7",
        "--report",
        report.to_str().unwrap(),
        "--diff",
        diff.to_str().unwrap(),
    ];
    let dry: Value = serde_json::from_str(&ok(&lens(
        &work,
        &api,
        OWNER,
        &[&args[..], &["--dry-run"]].concat(),
    )))
    .unwrap();
    assert_eq!(dry["event"], "REQUEST_CHANGES");
    assert_eq!(dry["payload"]["verdict"], "REQUEST_CHANGES");
    assert!(
        dry["payload"]["summary"]
            .as_str()
            .unwrap()
            .contains("new() panics"),
        "{dry}"
    );

    let out = lens(&work, &api, OWNER, &args);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("can't post on Bitbucket"), "{err}");
}
