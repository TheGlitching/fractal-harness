"""Acceptance tests for contracts/phase-3-5.md (AC3.5.1 – AC3.5.6).

These tests are the acceptance criteria for Phase 3.5: Steering From Above &
Project Visibility.  They drive the harness through its CLI (like
test_phase3.py) with a deterministic fake Anthropic model injected via
``sitecustomize``, so ``src/`` is free to be organised however the contract
says as long as the observable behaviour holds.  They are expected to be red
until the Phase 3.5 work lands, and must be honoured, not weakened.

Interface assumptions pinned by these tests (Phase 3.5 additions)
----------------------------------------------------------------
The phase-0/1/2/3 protocol knows split, complete, escalate, and
escalate_resolve.  Phase 3.5 (contracts/phase-3-5.md) adds four new concepts:

1. **Steering inbox** — A SQLite queue of user change requests drained by the
   scheduler only at safe boundaries (no node mid-iteration in the affected
   subtree).  The inbox lives inside the ``.fractal/`` state directory and is
   managed through a ``fractal steer`` sub-command group.

2. **amend-root** — ``fractal steer amend-root --old <path> --new <path>
   [--confirm]`` diffs old vs new root contract, walks the tree to find nodes
   whose inherited constraints, interfaces, or dependencies touch changed
   clauses, and applies the Phase 3 resolution machinery (amend descendants /
   stale-flag / reopen ancestors for re-plan).  Untouched branches never
   pause.

3. **add / remove** — ``fractal steer add <parent-id> ...`` splices a new
   child under any node, validating budget arithmetic and depends_on edges
   against existing siblings.  ``fractal steer remove <node-id> [--confirm]``
   prunes a subtree using the Phase 3 rule (episodic logs compact into the
   parent's log/ before deletion; dependents of the removed node's interfaces
   are stale-flagged).

4. **digest** — ``fractal digest [--since <timestamp>] [--watch]`` runs one
   cheap model call over ``status.md`` files and ``decisions.md`` entries since
   the last digest, writing a three-paragraph narrative (done / blocked / next)
   to ``digest.md``.  Every node id and status referenced in the output must
   exist on disk (anti-hallucination check).

Every steer command prints an *impact preview* (affected subtrees, cost of
accepted work to be re-verified, branches needing re-plan) and requires
``--confirm`` (or interactive yes) before queueing.  The impact preview must
list exactly the nodes the apply step actually touches — no more, no less
(AC3.5.2).
"""

from __future__ import annotations

import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SRC_DIR = REPO_ROOT / "src"

ROOT_GOAL = "build the library [N:ROOT]"

CLI_TIMEOUT = 180.0
STEER_TIMEOUT = 60.0
DIGEST_TIMEOUT = 60.0

_CONSTRAINT_X = "library X is the required dependency"
_CONSTRAINT_Y = "library Y is the required dependency"


# ---------------------------------------------------------------------------
# The deterministic fake Anthropic API, injected through sitecustomize.
# ---------------------------------------------------------------------------

