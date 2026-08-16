"""The scheduler (SPEC.md 4.3, phase 0 subset).

Pick a runnable node, run it, apply the structured result, repeat until the
root is terminal.  Phase 0 is deliberately sequential and bounded by a
hardcoded depth limit; budgets, parallelism and escalation arrive later.

Everything the model must not be able to ignore lives here rather than in the
prompt: depth accounting, the refusal of an illegal split, crash recovery and
the decision that a node is finished.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

from .runner import (
    COMPLETE,
    ESCALATE,
    ESCALATE_RESOLVE,
    SPLIT,
    Result,
    RunnerError,
    run_node,
    verify_node,
)
from .store import (
    EVENTS_FILENAME,
    COMPLETE as STATUS_COMPLETE,
    FAILED,
    PENDING,
    RUNNING,
    SPLIT as STATUS_SPLIT,
    SUSPENDED,
    TERMINAL_STATUSES,
    Node,
    Store,
    StoreError,
    plan_artifacts,
)

MAX_DEPTH = 3
"""Hardcoded for phase 0; the root is depth 1, so at most three levels nest."""

MAX_ATTEMPTS = 3
"""How often one node may be asked again after a refused answer (a refusal
includes a failed verification round, so at most two revise rounds happen)."""

MAX_STEPS = 500
"""A backstop so a pathological tree cannot spin forever."""


@dataclass
class RunReport:
    steps: int = 0
    completed: int = 0
    split: int = 0
    refused: int = 0
    failed: int = 0
    root_status: str = PENDING

    @property
    def ok(self) -> bool:
        return self.root_status not in (FAILED,) and self.root_status in TERMINAL_STATUSES


def _deps_complete(store: Store, node: Node, stale: set[str]) -> bool:
    """Every dependency of ``node`` is accepted and not itself stale."""
    if not node.depends_on:
        return True
    by_id = {item.id: item for item in store.walk()}
    for dep in node.depends_on:
        dep_node = by_id.get(dep)
        if dep_node is None or dep_node.status != STATUS_COMPLETE or dep in stale:
            return False
    return True


def _aggregatable(
    store: Store, node: Node, nodes: list[Node], stale: set[str]
) -> bool:
    children = [item for item in nodes if item.parent == node.id]
    return bool(children) and all(
        child.status in TERMINAL_STATUSES and child.id not in stale
        for child in children
    )


def next_node(store: Store, nodes: list[Node], stale: set[str]) -> Node | None:
    """The next runnable node: stale dependents first (their dependency moved),
    then pending work whose dependencies are all accepted, then the deepest
    parent whose children have all come back."""
    for node in nodes:
        if node.id in stale:
            return node
    for node in nodes:
        if node.status == PENDING and _deps_complete(store, node, stale):
            return node
    ready = [
        node
        for node in nodes
        if node.status == STATUS_SPLIT and _aggregatable(store, node, nodes, stale)
    ]
    if not ready:
        return None
    return max(ready, key=lambda node: node.depth)


def _split_refusal(reason: str) -> str:
    return (
        f"Your split was refused by the orchestrator: {reason}. "
        "No child nodes were created. Complete this contract yourself now, "
        "with the complete tool, producing the deliverable and its artifacts."
    )


def _complete_refusal(reason: str) -> str:
    return (
        f"Your completion was refused by the orchestrator: {reason}. "
        "Nothing was recorded. Answer again with the complete tool, and give "
        "at least one artifact with a path and non-empty content."
    )


def _dependency_summaries(store: Store, node: Node) -> list[tuple[str, str, list[str]]]:
    """For each accepted dependency, its summary and the paths of the artifacts
    it delivered, to inject into the dependent's context (AC1.1)."""
    out: list[tuple[str, str, list[str]]] = []
    if not node.depends_on:
        return out
    by_id = {item.id: item for item in store.walk()}
    for dep in node.depends_on:
        dep_node = by_id.get(dep)
        if dep_node is None or dep_node.status != STATUS_COMPLETE:
            continue
        paths: list[str] = []
        if dep_node.artifacts_dir.is_dir():
            paths = [
                str(path.relative_to(dep_node.path))
                for path in sorted(dep_node.artifacts_dir.rglob("*"))
                if path.is_file()
            ]
        out.append((dep_node.id, dep_node.summary, paths))
    return out


