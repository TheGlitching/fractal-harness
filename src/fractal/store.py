"""The state layer (SPEC.md 4.1).

The project is a directory tree mirroring the task tree.  Every node is a
directory named after its node id and holding

    contract.md    goal, acceptance criteria, interfaces, inherited constraints
    decisions.md   append-only semantic memory of the node
    log/           episodic traces
    artifacts/     deliverables
    children/      subordinate nodes

A small SQLite index carries what the scheduler needs (node id, parent,
status, created_at); the filesystem stays the source of truth, so the index
is reconciled from disk on every load and can be rebuilt from it.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sqlite3
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator

ROOT_ID = "root"

TREE_DIRNAME = "tree"
GLOBAL_DIRNAME = "global"
STATE_DIRNAME = ".fractal"
INDEX_FILENAME = "index.db"

CONTRACT_FILENAME = "contract.md"
DECISIONS_FILENAME = "decisions.md"
LOG_DIRNAME = "log"
ARTIFACTS_DIRNAME = "artifacts"
CHILDREN_DIRNAME = "children"
NODE_SUBDIRS = (LOG_DIRNAME, ARTIFACTS_DIRNAME, CHILDREN_DIRNAME)

EVENTS_FILENAME = "events.jsonl"

PENDING = "pending"
RUNNING = "running"
SPLIT = "split"
SUSPENDED = "suspended"
COMPLETE = "complete"
FAILED = "failed"
TERMINAL_STATUSES = frozenset({COMPLETE, FAILED})

_SCHEMA = """
CREATE TABLE IF NOT EXISTS nodes (
    id          TEXT PRIMARY KEY,
    parent      TEXT,
    depth       INTEGER NOT NULL,
    status      TEXT NOT NULL,
    goal        TEXT NOT NULL DEFAULT '',
    summary     TEXT NOT NULL DEFAULT '',
    depends_on  TEXT NOT NULL DEFAULT '[]',
    dep_fp      TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent);
