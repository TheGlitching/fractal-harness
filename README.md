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
- **Per-Node Isolation**: Independent ready nodes run concurrently, each in its
  own git worktree, so each node's diff, verification and commit are exactly its
  own; verified commits are integrated back onto the shared tree in dependency
  order. A failed node's worktree is discarded, so its half-written files can
  never leak into a sibling. The harness's own paths never enter the user's
  repository.
- **One Steering Agent**: a single butler (`fractal ask` / `fractal butler`) is
  the architect and maintainer you talk to. It sits outside the task tree - the
  root and every node are ordinary nodes with no special role - and applies a
  correction to only the nodes that must change rather than replaying the tree.
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
- `--no-dashboard` (or `FRACTAL_NO_DASHBOARD=1`) skips the auto-served web
  dashboard, for CI and other environments where binding a port is unwanted.
  The run still prints the `fractal serve` command to watch progress (see below).

### Resume a paused project

```bash
cd my-project
fractal run
```

### Talk to the butler

There is exactly one agent you talk to: the **butler** (`fractal ask` /
`fractal butler`, `src/butler.rs`). It is the maintainer, architect and mate of
the tree, and it lives *outside* the tree. It is not a node: the root and every
node stay ordinary nodes with no special role, and the interface is one
conversation rather than a set of parameters.

```bash
fractal ask "make the settings screen accessible on a phone"
fractal ask --run "drop the export button, it is not wanted"   # steer, then resume
fractal butler                                                 # interactive session
```

When you ask for a change, the butler inspects the tree through its tools and
chooses the **smallest** correct response:

- **none** - the tree already satisfies the request; nothing changes and it says
  which nodes already cover it;
- **reopen** - only the nodes that are wrong are sent back, with a precise
  reason, so only they re-run;
- **split** - genuinely new work gets a new child node under the right parent;
- **amend / edit_contract / retry / resolve** - the specific contract, gate,
  constraint or failure involved is changed in place.

It never resets or replays the whole tree. `--run` (or a later `fractal run`)
resumes the scheduler, which runs only the reopened or newly added nodes. The
plan, its rationale and the resulting status changes are recorded under
`.fractal/butler/` and printed.

Every butler tool routes through the same `Store` operations the TUI,
dashboard and scheduler use - there is no parallel state path. The tools are
also usable directly, which is how the butler calls them:

```bash
fractal butler-tool '{"tool":"tree"}'
fractal butler-tool '{"tool":"node","id":"root-01"}'
fractal butler-tool '{"tool":"reopen","parent":"root","children":["root-01"],"reason":"the wiring is wrong"}'
```

Tools: `tree`, `node`, `digest`, `trace`, `next`, `verify`, `constraint`,
`amend`, `edit_contract`, `reopen`, `split`, `retry`, `resolve`, `resume`,
`plan`. The butler agent uses the same executor as leaf nodes (`--executor` /
`FRACTAL_EXECUTOR`) and model selection (`--model` / `FRACTAL_MODEL`).

### Interactive TUI Controls

While `fractal init` or `fractal run` is active:
- **`↑` / `↓`** (or `k` / `j`): Select a node in the tree.
- **`i` / `Enter` / `?`**: Open the **Node Inspector** (view Goal, Status, inherited Constraints, and recent Decisions).
- **`s` / `m`**: **Steer / Add Constraint**: Opens a modal to input a constraint that is immediately applied to the selected node and propagated down to all descendants.
- **`r`**: **Retry**: Queue an instant reset and retry of the selected node and its subtasks.
- **`q`**: Exit TUI (or press `Ctrl+C` to interrupt).

### Web dashboard (observability and steering)

A local, browser-based dashboard is served by the binary. It reads and writes the
same `Store` the TUI uses, so an action taken in either place produces identical
state and appears in the other on its next read. There is no Node build step, no
CDN and no external service: one embedded HTML page and a small JSON API, served
offline.

