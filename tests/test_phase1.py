"""Acceptance tests for contracts/phase-1.md (AC1.1 - AC1.5).

These tests are the acceptance criteria for Phase 1: contracts as the layer
boundary, dependency-aware splits, and per-rollup verification. Like
test_phase0.py they drive the harness through its CLI only, injecting a
deterministic fake Anthropic model via ``sitecustomize``, so `src/` is free to
be organised however the contract says as long as the observable behaviour
holds.

Interface assumptions pinned by these tests
-------------------------------------------
1. The CLI is runnable as ``python -m fractal.cli`` with ``src/`` on
   ``PYTHONPATH`` and accepts ``init <goal>``, ``run`` and ``status``, all
   operating on a project rooted at the working directory.
2. A node execution reaches ``client.messages.create(**kwargs)`` with
   ``tools`` carrying the ``split`` / ``complete`` tools (as in phase 0).  A
   node-call is therefore recognised by the presence of those tool names.
3. Verification is a *separate model call* that does NOT carry the ``split`` /
   ``complete`` tools; it is the only other call the harness makes.  The fake
   distinguishes a critic from a node this way.  The critic is asked the
   per-criterion PASS/FAIL question and returns a result the harness can parse
   (the fake emits both a JSON text block and a plain-text verdict).
4. A split payload carries child contracts, each with an ``id`` and an
   optional ``depends_on`` list of sibling ``id``s forming a DAG.  This is the
   only extension to the phase-0 split payload the harness needs to accept.
5. Each node's identity — for the fake and, on disk, for the tests — is
   carried as a ``[N:<id>]`` tag in its goal.  The root's id is ``ROOT``.
6. A node that has already answered once returns ``complete`` on any later
   node-call, so an implementation that re-runs a parent to aggregate its
   children, re-runs a node for a revise round, or re-runs a stale dependent
   still terminates.

The fake appends every model call to ``FRACTAL_FAKE_LOG`` (JSONL, append-only)
so the tests can assert ordering ("A to acceptance before B starts"), context
injection ("B's context carries A's summary") and revise counts ("exactly one
revise round fixed it").

These tests are the acceptance criteria for work that is not yet implemented:
Phase 1 extends the phase-0 harness, so they are expected to be red until that
work lands.  They must be honoured, not weakened.
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

Only node executions carry the split/complete tools; verification is a
separate critic call without those tools.  A node is identified by the
greatest [N:<id>] tag in its request (the root's contract is the only other
tag a request can carry, and ROOT ranks below every real node id, so a real
node always outranks it).

Scenarios (FRACTAL_FAKE_MODE):
  saturate - root splits into A then B(depends_on=A); both complete.
             Used by AC1.1 and AC1.5.
  cycle    - root proposes A(depends_on=B), B(depends_on=A).  The cycle is
             rejected at split time; re-asked, the root completes.
  revise   - root splits into C only.  C's first completion is deliberately
             bad (no "CORRECT", hence BAD_OUTPUT); the critic FAILs it and C's
             second completion is good.  The critic FAILs exactly when the
             judged text lacks "CORRECT".
  amend    - root splits into A then B(depends_on=A); both complete.  A node
             that has completed re-completes on any later call, so when the
             test amends A and re-runs, a stale B is free to be re-executed.
"""

import json
import os
import re
import sys
import threading
import types

_LOG = os.environ.get("FRACTAL_FAKE_LOG")
_MODE = os.environ.get("FRACTAL_FAKE_MODE", "saturate")

