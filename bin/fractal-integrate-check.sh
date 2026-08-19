#!/usr/bin/env bash
# Find modules that exist but nothing imports: the signature failure of a
# fractal tree.
#
# Every child can pass its own contract and its own unit tests while the feature
# as a whole does not exist, because nothing ever wired the pieces together. A
# real project shipped exactly that way: a settings screen no code referenced, a
# panel left as a hardcoded mock, and a component that was still a stub.
#
# This is advisory, not a gate. It reports suspects for an integrating parent to
# judge; entry points and type-only modules are legitimately unreferenced.
#
# Usage: fractal-integrate-check.sh [project-dir]
set -euo pipefail

PROJECT="${1:-$PWD}"
cd "$PROJECT"

if [ ! -d src ]; then
  echo "fractal-integrate-check: no src/ directory in $PROJECT"
  exit 0
fi

# Entry points are referenced by manifests/HTML, not by other modules.
is_entry() {
  case "$1" in
    */main.tsx|*/main.ts|*/index.html|*/background/index.ts|*/content/index.ts) return 0 ;;
    *.d.ts) return 0 ;;
    *) return 1 ;;
  esac
}

orphans=0
stubs=0

while IFS= read -r file; do
  is_entry "$file" && continue

  base=$(basename "$file")
  stem="${base%.*}"

  # A barrel is imported by its directory (`from '../engine'`), never by the
  # literal name `index`, so match on the containing directory instead.
  if [ "$stem" = "index" ]; then
    stem=$(basename "$(dirname "$file")")
  fi

  # Any import referencing this module by stem, from a file other than itself.
  # The specifier is separated from `from`/`import` by whitespace, so the pattern
  # must allow it; without that every real import is missed and every module
  # looks orphaned.
  if ! grep -rqE "(from|import|require)[[:space:]]*\(?[[:space:]]*['\"][^'\"]*${stem}['\"]" \
      --include='*.ts' --include='*.tsx' --include='*.js' --include='*.jsx' \
      src tests 2>/dev/null; then
    echo "ORPHAN   $file  (nothing imports it)"
    orphans=$((orphans + 1))
  fi
done < <(find src -type f \( -name '*.ts' -o -name '*.tsx' \) | sort)

# A component whose body is a single empty element is a placeholder that will
# pass a "does it render" test and deliver nothing.
while IFS= read -r file; do
  if grep -qE 'return <[a-zA-Z]+ [^>]*/>;?\s*$' "$file" 2>/dev/null &&
     [ "$(wc -l < "$file")" -lt 12 ]; then
    echo "STUB     $file  (renders an empty element)"
    stubs=$((stubs + 1))
  fi
done < <(find src -type f -name '*.tsx' | sort)

echo ""
if [ "$orphans" -eq 0 ] && [ "$stubs" -eq 0 ]; then
  echo "fractal-integrate-check: every module is referenced; no obvious stubs"
else
  echo "fractal-integrate-check: $orphans unreferenced, $stubs stub-like"
  echo "Wire them in, or reopen the child that owns them:"
  echo "  fractal reopen --children <id> --reason \"<what is missing>\""
fi
