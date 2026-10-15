# lens

Agent code review as a Claude Code plugin. Fresh-context reviewers look at a
change in parallel, a verifier drops anything it can't show failing, and only
real problems reach you.

- **Local pass** (`/review`, or `lens review`): a few minutes, before you open a PR.
- **PR pass** (`/review --pr <n>`, or `lens review --pr <n>`): run without a terminal
  (`claude -p "/review --pr 12"`), posting verified findings on the PR as inline comments.

`lens review` runs the whole pass itself; no model orchestrates it. Each reviewer, verifier and
fixer is its own headless `claude -p`, started as lean as a review allows (see below), so a
review costs only what its agents read and write.

Works in any repo. Taskboard calls it at `tb done` and from its PR flow, but
lens doesn't depend on taskboard.

## Layout

```
.claude-plugin/plugin.json   plugin manifest
skills/review/               the /review skill (runs `lens review`) and its prepare scripts
prompts/                     the agents' system prompts: reviewer, verifier, fixer, escalator
bin/lens                     shim that runs the lens binary built from this repo
profiles/                    the default reviewer profiles (format in profiles/README.md)
src/                         the lens CLI (Rust)
docs/PROVIDERS.md            the change-map provider contract
docs/REVIEW.md               the .review/<branch>.json and .review/pr-<n>.json format
```

Build the binary with `cargo build --release`; `bin/lens` finds `target/release/lens`
(or `$LENS_BIN`, or `~/.cargo/bin/lens`) and points it at `profiles/` and `prompts/`.

## `lens review [<base>] [--intent <file>] [--no-fix] [--full]`, `lens review --pr <n>`

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