def _apply_result(
    store: Store,
    node: Node,
    result: Result,
    report: RunReport,
    aggregating: bool,
    pre_remaining: int | None,
    children: list[Node],
) -> tuple[bool, str | None]:
    """Apply a split or complete result.  Returns (applied, rejection).

    ``applied`` is True when the node is settled (the caller returns).
    ``rejection`` is a message to feed back when a result was refused and the
    node should answer again.
    """
    if result.verb == SPLIT:
        reason = _reject_split(store, node, result, aggregating, pre_remaining)
        if reason is None:
            children = store.add_children(node, result.subtasks)
            store.append_decision(
                node,
                "split into " + ", ".join(child.id for child in children),
            )
            report.split += 1
            return True, None
        store.append_log(node, {"event": "split_refused", "reason": reason})
        store.append_decision(node, f"split refused: {reason}")
        report.refused += 1
        return False, _split_refusal(reason)

    if result.verb == COMPLETE:
        reason = _reject_complete(result, is_leaf=not children)
        if reason is None:
            criteria = node.contract().acceptance_criteria
            verdict, results = verify_node(
                store, node, result.deliverable, criteria
            )
            if verdict != "PASS":
                store.append_log(
                    node, {"event": "verify_failed", "criteria": results}
                )
                report.refused += 1
                return False, _verify_refusal(results)
            store.complete(
                node,
                summary=result.summary,
                deliverable=result.deliverable,
                artifacts=result.artifacts,
            )
            store.record_dependencies(node)
            store.append_decision(
                node,
                "verified: verdict=PASS; " + _criteria_line(results, criteria),
            )
            report.completed += 1
            return True, None
        store.append_log(node, {"event": "complete_refused", "reason": reason})
        store.append_decision(node, f"completion refused: {reason}")
        report.refused += 1
        return False, _complete_refusal(reason)

    return False, "unknown verb; answer with the split or complete tool"


def execute(store: Store, node: Node, *, report: RunReport) -> None:
    """Run one node and apply its result."""
    children = store.children(node)
    aggregating = node.status == STATUS_SPLIT
    child_summaries = (
        [(child.id, child.summary) for child in children] if aggregating else []
    )
    store.set_status(node, RUNNING)

    rejection: str | None = None
    for _ in range(MAX_ATTEMPTS):
        pre_remaining = store.budget_remaining(node.id) if store.budget_enabled() else None
        try:
            result: Result = run_node(
                store,
                node,
                child_summaries=child_summaries,
                dependency_summaries=_dependency_summaries(store, node),
                rejection=rejection,
            )
        except RunnerError as error:
            store.append_log(node, {"event": "error", "error": str(error)})
            rejection = (
                f"Your last answer could not be used: {error}. "
                "Answer with the split tool or the complete tool."
            )
            continue
        except Exception as error:  # the executor itself failed
            store.append_log(
                node, {"event": "error", "error": f"{type(error).__name__}: {error}"}
            )
            store.append_decision(node, f"failed: {type(error).__name__}: {error}")
            store.set_status(node, FAILED)
            report.failed += 1
            return

        if result.verb == ESCALATE:
            handle_escalate(store, node, result, report)
            return
        if result.verb == ESCALATE_RESOLVE:
            store.append_log(
                node, {"event": "error", "error": "a leaf returned escalate_resolve"}
            )
            store.append_decision(
                node, "failed: returned escalate_resolve without an escalation"
            )
            store.set_status(node, FAILED)
            report.failed += 1
            return

        applied, new_rejection = _apply_result(
            store, node, result, report, aggregating, pre_remaining, children
        )
        if applied:
            return
        rejection = new_rejection
        continue

    store.append_decision(node, "failed: no usable answer after repeated refusals")
    store.set_status(node, FAILED)
    report.failed += 1


