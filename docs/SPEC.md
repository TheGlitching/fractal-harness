# The Fractal Harness
## Foundations, Literature, Gaps, and Roadmap for a Recursive Memory-Layered LLM Harness

*Working document — August 2026*

---

## 1. Vision and Problem Statement

Large language models can now execute impressively long chains of actions, but they remain bounded by a structural constraint: everything the model knows about its task must fit inside one context window, and everything it learns evaporates when that window closes. Current mitigations — subagents that offload focused work, plan files the agent rereads, context compaction — extend the horizon but do not remove the ceiling. They all assume a fixed, shallow structure decided in advance: one orchestrator, one layer of workers, one plan file.

Real projects do not have a depth known in advance. The analogy that anchors this document is Walt Disney's principle of layered detail in his parks: a macro layer of architecture and sightlines, a middle layer of buildings and interiors, and a micro layer of sculpted details and hidden references. Crucially, nobody at the macro layer specifies the micro details — they specify constraints and intent, and each layer determines for itself how much further detail its own domain requires. The depth of the detail tree is *emergent*, discovered during the work, not declared before it.

The thesis of this project is that long-horizon autonomy is fundamentally a **memory architecture problem**, not a planning problem. The harness we propose treats the task tree itself as the durable memory structure of the project. Agents become ephemeral, stateless workers that are hydrated from a node of the tree, act, and dissolve — while the tree accumulates contracts, decisions, artifacts, and lessons. Depth is unbounded because the same control loop applies at every node, and each node's agent alone decides whether its task fits within its competence or must be split further.

The rest of this document establishes the scientific lineage behind each component of this idea, identifies precisely where the literature stops short, specifies the architecture that fills those gaps, and lays out a build roadmap and a research agenda.

---

## 2. Scientific Foundations

The harness sits at the intersection of four research lineages. None of them alone solves the problem; each contributes one load-bearing concept.

### 2.1 Classical roots: hierarchical planning, blackboards, and cognitive architectures

**Hierarchical Task Network (HTN) planning** (Sacerdoti's NOAH, 1975; Erol, Hendler & Nau's formalization, 1994; the SHOP/SHOP2 planners) is the formal ancestor of everything in this document. HTN planning represents tasks as either *primitive* (directly executable) or *compound* (requiring decomposition via a method into subtasks), and planning proceeds by recursively decomposing compound tasks until only primitives remain. The atomization decision — "is this task primitive for me?" — is exactly the split decision our harness places in each agent. What HTN lacked was any way to author the decomposition methods except by hand; LLMs supply exactly that missing piece, which is why the HTN skeleton is being rediscovered across modern agent frameworks. Reading Erol et al. (1994) and the SHOP2 paper (Nau et al., 2003) provides the vocabulary — task networks, methods, critics, protection intervals — for problems the LLM-agent literature is currently renaming.

**Blackboard architectures** (Hearsay-II, Erman et al., 1980) contribute the idea of a shared persistent workspace that multiple specialized knowledge sources read and write asynchronously, coordinated by a scheduler rather than by direct communication. Our global cross-cutting memory store is a blackboard; the insight worth preserving is that the blackboard, not the agents, is the locus of system state.

**Cognitive architectures** — Soar (Laird, Newell & Rosenbloom) and ACT-R (Anderson) — contribute the memory taxonomy that the entire modern literature relies on: working memory versus long-term memory, with long-term memory divided into declarative (episodic + semantic) and procedural forms, following Tulving's psychology of memory. Soar's *impasse* mechanism is also a direct ancestor of our escalation channel: when Soar cannot proceed, it automatically creates a subgoal to resolve the impasse, and the resolution is chunked into a new rule. An impasse that propagates *upward* rather than downward is precisely the mechanism modern frameworks are missing.

**Organizational theory** deserves a mention as the oldest running experiment in this space: human institutions solved recursive delegation long ago through bounded spans of control, management by objectives, escalation paths, and the distinction between directives (constraints flowing down) and reporting (summaries flowing up). Herbert Simon's *bounded rationality* is the theoretical justification for our split criterion — an agent decomposes precisely when a task exceeds its bounded capacity — and Galbraith's information-processing view of organizations reads remarkably like a design document for a multi-agent harness.

