"""`fractal init <goal>`, `fractal run`, `fractal status`.

All three commands operate on the project rooted at the working directory.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Sequence, TextIO

from . import scheduler
from .store import Store, StoreError

GOAL_WIDTH = 72


def _store(args: argparse.Namespace) -> Store:
    return Store(Path(args.project).resolve())


def cmd_init(args: argparse.Namespace, out: TextIO) -> int:
    goal = " ".join(args.goal).strip()
    if not goal:
        print("a goal is required: fractal init <goal>", file=sys.stderr)
        return 2
    with _store(args) as store:
        node = store.init(goal)
        print(f"initialised {node.path.relative_to(store.project)} ({node.status})", file=out)
        print(f"goal: {goal}", file=out)
    return 0


def cmd_run(args: argparse.Namespace, out: TextIO) -> int:
    with _store(args) as store:
        store.require_initialised()
        report = scheduler.run(store)
        print(
            f"ran {report.steps} node(s): {report.completed} completed, "
            f"{report.split} split, {report.refused} split(s) refused, "
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
                goal = goal[: GOAL_WIDTH - 1].rstrip() + "…"
            print(f"{indent}{node.id}  [{node.status}]  {goal}".rstrip(), file=out)
    return 0


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

    initialise = commands.add_parser("init", help="create tree/root/ for a goal")
    initialise.add_argument("goal", nargs="+", help="what the root node must achieve")
    initialise.set_defaults(handler=cmd_init)

    run = commands.add_parser("run", help="run the tree until the root is finished")
    run.set_defaults(handler=cmd_run)

    status = commands.add_parser("status", help="print the tree with its statuses")
    status.set_defaults(handler=cmd_status)
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