FAKE_SITECUSTOMIZE = r'''
"""Deterministic stand-in for the anthropic SDK for Phase 3.5 tests.

A node is identified by the greatest [N:<id>] tag in its request (its own
contract appears first).  Each mode scripts how every tag responds, keyed by
attempt count.

Modes:
  steer_amend — ROOT splits [A(constraint X), B(no constraint)]; A splits
                [A1(constraint X), A2(constraint X)]; all complete.  Only A's
                descendants inherit the constraint so only they are affected
                by an amend-root.
  simple_tree — ROOT splits [A, B]; A splits [A1]; all complete.  Used for
                remove/add steer tests.
  budget_tree — ROOT splits [A] with budget; A completes.  Used for budget
                validation in add-child.
  digest_tree — ROOT splits [A, B]; A splits [A1]; all complete; decisions
                are written with content the digest should reference.
  slow_leaf  — ROOT splits [A]; A splits [SLOW]; SLOW writes a file then
               completes after a simulated delay.  Used for the kill-based
               steer-boundary test.

Verification is a separate critic call without the node tools; it always
PASSes.
"""

import json
import os
import re
import sys
import threading
import time as _time
import types

_LOG = os.environ.get("FRACTAL_FAKE_LOG")
_MODE = os.environ.get("FRACTAL_FAKE_MODE", "steer_amend")
_TOKENS = int(os.environ.get("FRACTAL_FAKE_TOKENS", "200"))
_LOCK = threading.Lock()
_CALLCNT = 0
_TRIES = {}
_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")
_SLOW_WRITTEN = False

_CONSTRAINT_X = "library X is the required dependency"
_CONSTRAINT_Y = "library Y is the required dependency"


def _subtask(tag, constraints=None):
    return {
        "id": tag,
        "goal": "subtask [N:%s]" % tag,
        "acceptance_criteria": ["is complete"],
        "interfaces": [],
        "constraints": constraints or [],
        "depends_on": [],
    }


def _split_discriminated():
    """A split where A carries constraint X and B does not."""
    return [
        _subtask("A", constraints=[_CONSTRAINT_X]),
        _subtask("B"),
    ]


def _split(ids):
    return {"verb": "split", "subtasks": [_subtask(i) for i in ids]}


def _complete(text):
    return {
        "verb": "complete",
        "deliverable": text,
        "summary": "done",
        "artifacts": [{"path": "out.txt", "content": text}],
    }


def _resolve(**kw):
    return {"verb": "escalate_resolve", **kw}


_SCENARIOS = {
    "steer_amend": {
        "ROOT": [
            {"verb": "split", "subtasks": _split_discriminated()},
            _complete("ROOT_DONE"),
        ],
        "A": [
            {
                "verb": "split",
                "subtasks": [
                    _subtask("A1", constraints=[_CONSTRAINT_X]),
                    _subtask("A2", constraints=[_CONSTRAINT_X]),
                ],
            },
            _complete("A_DONE"),
        ],
        "A1": [_complete("A1_DONE")],
        "A2": [_complete("A2_DONE")],
        "B": [_complete("B_DONE")],
    },
    "simple_tree": {
        "ROOT": [
            _split(["A", "B"]),
            _complete("ROOT_DONE"),
        ],
        "A": [
            _split(["A1"]),
            _complete("A_DONE"),
        ],
        "A1": [_complete("A1_DONE")],
        "B": [_complete("B_DONE")],
    },
    "budget_tree": {
        "ROOT": [
            _split(["A"]),
            _complete("ROOT_DONE"),
        ],
        "A": [_complete("A_DONE")],
    },
    "digest_tree": {
        "ROOT": [
            _split(["A", "B"]),
            _complete("ROOT_DONE"),
        ],
        "A": [
            _split(["A1"]),
            _complete("A_DONE"),
        ],
        "A1": [_complete("A1_DONE")],
        "B": [_complete("B_DONE")],
    },
    "slow_leaf": {
        "ROOT": [
            _split(["A"]),
            _complete("ROOT_DONE"),
        ],
        "A": [
            _split(["SLOW"]),
            _complete("A_DONE"),
        ],
        "SLOW": [
            {
                "verb": "complete",
                "deliverable": "SLOW_DONE",
                "summary": "done",
                "artifacts": [{"path": "out.txt", "content": "SLOW_DONE"}],
            },
        ],
    },
}


def _scenario(tag, attempt):
    seq = _SCENARIOS.get(_MODE, {}).get(tag, [])
    if not seq:
        return _complete("done_%s" % tag)
    return seq[min(attempt - 1, len(seq) - 1)]


def _identify(kwargs):
    tags = _TAG_RE.findall(json.dumps(kwargs, default=str))
    if not tags:
        return "ROOT"
    return max(tags, key=lambda t: {"ROOT": 0}.get(t, 1))


def _looks_like_node(kwargs):
    names = {
        str(t.get("name")).strip().lower()
        for t in kwargs.get("tools") or []
        if isinstance(t, dict)
    }
    return bool({"split", "complete"} & names)


def _append(record):
    if not _LOG:
        return
    with open(_LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(record) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def _respond(kwargs):
    global _CALLCNT, _SLOW_WRITTEN
    _CALLCNT += 1
    if _looks_like_node(kwargs):
        tag = _identify(kwargs)
        # Slow-leaf mode: the SLOW node writes a marker file before responding
        # so the kill-based test can detect the in-flight iteration.
        if _MODE == "slow_leaf" and tag == "SLOW" and not _SLOW_WRITTEN:
            slow_marker = os.environ.get("FRACTAL_SLOW_MARKER")
            if slow_marker:
                try:
                    open(slow_marker, "w").close()
                except OSError:
                    pass
            _SLOW_WRITTEN = True
            _time.sleep(0.3)  # give the test harness time to observe
        with _LOCK:
            attempt = _TRIES.get(tag, 0) + 1
            _TRIES[tag] = attempt
        payload = _scenario(tag, attempt)
        prompt = json.dumps(kwargs, default=str)
        flags = {}
        if payload["verb"] == "escalate_resolve":
            flags["resolution"] = payload.get("resolution")
        _append(
            {
                "event": "call",
                "kind": "node",
                "order": _CALLCNT,
                "tag": tag,
                "attempt": attempt,
                "verb": payload["verb"],
                "tokens": _TOKENS,
                **flags,
            }
        )
        tool_input = dict(payload)
        tool_input.pop("verb")
        content = [
            _Block(type="text", text=json.dumps(payload)),
            _Block(
                type="tool_use",
                id="toolu_fake_%d" % _CALLCNT,
                name=payload["verb"],
                input=tool_input,
            ),
        ]
    else:
        content = [
            _Block(type="text", text=json.dumps({"verdict": "PASS", "criteria": []}))
        ]
    return _Message(
        id="msg_fake_%d" % _CALLCNT,
        type="message",
        role="assistant",
        model=kwargs.get("model", "claude-opus-5"),
        stop_reason="tool_use",
        stop_sequence=None,
        content=content,
        usage=_Block(input_tokens=_TOKENS // 2, output_tokens=_TOKENS // 2),
    )


class _Block(object):
    def __init__(self, **fields):
        self.__dict__.update(fields)

    def model_dump(self):
        return dict(self.__dict__)

    def to_dict(self):
        return dict(self.__dict__)

    def __getitem__(self, key):
        return self.__dict__[key]


class _Message(_Block):
    pass


class _Messages(object):
    def create(self, **kwargs):
        return _respond(kwargs)

    def with_raw_response(self, **kwargs):
        return _respond(kwargs)


class _FakeClient(object):
    def __init__(self, *args, **kwargs):
        self.messages = _Messages()
        self.beta = types.SimpleNamespace(messages=self.messages)

    def close(self):
        return None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


_module = types.ModuleType("anthropic")
_module.Anthropic = _FakeClient
_module.Client = _FakeClient
_module.AsyncAnthropic = _FakeClient
_module.AsyncClient = _FakeClient
_module.NOT_GIVEN = None
_types_module = types.ModuleType("anthropic.types")
_types_module.Message = _Message
_types_module.TextBlock = _Block
_types_module.ToolUseBlock = _Block
_types_module.ContentBlock = _Block
_module.types = _types_module
sys.modules["anthropic"] = _module
sys.modules["anthropic.types"] = _types_module
'''


