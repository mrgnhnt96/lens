# The review file

`lens review` saves each pass to `<state>/<branch>.json` (a detached HEAD is
`detached-<short sha>`). The PR pass reads it. `<state>` is never inside the repo,
so a review writes nothing to the checkout:

- under the review protocol, `TASKBOARD_STATE_DIR`, which the task board keeps per task;
- run by hand, `$XDG_CACHE_HOME/lens/state/<repo>-<hash>` (`~/.cache` when `XDG_CACHE_HOME` isn't
  set), where `<repo>` is the checkout's folder name and `<hash>` an FNV-1a hash of its path, so two
  checkouts of one repo keep apart.

The pass's inputs sit beside the report in `<state>/<branch>/`: `diff.patch`, `map.json`,
`profiles.json`, `task.json`, `intent.md`; `agents/` holds the shared system prompt
(`change-<hash>.system.md`) and each agent's reply, and `prev/` the report this
one replaced. `reports/` keeps a copy of every report a pass wrote, named
`<startedAt>-<head>.json` (e.g. `20261102T204105Z-3cd14fc.json`), so earlier runs stay readable;
`[review] keep_reports = false` turns that off. A PR pass works the same way in `<state>/pr-<n>/`,
its latest report in `<state>/pr-<n>.json`.

```json
{
  "version": 1,
  "pass": "local",
  "root": "/repo",
  "branch": "feat/x",
  "base": "origin/main",
  "mergeBase": "71fae7c…",
  "head": "ff379e4…",
  "dirty": true,
  "tree": "9c1e04b7d2…",
  "changedLines": 104,
  "mode": "delta",
  "tier": "full",
  "delta": { "from": "72557a01ef…", "changedLines": 48 },
  "startedAt": "2026-10-14T19:40:02Z",
  "finishedAt": "2026-10-14T19:44:14Z",
  "seconds": { "total": 252, "reviewers": 118, "verify": 57 },
  "agents": [
    { "label": "reviewer-correctness", "model": "sonnet", "tools": "Read,Grep,Glob,Bash",
      "costUsd": 0.21, "turns": 9, "seconds": 104,
      "cacheCreationInputTokens": 18233, "cacheReadInputTokens": 0 },
    { "label": "reviewer-testing", "model": "sonnet", "tools": "Read,Grep,Glob,Bash",
      "costUsd": 0.12, "turns": 7, "seconds": 96,
      "cacheCreationInputTokens": 214, "cacheReadInputTokens": 18019 }
  ],
  "totals": { "agents": 5, "costUsd": 0.61,
              "cacheCreationInputTokens": 41022, "cacheReadInputTokens": 54107 },
  "costUsd": 0.61,
  "map": { "provider": "grep", "error": null },
  "profiles": [
    { "name": "correctness", "ran": true, "model": "sonnet", "reason": "always", "findings": 3 },
    { "name": "testing", "ran": true, "model": "sonnet", "reason": "inferred", "findings": 2 },
    { "name": "intent-alignment", "ran": false, "reason": "no task context" }
  ],
  "findings": [
    {
      "id": "F1",
      "profiles": ["correctness"],
      "file": "src/parser.rs",
      "line": 42,
      "severity": "blocker",
      "title": "empty input parses as a valid token",
      "failureScenario": "parse(\"\") returns Ok(Token::empty()), so login() accepts a blank password",
      "confidence": 0.8,
      "howItFails": "1. login() (src/auth.rs:30) passes the raw field to parse()\n2. …",
      "suggestedFix": "return Err from parse() on an empty string"
    }
  ],
  "dropped": [
    { "profiles": ["testing"], "file": "src/parser.rs", "line": 40, "severity": "major",
      "title": "…", "reason": "unverified: covered by tests/parser.rs:12" },
    { "profiles": ["correctness"], "file": "src/auth.rs", "line": 9, "severity": "minor",
      "title": "…", "reason": "below major" }
  ]
}
```

- `pass`: `local` for `lens review`, `pr` for `lens review --pr <n>`.
- `root`: the checkout reviewed.
- `dirty`: the working tree had uncommitted or untracked changes; they were reviewed too.
- `tree`: the working tree the pass reviewed, as a git tree object (untracked files in). The
  next pass compares against it.
- `mode`: `full`, or `delta` when only what changed since the last report (`delta.from`) was
  reviewed. A delta carries the last report's findings over: unfixed blockers, and majors in
  files the delta touched, are verified again; other majors are kept as they were.