### 2.2 Memory architectures for LLM agents

**MemGPT / Letta** (Packer et al., 2023) is the founding paper of LLM memory management. It frames the context window as RAM and external storage as disk, and introduces virtual context management in which the model itself pages information between tiers through function calls, with interrupts managing control flow. Two of its ideas carry directly into our harness: memory operations as *tools the agent invokes deliberately* (rather than passive RAG), and the recognition that persistence must live outside the model. Its limitation for our purposes is that it manages memory for a *single* long-lived agent along a *single* timeline; it has no notion of a task hierarchy, so its memory tiers are temporal (recent vs. archival) rather than structural (this layer vs. that layer).

**CoALA — Cognitive Architectures for Language Agents** (Sumers, Yao, Narasimhan & Griffiths, 2023) is the organizing taxonomy of the field. It positions the LLM inside a larger architecture with working memory plus long-term episodic, semantic, and procedural memory, and structures the action space into internal actions (retrieval, reasoning, learning) and external grounding actions. Our per-node memory design maps directly onto CoALA's taxonomy, and CoALA is the right shared vocabulary for any paper this project produces. Notably, recent critiques observe that CoALA does not distinguish the *persistence semantics* of its memory types — how a semantic fact should be superseded versus how an episodic trace should decay — which is one of the open seams our harness addresses with explicit propagation and compaction rules.

**Voyager** (Wang et al., 2023) demonstrated the value of *procedural* memory: an agent that writes code skills, stores them in a library, and retrieves them by embedding similarity for reuse in later tasks. This is the strongest existing evidence that cross-task, cross-branch memory (our global store) compounds capability over time rather than merely preserving context.

**The successor memory systems** — MemoryOS (three-tier short/medium/long-term storage with distinct management policies per tier), A-MEM (Zettelkasten-style linked notes with supersede detection to handle staleness), Zep (temporal knowledge graphs for multi-session reasoning), and Mem0 (production key-value extraction with dense retrieval) — collectively map the design space of *storage and retrieval*. The most relevant recent entry is **HORMA** (2026), which makes an argument central to our design: memory *construction* and memory *retrieval* operate at different timescales and deserve different specialized agents, both working over a shared hierarchical file-system workspace through ordinary file operations and shell tools. HORMA's strong results under tight context budgets validate our choice of a plain filesystem as the memory substrate.

**Hybrid per-agent memory** is validated by systems like Project Synapse (2026), which shows a supervisor/worker hierarchy performing markedly better when each agent carries the working/episodic/semantic triad, with semantic memory holding organizational policies — the direct analogue of our inherited constraints.

### 2.3 Recursive and hierarchical agent frameworks

**ROMA — Recursive Open Meta-Agents** (Sentient AGI, 2025; paper 2026) is the closest existing system to this project and its most important reference. ROMA applies a uniform control loop at every node of a task tree, built from four roles: an *Atomizer* that decides whether the current task is atomic, a *Planner* that expands non-atomic tasks into dependency-aware subtask graphs, *Executors* that handle atomic leaves, and an *Aggregator* that compresses and validates child results as they flow back up. Context flows down the tree as subtasks are delegated and results flow back up through aggregation, keeping context growth controlled and execution traces transparent. ROMA proves the core recursion thesis: emergent depth via a local atomization decision works, and it benchmarks well on long-horizon search and reasoning tasks. Its published engineering guidance — enforce maximum depth and branching in the Planner, treat cost and latency as first-class service objectives, emit structured traces with task and parent identifiers at every node — should be adopted wholesale.

**AgentOrchestra** (2025) represents the more common non-recursive alternative: a fixed two-level hierarchy with a central planning agent orchestrating specialized workers through a perceive–store–reason–act–record loop. It is a useful baseline precisely because its fixed depth is the limitation our harness removes.

**ReCAP** (Stanford, 2025) contributes *recursive context management*: a dynamic context tree tracking evolving task hierarchies and reasoning trajectories, with mechanisms for recalling parent context when subtask execution drifts. It is the closest academic treatment of the question "what does a child need to see of its ancestors?"

