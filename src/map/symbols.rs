//! Spots symbol definitions in a line of source, one language at a time. These are line-level
//! patterns, not a parser: they find the usual shapes (`fn x`, `def x`, `class X`, ...) and miss
//! the unusual ones. Precise providers (rust-analyzer and the like) plug in through `.lens.toml`.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

/// A definition found on a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    pub kind: String,
}

/// Words a pattern can mistake for a name or a return type.
const KEYWORDS: &[&str] = &[
    "if", "for", "while", "switch", "match", "return", "catch", "else", "new", "sizeof", "super",
    "this", "self", "function", "await", "throw", "case", "yield", "assert", "do", "try", "when",
    "with", "elif", "unless", "until", "loop", "in", "is", "as", "delete", "typeof", "print",
];

const VIS: &str = r"(?:pub(?:\([^)]*\))?\s+)?";
const MODS: &str = r"(?:(?:public|private|protected|internal|static|final|abstract|sealed|partial|open|data|export|inline|virtual|override|async|const|readonly|synchronized|extern|unsafe)\s+)*";

/// `(language, pattern, kind)`. A pattern captures the name as `n` and may capture the kind as
/// `k`, which wins over the listed one.
fn table() -> Vec<(&'static str, String, &'static str)> {
    let c_like_type = format!(
        r"^\s*{MODS}(?P<k>class|struct|interface|enum|protocol|object|record|trait|namespace)\s+(?P<n>\w+)"
    );
    let c_like_fn = format!(
        r"^\s*{MODS}(?:[\w:<>,\*&\[\]\.?]+\s+)+[\*&]*(?P<n>[A-Za-z_]\w*)\s*\([^;]*\)\s*(?:const\s*)?(?:noexcept\s*)?(?:throws\s+[\w.,\s]+)?\{{?\s*$"
    );
    let mut t = vec![
        ("rust", format!(r"^\s*{VIS}(?:(?:const|async|unsafe|extern(?:\s+\x22[^\x22]*\x22)?)\s+)*fn\s+(?P<n>\w+)"), "function"),
        ("rust", format!(r"^\s*{VIS}(?P<k>struct|enum|trait|type|union)\s+(?P<n>\w+)"), "type"),
        ("rust", format!(r"^\s*{VIS}(?:const|static)\s+(?:mut\s+)?(?P<n>\w+)\s*:"), "constant"),
        ("rust", r"^\s*macro_rules!\s*(?P<n>\w+)".into(), "macro"),
        ("python", r"^\s*(?:async\s+)?def\s+(?P<n>\w+)".into(), "function"),
        ("python", r"^\s*class\s+(?P<n>\w+)".into(), "class"),
        ("go", r"^func\s+(?:\([^)]*\)\s*)?(?P<n>\w+)".into(), "function"),
        ("go", r"^\s*type\s+(?P<n>\w+)".into(), "type"),
        ("dart", r"^\s*(?:(?:abstract|sealed|base|final|interface)\s+)*(?P<k>class|mixin|enum|extension|typedef)\s+(?P<n>\w+)".into(), "type"),
        ("dart", r"^\s*(?:static\s+)?(?:[\w<>?,\s]+\s+)?get\s+(?P<n>\w+)".into(), "getter"),
        ("dart", r"^\s*(?:(?:static|external|factory|const|final|late|override)\s+)*(?:[\w<>?,\[\]\.]+\s+)?(?P<n>[A-Za-z_][\w\.]*)\s*(?:<[^>]*>)?\s*\([^;]*$".into(), "function"),
        ("ruby", r"^\s*def\s+(?:self\.)?(?P<n>\w+[?!=]?)".into(), "method"),
        ("ruby", r"^\s*(?P<k>class|module)\s+(?P<n>[\w:]+)".into(), "type"),
        ("shell", r"^\s*(?:function\s+)?(?P<n>[\w-]+)\s*\(\)".into(), "function"),
        ("shell", r"^\s*function\s+(?P<n>[\w-]+)".into(), "function"),
        ("kotlin", r"\bfun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?(?P<n>\w+)".into(), "function"),
        ("swift", r"\bfunc\s+(?P<n>\w+)".into(), "function"),
        ("php", r"\bfunction\s+&?(?P<n>\w+)".into(), "function"),
    ];
    for lang in ["javascript", "typescript"] {
        t.extend([
            (lang, r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*(?P<n>[\w$]+)".to_string(), "function"),
            (lang, r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?P<k>class|interface|enum|type)\s+(?P<n>[\w$]+)".into(), "type"),
            (lang, r"^\s*(?:export\s+)?(?:const|let|var)\s+(?P<n>[\w$]+)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[\w$]+\s*=>)".into(), "function"),
            (lang, r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*(?P<n>[\w$]+)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::[^{]+)?\{\s*$".into(), "method"),
        ]);
    }
    for lang in [
        "java",
        "kotlin",
        "swift",
        "csharp",
        "c",
        "cpp",
        "objective-c",
        "php",
    ] {
        t.push((lang, c_like_type.clone(), "type"));
        if !matches!(lang, "kotlin" | "swift" | "php") {
            t.push((lang, c_like_fn.clone(), "function"));
        }
    }
    t
}

fn patterns() -> &'static HashMap<&'static str, Vec<(Regex, &'static str)>> {
    static PATTERNS: OnceLock<HashMap<&'static str, Vec<(Regex, &'static str)>>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut map: HashMap<_, Vec<_>> = HashMap::new();
        for (lang, pattern, kind) in table() {
            let re = Regex::new(&pattern).unwrap_or_else(|e| panic!("{lang} pattern: {e}"));
            map.entry(lang).or_default().push((re, kind));
        }
        map
    })
}