CREATE INDEX IF NOT EXISTS nodes_status ON nodes(status);
CREATE TABLE IF NOT EXISTS budget (
    node_id     TEXT PRIMARY KEY,
    parent      TEXT,
    allowance   INTEGER NOT NULL DEFAULT 0,
    calls       INTEGER NOT NULL DEFAULT 0,
    fee_paid    INTEGER NOT NULL DEFAULT 0,
    children    INTEGER NOT NULL DEFAULT 0,
    debits      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS steer_queue (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    command     TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS global_entries (
    id          TEXT PRIMARY KEY,
    type        TEXT NOT NULL,
    content     TEXT NOT NULL,
    superseded  INTEGER NOT NULL DEFAULT 0,
    supersedes  TEXT,
    created_at  TEXT NOT NULL
);
"""


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


class StoreError(RuntimeError):
    """Raised when the project on disk is not in a usable state."""


# ---------------------------------------------------------------------------
# Contracts (SPEC.md 4.1 / Gap 3: the contract is the layer boundary)
# ---------------------------------------------------------------------------


@dataclass
class Contract:
    """The boundary object handed to a node: what it must achieve and under
    which inherited laws.

    ``id`` is the model-proposed subtask id used only to resolve dependency
    edges at split time (mapped onto the real on-disk node id); ``depends_on``
    names the sibling subtasks a child needs finished before it starts.
    """

    goal: str
    acceptance_criteria: list[str] = field(default_factory=list)
    interfaces: list[str] = field(default_factory=list)
    constraints: list[str] = field(default_factory=list)
    id: str = ""
    depends_on: list[str] = field(default_factory=list)
    allocation: int = 0

    def render(self, node_id: str, depth: int, parent: str | None) -> str:
        def bullets(items: list[str]) -> str:
            if not items:
                return "- (none stated)\n"
            return "".join(f"- {item.strip()}\n" for item in items if str(item).strip())

        return (
            f"# Contract: {node_id}\n\n"
            f"- node: {node_id}\n"
            f"- parent: {parent or '(none)'}\n"
            f"- depth: {depth}\n\n"
            "## Goal\n\n"
            f"{self.goal.strip()}\n\n"
            "## Acceptance criteria\n\n"
            f"{bullets(self.acceptance_criteria)}\n"
            "## Interfaces\n\n"
            f"{bullets(self.interfaces)}\n"
            "## Inherited constraints\n\n"
            f"{bullets(self.constraints)}\n"
            "## depends_on\n\n"
            f"{bullets(self.depends_on)}"
        )

    @classmethod
    def parse(cls, text: str) -> "Contract":
        sections: dict[str, list[str]] = {}
        current: str | None = None
        for line in text.splitlines():
            if line.startswith("## "):
                current = line[3:].strip().lower()
                sections[current] = []
            elif current is not None:
                sections[current].append(line)

        def body(name: str) -> str:
            return "\n".join(sections.get(name, [])).strip()

        def items(name: str) -> list[str]:
            out: list[str] = []
            for line in sections.get(name, []):
                stripped = line.strip()
                if stripped.startswith("- "):
                    value = stripped[2:].strip()
                    if value and value != "(none stated)":
                        out.append(value)
            return out

        return cls(
            goal=body("goal"),
            acceptance_criteria=items("acceptance criteria"),
            interfaces=items("interfaces"),
            constraints=items("inherited constraints"),
            depends_on=items("depends_on"),
        )


@dataclass
class Node:
    """One node of the tree, as recorded on disk and in the index."""

    id: str
    path: Path
    parent: str | None
    depth: int
    status: str = PENDING
    goal: str = ""
    summary: str = ""
    depends_on: list[str] = field(default_factory=list)
    dep_fp: str = "{}"

    @property
    def contract_path(self) -> Path:
        return self.path / CONTRACT_FILENAME

    @property
    def decisions_path(self) -> Path:
        return self.path / DECISIONS_FILENAME

    @property
    def log_dir(self) -> Path:
        return self.path / LOG_DIRNAME

    @property
    def artifacts_dir(self) -> Path:
        return self.path / ARTIFACTS_DIRNAME

    @property
    def children_dir(self) -> Path:
        return self.path / CHILDREN_DIRNAME

    def contract(self) -> Contract:
        return Contract.parse(self.contract_path.read_text(encoding="utf-8"))


def _safe_relative_path(raw: str, fallback: str) -> Path:
    """Map a model-supplied artifact path onto a path inside the node."""
    candidate = Path(str(raw).strip().replace("\\", "/"))
    parts = [
        part
        for part in candidate.parts
        if part not in ("", ".", "..", "/") and not part.endswith(":")
    ]
    if not parts:
        return Path(fallback)
    return Path(*parts)


def plan_artifacts(
    artifacts: list[tuple[str, str]], deliverable: str = ""
) -> list[tuple[Path, str]]:
    """Decide which files a completion would leave behind, as paths relative
    to the node's ``artifacts/``.

    Stated once and shared, so the scheduler's check that a leaf delivered
    something and the writing of the files themselves can never disagree.
    """
    planned: list[tuple[Path, str]] = []
    for number, (raw_path, content) in enumerate(artifacts, start=1):
        if not str(content).strip():
            continue
        relative = _safe_relative_path(raw_path, f"artifact-{number:02d}.txt")
        planned.append((relative, str(content)))
    if not planned and str(deliverable).strip():
        planned.append((Path("deliverable.md"), str(deliverable)))
    return planned


@dataclass
class GlobalEntry:
    """One entry in the global cross-cutting semantic store."""

    id: str
    type: str
    content: str
    superseded: bool = False
    supersedes: str = ""
    created_at: str = ""


class Store:
    """Create, read and update nodes; keep the SQLite index in step."""

    def __init__(self, project: Path | str) -> None:
        self.project = Path(project).resolve()
        self.tree_dir = self.project / TREE_DIRNAME
        self.global_dir = self.project / GLOBAL_DIRNAME
        self.state_dir = self.project / STATE_DIRNAME
        self.index_path = self.state_dir / INDEX_FILENAME
        self._connection: sqlite3.Connection | None = None

    # -- lifecycle ---------------------------------------------------------

    @property
    def initialised(self) -> bool:
        return (self.tree_dir / ROOT_ID / CONTRACT_FILENAME).is_file()

    def require_initialised(self) -> None:
        if not self.initialised:
            raise StoreError(
                f"no fractal project in {self.project} (run `fractal init <goal>` first)"
            )

    @property
    def connection(self) -> sqlite3.Connection:
        if self._connection is None:
            self.state_dir.mkdir(parents=True, exist_ok=True)
            connection = sqlite3.connect(
                self.index_path, isolation_level=None, timeout=30.0
            )
            connection.row_factory = sqlite3.Row
            connection.execute("PRAGMA busy_timeout = 30000")
            # A rollback journal plus synchronous=FULL means a committed status
            # has reached the disk before the next model call starts, which is
            # what makes a SIGKILL survivable (AC0.3).
            connection.execute("PRAGMA journal_mode = DELETE")
            connection.execute("PRAGMA synchronous = FULL")
            connection.executescript(_SCHEMA)
            self._connection = connection
        return self._connection

    def close(self) -> None:
        if self._connection is not None:
            self._connection.close()
            self._connection = None

    def __enter__(self) -> "Store":
        return self

    def __exit__(self, *exc: object) -> bool:
        self.close()
        return False

    # -- transactions ------------------------------------------------------

    def _begin(self) -> sqlite3.Connection:
        connection = self.connection
        connection.execute("BEGIN IMMEDIATE")
        return connection

    @staticmethod
    def _commit(connection: sqlite3.Connection) -> None:
        connection.execute("COMMIT")

    # -- node creation -----------------------------------------------------

    def init(self, goal: str) -> Node:
        """Create tree/root/ and index it as a pending node (AC0.1)."""
        if self.initialised:
            raise StoreError(f"{self.project} already holds a fractal project")

        contract = Contract(
            goal=goal.strip(),
            acceptance_criteria=[
                "the goal is delivered in full",
                "every leaf leaves an artifact behind",
            ],
        )
        node = Node(
            id=ROOT_ID,
            path=self.tree_dir / ROOT_ID,
            parent=None,
            depth=1,
            status=PENDING,
            goal=contract.goal,
        )
        self._materialise(node, contract)

        connection = self._begin()
        self._insert(connection, node)
        budget_raw = os.environ.get("FRACTAL_BUDGET")
        if budget_raw is not None:
            connection.execute(
                "INSERT OR IGNORE INTO budget (node_id, parent, allowance)"
                " VALUES (?, ?, ?)",
                (ROOT_ID, None, int(budget_raw)),
            )
        self._commit(connection)
        return node

    def _materialise(self, node: Node, contract: Contract) -> None:
        node.path.mkdir(parents=True, exist_ok=True)
        for subdir in NODE_SUBDIRS:
            (node.path / subdir).mkdir(exist_ok=True)
        node.contract_path.write_text(
            contract.render(node.id, node.depth, node.parent), encoding="utf-8"
        )
        if not node.decisions_path.exists():
            node.decisions_path.write_text(
                f"# Decisions: {node.id}\n\n"
                "Append-only semantic memory of this node.\n\n"
                f"- {_now()} node created\n",
                encoding="utf-8",
            )

    def add_children(self, parent: Node, contracts: list[Contract]) -> list[Node]:
        """Create child nodes for an accepted `split` and mark the parent split.

        The directories are written first, then the index rows and the parent's
        new status land in a single transaction, so a crash can only ever leave
        directories the reconciliation pass adopts.
        """
        parent.children_dir.mkdir(parents=True, exist_ok=True)
        existing = len(self._child_dirs(parent.path))
        model_map: dict[str, str] = {}
        for offset, contract in enumerate(contracts, start=1):
            child_id = f"{parent.id}-{existing + offset:02d}"
            model_id = contract.id.strip() or child_id
            model_map[model_id] = child_id
        children: list[Node] = []
        for offset, contract in enumerate(contracts, start=1):
            child_id = f"{parent.id}-{existing + offset:02d}"
            depends_on = [
                model_map.get(dep.strip(), dep.strip()) for dep in contract.depends_on
            ]
            child = Node(
                id=child_id,
                path=parent.children_dir / child_id,
                parent=parent.id,
                depth=parent.depth + 1,
                status=PENDING,
                goal=contract.goal,
                depends_on=depends_on,
            )
            contract.depends_on = depends_on
            self._materialise(child, contract)
            children.append(child)

        connection = self._begin()
        for child in children:
            self._insert(connection, child)
        if self.budget_enabled():
            fee = self.split_fee()
            allocations = [max(0, int(contract.allocation)) for contract in contracts]
            connection.execute(
                "UPDATE budget SET fee_paid = fee_paid + ?, children = children + ?,"
                " debits = debits + ? WHERE node_id = ?",
                (fee, sum(allocations), fee, parent.id),
            )
            for child, allocation in zip(children, allocations):
                connection.execute(
                    "INSERT OR IGNORE INTO budget (node_id, parent, allowance)"
                    " VALUES (?, ?, ?)",
                    (child.id, parent.id, allocation),
                )
        connection.execute(
            "UPDATE nodes SET status = ?, updated_at = ? WHERE id = ?",
            (SPLIT, _now(), parent.id),
        )
        self._commit(connection)
        parent.status = SPLIT
        return children

    @staticmethod
    def _insert(connection: sqlite3.Connection, node: Node) -> None:
        stamp = _now()
        connection.execute(
            "INSERT OR IGNORE INTO nodes"
            " (id, parent, depth, status, goal, summary, depends_on, dep_fp,"
            "  created_at, updated_at)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                node.id,
                node.parent,
                node.depth,
                node.status,
                node.goal,
                node.summary,
                json.dumps(node.depends_on),
                node.dep_fp,
                stamp,
                stamp,
            ),
        )

    # -- status ------------------------------------------------------------

    def set_status(self, node: Node, status: str) -> None:
        connection = self._begin()
        connection.execute(
            "UPDATE nodes SET status = ?, updated_at = ? WHERE id = ?",
            (status, _now(), node.id),
        )
        self._commit(connection)
        node.status = status

    # -- budget ledger (SPEC.md gap 4 / Phase 2) ---------------------------

    def split_fee(self) -> int:
        return int(os.environ.get("FRACTAL_SPLIT_FEE", "200"))

    def budget_enabled(self) -> bool:
        """A per-subtree ledger exists when the root was initialised with a
        FRACTAL_BUDGET.  Without one the harness runs the phase-0/1 depth cap."""
        return self._budget_row(ROOT_ID) is not None

    def _budget_row(self, node_id: str) -> dict[str, Any] | None:
        row = self.connection.execute(
            "SELECT * FROM budget WHERE node_id = ?", (node_id,)
        ).fetchone()
        return dict(row) if row is not None else None

    def budget_remaining(self, node_id: str) -> int | None:
        """The token allowance still spendable by ``node_id`` and its subtree,
        with this node's split-fee set aside for the split it may propose."""
        row = self._budget_row(node_id)
        if row is None:
            return None
        return (
            row["allowance"]
            - row["calls"]
            - row["fee_paid"]
            - row["children"]
            - self.split_fee()
        )

    def debit_call(self, node: Node, tokens: int) -> None:
        """Charge a model call's actual usage to the node's ledger."""
        tokens = max(0, int(tokens or 0))
        if tokens == 0 or not self.budget_enabled():
            return
        connection = self._begin()
        connection.execute(
            "UPDATE budget SET calls = calls + ?, debits = debits + ?"
            " WHERE node_id = ?",
            (tokens, tokens, node.id),
        )
        self._commit(connection)

    def complete(
        self,
        node: Node,
        *,
        summary: str,
        deliverable: str = "",
        artifacts: list[tuple[str, str]] | None = None,
    ) -> list[Path]:
        """Write the deliverables, then record the node as complete.

        Artifacts hit the disk before the status does: a node indexed as
        complete has always left its artifacts behind.
        """
        written = self.write_artifacts(node, artifacts or [], deliverable)
        self.append_decision(
            node, f"completed: {summary.strip() or '(no summary given)'}"
        )
        connection = self._begin()
        connection.execute(
            "UPDATE nodes SET status = ?, summary = ?, updated_at = ? WHERE id = ?",
            (COMPLETE, summary.strip(), _now(), node.id),
        )
        self._commit(connection)
        node.status = COMPLETE
        node.summary = summary.strip()
        return written

    # -- escalation support (SPEC.md 4.3 / Phase 3) ------------------------

    def amend_inherited_constraint(
        self, owner: Node, old: str, new: str
    ) -> None:
        """Replace an inherited constraint the owner owns, in the owner and in
        every descendant's contract (SPEC.md 4.4 downward flow)."""
        if not new or not old:
            return
        self._replace_in_subtree(owner, f"- {old}", f"- {new}")

    def add_interface(self, node: Node, interface: str) -> None:
        """Expose a named interface on ``node``'s contract (amending a sibling
        so a discovered dependency is satisfied earlier, AC3.6)."""
        if not interface:
            return
        text = node.contract_path.read_text(encoding="utf-8")
        marker = "## Interfaces\n\n"
        head, found, tail = text.partition(marker)
        if not found:
            return
        if interface in text:
            return
        body, sep, rest = tail.partition("\n\n")
        line = f"- {interface}"
        body = body.replace("- (none stated)", line)
        if line not in body:
            body = body.rstrip("\n") + "\n" + line
        node.contract_path.write_text(
            head + marker + body + sep + rest, encoding="utf-8"
        )

    def add_depends_on(self, node: Node, dep_id: str) -> None:
        """Add a dependency edge ``node -> dep_id`` (the depends_on resolution,
        AC3.6)."""
        if not dep_id or dep_id in node.depends_on:
            return
        node.depends_on = node.depends_on + [dep_id]
        connection = self._begin()
        connection.execute(
            "UPDATE nodes SET depends_on = ?, updated_at = ? WHERE id = ?",
            (json.dumps(node.depends_on), _now(), node.id),
        )
        self._commit(connection)

    def delete_node(self, node: Node) -> None:
        """Remove a node's directory and index rows (re-plan pruning, AC3.5)."""
        import shutil

        shutil.rmtree(node.path, ignore_errors=True)
        connection = self._begin()
        connection.execute("DELETE FROM nodes WHERE id = ?", (node.id,))
        connection.execute("DELETE FROM budget WHERE node_id = ?", (node.id,))
        self._commit(connection)

    def _replace_in_subtree(self, node: Node, old: str, new: str) -> None:
        stack = [node]
        while stack:
            current = stack.pop()
            text = current.contract_path.read_text(encoding="utf-8")
            if old in text:
                current.contract_path.write_text(
                    text.replace(old, new), encoding="utf-8"
                )
            stack.extend(self.children(current))

    def write_artifacts(
        self,
        node: Node,
        artifacts: list[tuple[str, str]],
        deliverable: str = "",
    ) -> list[Path]:
        node.artifacts_dir.mkdir(parents=True, exist_ok=True)
        written: list[Path] = []
        for relative, content in plan_artifacts(artifacts, deliverable):
            target = node.artifacts_dir / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
            written.append(target)
        return written

    # -- dependency fingerprints (stale-flagging, AC1.4) -------------------

    @staticmethod
    def fingerprint(node: Node) -> str:
        """A stable digest of everything a node's deliverable comprises."""
        parts = [node.summary or ""]
        artifacts = node.artifacts_dir
        if artifacts.is_dir():
            for path in sorted(artifacts.rglob("*")):
                if path.is_file():
                    parts.append(path.relative_to(artifacts).as_posix())
                    parts.append(path.read_text(encoding="utf-8", errors="replace"))
        return hashlib.sha256("\x00".join(parts).encode("utf-8")).hexdigest()

    def record_dependencies(self, node: Node) -> None:
        """Snapshot the fingerprints of ``node``'s dependencies at acceptance."""
        if not node.depends_on:
            return
        current: dict[str, str] = {}
        for dep in node.depends_on:
            try:
                current[dep] = self.fingerprint(self.get(dep))
            except StoreError:
                current[dep] = ""
        connection = self._begin()
        connection.execute(
            "UPDATE nodes SET dep_fp = ? WHERE id = ?",
            (json.dumps(current), node.id),
        )
        self._commit(connection)
        node.dep_fp = json.dumps(current)

    def stale_ids(self) -> set[str]:
        """Complete nodes whose recorded dependency fingerprints no longer
        match disk (their dependency's deliverable changed after acceptance)."""
        stale: set[str] = set()
        for node in self.walk():
            if node.status != COMPLETE or not node.depends_on:
                continue
            recorded = json.loads(node.dep_fp or "{}")
            for dep in node.depends_on:
                try:
                    current = self.fingerprint(self.get(dep))
                except StoreError:
                    continue
                if recorded.get(dep) != current:
                    stale.add(node.id)
                    break
        return stale

    # -- memory ------------------------------------------------------------

    def append_decision(self, node: Node, entry: str) -> None:
        with node.decisions_path.open("a", encoding="utf-8") as handle:
            handle.write(f"- {_now()} {entry.strip()}\n")

    def append_log(self, node: Node, record: dict[str, Any]) -> None:
        node.log_dir.mkdir(parents=True, exist_ok=True)
        payload = dict(record)
        payload.setdefault("at", _now())
        payload.setdefault("node", node.id)
        with (node.log_dir / EVENTS_FILENAME).open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(payload, default=str) + "\n")

    # -- reading the tree --------------------------------------------------

    @staticmethod
    def _child_dirs(path: Path) -> list[Path]:
        children = path / CHILDREN_DIRNAME
        if not children.is_dir():
            return []
        return sorted(entry for entry in children.iterdir() if entry.is_dir())

    def _walk_disk(self) -> Iterator[tuple[Path, str, str | None, int]]:
        root = self.tree_dir / ROOT_ID
        if not root.is_dir():
            return

        def visit(path: Path, parent: str | None, depth: int):
            yield path, path.name, parent, depth
            for child in self._child_dirs(path):
                yield from visit(child, path.name, depth + 1)

        yield from visit(root, None, 1)

    def reconcile(self) -> None:
        """Make the index agree with the filesystem, which is authoritative.

        Adopts node directories the index has never seen, drops rows whose
        directory is gone, and repairs the statuses a crash can leave behind:
        a node that was running is pending again unless it had already split.
        """
        self.require_initialised()
        connection = self.connection
        rows = {
            row["id"]: dict(row) for row in connection.execute("SELECT * FROM nodes")
        }
        disk = list(self._walk_disk())
        seen: set[str] = set()

        self._begin()
        try:
            for path, node_id, parent, depth in disk:
                seen.add(node_id)
                has_children = bool(self._child_dirs(path))
                row = rows.get(node_id)
                if row is None:
                    node = Node(
                        id=node_id,
                        path=path,
                        parent=parent,
                        depth=depth,
                        status=SPLIT if has_children else PENDING,
                        goal=self._goal_on_disk(path),
                    )
                    self._insert(connection, node)
                    continue
                status = row["status"]
                repaired = status
                if status in (RUNNING, PENDING):
                    repaired = SPLIT if has_children else PENDING
                if (
                    repaired != status
                    or row["parent"] != parent
                    or row["depth"] != depth
                ):
                    connection.execute(
                        "UPDATE nodes SET status = ?, parent = ?, depth = ?,"
                        " updated_at = ? WHERE id = ?",
                        (repaired, parent, depth, _now(), node_id),
                    )
            for node_id in set(rows) - seen:
                connection.execute("DELETE FROM nodes WHERE id = ?", (node_id,))
        except Exception:
            self.connection.execute("ROLLBACK")
            raise
        self._commit(self.connection)

    @staticmethod
    def _goal_on_disk(path: Path) -> str:
        contract = path / CONTRACT_FILENAME
        if not contract.is_file():
            return ""
        return Contract.parse(contract.read_text(encoding="utf-8")).goal

    def walk(self) -> list[Node]:
        """Every node, depth-first pre-order, exactly as it sits on disk."""
        self.require_initialised()
        rows = {
            row["id"]: dict(row)
            for row in self.connection.execute("SELECT * FROM nodes")
        }
        nodes: list[Node] = []
        for path, node_id, parent, depth in self._walk_disk():
            row = rows.get(node_id, {})
            nodes.append(
                Node(
                    id=node_id,
                    path=path,
                    parent=parent,
                    depth=depth,
                    status=str(row.get("status") or PENDING),
                    goal=str(row.get("goal") or self._goal_on_disk(path)),
                    summary=str(row.get("summary") or ""),
                    depends_on=json.loads(str(row.get("depends_on") or "[]") or "[]"),
                    dep_fp=str(row.get("dep_fp") or "{}"),
                )
            )
        return nodes

    def get(self, node_id: str) -> Node:
        for node in self.walk():
            if node.id == node_id:
                return node
        raise StoreError(f"no node {node_id!r} in {self.tree_dir}")

    def children(self, node: Node) -> list[Node]:
        return [item for item in self.walk() if item.parent == node.id]

    def ancestors(self, node: Node) -> list[Node]:
        """Root first, immediate parent last."""
        by_id = {item.id: item for item in self.walk()}
        chain: list[Node] = []
        current = by_id.get(node.id)
        while current is not None and current.parent:
            current = by_id.get(current.parent)
            if current is None:
                break
            chain.append(current)
        chain.reverse()
        return chain

    # -- steer inbox (SPEC.md 4.3 / Phase 3.5) ---------------------------

    def has_running(self) -> bool:
        return bool(
            self.connection.execute(
                "SELECT 1 FROM nodes WHERE status = ? LIMIT 1", (RUNNING,)
            ).fetchone()
        )

    def enqueue_steer(self, command: str, payload: dict[str, Any]) -> int:
        connection = self._begin()
        cursor = connection.execute(
            "INSERT INTO steer_queue (command, payload, created_at)"
            " VALUES (?, ?, ?)",
            (command, json.dumps(payload), _now()),
        )
        row_id = cursor.lastrowid
        self._commit(connection)
        return row_id

    def drain_steer_queue(self) -> list[dict[str, Any]]:
        rows = self.connection.execute(
            "SELECT id, command, payload FROM steer_queue ORDER BY id"
        ).fetchall()
        items = [{"id": r["id"], "command": r["command"],
                  "payload": json.loads(r["payload"])} for r in rows]
        if not items:
            return []
        ids = [item["id"] for item in items]
        connection = self._begin()
        connection.executemany(
            "DELETE FROM steer_queue WHERE id = ?", [(i,) for i in ids]
        )
        self._commit(connection)
        return items

    # -- amend-root (Phase 3.5) -------------------------------------------

    @staticmethod
    def _contract_constraints_text(text: str) -> list[str]:
        sections: dict[str, list[str]] = {}
        current: str | None = None
        for line in text.splitlines():
            if line.startswith("## "):
                current = line[3:].strip().lower()
                sections[current] = []
            elif current is not None:
                sections[current].append(line)
        out: list[str] = []
        for line in sections.get("inherited constraints", []):
            stripped = line.strip()
            if stripped.startswith("- ") and stripped[2:].strip() and stripped[2:].strip() != "(none stated)":
                out.append(stripped[2:].strip())
        return out

    def amend_root(self, old_path: str, new_path: str,
                   *, dry_run: bool = False) -> list[str]:
        old_text = Path(old_path).read_text(encoding="utf-8")
        new_text = Path(new_path).read_text(encoding="utf-8")
        old_constraints = self._contract_constraints_text(old_text)
        new_constraints = self._contract_constraints_text(new_text)
        removed = [c for c in old_constraints if c not in new_constraints]
        added = [c for c in new_constraints if c not in old_constraints]
        changed: list[tuple[str, str]] = []
        while removed and added:
            changed.append((removed.pop(0), added.pop(0)))
        for r in removed:
            changed.append((r, ""))
        for a in added:
            changed.append(("", a))
        if not changed:
            return []

        affected: list[str] = []
        for old_val, new_val in changed:
            for node in self.walk():
                if node.id == ROOT_ID:
                    continue
                text = node.contract_path.read_text(encoding="utf-8")
                old_line = f"- {old_val}" if old_val else ""
                if not old_val or old_line not in text:
                    continue
                if node.id not in affected:
                    affected.append(node.id)
                if not dry_run:
                    new_line = f"- {new_val}" if new_val else ""
                    node.contract_path.write_text(text.replace(old_line, new_line))
                    if node.status == COMPLETE:
                        new_status = PENDING
                        if self.children(node):
                            new_status = SPLIT
                        self.set_status(node, new_status)

        if not dry_run:
            (self.tree_dir / ROOT_ID / CONTRACT_FILENAME).write_text(new_text)

        return affected

    # -- add child (Phase 3.5) --------------------------------------------

    def add_child(self, parent_id: str, goal: str,
                  acceptance_criteria: list[str],
                  interfaces: list[str] | None = None,
                  constraints: list[str] | None = None,
                  depends_on: list[str] | None = None,
                  allocation: int = 0,
                  *, dry_run: bool = False) -> str | None:
        try:
            parent = self.get(parent_id)
        except StoreError:
            return "parent not found"

        allocation = max(0, int(allocation))
        if self.budget_enabled():
            remaining = self.budget_remaining(parent_id)
            if remaining is not None:
                fee = self.split_fee()
                if allocation + fee > remaining:
                    return (
                        f"rejected: allocation {allocation} + split-fee {fee} "
                        f"exceeds parent's remaining budget {remaining}"
                    )

        deps = list(depends_on) if depends_on else []
        if parent_id in deps:
            return "rejected: depends_on creates a dependency cycle"
        if deps:
            ancestors = {a.id for a in self.ancestors(parent)}
            for dep in deps:
                if dep in ancestors:
                    return "rejected: depends_on creates a dependency cycle"

        if dry_run:
            return None

        contract = Contract(
            goal=goal.strip(),
            acceptance_criteria=list(acceptance_criteria),
            interfaces=list(interfaces) if interfaces else [],
            constraints=list(constraints) if constraints else [],
            depends_on=deps,
            allocation=allocation,
        )
        children = self.add_children(parent, [contract])
        return children[0].id if children else None

    # -- remove subtree (Phase 3.5) ---------------------------------------

    def remove_subtree(self, node_id: str, *, dry_run: bool = False) -> str | None:
        try:
            node = self.get(node_id)
        except StoreError:
            return "node not found"
        if node.id == ROOT_ID:
            return "rejected: cannot remove root"

        if dry_run:
            return None

        parent = self.get(node.parent) if node.parent else None
        if parent is not None:
            parent.log_dir.mkdir(parents=True, exist_ok=True)
            lines = [
                f"# Compacted trace of pruned child {node.id}",
                f"- goal: {node.goal}",
                f"- summary: {node.summary or '(none)'}",
            ]
            decisions = node.decisions_path
            if decisions.is_file():
                lines.append("- decisions:")
                lines += [
                    "    " + line
                    for line in decisions.read_text(encoding="utf-8").splitlines()
                ]
            events = node.log_dir / EVENTS_FILENAME
            if events.is_file():
                lines.append("- events:")
                lines += [
                    "    " + line
                    for line in events.read_text(encoding="utf-8").splitlines()
                ]
            (parent.log_dir / f"compacted-{node.id}.md").write_text(
                "\n".join(lines) + "\n", encoding="utf-8"
            )

        dependents = [
            n for n in self.walk()
            if node.id in n.depends_on and n.status == COMPLETE
        ]
        for dep in dependents:
            self.set_status(dep, PENDING)

        self.delete_node(node)
        return None

    # -- digest (Phase 3.5) ------------------------------------------------

    def generate_digest(self, out: Path | None = None) -> str:
        nodes = self.walk()
        done: list[str] = []
        blocked: list[str] = []
        pending: list[str] = []
        for node in nodes:
            goal = node.goal or ""
            tag = ""
            for match in re.findall(r"\[N:([A-Za-z0-9_]+)\]", goal):
                tag = match
                break
            label = f"{node.id} [{node.status}]"
            if tag:
                label += f" [N:{tag}]"
            label += f" {goal}"
            if node.status == COMPLETE:
                done.append(label)
            elif node.status in (FAILED, SUSPENDED):
                blocked.append(label)
            else:
                pending.append(label)

        parts: list[str] = ["# Digest\n"]
        parts.append("## Done\n")
        if done:
            parts.extend(f"- {d}\n" for d in done)
        else:
            parts.append("- (no completed tasks)\n")
        parts.append("\n## Blocked\n")
        if blocked:
            parts.extend(f"- {b}\n" for b in blocked)
        else:
            parts.append("- (no blocked tasks)\n")
        parts.append("\n## Next\n")
        if pending:
            parts.extend(f"- {n}\n" for n in pending)
        else:
            parts.append("- All tasks completed. No pending work.\n")

        text = "".join(parts)
        if out is not None:
            out.write_text(text, encoding="utf-8")
        return text

    # -- global store (SPEC.md 4.4 / Phase 4) ------------------------------

    def _next_global_id(self) -> str:
        row = self.connection.execute(
            "SELECT COUNT(*) AS cnt FROM global_entries"
        ).fetchone()
        return f"global-{row['cnt'] + 1:03d}"

    def note_global(
        self, entry_type: str, content: str, supersedes: str = ""
    ) -> str:
        eid = self._next_global_id()
        stamp = _now()

        if supersedes:
            connection = self._begin()
            connection.execute(
                "UPDATE global_entries SET superseded = 1 WHERE id = ?",
                (supersedes,),
            )
            self._commit(connection)

        connection = self._begin()
        connection.execute(
            "INSERT INTO global_entries (id, type, content, supersedes, created_at)"
            " VALUES (?, ?, ?, ?, ?)",
            (eid, entry_type, content, supersedes, stamp),
        )
        self._commit(connection)

        self.global_dir.mkdir(parents=True, exist_ok=True)
        (self.global_dir / f"{eid}.md").write_text(
            f"# {entry_type}: {eid}\n\n{content}\n", encoding="utf-8"
        )
        return eid

    def retrieve_global(
        self, query: str, k: int = 5
    ) -> list[GlobalEntry]:
        self.connection.execute("SELECT 1 FROM global_entries LIMIT 1").fetchone()
        rows = self.connection.execute(
            "SELECT * FROM global_entries WHERE superseded = 0"
        ).fetchall()
        entries = [
            GlobalEntry(
                id=row["id"],
                type=row["type"],
                content=row["content"],
                superseded=bool(row["superseded"]),
                supersedes=row["supersedes"] or "",
                created_at=row["created_at"],
            )
            for row in rows
        ]
        if not entries:
            return []

        query_lower = query.lower()
        keywords = [w for w in query_lower.split() if len(w) > 1]

        def score(entry: GlobalEntry) -> int:
            content_lower = entry.content.lower()
            if not keywords:
                return 0
            return sum(1 for kw in keywords if kw in content_lower)

        scored = [(e, score(e)) for e in entries]
        scored.sort(key=lambda pair: pair[1], reverse=True)
        top = scored[:k]
        if keywords:
            matched = [e for e, s in top if s > 0]
            return matched if matched else [e for e, _ in scored[:k]]
        return [e for e, _ in top]