**MiTa** (2026) documents the failure modes of hierarchy without memory discipline — memory inconsistency across agents, behavioral conflicts, information loss over long horizons — and addresses them with a centralized manager carrying allocation and summarization modules. Its diagnosis of *why* naive multi-agent systems fail on long tasks is a checklist of what our propagation rules must prevent.

**H2R — Hierarchical Hindsight Reflection** (2025) shows how to distill *reusable* hierarchical knowledge: high-level insights feed the planner while low-level ones feed executors, with an insight store updated through add/modify/upvote/downvote operations. This is a working model for how our global store should learn across projects rather than merely within one.

**EvoAgent** (2026) combines structured skill learning with hierarchical sub-agent delegation, modeling skills as multi-file capability units with triggering conditions and evolutionary metadata — evidence that the skill/procedural layer and the delegation layer reinforce each other.

### 2.4 Practitioner state of the art

The frontier labs' agent products embody the partial solutions this project starts from. Claude Code's subagents give one layer of context isolation: the orchestrator keeps the macro task while subagents burn their own context on focused work, returning only summaries. Plan-file conventions (a Markdown task list the agent maintains and rereads) are an externalized working memory. Context compaction summarizes a session in place when the window fills. Anthropic's own engineering writing on multi-agent research systems and on "context engineering" describes orchestrator–worker patterns, the practice of persisting state to files, and compaction as the current toolkit. The Agent Skills pattern (folders of instructions loaded on demand) is procedural memory by another name. All of these are single-layer instantiations of mechanisms our harness generalizes to arbitrary depth.

---

## 3. Gap Analysis: What the Literature Does Not Solve

Mapping the lineage above against the requirements of true end-to-end project autonomy leaves six gaps. These are simultaneously the design requirements of the harness and its candidate research contributions.

**Gap 1 — The tree as execution structure, not memory structure.** ROMA and its cousins build the task tree to *run* it: once a branch completes and aggregates, its interior is effectively discarded. No framework treats the tree as the durable, versioned, revisitable memory of the project — the thing you can reopen six weeks later, audit, and resume. Memory systems (Letta, MemoryOS, Zep) persist state, but for a single agent's timeline, not for a task hierarchy. The synthesis — a persistent tree whose nodes each own contracts, decisions, episodic traces, and artifacts, with agents as stateless functions over nodes — exists nowhere in the literature.

**Gap 2 — Upward escalation.** Downward flow (constraints, delegated subtasks) and upward rollup (summaries, aggregated results) are well handled. What no framework provides is a first-class channel for a deep node to *invalidate an ancestor's assumption* — the sculptor discovering the wall cannot bear the architect's assumed load. Soar's impasse mechanism generated subgoals downward; the required mechanism propagates an impasse upward, suspends the affected branch, reopens the ancestor with the child's evidence in context, and then resumes or re-plans the branch under amended constraints. Without this, deep trees silently build on broken foundations, and the failure is only discovered at final integration — the most expensive possible moment. This is, in our assessment, the single most valuable contribution the harness can make.

**Gap 3 — Contracts as the layer boundary.** Frameworks pass *tasks* (a goal string plus context) between layers. Organizational practice and interface theory suggest passing *contracts*: goal, explicit acceptance criteria, interfaces to respect, resource budget, and the accumulated constraints inherited from every ancestor. A contract makes the parent's verification step well-defined (check the deliverable against the criteria), makes compression principled (the parent needs contract status, not child internals), and gives escalation a precise target (name the inherited constraint being challenged). No current framework formalizes this boundary object.

**Gap 4 — Budget-bounded recursion.** Existing systems bound recursion with hardcoded depth and branching limits — ROMA's own guidance recommends exactly that. A principled alternative is economic: every node receives a budget (tokens, cost, wall-clock, depth allowance), splitting consumes budget, and depth becomes bounded by economics rather than a magic number. This also produces the self-assessment behavior the project requires: an agent's split decision is a claim that the task exceeds its bounded capacity, and the budget makes that claim accountable.

