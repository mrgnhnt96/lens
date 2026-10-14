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

## Frontmatter

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Profile id; matches the folder and `item.json`. |
| `description` | yes | One line on what the reviewer looks for. |
| `model` | yes | Model to run it on (`opus`, `sonnet`, ...). |
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
- `architecture` — layering, coupling, separation of concerns, pattern consistency.
- `intent-alignment` — does the change satisfy the linked ticket / design, without scope creep.
- `testing` — coverage of the change, brittle tests, mock-vs-reality mismatches.
- `security` — injection, validation, authn/authz, secrets, unsafe deserialization.
- `performance` — N+1, hot-path allocation, blocking I/O, complexity regressions.
- `dry-reuse` — duplicated logic and missed reuse of existing helpers.
- `naming-intuitivity` — names and signatures that mislead a reader.
- `coding-guidelines` — the repo's own written conventions.

Dart / Flutter:

- `dart-style` — Dart idioms and house style.
- `flutter-architecture` — layered architecture, bloc/Cubit state, DI, no `BuildContext` passing.
- `flutter-widgets` — widget construction performance and lifecycle.
- `flutter-localization` — ARB / l10n conventions.
