#!/usr/bin/env bash
# Gather what a /review pass needs into .review/<branch>/ and print a summary as JSON.
#
#   prepare.sh [<base>]
#
# <base> defaults to origin's default branch, then main, then master. Writes:
#   profiles.json  `lens profiles --base <base>`
#   map.json       `lens map --base <base>` (map.err instead when the provider fails)
#   diff.patch     the change since the merge base: commits, staged, unstaged and untracked files,
#                  as a full-index diff that `git apply --cached` replays onto the merge base
#   task.json      `tb peek`, when tb is installed and this terminal has a task
#   started        the start time, in epoch seconds
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd -P)
lens=${LENS:-$here/../../../bin/lens}

root=$(git rev-parse --show-toplevel)
cd "$root"

base=${1:-}
if [ -z "$base" ]; then
  base=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)
fi
if [ -z "$base" ]; then
  for b in main master; do
    if git rev-parse --verify --quiet "$b^{commit}" >/dev/null; then base=$b; break; fi
  done
fi
[ -n "$base" ] || { echo "prepare: no base; pass one (e.g. origin/main)" >&2; exit 2; }

merge_base=$(git merge-base "$base" HEAD)
branch=$(git symbolic-ref --quiet --short HEAD 2>/dev/null || echo "detached-$(git rev-parse --short HEAD)")
work=.review/$branch
mkdir -p "$work"
# Reviews are local output: keep the whole folder out of git without touching the repo's .gitignore.
[ -f .review/.gitignore ] || printf '*\n' > .review/.gitignore
rm -f "$work/map.err" "$work/task.json"
date +%s > "$work/started"

"$lens" profiles --base "$base" > "$work/profiles.json"

map_ok=true
map_start=$(date +%s)
if ! "$lens" map --base "$base" > "$work/map.json" 2> "$work/map.err"; then
  map_ok=false
  rm -f "$work/map.json"
else
  rm -f "$work/map.err"
fi
map_secs=$(( $(date +%s) - map_start ))

# The working tree as a tree object (untracked files in, ignored ones out), so diff.patch is one
# full-index diff from the merge base. `git apply --cached` of it onto the merge base rebuilds this
# tree exactly, binary files included (their blobs are stored by `git add`): that's how tb checks
# the review is still fresh. See docs/REVIEW.md.
tmp=$(mktemp -d "${TMPDIR:-/tmp}/lens-prepare.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
index=$tmp/index
GIT_INDEX_FILE=$index git read-tree HEAD
GIT_INDEX_FILE=$index git add -A .
tree=$(GIT_INDEX_FILE=$index git write-tree)
git diff --no-color --no-ext-diff --no-textconv --full-index --src-prefix=a/ --dst-prefix=b/ \
  "$merge_base" "$tree" -- > "$work/diff.patch"
lines=$(grep -c '^[+-]' "$work/diff.patch" || true)
headers=$(grep -cE '^(\+\+\+|---) ' "$work/diff.patch" || true)
changed=$(( ${lines:-0} - ${headers:-0} ))

task=false
if command -v tb >/dev/null 2>&1 && tb peek > "$work/task.json" 2>/dev/null \
  && grep -q '"task":{' "$work/task.json"; then
  task=true
else
  rm -f "$work/task.json"
fi

if git diff --quiet HEAD -- && [ -z "$(git ls-files --others --exclude-standard)" ]; then
  dirty=false
else
  dirty=true
fi

cat <<EOF
{
  "root": "$root",
  "branch": "$branch",
  "base": "$base",
  "mergeBase": "$merge_base",
  "head": "$(git rev-parse HEAD)",
  "dirty": $dirty,
  "work": "$root/$work",
  "report": "$root/.review/$branch.json",
  "changedLines": $changed,
  "map": $map_ok,
  "mapSeconds": $map_secs,
  "task": $task
}
EOF
