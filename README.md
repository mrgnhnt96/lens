# lens

Agent code review in one binary. Fresh-context reviewers look at a change in parallel, a
verifier drops anything it can't show failing, and only real problems reach you.

- **Local pass** (`lens review`): a few minutes, before you open a PR.
- **PR pass** (`lens review --pr <n>`): posts verified findings on the PR as inline comments.

`lens review` runs the whole pass itself; no model orchestrates it. Each reviewer, verifier and
fixer is its own headless `claude -p`, started as lean as a review allows (see below), so a
review costs only what its agents read and write. It needs `claude` (logged in) and `git` on the
PATH, and `gh` for the PR pass.

Works in any repo. Taskboard runs it when a task is submitted and from its PR flow, but lens
doesn't depend on taskboard.

## Install

```sh
cargo install --locked --path .
```

The binary carries everything it runs: the agents' prompts, the default profiles and the prepare
scripts are compiled in, and written to `~/.cache/lens/<hash>/` (or `$XDG_CACHE_HOME`) the first
time a build runs. Set `LENS_ASSETS` to a checkout to use its files instead, while working on
them.

## Layout

```
prompts/                     the agents' system prompts: reviewer, verifier, fixer, escalator
profiles/                    the default reviewer profiles (format in profiles/README.md)
scripts/                     prepare.sh and prepare-pr.sh, which gather a pass's inputs
build.rs                     compiles prompts/, profiles/ and scripts/ into the binary
src/                         the lens CLI (Rust)
docs/PROVIDERS.md            the change-map provider contract
docs/REVIEW.md               the .review/<branch>.json and .review/pr-<n>.json format
```

## `lens review [<base>] [--intent <file>] [--no-fix] [--full]`, `lens review --floor`, `lens review --pr <n>`

Runs a pass and prints its summary; the report goes to `.review/<branch>.json` (format in
[docs/REVIEW.md](docs/REVIEW.md)).

**What runs.** The local pass runs `correctness` (Opus) always, plus `testing` and, with a task
or `--intent`, `intent-alignment` (Sonnet) when the change is over `small_lines`. A change that
only touches docs runs nothing. Blockers and majors go to an Opus verifier each; confirmed
blockers go through the fix loop unless `--no-fix`. The PR pass runs every profile lens picks,
the `impact-assessment` lead first and a correctness reviewer per focus area it names.

**Reviewing again.** When the last report reviewed this branch from the same merge base, an
unchanged tree isn't reviewed again (an open fix loop resumes), and a changed tree is reviewed
only for what changed since (`mode: "delta"`). The last report's findings carry over: blockers
left unfixed, and majors in files the delta touched, are verified again. A report a reviewer
failed on doesn't count, so the next pass reviews everything; `--full` does too.

**How agents start.** Each one is `claude -p` with:

- `--setting-sources ""`, `--strict-mcp-config`, `--disable-slash-commands`: none of your
  CLAUDE.md, memory, hooks, plugins, skills or MCP servers. It still uses your login.
- `--system-prompt-file`: the role's prompt from `prompts/`, followed by the change itself (the
  diff, the change map, the task and the repo's `CLAUDE.md`/`AGENTS.md`). Every agent of a role
  shares it, and the first agent of a batch runs alone until its first response, so the others
  read it from the prompt cache.
- `CLAUDE_CODE_PROMPT_CACHE_TTL=5m`: a headless session otherwise writes the 1-hour cache, at
  twice the input price; review agents never pause long enough to need it.
- Only the tools the role needs: reviewers and verifiers read files and run read-only git
  commands (`--permission-mode dontAsk`), the judge only reads, the fixer edits and runs tests.

Each agent's reply is saved in `.review/<branch>/agents/`, and the report records `agents` and
`costUsd`.

```toml
# .lens.toml
[review]
small_lines = 40                            # at most this many changed lines: correctness only
parallel = 4                                # agents running at once
timeout_mins = 20                           # per agent
fixer_permission_mode = "bypassPermissions" # the fixer edits and runs tests, headless
```

**The floor.** Deep review runs on the sweep, so each PR keeps only a cheap check:
`lens review --floor` runs one Sonnet `correctness` reviewer, and only when the change touches a
path matching a glob under `[floor] risky` in the repo's `.lens.toml`. Any other change runs
nothing and prints so. Put the code where a miss costs most there: auth, migrations, money.
Globs follow `globset`, where `*` also crosses `/`, and match repo-relative paths. Its findings
are verified like any local pass's, but the floor raises only what this change caused, and it
doesn't start the fix loop. The report records tier `floor`. A repo with no `[floor]` section
never gets a floor review.

