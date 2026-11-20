# Review profiles

Code-review reviewer profiles. Each profile is one specialist reviewer: a focus, a model, and the
rules for when it runs and how hard it grades. A review runner picks the profiles that apply to a
change and runs each one over the diff with read access to the whole repo.

Each profile is a folder:

```
<name>/
  item.json     catalog entry: {"name", "conformsTo": "reviewer@1", "description"}
  profile.md    YAML frontmatter + the reviewer's instructions (markdown body)
```

## Replacing and adding profiles

The profiles here are the defaults. Two layers go over them, matched by `name`: a profile with a
default's name replaces it, and a new name adds a reviewer.

1. The reviewed repo's `review-profiles/<name>/profile.md`.
2. Folders outside the repo, for repos you can't add files to: `lens review --profiles <dir>`
   (repeatable) or `$LENS_PROFILE_OVERRIDES` (a `:`-separated path list). They win over the
   repo's own profiles, and a later folder wins over an earlier one. `--profiles` replaces the
   variable.

An override folder is laid out like this one (`<name>/profile.md`; `item.json` is optional). To
change one reviewer, copy its folder and edit it, since an override replaces the whole profile,
frontmatter and prompt alike. To turn a reviewer off, copy it with `enabled: false`.
`$LENS_PROFILES_DIR` is different: it replaces this whole set.

## Frontmatter

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Profile id; matches the folder and `item.json`. |
| `description` | yes | One line on what the reviewer looks for. |
| `model` | yes | Model to run it on. Every built-in profile uses `sonnet`; set `opus` to opt one back in (focus-area reviewers follow `correctness`). |
| `effort` | yes | Reasoning effort (`low` … `max`). |
| `severityGuidance` | yes | What counts as `blocker` / `major` / `minor` / `nit` for this reviewer. |
| `enabled` | yes | `false` keeps the profile but never runs it. |
| `languages` | no | Run only when the change touches these languages. |
| `globs` | no | Run when a changed path matches one of these globs. |
| `pathKeywords` | no | Run when a changed path contains one of these words. |
| `inferenceRules` | no | Plain-language conditions; the runner asks a model whether the change meets any. |
| `dispatch` | no | `true` lets the profile emit focus areas that spawn narrow follow-up ("minion") reviews. |
| `maxMinions` | no | Cap on minion reviews a dispatching profile may spawn. |

A profile with no `languages`, `globs`, `pathKeywords` or `inferenceRules` runs on every change.
The body after the frontmatter is the reviewer's prompt.

## Profiles

General:

- `impact-assessment` — lead assessment (impact, blast radius, risk, needs human judgment); runs first and may dispatch minions.
- `correctness` — logic errors, races, leaks, null handling, async mistakes.
- `architecture` — structural changes that cost something now: broken boundaries, a second path for an existing job, APIs that trap callers. Returns nothing for diffs that touch no structure.
- `intent-alignment` — does the change satisfy the linked ticket / design, without scope creep.
- `testing` — coverage of the change, brittle tests, mock-vs-reality mismatches.
- `security` — injection, validation, authn/authz, secrets, unsafe deserialization.
- `performance` — N+1, hot-path allocation, blocking I/O, complexity regressions.
- `dry-reuse` — duplicated logic that already differs or must stay in step, and repo helpers the diff rewrites instead of calling.
- `coding-guidelines` — the repo's own written conventions.

Dart / Flutter:

- `dart-style` — Dart idioms and house style.
- `flutter-architecture` — layered architecture, bloc/Cubit state, DI, no `BuildContext` passing.
- `flutter-widgets` — widget construction performance and lifecycle.
- `flutter-localization` — ARB / l10n conventions.

Run by the PR pass itself (`enabled: false` keeps the selector from picking it; leave it so):

- `stack-integration` — where child PRs merged into this PR's branch meet each other and the parent. The PR pass runs it when it takes reviewed child PRs out of the delta (docs/REVIEW.md, "Stacked PRs").

Turned off (`enabled: false`; set it to `true` to run it again):

- `naming-intuitivity` — names and signatures that mislead a reader. Off because naming is a matter of preference.
