"""Acceptance tests for contracts/phase-2.md (AC2.1 - AC2.4).

These tests are the acceptance criteria for Phase 2: replacing the hardcoded
depth cap with budget-bounded recursion.  They drive the harness through its
CLI (like test_phase1.py) with a deterministic fake Anthropic model injected
via ``sitecustomize``, so `src/` is free to be organised however the contract
says as long as the observable behaviour holds.  They are expected to be red
until the Phase 2 work lands, and must be honoured, not weakened.

Interface assumptions pinned by these tests (Phase 2 additions)
--------------------------------------------------------------
1. Budgets are configured by env var at ``fractal init``:
   ``FRACTAL_BUDGET`` = the root node's token allowance.  A per-subtree
   budget ledger lives in the SQLite index; the harness debits it.
2. Every model call debits its actual usage (tokens) from the calling node's
   budget.  Every *accepted* split debits an additional split-fee, whose value
   the harness reads from ``FRACTAL_SPLIT_FEE`` (default 200).  A rejected
   split debits nothing beyond the calls it caused.
3. A node's prompt shows its remaining token allowance so the agent can make
   informed allocation proposals (a line carrying "remaining" or "budget"
   followed by a number).  The fake reads it back.
4. A proposed split carries per-child ``allocation`` values (tokens).  The
   harness validates that the split-fee plus the sum of allocations does not
   exceed the node's remaining budget; an over-allocation is rejected at split
   time with a structured error, and the proposing node is re-asked with that
   error in its context.
5. On budget exhaustion a node must either complete degraded (explicitly
   marked) or fail — never silently continue.  The fake never supplies a
   degraded deliverable, so exhaustion surfaces as a failed node.
6. The ledger records each node's own total debits (its call usage plus its
   split-fee if it split) so that summing every row equals the total debits.

The fake appends every model call to ``FRACTAL_FAKE_LOG`` (JSONL) carrying
``tokens`` and ``verb``, so the tests can re-derive the "sum of recorded call
costs" from the log and compare it against the on-disk ledger exactly.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterable

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SRC_DIR = REPO_ROOT / "src"

ROOT_GOAL = "integrate the modules into one library [N:ROOT]"

CLI_TIMEOUT = 180.0

_COMPLETE_TOKENS = ("complete", "completed", "done", "succeeded")


# --------------------------------------------------------------------------
# The deterministic fake Anthropic API, injected through sitecustomize.
# --------------------------------------------------------------------------

FAKE_SITECUSTOMIZE = r'''
"""Deterministic stand-in for the anthropic SDK, installed at interpreter
start so every harness subprocess sees it.

Node executions carry the split/complete tools; a node is identified by the
greatest [N:<id>] tag in its request.  Verification is a separate critic call
without those tools.

Env:
  FRACTAL_FAKE_MODE     chain | overalloc
  FRACTAL_FAKE_TOKENS   tokens reported as usage per model call (input+output)
  FRACTAL_FAKE_LOG      append-only JSONL of every call, for assertions

Modes:
  chain     - every node splits into exactly ONE child, forever; a node that
              has already answered returns split again, so the fake genuinely
              "always wants splits" (AC2.1) and depth is bounded only by the
              budget.  The child's proposed allocation is the node's remaining
              budget minus the split-fee (read from FRACTAL_SPLIT_FEE), so it
              is accepted whenever the node can afford the split and rejected
              (forced to complete/fail) when it cannot.
  overalloc - the root proposes two children A and B each asking for an absurd
              allocation, so their sum exceeds the remaining budget; the split
              must be rejected.  Re-asked, the root completes with a real
              deliverable.  The root's second call logs 'saw_rejection' when
              its prompt carries the structured rejection error.