```bash
fractal serve                 # http://127.0.0.1:8787
fractal serve --port 9000
fractal serve -p my-project   # same --project as the other commands
```

It works against a completed project and alongside a live `fractal run`. The
page polls, so a running node's status and event log update without a full
reload.

`fractal init` and `fractal run` also auto-serve this dashboard for the length of
the run and print the URL:

```text
fractal started - see progress here: http://127.0.0.1:8787/
```

If the default port is taken the run falls back to a free port; if the dashboard
cannot bind at all, the run continues and the line names the `fractal serve`
command instead. Disable auto-serving with `--no-dashboard` or
`FRACTAL_NO_DASHBOARD=1` (for CI).

The page is organised so a run can be understood at a glance, then acted on:

- **Run overview**: the project goal, the whole-run status (a failed node
  anywhere surfaces here even when the root is still `split`), counts
  (complete / running / pending / failed / split / refused), a completion bar,
  and the node that is running right now with its latest output line.
- **Task graph**, full width across the top of the page: the tree drawn as a
  graph of nodes, each labelled with its **goal** and its **run state** in
  words - `ready`, `running`, `waiting on <ids>`, `done`, `failed`. `ready`
  means pending with every `depends_on` complete, so what can run in parallel
  and what must wait are readable without decoding colours. A **Next up** line
  names the nodes runnable right now. Parent → child edges are always drawn;
  `depends_on` edges are hidden by default (they are what makes a dense graph
  unreadable) and shown by the **Dependencies** toggle, or focused on the
  selected node so one node's inputs and outputs appear on selection. The
  legend names every edge state, so colour is never the only carrier. A
  **Rows** view gives the same information as a flat, indented list, and is the
  default under 640 px; a **Graph / Rows** toggle is always available.
- **Node detail** (select any node, below the graph): the full contract (goal,
  acceptance criteria, `verification`, `manual_verification`, `depends_on`,
  interfaces, inherited constraints), its latest live activity, decisions, the
  `log/events.jsonl` tail, gate outcomes, errors, artifacts, and the committed
  diff rendered **the way git prints it** - `diff --git` / `---` / `+++`
  headers, `@@` hunk headers, green additions, red removals and context lines
  in monospace, scrollable both directions. Raw JSON is never the default view.
- **Steering**: grouped actions on the selected node - add a constraint/idea
  (propagates to descendants), edit the contract, answer/route an escalation,
  and a separate, visually marked **destructive** group for retry. Each action
  states what it is, when to use it and its consequence. The edit-contract form
  is **pre-filled** with the node's current goal, acceptance criteria,
  verification and manual verification, ready to change - never a placeholder.
  Every action is confirmed **inline** (one prompt, no blocking browser
  dialog), and on success the input clears, a persistent result is shown, and
  the added constraint/decision is highlighted in the node's contract and
  decisions. When no scheduler is running the result says so plainly: the
  change is recorded in the contracts and queued, and `fractal run` must be
  resumed for it to take effect. Each mutation is an explicit form submit that
  routes through the same `Store` methods as the TUI and scheduler; the server
  never mutates implicitly.

The page is a single embedded HTML file with inline CSS/JS: no CDN, no build
step, works offline. It respects `prefers-reduced-motion` (all animation is
disabled, state remains in text and colour) and is responsive down to ~390 px.

Live activity is written by the running scheduler to `.fractal/activity/<id>`,
a display-only scratch file outside `log/events.jsonl`: it is not part of the
audit trail and never affects the memory-tamper digest.

Options:

| Flag | Default | Meaning |
|------|---------|---------|
| `--port <port>` | `8787` | TCP port to listen on |
| `--host <addr>` | `127.0.0.1` | Address to bind |
| `--bind-all` | off | Bind `0.0.0.0` so another device (e.g. a phone) can reach it |
| `--allow-remote-mutations` | off | Permit steering from non-local addresses |

