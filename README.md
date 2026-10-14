# lens

Agent code review as a Claude Code plugin. Fresh-context reviewers look at a
change in parallel, a verifier drops anything it can't show failing, and only
real problems reach you.

- **Local pass** (`/review`): about 5 minutes, before you open a PR.
- **PR pass**: about 15 minutes, run without a terminal, posting findings on the PR.

Works in any repo. Taskboard calls it at `tb done` and from its PR flow, but
lens doesn't depend on taskboard.

Plan: https://claude.ai/code/artifact/a12f42b8-ee3b-41ee-953b-94b9091d47b1