- `incomplete`: the profiles whose reviewer gave no readable reply. The next pass doesn't trust
  such a report and reviews the whole change.
- `tier`: `skip` (only docs changed: no agents ran), `small` (at most `[review] small_lines`
  changed lines: correctness only) or `full`.
- `agents`: one row per agent session: its `label`, `model`, `tools`, `costUsd` and `turns` as
  Claude Code reports them, `seconds`, and an `error` when it failed. `cacheCreationInputTokens`
  and `cacheReadInputTokens` are the usage of its first request, which is where the change gets
  cached: every agent of a pass shares one system prompt (a short preamble plus the diff, change
  map, task and guideline files) and gets its role's instructions as its first message, so agents
  with the same model and tools share one cached prefix. Every batch starts one agent per (model,
  tools) group first and holds the rest of the group until it answers, even when the group ran a
  moment ago, so a cache miss costs one write rather than one per agent; after that, the group's
  first agent shows the change under creation and every other one under read, with only its role
  prompt (a few hundred tokens) as creation. A large creation on a later agent means the cache
  missed: the prompt changed between them, more than 5 minutes passed, or the API didn't have it
  (its cache is best-effort).
- `totals`: `agents` (how many sessions ran), `costUsd`, and the two cache fields summed over
  `agents`. The top-level `costUsd` is the same total, kept for older readers.
- `findings`: blocker and major only, every one confirmed by a verifier. `severity` is the
  verifier's, `line` the verifier's when it moved it, and `failureScenario` the verifier's when
  it took out a failure it couldn't reach (otherwise the reviewer's). `suggestedFix` is an
  optional one-line fix, only ever the verifier's (a reviewer's suggestion is a hint it checks).
  Ids are `F1`, `F2`, … (blockers first). A
  finding carried over from the last report keeps its id, and a new one takes the next number
  after any id the branch's reports have used (`lastId`), so an id never changes meaning.
- `settled`: findings the task board's triage dismissed or backlogged (see the protocol below),
  with `id`, `file`, `line`, `severity`, `title`, `outcome` and `reason`. Kept from report to
  report; a finding that raises one of them again is dropped as `dismissed in triage: <reason>`
  or `backlogged in triage: <reason>`.
- `lastId`: the highest finding number the branch's reports have used.
- `dropped`: everything else, with why: `below major` or `unverified: <the verifier's reason>`.
  Duplicates raised by several profiles are merged into one entry listing them all.
- `map.error`: the provider's error when `lens map` failed; the pass ran without a map.
- `profiles[].ran: false` with `reason`: a local-pass profile that didn't apply. Profiles left
  to the PR pass aren't listed. `failed` is set when the reviewer's reply had no readable JSON.

## diff.patch

`prepare.sh` writes `diff.patch` as one `git diff --full-index` from `mergeBase` to a snapshot of
the working tree, made with `git add -A` into a throwaway index (the checkout's own index isn't
touched). `git apply --cached` of it onto `mergeBase` rebuilds the reviewed tree exactly, binary
files included: they show as `Binary files … differ`, which keeps the patch readable for
reviewers, and still apply, since the full index line names a blob `git add` already stored.

## The review protocol

The task board's pre-PR review runs a reviewer command in a task's worktree and reads back one
JSON result. `lens review` is such a reviewer: it runs as one whenever `TASKBOARD_RESULT` is set,
and only reads the rest of the protocol's variables then.

| Variable | lens uses it as |
| --- | --- |
| `TASKBOARD_RESULT` | where the result goes; set means protocol mode |
| `TASKBOARD_REPO` | the repo to review, unless a path is passed (lens runs in the worktree anyway) |
| `TASKBOARD_BASE` | the base, unless one is passed |
| `TASKBOARD_INTENT` | the task's intent (markdown) for `intent-alignment`, unless `--intent` is passed |
| `TASKBOARD_STATE_DIR` | `<state>`: kept from round to round, so round 2 finds round 1's report |
| `TASKBOARD_PREVIOUS` | the last round's result with triage's `outcome` and `reason` on each finding; unset in round 1 |

In protocol mode lens never touches the worktree: no files in
the repo, no commits. It doesn't ask `tb peek` for a task either; the intent file is the task.