**Safety.** The dashboard binds to loopback by default and treats any
non-loopback request as read-only; GET cannot mutate in any case. `--bind-all`
exposes the dashboard to your local network, so anyone able to reach the port can
read the project tree, contracts, decisions and diffs. Steering from another
device requires `--allow-remote-mutations`, which you should only enable on a
network you trust. Use a firewall or an SSH tunnel for anything more exposed.

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
| `FRACTAL_MAX_ATTEMPTS` | `6` | Retries per node before it fails closed; also how many times a narrated-but-unparsed decision is retried |
| `FRACTAL_PARALLEL` | `4` | Independent ready nodes run concurrently, this many at a time, each in its own git worktree; `1` keeps the original single-tree serial path |

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

Nodes whose dependencies are satisfied run concurrently, up to
`FRACTAL_PARALLEL` (default `4`) at a time. Each running node gets its own git
worktree, branched from the shared tree's current `HEAD`, so its edits, `git
diff`, verification gates and commit are exactly its own: a sibling's
uncommitted files do not exist in that worktree and cannot be attributed to it,
and `git add` cannot sweep them into the wrong node. When the batch finishes, its
verified commits are cherry-picked back onto the shared tree. A cherry-pick that
conflicts fails that node with the conflict recorded as its reason and keeps its
branch for recovery, leaving the shared tree exactly as it was rather than
half-merged. A node that fails terminally simply has its worktree discarded, so
its half-written work cannot leak into a sibling.

Each node's harness memory - `tree/<id>/contract.md`, `decisions.md`, `log/`,
`artifacts/` - lives once in the shared tree and is never copied into a worktree;
the agent reads its generated `CLAUDE.md` from that shared path and edits code in
its worktree. The orchestrator therefore owns a single audit trail, and the
memory-tamper digest is unaffected. `FRACTAL_PARALLEL=1` keeps the original
single-tree serial path exactly. Escalation is the one serialization point: an
owner reopened to settle an escalation runs on the shared tree under a lock (and
its own work is committed under its own node), so concurrency never reroutes an
ancestor's work into a child's commit.

The harness never commits its own working directories into the project. `tree/`,
`.fractal/` (including SQLite `-wal`/`-shm` and `.fractal/worktrees/`), `global/`,
`dist/`, `trace.json`, `digest.md` and `.fractal_decision_*` are written to the
repository's local `.git/info/exclude` and are also excluded from every `git add`,
`git status` and diff the harness runs. A pre-existing `.gitignore` is never
modified or replaced, so running inside a real repository cannot pollute its
history with harness internals. Worktrees left behind by a crash are pruned at
the start of the next run.

The same exclusion covers dependency trees and build junk (`node_modules/`,
`.pnpm-store/`, `__pycache__/`, `.venv/`, `coverage/`, `.next/`, `*.log`, ...), so
a leaf running `npm install` commits its source, not 5000 dependency files. Runtime
state an app writes while a verification gate runs (for example `.portfolio.json`)
is detected as newly-untracked across the gate and added to `.git/info/exclude`
too, keeping it out of the diff, the critic's evidence and history.

### Failure isolation and recovery

A node that exhausts its retries fails closed: its own worktree is discarded, so
its uncommitted files cannot leak into a sibling's diff, and the run keeps going. In a
headless run, a failure no longer ends the whole tree - independent and deferred
siblings still run, dependents simply wait, and the completed run reports its root
as `failed` (the root node itself stays `split` so `fractal retry` can still
aggregate it later). When a node's objective gates all passed and only the critic
rejected it, that substantially-correct work is committed as an explicit
*unverified checkpoint* instead of being wiped, so a retry or `reopen` builds on
it rather than starting over.

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
context, failure isolation with unverified checkpointing, a tolerant
transcript-command completion channel, the butler steering agent and its
tool surface, and the `omp`/`pi`/`opencode` executors are implemented. Known
gaps tracked for follow-up work: crash-safe reaping of gate subprocesses and a
dashboard chat surface for the butler.

## License

Licensed under MIT.
