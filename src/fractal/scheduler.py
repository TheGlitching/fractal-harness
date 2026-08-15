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

from .runner import COMPLETE, SPLIT, Result, RunnerError, run_node, verify_node
from .store import (
    COMPLETE as STATUS_COMPLETE,
    FAILED,
    PENDING,
    RUNNING,
    SPLIT as STATUS_SPLIT,
    TERMINAL_STATUSES,
    Node,
    Store,
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

        if result.verb == SPLIT:
            reason = _reject_split(store, node, result, aggregating, pre_remaining)
            if reason is None:
                children = store.add_children(node, result.subtasks)
                store.append_decision(
                    node,
                    "split into " + ", ".join(child.id for child in children),
                )
                report.split += 1
                return
            store.append_log(node, {"event": "split_refused", "reason": reason})
            store.append_decision(node, f"split refused: {reason}")
            report.refused += 1
            rejection = _split_refusal(reason)
            continue

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
                    rejection = _verify_refusal(results)
                    continue
                store.complete(
                    node,
                    summary=result.summary,
                    deliverable=result.deliverable,
                    artifacts=result.artifacts,
                )
                store.record_dependencies(node)
                store.append_decision(
                    node,
                    "verified: verdict=PASS; "
                    + _criteria_line(results, criteria),
                )
                report.completed += 1
                return
            store.append_log(node, {"event": "complete_refused", "reason": reason})
            store.append_decision(node, f"completion refused: {reason}")
            report.refused += 1
            rejection = _complete_refusal(reason)
            continue

    store.append_decision(node, "failed: no usable answer after repeated refusals")
    store.set_status(node, FAILED)
    report.failed += 1


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


def run(store: Store, *, max_steps: int | None = None) -> RunReport:
    """Drive the tree until the root is terminal or there is nothing to run."""
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