# ---------------------------------------------------------------------------
# Upward escalation (SPEC.md 4.3 / Phase 3)
# ---------------------------------------------------------------------------


def find_escalation_point(
    store: Store, node: Node, assumption: str
) -> Node | None:
    """The nearest ancestor that owns the challenged assumption.

    An ancestor owns an assumption when its contract lists it as an inherited
    constraint.  When no ancestor names it (a discovered dependency: "my
    contract does not provide X; sibling B owns it"), the escalation goes to
    the direct parent, which owns the sibling topology.
    """
    ancestors = store.ancestors(node)  # root first, parent last
    for ancestor in reversed(ancestors):  # nearest first
        if assumption in ancestor.contract().constraints:
            return ancestor
    if node.parent is None:
        return None
    try:
        return store.get(node.parent)
    except StoreError:
        return None


def _resolve_sibling(store: Store, owner: Node, tag: str) -> Node | None:
    """Find a sibling of ``owner`` by its [N:<tag>] goal marker."""
    for child in store.children(owner):
        if f"[N:{tag}]" in child.goal:
            return child
    return None


def _suspend_branch(store: Store, node: Node, owner: Node) -> None:
    """Mark ``node`` and its ancestors down to (not including) ``owner``
    suspended, so nothing in the challenged branch runs while the escalation
    is being settled."""
    current = node
    while current is not None and current.id != owner.id:
        store.set_status(current, SUSPENDED)
        current = store.get(current.parent) if current.parent else None


def _resume_ancestors(store: Store, node: Node, owner: Node) -> None:
    """Return the suspended intermediate ancestors to SPLIT so they can
    aggregate once the escalated leaf has resumed."""
    parent = store.get(node.parent) if node.parent else None
    while parent is not None and parent.id != owner.id:
        store.set_status(parent, STATUS_SPLIT)
        parent = store.get(parent.parent) if parent.parent else None


def _resume_escalating(
    store: Store, node: Node, rationale: str | None, report: RunReport
) -> None:
    """Re-run the escalated node (under the new terms or the overruling
    rationale) and apply its answer."""
    children = store.children(node)
    aggregating = node.status == STATUS_SPLIT
    store.set_status(node, RUNNING)
    try:
        result = run_node(store, node, rationale=rationale)
    except RunnerError as error:
        store.append_decision(node, f"failed after escalation: {error}")
        store.set_status(node, FAILED)
        report.failed += 1
        return
    pre_remaining = store.budget_remaining(node.id) if store.budget_enabled() else None
    applied, rejection = _apply_result(
        store, node, result, report, aggregating, pre_remaining, children
    )
    if not applied:
        store.append_decision(node, f"failed after escalation: {rejection}")
        store.set_status(node, FAILED)
        report.failed += 1


def _compact_child_into(parent: Node, child: Node) -> None:
    """Preserve a pruned child's episodic trace in the ancestor's log/ so the
    work survives even when the child is deleted (AC3.5)."""
    parent.log_dir.mkdir(parents=True, exist_ok=True)
    lines = [
        f"# Compacted trace of pruned child {child.id}",
        f"- goal: {child.goal}",
        f"- summary: {child.summary or '(none)'}",
    ]
    decisions = child.decisions_path
    if decisions.is_file():
        lines.append("- decisions:")
        lines += [
            "    " + line for line in decisions.read_text(encoding="utf-8").splitlines()
        ]
    events = child.log_dir / EVENTS_FILENAME
    if events.is_file():
        lines.append("- events:")
        lines += [
            "    " + line for line in events.read_text(encoding="utf-8").splitlines()
        ]
    (parent.log_dir / f"compacted-{child.id}.md").write_text(
        "\n".join(lines) + "\n", encoding="utf-8"
    )


