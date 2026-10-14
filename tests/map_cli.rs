//! `lens map --base` end to end, against a throwaway fixture repo.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
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
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const PARSER: &str = "\
pub fn parse_token(s: &str) -> Option<u32> {
    s.parse().ok()
}

pub fn old_helper() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        assert_eq!(super::parse_token(\"1\"), Some(1));
    }
}
";

const SERVER: &str = "\
use crate::parser::parse_token;

pub fn handle(raw: &str) -> u32 {
    parse_token(raw).unwrap_or(0) + crate::parser::old_helper()
}
";

/// A small Rust crate on `main`, with a `feature` branch checked out.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "src/parser.rs", PARSER);
    write(d, "src/server.rs", SERVER);
    write(d, "src/lib.rs", "pub mod parser;\npub mod server;\n");
    write(
        d,
        "tests/parser_test.rs",
        "#[test]\nfn t() { assert!(lens_fixture::parser::parse_token(\"2\").is_some()); }\n",
    );
    write(d, "README.md", "fixture\n");
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "init"]);
    git(d, &["switch", "-qc", "feature"]);
    dir
}

fn lens_map(repo: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lens"))
        .args(["map", "--base", "main", "--repo"])
        .arg(repo)
        .args(extra)
        .output()
        .unwrap()
}

fn map_ok(repo: &Path, extra: &[&str]) -> Value {
    let out = lens_map(repo, extra);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn symbol<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no symbol {name} in {:#}", v["symbols"]))
}

fn files_of(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|l| l["file"].as_str().unwrap())
        .collect()
}

#[test]
fn grep_provider_maps_symbols_callers_and_tests() {
    let dir = fixture();
    let d = dir.path();
    // Edit inside parse_token's body, drop old_helper, add a new untracked module.
    let edited = PARSER
        .replace("s.parse().ok()", "s.trim().parse().ok()")
        .replace("pub fn old_helper() -> u32 {\n    1\n}\n\n", "");
    write(d, "src/parser.rs", &edited);
    write(d, "src/extra.rs", "pub fn brand_new() -> u8 {\n    7\n}\n");

    let v = map_ok(d, &[]);
    assert_eq!(v["version"], 1);
    assert_eq!(v["provider"], "grep");
    assert_eq!(v["base"], "main");
    assert_eq!(
        v["changedFiles"],
        json!([
            {"path": "src/extra.rs", "status": "added", "language": "rust"},
            {"path": "src/parser.rs", "status": "modified", "language": "rust"},
        ])
    );

    let parse = symbol(&v, "parse_token");
    assert_eq!(parse["change"], "modified");
    assert_eq!(parse["kind"], "function");
    assert_eq!(
        (parse["file"].as_str(), parse["line"].as_u64()),
        (Some("src/parser.rs"), Some(1))
    );
    assert_eq!(
        files_of(&parse["callers"]),
        ["src/server.rs", "src/server.rs"]
    );
    let tests = files_of(&parse["tests"]);
    assert!(
        tests.contains(&"src/parser.rs"),
        "inline #[cfg(test)] use: {tests:?}"
    );
    assert!(tests.contains(&"tests/parser_test.rs"), "{tests:?}");

    let helper = symbol(&v, "old_helper");
    assert_eq!(helper["change"], "removed");
    assert_eq!(helper["line"], 5, "line at the merge base");
    assert_eq!(files_of(&helper["callers"]), ["src/server.rs"]);

    let new = symbol(&v, "brand_new");
    assert_eq!(new["change"], "added");
    assert_eq!(new["callers"], json!([]));

    let tests = v["tests"].as_array().unwrap();
    let t = tests
        .iter()
        .find(|t| t["file"] == "tests/parser_test.rs")
        .expect("covering test");
    assert_eq!(t["symbols"], json!(["parse_token"]));
    assert_eq!(t["namedAfter"], json!(["src/parser.rs"]));
}

#[test]
fn no_change_is_an_empty_map() {
    let dir = fixture();
    let v = map_ok(dir.path(), &[]);
    assert_eq!(v["changedFiles"], json!([]));
    assert_eq!(v["symbols"], json!([]));
    assert_eq!(v["tests"], json!([]));
}

/// A provider script that echoes what it was given, so the test sees the contract's inputs.
const ECHO_PROVIDER: &str = r#"#!/bin/sh
files=$(cat | tr '\n' ' ')
printf '{"version":1,"symbols":[{"name":"%s","kind":"x","file":"%s","line":1,"change":"modified","callers":[{"file":"%s","line":2}]}],"tests":[]}\n' \
  "$LENS_BASE" "$files" "$*"
"#;

fn install_provider(d: &Path, path: &str, script: &str) {
    write(d, path, script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.join(path), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn repo_provider_from_lens_toml_gets_files_on_stdin() {
    let dir = fixture();
    let d = dir.path();
    install_provider(d, "tools/map.sh", ECHO_PROVIDER);
    write(
        d,
        ".lens.toml",
        "[map]\nprovider = [\"tools/map.sh\", \"--flag\"]\n",
    );
    write(d, "src/server.rs", &format!("{SERVER}// touched\n"));

    let v = map_ok(d, &[]);
    assert_eq!(v["provider"], "tools/map.sh --flag");
    let s = &v["symbols"][0];
    assert_eq!(s["name"], "main", "LENS_BASE");
    // Untracked .lens.toml and tools/map.sh are part of the change too.
    assert_eq!(s["file"], ".lens.toml src/server.rs tools/map.sh ");
    assert_eq!(s["callers"][0]["file"], "--flag");
    // lens fills changedFiles itself.
    assert_eq!(v["changedFiles"].as_array().unwrap().len(), 3);
}

#[test]
fn repo_provider_can_take_files_as_args() {
    let dir = fixture();
    let d = dir.path();
    install_provider(d, "tools/map.sh", ECHO_PROVIDER);
    write(
        d,
        ".lens.toml",
        "[map]\nprovider = \"tools/map.sh\"\ninput = \"args\"\n",
    );

    let v = map_ok(d, &[]);
    assert_eq!(v["symbols"][0]["file"], "");
    assert_eq!(
        v["symbols"][0]["callers"][0]["file"],
        ".lens.toml tools/map.sh"
    );
}

#[test]
fn provider_flag_overrides_lens_toml() {
    let dir = fixture();
    let d = dir.path();
    write(d, ".lens.toml", "[map]\nprovider = \"tools/missing\"\n");
    let v = map_ok(d, &["--provider", "grep"]);
    assert_eq!(v["provider"], "grep");
}

#[test]
fn failing_or_garbled_provider_is_an_error() {
    let dir = fixture();
    let d = dir.path();
    install_provider(d, "tools/fail.sh", "#!/bin/sh\necho boom >&2\nexit 3\n");
    install_provider(d, "tools/junk.sh", "#!/bin/sh\necho '{\"version\": 9}'\n");

    let out = lens_map(d, &["--provider", "tools/fail.sh"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));

    let out = lens_map(d, &["--provider", "tools/junk.sh"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("version 9"));
}
