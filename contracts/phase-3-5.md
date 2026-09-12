# Phase 3.5 Contract: Steering From Above & Project Visibility
Goal: let the user redirect, extend, or prune a running project safely,
and understand its state without reading the tree by hand.

Deliverables:
- Steering inbox: a SQLite queue of user change requests. The scheduler
  drains it only at safe boundaries (no node mid-iteration in the
  affected subtree). Never interrupts a running worker.
-  — diffs old vs new root
  contract, walks the tree to find nodes whose inherited constraints,
  interfaces, or dependencies touch changed clauses, and applies the
  Phase 3 resolution machinery (amend descendants / stale-flag /
  reopen ancestors for re-plan). Untouched branches never pause.
-  — splices a new
  child under any node, validating budget arithmetic and depends_on
  edges against existing siblings.
-  — prunes a subtree using the Phase 3
  rule: episodic logs compact into the parent's log/ before deletion;
  dependents of the removed node's interfaces are stale-flagged.
- Impact preview: every steer command first prints affected subtrees,
  $ of accepted work to be re-verified, and branches needing re-plan,
  then requires --confirm (or interactive yes) before queueing.
-  — one cheap model call over status.md files and
  decisions.md entries since the last digest, writing a three-paragraph
  narrative (done / blocked / next) to digest.md.  flag
  supported. A --watch mode regenerates on a timer.

Acceptance criteria:
AC3.5.1 An amend-root touching one constraint stale-flags exactly the
        subtrees inheriting it; an unrelated running branch is never
        paused (asserted via scheduler event log).
AC3.5.2 The impact preview's list of affected nodes equals the set the
        apply step actually touches — no more, no less.
AC3.5.3 A steer queued while the affected subtree is mid-iteration is
        applied only after that iteration's boundary; the worker's
        in-flight write is never corrupted (kill-based test).
AC3.5.4 remove compacts pruned logs into the parent and stale-flags
        dependents; re-adding a similar child afterward hydrates with
        the compacted history retrievable in context.
AC3.5.5 add rejects a child whose budget allocation exceeds the
        parent's remainder or whose depends_on creates a cycle.
AC3.5.6 digest output references only real node ids and statuses
        present on disk (anti-hallucination check: every named node in
        the digest is validated to exist).

Budget: <= 800 new lines. Out of scope: web UI, push notifications
(mail/Slack) — stub a notify() hook for later.
