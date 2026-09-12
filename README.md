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
| `complete(deliverable, summary)` | submit work for verification against the contract | implemented; fails closed on an empty diff |
| `escalate(assumption, evidence)` | raise an invalid inherited assumption to the owner | implemented |
| `escalate_resolve(resolution)` | settle an escalation (`amend`/`overrule`/`replan`/`depends_on`) | implemented |
| `reopen(children, reason)` | return specific children for rework after integration fails | implemented |
| `note_global(type, content)` | write a lesson, convention, or skill to the shared global store | implemented |

The **leaf executor** spawns a headless coding agent in each node directory.
Three executors are supported and selected with `--executor`/`FRACTAL_EXECUTOR`:
`omp` (default, also `pi`) and `opencode`. Agents read a generated `CLAUDE.md`
(atomic contract, with its inherited constraints, plus relevant global knowledge)
and edit the live project directly; each verified node's work becomes one commit
in the project repo.

## Key Modern Harness Principles

- **Atomic Decomposition**: Each node is small, atomic (single file / single concern) so lightweight models can succeed reliably without context dilution.
- **Minimal Context**: Each agent only receives its contract (including its inherited constraints) and sibling goals. It does not carry the full ancestral chain, and every injected collection is bounded so a prompt cannot grow with the project's age or breadth.
- **Fail-Safe & Auto-Healing Retries**: Nodes retry up to 3 times on runtime errors or verification failures, feeding back precise failure reasons so the model corrects its output.
- **Upward Escalation**: A node that finds an inherited assumption false suspends its branch and reopens the ancestor that owns it, which resolves with `amend`, `overrule`, `replan`, or `depends_on`. A challenged assumption is not treated as an accepted constraint until the owner has ruled.
- **Fail-Closed Completion & Verification**: No decision is an error and a retry, never a fabricated `complete`; a completion must be backed by a real git diff; the critic's verdict must be an explicit `PASS` with per-criterion results.
- **Per-Node Isolation**: Nodes execute one at a time on the shared working tree, so each node's diff, verification and commit are exactly its own; a failed node's uncommitted files are reverted before the next node runs. The harness's own paths never enter the user's repository.
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
# opencode is a real executor, with its own model selection and flags
fractal init --executor opencode --model anthropic/claude-3-7-sonnet "Goal"
```

### Non-interactive / headless runs

`fractal init` and `fractal run` open the TUI only when attached to a real
terminal. Anywhere else — CI, a supervisor, another agent — they run headless
and terminate on their own: a failed node ends the run with a non-zero exit
status rather than waiting for a keystroke that cannot come.

```bash
fractal --model smol --yes init "Goal"
fractal --no-tui run
```

- `--model <model>` passes the model straight to the leaf executor and skips the
  picker.
- `--yes` (alias `--no-tui`) forces headless mode even on a terminal.

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

1. `--model <model>` if given.
2. `FRACTAL_MODEL` if set and non-empty.
3. On an interactive terminal, a picker listing available models.
4. Otherwise the default model.

The picker is only opened on a real terminal; headless runs never prompt.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `FRACTAL_MODEL` | unset | Model for the leaf executor; skips the interactive picker |
| `FRACTAL_EXECUTOR` | `omp` | Leaf executor (`omp`/`pi` or `opencode`) |
| `FRACTAL_BUDGET` | unset | Token allowance at the root; when set, recursion is bounded economically |
| `FRACTAL_SPLIT_FEE` | `200` | Token cost charged to a node each time it splits |
| `FRACTAL_CALL_TOKENS` | unset | Tokens charged per model call, overriding the character-based estimate |
| `FRACTAL_MAX_STEPS` | `500` | Backstop loop bound for a single run |
| `FRACTAL_TIMEOUT` | `300` | Seconds before a stuck node is killed |
| `FRACTAL_PARALLEL` | `4` | Nodes selected per scheduling batch; execution is serialized (see Per-Node Isolation) |

### Budgets

Set `FRACTAL_BUDGET` to a token allowance at the root to bound recursion
economically instead of by depth alone:

- every model call debits its estimated token usage to the calling node;
- a split charges `FRACTAL_SPLIT_FEE` and grants each child the `allocation` it
  proposed; the fee plus the grants must fit the node's remaining allowance or
  the split is refused with the reason fed back to the agent;
- a node whose allowance is exhausted before it can run fails, rather than
  continuing silently;
- the hard depth cap (`4`) always applies as a backstop, budget or not.

`FRACTAL_CALL_TOKENS` calibrates the per-call debit for providers that do not
report exact usage; without it, four characters are counted as one token.

### Per-node isolation and workspace exclusion

Nodes run one at a time against the single live project tree. That is deliberate:
agents edit the real repository in place, so two nodes writing concurrently
cannot be attributed - the second node's `git diff` would contain the first's
uncommitted files, and `git add` could commit a sibling's work under the wrong
node. Serializing the whole node (not just verify+commit) means each node's diff,
verification and commit are exactly its own, and a node that fails terminally has
its uncommitted files reverted before the next node starts. Git index mutation is
guarded process-wide.

The harness never commits its own working directories into the project. `tree/`,
`.fractal/` (including SQLite `-wal`/`-shm`), `global/`, `dist/`, `trace.json`,
`digest.md` and `.fractal_decision_*` are written to the repository's local
`.git/info/exclude` and are also excluded from every `git add`, `git status` and
diff the harness runs. A pre-existing `.gitignore` is never modified or replaced,
so running inside a real repository cannot pollute its history with harness
internals.

### Durability and trust boundary

The SQLite index uses a rollback journal with `synchronous=FULL`, so a committed
node status has reached disk before the next model call starts and the tree
survives a `SIGKILL`; the filesystem remains the source of truth and `reconcile`
repairs the index from it. On `q`/Ctrl-C the running executor child is killed
and reaped before the harness exits.

Executors run with full shell access and auto-approved permissions
(`--auto-approve --approval-mode=yolo`, opencode's `--auto`). Dependency
artifacts and the global store are untrusted prompt input, so a prompt-injected
instruction can reach the whole project. Point the harness only at repositories
and accounts where that blast radius is acceptable.

## Status

The canonical implementation is Rust. The design spec is `docs/SPEC.md` and the
build contracts are in `contracts/`. Upward escalation, fail-closed completion
and verification, deterministic non-interactive termination, enforced budgets,
dependency staleness, per-node isolation, workspace exclusion, bounded per-node
context, and the `omp`/`pi`/`opencode` executors are implemented. Known gaps
tracked for follow-up work: crash-safe reaping of gate subprocesses.

## License

Licensed under MIT.
