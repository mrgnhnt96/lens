---
name: review
description: Review the current change with lens before opening a PR. Fresh-context reviewers look at the diff in parallel, an Opus verifier drops anything it can't show failing, only blocker and major findings are reported, and blockers go through a fix loop. Use when the user runs /review or asks for a lens review of their branch. `/review --pr <n>` runs the PR pass instead: every matching profile, all severities, posted on the PR.
argument-hint: "[<base>] [--intent <file>] [--no-fix] [--full] | --pr <n> [--dry-run]"
---

# /review

The whole pass runs in one command, `lens review`, which starts every reviewer, verifier and
fixer as its own headless Claude session. You only run it and relay what it prints. Don't
review the code yourself, don't read the report or the diff first, and never pass this
conversation to it: the reviewers judge the change fresh.

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/lens" review <the arguments you were given>
```

It takes a few minutes (longer when it fixes a blocker), so run it with Bash's
`run_in_background: true` and wait for it to finish; don't poll it. Progress goes to stderr; the
summary it prints on stdout is what the user sees. Print that summary as it is.

Arguments, passed through unchanged:

- `<base>`: the base ref (default: origin's default branch, then `main`, then `master`).
- `--intent <file>`: a ticket or spec the change implements, for the intent-alignment reviewer.
- `--no-fix`: report blockers without running the fix loop.
- `--full`: review the whole change, not just what changed since the last review.
- `--pr <n>`: the PR pass. Every profile lens picks, all severities, posted on the PR as one
  review with inline comments (`--dry-run` builds it without posting). It runs headless
  (`claude -p "/review --pr 12"`), so never ask a question and never wait for an answer.

What it does, so you can answer questions about it:

- **Local pass**: `correctness` (Opus) always; `testing` and, with a task or `--intent`,
  `intent-alignment` (Sonnet) when the change is more than `[review] small_lines` lines (40 by
  default); nothing at all when only docs changed. Every blocker and major is verified by an
  Opus verifier; blockers then go to the fix loop (a fixer, a re-review, a Haiku judge, at most 5
  attempts). The report is `.review/<branch>.json`, in the format in
  `${CLAUDE_PLUGIN_ROOT}/docs/REVIEW.md`.
- **Again on the same branch**: an unchanged tree isn't reviewed again (it only resumes an open
  fix loop); a changed one reviews only what changed since the last review and carries the last
  report's findings over, verifying again the unfixed blockers and the majors in files it touched.
- **PR pass**: the `impact-assessment` lead first, then every other profile plus a reviewer per
  focus area the lead names, all verified, then `lens pr post`.

If it fails, print its error. A failed post in the PR pass leaves the report in place and prints
the command that retries the post.
