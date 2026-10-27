//! lens's own files, compiled into the binary by `build.rs`: the agents' prompts (`prompts/`), the
//! default profiles (`profiles/`) and the prepare scripts (`scripts/`). The first run of a build
//! writes them to a folder named after their hash under the user's cache, so every later run, and
//! every build with the same files, reuses it.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use anyhow::{Context, Result};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The folder holding lens's files: `$LENS_ASSETS` when set (a checkout, while working on lens),
/// otherwise the embedded files written out under `$XDG_CACHE_HOME/lens` or `~/.cache/lens`.
pub fn root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("LENS_ASSETS") {
        return Ok(dir.into());
    }
    let mut hasher = DefaultHasher::new();
    FILES.hash(&mut hasher);
    let base = cache()?;
    let dir = base.join("lens").join(format!("{:016x}", hasher.finish()));
    if dir.join(".complete").is_file() {
        return Ok(dir);
    }
    // Write to a fresh folder and move it into place, so a run never sees half the files.
    let tmp = base
        .join("lens")
        .join(format!(".tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    for (path, bytes) in FILES {
        let file = tmp.join(path);
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(&file, bytes).with_context(|| format!("writing {}", file.display()))?;
        #[cfg(unix)]
        if path.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
        }
    }
    fs::write(tmp.join(".complete"), "")?;
    if fs::rename(&tmp, &dir).is_err() {
        // Another run got there first.
        let _ = fs::remove_dir_all(&tmp);
    }
    Ok(dir)
}

/// The user's cache: `$XDG_CACHE_HOME`, else `~/.cache`.
pub fn cache() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?).join(".cache"),
    })
}

/// Where a review run by hand keeps its state: one folder per repo under the cache's
/// `lens/state`, never inside the repo.
pub fn state_root() -> Result<PathBuf> {
    Ok(cache()?.join("lens").join("state"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn embeds_the_prompts_profiles_and_scripts() {
        let has = |p: &str| super::FILES.iter().any(|(path, _)| *path == p);
        assert!(has("prompts/reviewer.md"));
        assert!(has("profiles/correctness/profile.md"));
        assert!(has("scripts/prepare.sh"));
        assert!(has("scripts/prepare-pr.sh"));
    }
}
