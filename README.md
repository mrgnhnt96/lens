# lens

Agent code review in one binary. Fresh-context reviewers look at a change in parallel, a
verifier drops anything it can't show failing, and only real problems reach you.

- **Local pass** (`lens review`): a few minutes, before you open a PR.
- **PR pass** (`lens review --pr <n>`): posts verified findings on the PR as inline comments.

`lens review` runs the whole pass itself; no model orchestrates it. Each reviewer and
verifier is its own headless `claude -p`, started as lean as a review allows (see below), so a
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
prompts/                     the agents' system prompts: reviewer, verifier
profiles/                    the default reviewer profiles (format in profiles/README.md)
scripts/                     prepare.sh and prepare-pr.sh, which gather a pass's inputs
build.rs                     compiles prompts/, profiles/ and scripts/ into the binary
src/                         the lens CLI (Rust)
docs/PROVIDERS.md            the change-map provider contract
docs/REVIEW.md               the report and protocol result formats, and where state is kept
```

## `lens review [<base>] [--intent <file>] [--full]`, `lens review --floor`, `lens review --pr <n>`

Runs a pass and prints its summary; the report goes to `<state>/<branch>.json` (format in
[docs/REVIEW.md](docs/REVIEW.md)). Nothing is written to the repo: `<state>` is
`$XDG_CACHE_HOME/lens/state/<repo>-<hash>` (`~/.cache` without it), one folder per checkout.

**Under the review protocol.** This is how lens is meant to run: the task board runs
`lens review` before a task's PR, with `TASKBOARD_RESULT` and the other `TASKBOARD_*` variables
set. lens then reviews the worktree against `TASKBOARD_BASE`, checks intent against
`TASKBOARD_INTENT`, keeps its state in `TASKBOARD_STATE_DIR`, writes the result
JSON to `TASKBOARD_RESULT` and exits 0. From the second round, `TASKBOARD_PREVIOUS` holds the last
round's findings with how triage settled each: only the delta is reviewed, findings marked `fix`
are verified again, and ones dismissed or backlogged aren't raised again. Finding ids stay the
same from round to round. A review a reviewer failed on, or one that can't run (the usage gate),
exits non-zero without a result, which the board counts as a reviewer error. The details are in
[docs/REVIEW.md](docs/REVIEW.md).

**What runs.** The local pass runs `correctness` (Sonnet) always, plus `testing` and, with a task
or `--intent`, `intent-alignment` (Sonnet) when the change is over `small_lines`. A change that
only touches docs runs nothing. Blockers and majors go to a Sonnet verifier each; confirmed
blockers are in the report; fixing them is the task board's job. The PR pass runs every profile lens picks,
the `impact-assessment` lead first and a correctness reviewer per focus area it names. Reviewers
run on their profile's `model` (Sonnet for every built-in one); verifiers run on Sonnet.

**Reviewing again.** When the last report reviewed this branch from the same merge base, an
unchanged tree isn't reviewed again (its report stands), and a changed tree is reviewed
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
  commands (`--permission-mode dontAsk`), and none of them edits the tree.

Each agent's reply is saved in `<state>/<branch>/agents/`, and the report records `agents` and
`costUsd`.

```toml
# .lens.toml
[review]
small_lines = 40                            # at most this many changed lines: correctness only
parallel = 4                                # agents running at once
timeout_mins = 20                           # per agent
```

**The floor.** Deep review runs on the sweep, so each PR keeps only a cheap check:
`lens review --floor` runs one Sonnet `correctness` reviewer, and only when the change touches a
path matching a glob under `[floor] risky` in the repo's `.lens.toml`. Any other change runs
nothing and prints so. Put the code where a miss costs most there: auth, migrations, money.
Globs follow `globset`, where `*` also crosses `/`, and match repo-relative paths. Its findings
are verified like any local pass's, but the floor raises only what this change caused.
The report records tier `floor`. A repo with no `[floor]` section
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
the reports in lens's state folders and any an older lens left under `<root>/**/.review/`
(default `~/Development`): `costUsd`, agents and findings
per repo and per day, plus cache creation/read tokens when the reports carry them. `--since`
takes a span (`1d`) or a date (`2026-10-19`). Archived earlier passes (`prev/`) are left out
unless you pass `--with-prev`.

## Usage log

Every agent lens runs also records what it took of the Claude plan, in `~/.lens/usage/` (or
`$LENS_USAGE_DIR`), one JSON object per line:

- `agents.jsonl`: a row per agent run and model, with its repo, label, start and finish times,
  input, output and cache tokens, and `costUsd`.
- `readings.jsonl`: every reading of the 5-hour and 7-day windows the agents' streams report
  (`window`, `utilization` from 0 to 1, `resetsAt`, when it was seen).

Many sessions share the windows, so one run's before and after readings say little on their own;
the two logs are kept for comparing the change between readings with all the usage in that gap.

### `lens usage [--since 1d|<date>]`

Reads the two logs back:

- lens's spend (runs, `costUsd`, input, output and cache tokens) per repo, per role (reviewer,
  verifier) and per model.
- each window's first and last reading, its peak, and how far it rose in all (a reset counts the
  next period from 0).
- the least lens work 1% of each window holds. The readings are split into stretches that rose at
  least 2%, and each stretch's lens spend (each run's share by time) over its rise is a floor,
  since other sessions' usage is in the rise too. The tightest stretch gives the floor, and from it
  the least lens work the rest of the window holds if nothing else ran.

## `lens pr earlier --pr <n>` and `lens pr post --pr <n> --report <file> --diff <patch>`

The PR pass's GitHub side, through `gh`. `earlier` lists the lens findings already posted on the
PR. `post` posts a report as one review at the reviewed commit: an inline comment per new finding
on a line the diff shows (minor and nit collapsed), the rest in the review's summary. A review is
never just comments: it requests changes while a blocker or major is unfixed, and approves
otherwise, minors and nits included. A clean review approves with an empty body. Findings an
earlier run posted (same finding, or the same file within 3 lines) are skipped, and a rerun with
nothing new and the same verdict posts nothing. GitHub doesn't let a PR's author approve or
request changes on it, so posting as the author falls back to a plain review with the findings,
or posts nothing for a clean one. `--dry-run` prints the payload instead.

Which PRs `lens review --pr` reviews, and who posts, are set in `.lens.toml`:

```toml
[pr]
sample = 0.5                      # the share of PRs reviewed, 0 to 1 (default 1, every PR)
post_as = "bot"                   # "owner" (default): whoever `gh` is logged in as
bot_token_env = "LENS_BOT_TOKEN"  # the env var holding the bot's token (this is the default)
```

`sample` picks PRs by a hash of the PR number, so a rerun on the same PR makes the same choice. A
PR outside the sample runs nothing and prints so; `sample = 0.5` reviews about half.

With `post_as = "bot"`, `lens pr` fails rather than post as the owner when the variable is unset.

## License

MIT. See [LICENSE](LICENSE).
