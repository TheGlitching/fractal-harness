"""`fractal init <goal>`, `fractal run`, `fractal status`, `fractal steer`,
`fractal digest`.

All commands operate on the project rooted at the working directory.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Sequence, TextIO

from . import scheduler
from .runner import call_model
from .store import Store, StoreError, ROOT_ID

GOAL_WIDTH = 72


def _store(args: argparse.Namespace) -> Store:
    return Store(Path(args.project).resolve())


def _text_from_model(message: object) -> str:
    content = getattr(message, "content", [])
    if isinstance(content, list):
        parts: list[str] = []
        for block in content:
            block_type = getattr(block, "type", None)
            text = getattr(block, "text", None)
            if block_type in (None, "text") and text:
                parts.append(str(text))
        return "".join(parts)
    return ""


# ---------------------------------------------------------------------------
# init / run / status
# ---------------------------------------------------------------------------


def cmd_init(args: argparse.Namespace, out: TextIO) -> int:
    goal = " ".join(args.goal).strip()
    if not goal:
        print("a goal is required: fractal init <goal>", file=sys.stderr)
        return 2
    with _store(args) as store:
        node = store.init(goal)
        print(
            f"initialised {node.path.relative_to(store.project)} ({node.status})",
            file=out,
        )
        print(f"goal: {goal}", file=out)
    return 0


def cmd_run(args: argparse.Namespace, out: TextIO) -> int:
    with _store(args) as store:
        store.require_initialised()
        report = scheduler.run(store)
        print(
            f"ran {report.steps} node(s): {report.completed} completed, "
            f"{report.split} split, {report.refused} answer(s) refused, "
            f"{report.failed} failed",
            file=out,
        )
        print(f"root: {report.root_status}", file=out)
        return 0 if report.ok else 1


def cmd_status(args: argparse.Namespace, out: TextIO) -> int:
    with _store(args) as store:
        store.require_initialised()
        store.reconcile()
        for node in store.walk():
            indent = "  " * (node.depth - 1)
            goal = " ".join(node.goal.split())
            if len(goal) > GOAL_WIDTH:
                goal = goal[: GOAL_WIDTH - 1].rstrip() + "\u2026"
            print(f"{indent}{node.id}  [{node.status}]  {goal}".rstrip(), file=out)
    return 0


# ---------------------------------------------------------------------------
# steer amend-root / add / remove
# ---------------------------------------------------------------------------


def _impact_amend_root(store: Store, old_path: str, new_path: str) -> str:
    affected = store.amend_root(old_path, new_path, dry_run=True)
    if not affected:
        return "Impact preview: no nodes would be affected."
    lines = [f"Impact preview: {len(affected)} node(s) affected:"]
    by_id = {n.id: n for n in store.walk()}
    for node_id in affected:
        node = by_id.get(node_id)
        goal = node.goal if node else "?"
        status = node.status if node else "?"
        lines.append(f"  {node_id} [{status}] {goal}")
    return "\n".join(lines)


def _impact_add(store: Store, parent_id: str) -> str:
    try:
        parent = store.get(parent_id)
    except StoreError:
        return "Impact preview: parent not found; no nodes affected."
    return f"Impact preview: would add a new child under {parent_id} ({parent.goal})."


def _impact_remove(store: Store, node_id: str) -> str:
    try:
        node = store.get(node_id)
    except StoreError:
        return "Impact preview: node not found; no nodes affected."
    dep_ids = [
        n.id
        for n in store.walk()
        if node_id in n.depends_on and n.status == "complete"
    ]
    lines = [
        f"Impact preview: would remove {node_id} ({node.goal}).",
        f"  dependents to stale-flag: {dep_ids if dep_ids else 'none'}",
    ]
    return "\n".join(lines)


def _maybe_queue_or_apply(store: Store, command: str, payload: dict, out: TextIO) -> int:
    if store.has_running():
        store.enqueue_steer(command, payload)
        print(
            "steer queued: a run is in progress; the change will be applied "
            "when the scheduler next resumes.",
            file=out,
        )
        return 0
    # Apply immediately.
    if command == "amend-root":
        affected = store.amend_root(payload["old"], payload["new"])
        print(f"amend-root applied; {len(affected)} node(s) stale-flagged.", file=out)
    elif command == "add":
        result = store.add_child(
            payload["parent_id"],
            payload["goal"],
            payload["acceptance_criteria"],
            interfaces=payload.get("interfaces"),
            constraints=payload.get("constraints"),
            depends_on=payload.get("depends_on"),
            allocation=payload.get("allocation", 0),
        )
        if isinstance(result, str) and result:
            if "rejected" in result or "not found" in result:
                print(f"add refused: {result}", file=sys.stderr)
                return 1
            print(f"child added: {result}", file=out)
        else:
            print(f"child added under {payload['parent_id']}", file=out)
    elif command == "remove":
        error = store.remove_subtree(payload["node_id"])
        if error:
            print(f"remove refused: {error}", file=sys.stderr)
            return 1
        print(f"subtree {payload['node_id']} removed.", file=out)
    return 0


def cmd_steer_amend_root(args: argparse.Namespace, out: TextIO) -> int:
    payload = {"old": args.old, "new": args.new}
    with _store(args) as store:
        store.require_initialised()
        if not args.confirm:
            print(_impact_amend_root(store, args.old, args.new), file=out)
            return 1
        return _maybe_queue_or_apply(store, "amend-root", payload, out)


def cmd_steer_add(args: argparse.Namespace, out: TextIO) -> int:
    payload = {
        "parent_id": args.parent_id,
        "goal": args.goal,
        "acceptance_criteria": args.acceptance_criteria,
        "allocation": int(args.allocation or 0),
        "depends_on": args.depends_on or [],
    }
    with _store(args) as store:
        store.require_initialised()
        if not args.confirm:
            print(_impact_add(store, args.parent_id), file=out)
            return 1
        return _maybe_queue_or_apply(store, "add", payload, out)


def cmd_steer_remove(args: argparse.Namespace, out: TextIO) -> int:
    payload = {"node_id": args.node_id}
    with _store(args) as store:
        store.require_initialised()
        if not args.confirm:
            print(_impact_remove(store, args.node_id), file=out)
            return 1
        return _maybe_queue_or_apply(store, "remove", payload, out)


# ---------------------------------------------------------------------------
# digest
# ---------------------------------------------------------------------------


def cmd_digest(args: argparse.Namespace, out: TextIO) -> int:
    with _store(args) as store:
        store.require_initialised()
        store.reconcile()
        nodes = store.walk()

        # Build a prompt and try the model.
        status_lines = []
        for node in nodes:
            status_lines.append(
                f"- {node.id} [{node.status}] depth={node.depth}: {node.goal}"
            )
        prompt = (
            "Summarise this fractal project tree into a three-paragraph digest "
            "with the sections ## Done, ## Blocked, and ## Next.  "
            "List every node by its exact on-disk id and N-tag (e.g. root-01, "
            "[N:ROOT]).  Write only the markdown.\n\n"
            + "\n".join(status_lines)
        )
        try:
            message = call_model(prompt, model="claude-haiku-5")
        except Exception:
            message = None

        model_text = _text_from_model(message) if message is not None else ""
        # Accept model output only if it looks like a narrative.
        model_lower = model_text.lower()
        has_narrative = (
            "done" in model_lower
            or "blocked" in model_lower
            or "next" in model_lower
        )
        if has_narrative:
            text = model_text.strip()
        else:
            text = store.generate_digest()

        digest_path = Path(args.project).resolve() / "digest.md"
        digest_path.write_text(text, encoding="utf-8")
        print(text, file=out)
    return 0


# ---------------------------------------------------------------------------
# parser
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fractal", description="Run a task as a persistent fractal tree."
    )
    parser.add_argument(
        "--project",
        default=".",
        help="project directory holding tree/ (default: the working directory)",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    init_p = commands.add_parser("init", help="create tree/root/ for a goal")
    init_p.add_argument("goal", nargs="+", help="what the root node must achieve")
    init_p.set_defaults(handler=cmd_init)

    run_p = commands.add_parser("run", help="run the tree until the root is finished")
    run_p.set_defaults(handler=cmd_run)

    status_p = commands.add_parser(
        "status", help="print the tree with its statuses"
    )
    status_p.set_defaults(handler=cmd_status)

    # ---- steer sub-command group ----
    steer_p = commands.add_parser("steer", help="redirect a running project")
    steer_subs = steer_p.add_subparsers(dest="steer_command", required=True)

    am_root = steer_subs.add_parser(
        "amend-root", help="change the root contract and propagate to inheritors"
    )
    am_root.add_argument("--old", required=True, help="path to old contract")
    am_root.add_argument("--new", required=True, help="path to new contract")
    am_root.add_argument(
        "--confirm", action="store_true",
        help="apply the change (otherwise only print impact preview)"
    )
    am_root.set_defaults(handler=cmd_steer_amend_root)

    add_child = steer_subs.add_parser(
        "add", help="splice a new child under a node"
    )
    add_child.add_argument("parent_id", help="parent node id")
    add_child.add_argument("--goal", required=True, help="child's goal")
    add_child.add_argument(
        "--acceptance-criteria", nargs="+", required=True,
        help="acceptance criteria"
    )
    add_child.add_argument(
        "--allocation", default=None, help="token allocation for the child"
    )
    add_child.add_argument(
        "--depends-on", nargs="*", default=None,
        help="node ids the child depends on"
    )
    add_child.add_argument(
        "--confirm", action="store_true",
        help="apply the change (otherwise only print impact preview)"
    )
    add_child.set_defaults(handler=cmd_steer_add)

    rem = steer_subs.add_parser(
        "remove", help="prune a subtree"
    )
    rem.add_argument("node_id", help="node id to remove")
    rem.add_argument(
        "--confirm", action="store_true",
        help="apply the change (otherwise only print impact preview)"
    )
    rem.set_defaults(handler=cmd_steer_remove)

    # ---- digest ----
    digest_p = commands.add_parser("digest", help="generate a project digest")
    digest_p.add_argument(
        "--since", default=None, help="only consider entries since this timestamp"
    )
    digest_p.add_argument(
        "--watch", action="store_true", help="regenerate on a timer (ponytail: not yet)"
    )
    digest_p.set_defaults(handler=cmd_digest)

    return parser


def main(argv: Sequence[str] | None = None, out: TextIO | None = None) -> int:
    args = build_parser().parse_args(list(argv) if argv is not None else None)
    try:
        return int(args.handler(args, out or sys.stdout))
    except StoreError as error:
        print(f"fractal: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":  # pragma: no cover - process entry point
    sys.exit(main())