**Gap 5 — Cross-cutting memory orthogonal to the tree.** Hierarchies fragment knowledge: a lesson learned in branch A never reaches branch B if all memory flows along tree edges. Voyager's skill library and H2R's insight store prove the value of a shared store, but neither operates inside a recursive task hierarchy. The requirement is a blackboard-style global semantic store — glossary, conventions, learned lessons, reusable skills — writable by any node and retrieved into any node's context at hydration time, with supersede semantics (A-MEM's contribution) so stale entries are replaced rather than accumulated.

**Gap 6 — Verification at every rollup.** Aggregation in current frameworks compresses child outputs; it rarely *verifies* them. Because a contract carries acceptance criteria, every parent–child boundary becomes a natural checkpoint: the parent (or a dedicated critic pass) checks the deliverable against the criteria before accepting the rollup. Hierarchical verification also localizes failure — a property ROMA's authors identify as a chief weakness of sequential orchestration, where failures are hard to attribute.

---

## 4. Proposed Architecture: The Fractal Harness

The architecture follows one organizing principle: **everything durable is files; everything intelligent is stateless and ephemeral.** The harness is a workflow engine whose workers happen to be LLMs.

### 4.1 The state layer

The project is a directory tree mirroring the task tree, kept in git. Each node directory contains a `contract.md` (goal, acceptance criteria, interfaces, budget, inherited constraints), a `decisions.md` holding the node's semantic memory as an append-only log of distilled decisions with supersede markers, a `log/` directory of episodic traces compacted on rollup, an `artifacts/` directory of deliverables, and a `children/` directory of subordinate nodes. A small SQLite index carries what the scheduler needs — node status, dependency edges, budget ledger — but the filesystem remains the source of truth. Git supplies versioning, diffability, and human audit for free; HORMA's results justify the plain-filesystem substrate empirically.

### 4.2 The node runner

`run_node(node_id)` is a stateless function. It assembles the node's context — the contract, the chain of inherited constraints from ancestors, the node's own semantic memory, relevant entries retrieved from the global store, and (on resume) the compacted episodic trace — then spawns a fresh agent session over a headless agent SDK. The agent receives exactly four structured verbs, and the entire inter-layer protocol is these verbs:

`split(subtasks)` proposes child contracts; the orchestrator validates them (budget arithmetic, constraint inheritance) and creates child nodes. `complete(deliverable, summary)` submits work for verification against the acceptance criteria; the distilled summary is what rolls up. `escalate(assumption, evidence)` names an inherited constraint the node believes false, with evidence; the orchestrator suspends the branch and reopens the owning ancestor. `note_global(entry)` writes a lesson, convention, or skill to the cross-cutting store.

Everything not expressible in these verbs — budget enforcement, depth accounting, scheduling, verification, crash recovery — lives in orchestrator code the model cannot ignore. This is the decisive argument against implementing the harness as a prompt convention or skill file alone: a compliant model will mostly follow a described protocol, but guarantees require enforcement outside the model.

### 4.3 The scheduler and the escalation path

The scheduler loop is deliberately simple: select nodes whose dependencies are satisfied, run them (parallelizing independent siblings), process the structured result, repeat until the root completes or budgets exhaust. Escalation is the one intricate path. On `escalate`, the orchestrator marks the subtree suspended, re-enqueues the ancestor node that owns the challenged constraint with the child's evidence injected into its context, and the reopened ancestor either amends the constraint (the branch resumes under the new contract terms, with affected sibling branches notified through their own contract updates) or overrules with rationale (the child resumes, and the rationale enters its context) or re-plans the branch entirely (pruned children roll their episodic traces into the ancestor's log so the work is not lost as information, even when discarded as output).

### 4.4 Memory propagation rules

Three flows, each with a distinct policy. *Downward*: constraints only — the accumulated laws of all ancestors, entering every descendant's contract. *Upward*: distilled summaries and contract status only — a parent never ingests a child's episodic log except during failure forensics. *Lateral*: exclusively through the global store, never directly between branches, preserving the tree's isolation guarantees. Compaction is structural rather than temporal: a node's episodic trace is compacted when the node completes, because the tree — unlike a chat session — tells us exactly when detail stops being live context and becomes history.

### 4.5 Pluggable leaf execution