def _replan(store: Store, parent: Node, report: RunReport) -> None:
    """Prune the parent's children, compacting their logs into the parent's
    log/ before deleting them (AC3.5)."""
    for child in store.children(parent):
        _compact_child_into(parent, child)
        store.delete_node(child)
    store.set_status(parent, PENDING)


def _apply_resolution(
    store: Store,
    owner: Node,
    node: Node,
    resolve: Result,
    assumption: str,
    evidence: str,
    report: RunReport,
) -> None:
    resolution = resolve.resolution
    store.append_log(
        owner,
        {"event": "escalation_resolved", "resolution": resolution},
    )
    if resolution == "amend":
        if resolve.amended_constraint:
            store.amend_inherited_constraint(owner, assumption, resolve.amended_constraint)
        if resolve.amended_interface and resolve.target:
            target = _resolve_sibling(store, owner, resolve.target)
            if target is not None:
                store.add_interface(target, resolve.amended_interface)
        _resume_escalating(store, node, None, report)
        _resume_ancestors(store, node, owner)
        return
    if resolution == "overrule":
        _resume_escalating(store, node, resolve.rationale, report)
        _resume_ancestors(store, node, owner)
        return
    if resolution == "depends_on":
        dep = _resolve_sibling(store, owner, resolve.dependency)
        if dep is not None:
            store.add_depends_on(node, dep.id)
        store.set_status(node, PENDING)
        _resume_ancestors(store, node, owner)
        return
    if resolution == "replan":
        parent = store.get(node.parent) if node.parent else owner
        try:
            parent_resolve = run_node(
                store, parent, escalation=(assumption, evidence)
            )
        except RunnerError as error:
            store.append_decision(node, f"failed during re-plan: {error}")
            store.set_status(node, FAILED)
            report.failed += 1
            return
        if parent_resolve.resolution == "replan":
            _replan(store, parent, report)
        else:
            _resume_escalating(store, node, None, report)
            _resume_ancestors(store, node, owner)
        return
    store.append_decision(
        node, f"failed: escalation resolved with unknown resolution {resolution!r}"
    )
    store.set_status(node, FAILED)
    report.failed += 1


def handle_escalate(
    store: Store, node: Node, result: Result, report: RunReport
) -> None:
    """Suspend the branch, reopen the owning ancestor with the child's
    evidence, and apply its resolution (AC3.1-AC3.6)."""
    assumption = result.assumption
    evidence = result.evidence
    store.append_log(
        node, {"event": "escalate", "assumption": assumption, "evidence": evidence}
    )
    store.append_decision(
        node, f"escalated: {assumption}; evidence: {evidence}"
    )

    owner = find_escalation_point(store, node, assumption)
    if owner is None:
        store.append_decision(node, "failed: escalated with no owning ancestor")
        store.set_status(node, FAILED)
        report.failed += 1
        return

    _suspend_branch(store, node, owner)
    store.set_status(owner, RUNNING)
    try:
        resolve = run_node(store, owner, escalation=(assumption, evidence))
    except RunnerError as error:
        store.set_status(owner, STATUS_SPLIT)
        store.append_decision(node, f"failed: escalation resolution unusable: {error}")
        store.set_status(node, FAILED)
        report.failed += 1
        return
    store.set_status(owner, STATUS_SPLIT)
    _apply_resolution(
        store, owner, node, resolve, assumption, evidence, report
    )


def _has_cycle(edges: dict[str, list[str]]) -> bool:
    WHITE, GREY, BLACK = 0, 1, 2
    colour: dict[str, int] = {}

    def visit(node_id: str) -> bool:
        colour[node_id] = GREY
        for dep in edges.get(node_id, []):
            if dep not in colour:
                if visit(dep):
                    return True
            elif colour[dep] == GREY:
                return True
        colour[node_id] = BLACK
        return False

    return any(
        visit(node_id) for node_id in list(edges) if colour.get(node_id, WHITE) == WHITE
    )


