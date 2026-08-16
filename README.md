# Fractal Harness

A persistent task tree executed by ephemeral agents. Long-horizon autonomy
treated as a **memory architecture problem**: instead of one agent with one
context window, the harness grows a tree of nodes, each with its own contract,
decisions, logs, and artifacts. Agents are stateless workers hydrated from a
node; the tree is the memory.

Agents answer with one of five verbs, all enforced by orchestrator code, not
by prompting:

| Verb | Meaning |
|------|---------|
| `split(subtasks)` | the task is too big for one agent; propose child contracts |
| `complete(deliverable, summary)` | submit work for verification against the contract |
| `escalate(assumption, evidence)` | an inherited constraint is false; reopen the owner |
| `escalate_resolve(resolution)` | settle an escalation |
| `note_global(type, content)` | write a lesson, convention, or skill to the shared global store |

The **leaf executor** spawns opencode headlessly in each node directory.
opencode reads a generated `CLAUDE.md` (the contract + inherited constraints
+ global knowledge) and writes deliverables as real files.

## How it works

A project is a directory tree that mirrors a task tree. Every node holds:

- `contract.md` — goal, acceptance criteria, interfaces, inherited constraints
- `decisions.md` — append-only semantic memory of the node
- `log/` — episodic traces
- `artifacts/` — the deliverables a node produces
- `children/` — subordinate nodes

A SQLite index (`.fractal/index.db`) carries what the scheduler needs; the
filesystem is the source of truth.

## Installation

Requires [Rust](https://rustup.rs) and [opencode](https://github.com/sst/opencode) on your PATH.

```bash
git clone https://github.com/TheGlitching/fractal-harness.git
cd fractal-harness
cargo install --path .
```

After installation, the `fractal` command is available globally.

## Usage

### Start a project (init + run)

```bash
fractal init "Build a CLI weather dashboard with tests"
```

This creates the tree, starts the scheduler, and shows a real-time animated
tree view. No separate `run` command is needed.

### Resume a paused project

```bash
cd my-project
fractal run
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

### Summarize

```bash
fractal digest
```

Writes `digest.md` with three sections (done / blocked / next).

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `FRACTAL_EXECUTOR` | `opencode` | Leaf executor (only opencode in the Rust binary) |
| `FRACTAL_BUDGET` | unset | Token allowance at the root; enables budget scaling |
| `FRACTAL_SPLIT_FEE` | `200` | Token cost charged per split |
| `FRACTAL_MAX_STEPS` | `500` | Backstop loop bound for a single run |
| `FRACTAL_TIMEOUT` | `600` | Seconds before a stuck node is killed |

With `FRACTAL_BUDGET`, recursion is bounded by economics: every split debits a
split-fee and divides the remaining allowance across children.

```bash
FRACTAL_BUDGET=100000 FRACTAL_SPLIT_FEE=500 fractal init "Large task"
```

## What the animation looks like

```
┌─ fractal ──────────────────────────────
  ● root  Write a CLI weather dashboard with tests
  ├─ ◉ root-01  Write the weather fetch module...
  ├─ ○ root-02  Write the CLI entry point...
  └─ ● root-03  Write the README
├─────────────────────────────────────
  steps: 4  ✓3  ◇1  ✗0  ⤴0  verify: 4/4
└─────────────────────────────────────
```

Every `init` and `run` writes `trace.json` to the project root with
per-node status, depth distribution, escalation count, and verification stats.

## License / status

Single-binary Rust rewrite. Licensed under MIT.