A leaf's executor is pluggable: it may be a bare LLM call, a tool-using session, or an entire existing coding agent (Claude Code, opencode) run headlessly in the node's directory with the contract as its prompt. The harness is thereby a meta-layer above existing agents rather than a competitor to them, inheriting their tooling while contributing the layer none of them has: the persistent tree, contracts, and escalation.

---

## 5. Build Roadmap

The roadmap is ordered so that every phase produces a usable system and each phase de-risks the next. Timeboxes assume one focused builder.

**Phase 0 — Skeleton (a weekend).** Tree store on disk, SQLite index, node runner with `split` and `complete` only, a hardcoded depth limit, and leaf execution as a plain LLM call. Exit criterion: a three-level toy project (e.g., "write a small documented library") runs end to end, survives a process kill, and resumes from disk. This phase alone reproduces ROMA-style recursion with the persistence ROMA lacks.

**Phase 1 — Contracts and verification (one to two weeks).** Formalize `contract.md`, implement constraint inheritance down the tree, and add the parent-side verification pass checking deliverables against acceptance criteria, with a bounded revise-and-resubmit loop on failure. Exit criterion: an injected deliberately-bad leaf deliverable is caught at the parent boundary, not at the root.

**Phase 2 — Budgets (one week).** Token/cost/depth ledgers per subtree; splitting debits the ledger; exhaustion forces the node to complete degraded or escalate. Remove the hardcoded depth limit and confirm the economics bound recursion instead. Exit criterion: recursion depth demonstrably responds to budget size on the same task.

**Phase 3 — Escalation (two to three weeks; the differentiating feature).** Implement the full suspend–reopen–resolve cycle of §4.3. Test with adversarial scenarios purpose-built to embed a false assumption at the root that only a leaf can discover (e.g., a spec that mandates a library incompatible with a requirement only visible during implementation). Exit criterion: the harness corrects the ancestor and completes; the baseline without escalation ships the flaw.

**Phase 4 — Global store (one to two weeks).** Blackboard-style store with `note_global`, supersede semantics, and retrieval into node hydration. Exit criterion: a lesson written by one branch measurably changes a sibling branch's behavior.

**Phase 5 — Pluggable executors and scale trials (ongoing).** Wire Claude Code / opencode headless as leaf executors; run the harness on a real multi-day project (a small product built end to end); instrument everything — per-node cost, depth distributions, escalation frequency, verification catch rate — because these traces are also the research dataset.

---

## 6. Research Agenda: Filling the Gaps in the Literature

Each gap in §3 converts into a testable claim, and the harness is the instrument for testing all of them.

**Claim 1 (persistence).** A persistent tree-as-memory harness completes long multi-session projects that execution-tree frameworks cannot resume, at comparable cost on single-session tasks. Test by killing and resuming runs at random points; measure completion rate and rework cost against ROMA-style baselines.

**Claim 2 (escalation).** Upward escalation materially improves outcomes on tasks with latent flawed assumptions. This needs a benchmark that does not yet exist — a suite of tasks with planted cross-layer assumption violations — and building that benchmark is itself a publishable contribution, since current long-horizon suites (ALFWorld, GAIA-style tasks, SWE-bench variants) do not isolate this failure mode. Metrics: flaw-shipped rate, cost of correction versus depth at which the flaw is discovered.

**Claim 3 (contracts).** Contract-based delegation with acceptance-criteria verification beats goal-string delegation on integration success at equal budget. Ablate the contract fields to find which carry the effect.

**Claim 4 (economic depth).** Budget-bounded recursion finds better depth/cost frontiers than fixed depth caps across heterogeneous tasks. Sweep budgets, plot quality against spend, compare against the best fixed cap chosen in hindsight.

**Claim 5 (lateral memory).** A supersede-aware global store improves cross-branch consistency (terminology drift, duplicated work, interface mismatches between sibling branches) versus tree-only memory flow.

**Claim 6 (localized verification).** Per-boundary verification localizes failures: measure the distance (in tree hops) between where an error is introduced and where it is detected, with and without contract verification.

A natural first paper is the escalation study (Claims 2 and 6 together): it is the sharpest gap, it has a clean ablation, and the benchmark it requires is reusable by the whole field. The persistence and contract studies follow with infrastructure already built.