def _reject_split(
    store: Store,
    node: Node,
    result: Result,
    aggregating: bool,
    remaining: int | None,
) -> str | None:
    """Return why this split must be refused, or None if it is legal.

    Phase 2 removes the hardcoded depth cap: when a budget ledger exists,
    recursion is bounded by economics (the split-fee plus the proposed child
    allocations must fit in the node's remaining allowance).  Without a budget
    the phase-0/1 depth cap still applies.
    """
    if aggregating:
        return "this node has already split once and its children are finished"
    if store.budget_enabled():
        fee = store.split_fee()
        proposed = sum(max(0, int(contract.allocation)) for contract in result.subtasks)
        if fee + proposed > (remaining if remaining is not None else 0):
            return (
                f"your proposed allocation is over budget: the {fee}-token "
                f"split-fee plus {proposed} in child allocations exceeds your "
                f"remaining token allowance of {remaining}"
            )
    elif node.depth >= MAX_DEPTH:
        return (
            f"the tree is limited to {MAX_DEPTH} levels and this node is already "
            f"at depth {node.depth}"
        )
    if not result.subtasks:
        return "a split must propose at least one subtask"
    edges = {
        contract.id or f"#{index}": [
            dep.strip() for dep in contract.depends_on
        ]
        for index, contract in enumerate(result.subtasks)
    }
    for deps in edges.values():
        for dep in deps:
            if dep and dep not in edges:
                return f"depends_on names an unknown sibling {dep!r}"
    if _has_cycle(edges):
        return (
            "the proposed split contains a dependency cycle; a circular "
            "dependency cannot be scheduled"
        )
    return None


def _verify_refusal(results: list[dict[str, Any]]) -> str:
    return (
        "Your completion was refused because it failed verification against "
        "your acceptance criteria. Nothing was recorded. Fix the deliverable "
        "so it satisfies every criterion, then answer again with the complete "
        "tool. Criteria checked: "
        + ", ".join(str(item.get("name") or item) for item in results)
    )


def _criteria_line(results: list[dict[str, Any]], fallback: list[str]) -> str:
    if results:
        return "criteria=[" + ", ".join(
            f"{item.get('name') or item}={item.get('pass')}" for item in results
        ) + "]"
    return "criteria=" + ", ".join(fallback)


def _reject_complete(result: Result, *, is_leaf: bool) -> str | None:
    """Return why this completion must be refused, or None if it is usable.

    A leaf is the only place work actually lands, so a leaf that claims to be
    finished having written nothing has not delivered its contract.  A node
    whose children carry the artifacts is judged by their deliverables, not
    by its own.
    """
    if is_leaf and not plan_artifacts(result.artifacts, result.deliverable):
        return (
            "a leaf must leave an artifact behind, and this completion carried "
            "neither an artifact with content nor a deliverable"
        )
    return None


def _drain_steers(store: Store) -> None:
    """Drain the steering inbox, applying each queued command."""
    items = store.drain_steer_queue()
    for item in items:
        command = item["command"]
        payload = item["payload"]
        if command == "amend-root":
            store.amend_root(payload["old"], payload["new"])
        elif command == "add":
            store.add_child(
                payload["parent_id"],
                payload["goal"],
                payload["acceptance_criteria"],
                interfaces=payload.get("interfaces"),
                constraints=payload.get("constraints"),
                depends_on=payload.get("depends_on"),
                allocation=payload.get("allocation", 0),
            )
        elif command == "remove":
            store.remove_subtree(payload["node_id"])


def run(store: Store, *, max_steps: int | None = None) -> RunReport:
    """Drive the tree until the root is terminal or there is nothing to run."""
    store.reconcile()
    _drain_steers(store)
    store.reconcile()
    report = RunReport()
    limit = max_steps or int(os.environ.get("FRACTAL_MAX_STEPS", MAX_STEPS))

    while report.steps < limit:
        nodes = store.walk()
        root = nodes[0] if nodes else None
        stale = store.stale_ids()
        node = next_node(store, nodes, stale)
        if node is None:
            break
        report.steps += 1
        execute(store, node, report=report)

    nodes = store.walk()
    report.root_status = nodes[0].status if nodes else PENDING
    return report