```toml
# .lens.toml
[floor]
risky = ["src/auth/**", "migrations/**", "**/billing/*.rs", "**/payment*"]
```

`$LENS_CLAUDE` names the `claude` program to run (the tests use a fake).

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

## `lens sweep [--since <ref>] [--dry-run] [--now]`

Reviews everything pushed to origin's default branch since the last sweep, in one batch per repo
instead of inside every task. The range runs from the watermark ref `refs/lens/swept` to
`origin/main` (origin's default branch). The first sweep only sets the watermark; a range with
no new commits does nothing.

- The `impact-assessment` lead reads the whole range and splits it into areas of at most
  `area_lines` changed lines. lens enforces the cap, and files the lead left out are packed by
  path. Each area gets its own change block (one prompt-cache group) and its own reviewers.
  Areas that only touch docs are skipped.
- Reviewers are asked only for problems these commits caused. Findings an earlier sweep already
  reported (the same file and title, or the same file within 3 lines) are skipped.
- Blockers and majors go to the verifier. Confirmed blockers go through the fix loop in a
  worktree on `lens/sweep-<stamp>`, one commit per fix. The branch is pushed and opened as one
  PR, and its body is the digest (`.review/sweep-<stamp>.md`). Minors are listed in the digest
  only.
- The watermark moves to the reviewed head only when no reviewer failed.

The report goes to `.review/sweep-<stamp>.json` (`<stamp>` is UTC `YYYY-MM-DD-HHMM`).
`--dry-run` reviews and reports, but fixes nothing, opens no PR and leaves the watermark.
`--since` reviews from another ref. `--now` runs even when the sweep isn't due.

```toml
# .lens.toml
[sweep]
every = "6h"            # a sweep is due this long after the last one (90m, 6h, 1d)...
max_lines = 1500        # ...or sooner, once this many changed lines are waiting
area_lines = 800        # the most changed lines in one area
daily_budget_usd = 20.0 # once today's sweeps spent this, wait for the next sweep (default: none)
```

## `lens cost [--since 1d|<date>] [--root <dir>]`

What reviews spent. lens's `claude -p` agents never show in session transcripts, so this adds up
the reports under `<root>/**/.review/` (default `~/Development`): `costUsd`, agents and findings
per repo and per day, plus cache creation/read tokens when the reports carry them. `--since`
takes a span (`1d`) or a date (`2026-10-19`). Archived earlier passes (`prev/`) are left out
unless you pass `--with-prev`.

## `lens fix`

The blocker fix loop's ledger. `lens review` runs a fixer agent on each blocker, re-reviews the
fix, and asks a Haiku judge after every failed re-review whether to try again or bring you in;
`lens fix` keeps the record and the rules a model doesn't get to bend: at most 5 attempts
(`--cap`), and a fix counts only when the verifier drops the finding and nothing new is
confirmed.

```sh
lens fix start   --report .review/feat/x.json --finding F1   # prints the next step
lens fix diff    --ledger <ledger>                            # the attempt in progress
lens fix record  --ledger <ledger> --fixer <reply> --review <re-review>
lens fix judge   --ledger <ledger> --decision <judge reply>
lens fix summary --ledger <ledger>                            # what to tell the human
```

Each command but `diff` and `summary` prints `{"next": "fix" | "judge" | "done" | "escalate"}`.
The ledger format is in [docs/REVIEW.md](docs/REVIEW.md).

## `lens pr earlier --pr <n>` and `lens pr post --pr <n> --report <file> --diff <patch>`

The PR pass's GitHub side, through `gh`. `earlier` lists the lens findings already posted on the
PR. `post` posts a report as one review at the reviewed commit: an inline comment per new finding
on a line the diff shows (minor and nit collapsed), the rest in the review's summary. Findings an
earlier run posted (same finding, or the same file within 3 lines) are skipped, and a rerun with
nothing new posts nothing. `--dry-run` prints the payload instead.

Who posts is set in `.lens.toml`:

```toml
[pr]
post_as = "bot"                   # "owner" (default): whoever `gh` is logged in as
bot_token_env = "LENS_BOT_TOKEN"  # the env var holding the bot's token (this is the default)
```

With `post_as = "bot"`, `lens pr` fails rather than post as the owner when the variable is unset.

Plan: https://claude.ai/code/artifact/a12f42b8-ee3b-41ee-953b-94b9091d47b1
