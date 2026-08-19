#!/usr/bin/env bash
# Run this project's verification gates and report the result.
#
# This is the gate that decides whether work is real. It runs the project's own
# commands and trusts only their exit codes, because prose describing a feature
# and a feature existing are different things.
#
# Prints one PASS/FAIL line per gate. Exits non-zero on the first failure, with
# the failing command's output, so the caller has something actionable.
#
# Usage: fractal-verify.sh [project-dir]
#   project-dir  defaults to the current directory
set -euo pipefail

PROJECT="${1:-$PWD}"

if [ ! -d "$PROJECT" ]; then
  echo "fractal-verify: no such directory: $PROJECT" >&2
  exit 2
fi

cd "$PROJECT"

gates=()

if [ -f package.json ]; then
  [ -f tsconfig.json ] && gates+=("npx tsc --noEmit")
  grep -q '"build"' package.json && gates+=("npm run build")
  grep -q '"test"' package.json && gates+=("npm test --silent")
fi

if [ -f Cargo.toml ]; then
  gates+=("cargo check --all-targets")
  gates+=("cargo test")
fi

if [ -f pyproject.toml ] && [ -d tests ]; then
  gates+=("python -m pytest -q")
fi

if [ ${#gates[@]} -eq 0 ]; then
  echo "fractal-verify: no gates detected in $PROJECT"
  exit 0
fi

failed=0
for gate in "${gates[@]}"; do
  echo "--- \$ $gate"
  if output=$(eval "$gate" 2>&1); then
    echo "PASS $gate"
  else
    echo "FAIL $gate"
    # The tail carries the actual failure summary for compilers and runners.
    echo "$output" | tail -n 60
    failed=1
    break
  fi
done

if [ "$failed" -ne 0 ]; then
  echo "" >&2
  echo "fractal-verify: verification FAILED in $PROJECT" >&2
  exit 1
fi

echo ""
echo "fractal-verify: all ${#gates[@]} gate(s) passed"