**Rounds.** Round 1 reviews the whole change. A later round finds the last report in `<state>`
and reviews only what changed since (`mode: "delta"`), as any pass does. From
`TASKBOARD_PREVIOUS`:

- `fix`: the finding is verified again whatever files the delta touched, and keeps its id while
  it still holds. One the task's agent dealt with is dropped and leaves the result.
- `dismiss` and `backlog`: the finding moves to the report's `settled` list and isn't reported
  again, in this round or any later one, even when a reviewer raises it afresh (same id, or the
  same file, nearby line and title).

A tree unchanged since the last round isn't reviewed again: the result repeats the last report's
findings, less those triage settled, at no cost.

**The result**, written to `TASKBOARD_RESULT`:

```json
{
  "protocol": 1,
  "reviewer": "lens",
  "verdict": "fail",
  "headline": "1 finding (1 blocker) in 12s",
  "findings": [
    {
      "id": "F1",
      "title": "empty input parses as a valid token",
      "severity": "blocker",
      "file": "src/parser.rs",
      "line": 42,
      "detail": "parse(\"\") returns Ok(Token::empty()), so login() accepts a blank password\n\n1. login() …",
      "url": null
    }
  ],
  "cost": { "usd": 0.61 }
}
```

- `verdict`: `fail` when there are findings, `pass` when reviewers ran and confirmed none, `skip`
  when nothing needed review (no change against the base, only docs changed, or the floor found
  no risky path).
- `headline`: the first line of the summary `lens review` prints.
- `findings`: the report's `findings` (blocker and major, verified), ids as in the report.
  `detail` is the failure scenario, then the steps of how it fails.
- `cost.usd`: what this round's agents cost.

**Exit status.** lens exits 0 once the result is written. It exits non-zero without writing one,
which the board counts as a reviewer error, when a reviewer gave no readable reply (the review is
incomplete; the report is still saved and the next round reviews everything), when the usage
gate holds the review, or when anything else fails. Progress goes to stderr.

## The PR pass

`lens review --pr <n>` saves to `<state>/pr-<n>.json`, with its inputs in `<state>/pr-<n>/` (plus
`pr.json`, `intent.md`, `posted.json` and, when the checkout isn't at the PR's head, `tree/`).
The format is the same, except:

- `pass` is `pr`, and `pr` is `{ "number": 12, "url": "https://github.com/o/r/pull/12" }`.
  `branch` is the PR's head branch, `head` its head commit, `base` `origin/<its base>`. It has
  `risk` and `assessment` from the `impact-assessment` lead, its focus-area reviewers are listed
  in `profiles` as `focus: <title>`, and it has no `tree`, `mode` or `tier`.
- `findings` holds every confirmed finding, `minor` and `nit` included; `dropped` only the
  unverified ones. Every profile lens picked is listed in `profiles`.
- `lens` is the lens that wrote it: its version and a hash of its prompts and profiles.

A PR lens already reviewed is reviewed again only for what changed since: the pass reads the
commit its last review was for from that review's marker (in `posted.json`) and, when it is an
ancestor of the head, gives every agent `git diff <that commit> <head>` as "what changed since
the last review". Reviewers and the verifier raise nothing about code the delta doesn't touch
or newly depend on, unless it is a blocker. The last review's blockers and majors (from its kept
report) are verified again at the head: still failing, they stay and keep the verdict; fixed,
they drop. The report has `"mode": "delta"` and `"delta": {"from", "changedLines"}`; a first
review, a rewritten history or `--full` reviews the whole change (`"mode": "full"`).

A PR pass whose `head` and `base` match the last report's, written by the same `lens`, runs no
agents: it reuses that report and posts what's left to post. `--full` reviews it again. A PR
gets one review per commit, and a rerun can't turn up a different set of findings for the same
code.

`lens review --pr <n> --dry-run --only <profile>,...` is for tuning reviewers: `profiles.json`
picks only the named profiles (the rest are `skipped` with `"not named in --only"`), so the
report's `profiles` lists just those, and their findings are verified as usual. The lead runs
only when `impact-assessment` is named. The run is always `"mode": "full"`, posts nothing, and
keeps its state in `<state>/.only/` instead of `<state>/`, so the PR's own report, its reuse and
its deltas never see a partial run.

