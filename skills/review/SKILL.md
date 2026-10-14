---
name: review
description: Review the current change with lens before opening a PR. Fresh-context reviewers look at the diff in parallel, an Opus verifier drops anything it can't show failing, and only blocker and major findings are reported. Use when the user runs /review or asks for a lens review of their branch.
argument-hint: "[<base>] [--intent <file>]"
---

# /review: the local pass

A review in about 5 minutes on a ~100-line diff. You orchestrate; you don't review. Every
judgment about the code comes from a subagent with a fresh context: a reviewer per profile, then
a verifier per finding. **Never pass this conversation to them**: no summary of what the author
was trying to do, no "we changed X because Y". They get the diff, the repo, the repo's guideline
files and the task from `tb peek`, and nothing else.

The local pass runs four profiles: `impact-assessment` (lead, first), then `correctness`,
`testing` and `intent-alignment` in parallel. Other profiles lens picks are left to the PR pass.

Arguments: an optional base ref (default: origin's default branch, then `main`, then `master`)
and `--intent <file>`: a ticket or spec the change implements, for `intent-alignment`.

Keep it moving: every step below that spawns agents spawns them **all in one message** so they
run in parallel. Don't read the repo yourself beyond what a step says.

## 1. Prepare

From the repo being reviewed:

```sh
"${CLAUDE_PLUGIN_ROOT}/skills/review/scripts/prepare.sh" [<base>]
```

It prints a summary (`branch`, `base`, `mergeBase`, `head`, `dirty`, `work`, `report`,
`changedLines`, `map`, `mapSeconds`, `task`) and fills the work folder `.review/<branch>/`:
`profiles.json` (`lens profiles`), `map.json` (`lens map`), `diff.patch`, `task.json` (`tb
peek`, when this terminal has a task) and `started`. `.review/` ignores itself in git.

- An empty diff (`changedLines: 0`): say there's nothing to review and stop.
- `map: false`: the change-map provider failed and `map.err` holds why. Carry on without a map
  (reviewers fall back to grepping), and put the error in the report: a broken provider must be
  noticed, not hidden. lens never falls back to its grep provider on its own.
- `--intent <file>`: copy it into the work folder as `intent.md`.

Also collect the guideline files: `CLAUDE.md` and `AGENTS.md` at the repo root and in the
folders of the changed files (`ls`, don't read them). Reviewers read them themselves.

## 2. Pick the profiles

Read `profiles.json`. Of its `picked` profiles, the local pass keeps only `impact-assessment`,
`correctness`, `testing` and `intent-alignment` (a repo's own `review-profiles/<name>/` override
comes through `path`, so always use that path, never lens's default). A profile lens skipped
(disabled, wrong language) stays skipped.

`decision: "infer"` means lens couldn't judge it from paths; you decide from `diff.patch`:

- `testing`: run it if the change alters behavior (not a pure refactor, rename, docs or config
  tweak) or fixes a bug.
- `intent-alignment`: run it if `task.json` or `intent.md` exists. Otherwise skip it with
  "no task context". It reviews against the task, never against what you know of the authoring
  conversation.
- Any other infer profile: check its `inferenceRules` against the diff.

Note each decision and its one-line reason; they go in the report.

## 3. Lead assessment (impact-assessment)

Spawn one `lens:reviewer` agent with the profile's `model` and `effort` (default `opus`,
`high`), prompt:

```
Profile: <path to impact-assessment's profile.md>
Work folder: <work>  (diff.patch, map.json, task.json, intent.md: whichever exist)
Guidelines: <guideline file paths, or "none">
Base: <base>, merge base <mergeBase>.
You are the lead reviewer. Return the assessment, risk and focusAreas as well as findings.
```

Write its `assessment` to `<work>/impact.md`. Keep its findings for step 5.

## 4. Fan out the reviewers

In **one message**, spawn a `lens:reviewer` per remaining profile, each with that profile's
`model` and `effort`:

```
Profile: <path to the profile.md>
Work folder: <work>  (diff.patch, map.json, task.json, intent.md, impact.md: whichever exist)
Guidelines: <guideline file paths, or "none">
Base: <base>, merge base <mergeBase>.
```

Plus one more `lens:reviewer` (Opus, `high`) for each of the lead's `focusAreas` (at most its
profile's `maxMinions`), using the correctness profile and adding:

```
Focus only on: <title>, in <files>. Why it's risky: <why>.
```

If an agent's reply has no parseable `json` block, note the profile as failed in the report and
move on; don't rerun it.

## 5. Verify

Gather every finding from steps 3 and 4. Before verifying:

- Drop `minor` and `nit`: the local pass reports blocker and major only.
- Merge duplicates (same file, lines within 3 of each other, same failure): keep the highest
  severity and confidence, list every profile that raised it.

Then, in **one message**, spawn a `lens:verifier` agent (Opus) per remaining finding:

```
Work folder: <work>  (diff.patch, map.json)
Finding (from the <profile> reviewer):
  file: <file>
  line: <line>
  severity: <severity>
  title: <title>
  failureScenario: <failureScenario>
```

Keep a finding only when the verifier says `confirmed` **and** its severity is `blocker` or
`major`; take the verifier's severity, `line` and `howItFails`. Everything else is dropped,
with the verifier's reason (an unparseable reply counts as dropped).

## 6. Report

Write `.review/<branch>.json` (the summary's `report` path) in the format in
`${CLAUDE_PLUGIN_ROOT}/docs/REVIEW.md`. Get the timings with `date +%s` at the end of each step
(the start is in `<work>/started`).

Then tell the user, in this shape:

```
lens: 2 findings (1 blocker, 1 major) on feat/x vs origin/main · 4m 12s · risk moderate

blocker  src/parser.rs:42  empty input parses as a valid token  [correctness]
  parse("") returns Ok(Token::empty()), so login() accepts a blank password
  1. login() (src/auth.rs:30) passes the raw field to parse() ...

Ran: impact-assessment, correctness, testing. Skipped: intent-alignment (no task context).
Dropped 3 findings the verifier couldn't show failing; 5 below major.
Report: .review/feat/x.json
```

With no findings, say so in one line plus the `Ran` / `Dropped` lines. Don't fix anything:
the review reports, and fixing is a separate step the user (or the fix loop) chooses.
