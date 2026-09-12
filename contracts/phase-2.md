# Phase 2 Contract: Economic Depth
Goal: replace the hardcoded depth cap with budget-bounded recursion.

Deliverables:
- Budget ledger per subtree in SQLite: tokens, model-dollars, depth
  allowance. Every model call debits actuals; every split debits a
  split-fee and divides the remainder across children per the parent's
  proposed allocation (validated: allocations must sum <= remaining).
- On exhaustion mid-node: the node must either complete degraded
  (explicitly marked) or fail — never silently continue.
-  shows per-subtree burn.

Acceptance criteria:
AC2.1 Same task run at budgets X and 4X produces strictly deeper or
      equal max depth at 4X (fake model scripted to always want splits).
AC2.2 A split proposing allocations exceeding the remaining budget is
      rejected with a structured error the agent sees.
AC2.3 Exhaustion produces a degraded-complete or failed node, and the
      ledger's total debits equal the sum of recorded call costs exactly.
AC2.4 The hardcoded depth cap is deleted; the depth-3 test from AC0.4 is
      reimplemented as a budget that affords exactly 3 levels.
