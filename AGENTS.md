# Working agreements for the fractal repo
- The authoritative spec is docs/SPEC.md. The current objective is the
  contract in contracts/ named in your prompt. Nothing else is in scope.
- Tests in tests/ are the acceptance criteria. Never weaken, skip, or
  delete a test to make it pass. If a test seems wrong, write your
  reasoning in notes.md and stop.
- One small change per iteration. Run `pytest -x -q` before finishing.
- Python 3.12, type hints everywhere, no dependencies beyond
  pytest/pyyaml/click/anthropic/sqlite3 without noting why in notes.md.
- State layout, verbs, and terminology must match docs/SPEC.md §4 exactly:
  contract.md, decisions.md, log/, artifacts/, children/, and the verbs
  split / complete / escalate / note_global.