"""

import json
import os
import re
import sys
import threading
import types

_LOG = os.environ.get("FRACTAL_FAKE_LOG")
_MODE = os.environ.get("FRACTAL_FAKE_MODE", "chain")
_TOKENS = int(os.environ.get("FRACTAL_FAKE_TOKENS", "200"))
_SPLIT_FEE = int(os.environ.get("FRACTAL_SPLIT_FEE", "200"))
_LOCK = threading.Lock()
_CALLCNT = 0
_COUNT = 0
_TRIES = {}
_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")
_REM_RE = re.compile(r"(?:remaining|budget)[^\d]*?(\d+)", re.IGNORECASE)
_RANK = {"ROOT": 0}

if _LOG:
    def _identify(kwargs):
        tags = _TAG_RE.findall(json.dumps(kwargs, default=str))
        if not tags:
            return "ROOT"
        return max(tags, key=lambda t: _RANK.get(t, 1))

    def _looks_like_node(kwargs):
        names = {
            str(t.get("name")).strip().lower()
            for t in kwargs.get("tools") or []
            if isinstance(t, dict)
        }
        return bool({"split", "complete"} & names)

    def _append(record):
        with open(_LOG, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")
            handle.flush()
            os.fsync(handle.fileno())

    def _subtask(tag, allocation):
        child = {
            "id": tag,
            "goal": "chain [N:%s]" % tag,
            "acceptance_criteria": ["is complete"],
            "interfaces": [],
            "constraints": [],
            "depends_on": [],
        }
        if allocation is not None:
            child["allocation"] = allocation
        return child

    def _remaining(prompt_text):
        match = _REM_RE.search(prompt_text)
        return int(match.group(1)) if match else 0

    def _node_payload(tag, attempt, prompt_text):
        if _MODE == "overalloc" and tag == "ROOT":
            if attempt == 1:
                return {
                    "verb": "split",
                    "subtasks": [
                        _subtask("A", allocation=10**15),
                        _subtask("B", allocation=10**15),
                    ],
                }
            return {
                "verb": "complete",
                "deliverable": "ROOT_OK_CORRECT",
                "summary": "root done [N:ROOT]",
                "artifacts": [{"path": "out.txt", "content": "ROOT_OK_CORRECT"}],
            }
        global _COUNT
        _COUNT += 1
        child = "c%d" % _COUNT
        allocation = max(0, _remaining(prompt_text) - _SPLIT_FEE)
        return {"verb": "split", "subtasks": [_subtask(child, allocation)]}

    def _respond(kwargs):
        global _CALLCNT
        _CALLCNT += 1
        with _LOCK:
            if _looks_like_node(kwargs):
                tag = _identify(kwargs)
                attempt = _TRIES.get(tag, 0) + 1
                prompt_text = json.dumps(kwargs, default=str)
                payload = _node_payload(tag, attempt, prompt_text)
                flags = {}
                if _MODE == "overalloc" and tag == "ROOT" and attempt >= 2:
                    lowered = prompt_text.lower()
                    flags["saw_rejection"] = any(
                        word in lowered
                        for word in ("budget", "allocat", "exceed", "refus")
                    )
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
                    _Block(type="text", text=json.dumps(payload, indent=2)),
                    _Block(
                        type="tool_use",
                        id="toolu_fake_%d" % _CALLCNT,
                        name=payload["verb"],
                        input=tool_input,
                    ),
                ]
            else:
                content = [
                    _Block(
                        type="text",
                        text=json.dumps({"verdict": "PASS", "criteria": []}),
                    )
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


# --------------------------------------------------------------------------
# Process helpers
# --------------------------------------------------------------------------


def _make_project(
    base: Path,
    mode: str,
    *,
    budget: int,
    split_fee: int = 200,
    tokens: int = 200,
) -> dict[str, Any]:
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
    env["FRACTAL_BUDGET"] = str(budget)
    env["FRACTAL_SPLIT_FEE"] = str(split_fee)

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


# --------------------------------------------------------------------------
# On-disk tree helpers
# --------------------------------------------------------------------------


def _root_dir(project: dict[str, Any]) -> Path:
    return project["dir"] / "tree" / "root"


def _children_of(node: Path) -> list[Path]:
    children = node / "children"
    if not children.is_dir():
        return []
    return sorted(path for path in children.iterdir() if path.is_dir())


def _walk_nodes(node: Path) -> Iterable[Path]:
    yield node
    for child in _children_of(node):
        yield from _walk_nodes(child)


def _max_depth(project: dict[str, Any]) -> int:
    """Deepest node path length: root alone is depth 1, root+child is 2, ..."""
    root = _root_dir(project)
    return max(
        (len(node.relative_to(root).parts) + 1 for node in _walk_nodes(root)),
        default=0,
    )


def _split_node_count(project: dict[str, Any]) -> int:
    """Number of nodes that actually have children = accepted splits."""
    return sum(
        1 for node in _walk_nodes(_root_dir(project)) if _children_of(node)
    )


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


def _decisions_text(project: dict[str, Any], tag: str) -> str:
    return (_node_by_tag(project, tag) / "decisions.md").read_text(encoding="utf-8")


# --------------------------------------------------------------------------
# SQLite index / ledger helpers
# --------------------------------------------------------------------------


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


def _ledger_total(project: dict[str, Any]) -> int | None:
    """Sum of every ledger row's own-debits column (total debits)."""
    db = _find_index_db(project)
    uri = f"file:{db}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=1.0)
    connection.row_factory = sqlite3.Row
    try:
        total: int | None = None
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
            id_column = lowered.get("node_id") or lowered.get("id")
            debit_column = next(
                (
                    column
                    for key, column in lowered.items()
                    if any(word in key for word in ("token", "dollar", "debit", "burn", "cost"))
                ),
                None,
            )
            if not (id_column and debit_column):
                continue
            for (value,) in connection.execute(
                f'SELECT "{debit_column}" FROM "{table}"'
            ):
                if isinstance(value, (int, float)):
                    total = (total or 0) + int(value)
        return total
    finally:
        connection.close()


def _is_complete(status: str) -> bool:
    return status.strip().lower() in _COMPLETE_TOKENS


# --------------------------------------------------------------------------
# AC2.1
# --------------------------------------------------------------------------


