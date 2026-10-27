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
one replaced.

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
      "howItFails": "1. login() (src/auth.rs:30) passes the raw field to parse()\n2. …"
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
  with the same model and tools share one cached prefix. A pass starts one agent per (model,
  tools) group first and holds the rest of the group until it answers; after that, the group's
  first agent shows the change under creation and every other one under read, with only its role
  prompt (a few hundred tokens) as creation. A large creation on a later agent means the cache
  missed: the prompt changed between them, or more than 5 minutes passed.
- `totals`: `agents` (how many sessions ran), `costUsd`, and the two cache fields summed over
  `agents`. The top-level `costUsd` is the same total, kept for older readers.
- `findings`: blocker and major only, every one confirmed by a verifier. `severity` is the
  verifier's, `line` the verifier's when it moved it. Ids are `F1`, `F2`, … (blockers first). A
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

`lens pr post` reads the report and posts it; it uses `head`, `base`, `risk`, `assessment`,
`profiles`, `findings` and the length of `dropped`, and ignores the rest. Each posted comment
ends with `<!-- lens:finding key=<hash of file and title> -->` and the review body with
`<!-- lens:review head=<sha> verdict=<APPROVE|REQUEST_CHANGES> -->`, which is how later runs find
what's already there and the verdict they last gave.

## The sweep

`lens sweep` saves to `.review/sweep-<stamp>.json`, with its inputs in `.review/sweep-<stamp>/`
(the whole range's `diff.patch`, `profiles.json` and `map.json`; an `area-<n>/` folder per area
with the same files cut down to that area, plus its agents' replies; `fix/` and the worktree
`tree/` while it runs). The digest goes to `.review/sweep-<stamp>.md`. The format is the same
as the PR pass's, except:

- `pass` is `sweep`. `target` is the branch swept (`origin/main`). `since`, `base` and
  `mergeBase` are the watermark the range starts from, and `head` is where it ends. It also
  has `commits` and `changedLines`, `dryRun`, `branch` (the fix branch, not set on a dry run)
  and `worktree`.
- `areas` lists `{ "title", "files", "why", "lines" }`. `docAreas` lists the areas that weren't
  reviewed because they only touch docs. Every finding, minor and skipped entry carries the
  `area` (1-based) it came from, and so do the reviewers' rows in `profiles`.
- `findings` holds the confirmed blockers and majors. A fixed blocker has `fix.status:
  "resolved"` and the `commit` of its fix. `minors` holds the minors and nits, unverified, with
  ids `M1`, `M2`... `skipped` holds the findings an earlier sweep reported, with `reason`.
- `pr` is the URL of the PR the sweep opened. `watermark` is `{ "moved": true, "to": "<sha>" }`,
  or `{ "moved": false }` after a dry run or a failed reviewer.

The next sweep reads earlier reports for three things: when the last one started (`every`),
what today's cost (`daily_budget_usd`) and which findings they reported. Dry runs count only
towards the budget.
