# Phase 0 Contract: Persistent Tree Skeleton
Goal: a runnable harness that recursively decomposes a task into a
persistent on-disk tree and executes leaves, surviving process death.

Deliverables:
- src/fractal/store.py    — tree store: create/read/update nodes as
  directories (contract.md, decisions.md, log/, artifacts/, children/)
  plus a SQLite index (node id, parent, status, created_at).
- src/fractal/runner.py   — run_node(node_id): assemble context
  (own contract + ancestor constraints chain), call the model, parse a
  structured result that is either split(subtasks) or
  complete(deliverable, summary).
- src/fractal/scheduler.py — loop: pick runnable nodes, run, apply
  result, repeat until root completes. Hardcoded max depth = 3.
- src/fractal/cli.py      — `fractal init <goal>`, `fractal run`,
  `fractal status` (prints the tree with statuses).
- Leaf executor = one Anthropic API call (no Claude Code yet).

Acceptance criteria (each must map to a named test):
AC0.1 `fractal init "write a documented fizzbuzz library"` creates
      tree/root/ with a valid contract.md and an indexed pending node.
AC0.2 `fractal run` on that goal produces a completed root with >=1
      level of children and artifacts present at every leaf.
AC0.3 Kill -9 the process at a random point mid-run; `fractal run`
      again resumes from disk and completes. No node runs twice
      after completing.
AC0.4 A split at depth 3 is rejected and the node is forced to
      complete or fail; the tree never exceeds depth 3.
AC0.5 `fractal status` output matches the on-disk tree exactly.

Budget: <= 1,500 lines of src/. Out of scope: escalation, budgets,
global store, verification beyond "artifact exists", parallelism.