`lens pr post` reads the report and posts it; it uses `pr.url`, `head` and `findings`, and
ignores the rest. It posts `blocker`, `major` and `minor` findings; `nit`s are counted in the
review's first line (`· 2 nit left out`) and not shown. An agent reads the post, so it holds
nothing but the findings: no impact assessment, no list of reviewers, no verifier trace. `[pr] comments` in `.lens.toml` picks where the posted
ones go:

- `single` (the default): the review has no inline comments. Each new finding is listed in its
  body as `**<severity>** · <title> · <file:line>`, then its failure scenario.
  The `file:line` links to the line in the PR's diff (`<pr.url>/files#diff-<sha256 of the
  path>R<line>`) when the diff shows it, else to the file at `head`; without `pr.url` it's plain
  text.
- `inline`: a finding on a line the diff shows is an inline comment on that line; the rest are
  listed in the body as in `single`.

Each posted finding, inline or in the body, ends with `<!-- lens:finding key=<hash of file and
title> -->` and the review body with `<!-- lens:review head=<sha>
verdict=<APPROVE|REQUEST_CHANGES> -->`, which is how later runs find what's already there and the
verdict they last gave. A finding is already there when an earlier one has its key (in a body or
inline) or sits in the same file within 3 lines: commented inline there, or listed in a review
body with that `file:line`, so a rerun skips it whichever
mode either run used. The pre-PR protocol review is unaffected: it keeps every severity.

On Bitbucket, which shows HTML comments, the markers are link-reference lines on their own:
`[//]: # (lens:finding key=<hash>)` and `[//]: # (lens:review head=<sha> verdict=<APPROVE|REQUEST_CHANGES>)`;
lens reads both forms on either forge. The review there is a verdict and comments, sent in this
order: `DELETE …/pullrequests/<n>/approve` or `…/request-changes` (the opposite verdict; a 400,
404 or 409 means there was none), `POST` the verdict (a 409 means it's already given; any other
failure stops before a comment goes up), a `POST …/comments` with `inline: {path, to}` for each
inline finding, and last the summary `POST …/comments`, the review body, whose marker records the
review. `file:line` links go to the file at `head` (`<repo>/src/<head>/<file>#lines-<n>`),
on the diff or not.

## What became of posted findings

Before every review (`lens review` in any pass), lens checks the PRs it has reviewed whose
outcomes aren't settled: those with a `<state>/pr-<n>.json` and no `pr-<n>/outcomes.json`, an
`outcomes.json` older than that report, or one with `"settled": false`. Nothing runs on a
schedule. On GitHub it reads every such PR's state, head and review threads with one GraphQL
query per 20 PRs, fetches their heads to `refs/lens/pr/<n>` in one `git fetch`, and writes each
PR's `pr-<n>/outcomes.json`. Each finding lens posted gets one of these fates:

- `fixed`: the code within 3 lines of its spot changed between the commit it was posted for and
  the PR's head.
- `dismissed`: no such change, but someone replied in its thread, or the thread was resolved.
- `ignored`: no change and no answer, and the PR is merged or closed.
- `open`: none of these yet.

A PR is `settled` once it is merged or closed. A settled PR isn't checked again until a new report
for it is written. A failed check prints a warning and the review goes on. `[review]
track_outcomes = false` in `.lens.toml` turns the check off. `lens pr outcomes` runs the same check
whenever you ask. Bitbucket PRs aren't checked yet.

```json
{
  "pr": 12,
  "head": "9f1c2ab…",
  "state": "MERGED",
  "settled": true,
  "checkedAt": "2026-11-02T20:41:05Z",
  "findings": [
    {
      "key": "3be1…", "id": "F2", "profiles": ["correctness"], "severity": "major",
      "title": "…", "file": "src/a.rs", "line": 40, "postedAt": "72557a01ef…",
      "url": "https://github.com/o/r/pull/12#discussion_r1", "fate": "fixed",
      "why": "the code at its spot changed since"
    }
  ],
  "summary": {
    "total": { "posted": 1, "fixed": 1, "dismissed": 0, "ignored": 0, "open": 0 },
    "byProfile": { "correctness": { "posted": 1, "fixed": 1, "dismissed": 0, "ignored": 0, "open": 0 } },
    "bySeverity": { "major": { "posted": 1, "fixed": 1, "dismissed": 0, "ignored": 0, "open": 0 } }
  }
}
```

A finding's `profiles`, `severity` and `postedAt` come from the first kept report (in `reports/`)
that has it. A finding that no kept report holds is counted under the `unknown` profile.
