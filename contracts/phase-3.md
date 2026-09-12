# Phase 3 Contract: Upward Escalation
AC3.1 A leaf escalating a named inherited constraint suspends exactly
      its branch; unrelated branches keep running.
AC3.2 The ancestor owning the constraint is reopened with the child's
      evidence in context and must return amend | overrule | re-plan.
AC3.3 Amend: descendant contracts are updated, stale dependents
      re-verified, the branch resumes and completes under new terms.
AC3.4 Overrule: rationale is written into the child's context; the child
      resumes and must address it.
AC3.5 Re-plan: pruned children's logs are compacted into the ancestor's
      log/ before deletion.
AC3.6 Discovered dependency: a running node escalates "my contract does
      not provide X; sibling B owns it"; the parent resolves by adding a
      depends_on edge (node pauses until B delivers) OR amending B's
      contract to expose the interface earlier. Both paths tested. No
      worker-to-worker channel exists anywhere in the codebase.
AC3.7 End-to-end adversarial scenario: a root contract mandates library X;
      a scripted leaf discovers X conflicts with a requirement; the run
      completes with the root constraint amended. A control run with
      escalation disabled ships the flaw. Both outcomes asserted.
