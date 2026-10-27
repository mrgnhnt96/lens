#!/usr/bin/env bash
# Gather what a `lens review --pr <n>` pass needs into $LENS_STATE/pr-<n>/ and print a summary as
# JSON.
#
#   LENS_STATE=<dir> prepare-pr.sh <n>
#
# Fetches the PR's head and base from origin. When the checkout is already at the PR's head with
# a clean tree it reviews in place; otherwise it checks the head out detached in
# $LENS_STATE/pr-<n>/tree, so the user's checkout is never touched. Writes:
#   pr.json        `gh pr view` (number, url, title, body, baseRefName, headRefName, headRefOid)
#   intent.md      the PR's title and description, for intent-alignment
#   profiles.json  `lens profiles --base origin/<base>`
#   map.json       `lens map --base origin/<base>` (map.err instead when the provider fails)
#   diff.patch     the PR's change since the merge base
#   posted.json    `lens pr earlier`: lens findings already on the PR from earlier runs
#   started        the start time, in epoch seconds
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd -P)
lens=${LENS:-lens}
gh=${LENS_GH:-gh}
state=${LENS_STATE:?prepare-pr: set LENS_STATE to the state folder of the review}

pr=${1:-}
case $pr in
  ''|*[!0-9]*) echo "prepare-pr: pass the PR number (prepare-pr.sh 12)" >&2; exit 2 ;;
esac

root=$(git rev-parse --show-toplevel)
cd "$root"

work=$state/pr-$pr
mkdir -p "$work"
rm -f "$work/map.err"
date +%s > "$work/started"

fields=number,url,title,body,baseRefName,headRefName,headRefOid
"$gh" pr view "$pr" --json "$fields" > "$work/pr.json"
field() { "$gh" pr view "$pr" --json "$fields" --jq ".$1"; }
base_ref=$(field baseRefName)
head_ref=$(field headRefName)
head=$(field headRefOid)
url=$(field url)
{
  printf '# %s\n\n' "$(field title)"
  field body
} > "$work/intent.md"

git fetch --quiet origin "+refs/heads/$base_ref:refs/remotes/origin/$base_ref" \
  "+refs/pull/$pr/head:refs/lens/pr/$pr"
base=origin/$base_ref
merge_base=$(git merge-base "$base" "$head")

# Review in place only when the checkout is exactly the PR's head; never move the user's checkout.
if [ "$(git rev-parse HEAD)" = "$head" ] && git diff --quiet HEAD -- \
  && [ -z "$(git ls-files --others --exclude-standard)" ]; then
  tree=$root
else
  tree=$work/tree
  git worktree remove --force "$tree" 2>/dev/null || rm -rf "$tree"
  git worktree prune
  git worktree add --quiet --detach "$tree" "$head"
fi

"$lens" profiles --repo "$tree" --base "$base" > "$work/profiles.json"

map_ok=true
map_start=$(date +%s)
if ! "$lens" map --repo "$tree" --base "$base" > "$work/map.json" 2> "$work/map.err"; then
  map_ok=false
  rm -f "$work/map.json"
else
  rm -f "$work/map.err"
fi
map_secs=$(( $(date +%s) - map_start ))

git diff "$merge_base" "$head" -- > "$work/diff.patch"
lines=$(grep -c '^[+-]' "$work/diff.patch" || true)
headers=$(grep -cE '^(\+\+\+|---) ' "$work/diff.patch" || true)
changed=$(( ${lines:-0} - ${headers:-0} ))

"$lens" pr earlier --repo "$root" --pr "$pr" > "$work/posted.json"

cat <<EOF
{
  "root": "$root",
  "pr": $pr,
  "url": "$url",
  "branch": "$head_ref",
  "base": "$base",
  "mergeBase": "$merge_base",
  "head": "$head",
  "tree": "$tree",
  "work": "$work",
  "report": "$state/pr-$pr.json",
  "changedLines": $changed,
  "map": $map_ok,
  "mapSeconds": $map_secs
}
EOF
