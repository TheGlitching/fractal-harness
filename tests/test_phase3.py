"""Acceptance tests for contracts/phase-3.md (AC3.1 - AC3.7).

These tests are the acceptance criteria for Phase 3: upward escalation.  They
drive the harness through its CLI (like test_phase2.py) with a deterministic
fake Anthropic model injected via ``sitecustomize``, so `src/` is free to be
organised however the contract says as long as the observable behaviour holds.
They are expected to be red until the Phase 3 work lands, and must be honoured,
not weakened.

Interface assumptions pinned by these tests (Phase 3 additions)
---------------------------------------------------------------
The phase-0/1/2 protocol knows split and complete.  Phase 3 (SPEC 4.2 / 4.3)
adds upward escalation.  These tests pin the following additions:

1. A node may return a third verb ``escalate`` carrying ``assumption`` (the
   name of the inherited constraint believed false) and ``evidence``.  It is
   the leaf's channel for "my contract rests on a law that is wrong".
2. When a node escalates, the harness suspends that node's branch (its
   ancestors down to the escalation point are marked suspended) and reopens
   the nearest ancestor that owns the named constraint, with the child's
   evidence injected into that ancestor's context.
3. The reopened ancestor must return a fourth verb ``escalate_resolve`` with a
   ``resolution`` field in {"amend", "overrule", "replan"}:
   - amend: carry ``amended_constraint`` (and/or ``amended_interface`` and a
     ``target`` sibling) that the orchestrator writes into the owning
     contract; descendant contracts are updated, stale dependents re-run, and
     the branch resumes and completes under the new terms.
   - overrule: carry ``rationale`` written into the escalating child's
     context; the child resumes and must address it.
   - replan: the ancestor re-plans the branch; pruned children's episodic logs
     are compacted into the ancestor's log/ before the children are deleted.
4. A discovered dependency is escalated as "my contract does not provide X;
   sibling B owns it".  The owning ancestor resolves by either returning
   resolution=depends_on (the orchestrator adds a depends_on edge on the
   escalating node to B; the node pauses until B delivers) or amend (B's
   contract is amended to expose the interface earlier).  No worker-to-worker
   channel exists anywhere in the codebase: every communication in the fake
   log flows through the orchestrator.
5. Constraint ownership is seeded by writing the root's ``## Inherited
   constraints`` section before running (the adversarial scenario's premise is
   that the root mandates a library).  A seeded constraint that the root
   owns is the escalation target; when amended it is replaced in that file.

Escalation tool names are pinned (``escalate`` / ``escalate_resolve``) exactly
as split/complete were pinned before them; the observable effects each test
asserts are the contract's own words.
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

ROOT_GOAL = "build the library [N:ROOT]"

CLI_TIMEOUT = 180.0

_CONSTRAINT_X = "library X is the required dependency"
_CONSTRAINT_Y = "library Y is the required dependency"
_EVIDENCE = "EVIDENCE_X_CONFLICTS_Y"
_RATIONALE = "RATIONALE_X_OK"
_NEED_EVIDENCE = "EVIDENCE_NEED_X"

_COMPLETE_TOKENS = ("complete", "completed", "done", "succeeded")


# --------------------------------------------------------------------------
# The deterministic fake Anthropic API, injected through sitecustomize.
# --------------------------------------------------------------------------

FAKE_SITECUSTOMIZE = r'''
"""Deterministic stand-in for the anthropic SDK for Phase 3 tests.

A node is identified by the greatest [N:<id>] tag in its request (its own
contract appears first).  Each mode scripts how every tag responds, keyed by
attempt count, so the tests can reproduce an escalation cycle exactly:

  amend      - root owns a library constraint and splits [A, B]; A splits
               [A1]; A1 escalates the constraint; root (the owner) is reopened
               and amends it; A1 re-runs; A and B both complete; root rolls up.
  overrule   - A1 escalates; the reopened owner overrules with a rationale.
  replan     - A splits [A2, A1]; both run; A1 escalates; the owner re-plans,
               pruning A's children and re-splitting into [C1].
  dep_edge   - A escalates "sibling B owns X but my contract lacks it"; root
               adds a depends_on edge from A to B.
  dep_amend  - A escalates the same; root amends B's contract to expose the
               interface earlier.
  adversarial - a leaf discovers the seeded library constraint conflicts and
               escalates; the run completes with the root constraint amended.
  control    - escalation disabled: the leaf just completes, and the flawed
               root constraint ships.

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
_MODE = os.environ.get("FRACTAL_FAKE_MODE", "amend")
_TOKENS = int(os.environ.get("FRACTAL_FAKE_TOKENS", "200"))
_LOCK = threading.Lock()
_CALLCNT = 0
_TRIES = {}
_TAG_RE = re.compile(r"\[N:([A-Za-z0-9_]+)\]")

