# /review --pr <n>: the PR pass

The full review of a pull request, posted on it as one GitHub review. About 15 minutes. It runs
headless (`claude -p "/review --pr 12"`), so **never ask a question and never wait for an
answer**: decide, note why in the report, and carry on. The rules of the local pass still hold:
you orchestrate, subagents judge, and they never see this conversation.

What differs from the local pass:

- **Every profile lens picks runs**, not just the local four: `impact-assessment` (lead, first),
  then all the others in parallel, plus the lead's focus areas.
- **All severities are reported.** Every finding is verified, minor and nit too; on the PR,
  minor and nit comments are collapsed.
- **Findings are posted** as inline comments on the PR's diff by `lens pr post`, as the account
  `.lens.toml` names (`[pr] post_as`).
- **Earlier runs count.** A finding lens already posted on this PR isn't raised again.
- **No fix loop.** The PR pass reports; it never edits the PR's code (step 7 of the local pass
  doesn't apply).

## 1. Prepare

From the repo the PR belongs to:

```sh
"${CLAUDE_PLUGIN_ROOT}/skills/review/scripts/prepare-pr.sh" <n>
```

It prints a summary (`pr`, `url`, `branch`, `base`, `mergeBase`, `head`, `tree`, `work`,
`report`, `changedLines`, `map`, `mapSeconds`) and fills `.review/pr-<n>/`: `pr.json`,
`intent.md` (the PR's title and description), `profiles.json`, `map.json` (or `map.err`),
`diff.patch`, `posted.json` (findings earlier runs posted) and `started`. `tree` is the checkout
of the PR's head that reviewers read: the repo itself when it's already there and clean,
otherwise a detached worktree in the work folder. The user's checkout is never moved.

- A failed script (no `gh` login, PR not found): print its error and stop.
- `changedLines: 0`: say there's nothing to review and stop; post nothing.
- `map: false`: carry on without a map and put `map.err` in the report, as in the local pass.

Collect the guideline files (`CLAUDE.md`, `AGENTS.md` at the root of `tree` and in the changed
files' folders) with `ls`.

## 2. Pick the profiles

Every `picked` profile in `profiles.json` runs, through its `path`. For `decision: "infer"`,
decide from `diff.patch` as the local pass does (`intent-alignment` always has `intent.md` here,
so it runs unless the PR has no description). Note each decision and its reason.

## 3. Lead, reviewers, verify

As steps 3 to 5 of the local pass, with these changes:

- Point every reviewer at `tree` as the repo, and add to each prompt:

  ```
  Repo: <tree>  (the PR's head; read files there, not in the current directory)
  Already posted on this PR (don't raise these again): <work>/posted.json
  ```

- The lead's focus areas get up to its profile's `maxMinions` extra correctness reviewers.
- Keep every severity: drop nothing for being minor or nit. Merge duplicates as before.
- Verify every finding (one `lens:verifier` each, all in one message, with `Repo: <tree>`). Keep
  only `confirmed`, with the verifier's severity, `line` and `howItFails`. Unconfirmed findings
  go in `dropped` with the verifier's reason.

## 4. Report and post

Write the report (the summary's `report`, `.review/pr-<n>.json`) in the format in
`${CLAUDE_PLUGIN_ROOT}/docs/REVIEW.md`, with `pass: "pr"` and `pr: {"number", "url"}`. Then post:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/lens" pr post --pr <n> --report <report> --diff <work>/diff.patch
```

It posts one review (`COMMENT`, never approve or request changes) at the reviewed head: a
summary body, plus an inline comment per finding on a line the diff shows. Findings on lines
outside the diff go in the body. It leaves out anything an earlier run posted (same finding, or
the same file within 3 lines), and posts nothing at all when there's nothing new and an earlier
lens review is already on the PR. It prints what it did as JSON (`posted`, `review`, `inline`,
`inBody`, `skipped`). Add `--dry-run` to see the payload without posting.

If it fails (no bot token, GitHub rejects the review), print the error and leave the report in
place, so the post can be retried by hand with the same command.

Finish with a short summary:

```
lens: 4 findings (1 major, 2 minor, 1 nit) on PR #12 · 13m 40s · risk moderate
Posted as owner: https://github.com/o/r/pull/12#pullrequestreview-1 (3 inline, 1 in the body; 2 already posted)
Ran: impact-assessment, correctness, testing, security, naming-intuitivity. Skipped: dart-style (no Dart files).
Report: .review/pr-12.json
```
