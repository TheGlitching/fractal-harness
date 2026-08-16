"""Acceptance tests for contracts/phase-4.md (AC4.1 – AC4.3).

These tests are the acceptance criteria for Phase 4: the global cross-cutting
memory store.  AC4.1 drives the harness through its CLI with a deterministic
fake Anthropic model injected via ``sitecustomize``, so ``src/`` is free to be
organised however the contract says as long as the observable behaviour holds.
AC4.2 and AC4.3 drive the Store API in-process.

Expected to be RED until the Phase 4 work lands.  Must not be weakened.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

from fractal.store import Store

REPO_ROOT = Path(__file__).resolve().parents[1]
SRC_DIR = REPO_ROOT / "src"

ROOT_GOAL = "build the library [N:ROOT]"

CLI_TIMEOUT = 180.0

_CONVENTION = "SNAKE_CASE_CONVENTION"
_ENTRY_TEXT_1 = "TABS_4_SPACES"
_ENTRY_TEXT_2 = "TABS_2_SPACES"


# --------------------------------------------------------------------------
# The deterministic fake Anthropic API for Phase 4, injected through
# sitecustomize.  Supports note_global alongside split / complete / escalate
# / escalate_resolve.
# --------------------------------------------------------------------------

FAKE_SITECUSTOMIZE = r'''
"""Deterministic stand-in for the anthropic SDK for Phase 4 tests.

Mode global:  ROOT splits [A, B]; A writes a global convention entry via
note_global then completes; B's response depends on whether the global
convention appears in its prompt (read from an env-var flag).

