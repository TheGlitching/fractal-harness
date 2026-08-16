# Fractal Harness

A persistent task tree executed by ephemeral agents. Long-horizon autonomy
treated as a **memory architecture problem**: instead of one agent with one
context window, the harness grows a tree of nodes, each with its own contract,
decisions, logs, and artifacts. Agents are stateless workers hydrated from a
node; the tree is the memory.

The design rationale is fully spelled out in [`docs/SPEC.md`](docs/SPEC.md).

## How it works

A project is a directory tree that mirrors a task tree. Every node holds:

- `contract.md` — goal, acceptance criteria, interfaces, inherited constraints
- `decisions.md` — append-only semantic memory of the node
- `log/` — episodic traces (the raw model-call events)
- `artifacts/` — the deliverables a node produces
- `children/` — subordinate nodes

A small SQLite index (`.fractal/index.db`) carries what the scheduler needs
(node id, parent, status, budget ledger). The filesystem is the source of
truth; the index is reconciled from disk on every load.

Agents answer with one of five verbs, all enforced by orchestrator code, not
by prompting:

| Verb | Meaning |
|------|---------|
| `split(subtasks)` | the task is too big for one agent; propose child contracts |
| `complete(deliverable, summary)` | submit work for verification against the contract |
| `escalate(assumption, evidence)` | an inherited constraint is false; reopen the owner |
| `escalate_resolve(resolution)` | settle an escalation: amend / overrule / replan / depends_on |
| `note_global(type, content, supersedes?)` | write a lesson, convention, or skill to the shared global store |

The **leaf executor is pluggable**. It can be:

- `opencode` (default) — spawns opencode headlessly in the node directory,
  which reads a generated `CLAUDE.md` (the contract + inherited constraints +
  global knowledge) and writes its deliverables as real files.
- `anthropic` — a bare API call with tool-use parsing (used by the test suite
  with a deterministic fake model).

## Installation

Requires Python 3.12.

```bash
# 1. Create a virtual environment and install the harness (no runtime deps
#    beyond the stdlib; anthropic SDK is only needed for FRACTAL_EXECUTOR=anthropic)
python3 -m venv .venv
.venv/bin/pip install -e .

# 2. Ensure the leaf executor binary is on your PATH.
#    opencode (default):
which opencode            # https://github.com/sst/opencode
#    or Anthropic SDK for FRACTAL_EXECUTOR=anthropic:
.venv/bin/pip install anthropic
```

No `setup.py`/`pyproject.toml` is committed yet — the package is importable
straight from `src/`:

```bash
export PYTHONPATH="$PWD/src"
alias fractal=".venv/bin/python -m fractal.cli"
```

## Usage

### Start a project

```bash
fractal init "Build a CLI weather dashboard with tests"
```

This creates `tree/root/` with a pending root node and the SQLite index.

### Run it

```bash
fractal run
```

The scheduler picks runnable nodes, hydrates each from its contract, invokes
the executor, and applies the structured result — splitting when a task is too
big, completing leaves, escalating conflicts — until the root is finished.
Every `run` writes `trace.json` to the project root with the research dataset:

```jsonc
{
  "steps": 1, "completed": 1, "split": 0, "refused": 0, "failed": 0,
  "root_status": "complete",
  "escalations": 0,
  "verifications": 1, "verify_failures": 0, "verify_catch_rate": 0.0,
  "depth_distribution": {"1": 1}, "max_depth": 1,
  "total_cost_tokens": 0,
  "per_node_cost": [{"node": "root", "depth": 1, "tokens": 0}]
}
```

### Inspect the tree

```bash
fractal status
```

```
root  [complete]  Build a CLI weather dashboard with tests
  root-01  [complete]  Write the weather fetch module
  root-02  [complete]  Write the CLI entry point
```

### Steer a running project

Redirect, extend, or prune a project without reading the tree by hand. Every
command prints an impact preview and requires `--confirm` before applying.

```bash
# Amend the root contract (propagates to inheritors)
fractal steer amend-root --old old.md --new new.md --confirm

# Splice a new child under a node
fractal steer add root-01 --goal "Add caching" \
  --acceptance-criteria "reads cache first" --confirm

# Prune a subtree (compacts its logs into the parent first)
fractal steer remove root-02 --confirm
```

### Summarize

```bash
fractal digest
```

Writes a three-paragraph narrative (done / blocked / next) to `digest.md`,
referencing only real node ids and statuses present on disk.

## Configuration

Behaviour is driven by environment variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `FRACTAL_EXECUTOR` | `opencode` | Leaf executor: `opencode` or `anthropic` |
| `FRACTAL_MODEL` | `claude-opus-5` | Model for node calls (both executors) |
| `FRACTAL_MAX_TOKENS` | `8192` | Max output tokens for a node call |
| `FRACTAL_BUDGET` | unset | Token allowance at the root; enables the budget ledger |
| `FRACTAL_SPLIT_FEE` | `200` | Token cost charged per split |
| `FRACTAL_MAX_STEPS` | `500` | Backstop loop bound for a single run |
| `FRACTAL_VERIFY_MODEL` | `FRACTAL_MODEL` | Model for the critic/verifier |
| `FRACTAL_VERIFY_MAX_TOKENS` | `4000` | Max output tokens for the verifier |

### Using the budget to bound recursion

Without a budget the tree is capped at `MAX_DEPTH` (3 levels). With
`FRACTAL_BUDGET`, recursion is bounded by economics: every split debits a
split-fee and divides the remaining allowance across its children, so depth
responds to budget rather than a magic number.

```bash
FRACTAL_BUDGET=100000 FRACTAL_SPLIT_FEE=500 fractal run
```

## Development

The authoritative spec is `docs/SPEC.md`; each phase of work is a contract in
`contracts/`. The acceptance criteria live as tests in `tests/` — one test per
criterion — and must never be weakened to make them pass.

```bash
.venv/bin/python -m pytest -x -q
```

Tests inject a deterministic fake Anthropic module (via `sitecustomize`) and
force `FRACTAL_EXECUTOR=anthropic`, so the suite runs offline with no real
model and no network. All 34 tests pass.

The implemented phases are `contracts/phase-0.md` through `contracts/phase-5.md`:
skeleton, contracts & verification, budgets, escalation, the global
cross-cutting store, and pluggable executors + instrumentation.

## License / status

Research-grade prototype (`__version__ = "0.1.0"`). No license declared yet.
