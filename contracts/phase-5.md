# Phase 5 Contract: Claude Executor & End-to-End Trial

Goal: swap the leaf executor so the harness can drive `claude` (Anthropic's
CLI coding agent) headlessly with a generated CLAUDE.md, then run the
harness end-to-end on a real project to produce a research dataset of
per-node cost, depth distribution, escalation count, and verification
catch rate.

Deliverables:
- When ``FRACTAL_EXECUTOR=claude``, the runner generates a ``CLAUDE.md`` file
  in the node's ``artifacts/`` directory from the node's contract and
  inherited constraints before invoking the agent.  The file contains the
  goal, acceptance criteria, interfaces, inherited constraints, and the
  ``## Global knowledge`` section (from Phase 4), rendered as a standard
  CLAUDE.md instruction block.
- The runner invokes ``claude -p "<goal>" --output-format json`` (or the
  nearest equivalent flag set for headless structured output) with cwd set
  to the node's directory, so the agent finds the generated CLAUDE.md,
  contract.md, and any sibling artifacts automatically.
- Adapt the existing JSON-output recovery and error-filtering adapter code
  that ``gnhf`` already maintains for Claude's stdout format — do not
  rewrite that plumbing, borrow it.  The adapter handles the common
  patterns: JSON output wrapped in commentary, multi-line outputs, and the
  ``--output-format json`` structured envelope.
- Instrumentation hooks (per-node cost, depth distribution, escalation
  count, verification catch rate) are stubbed in this phase and wired
  during the trial run; they need not live in a formal metrics module yet.

Acceptance: empirical, not unit-tested

Phase 5 is accepted through an end-to-end trial of the harness on a real
project rather than through unit tests.  The recommended first target is
"build and document a CLI weather dashboard with tests".  The trial run
must produce:

  1. A completed root node (the dashboard is built and documented).
  2. Traces of per-node cost (tokens or wall-clock per agent invocation).
  3. The depth distribution of the task tree.
  4. The count of escalation events (if any).
  5. The verification catch rate (number of verify FAIL verdicts that led
     to a correction, versus total verification passes).

These traces are the research dataset described in SPEC.md §5 (Phase 5
acceptance) and §6 (Claim instrument).

No new tests in ``tests/`` are required for Phase 5.  The existing test
suite (31 tests, all using ``FRACTAL_EXECUTOR=anthropic``) must still
pass — the Claude path is additive and does not alter the Anthropic or
opencode paths.
