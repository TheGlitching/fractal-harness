#!/usr/bin/env bash
# Show what a node actually changed, from git rather than from its own claims.
#
# An integrating parent needs this: before it can assemble its children's work,
# it has to see the real diff, not each child's summary of itself. A node that
# reports "implemented the settings modal" and a node that created the file are
# indistinguishable in prose and obvious here.
#
# Usage: fractal-node-diff.sh [--stat] [node-id] [project-dir]
#   node-id      commit subjects are "<node-id>: <summary>"; omit to list all
#                node commits
#   --stat       summary only, no patch body
set -euo pipefail

STAT=0
if [ "${1:-}" = "--stat" ]; then
  STAT=1
  shift
fi

NODE_ID="${1:-}"
PROJECT="${2:-$PWD}"

cd "$PROJECT"

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "fractal-node-diff: $PROJECT is not a git repository" >&2
  exit 2
fi

if [ -z "$NODE_ID" ]; then
  echo "Node commits in $PROJECT:"
  git log --oneline --no-decorate | grep -E '^[0-9a-f]+ (root|root-[0-9-]+):' || {
    echo "(no node commits yet)"
    exit 0
  }
  exit 0
fi

# A node may have been reopened and recommitted, so there can be several.
shas=$(git log --format='%H %s' | awk -v n="$NODE_ID:" '$2==n {print $1}')

if [ -z "$shas" ]; then
  echo "fractal-node-diff: no commit found for node '$NODE_ID'" >&2
  echo "hint: run without a node id to list known node commits" >&2
  exit 1
fi

for sha in $shas; do
  short="${sha:0:8}"
  echo "=== $NODE_ID @ $short"
  if [ "$STAT" -eq 1 ]; then
    git show --stat --oneline "$sha" | tail -n +2
  else
    git show "$sha"
  fi
  echo ""
done
