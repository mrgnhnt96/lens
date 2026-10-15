# Change-map providers

`lens map --base <ref>` prints the **change map**: what a change touches, which symbols it adds,
edits or removes, who calls them and which tests cover them. Reviewers use it to look past the
diff at the code the change can break.

A **provider** builds the map. lens ships one, `grep`, that works in any repo. A repo that has
something better (rust-analyzer, graphify, a language server, a build graph) names its own
provider in `.lens.toml`, and lens runs it instead.

## Choosing a provider

`.lens.toml` at the repo root:

```toml
[map]
# "grep" (the default), a program, or a program and its arguments.
provider = ["tools/lens-map-ra", "--workspace"]
# How the provider gets the changed files: "stdin" (default) or "args".
input = "stdin"
```

A program path with a `/` in it is relative to the repo root; a bare name is looked up on
`PATH`. `lens map --provider <grep|program>` overrides `.lens.toml` for one run.

## The contract (version 1)

### Input

lens runs the provider **from the repo root** with:

| What | Value |
| --- | --- |
| stdin (`input = "stdin"`) | the changed files, one repo-relative path per line |
| trailing args (`input = "args"`) | the changed files, after the configured arguments; stdin is empty |
| `LENS_MAP_VERSION` | `1` |
| `LENS_BASE` | the `--base` ref as given |
| `LENS_MERGE_BASE` | the commit the change is measured from (merge base of the base and `HEAD`) |
| `LENS_ROOT` | the repo root, absolute |

The changed files are everything that differs between `LENS_MERGE_BASE` and the working tree:
commits on the branch, staged and unstaged edits, untracked (not ignored) files and deleted
files. Renames show up as the old path and the new one. Use `git diff $LENS_MERGE_BASE` to see
the lines.

### Output

One JSON object on stdout, exit 0:

```json
{
  "version": 1,
  "symbols": [
    {
      "name": "parse_token",
      "kind": "function",
      "file": "src/parser.rs",
      "line": 12,
      "change": "modified",
      "callers": [{ "file": "src/server.rs", "line": 40, "text": "let t = parse_token(raw)?;" }],
      "tests": [{ "file": "tests/parser.rs", "line": 8 }],
      "truncated": false
    }
  ],
  "tests": [
    {
      "file": "tests/parser.rs",
      "changed": false,
      "symbols": ["parse_token"],
      "namedAfter": ["src/parser.rs"]
    }
  ]
}
```

- `version`: optional; when present it must be `1`.
- `symbols`: the symbols the change adds, edits or removes.
  - `name`, `kind` (free text: `function`, `struct`, `class`, `method`, ...), `file`, `line`
    (1-based) and `change` (`added`, `modified` or `removed`) are required.
  - `file` and `line` are where the symbol is defined in the working tree, or at the merge base
    when it was `removed`.
  - `callers`: places outside tests that use the symbol. `tests`: places in tests that use it.
    Each is `{file, line, text?}`; `text` is the source line, trimmed. Both default to `[]`.
  - `truncated: true` says some callers or tests were left out.
- `tests`: test files that cover the change, each with any of `changed` (the change edits the
  test), `symbols` (changed symbols it uses) and `namedAfter` (changed files it's named after).
  Defaults to `[]`.

Missing lists default to empty and unknown fields are ignored, so a provider can start with just
`symbols` and grow. Anything else (a non-zero exit, output that isn't that shape, another
`version`) fails `lens map` with the provider's stderr: lens doesn't fall back to `grep`, so a
broken provider is noticed.

lens wraps the output with what it knows itself, and that's what `lens map` prints:

```json
{
  "version": 1,
  "base": "origin/main",
  "mergeBase": "8027773…",
  "root": "/path/to/repo",
  "provider": "tools/lens-map-ra --workspace",
  "changedFiles": [{ "path": "src/parser.rs", "status": "modified", "language": "rust" }],
  "symbols": [ … ],
  "tests": [ … ]
}
```

`status` is `added`, `modified` or `deleted`; `language` is left out when lens doesn't know it.

## The built-in `grep` provider

No setup, any language lens recognises, and approximate:

- **Changed symbols**: `git diff -U0` against the merge base. A line that matches a definition
  pattern for its language (`fn x`, `def x`, `class X`, `func x`, ...) is a changed symbol; any
  other changed line belongs to the nearest less-indented definition above it (at the merge base
  for removed lines). Definitions that only exist on removed lines are `removed`.
- **Callers**: `git grep -w -F <name>` over tracked and untracked files. That's every whole-word
  mention, so a common name over-reports and a call through an alias or a macro is missed.
- **Tests**: a mention is a test when its file looks like a test (a `test`, `tests`, `spec`,
  `__tests__` or `integration_test` folder, or a name like `foo_test.go`, `test_foo.py`,
  `foo.test.ts`, `foo_spec.rb`, `FooTest.java`) or follows `#[cfg(test)]` in a Rust file. Test
  files named after a changed file (`src/foo.rs` → `tests/foo.rs`, `foo_test.dart`) are listed
  too.
- Limits: 200 symbols, 50 callers and 50 tests per symbol.

## Writing a provider

Anything that reads stdin and prints JSON works. A sketch for rust-analyzer or graphify:

1. Read the changed files and `git diff -U0 "$LENS_MERGE_BASE"` to get the changed lines.
2. Ask the tool for the symbols whose definitions span those lines (document symbols), then for
   each one's references (find-references or the graph's in-edges).
3. Split references into callers and tests by the tool's own notion of tests (`#[test]`,
   test targets, graph tags).
4. Print the object above.

Keep it fast: `lens review` calls `lens map` on every local pass, inside a 5-minute budget.
