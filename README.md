# Fractal Harness

A persistent task tree executed by ephemeral agents. Long-horizon autonomy
treated as a **memory architecture problem**: instead of one agent with one
context window, the harness grows a tree of nodes, each with its own contract,
decisions, logs, and artifacts. Agents are stateless workers hydrated from a
node; the tree is the memory.

Agents answer with a verb; the scheduler parses and routes the node, rather than
trusting prose:

| Verb | Meaning | Status |
|------|---------|--------|
| `split(subtasks)` | the task is too big for one agent; propose child contracts | implemented |
| `complete(deliverable, summary)` | submit work for verification against the contract | implemented |
| `escalate(assumption, evidence)` | raise an invalid inherited assumption to the parent | implemented; resolution path not yet |
| `reopen(children, reason)` | return specific children for rework after integration fails | implemented |
| `note_global(type, content)` | write a lesson, convention, or skill to the shared global store | implemented |
| `escalate_resolve(resolution)` | settle an escalation | **not yet implemented** |

The **leaf executor** spawns `omp` (also `pi`) headlessly in each node directory.
Agents read a generated `CLAUDE.md` (atomic contract + direct parent constraints
+ relevant global knowledge) and write deliverables as real files in `artifacts/`.
`opencode` is **not yet implemented**: selecting it currently still runs `omp`.

## Key Modern Harness Principles

- **Atomic Decomposition**: Each node is small, atomic (single file / single concern) so lightweight models can succeed reliably without context dilution.
- **Minimal Context**: Each agent only receives its contract, direct parent constraints, and sibling goals. It does not carry the full ancestral chain.
- **Fail-Safe & Auto-Healing Retries**: Nodes retry up to 3 times on runtime errors or verification failures, feeding back precise failure reasons so the model corrects its output.
- **Constraint Escalation & Downstream Propagation**: When a child escalates an invalid assumption or a constraint is added, it is recorded in the parent and automatically propagated to all descendant contracts.
- **Interactive TUI Steering**: Inspect running nodes, review decisions & constraints, inject new global or subtree constraints, and trigger retries directly from the live TUI.

## Installation

Requires [Rust](https://rustup.rs) and [`omp`](https://github.com/can1357/omp) on your PATH.

```bash
git clone https://github.com/TheGlitching/fractal-harness.git
cd fractal-harness
cargo install --path .
```

After installation, the `fractal` command is available globally. The helper
checks described below run through that binary (`fractal node-diff`,
`fractal integrate-check`), so they are available after `cargo install` with no
extra PATH setup; `bin/*.sh` are the embedded sources.

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

### Inspect a node's real changes

`fractal node-diff` reads git, not a node's summary of itself. Installing parents
need the real diff to decide whether children wired their work in.

```bash
fractal node-diff --stat <child-id>   # summary only
fractal node-diff <child-id>          # full patch
fractal node-diff                     # list node commits
```

`fractal integrate-check` reports modules nothing imports and stub components.
It is advisory, and only meaningful for JS/TS projects; it exits cleanly with a
"check skipped" note for other stacks.

### Model selection

The model passed to the executor is chosen in this order:

1. `FRACTAL_MODEL` if set and non-empty.
2. On an interactive terminal, a picker listing available models.
3. Otherwise the default model.

There is currently no `--model` flag or non-interactive `--yes` flag; in a
non-TTY context the default is used without prompting.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `FRACTAL_MODEL` | unset | Model for the leaf executor; skips the interactive picker |
| `FRACTAL_EXECUTOR` | `omp` | Leaf executor (`omp`/`pi`; `opencode` not yet implemented) |
| `FRACTAL_BUDGET` | unset | Token allowance at the root; **not yet enforced** |
| `FRACTAL_SPLIT_FEE` | `200` | Token cost charged per split; **not yet enforced** |
| `FRACTAL_MAX_STEPS` | `500` | Backstop loop bound for a single run |
| `FRACTAL_TIMEOUT` | `300` | Seconds before a stuck node is killed |
| `FRACTAL_PARALLEL` | `4` | Number of concurrent nodes executed in parallel |

## Status

The canonical implementation is Rust. The design spec is `docs/SPEC.md` and the
build contracts are in `contracts/`. Known gaps — upward escalation resolution,
fail-closed verification, deterministic non-interactive termination, per-node
isolation, context bounds, and an enforced budget — are tracked for follow-up
work and are not yet reflected in the behaviour above.

## License

Licensed under MIT.
