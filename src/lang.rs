//! Maps changed paths to the language names profiles list under `languages`.

use std::path::Path;

/// The language a path is written in, judged by its extension (or file name for a few
/// well-known files). `None` when lens doesn't recognise it.
pub fn language_of(path: &str) -> Option<&'static str> {
    let path = Path::new(path);
    let name = path.file_name()?.to_str()?;
    match name {
        "Dockerfile" => return Some("dockerfile"),
        "Makefile" => return Some("make"),
        _ => {}
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "dart" => "dart",
        "py" | "pyi" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "m" | "mm" => "objective-c",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "shell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "arb" => "arb",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_extensions_case_insensitively() {
        assert_eq!(language_of("lib/main.dart"), Some("dart"));
        assert_eq!(language_of("src/Lib.RS"), Some("rust"));
        assert_eq!(language_of("web/app.tsx"), Some("typescript"));
        assert_eq!(language_of("Dockerfile"), Some("dockerfile"));
    }

    #[test]
    fn unknown_or_extensionless_paths_have_no_language() {
        assert_eq!(language_of("LICENSE"), None);
        assert_eq!(language_of("data/blob.xyz"), None);
    }
}
