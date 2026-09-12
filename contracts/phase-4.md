# Phase 4 Contract: Global Cross-Cutting Store

Goal: a blackboard-style global semantic memory store orthogonal to the task
tree, so a lesson learned in one branch can inform another (SPEC.md §4.4
lateral flow, Gap 5 — cross-cutting memory).

Deliverables:
- `global/` directory in the project root (sibling to `tree/`), holding typed
  entries as individual Markdown files.  The SQLite index carries enough
  metadata for retrieval — entry id, type (lesson | convention | skill),
  content fingerprint, superseded flag, and supersedes pointer.
- `note_global(entry)` — a fifth verb nodes may return during execution.
  Entry payload: ``type`` (one of lesson, convention, skill), ``content``
  (a concise Markdown string), and an optional ``supersedes`` field naming
  the id of an existing entry this one replaces.
- Supersede semantics (Gap 5, A-MEM lineage): a new entry whose ``supersedes``
  field names an existing entry marks that entry as superseded in the index.
  Superseded entries are never returned by hydration retrieval; they stay on
  disk for audit but are logically retired.
- Node hydration (ASSEMBLE CONTEXT step): when the runner builds a node's
  prompt, it queries the global store for the top-k (default 5) entries most
  relevant to the node's contract text and includes them as a "## Global
  knowledge" section.  Relevance ranking is a stub in this phase
  (lexicographic / keyword-match approximate nearest neighbour); real
  embedding-based retrieval is a later refinement.
- Persistence: global entries are files on disk indexed in SQLite, so the
  store survives process restart.
- The scheduler processes ``note_global`` without interrupting the node's
  iteration: after writing the entry, the node is re-asked (attempt counter
  increments normally), allowing it to write multiple entries before
  splitting or completing.

Budget: <= 400 new lines added / modified.  Out of scope: vector embeddings
and approximate-nearest-neighbour retrieval (stub keyword-match is the
deliverable), entry edit / delete through the model (the store API may later
add ``update_global``), and any per-entry access control.

Acceptance criteria:

AC4.1 An entry written by branch A appears in branch B's hydration context
      and measurably changes the fake model's scripted behaviour.  Driven
      through the CLI: the root splits into [A, B]; A writes a global
      convention entry via ``note_global`` before completing; when B runs,
      its prompt includes the convention text, and the fake model's
      scripted response for B differs between a run where A wrote the entry
      and an otherwise-identical run where A did not.  Assert both that the
      entry text appears in B's context and that B's deliverable changes.

AC4.2 A superseded entry is never retrieved.  Driven in-process through the
      Store API: write entry E1, then write entry E2 with
      ``supersedes=<E1.id>``; retrieve relevant entries against a query
      that would match both and assert only E2 is returned, with E1 absent.

AC4.3 The store survives restart.  Driven in-process: write a global entry
      through a Store, close it (let the object go out of scope / close the
      sqlite connection), open a fresh Store on the same project directory,
      and assert the entry is still present and its content matches.
