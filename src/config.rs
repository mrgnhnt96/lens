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
    #[serde(default)]
    pub floor: FloorConfig,
    #[serde(default)]
    pub sweep: SweepConfig,
}

/// `[sweep]`: when `lens sweep` runs and how it splits what it reviews.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SweepConfig {
    /// How long after the last sweep the next one is due: a span like `6h`, `90m` or `1d`.
    pub every: String,
    /// A sweep is due early once this many lines changed since the last one.
    pub max_lines: u32,
    /// The most changed lines one area of the sweep gets.
    pub area_lines: u32,
    /// Once today's sweeps (UTC) spent this much, the next waits for tomorrow.
    pub daily_budget_usd: Option<f64>,
}

impl Default for SweepConfig {
    fn default() -> Self {
        SweepConfig {
            every: "6h".into(),
            max_lines: 1500,
            area_lines: 800,
            daily_budget_usd: None,
        }
    }
}

/// `[floor]`: what `lens review --floor` guards.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorConfig {
    /// Globs (`globset`, where `*` also crosses `/`) for the paths worth a review on every PR:
    /// auth, migrations, money code. A change touching none of them gets no floor review.
    #[serde(default)]
    pub risky: Vec<String>,
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

/// `[pr]`: which PRs the PR pass reviews and how it posts its findings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrConfig {
    /// The share of PRs `lens review --pr` reviews, 0 to 1. Which ones is a hash of the PR
    /// number, so a rerun makes the same choice.
    #[serde(default = "PrConfig::default_sample")]
    pub sample: f64,
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

impl Default for PrConfig {
    fn default() -> Self {
        PrConfig {
            sample: Self::default_sample(),
            post_as: PostAs::default(),
            bot_token_env: None,
        }
    }
}

impl PrConfig {
    pub const DEFAULT_BOT_TOKEN_ENV: &'static str = "LENS_BOT_TOKEN";

    fn default_sample() -> f64 {
        0.5
    }

    /// Whether PR `number` is in the sample.
    pub fn samples(&self, number: u64) -> bool {
        // splitmix64: spreads consecutive PR numbers evenly over the range.
        let mut z = number.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z as f64) / (u64::MAX as f64) < self.sample
    }

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
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if !(0.0..=1.0).contains(&cfg.pr.sample) {
        anyhow::bail!(
            "{}: [pr] sample is {}, it must be from 0 to 1",
            path.display(),
            cfg.pr.sample
        );
    }
    Ok(cfg)
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
    fn pr_samples_half_by_default_and_the_same_prs_each_time() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.pr.sample, 0.5);
        let picked: Vec<u64> = (1..=1000).filter(|&n| c.pr.samples(n)).collect();
        assert!((450..=550).contains(&picked.len()), "{}", picked.len());
        assert_eq!(
            picked,
            (1..=1000).filter(|&n| c.pr.samples(n)).collect::<Vec<_>>()
        );
        let all: Config = toml::from_str("[pr]\nsample = 1.0\n").unwrap();
        assert!((1..=1000).all(|n| all.pr.samples(n)));
        let none: Config = toml::from_str("[pr]\nsample = 0\n").unwrap();
        assert!((1..=1000).all(|n| !none.pr.samples(n)));
    }

    #[test]
    fn a_sample_outside_0_to_1_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "[pr]\nsample = 1.5\n").unwrap();
        let e = load(dir.path()).unwrap_err().to_string();
        assert!(e.contains("[pr] sample is 1.5"), "{e}");
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
    fn sweep_settings_default_and_override() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.sweep.every, "6h");
        assert_eq!(c.sweep.max_lines, 1500);
        assert_eq!(c.sweep.area_lines, 800);
        assert_eq!(c.sweep.daily_budget_usd, None);
        let c: Config =
            toml::from_str("[sweep]\nevery = \"2h\"\ndaily_budget_usd = 5.0\n").unwrap();
        assert_eq!(c.sweep.every, "2h");
        assert_eq!(c.sweep.daily_budget_usd, Some(5.0));
        assert_eq!(c.sweep.max_lines, 1500);
        assert!(toml::from_str::<Config>("[sweep]\nevry = \"2h\"\n").is_err());
    }

    #[test]
    fn floor_risky_defaults_to_nothing() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.floor.risky.is_empty());
        let c: Config =
            toml::from_str("[floor]\nrisky = [\"**/auth/**\", \"migrations/*\"]\n").unwrap();
        assert_eq!(c.floor.risky, ["**/auth/**", "migrations/*"]);
        assert!(toml::from_str::<Config>("[floor]\nrisk = []\n").is_err());
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("[map]\nprovidr = \"x\"\n").is_err());
    }
}