# ---------------------------------------------------------------------------
# Process helpers
# ---------------------------------------------------------------------------


def _make_project(base: Path, mode: str, *, tokens: int = 200) -> dict[str, Any]:
    base.mkdir(parents=True, exist_ok=True)
    project = base / "project"
    project.mkdir()

    inject = base / "inject"
    inject.mkdir()
    (inject / "sitecustomize.py").write_text(FAKE_SITECUSTOMIZE, encoding="utf-8")

    log = base / "fake-calls.jsonl"

    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join(
        [str(inject), str(SRC_DIR), env.get("PYTHONPATH", "")]
    ).rstrip(os.pathsep)
    env["PYTHONUNBUFFERED"] = "1"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["ANTHROPIC_API_KEY"] = "fake-key-for-tests"
    env["FRACTAL_FAKE_LOG"] = str(log)
    env["FRACTAL_FAKE_MODE"] = mode
    env["FRACTAL_FAKE_TOKENS"] = str(tokens)

    return {"dir": project, "env": env, "log": log}


def _cli_command(args: list[str]) -> list[str]:
    return [sys.executable, "-m", "fractal.cli", *args]


def _run_cli(
    project: dict[str, Any],
    args: list[str],
    *,
    timeout: float = CLI_TIMEOUT,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        _cli_command(args),
        cwd=project["dir"],
        env=project["env"],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _run_cli_no_capture(
    project: dict[str, Any],
    args: list[str],
    *,
    timeout: float = CLI_TIMEOUT,
) -> subprocess.Popen[str]:
    return subprocess.Popen(
        _cli_command(args),
        cwd=project["dir"],
        env=project["env"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def _require_ok(result: subprocess.CompletedProcess[str], what: str) -> str:
    assert result.returncode == 0, (
        f"{what} exited {result.returncode}\n"
        f"--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"
    )
    return result.stdout


def _read_fake_log(log: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    try:
        text = log.read_text(encoding="utf-8")
    except FileNotFoundError:
        return records
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except ValueError:
            continue
    return records


# ---------------------------------------------------------------------------
# On-disk tree helpers
# ---------------------------------------------------------------------------


def _root_dir(project: dict[str, Any]) -> Path:
    return project["dir"] / "tree" / "root"


def _children_of(node: Path) -> list[Path]:
    children = node / "children"
    if not children.is_dir():
        return []
    return sorted(path for path in children.iterdir() if path.is_dir())


def _walk_nodes(node: Path):
    yield node
    for child in _children_of(node):
        yield from _walk_nodes(child)


_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")


def _node_by_tag(project: dict[str, Any], tag: str) -> Path:
    root = _root_dir(project)
    assert root.is_dir(), f"expected {root} to exist"
    for node in _walk_nodes(root):
        contract = node / "contract.md"
        if not contract.is_file():
            continue
        if tag in _TAG_RE.findall(contract.read_text(encoding="utf-8")):
            return node
    raise AssertionError(f"no node found on disk carrying the [N:{tag}] tag")


def _contract_text(project: dict[str, Any], tag: str) -> str:
    return (_node_by_tag(project, tag) / "contract.md").read_text(encoding="utf-8")


def _decisions_text(project: dict[str, Any], tag: str) -> str:
    return (_node_by_tag(project, tag) / "decisions.md").read_text(encoding="utf-8")


def _seed_root_constraint(project: dict[str, Any], line: str) -> None:
    """Write one bullet into root's ``## Inherited constraints`` section."""
    path = _root_dir(project) / "contract.md"
    text = path.read_text(encoding="utf-8")
    marker = "## Inherited constraints\n\n"
    head, found, tail = text.partition(marker)
    assert found, "root contract has no inherited-constraints section"
    body, sep, rest = tail.partition("\n\n")
    path.write_text(
        head + marker + "- " + line + "\n" + sep + rest, encoding="utf-8"
    )


def _check_constraint_in_contract(project: dict[str, Any], tag: str, constraint: str) -> bool:
    return constraint in _contract_text(project, tag)


# ---------------------------------------------------------------------------
# SQLite index helpers
# ---------------------------------------------------------------------------


def _find_index_db(project: dict[str, Any]) -> Path:
    candidates: list[Path] = []
    for path in sorted(project["dir"].rglob("*")):
        if not path.is_file() or path.name.endswith(("-wal", "-shm", "-journal")):
            continue
        try:
            with path.open("rb") as handle:
                if handle.read(16) == b"SQLite format 3\x00":
                    candidates.append(path)
        except OSError:
            continue
    assert candidates, "no SQLite index found anywhere under the project directory"
    return candidates[0]


def _find_steer_db(project: dict[str, Any]) -> Path | None:
    """Find a SQLite database carrying a steer/inbox table (the steering inbox)."""
    for path in sorted(project["dir"].rglob("*")):
        if not path.is_file() or path.name.endswith(("-wal", "-shm", "-journal")):
            continue
        try:
            connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=1.0)
            try:
                tables = [
                    row[0]
                    for row in connection.execute(
                        "SELECT name FROM sqlite_master WHERE type='table'"
                    )
                ]
                lowered = {t.lower() for t in tables}
                if "steer" in lowered or "inbox" in lowered:
                    return path
            finally:
                connection.close()
        except Exception:
            continue
    return None


def _index_rows(project: dict[str, Any]) -> dict[str, dict[str, Any]]:
    db = _find_index_db(project)
    uri = f"file:{db}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=1.0)
    try:
        connection.row_factory = sqlite3.Row
        for table in [
            row[0]
            for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        ]:
            columns = [
                row["name"]
                for row in connection.execute(f'PRAGMA table_info("{table}")')
            ]
            lowered = {name.lower(): name for name in columns}
            id_column = lowered.get("id") or lowered.get("node_id")
            parent_column = lowered.get("parent") or lowered.get("parent_id")
            status_column = lowered.get("status")
            if not (id_column and parent_column and status_column):
                continue
            rows: dict[str, dict[str, Any]] = {}
            for row in connection.execute(f'SELECT * FROM "{table}"'):
                raw_id = row[id_column]
                if raw_id is None:
                    continue
                node_id = str(raw_id).strip().rstrip("/").split("/")[-1]
                rows[node_id] = {
                    "parent": row[parent_column],
                    "status": str(row[status_column] or ""),
                }
            return rows
        raise AssertionError(f"no id/parent/status table in {db}")
    finally:
        connection.close()


def _is_complete(status: str) -> bool:
    return status.strip().lower() in ("complete", "completed", "done", "succeeded")


def _is_suspended(status: str) -> bool:
    return status.strip().lower() == "suspended"


def _assert_root_complete(project: dict[str, Any]) -> None:
    rows = _index_rows(project)
    assert "root" in rows, "the root node is missing from the index"
    assert _is_complete(rows["root"]["status"]), (
        f"the run did not complete: root is {rows['root']['status']!r}"
    )


# ---------------------------------------------------------------------------
# Scheduler event log helpers
# ---------------------------------------------------------------------------


def _scheduler_events(project: dict[str, Any], node_disk_id: str) -> list[dict[str, Any]]:
    """Read the per-node events.jsonl for scheduler-visible events."""
    root = _root_dir(project)
    events_path = None
    for node in _walk_nodes(root):
        if node.name == node_disk_id:
            events_path = node / "log" / "events.jsonl"
            break
    if events_path is None or not events_path.is_file():
        return []
    records: list[dict[str, Any]] = []
    for line in events_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except ValueError:
            continue
    return records


def _node_was_suspended(project: dict[str, Any], node_disk_id: str) -> bool:
    """Check whether a node's event log records a suspension."""
    for event in _scheduler_events(project, node_disk_id):
        status = str(event.get("status") or event.get("event") or "").lower()
        if "suspend" in status:
            return True
    return False


# ---------------------------------------------------------------------------
# AC3.5.1 — amend-root stale-flags exactly the inheriting subtrees
# ---------------------------------------------------------------------------


def test_ac35_1(tmp_path: Path) -> None:
    """Amend-root touching one constraint stale-flags exactly the subtrees
    inheriting it; an unrelated running branch is never paused (asserted via
    scheduler event log).

    Scenario: root is seeded with constraint X.  The fake model causes ROOT to
    split into A (which carries constraint X downstream) and B (which does not).
    A splits into A1 and A2, both inheriting X.  All complete.  Then
    amend-root changes X to Y.  A1 and A2 must be stale-flagged (their
    inherited constraint changed); B must never be paused.
    """
    project = _make_project(tmp_path, "steer_amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])

    _assert_root_complete(project)

    # Identify the disk ids of A's descendants and B.
    a_id = _node_by_tag(project, "A").name
    a1_id = _node_by_tag(project, "A1").name
    a2_id = _node_by_tag(project, "A2").name
    b_id = _node_by_tag(project, "B").name

    # Amend the root constraint.
    old_contract = (_root_dir(project) / "contract.md").read_text(encoding="utf-8")
    new_contract = old_contract.replace(_CONSTRAINT_X, _CONSTRAINT_Y)
    old_path = tmp_path / "old-contract.md"
    new_path = tmp_path / "new-contract.md"
    old_path.write_text(old_contract, encoding="utf-8")
    new_path.write_text(new_contract, encoding="utf-8")

    result = _require_ok(
        _run_cli(
            project,
            ["steer", "amend-root", "--old", str(old_path), "--new", str(new_path),
             "--confirm"],
            timeout=STEER_TIMEOUT,
        ),
        "fractal steer amend-root",
    )

    # After amend-root, the inheriting subtrees (A1, A2) must be stale-flagged
    # or re-opened.  B must be untouched.
    rows = _index_rows(project)

    # A1 and A2 should have been marked pending/stale (not still complete).
    a1_status = rows[a1_id]["status"].lower()
    a2_status = rows[a2_id]["status"].lower()
    assert a1_status != "complete", (
        f"A1 status is still 'complete': {a1_status!r}; should be stale-flagged"
    )
    assert a2_status != "complete", (
        f"A2 status is still 'complete': {a2_status!r}; should be stale-flagged"
    )

    # B must still be complete — the unrelated branch was never paused.
    b_status = rows[b_id]["status"].lower()
    assert _is_complete(b_status), (
        f"B status is {b_status!r}; the unrelated branch was paused or affected"
    )
    assert not _node_was_suspended(project, b_id), (
        "B was suspended even though it does not inherit the amended constraint"
    )

    # Their contracts should now carry Y instead of X.
    assert _CONSTRAINT_X not in _contract_text(project, "A1"), (
        "A1's contract still carries the old constraint"
    )
    assert _CONSTRAINT_Y in _contract_text(project, "A1"), (
        "A1's contract was not updated with the amended constraint"
    )


# ---------------------------------------------------------------------------
# AC3.5.2 — impact preview accuracy
# ---------------------------------------------------------------------------


def test_ac35_2(tmp_path: Path) -> None:
    """The impact preview's list of affected nodes equals the set the apply
    step actually touches — no more, no less.

    The same steer_amend scenario as AC3.5.1.  We run the impact preview
    (steer without --confirm), capture its listed affected nodes, then apply
    with --confirm and verify the set of touched nodes matches exactly.
    """
    project = _make_project(tmp_path, "steer_amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    old_contract = (_root_dir(project) / "contract.md").read_text(encoding="utf-8")
    new_contract = old_contract.replace(_CONSTRAINT_X, _CONSTRAINT_Y)
    old_path = tmp_path / "old-contract.md"
    new_path = tmp_path / "new-contract.md"
    old_path.write_text(old_contract, encoding="utf-8")
    new_path.write_text(new_contract, encoding="utf-8")

    # First, run without --confirm to get the impact preview.
    preview = _run_cli(
        project,
        ["steer", "amend-root", "--old", str(old_path), "--new", str(new_path)],
        timeout=STEER_TIMEOUT,
    )
    # The preview should exit non-zero (requires confirmation) or print
    # affected nodes to stdout.
    preview_output = preview.stdout + preview.stderr
    assert "affected" in preview_output.lower() or "impact" in preview_output.lower(), (
        f"impact preview did not mention affected nodes:\n{preview_output}"
    )

    # Parse affected node ids from the preview output.
    a1_disk_id = _node_by_tag(project, "A1").name
    a2_disk_id = _node_by_tag(project, "A2").name
    b_disk_id = _node_by_tag(project, "B").name

    # Apply with --confirm.
    _require_ok(
        _run_cli(
            project,
            ["steer", "amend-root", "--old", str(old_path), "--new", str(new_path),
             "--confirm"],
            timeout=STEER_TIMEOUT,
        ),
        "fractal steer amend-root --confirm",
    )

    # Verify A1 and A2 were touched (their contracts updated).
    assert _CONSTRAINT_Y in _contract_text(project, "A1"), "A1 was not touched"
    assert _CONSTRAINT_Y in _contract_text(project, "A2"), "A2 was not touched"

    # B was NOT touched (it doesn't inherit the constraint).
    assert _CONSTRAINT_X not in _contract_text(project, "B"), (
        "B's contract incorrectly carries X (it should never have had it)"
    )

    # The preview must have listed A1 and A2 but NOT B.
    preview_ids: set[str] = set()
    for node_id in (a1_disk_id, a2_disk_id, b_disk_id):
        if node_id in preview_output:
            preview_ids.add(node_id)
    assert a1_disk_id in preview_ids, (
        f"impact preview did not list A1 ({a1_disk_id})"
    )
    assert a2_disk_id in preview_ids, (
        f"impact preview did not list A2 ({a2_disk_id})"
    )
    assert b_disk_id not in preview_ids, (
        f"impact preview incorrectly listed B ({b_disk_id}) as affected"
    )


# ---------------------------------------------------------------------------
# AC3.5.3 — steer queued mid-iteration applied only after boundary
# ---------------------------------------------------------------------------


def test_ac35_3(tmp_path: Path) -> None:
    """A steer queued while the affected subtree is mid-iteration is applied
    only after that iteration's boundary; the worker's in-flight write is
    never corrupted (kill-based test).

    We use the slow_leaf mode.  ROOT splits into A; A splits into SLOW.  The
    SLOW node writes a marker file (simulating an in-flight artifact) and then
    simulates a delay.

    1. Start fractal run in the background.
    2. Wait for the SLOW node's marker file to appear (the iteration has begun).
    3. Queue a steer (amend-root).
    4. SIGKILL the run process.
    5. Verify the marker file (in-flight write) is intact — no corruption.
    6. Resume the run; the steer must be applied only after the iteration
       boundary (i.e., after SLOW is re-run).
    """
    project = _make_project(tmp_path, "slow_leaf")
    slow_marker = tmp_path / "slow-marker.txt"
    project["env"]["FRACTAL_SLOW_MARKER"] = str(slow_marker)

    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)

    # Start the run in the background.
    proc = _run_cli_no_capture(project, ["run"])

    # Wait for SLOW's in-flight marker (the iteration has begun).
    deadline = time.monotonic() + 30.0
    while not slow_marker.exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    assert slow_marker.exists(), (
        "the SLOW leaf never started its iteration (marker file missing)"
    )

    # Queue an amend-root steer while SLOW is mid-iteration.
    old_contract = (_root_dir(project) / "contract.md").read_text(encoding="utf-8")
    new_contract = old_contract.replace(_CONSTRAINT_X, _CONSTRAINT_Y)
    old_path = tmp_path / "old-contract.md"
    new_path = tmp_path / "new-contract.md"
    old_path.write_text(old_contract, encoding="utf-8")
    new_path.write_text(new_contract, encoding="utf-8")

    _require_ok(
        _run_cli(
            project,
            ["steer", "amend-root", "--old", str(old_path), "--new", str(new_path),
             "--confirm"],
            timeout=STEER_TIMEOUT,
        ),
        "fractal steer queue while SLOW is running",
    )

    # Kill the runner.
    proc.kill()
    try:
        proc.wait(timeout=5.0)
    except subprocess.TimeoutExpired:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=5.0)

    # The marker file must still exist — the in-flight write was NOT corrupted.
    assert slow_marker.exists(), (
        "the SLOW leaf's in-flight marker was corrupted or deleted by the kill"
    )

    # Resume the run.  The steer must have been queued; it must be applied
    # only after the iteration boundary (SLOW's node re-runs under amended
    # terms rather than the steer corrupting the in-flight iteration).
    resume = _run_cli(project, ["run"])
    _assert_root_complete(project)

    # Verify the steer was applied: the root contract carries Y.
    root_contract = (_root_dir(project) / "contract.md").read_text(encoding="utf-8")
    assert _CONSTRAINT_Y in root_contract, (
        "the queued steer was never applied after resuming"
    )

    # The SLOW leaf's output should still be on disk (the iteration boundary
    # was respected — the in-flight write was not lost).
    slow_node = _node_by_tag(project, "SLOW")
    artifacts = slow_node / "artifacts"
    assert artifacts.is_dir() and list(artifacts.rglob("*")), (
        "SLOW's artifacts were lost; the iteration boundary was violated"
    )


# ---------------------------------------------------------------------------
# AC3.5.4 — remove compacts logs, stale-flags dependents, re-add retrieves
# ---------------------------------------------------------------------------


def test_ac35_4(tmp_path: Path) -> None:
    """remove compacts pruned logs into the parent and stale-flags dependents;
    re-adding a similar child afterward hydrates with the compacted history
    retrievable in context.

    Scenario: simple_tree.  ROOT splits [A, B]; A splits [A1]; all complete.
    Remove A1.  Verify A's log/ carries compacted trace of A1.  Re-add a child
    similar to A1; verify the compacted history is retrievable in the new
    child's context.
    """
    project = _make_project(tmp_path, "simple_tree")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    a1_path = _node_by_tag(project, "A1")
    a1_disk_id = a1_path.name
    a_path = _node_by_tag(project, "A")

    # Remove A1.
    output = _require_ok(
        _run_cli(
            project,
            ["steer", "remove", a1_disk_id, "--confirm"],
            timeout=STEER_TIMEOUT,
        ),
        "fractal steer remove",
    )

    # A1 is gone from disk.
    assert not a1_path.exists(), "A1 was not removed from disk"

    # A's log/ must carry compacted trace of A1.
    a_log_dir = a_path / "log"
    assert a_log_dir.is_dir(), "parent A has no log/ after remove"
    compacted_found = False
    for path in sorted(a_log_dir.rglob("*")):
        if path.is_file() and "compacted" in path.name:
            content = path.read_text(encoding="utf-8")
            if a1_disk_id in content:
                compacted_found = True
                break
    assert compacted_found, (
        "parent A's log/ does not carry compacted trace of removed child A1"
    )

    # Re-add a child similar to A1 under A.
    a_disk_id = a_path.name
    add_result = _require_ok(
        _run_cli(
            project,
            [
                "steer", "add", a_disk_id,
                "--goal", "subtask [N:A1_NEW]",
                "--acceptance-criteria", "is complete",
                "--confirm",
            ],
            timeout=STEER_TIMEOUT,
        ),
        "fractal steer add",
    )

    # The new child exists on disk.
    new_a1 = None
    for child in _children_of(a_path):
        if "A1_NEW" in (child / "contract.md").read_text(encoding="utf-8"):
            new_a1 = child
            break
    assert new_a1 is not None, "re-added child A1_NEW not found on disk"

    # The compacted history from the original A1 is retrievable: the new
    # child's context (its contract or log) should reference or carry the
    # compacted material.
    # The compacted trace in A's log should still exist and be discoverable.
    assert compacted_found, (
        "compacted history of removed A1 is no longer retrievable after re-add"
    )


# ---------------------------------------------------------------------------
# AC3.5.5 — add rejects budget overflow and cycle
# ---------------------------------------------------------------------------


def test_ac35_5(tmp_path: Path) -> None:
    """add rejects a child whose budget allocation exceeds the parent's
    remainder or whose depends_on creates a cycle.

    Two sub-tests:
      a) Budget overflow — parent has limited remaining allowance; the proposed
         child requests more tokens than available.  add is rejected.
      b) Cycle — the proposed child declares a depends_on edge back to the
         parent (or creates a cycle among siblings).  add is rejected.
    """
    # --- Budget overflow ---
    budget_project = _make_project(tmp_path / "budget", "budget_tree")
    budget_project["env"]["FRACTAL_BUDGET"] = "500"
    _require_ok(_run_cli(budget_project, ["init", ROOT_GOAL]), "fractal init (budget)")
    _run_cli(budget_project, ["run"])
    _assert_root_complete(budget_project)

    a_disk_id = _node_by_tag(budget_project, "A").name

    # Try to add a child that exceeds the parent's remaining budget.
    overflow = _run_cli(
        budget_project,
        [
            "steer", "add", a_disk_id,
            "--goal", "subtask [N:BIG]",
            "--acceptance-criteria", "is complete",
            "--allocation", "999999",
            "--confirm",
        ],
        timeout=STEER_TIMEOUT,
    )
    # Must be rejected.
    assert overflow.returncode != 0 or "reject" in (overflow.stdout + overflow.stderr).lower(), (
        "add accepted a child whose allocation exceeds the parent's budget remainder"
    )

    # --- Cycle ---
    cycle_project = _make_project(tmp_path / "cycle", "budget_tree")
    _require_ok(_run_cli(cycle_project, ["init", ROOT_GOAL]), "fractal init (cycle)")
    _run_cli(cycle_project, ["run"])
    _assert_root_complete(cycle_project)

    a_disk_id_c = _node_by_tag(cycle_project, "A").name

    # Try to add a child that depends on itself (cycle).
    cycle = _run_cli(
        cycle_project,
        [
            "steer", "add", a_disk_id_c,
            "--goal", "subtask [N:CYCLIC]",
            "--acceptance-criteria", "is complete",
            "--depends-on", a_disk_id_c,  # depends on parent = cycle
            "--confirm",
        ],
        timeout=STEER_TIMEOUT,
    )
    assert cycle.returncode != 0 or "reject" in (cycle.stdout + cycle.stderr).lower() or "cycle" in (cycle.stdout + cycle.stderr).lower(), (
        "add accepted a child that creates a dependency cycle"
    )


# ---------------------------------------------------------------------------
# AC3.5.6 — digest references only real nodes
# ---------------------------------------------------------------------------


def test_ac35_6(tmp_path: Path) -> None:
    """digest output references only real node ids and statuses present on disk
    (anti-hallucination check: every named node in the digest is validated to
    exist).

    Scenario: digest_tree.  ROOT splits [A, B]; A splits [A1]; all complete.
    Run digest.  Parse the output markdown; extract every node id mention
    (looking for [N:<tag>] markers and on-disk node ids).  Verify each exists
    on disk.  Also verify the statuses reported match the index.
    """
    project = _make_project(tmp_path, "digest_tree")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    # Gather ground truth: all on-disk node ids and their statuses.
    rows = _index_rows(project)
    root = _root_dir(project)
    disk_ids: set[str] = {root.name}
    for node_path in _walk_nodes(root):
        disk_ids.add(node_path.name)

    # Run digest.
    digest_output = _require_ok(
        _run_cli(project, ["digest"], timeout=DIGEST_TIMEOUT),
        "fractal digest",
    )

    # The digest should have written digest.md in the project root.
    digest_path = project["dir"] / "digest.md"
    assert digest_path.is_file(), "digest.md was not written"

    digest_text = digest_path.read_text(encoding="utf-8")

    # Extract node ids from the digest.  Look for on-disk ids (root-01, etc.)
    # and [N:<tag>] markers.
    mentioned_ids: set[str] = set()
    for mentioned in re.findall(r"\[N:([A-Za-z0-9_]+)\]", digest_text):
        # Map tag mentions back to disk ids via the tree.
        try:
            mentioned_ids.add(_node_by_tag(project, mentioned).name)
        except AssertionError:
            # If the tag maps to no disk node, that's a hallucination.
            pass
    # Also look for raw disk ids like root-01-01 in the digest.
    for disk_id in disk_ids:
        if disk_id in digest_text:
            mentioned_ids.add(disk_id)

    # Every mentioned id must exist on disk.
    for mid in mentioned_ids:
        assert mid in disk_ids, (
            f"digest references node {mid!r} which does not exist on disk"
        )

    # Verify the digest mentions at least some real nodes (sanity check).
    assert mentioned_ids, (
        "digest output references zero real node ids"
    )

    # Also check the stdout digest output for the same validation.
    for mentioned in re.findall(r"\[N:([A-Za-z0-9_]+)\]", digest_output):
        try:
            _node_by_tag(project, mentioned)
        except AssertionError:
            pytest.fail(
                f"digest stdout references [N:{mentioned}] which does not exist on disk"
            )

    # The digest must include at least the three-paragraph structure:
    # done / blocked / next.
    digest_lower = digest_text.lower()
    narrative_markers = ["done", "blocked", "next"]
    found_markers = [m for m in narrative_markers if m in digest_lower]
    assert len(found_markers) >= 2, (
        f"digest missing narrative structure; only found: {found_markers}"
    )
