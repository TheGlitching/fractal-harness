# Working agreements for the fractal repo
- The authoritative spec is docs/SPEC.md. The design contracts are in
  contracts/. The current objective is the contract named in your prompt;
  nothing else is in scope.
- The canonical implementation is Rust (`src/*.rs`), built with Cargo. There is
  one `fractal` binary; do not reintroduce a second implementation.
- Tests are the acceptance criteria. Never weaken, skip, or delete a test to
  make it pass. If a test seems wrong, write your reasoning in notes.md and stop.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`
  before finishing. `tests/legacy-python/` preserves the original Python
  acceptance suite as the porting reference for intended scheduler semantics;
  CI does not run it.
- Nodes execute one at a time on the shared working tree; per-node diffs,
  verification and commits rely on that. Do not re-enable concurrent node
  execution without solving diff/commit attribution first, and keep the
  harness's own paths out of the user's repo. See README "Per-node isolation and
  workspace exclusion".
- State layout, verbs, and terminology must match docs/SPEC.md §4 exactly:
  contract.md, decisions.md, log/, artifacts/, children/, and the verbs
  split / complete / escalate / note_global.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
