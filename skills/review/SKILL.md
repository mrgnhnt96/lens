---
name: review
description: Review the current change with lens's reviewer profiles. Use when the user runs /review or asks for a lens review of their branch before opening a PR.
---

# /review

lens picks the reviewer profiles that apply to the change, then each one reviews the diff.
This skill is a scaffold: today it covers picking the profiles. The reviewer, verifier and fix
loop steps land in later lens releases.

## 1. Pick the profiles

Run from the repo being reviewed, with the branch the change will merge into:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/lens" profiles --base origin/main
```

It prints JSON:

- `changedFiles`, `languages`: what the change touches (commits since the merge base, staged,
  unstaged and untracked files).
- `picked`: profiles to run, each with `path` (its `profile.md`, whose body is the reviewer's
  prompt), `model`, `effort`, `severityGuidance` and `reasons`.
  - `decision: "selected"` runs.
  - `decision: "infer"` runs only if the change meets one of its `inferenceRules`. Read the diff
    and decide; drop the profile when none apply.
- `skipped`: profiles that don't apply, with the reason.

A repo's own `review-profiles/<name>/profile.md` replaces lens's default of the same name.