/// The symbol `line` defines, if it looks like a definition in `language`.
pub fn definition(language: &str, line: &str) -> Option<Def> {
    let trimmed = line.trim_start();
    let first = trimmed
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    if KEYWORDS.contains(&first)
        || trimmed.starts_with("//")
        || trimmed.starts_with('#') && language != "rust"
    {
        return None;
    }
    for (re, kind) in patterns().get(language)? {
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let name = caps["n"].rsplit('.').next().unwrap_or("").to_string();
        if name.is_empty() || KEYWORDS.contains(&name.as_str()) {
            continue;
        }
        let kind = caps.name("k").map_or(*kind, |k| k.as_str());
        return Some(Def {
            name,
            kind: kind.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(lang: &str, line: &str) -> Option<(String, String)> {
        definition(lang, line).map(|d| (d.name, d.kind))
    }

    fn some(name: &str, kind: &str) -> Option<(String, String)> {
        Some((name.to_string(), kind.to_string()))
    }

    #[test]
    fn rust_definitions() {
        assert_eq!(
            def("rust", "pub(crate) async fn load(x: u8) {"),
            some("load", "function")
        );
        assert_eq!(def("rust", "    fn helper<T>("), some("helper", "function"));
        assert_eq!(def("rust", "pub struct Token {"), some("Token", "struct"));
        assert_eq!(
            def("rust", "const MAX: usize = 3;"),
            some("MAX", "constant")
        );
        assert_eq!(def("rust", "    let x = load(1);"), None);
        assert_eq!(def("rust", "    if load(1) {"), None);
        // Module names are mostly common words (`tests`, `map`); grepping them is noise.
        assert_eq!(def("rust", "mod tests {"), None);
    }

    #[test]
    fn python_go_and_ts_definitions() {
        assert_eq!(
            def("python", "    async def fetch(self):"),
            some("fetch", "function")
        );
        assert_eq!(def("python", "class Thing(Base):"), some("Thing", "class"));
        assert_eq!(
            def("go", "func (s *Server) Serve(l net.Listener) error {"),
            some("Serve", "function")
        );
        assert_eq!(
            def("typescript", "export const parse = (s: string) => {"),
            some("parse", "function")
        );
        assert_eq!(
            def("typescript", "export interface Opts {"),
            some("Opts", "interface")
        );
        assert_eq!(
            def("typescript", "  async load(id: string): Promise<void> {"),
            some("load", "method")
        );
        assert_eq!(def("typescript", "  if (x) {"), None);
    }

    #[test]
    fn dart_and_java_definitions() {
        assert_eq!(
            def("dart", "class LoginBloc extends Bloc<E, S> {"),
            some("LoginBloc", "class")
        );
        assert_eq!(
            def("dart", "  Future<void> submit(String email) async {"),
            some("submit", "function")
        );
        assert_eq!(
            def("dart", "  int get count => _count;"),
            some("count", "getter")
        );
        assert_eq!(def("dart", "    return compute(x);"), None);
        assert_eq!(
            def("java", "  public static int add(int a, int b) {"),
            some("add", "function")
        );
    }

    #[test]
    fn unknown_language_has_no_definitions() {
        assert_eq!(def("markdown", "# fn heading"), None);
    }
}
