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
| `escalate(assumption, evidence)` | an inherited constraint or blocker; propagates up to parent |
| `escalate_resolve(resolution)` | settle an escalation |
| `note_global(type, content)` | write a lesson, convention, or skill to the shared global store |

The **leaf executor** spawns `omp` (default) or `opencode` headlessly in each node directory.
Agents read a generated `CLAUDE.md` (atomic contract + direct parent constraints
+ relevant global knowledge) and write deliverables as real files in `artifacts/`.

## Key Modern Harness Principles

- **Atomic Decomposition**: Each node is small, atomic (single file / single concern) so lightweight models can succeed reliably without context dilution.
- **Minimal Context**: Each agent only receives its contract, direct parent constraints, and sibling goals. It does not carry the full ancestral chain.
- **Fail-Safe & Auto-Healing Retries**: Nodes retry up to 3 times on runtime errors or verification failures, feeding back precise failure reasons so the model corrects its output.
- **Constraint Escalation & Downstream Propagation**: When a child escalates an invalid assumption or a constraint is added, it is recorded in the parent and automatically propagated to all descendant contracts.
- **Interactive TUI Steering**: Inspect running nodes, review decisions & constraints, inject new global or subtree constraints, and trigger retries directly from the live TUI.

## Installation

Requires [Rust](https://rustup.rs) and [`omp`](https://github.com/can1357/omp) (or `opencode`) on your PATH.

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

### Options and Executor selection

```bash
fractal init --executor omp "Goal"
# or
FRACTAL_EXECUTOR=omp fractal init "Goal"
```

### Resume a paused project

```bash
cd my-project
fractal run
```

### Interactive TUI Controls

While `fractal init` or `fractal run` is active:
- **`↑` / `↓`** (or `k` / `j`): Select a node in the tree.
- **`i` / `Enter` / `?`**: Open the **Node Inspector** (view Goal, Status, inherited Constraints, and recent Decisions).
- **`s` / `m`**: **Steer / Add Constraint**: Opens a modal to input a constraint that is immediately applied to the selected node and propagated down to all descendants.
- **`r`**: **Retry**: Queue an instant reset and retry of the selected node and its subtasks.
- **`q`**: Exit TUI (or press `Ctrl+C` to interrupt).

### Inspect the tree (CLI)

```bash
fractal status
```

### Summarize

```bash
fractal digest
```

Writes `digest.md` with three sections (done / blocked / next).

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `FRACTAL_EXECUTOR` | `omp` | Leaf executor (`omp` or `opencode`) |
| `FRACTAL_BUDGET` | unset | Token allowance at the root; enables budget scaling |
| `FRACTAL_SPLIT_FEE` | `200` | Token cost charged per split |
| `FRACTAL_MAX_STEPS` | `500` | Backstop loop bound for a single run |
| `FRACTAL_TIMEOUT` | `1200` | Seconds before a stuck node is killed |
| `FRACTAL_PARALLEL` | `4` | Number of concurrent nodes executed in parallel |

## License / status

Single-binary Rust rewrite. Licensed under MIT.