def test_ac2_1(tmp_path: Path) -> None:
    """The same task at budgets X and 4X produces strictly deeper or equal max
    depth at 4X, with a fake model that always wants splits."""
    tokens = 200
    fee = 200
    x = 2 * tokens + 1 * fee  # a budget that affords depth 2

    small = _make_project(tmp_path, "chain", budget=x, split_fee=fee, tokens=tokens)
    _require_ok(_run_cli(small, ["init", ROOT_GOAL]), "fractal init (budget X)")
    _run_cli(small, ["run"])
    depth_x = _max_depth(small)

    big = _make_project(tmp_path / "4x", "chain", budget=4 * x, split_fee=fee, tokens=tokens)
    _require_ok(_run_cli(big, ["init", ROOT_GOAL]), "fractal init (budget 4X)")
    _run_cli(big, ["run"])
    depth_4x = _max_depth(big)

    assert depth_x >= 1, "even the X budget must afford the root node"
    assert depth_4x >= depth_x, (
        "budget 4X produced a shallower tree than budget X: "
        f"depth(X)={depth_x}, depth(4X)={depth_4x}"
    )


# --------------------------------------------------------------------------
# AC2.2
# --------------------------------------------------------------------------


def test_ac2_2(tmp_path: Path) -> None:
    """A split proposing allocations that exceed the remaining budget is
    rejected at split time, with a structured error the agent sees."""
    project = _make_project(tmp_path, "overalloc", budget=1000)
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(project, ["run"]), "fractal run")

    root = _root_dir(project)
    assert not _children_of(root), (
        "the over-allocating split was accepted and created children: "
        f"{[child.name for child in _children_of(root)]}"
    )
    assert _is_complete(_index_rows(project)["root"]["status"]), (
        "the root did not reach a terminal accepted state after the rejection"
    )
    assert re.search(r"budget|allocat|exceed", _decisions_text(project, "ROOT"), re.IGNORECASE), (
        "the root's decisions.md does not record the budget-rejection"
    )

    records = _read_fake_log(project["log"])
    second_root = next(
        record
        for record in records
        if record.get("kind") == "node"
        and record.get("tag") == "ROOT"
        and record.get("attempt") == 2
    )
    assert second_root.get("saw_rejection"), (
        "the re-asked root's context did not carry the structured "
        "budget/allocation error the agent must see"
    )


# --------------------------------------------------------------------------
# AC2.3
# --------------------------------------------------------------------------


def test_ac2_3(tmp_path: Path) -> None:
    """Exhaustion produces a degraded-complete or failed node, and the ledger's
    total debits equal the sum of recorded call costs exactly."""
    project = _make_project(
        tmp_path, "chain", budget=2 * 200 + 1 * 200, split_fee=200, tokens=200
    )
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    run_result = _run_cli(project, ["run"])

    status = _require_ok(_run_cli(project, ["status"]), "fractal status")
    assert "failed" in status.lower(), (
        "no node was recorded failed/degraded on budget exhaustion; "
        "the harness must not silently continue\n"
        f"--- status ---\n{status}"
    )
    index = _index_rows(project)
    assert not _is_complete(index["root"]["status"]), (
        "the root was recorded complete despite budget exhaustion"
    )

    records = _read_fake_log(project["log"])
    usage_sum = sum(
        int(record.get("tokens") or 0)
        for record in records
        if record.get("kind") == "node"
    )
    fee = 200
    expected = usage_sum + fee * _split_node_count(project)

    ledger_total = _ledger_total(project)
    assert ledger_total is not None, "no budget ledger was written to the index"
    assert ledger_total == expected, (
        "ledger total debits do not equal the sum of recorded call costs exactly: "
        f"ledger={ledger_total}, recorded={expected}"
    )
    assert run_result.returncode in (0, 1), (
        f"fractal run exited unexpectedly: {run_result.returncode}"
    )


# --------------------------------------------------------------------------
# AC2.4
# --------------------------------------------------------------------------


def test_ac2_4(tmp_path: Path) -> None:
    """The hardcoded depth cap is deleted: the old depth-3 test is reimplemented
    as a budget that affords exactly 3 levels."""
    tokens = 200
    fee = 200
    b3 = 3 * tokens + 2 * fee  # a budget that affords exactly 3 levels

    three = _make_project(tmp_path, "chain", budget=b3, split_fee=fee, tokens=tokens)
    _require_ok(_run_cli(three, ["init", ROOT_GOAL]), "fractal init (3 levels)")
    _run_cli(three, ["run"])
    assert _max_depth(three) == 3, (
        "a budget that affords exactly 3 levels did not stop at depth 3: "
        f"got depth {_max_depth(three)}"
    )

    deeper = _make_project(tmp_path / "4x", "chain", budget=4 * b3, split_fee=fee, tokens=tokens)
    _require_ok(_run_cli(deeper, ["init", ROOT_GOAL]), "fractal init (4x)")
    _run_cli(deeper, ["run"])
    assert _max_depth(deeper) > 3, (
        "a hardcoded depth cap of 3 is still in force: 4x the budget did not "
        f"recursion deeper than 3 (depth={_max_depth(deeper)})"
    )
