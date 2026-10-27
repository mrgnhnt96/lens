//! `prepare.sh`'s diff.patch, against a throwaway repo: `git apply --cached` of the patch onto the
//! merge base has to rebuild the working tree exactly, untracked binaries included, and the
//! script writes only to its state folder, never to the checkout.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn git(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn write(dir: &Path, path: &str, bytes: &[u8]) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The working tree as a tree object, the way tb sees it: untracked files in, ignored ones out.
fn tree_now(dir: &Path, index: &Path) -> String {
    let env = [("GIT_INDEX_FILE", index)];
    git(dir, &env, &["read-tree", "HEAD"]);
    git(dir, &env, &["add", "-A", "."]);
    git(dir, &env, &["write-tree"])
}

#[test]
fn diff_patch_replays_onto_the_merge_base_with_untracked_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &[], &["init", "-q", "-b", "main"]);
    write(d, "a.txt", b"a\n");
    write(d, "logo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);
    git(d, &[], &["add", "."]);
    git(d, &[], &["commit", "-qm", "init"]);
    git(d, &[], &["switch", "-qc", "feature"]);
    write(d, "a.txt", b"a\nb\n");
    git(d, &[], &["commit", "-qam", "two"]);
    // Uncommitted, untracked and binary: a tracked binary edited, a new binary, a new text file.
    write(d, "logo.png", &[0x89, b'P', b'N', b'G', 0, 9, 9, 9]);
    write(d, "promo/frame.png", &[0, 0xff, 0, 0xfe, 0, 1]);
    write(d, "notes.txt", b"new\n");

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/prepare.sh");
    let state = tempfile::tempdir().unwrap();
    let before = git(d, &[], &["status", "--porcelain", "--ignored"]);
    let out = Command::new("bash")
        .arg(script)
        .arg("main")
        .current_dir(d)
        .env("LENS", "true")
        .env("LENS_STATE", state.path())
        // Keep tb out of it: the summary's task field doesn't matter here.
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "prepare.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["dirty"], true);
    assert_eq!(
        summary["work"],
        state.path().join("feature").to_str().unwrap()
    );
    assert_eq!(git(d, &[], &["status", "--porcelain", "--ignored"]), before);
    let patch = Path::new(summary["work"].as_str().unwrap()).join("diff.patch");
    let text = fs::read_to_string(&patch).unwrap();
    assert!(text.contains("promo/frame.png"), "{text}");
    assert!(!text.contains(".review/"), "{text}");

    let tmp = tempfile::tempdir().unwrap();
    let replay = tmp.path().join("replay");
    let env = [("GIT_INDEX_FILE", replay.as_path())];
    git(
        d,
        &env,
        &["read-tree", summary["mergeBase"].as_str().unwrap()],
    );
    git(
        d,
        &env,
        &[
            "apply",
            "--cached",
            "--allow-empty",
            patch.to_str().unwrap(),
        ],
    );
    let reviewed = git(d, &env, &["write-tree"]);

    assert_eq!(reviewed, tree_now(d, &tmp.path().join("now")));
}
