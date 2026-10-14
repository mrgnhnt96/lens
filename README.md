# lens

Agent code review as a Claude Code plugin. Fresh-context reviewers look at a
change in parallel, a verifier drops anything it can't show failing, and only
real problems reach you.

- **Local pass** (`/review`): about 5 minutes, before you open a PR.
- **PR pass**: about 15 minutes, run without a terminal, posting findings on the PR.

Works in any repo. Taskboard calls it at `tb done` and from its PR flow, but
lens doesn't depend on taskboard.

## Layout

```
.claude-plugin/plugin.json   plugin manifest
skills/review/               the /review skill
bin/lens                     shim that runs the lens binary built from this repo
profiles/                    the default reviewer profiles (format in profiles/README.md)
src/                         the lens CLI (Rust)
docs/PROVIDERS.md            the change-map provider contract
```

Build the binary with `cargo build --release`; `bin/lens` finds `target/release/lens`
(or `$LENS_BIN`, or `~/.cargo/bin/lens`) and points it at `profiles/`.

## `lens profiles --base <ref>`

Prints, as JSON, the profiles that apply to everything that changed since the merge base of
`<ref>` and `HEAD` (commits, staged, unstaged and untracked files):

- `enabled: false` never runs.
- `languages` gates: the change must touch a file in one of them.
- `globs` and `pathKeywords` select the profile when any changed path matches.
- `inferenceRules` can't be judged from paths: a profile that only has rules left to check
  comes back with `decision: "infer"` and the caller asks a model.
- A profile with no globs, keywords or rules runs whenever it passes its gate.

A repo's own `review-profiles/<name>/profile.md` replaces the default with that name; new
names are added.

## `lens map --base <ref>`

Prints the change map as JSON: the changed files, the symbols the change adds, edits or removes,
their callers, and the tests that cover them. The built-in provider uses `git diff` and
`git grep`; a repo can plug in a precise one (rust-analyzer, graphify, ...) through `.lens.toml`:

```toml
[map]
provider = ["tools/lens-map", "--fast"]
```

The contract and the output format are in [docs/PROVIDERS.md](docs/PROVIDERS.md).

Plan: https://claude.ai/code/artifact/a12f42b8-ee3b-41ee-953b-94b9091d47b1
