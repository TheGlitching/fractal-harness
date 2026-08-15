# Phase 3 — notes on the two test-side defects

**RESOLVED (25 passed).** Two defects in `tests/test_phase3.py` were fixed at
the owner's request (same category as the phase-2 fixes). Both were defects in
the test's own fake/assertions, not in the harness; no AC was weakened.

1. **The injected fake referenced five constants it never defined.** The
   `FAKE_SITECUSTOMIZE` string built its scenario payloads from
   `_CONSTRAINT_X`, `_CONSTRAINT_Y`, `_EVIDENCE`, `_RATIONALE` and
   `_NEED_EVIDENCE`, but those names exist only in the test module (lines
   69-73), outside the injected module. The very first time the fake tried to
   build a `resolve` payload it raised `NameError`, so **no phase-3 test could
   ever have run**. Fixed by defining the five constants inside the fake
   string, next to `_TAG_RE`.

2. **AC3.5 asserted names no data source produces, and a state the scenario
   forbids.** The replan scenario has A re-split into `[C1]` after pruning, so
   A legitimately ends owning its replacement child — yet the test asserted
   `assert not pruned` (A ends childless). And it asserted the compacted
   `log/` contains `"A-01"`/`"A-02"`, but the pruned children are tagged
   `A1`/`A2` and stored on disk as `root-01-01`/`root-01-02` (child ids follow
   the parent's on-disk id `root-01`), which phase 0's
   `_assert_status_matches_disk` pins. Fixed the first to assert the *pruned*
   children (A1/A2) are gone while the replacement (C1) remains, and the
   second to check the real child ids `{a.name}-01`/`{a.name}-02`. Compaction
   genuinely happens (the old children are deleted from disk after their logs
   are written into A/`log/`).

The harness work itself: the suspend–reopen–resolve cycle is implemented in
`src/fractal/scheduler.py` (upward escalation, SPEC.md 4.3), with the new
`escalate`/`escalate_resolve` verbs in `src/fractal/runner.py` and the
`SUSPENDED` status plus amend/interface/depends_on/delete operations in
`src/fractal/store.py`. AC3.1-AC3.7 all pass, and phases 0-2 remain green.


# Phase 2 — notes on the two test-side defects

**RESOLVED (18 passed).** The two defects below were fixed in
`tests/test_phase2.py` at the owner's request: `_max_depth` now ignores the
`children/` path segment, and the fake increments `_TRIES`. Full suite green.


The Phase 2 harness work is complete: the SQLite budget ledger, per-call
debiting, split-fee + allocation validation, budget-aware prompts, and
exhaustion-to-fail are all implemented, and AC2.1 and AC2.3 pass alongside all
of phase 0 and phase 1 (16 passed). The only two red tests are **defects in
`tests/test_phase2.py` itself**, not in the harness. Fixing them would require
changing the test, which the working agreements forbid ("never weaken, skip, or
delete a test"; "if a test seems wrong, write your reasoning in notes.md and
stop"). Both were already called out as test-side in the Phase 2 implementation
commit (7b325a2).

## AC2.4 — `_max_depth` over-counts the `children/` path segment

`_max_depth` (tests/test_phase2.py:389) measures depth as the length of a
node's path **relative to the root**, plus one:

    len(node.relative_to(root).parts) + 1

The on-disk layout mandated by SPEC.md 4.1 and enforced by the (passing)
phase-0 suite nests child nodes under a literal `children/` directory
(`tree/root/children/<id>/...`), and phase-0's `_assert_node_layout` requires
that directory to exist. So the relative path of a depth-2 node is
`children/<id>` (2 parts) and a depth-3 node is `children/<id>/children/<id>`
(4 parts). `_max_depth` therefore reports a genuinely three-level tree as 5.

Verified by running AC2.4's exact scenario: the harness produced the correct
budget-sized tree `root → root-01 → root-01-01` (three levels; the leaf at
depth 3 cannot afford the 200-token split fee and fails on exhaustion, exactly
what "a budget that affords 3 levels" means), but `_max_depth` returned 5.
AC2.1 is unaffected because it only compares relative depths, and it passes.

This helper is also internally self-contradictory: its own docstring says
"root alone is depth 1, root+child is 2", but for a root+child tree it computes
1 and 3. The harness layout cannot be changed to satisfy it without breaking
phase 0.

## AC2.2 — the fake never increments `_TRIES`, so the ROOT can never complete

The sitecustomize fake derives each node's attempt number from a global
`_TRIES` dict (tests/test_phase2.py:107, 178):

    attempt = _TRIES.get(tag, 0) + 1

but **nothing ever assigns to `_TRIES`**, so `attempt` is always 1 for every
tag. In `overalloc` mode the fake returns `split` on attempt 1 and `complete`
on attempt ≥ 2 (lines 150-165). Because attempt never advances, the ROOT
always proposes the over-budget split. The harness correctly rejects it each
time (that part of the test passes — the rejection message, decisions.md entry,
and no-children assertions all hold) and after `MAX_ATTEMPTS = 3` refusals
records the node failed.

The test then asserts the root completed and that a log record with
`attempt == 2` exists carrying `saw_rejection` (lines 572, 580-590). No such
record can ever be written because the fake never returns `complete`. No harness
behaviour can force the fake to emit attempt 2 — the attempt counter lives
entirely inside the test's fake.

## What this means

Both red tests fail because of code inside the test file (`_max_depth`'s path
arithmetic and `_TRIES`'s missing increment), not because the harness misbehaves.
I stopped here rather than edit the tests. To make the full suite green, one of
these test-side defects must be fixed; the harness code needs no further change.
