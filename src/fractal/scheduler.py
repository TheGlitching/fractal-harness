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

from .runner import COMPLETE, SPLIT, Result, RunnerError, run_node
from .store import (
    FAILED,
    PENDING,
    RUNNING,
    SPLIT as STATUS_SPLIT,
    TERMINAL_STATUSES,
    Node,
    Store,
)

MAX_DEPTH = 3
"""Hardcoded for phase 0; the root is depth 1, so at most three levels nest."""

MAX_ATTEMPTS = 3
"""How often one node may be asked again after a refused answer."""

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


def _aggregatable(store: Store, node: Node, nodes: list[Node]) -> bool:
    children = [item for item in nodes if item.parent == node.id]
    return bool(children) and all(
        child.status in TERMINAL_STATUSES for child in children
    )


def next_node(store: Store, nodes: list[Node]) -> Node | None:
    """The next runnable node: pending work first, in on-disk order, then the
    deepest parent whose children have all come back."""
    for node in nodes:
        if node.status == PENDING:
            return node
    ready = [
        node
        for node in nodes
        if node.status == STATUS_SPLIT and _aggregatable(store, node, nodes)
    ]
    if not ready:
        return None
    return max(ready, key=lambda node: node.depth)


def _refusal(node: Node, reason: str) -> str:
    return (
        f"Your split was refused by the orchestrator: {reason}. "
        "No child nodes were created. Complete this contract yourself now, "
        "with the complete tool, producing the deliverable and its artifacts."
    )


def execute(store: Store, node: Node, *, report: RunReport) -> None:
    """Run one node and apply its result."""
    aggregating = node.status == STATUS_SPLIT
    child_summaries = (
        [(child.id, child.summary) for child in store.children(node)]
        if aggregating
        else []
    )
    store.set_status(node, RUNNING)

    rejection: str | None = None
    for _ in range(MAX_ATTEMPTS):
        try:
            result: Result = run_node(
                store,
                node,
                child_summaries=child_summaries,
                rejection=rejection,
                max_depth=MAX_DEPTH,
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
            reason = _reject_split(node, result, aggregating)
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
            rejection = _refusal(node, reason)
            continue

        if result.verb == COMPLETE:
            store.complete(
                node,
                summary=result.summary,
                deliverable=result.deliverable,
                artifacts=result.artifacts,
            )
            report.completed += 1
            return

    store.append_decision(node, "failed: no usable answer after repeated refusals")
    store.set_status(node, FAILED)
    report.failed += 1


def _reject_split(node: Node, result: Result, aggregating: bool) -> str | None:
    """Return why this split must be refused, or None if it is legal."""
    if aggregating:
        return "this node has already split once and its children are finished"
    if node.depth >= MAX_DEPTH:
        return (
            f"the tree is limited to {MAX_DEPTH} levels and this node is already "
            f"at depth {node.depth}"
        )
    if not result.subtasks:
        return "a split must propose at least one subtask"
    return None


def run(store: Store, *, max_steps: int | None = None) -> RunReport:
    """Drive the tree until the root is terminal or there is nothing to run."""
    store.reconcile()
    report = RunReport()
    limit = max_steps or int(os.environ.get("FRACTAL_MAX_STEPS", MAX_STEPS))

    while report.steps < limit:
        nodes = store.walk()
        root = nodes[0] if nodes else None
        if root is not None and root.status in TERMINAL_STATUSES:
            break
        node = next_node(store, nodes)
        if node is None:
            break
        report.steps += 1
        execute(store, node, report=report)

    nodes = store.walk()
    report.root_status = nodes[0].status if nodes else PENDING
    return report
