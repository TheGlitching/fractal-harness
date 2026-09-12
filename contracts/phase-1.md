# Phase 1 Contract: Contracts as the Layer Boundary
Goal: replace goal-strings with structured contracts; make splits
dependency-aware DAGs; verify every rollup.

Deliverables:
- src/fractal/contract.py — Contract dataclass + md serialization:
  goal, acceptance_criteria[], interfaces[], budget, inherited_constraints[],
  depends_on[]. Constraint inheritance concatenates down the tree.
- split() now returns child contracts with depends_on edges; the
  scheduler only runs nodes whose dependencies are complete, and injects
  each dependency's deliverable path + distilled summary into the
  dependent node's context at hydration.
- src/fractal/verify.py — on complete(), the parent context (contract +
  child summary + deliverable) is sent to a critic model call that
  returns per-criterion PASS/FAIL. FAIL triggers a revise-and-resubmit
  loop, max 2 rounds, then the node is marked failed.
- Stale-flagging: if a dependency's deliverable changes after acceptance,
  dependents are flagged stale and re-verified.

Acceptance criteria:
AC1.1 A split producing children A, B(depends_on=A) runs A to acceptance
      before B starts, and B's context contains A's summary.
AC1.2 A dependency cycle in a proposed split is rejected at split time.
AC1.3 A planted bad deliverable (violates a named criterion) is caught
      by verify at the parent boundary — not at the root — and one
      revise round fixes it.
AC1.4 Amending A's deliverable after acceptance flags B stale; rerunning
      re-verifies B.
AC1.5 Every accepted node's decisions.md contains the verification
      verdict with per-criterion results.

Budget: <= 1,000 new lines. Out of scope: escalation, parallel siblings.