Verification is a separate critic call without the node tools; it always
PASSes.
"""

import json
import os
import re
import sys
import threading
import types

_LOG = os.environ.get("FRACTAL_FAKE_LOG")
_MODE = os.environ.get("FRACTAL_FAKE_MODE", "global")
_TOKENS = int(os.environ.get("FRACTAL_FAKE_TOKENS", "200"))
_CONVENTION = os.environ.get("FRACTAL_FAKE_GLOBAL_CONVENTION", "SNAKE_CASE_CONVENTION")
_LOCK = threading.Lock()
_CALLCNT = 0
_TRIES = {}

_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)")

_Block = types.SimpleNamespace


def _subtask(tag):
    return {
        "id": tag,
        "goal": "subtask [N:%s]" % tag,
        "acceptance_criteria": ["is complete"],
        "interfaces": [],
        "constraints": [],
        "depends_on": [],
    }


def _split(ids):
    return {"verb": "split", "subtasks": [_subtask(i) for i in ids]}


def _complete(text):
    return {
        "verb": "complete",
        "deliverable": text,
        "summary": "done",
        "artifacts": [{"path": "out.txt", "content": text}],
    }


def _note_global(entry_type, content, supersedes=None):
    result = {"verb": "note_global", "type": entry_type, "content": content}
    if supersedes:
        result["supersedes"] = supersedes
    return result


_SCENARIOS = {
    "global": {
        "ROOT": [
            _split(["A", "B"]),
            _complete("ROOT_DONE"),
        ],
        "A": [
            _note_global("convention", _CONVENTION),
            _complete("A_DONE"),
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
    with open(_LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(record) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def _respond(kwargs):
    global _CALLCNT
    _CALLCNT += 1
    if _looks_like_node(kwargs):
        tag = _identify(kwargs)
        with _LOCK:
            attempt = _TRIES.get(tag, 0) + 1
            _TRIES[tag] = attempt
        prompt = json.dumps(kwargs, default=str)

        if tag == "B" and _CONVENTION in prompt:
            payload = _complete("B_SEES_CONVENTION")
        elif tag == "B":
            payload = _complete("B_NO_CONVENTION")
        else:
            payload = _scenario(tag, attempt)

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

        saw_convention = _CONVENTION in prompt
        _append(
            {
                "event": "call",
                "kind": "node",
                "order": _CALLCNT,
                "tag": tag,
                "attempt": attempt,
                "verb": payload["verb"],
                "tokens": _TOKENS,
                "saw_convention": saw_convention,
            }
        )
    else:
        content = [
            _Block(type="text", text=json.dumps({"verdict": "PASS", "criteria": []}))
        ]

    return types.SimpleNamespace(
        id="msg_fake_%d" % _CALLCNT,
        type="message",
        role="assistant",
        model=kwargs.get("model", "claude-opus-5"),
        stop_reason="tool_use",
        stop_sequence=None,
        content=content,
        usage=_Block(input_tokens=_TOKENS // 2, output_tokens=_TOKENS // 2),
    )


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
_types_module.Message = types.SimpleNamespace
_types_module.TextBlock = _Block
_types_module.ToolUseBlock = _Block
_types_module.ContentBlock = _Block
_module.types = _types_module
sys.modules["anthropic"] = _module
sys.modules["anthropic.types"] = _types_module
'''


def _make_project(
    base: Path, mode: str, tokens: int = 200
) -> dict[str, Any]:
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
    env["FRACTAL_EXECUTOR"] = "anthropic"
    env["FRACTAL_FAKE_LOG"] = str(log)
    env["FRACTAL_FAKE_MODE"] = mode
    env["FRACTAL_FAKE_TOKENS"] = str(tokens)
    env["FRACTAL_FAKE_GLOBAL_CONVENTION"] = _CONVENTION

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
    assert candidates, "no SQLite index found"
    return candidates[0]


def _index_rows(
    project: dict[str, Any],
) -> dict[str, dict[str, Any]]:
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


def _root_dir(project: dict[str, Any]) -> Path:
    return project["dir"] / "tree" / "root"


def _children_of(node: Path) -> list[Path]:
    children = node / "children"
    if not children.is_dir():
        return []
    return sorted(path for path in children.iterdir() if path.is_dir())


def _node_by_tag(project: dict[str, Any], tag: str) -> Path:
    root = _root_dir(project)
    assert root.is_dir(), f"expected {root} to exist"
    _TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")
    stack: list[Path] = [root]
    while stack:
        node = stack.pop()
        contract = node / "contract.md"
        if contract.is_file():
            if tag in _TAG_RE.findall(contract.read_text(encoding="utf-8")):
                return node
        children = node / "children"
        if children.is_dir():
            stack.extend(
                sorted(path for path in children.iterdir() if path.is_dir())
            )
    raise AssertionError(f"no node found carrying the [N:{tag}] tag")


_COMPLETE_TOKENS = ("complete", "completed", "done", "succeeded")


def _is_complete(status: str) -> bool:
    return status.strip().lower() in _COMPLETE_TOKENS


def _assert_root_complete(project: dict[str, Any]) -> None:
    rows = _index_rows(project)
    assert "root" in rows, "the root node is missing from the index"
    assert _is_complete(rows["root"]["status"]), (
        "root is not complete: " f"{rows['root']['status']!r}"
    )


# --------------------------------------------------------------------------
# AC4.1 — branch A writes a global entry; branch B sees it and changes
#         behaviour.
# --------------------------------------------------------------------------


def test_ac4_1_cross_branch_global_entry(tmp_path: Path) -> None:
    """An entry written by branch A appears in branch B's hydration context
    and measurably changes the fake model's scripted behaviour.  The fake
    model scripts B to respond ``B_SEES_CONVENTION`` only when the global
    convention string appears in its prompt; otherwise
    ``B_NO_CONVENTION``."""
    project = _make_project(tmp_path, "global")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _run_cli(project, ["run"])

    _assert_root_complete(project)

    # ----- B's deliverable changed -------------------------------------------
    b_dir = _node_by_tag(project, "B")
    b_deliverable = (b_dir / "artifacts" / "out.txt").read_text(encoding="utf-8")
    assert "B_SEES_CONVENTION" in b_deliverable, (
        "branch B did NOT see the global convention; its deliverable "
        f"should carry B_SEES_CONVENTION:\n{b_deliverable}"
    )
    assert "B_NO_CONVENTION" not in b_deliverable, (
        "branch B responded with the no-convention payload but it "
        "should have seen the global entry in its context"
    )

    # ----- A issued a note_global call ---------------------------------------
    calls = _read_fake_log(project["log"])
    note_global_calls = [
        record
        for record in calls
        if record.get("kind") == "node" and record.get("verb") == "note_global"
    ]
    assert note_global_calls, "no node ever issued a note_global verb"
    assert note_global_calls[0]["tag"] == "A", (
        "note_global was issued by the wrong node: "
        f"{note_global_calls[0]['tag']!r}"
    )

    # ----- B's prompt contained the global entry text ------------------------
    b_calls = [
        record for record in calls
        if record.get("kind") == "node" and record.get("tag") == "B"
    ]
    assert b_calls, "node B was never called"
    assert b_calls[0]["saw_convention"], (
        "node B's prompt did NOT contain the global convention text; "
        "the entry was not included in B's hydration context"
    )

    # ----- The global/ directory exists and contains an entry file -----------
    global_dir = project["dir"] / "global"
    assert global_dir.is_dir(), (
        "the global/ directory does not exist at the project root"
    )
    entry_files = list(global_dir.iterdir())
    assert entry_files, "no entry files were written to global/"
    entry_text = entry_files[0].read_text(encoding="utf-8")
    assert _CONVENTION.lower() in entry_text.lower(), (
        f"the global entry does not contain the convention:\n{entry_text}"
    )


# --------------------------------------------------------------------------
# AC4.2 — a superseded entry is never retrieved.
# --------------------------------------------------------------------------


def test_ac4_2_superseded_entry_not_retrieved(tmp_path: Path) -> None:
    """Write E1, then E2 with ``supersedes=<E1.id>``, then retrieve
    relevant entries; only E2 is returned."""
    store = Store(tmp_path / "project")
    store.init("test goal")

    e1_id = store.note_global("convention", _ENTRY_TEXT_1)
    assert e1_id, "note_global must return the new entry id"

    e2_id = store.note_global("convention", _ENTRY_TEXT_2, supersedes=e1_id)
    assert e2_id, "note_global must return the new entry id"
    assert e2_id != e1_id, "supersede entry must have a distinct id"

    entries = store.retrieve_global("tab indentation", k=10)
    contents = [e.content for e in entries]

    assert any(_ENTRY_TEXT_2 in c for c in contents), (
        f"the superseding entry ({_ENTRY_TEXT_2!r}) was not retrieved"
    )
    assert not any(_ENTRY_TEXT_1 in c for c in contents), (
        f"the superseded entry ({_ENTRY_TEXT_1!r}) was retrieved; "
        "superseded entries must never appear in retrieval results"
    )


# --------------------------------------------------------------------------
# AC4.3 — the store survives restart.
# --------------------------------------------------------------------------


def test_ac4_3_store_survives_restart(tmp_path: Path) -> None:
    """Write a global entry, close the Store, open a fresh one on the same
    directory, and assert the entry is still there."""
    project_dir = tmp_path / "project"
    content = "survive_restart_entry"

    # Create and write.
    with Store(project_dir) as store:
        store.init("test goal")
        entry_id = store.note_global("lesson", content)
        assert entry_id, "note_global must return a non-empty id"

    # Close the Store (the ``with`` block calls __exit__), then reopen.
    with Store(project_dir) as store2:
        entries = store2.retrieve_global("survive", k=5)
        contents = [e.content for e in entries]
        assert any(content in c for c in contents), (
            f"the entry ({content!r}) was not found after reopening "
            "the store; persistence failed"
        )

    # Also verify the file is on disk independently of the Store.
    global_dir = project_dir / "global"
    assert global_dir.is_dir(), "global/ directory did not survive restart"
    files = list(global_dir.iterdir())
    assert files, "no entry files in global/ after restart"
