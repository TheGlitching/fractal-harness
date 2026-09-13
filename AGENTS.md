# Working agreements for the fractal repo
- The authoritative spec is docs/SPEC.md. The design contracts are in
  contracts/. The current objective is the contract named in your prompt;
  nothing else is in scope.
- The canonical implementation is Rust (`src/*.rs`), built with Cargo. There is
  one `fractal` binary; do not reintroduce a second implementation.
- Tests are the acceptance criteria. Never weaken, skip, or delete a test to
  make it pass. If a test seems wrong, write your reasoning in notes.md and stop.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`
  before finishing.
- Nodes whose dependencies are satisfied run concurrently (up to
  `FRACTAL_PARALLEL`, default 4), each in its own git worktree under
  `.fractal/worktrees/`, so each node's diff, verification and commit are
  exactly its own; verified commits are cherry-picked back onto the shared tree
  in batch order. Node memory stays in the shared `tree/`, never in a worktree.
  `FRACTAL_PARALLEL=1` keeps the original single-tree serial path. Keep the
  harness's own paths out of the user's repo. See README "Per-node isolation and
  workspace exclusion".
- State layout, verbs, and terminology must match docs/SPEC.md §4 exactly:
  contract.md, decisions.md, log/, artifacts/, children/, and the verbs
  split / complete / escalate / note_global.
- The web dashboard (`fractal serve`, src/dashboard.rs) is a second view over the
  same `Store`, not a parallel state path. Route every dashboard read/mutation
  through `Store` methods (or a new one there), never through ad-hoc file or SQL
  access, so TUI, CLI and dashboard state stay identical. Its loopback/read-only
  defaults and `--bind-all` risk are documented in README "Web dashboard". The
  page has no steering controls: the graph is design and navigation (zoom / pan /
  select), the butler chat is the only action surface, and node detail is
  read-only. A graph selection rides to the butler as the optional `node` field
  on `POST /api/butler`; if a control is ever removed, its capability must
  already exist as a butler tool.
- The butler (`fractal ask` / `fractal butler`, src/butler.rs) is the one
  steering agent. It lives outside the task tree, is not a node, and the root and
  every node stay ordinary nodes - no node owns an architect role. Every butler
  tool routes through `Store` (or the scheduler), never a parallel state path. A
  correction reopens or adds only the nodes it must; it never replays the whole
  tree. See README "Talk to the butler".

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