_CONSTRAINT_X = "library X is the required dependency"
_CONSTRAINT_Y = "library Y is the required dependency"
_EVIDENCE = "EVIDENCE_X_CONFLICTS_Y"
_RATIONALE = "RATIONALE_X_OK"
_NEED_EVIDENCE = "EVIDENCE_NEED_X"


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


def _escalate_x():
    return {
        "verb": "escalate",
        "assumption": _CONSTRAINT_X,
        "evidence": _EVIDENCE,
    }


def _escalate_need_x():
    return {
        "verb": "escalate",
        "assumption": "my contract does not provide X; sibling B owns it",
        "evidence": _NEED_EVIDENCE,
    }


def _resolve(**kw):
    return {"verb": "escalate_resolve", **kw}


_SCENARIOS = {
    "amend": {
        "ROOT": [
            _split(["A", "B"]),
            _resolve(resolution="amend", amended_constraint=_CONSTRAINT_Y),
            _complete("ROOT_DONE"),
        ],
        "A": [_split(["A1"]), _complete("A_DONE")],
        "A1": [_escalate_x(), _complete("A1_DONE")],
        "B": [_complete("B_DONE")],
    },
    "overrule": {
        "ROOT": [
            _split(["A"]),
            _resolve(resolution="overrule", rationale=_RATIONALE),
            _complete("ROOT_DONE"),
        ],
        "A": [_split(["A1"]), _complete("A_DONE")],
        "A1": [_escalate_x(), _complete("A1_DONE")],
    },
    "replan": {
        "ROOT": [
            _split(["A"]),
            _resolve(resolution="replan"),
            _complete("ROOT_DONE"),
        ],
        "A": [
            _split(["A2", "A1"]),
            _resolve(resolution="replan"),
            _split(["C1"]),
            _complete("A_DONE"),
        ],
        "A2": [_complete("A2_DONE")],
        "A1": [_escalate_x()],
        "C1": [_complete("C1_DONE")],
    },
    "dep_edge": {
        "ROOT": [
            _split(["A", "B"]),
            _resolve(resolution="depends_on", dependency="B"),
            _complete("ROOT_DONE"),
        ],
        "A": [_escalate_need_x(), _complete("A_DONE")],
        "B": [_complete("B_DONE")],
    },
    "dep_amend": {
        "ROOT": [
            _split(["A", "B"]),
            _resolve(
                resolution="amend",
                target="B",
                amended_interface="expose_X_interface",
            ),
            _complete("ROOT_DONE"),
        ],
        "A": [_escalate_need_x(), _complete("A_DONE")],
        "B": [_complete("B_DONE")],
    },
    "adversarial": {
        "ROOT": [
            _split(["A"]),
            _resolve(resolution="amend", amended_constraint=_CONSTRAINT_Y),
            _complete("ROOT_DONE"),
        ],
        "A": [_split(["A1"]), _complete("A_DONE")],
        "A1": [_escalate_x(), _complete("A1_DONE")],
    },
    "control": {
        "ROOT": [_split(["A"]), _complete("ROOT_DONE")],
        "A": [_split(["A1"]), _complete("A_DONE")],
        "A1": [_complete("A1_FLAWED")],
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
        payload = _scenario(tag, attempt)
        prompt = json.dumps(kwargs, default=str)
        flags = {
            "saw_evidence": (_EVIDENCE in prompt) or (_NEED_EVIDENCE in prompt),
            "saw_rationale": _RATIONALE in prompt,
        }
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


# --------------------------------------------------------------------------
# Process helpers
# --------------------------------------------------------------------------


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
    env["FRACTAL_EXECUTOR"] = "anthropic"
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


def _contract_text(project: dict[str, Any], tag: str) -> str:
    return (_node_by_tag(project, tag) / "contract.md").read_text(encoding="utf-8")


def _seed_root_constraint(project: dict[str, Any], line: str) -> None:
    """Write one bullet into root's ``## Inherited constraints`` section, so
    the root owns a constraint (the adversarial premise)."""
    path = _root_dir(project) / "contract.md"
    text = path.read_text(encoding="utf-8")
    marker = "## Inherited constraints\n\n"
    head, found, tail = text.partition(marker)
    assert found, "root contract has no inherited-constraints section"
    body, sep, rest = tail.partition("\n\n")
    path.write_text(
        head + marker + "- " + line + "\n" + sep + rest, encoding="utf-8"
    )


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


def _node_depends_on(project: dict[str, Any], disk_id: str) -> list[str]:
    db = _find_index_db(project)
    uri = f"file:{db}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=1.0)
    try:
        row = connection.execute(
            "SELECT depends_on FROM nodes WHERE id = ?", (disk_id,)
        ).fetchone()
    finally:
        connection.close()
    if row is None or not row[0]:
        return []
    return json.loads(row[0])


def _is_complete(status: str) -> bool:
    return status.strip().lower() in _COMPLETE_TOKENS


def _assert_root_complete(project: dict[str, Any]) -> None:
    rows = _index_rows(project)
    assert "root" in rows, "the root node is missing from the index"
    assert _is_complete(rows["root"]["status"]), (
        "the run did not complete: root is "
        f"{rows['root']['status']!r}"
    )


# --------------------------------------------------------------------------
# AC3.1
# --------------------------------------------------------------------------


def test_ac3_1(tmp_path: Path) -> None:
    """A leaf escalating a named inherited constraint suspends exactly its
    branch; an unrelated branch keeps running to completion in the same run."""
    project = _make_project(tmp_path, "amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])

    _assert_root_complete(project)

    rows = _index_rows(project)
    b_id = _node_by_tag(project, "B").name
    a_id = _node_by_tag(project, "A").name
    a1_id = _node_by_tag(project, "A1").name

    # The unrelated branch kept running and completed, untouched by the
    # escalation that was raised in A's branch.
    assert _is_complete(rows[b_id]["status"]), (
        "the unrelated branch B did not complete; the run did not keep running"
    )
    assert "escalat" not in _decisions_text(project, "B").lower(), (
        "the escalation leaked into the unrelated branch B"
    )

    # The escalating branch was suspended then resumed to completion.
    assert _is_complete(rows[a_id]["status"]), "escalating branch A never resumed"
    assert _is_complete(rows[a1_id]["status"]), "escalating leaf A1 never resumed"
    assert "escalat" in _decisions_text(project, "A1").lower(), (
        "the leaf A1's escalation is not recorded anywhere in its branch"
    )

    # The run continued past the escalation rather than halting: at least one
    # node call was made after the first escalate.
    calls = _read_fake_log(project["log"])
    escalate_orders = [
        record["order"]
        for record in calls
        if record.get("kind") == "node" and record.get("verb") == "escalate"
    ]
    assert escalate_orders, "no node ever issued an escalate verb"
    first_escalate = min(escalate_orders)
    assert any(
        record.get("kind") == "node" and record["order"] > first_escalate
        for record in calls
    ), "no node call happened after the escalation; the run must not halt"


# --------------------------------------------------------------------------
# AC3.2
# --------------------------------------------------------------------------


def test_ac3_2(tmp_path: Path) -> None:
    """The ancestor owning the constraint is reopened with the child's
    evidence in context and must return amend | overrule | re-plan."""
    project = _make_project(tmp_path, "amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    calls = _read_fake_log(project["log"])
    resolves = [
        record
        for record in calls
        if record.get("kind") == "node" and record.get("verb") == "escalate_resolve"
    ]
    assert resolves, "no reopened ancestor returned an escalation resolution"

    owner = next(
        record for record in resolves if record.get("tag") == "ROOT"
    )
    assert owner.get("saw_evidence"), (
        "the reopened ancestor's context did not carry the child's evidence"
    )
    # The ancestor owning the constraint (ROOT, seeded with library X) is the
    # one reopened, and it must have returned one of the three resolutions.
    assert owner.get("resolution") in {"amend", "overrule", "replan"}, (
        "the reopened ancestor did not return amend | overrule | re-plan: "
        f"{owner.get('resolution')!r}"
    )


# --------------------------------------------------------------------------
# AC3.3
# --------------------------------------------------------------------------


def test_ac3_3(tmp_path: Path) -> None:
    """Amend: descendant contracts are updated, stale dependents re-verified,
    the branch resumes and completes under the new terms."""
    project = _make_project(tmp_path, "amend")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    # The owning ancestor's contract was updated: the amended constraint
    # replaced the old one.
    root_contract = _root_dir(project).joinpath("contract.md").read_text(encoding="utf-8")
    assert _CONSTRAINT_X not in root_contract, (
        "the root constraint was not replaced by the amendment"
    )
    assert _CONSTRAINT_Y in root_contract, (
        "the amended constraint is not present in the owning ancestor's contract"
    )

    # The stale dependent (the escalating leaf) was re-verified: it ran again
    # after the amendment and completed under the new terms.
    calls = _read_fake_log(project["log"])
    a1_calls = [
        record for record in calls if record.get("kind") == "node" and record.get("tag") == "A1"
    ]
    assert len(a1_calls) >= 2, "the escalating leaf was not re-run after the amendment"
    amend_order = next(
        record["order"]
        for record in calls
        if record.get("verb") == "escalate_resolve" and record.get("tag") == "ROOT"
    )
    assert a1_calls[-1]["order"] > amend_order, (
        "the leaf did not resume after the amendment was applied"
    )
    assert a1_calls[-1]["verb"] == "complete", (
        "the re-run leaf did not complete under the amended terms"
    )

    # The branch resumed and completed: A and the root are both terminal.
    rows = _index_rows(project)
    assert _is_complete(rows[_node_by_tag(project, "A").name]["status"]), (
        "the escalating branch did not complete under the new terms"
    )


# --------------------------------------------------------------------------
# AC3.4
# --------------------------------------------------------------------------


def test_ac3_4(tmp_path: Path) -> None:
    """Overrule: the rationale is written into the child's context; the child
    resumes and must address it."""
    project = _make_project(tmp_path, "overrule")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    calls = _read_fake_log(project["log"])
    a1_calls = [
        record for record in calls if record.get("kind") == "node" and record.get("tag") == "A1"
    ]
    assert len(a1_calls) >= 2, "the overruled child did not resume"

    resumed = a1_calls[-1]
    assert resumed.get("saw_rationale"), (
        "the overruling rationale was not written into the child's context"
    )
    assert resumed["verb"] == "complete", (
        "the child did not address the rationale and complete"
    )

    rows = _index_rows(project)
    assert _is_complete(rows[_node_by_tag(project, "A1").name]["status"]), (
        "the overruled child did not reach completion"
    )


# --------------------------------------------------------------------------
# AC3.5
# --------------------------------------------------------------------------


def test_ac3_5(tmp_path: Path) -> None:
    """Re-plan: pruned children's logs are compacted into the ancestor's log/
    before deletion."""
    project = _make_project(tmp_path, "replan")
    _require_ok(_run_cli(project, ["init", ROOT_GOAL]), "fractal init")
    _seed_root_constraint(project, _CONSTRAINT_X)
    _run_cli(project, ["run"])
    _assert_root_complete(project)

    a = _node_by_tag(project, "A")

    # The pruned children (A1, A2) are gone from disk.  The only child A may
    # still own is the re-plan's replacement (C1); never a pruned one.
    pruned = _children_of(a)
    for child in pruned:
        assert not re.search(
            r"\[N:A[12]\]", (child / "contract.md").read_text(encoding="utf-8")
        ), (
            "re-plan did not prune A's children; still present: "
            f"{[p.name for p in pruned]}"
        )
    for tag in ("A1", "A2"):
        with pytest.raises(AssertionError):
            _node_by_tag(project, tag)

    # Their episodic traces were compacted into the ancestor's log before
    # deletion, so the work is not lost as information.  The children's
    # on-disk node ids follow A's own id (root-01-01, root-01-02).
    log_dir = a / "log"
    assert log_dir.is_dir(), "ancestor A has no log/ after the re-plan"
    log_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted(log_dir.rglob("*"))
        if path.is_file()
    )
    assert f"{a.name}-01" in log_text and f"{a.name}-02" in log_text, (
        "the ancestor's log does not carry the compacted traces of its pruned "
        f"children ({a.name}-01, {a.name}-02)"
    )


# --------------------------------------------------------------------------
# AC3.6
# --------------------------------------------------------------------------


def test_ac3_6(tmp_path: Path) -> None:
    """A discovered dependency escalates "my contract does not provide X;
    sibling B owns it".  Both parent resolutions are tested: adding a
    depends_on edge (node pauses until B delivers), and amending B's contract
    to expose the interface earlier.  No worker-to-worker channel exists."""

    # Path 1: the parent resolves by adding a depends_on edge on A -> B.
    edge = _make_project(tmp_path / "edge", "dep_edge")
    _require_ok(_run_cli(edge, ["init", ROOT_GOAL]), "fractal init (edge)")
    _run_cli(edge, ["run"])
    _assert_root_complete(edge)

    rows = _index_rows(edge)
    a_id = _node_by_tag(edge, "A").name
    b_id = _node_by_tag(edge, "B").name
    deps = _node_depends_on(edge, a_id)
    assert deps, "the escalating node A was not given a depends_on edge to B"
    for dep in deps:
        assert dep in rows, f"depends_on names unknown sibling {dep!r}"
        assert rows[dep]["parent"] == rows[a_id]["parent"], (
            "the dependency must be a sibling of A"
        )
        assert _is_complete(rows[dep]["status"]), (
            "A's new dependency is not delivered"
        )
    assert _is_complete(rows[b_id]["status"]), "B (the owner of X) did not deliver"

    # Path 2: the parent resolves by amending B's contract to expose the
    # interface earlier.
    amended = _make_project(tmp_path / "amendb", "dep_amend")
    _require_ok(_run_cli(amended, ["init", ROOT_GOAL]), "fractal init (amend B)")
    _run_cli(amended, ["run"])
    _assert_root_complete(amended)

    b_contract = _contract_text(amended, "B")
    assert "expose_X_interface" in b_contract, (
        "B's contract was not amended to expose the interface the escalating "
        "sibling needed"
    )

    # No worker-to-worker channel anywhere: every escalate_resolve call was a
    # reopened ancestor (tag ROOT), never a sibling answering the sibling.
    for project in (edge, amended):
        calls = _read_fake_log(project["log"])
        resolves = [
            record
            for record in calls
            if record.get("kind") == "node" and record.get("verb") == "escalate_resolve"
        ]
        assert resolves, "no parent resolution happened for a discovered dependency"
        for record in resolves:
            assert record.get("tag") == "ROOT", (
                "an escalation was resolved by a worker rather than the parent; "
                "a worker-to-worker channel must not exist"
            )


# --------------------------------------------------------------------------
# AC3.7
# --------------------------------------------------------------------------


def test_ac3_7(tmp_path: Path) -> None:
    """End-to-end adversarial scenario: a root contract mandates library X; a
    scripted leaf discovers X conflicts with a requirement; the run completes
    with the root constraint amended.  A control run with escalation disabled
    ships the flaw.  Both outcomes asserted."""
    # The escalating run: root constraint amended, flaw corrected.
    run = _make_project(tmp_path / "run", "adversarial")
    _require_ok(_run_cli(run, ["init", ROOT_GOAL]), "fractal init (adversarial)")
    _seed_root_constraint(run, _CONSTRAINT_X)
    _run_cli(run, ["run"])
    _assert_root_complete(run)

    root_contract = _root_dir(run).joinpath("contract.md").read_text(encoding="utf-8")
    assert _CONSTRAINT_X not in root_contract, (
        "the run completed but the root still mandates the flawed library X"
    )
    assert _CONSTRAINT_Y in root_contract, (
        "the root constraint was not amended to the corrected library Y"
    )
    leaf_log = (_node_by_tag(run, "A1") / "log")
    assert any(
        "EVIDENCE_X_CONFLICTS_Y" in path.read_text(encoding="utf-8")
        for path in sorted(leaf_log.rglob("*"))
        if path.is_file()
    ), "the leaf's evidence of the conflict was not recorded"

    # Control run with escalation disabled: the run completes but ships the
    # flaw — the root still mandates the flawed library X.
    control = _make_project(tmp_path / "control", "control")
    _require_ok(_run_cli(control, ["init", ROOT_GOAL]), "fractal init (control)")
    _seed_root_constraint(control, _CONSTRAINT_X)
    _require_ok(_run_cli(control, ["run"]), "fractal run (control)")
    _assert_root_complete(control)

    control_contract = _root_dir(control).joinpath("contract.md").read_text(encoding="utf-8")
    assert _CONSTRAINT_X in control_contract, (
        "the control run did not ship the flaw: the root constraint X was "
        "changed without any escalation"
    )