if _LOG:
    _LOCK = threading.Lock()
    _CALLCNT = 0
    _TRIES = {}  # tag -> number of node calls so far
    _TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")
    _RANK = {"ROOT": 0, "A": 1, "B": 2, "C": 3}

    def _identify(kwargs):
        tags = _TAG_RE.findall(json.dumps(kwargs, default=str))
        if not tags:
            return "ROOT"
        return max(tags, key=lambda t: _RANK.get(t, 0))

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

    def _subtask(tag, goal, criteria, depends_on):
        return {
            "id": tag,
            "goal": goal,
            "acceptance_criteria": criteria,
            "interfaces": [],
            "constraints": [],
            "depends_on": depends_on,
        }

    def _root_split():
        if _MODE == "cycle":
            a = _subtask("A", "package one [N:A]", ["is complete"], ["B"])
            b = _subtask("B", "package two [N:B]", ["is complete"], ["A"])
            return {"verb": "split", "subtasks": [a, b]}
        if _MODE == "revise":
            c = _subtask("C", "module C [N:C]", ["is complete"], [])
            return {"verb": "split", "subtasks": [c]}
        a = _subtask("A", "module A [N:A]", ["provides a function"], [])
        b = _subtask("B", "module B [N:B]", ["uses module A"], ["A"])
        return {"verb": "split", "subtasks": [a, b]}

    def _complete(deliverable, summary):
        return {
            "verb": "complete",
            "deliverable": deliverable,
            "summary": summary,
            "artifacts": [{"path": "out.txt", "content": deliverable}],
        }

    def _node_payload(tag, attempt):
        _TRIES[tag] = attempt
        if tag == "ROOT":
            if attempt == 1:
                return _root_split()
            return _complete("combined", "root done [N:ROOT]")
        if tag == "A":
            return _complete("MODULE_A_DELIVERABLE CORRECT", "SUMMARY_FOR_A done")
        if tag == "B":
            return _complete("MODULE_B_DELIVERABLE CORRECT", "module B done")
        if tag == "C":
            if attempt == 1:
                return _complete("MODULE_C_BAD_OUTPUT", "C first pass")
            return _complete("MODULE_C_CORRECT", "C fixed")
        return _complete("unknown tag", "unknown tag")

    def _criteria_for(text):
        criteria = [{"name": "a deliverable exists", "pass": bool(text.strip())}]
        tags = _TAG_RE.findall(text)
        judging = max(tags, key=lambda t: _RANK.get(t, 0)) if tags else "ROOT"
        if _MODE == "revise" and judging == "C":
            criteria.append({"name": "must contain CORRECT", "pass": "CORRECT" in text})
        return criteria

    def _critic(kwargs, order):
        text = json.dumps(kwargs, default=str)
        criteria = _criteria_for(text)
        ok = all(item["pass"] for item in criteria)
        _append(
            {
                "event": "call",
                "kind": "critic",
                "order": order,
                "ok": ok,
                "criteria": criteria,
            }
        )
        return json.dumps({"verdict": "PASS" if ok else "FAIL", "criteria": criteria})

    def _respond(kwargs):
        global _CALLCNT
        _CALLCNT += 1
        with _LOCK:
            if _looks_like_node(kwargs):
                tag = _identify(kwargs)
                attempt = _TRIES.get(tag, 0) + 1
                payload = _node_payload(tag, attempt)
                flags = {}
                if tag == "B":
                    flags["context_has_A_summary"] = (
                        "SUMMARY_FOR_A" in json.dumps(kwargs, default=str)
                    )
                _append(
                    {
                        "event": "call",
                        "kind": "node",
                        "order": _CALLCNT,
                        "tag": tag,
                        "attempt": attempt,
                        "verb": payload["verb"],
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
                content = [_Block(type="text", text=_critic(kwargs, _CALLCNT))]
        return _Message(
            id="msg_fake_%d" % _CALLCNT,
            type="message",
            role="assistant",
            model=kwargs.get("model", "claude-opus-5"),
            stop_reason="tool_use",
            stop_sequence=None,
            content=content,
            usage=_Block(input_tokens=100, output_tokens=100),
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


def _make_project(tmp_path: Path, mode: str) -> dict[str, Any]:
    project = tmp_path / "project"
    project.mkdir()

    inject = tmp_path / "inject"
    inject.mkdir()
    (inject / "sitecustomize.py").write_text(FAKE_SITECUSTOMIZE, encoding="utf-8")

    log = tmp_path / "fake-calls.jsonl"

    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join(
        [str(inject), str(SRC_DIR), env.get("PYTHONPATH", "")]
    ).rstrip(os.pathsep)
    env["PYTHONUNBUFFERED"] = "1"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["ANTHROPIC_API_KEY"] = "fake-key-for-tests"
    env["FRACTAL_FAKE_LOG"] = str(log)
    env["FRACTAL_FAKE_MODE"] = mode

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


_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")


def _node_by_tag(project: dict[str, Any], tag: str) -> Path:
    """The on-disk node whose contract.md goal carries the ``[N:<tag>]`` tag."""
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


def _artifacts_content(project: dict[str, Any], tag: str) -> list[str]:
    artifacts_root = _node_by_tag(project, tag) / "artifacts"
    return [
        path.read_text(encoding="utf-8")
        for path in artifacts_root.rglob("*")
        if path.is_file()
    ]


# --------------------------------------------------------------------------
# SQLite index helpers
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


def _is_complete(status: str) -> bool:
    return status.strip().lower() in _COMPLETE_TOKENS


# --------------------------------------------------------------------------
# Fake-log query helpers
# --------------------------------------------------------------------------


def _node_call(records: list[dict[str, Any]], tag: str, attempt: int) -> dict[str, Any]:
    for record in records:
        if record.get("kind") == "node" and record.get("tag") == tag:
            if record.get("attempt") == attempt:
                return record
    raise AssertionError(
        f"no node call for tag {tag!r} with attempt {attempt} in {records}"
    )


def _node_calls(records: list[dict[str, Any]], tag: str) -> list[dict[str, Any]]:
    return [
        record
        for record in records
        if record.get("kind") == "node" and record.get("tag") == tag
    ]


def _any_critic(records: list[dict[str, Any]], **flags: object) -> bool:
    return any(
        record.get("kind") == "critic"
        and all(record.get(k) == v for k, v in flags.items())
        for record in records
    )


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


@pytest.fixture()
def deps_project(tmp_path: Path) -> dict[str, Any]:
    return _make_project(tmp_path, "saturate")


@pytest.fixture()
def revise_project(tmp_path: Path) -> dict[str, Any]:
    return _make_project(tmp_path, "revise")


# --------------------------------------------------------------------------
# AC1.1
# --------------------------------------------------------------------------


def test_ac1_1(deps_project: dict[str, Any]) -> None:
    """A split producing children A, B(depends_on=A) runs A to acceptance
    before B starts, and B's context carries A's summary."""
    _require_ok(_run_cli(deps_project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(deps_project, ["run"]), "fractal run")

    records = _read_fake_log(deps_project["log"])
    a_done = _node_call(records, "A", 1)
    b_first = _node_call(records, "B", 1)
    assert a_done["verb"] == "complete", "A never reached acceptance"
    assert a_done["order"] < b_first["order"], (
        "B started before A reached acceptance; the depends_on edge was ignored\n"
        f"A: {a_done}, B: {b_first}"
    )
    assert b_first["context_has_A_summary"], (
        "B's context does not carry A's distilled summary at hydration"
    )

    index = _index_rows(deps_project)
    assert _is_complete(index["root"]["status"]), "the root is not accepted after the run"
    assert "depends_on" in _contract_text(deps_project, "B"), (
        "B's contract does not record its dependency on A"
    )
    for tag in ("A", "B"):
        node_id = _node_by_tag(deps_project, tag).name
        assert _is_complete(index[node_id]["status"]), f"{tag} ({node_id}) was not accepted"


# --------------------------------------------------------------------------
# AC1.2
# --------------------------------------------------------------------------


def test_ac1_2(tmp_path: Path) -> None:
    """A dependency cycle in a proposed split is rejected at split time: no
    children are created and the proposing node is forced to finish its own
    work rather than deadlock."""
    project = _make_project(tmp_path, "cycle")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(project, ["run"]), "fractal run")

    root = _root_dir(project)
    assert not _children_of(root), (
        "a cyclic split created children instead of being rejected: "
        f"{[child.name for child in _children_of(root)]}"
    )
    assert _is_complete(_index_rows(project)["root"]["status"]), (
        "the root did not reach a terminal accepted state after the cycle was rejected"
    )
    assert re.search(r"refus", _decisions_text(project, "ROOT"), re.IGNORECASE), (
        "the root's decisions.md does not record that the cyclic split was refused"
    )


# --------------------------------------------------------------------------
# AC1.3
# --------------------------------------------------------------------------


def test_ac1_3(revise_project: dict[str, Any]) -> None:
    """A planted bad deliverable is caught by verify at the parent boundary —
    not at the root — and one revise round fixes it."""
    _require_ok(_run_cli(revise_project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(revise_project, ["run"]), "fractal run")

    records = _read_fake_log(revise_project["log"])
    c_calls = _node_calls(records, "C")
    assert len(c_calls) == 2, (
        "expected exactly one revise round (two C node calls), "
        f"got {len(c_calls)}: {c_calls}"
    )
    assert c_calls[0]["verb"] == "complete" and c_calls[1]["verb"] == "complete"

    assert _any_critic(records, ok=False), "verify never FAILed the planted bad deliverable"
    failing_order = next(
        record["order"]
        for record in records
        if record.get("kind") == "critic" and not record["ok"]
    )
    assert c_calls[0]["order"] < failing_order < c_calls[1]["order"], (
        "the verify FAIL did not land between the bad submission and the revision"
    )

    assert _is_complete(_index_rows(revise_project)["root"]["status"]), (
        "the run did not complete acceptance after the revise"
    )
    for node in _walk_nodes(_root_dir(revise_project)):
        for art in (node / "artifacts").rglob("*"):
            if art.is_file():
                assert "BAD_OUTPUT" not in art.read_text(encoding="utf-8"), (
                    f"the rejected bad deliverable shipped in {art}"
                )
    corrected = _artifacts_content(revise_project, "C")
    assert any("CORRECT" in text for text in corrected), (
        "the revised (accepted) deliverable is not the corrected version"
    )


# --------------------------------------------------------------------------
# AC1.4
# --------------------------------------------------------------------------


def test_ac1_4(tmp_path: Path) -> None:
    """Amending A's deliverable after acceptance flags B stale: rerunning
    re-executes and re-verifies B."""
    project = _make_project(tmp_path, "amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(project, ["run"]), "fractal run (first)")

    a_node = _node_by_tag(project, "A")
    for path in (a_node / "artifacts").rglob("*"):
        if path.is_file():
            path.write_text(
                path.read_text(encoding="utf-8") + "\nAMENDED_AFTER_ACCEPT\n",
                encoding="utf-8",
            )

    _require_ok(_run_cli(project, ["run"]), "fractal run (after amend)")

    records = _read_fake_log(project["log"])
    assert len(_node_calls(records, "B")) >= 2, (
        "B was not re-executed after its dependency A was amended"
    )
    index = _index_rows(project)
    assert _is_complete(index["root"]["status"]), "the root is not accepted after amend"
    assert _is_complete(index[_node_by_tag(project, "B").name]["status"]), (
        "B did not re-verify to acceptance after the amend"
    )


# --------------------------------------------------------------------------
# AC1.5
# --------------------------------------------------------------------------


def test_ac1_5(deps_project: dict[str, Any]) -> None:
    """Every accepted node's decisions.md carries the verification verdict with
    per-criterion results."""
    _require_ok(_run_cli(deps_project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(deps_project, ["run"]), "fractal run")

    index = _index_rows(deps_project)
    for node in _walk_nodes(_root_dir(deps_project)):
        if not _is_complete(index[node.name]["status"]):
            continue
        decisions = (node / "decisions.md").read_text(encoding="utf-8")
        assert re.search(r"verdict", decisions, re.IGNORECASE), (
            f"accepted node {node.name} is missing the verification verdict "
            f"in decisions.md:\n{decisions}"
        )
        assert re.search(r"criter", decisions, re.IGNORECASE) and re.search(
            r"PASS", decisions
        ), (
            f"accepted node {node.name} is missing per-criterion results "
            f"in decisions.md:\n{decisions}"
        )
