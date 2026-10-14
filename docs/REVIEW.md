# The review file

`/review` saves each pass to `.review/<branch>.json` in the reviewed repo (a detached HEAD is
`detached-<short sha>`). The fix loop and the PR pass read it. `.review/` holds a `.gitignore`
of `*`, so it never reaches a commit. The pass's inputs sit beside it in `.review/<branch>/`:
`diff.patch`, `map.json`, `profiles.json`, `task.json`, `impact.md`.

```json
{
  "version": 1,
  "pass": "local",
  "branch": "feat/x",
  "base": "origin/main",
  "mergeBase": "71fae7c…",
  "head": "ff379e4…",
  "dirty": true,
  "changedLines": 104,
  "startedAt": "2026-10-14T19:40:02Z",
  "finishedAt": "2026-10-14T19:44:14Z",
  "seconds": { "total": 252, "prepare": 6, "lead": 71, "reviewers": 118, "verify": 57 },
  "map": { "provider": "grep", "error": null },
  "risk": "moderate",
  "assessment": "**Impact** — …",
  "profiles": [
    { "name": "impact-assessment", "ran": true, "model": "opus", "reason": "always", "findings": 0 },
    { "name": "correctness", "ran": true, "model": "opus", "reason": "always", "findings": 3 },
    { "name": "testing", "ran": true, "model": "sonnet", "reason": "inference: changes behavior", "findings": 2 },
    { "name": "intent-alignment", "ran": false, "reason": "no task context" },
    { "name": "focus: token parsing", "ran": true, "model": "opus", "reason": "lead focus area", "findings": 1 }
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

- `pass`: `local` for `/review`.
- `dirty`: the working tree had uncommitted or untracked changes; they were reviewed too.
- `findings`: blocker and major only, every one confirmed by a verifier. `severity` is the
  verifier's, `line` the verifier's when it moved it. Ids are `F1`, `F2`, … in report order
  (blockers first).
- `dropped`: everything else, with why: `below major` or `unverified: <the verifier's reason>`.
  Duplicates raised by several profiles are merged into one entry listing them all.
- `map.error`: the provider's error when `lens map` failed; the pass ran without a map.
- `profiles[].ran: false` with `reason`: a local-pass profile that didn't apply. Profiles left
  to the PR pass aren't listed.
