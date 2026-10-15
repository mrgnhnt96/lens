//! Embeds lens's own files (the agents' prompts, the default profiles and the prepare scripts) in
//! the binary, so `lens` runs on its own. `src/assets.rs` writes them out at runtime.

use std::fmt::Write;
use std::path::Path;
use std::{env, fs};

const DIRS: &[&str] = &["prompts", "profiles", "scripts"];

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in DIRS {
        println!("cargo:rerun-if-changed={dir}");
        walk(&root.join(dir), &mut files);
    }
    files.sort();
    let mut out = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for path in &files {
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        writeln!(
            out,
            "    ({rel:?}, include_bytes!({:?})),",
            path.to_str().unwrap()
        )
        .unwrap();
    }
    out.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("assets.rs"),
        out,
    )
    .unwrap();
}

fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, files);
        } else {
            files.push(path);
        }
    }
}
