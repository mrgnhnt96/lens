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
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub review: ReviewConfig,
}

/// `[review]`: how `lens review` runs its agents.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReviewConfig {
    /// A change of at most this many lines gets the correctness reviewer only.
    pub small_lines: u32,
    /// Agents running at once.
    pub parallel: usize,
    /// Minutes before an agent is stopped.
    pub timeout_mins: u64,
    /// The fixer's `claude --permission-mode`: it edits the tree and runs tests, headless.
    pub fixer_permission_mode: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            small_lines: 40,
            parallel: 4,
            timeout_mins: 20,
            fixer_permission_mode: "bypassPermissions".into(),
        }
    }
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

/// `[pr]`: how the PR pass posts its findings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrConfig {
    /// Whose account the comments come from.
    #[serde(default)]
    pub post_as: PostAs,
    /// The environment variable holding the bot's token, for `post_as = "bot"`.
    /// Defaults to `LENS_BOT_TOKEN`.
    pub bot_token_env: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PostAs {
    /// The account `gh` is logged in as.
    #[default]
    Owner,
    /// A bot account, through the token in `bot_token_env`.
    Bot,
}

impl PrConfig {
    pub const DEFAULT_BOT_TOKEN_ENV: &'static str = "LENS_BOT_TOKEN";

    pub fn bot_token_env(&self) -> &str {
        self.bot_token_env
            .as_deref()
            .unwrap_or(Self::DEFAULT_BOT_TOKEN_ENV)
    }
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
    fn pr_posts_as_the_owner_unless_set_to_bot() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.pr.post_as, PostAs::Owner);
        assert_eq!(c.pr.bot_token_env(), "LENS_BOT_TOKEN");
        let c: Config =
            toml::from_str("[pr]\npost_as = \"bot\"\nbot_token_env = \"REVIEW_BOT\"\n").unwrap();
        assert_eq!(c.pr.post_as, PostAs::Bot);
        assert_eq!(c.pr.bot_token_env(), "REVIEW_BOT");
        assert!(toml::from_str::<Config>("[pr]\npost_as = \"me\"\n").is_err());
    }

    #[test]
    fn review_settings_default_and_override() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.review.small_lines, 40);
        assert_eq!(c.review.fixer_permission_mode, "bypassPermissions");
        let c: Config = toml::from_str("[review]\nsmall_lines = 10\n").unwrap();
        assert_eq!(c.review.small_lines, 10);
        assert_eq!(c.review.parallel, 4);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("[map]\nprovidr = \"x\"\n").is_err());
    }
}