---

## 7. Annotated Reading List

Ordered as a curriculum, not alphabetically.

1. **Sumers, Yao, Narasimhan, Griffiths — "Cognitive Architectures for Language Agents" (CoALA), 2023, arXiv:2309.02427.** The field's shared vocabulary: memory taxonomy, internal/external action spaces, decision loops. Read first.
2. **Packer et al. — "MemGPT: Towards LLMs as Operating Systems," 2023, arXiv:2310.08560.** Virtual context management, memory tiers, agent-driven paging; now the Letta framework. The founding memory paper.
3. **"ROMA: Recursive Open Meta-Agent Framework for Long-Horizon Multi-Agent Systems," 2026, arXiv:2602.01848** (code: github.com/sentient-agi/ROMA). Atomizer/Planner/Executor/Aggregator recursion over dependency-aware task trees. The closest prior system; study its engineering guidance and its absences equally.
4. **Erol, Hendler, Nau — "HTN Planning: Complexity and Expressivity," 1994**, and **Nau et al. — "SHOP2: An HTN Planning System," JAIR 2003.** The formal theory of recursive decomposition; decades of vocabulary the LLM literature is reinventing.
5. **Wang et al. — "Voyager: An Open-Ended Embodied Agent with Large Language Models," 2023.** Procedural memory as a growing, retrievable skill library.
6. **"Organize then Retrieve: Hierarchical Memory Navigation for Efficient Agents" (HORMA), 2026, arXiv:2606.11680.** Separate construction and retrieval agents over a file-system memory workspace; strong results under tight context budgets.
7. **ReCAP — "Recursive Context-Aware Reasoning and Planning with Language Models," Stanford, 2025.** Dynamic context trees and parent-context recall during subtask execution.
8. **"H2R: Hierarchical Hindsight Reflection for Multi-Task LLM Agents," 2025, arXiv:2509.12810.** Two-level insight distillation with add/modify/upvote/downvote maintenance.
9. **"MiTa: A Hierarchical Multi-Agent Collaboration Framework with Memory-integrated Task Allocation," 2026, arXiv:2601.22974.** The catalogue of hierarchical failure modes and centralized remedies.
10. **"AgentOrchestra: A Hierarchical Multi-Agent Framework for General-Purpose Task Solving," 2025, arXiv:2506.12508.** The fixed-depth baseline to beat.
11. **"Project Synapse," 2026, arXiv:2601.08156.** Working/episodic/semantic hybrid memory in a supervisor–worker hierarchy, applied end to end.
12. **A-MEM (2025)** for supersede-aware linked memory; **MemoryOS (2025)** for tiered storage policies; **Zep (2025)** for temporal knowledge graphs — the storage/retrieval design space for the global store.
13. **Laird — "The Soar Cognitive Architecture" (book, 2012)**, especially impasses and chunking, as the ancestor of escalation. **Erman et al. — "The Hearsay-II Speech-Understanding System," ACM Computing Surveys 1980**, for the blackboard model behind the global store.
14. **Anthropic engineering blog** — the multi-agent research system and context-engineering posts — for the practitioner baseline of orchestrator–worker patterns, compaction, and file-based state.
15. **Simon — "Administrative Behavior"** and **Galbraith — "Designing Complex Organizations"** for the organizational theory of bounded rationality, delegation, and escalation. Optional but clarifying.

---

## 8. Summary in One Paragraph

Long-horizon autonomy is a memory problem wearing a planning costume. The literature supplies recursion with local atomization (HTN, ROMA), memory taxonomy and paging (CoALA, MemGPT), hierarchical memory workspaces (HORMA), and cross-task procedural stores (Voyager, H2R) — but nowhere combines them into a persistent task tree that *is* the project's memory, bounded by contracts at every layer, governed by budget economics rather than depth caps, and equipped with the one mechanism nothing yet has: upward escalation that lets a leaf overturn an ancestor's assumption before the flaw ships. Build the skeleton first, the escalation channel early, and instrument everything — the traces are both your debugging tool and your dataset for the papers this design makes possible.
