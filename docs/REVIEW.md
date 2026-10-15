# The review file

`lens review` saves each pass to `.review/<branch>.json` in the reviewed repo (a detached HEAD is
`detached-<short sha>`). The fix loop and the PR pass read it. `.review/` holds a `.gitignore`
of `*`, so it never reaches a commit. The pass's inputs sit beside it in `.review/<branch>/`:
`diff.patch`, `map.json`, `profiles.json`, `task.json`, `intent.md`; `agents/` holds each agent's
system prompts and replies, and `prev/` the report (and fix ledgers) this one replaced.

```json
{
  "version": 1,
  "pass": "local",
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
  "seconds": { "total": 252, "reviewers": 118, "verify": 57, "fix": 0 },
  "agents": 5,
  "costUsd": 0.61,
  "map": { "provider": "grep", "error": null },
  "profiles": [
    { "name": "correctness", "ran": true, "model": "opus", "reason": "always", "findings": 3 },
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
- `dirty`: the working tree had uncommitted or untracked changes; they were reviewed too.
- `tree`: the working tree the pass reviewed, as a git tree object (untracked files in). The
  next pass compares against it (or against the last fix attempt's `after`).
- `mode`: `full`, or `delta` when only what changed since the last report (`delta.from`) was
  reviewed. A delta carries the last report's findings over: unfixed blockers, and majors in
  files the delta touched, are verified again; other majors are kept as they were.
- `incomplete`: the profiles whose reviewer gave no readable reply. The next pass doesn't trust
  such a report and reviews the whole change.
- `tier`: `skip` (only docs changed: no agents ran), `small` (at most `[review] small_lines`
  changed lines: correctness only) or `full`.
- `agents`, `costUsd`: how many agent sessions the pass ran and what they cost, as Claude Code
  reports it.
- `findings`: blocker and major only, every one confirmed by a verifier. `severity` is the
  verifier's, `line` the verifier's when it moved it. Ids are `F1`, `F2`, … in report order
  (blockers first).
- `dropped`: everything else, with why: `below major` or `unverified: <the verifier's reason>`.
  Duplicates raised by several profiles are merged into one entry listing them all.
- `map.error`: the provider's error when `lens map` failed; the pass ran without a map.
- `profiles[].ran: false` with `reason`: a local-pass profile that didn't apply. Profiles left
  to the PR pass aren't listed. `failed` is set when the reviewer's reply had no readable JSON.
- `findings[].fix`: set on a blocker once the fix loop takes it up:
  `{"status": "open" | "resolved" | "escalated", "attempts": 2, "ledger": "<path>"}`.

## Freshness: what `tb submit` checks

The task board takes a local report as current when the change it reviewed is the change the
checkout holds now. It reads `.review/<branch>.json` (the branch `git symbolic-ref` gives, or
`detached-<short sha>`) and compares two patch ids (`git patch-id --stable` of a `git diff
--binary`):

- **Reviewed**: from `mergeBase` to the reviewed tree. That tree is the last fix attempt's `after`
  when a finding has a ledger; otherwise `head^{tree}` when `dirty` is false; otherwise the tree
  `git apply --cached .review/<branch>/diff.patch` builds on top of `mergeBase`.
- **Now**: from `git merge-base <base> HEAD` to the working tree as a tree object (untracked files
  in, ignored ones out).

Only `base`, `mergeBase`, `head`, `dirty`, `branch`, `findings[].fix.ledger` and `diff.patch` count.
Timestamps, `changedLines` and file mtimes don't, so a commit or a clean rebase of the same change
keeps the report fresh and any edit to the tree, untracked files included, makes it stale.

So `diff.patch` has to replay exactly. `prepare.sh` writes it as one `git diff --full-index` from
`mergeBase` to a snapshot of the working tree, made with `git add -A` into a throwaway index. Binary
files show as `Binary files … differ`, which keeps the patch readable for reviewers, and still
apply: the full index line names a blob `git add` already stored. A patch built from `git diff
--no-index` per untracked file had short index lines, so any untracked binary (screenshots, media)
made `git apply` fail and every review read as older than the change.

## The fix ledger

The fix loop keeps one ledger per blocker at `.review/<branch>/fix/<id>.json`, written only by
`lens fix`, with each attempt's diff beside it as `<id>-attempt-<n>.patch`.

```json
{
  "version": 1,
  "findingId": "F1",
  "finding": { "id": "F1", "file": "src/parser.rs", "line": 42, "severity": "blocker", "…": "…" },
  "report": "/repo/.review/feat/x.json",
  "root": "/repo",
  "baseTree": "72557a01ef…",
  "cap": 5,
  "status": "escalated",
  "attempts": [
    {
      "n": 1,
      "before": "72557a01ef…",
      "after": "9c1e04b7d2…",
      "patch": "/repo/.review/feat/x/fix/F1-attempt-1.patch",
      "changedLines": 6,
      "fixer": { "summary": "reject empty input in parse()", "needsDecision": null },
      "review": {
        "original": { "verdict": "confirmed", "howItFails": "1. login() …" },
        "newFindings": []
      },
      "resolved": false,
      "decision": { "decision": "escalate", "needs": "no-progress", "reason": "…" }
    }
  ],
  "escalation": { "by": "judge", "needs": "no-progress", "reason": "…" }
}
```

- `baseTree`, `before`, `after`: the working tree as git tree objects (untracked files
  included, `.review/` and ignored files left out). `git diff <baseTree>` shows every attempt.
- An attempt is `resolved` when its diff isn't empty, the verifier drops the original finding
  and the re-review confirms nothing new. An attempt that changed nothing has no `review`.
- `escalation.by`: `judge` (the Haiku escalation judge called it) or `cap` (the attempt that
  hit `cap` failed; the judge isn't asked).

## The PR pass

`lens review --pr <n>` saves to `.review/pr-<n>.json`, with its inputs in `.review/pr-<n>/` (plus
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
`<!-- lens:review head=<sha> -->`, which is how later runs find what's already there.
