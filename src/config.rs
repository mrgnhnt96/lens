//! A repo's `.lens.toml`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const FILE: &str = ".lens.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub map: MapConfig,
}

/// `[map]`: who builds the change map.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapConfig {
    /// `"grep"` (the built-in default), a program, or a program and its arguments.
    pub provider: Option<ProviderSpec>,
    /// How the provider gets the changed files.
    #[serde(default)]
    pub input: Input,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ProviderSpec {
    Program(String),
    Argv(Vec<String>),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// One path per line on stdin.
    #[default]
    Stdin,
    /// Appended to the provider's arguments.
    Args,
}

/// Reads `<root>/.lens.toml`; a repo without one gets the defaults.
pub fn load(root: &Path) -> Result<Config> {
    let path = root.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_is_a_program_or_an_argv() {
        let c: Config = toml::from_str("[map]\nprovider = \"tools/map\"\n").unwrap();
        assert_eq!(
            c.map.provider,
            Some(ProviderSpec::Program("tools/map".into()))
        );
        assert_eq!(c.map.input, Input::Stdin);
        let c: Config =
            toml::from_str("[map]\nprovider = [\"ra-map\", \"--fast\"]\ninput = \"args\"\n")
                .unwrap();
        assert_eq!(
            c.map.provider,
            Some(ProviderSpec::Argv(vec!["ra-map".into(), "--fast".into()]))
        );
        assert_eq!(c.map.input, Input::Args);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("[map]\nprovidr = \"x\"\n").is_err());
    }
}
