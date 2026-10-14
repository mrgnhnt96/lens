//! Picks the profiles that apply to a change from their `languages`, `globs`, `pathKeywords` and
//! `inferenceRules` frontmatter.
//!
//! The rules:
//! - `enabled: false` never runs.
//! - `languages` is a gate: when set, the change must touch at least one file in one of them.
//! - `globs` and `pathKeywords` are triggers: any changed path matching one selects the profile.
//!   Globs follow `globset`, where `*` also crosses `/`; keywords match case-insensitively
//!   anywhere in the path.
//! - `inferenceRules` can't be judged from paths. A profile that passes its gate, has rules and
//!   wasn't selected by a trigger comes back as [`Decision::Infer`], and the caller asks a model
//!   whether the change meets any rule.
//! - A profile with no triggers (no globs, keywords or rules) runs whenever it passes its gate,
//!   so a profile with none of the four fields runs on every change.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use globset::Glob;
use serde::Serialize;

use crate::lang::language_of;
use crate::profile::{Profile, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Runs on this change.
    Selected,
    /// Runs only if a model finds the change meets one of its `inferenceRules`.
    Infer,
}

/// A profile picked for the change, with why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Picked {
    pub name: String,
    pub decision: Decision,
    pub reasons: Vec<String>,
    pub source: Source,
    pub path: PathBuf,
    pub description: String,
    pub model: String,
    pub effort: String,
    pub severity_guidance: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inference_rules: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dispatch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_minions: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub picked: Vec<Picked>,
    pub skipped: Vec<Skipped>,
}

/// The languages the changed paths are written in, sorted.
pub fn languages(changed: &[String]) -> BTreeSet<&'static str> {
    changed.iter().filter_map(|p| language_of(p)).collect()
}

pub fn select(profiles: &[Profile], changed: &[String]) -> Result<Selection> {
    let langs = languages(changed);
    let mut selection = Selection {
        picked: Vec::new(),
        skipped: Vec::new(),
    };
    for profile in profiles {
        match judge(profile, changed, &langs)? {
            Ok((decision, reasons)) => selection.picked.push(picked(profile, decision, reasons)),
            Err(reason) => selection.skipped.push(Skipped {
                name: profile.meta.name.clone(),
                reason,
            }),
        }
    }
    Ok(selection)
}

/// `Ok(Ok(..))` when the profile is picked, `Ok(Err(reason))` when it's skipped.
fn judge(
    profile: &Profile,
    changed: &[String],
    langs: &BTreeSet<&str>,
) -> Result<std::result::Result<(Decision, Vec<String>), String>> {
    let meta = &profile.meta;
    if !meta.enabled {
        return Ok(Err("disabled".into()));
    }

    let mut reasons = Vec::new();
    if !meta.languages.is_empty() {
        let hit: Vec<_> = meta
            .languages
            .iter()
            .filter(|l| langs.contains(l.to_ascii_lowercase().as_str()))
            .collect();
        if hit.is_empty() {
            return Ok(Err(format!(
                "change touches none of its languages ({})",
                meta.languages.join(", ")
            )));
        }
        reasons.extend(hit.iter().map(|l| format!("language: {l}")));
    }

    for pattern in &meta.globs {
        let glob = Glob::new(pattern)
            .with_context(|| format!("{}: bad glob '{pattern}'", profile.path.display()))?
            .compile_matcher();
        if let Some(path) = changed.iter().find(|p| glob.is_match(p.as_str())) {
            reasons.push(format!("glob: {pattern} ({path})"));
        }
    }

    for keyword in &meta.path_keywords {
        let needle = keyword.to_ascii_lowercase();
        if let Some(path) = changed
            .iter()
            .find(|p| p.to_ascii_lowercase().contains(&needle))
        {
            reasons.push(format!("keyword: {keyword} ({path})"));
        }
    }

    let has_triggers = !meta.globs.is_empty()
        || !meta.path_keywords.is_empty()
        || !meta.inference_rules.is_empty();
    let triggered = reasons.iter().any(|r| !r.starts_with("language:"));

    Ok(if !has_triggers {
        if reasons.is_empty() {
            reasons.push("always".into());
        }
        Ok((Decision::Selected, reasons))
    } else if triggered {
        Ok((Decision::Selected, reasons))
    } else if !meta.inference_rules.is_empty() {
        reasons.push("inference rules need a model check".into());
        Ok((Decision::Infer, reasons))
    } else {
        Err("no changed path matched its globs or keywords".into())
    })
}

fn picked(profile: &Profile, decision: Decision, reasons: Vec<String>) -> Picked {
    let meta = &profile.meta;
    Picked {
        name: meta.name.clone(),
        decision,
        reasons,
        source: profile.source,
        path: profile.path.clone(),
        description: meta.description.clone(),
        model: meta.model.clone(),
        effort: meta.effort.clone(),
        severity_guidance: meta.severity_guidance.trim().to_string(),
        inference_rules: meta.inference_rules.clone(),
        dispatch: meta.dispatch,
        max_minions: meta.max_minions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Frontmatter;

    fn profile(name: &str, edit: impl FnOnce(&mut Frontmatter)) -> Profile {
        let mut meta = Frontmatter {
            name: name.into(),
            description: "d".into(),
            model: "sonnet".into(),
            effort: "high".into(),
            severity_guidance: "s".into(),
            enabled: true,
            languages: vec![],
            globs: vec![],
            path_keywords: vec![],
            inference_rules: vec![],
            dispatch: false,
            max_minions: None,
        };
        edit(&mut meta);
        Profile {
            meta,
            path: PathBuf::from(format!("{name}/profile.md")),
            source: Source::Default,
        }
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn decision(p: &Profile, changed: &[&str]) -> Option<Decision> {
        let s = select(std::slice::from_ref(p), &files(changed)).unwrap();
        s.picked.first().map(|p| p.decision)
    }

    #[test]
    fn profile_without_rules_always_runs() {
        let p = profile("correctness", |_| {});
        assert_eq!(decision(&p, &["README.md"]), Some(Decision::Selected));
        let s = select(&[p], &files(&["x"])).unwrap();
        assert_eq!(s.picked[0].reasons, vec!["always"]);
    }

    #[test]
    fn disabled_profile_never_runs() {
        let p = profile("off", |m| m.enabled = false);
        let s = select(&[p], &files(&["a.rs"])).unwrap();
        assert!(s.picked.is_empty());
        assert_eq!(s.skipped[0].reason, "disabled");
    }

    #[test]
    fn languages_gate_the_profile() {
        let p = profile("dart-only", |m| m.languages = vec!["dart".into()]);
        assert_eq!(decision(&p, &["src/main.rs"]), None);
        assert_eq!(decision(&p, &["lib/a.dart"]), Some(Decision::Selected));
    }

    #[test]
    fn languages_match_case_insensitively() {
        let p = profile("dart-only", |m| m.languages = vec!["Dart".into()]);
        assert_eq!(decision(&p, &["lib/a.dart"]), Some(Decision::Selected));
    }

    #[test]
    fn glob_selects_on_any_matching_path() {
        let p = profile("l10n", |m| m.globs = vec!["**/*.arb".into()]);
        assert_eq!(decision(&p, &["src/a.rs"]), None);
        assert_eq!(
            decision(&p, &["src/a.rs", "app_en.arb"]),
            Some(Decision::Selected)
        );
        assert_eq!(
            decision(&p, &["lib/l10n/app_en.arb"]),
            Some(Decision::Selected)
        );
    }

    #[test]
    fn glob_star_crosses_directories() {
        let p = profile("testing", |m| m.globs = vec!["**/*test*".into()]);
        assert_eq!(
            decision(&p, &["crates/x/tests/cli.rs"]),
            Some(Decision::Selected)
        );
        assert_eq!(
            decision(&p, &["src/widget_test.dart"]),
            Some(Decision::Selected)
        );
    }

    #[test]
    fn path_keyword_matches_substring_case_insensitively() {
        let p = profile("security", |m| m.path_keywords = vec!["auth".into()]);
        assert_eq!(
            decision(&p, &["src/OAuthClient.rs"]),
            Some(Decision::Selected)
        );
        assert_eq!(decision(&p, &["src/ui.rs"]), None);
    }

    #[test]
    fn inference_rules_alone_ask_for_a_model_check() {
        let p = profile("architecture", |m| {
            m.inference_rules = vec!["new layer".into()]
        });
        let s = select(&[p], &files(&["src/a.rs"])).unwrap();
        assert_eq!(s.picked[0].decision, Decision::Infer);
        assert_eq!(s.picked[0].inference_rules, vec!["new layer"]);
    }

    #[test]
    fn trigger_match_beats_inference() {
        let p = profile("perf", |m| {
            m.path_keywords = vec!["db".into()];
            m.inference_rules = vec!["hot path".into()];
        });
        assert_eq!(decision(&p, &["src/db/pool.rs"]), Some(Decision::Selected));
        assert_eq!(decision(&p, &["src/ui.rs"]), Some(Decision::Infer));
    }

    #[test]
    fn language_gate_applies_before_inference() {
        let p = profile("flutter-architecture", |m| {
            m.languages = vec!["dart".into()];
            m.inference_rules = vec!["logic in a view".into()];
        });
        assert_eq!(decision(&p, &["src/main.rs"]), None);
        assert_eq!(decision(&p, &["lib/view.dart"]), Some(Decision::Infer));
    }

    #[test]
    fn language_only_profile_runs_when_language_matches() {
        let p = profile("rusty", |m| m.languages = vec!["rust".into()]);
        let s = select(&[p], &files(&["src/a.rs"])).unwrap();
        assert_eq!(s.picked[0].reasons, vec!["language: rust"]);
    }

    #[test]
    fn globs_or_keywords_without_match_and_no_rules_skip() {
        let p = profile("l10n", |m| {
            m.globs = vec!["**/*.arb".into()];
            m.path_keywords = vec!["intl".into()];
        });
        let s = select(&[p], &files(&["src/a.rs"])).unwrap();
        assert!(s.picked.is_empty());
        assert!(s.skipped[0].reason.contains("globs or keywords"));
    }

    #[test]
    fn bad_glob_is_an_error() {
        let p = profile("bad", |m| m.globs = vec!["a[".into()]);
        assert!(select(&[p], &files(&["a"])).is_err());
    }
}
