"""Acceptance tests for contracts/phase-0.md (AC0.1 - AC0.5).

These tests are the acceptance criteria for Phase 0. They drive the harness
through its CLI only, so `src/` is free to be organised however the contract
says as long as the observable behaviour holds.

Interface assumptions pinned by these tests
-------------------------------------------
1. The CLI is runnable as ``python -m fractal.cli`` with ``src/`` on
   ``PYTHONPATH``, and accepts ``init <goal>``, ``run`` and ``status``.
   All three operate on a project rooted at the process working directory.
2. ``init`` creates ``tree/root/`` containing ``contract.md``,
   ``decisions.md``, ``log/``, ``artifacts/`` and ``children/`` (SPEC.md
   Section 4.1), and child nodes live in ``<node>/children/<node id>/``.
   A node's directory name is its node id; the root's id is ``root``.
3. The SQLite index is a single SQLite file somewhere under the project
   directory, holding one row per node with an id column (``id`` or
   ``node_id``), a parent column (``parent`` or ``parent_id``) and a
   ``status`` column. Its location and file name are not constrained; the
   tests discover it by scanning for the SQLite magic header.
4. The leaf executor reaches Anthropic through ``anthropic.Anthropic(...)``
   (or ``anthropic.Client(...)``) and ``client.messages.create(**kwargs)``.
   The fake below is injected via ``sitecustomize`` so it applies to every
   harness process, including the ones these tests kill and resume.
5. The fake answers with both a JSON text block and an equivalent
   ``tool_use`` block named ``split`` / ``complete``, so either parsing
   strategy works.

Depth counting: the root is depth 1, so "max depth 3" means at most three
nested levels of nodes. The tests assert this structurally (nesting levels
on disk) rather than reading a depth field, so the implementation is free to
number depths from 0 internally.
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

ROOT_GOAL = "write a documented fizzbuzz library"

CLI_TIMEOUT = 180.0

# Statuses are matched loosely so the implementation may pick its own words.
_COMPLETE_TOKENS = ("complete", "completed", "done", "succeeded")
_FAILED_TOKENS = ("failed", "fail", "error", "aborted")
_PENDING_TOKENS = ("pending", "ready", "queued", "todo", "new")


# --------------------------------------------------------------------------
# The deterministic fake Anthropic API, injected through sitecustomize.
# --------------------------------------------------------------------------

FAKE_SITECUSTOMIZE = r'''
"""Deterministic stand-in for the anthropic SDK, installed at interpreter
start so every harness subprocess sees it.

Node identity is carried in the goal text as a tag "[L<level>#<index>]".
A node's assembled prompt contains its own contract, so the deepest tag in
the request identifies the node being run; the root carries no tag.

Behaviour:
  shallow mode - the root splits into two children, every child completes.
  deep mode    - levels 1, 2 and 3 all attempt a split; the level 3 split
                 is illegal and must be rejected by the harness.
A node that has already returned `split` once returns `complete` on any
later call, so an implementation that re-runs a parent to aggregate its
children still terminates.
"""

import json
import os
import re
import sys
import threading
import time
import types

_LOG = os.environ.get("FRACTAL_FAKE_LOG")

if _LOG:
    _MODE = os.environ.get("FRACTAL_FAKE_MODE", "shallow")
    _BLOCK_AT = int(os.environ.get("FRACTAL_FAKE_BLOCK_AT", "0") or "0")
    _LOCK = threading.Lock()
    _TAG_RE = re.compile(r"\[L(\d+)#(\d+)\]")

    _DELIVERABLE = (
        "def fizzbuzz(n: int) -> str:\n"
        '    """Return the FizzBuzz rendering of ``n``."""\n'
        '    if n % 15 == 0:\n'
        '        return "FizzBuzz"\n'
        '    if n % 3 == 0:\n'
        '        return "Fizz"\n'
        '    if n % 5 == 0:\n'
        '        return "Buzz"\n'
        "    return str(n)\n"
    )

    def _read_log():
        records = []
        try:
            with open(_LOG, encoding="utf-8") as handle:
                for line in handle:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        records.append(json.loads(line))
                    except ValueError:
                        continue
        except FileNotFoundError:
            pass
        return records

    def _append(record):
        with open(_LOG, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")
            handle.flush()
            os.fsync(handle.fileno())

    def _identify(kwargs):
        blob = json.dumps(kwargs, default=str)
        matches = _TAG_RE.findall(blob)
        if not matches:
            return 1, 0, "ROOT"
        level, index = max((int(a), int(b)) for a, b in matches)
        return level, index, "L%d#%d" % (level, index)

    def _subtasks(level, index):
        child_level = level + 1
        first = 1 if level == 1 else (2 * index - 1)
        out = []
        for offset in (0, 1):
            child_index = first + offset
            tag = "[L%d#%d]" % (child_level, child_index)
            out.append(
                {
                    "goal": "part %d of %s %s" % (child_index, "the fizzbuzz library", tag),
                    "acceptance_criteria": [
                        "an artifact file exists for %s" % tag,
                        "the part is documented",
                    ],
                    "interfaces": [],
                    "constraints": [],
                }
            )
        return out

    def _decide(level, tag):
        already_split = any(
            record.get("event") == "result"
            and record.get("tag") == tag
            and record.get("verb") == "split"
            for record in _read_log()
        )
        if already_split:
            return "complete"
        if _MODE == "deep":
            return "split" if level <= 3 else "complete"
        return "split" if level == 1 else "complete"

    def _payload(verb, level, index, tag):
        if verb == "split":
            return {"verb": "split", "subtasks": _subtasks(level, index)}
        return {
            "verb": "complete",
            "deliverable": _DELIVERABLE,
            "summary": "completed %s of the fizzbuzz library" % tag,
            "artifacts": [
                {"path": "fizzbuzz.py", "content": _DELIVERABLE},
                {
                    "path": "README.md",
                    "content": "# fizzbuzz\n\nDeliverable for %s.\n" % tag,
                },
            ],
        }

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

    def _respond(kwargs):
        with _LOCK:
            call_number = (
                sum(1 for record in _read_log() if record.get("event") == "start") + 1
            )
            level, index, tag = _identify(kwargs)
            _append(
                {"event": "start", "n": call_number, "tag": tag, "level": level}
            )
            if _BLOCK_AT and call_number == _BLOCK_AT:
                blocked_until = time.time() + 600
                while time.time() < blocked_until:
                    time.sleep(0.5)
            verb = _decide(level, tag)
            payload = _payload(verb, level, index, tag)
            _append(
                {
                    "event": "result",
                    "n": call_number,
                    "tag": tag,
                    "level": level,
                    "verb": verb,
                }
            )

        tool_input = dict(payload)
        tool_input.pop("verb")
        return _Message(
            id="msg_fake_%d" % call_number,
            type="message",
            role="assistant",
            model=kwargs.get("model", "claude-opus-5"),
            stop_reason="tool_use",
            stop_sequence=None,
            content=[
                _Block(type="text", text=json.dumps(payload, indent=2)),
                _Block(
                    type="tool_use",
                    id="toolu_fake_%d" % call_number,
                    name=verb,
                    input=tool_input,
                ),
            ],
            usage=_Block(input_tokens=100, output_tokens=100),
        )

    class _Messages(object):
        def create(self, **kwargs):
            return _respond(kwargs)

        def with_raw_response(self, **kwargs):
            return _respond(kwargs)

    class _FakeClient(object):
        def __init__(self, *args, **kwargs):
            self.api_key = kwargs.get("api_key") or os.environ.get(
                "ANTHROPIC_API_KEY", "fake-key"
            )
            self.messages = _Messages()
            self.beta = types.SimpleNamespace(messages=self.messages)

        def close(self):
            return None

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

    class _AnthropicError(Exception):
        pass

    _module = types.ModuleType("anthropic")
    _module.Anthropic = _FakeClient
    _module.Client = _FakeClient
    _module.AsyncAnthropic = _FakeClient
    _module.AsyncClient = _FakeClient
    _module.NOT_GIVEN = None
    _module.APIError = _AnthropicError
    _module.APIStatusError = _AnthropicError
    _module.APIConnectionError = _AnthropicError
    _module.APITimeoutError = _AnthropicError
    _module.RateLimitError = _AnthropicError
    _module.BadRequestError = _AnthropicError
    _module.InternalServerError = _AnthropicError
    _module.AuthenticationError = _AnthropicError

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
    """Create an isolated project directory plus the injected fake."""
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
    env["FRACTAL_FAKE_BLOCK_AT"] = "0"

    return {"dir": project, "env": env, "log": log}


def _cli_command(args: list[str]) -> list[str]:
    return [sys.executable, "-m", "fractal.cli", *args]


def _run_cli(
    project: dict[str, Any],
    args: list[str],
    *,
    env_overrides: dict[str, str] | None = None,
    timeout: float = CLI_TIMEOUT,
) -> subprocess.CompletedProcess[str]:
    env = dict(project["env"])
    if env_overrides:
        env.update(env_overrides)
    return subprocess.run(
        _cli_command(args),
        cwd=project["dir"],
        env=env,
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


def _disk_nodes(project: dict[str, Any]) -> list[tuple[int, Path, str]]:
    """DFS pre-order walk of the on-disk tree: (depth, path, node id)."""
    root = _root_dir(project)
    assert root.is_dir(), f"expected {root} to exist"

    ordered: list[tuple[int, Path, str]] = []

    def visit(node: Path, depth: int) -> None:
        ordered.append((depth, node, node.name))
        for child in _children_of(node):
            visit(child, depth + 1)

    visit(root, 1)
    return ordered


def _leaf_nodes(project: dict[str, Any]) -> list[Path]:
    return [path for _, path, _ in _disk_nodes(project) if not _children_of(path)]


def _assert_node_layout(node: Path) -> None:
    """SPEC.md Section 4.1: every node directory has the same shape."""
    contract = node / "contract.md"
    assert contract.is_file(), f"{node} is missing contract.md"
    assert contract.read_text(encoding="utf-8").strip(), f"{contract} is empty"
    assert (node / "decisions.md").is_file(), f"{node} is missing decisions.md"
    for subdir in ("log", "artifacts", "children"):
        assert (node / subdir).is_dir(), f"{node} is missing {subdir}/"


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
    """Return {node id: {"parent": ..., "status": ...}} from the SQLite index."""
    db = _find_index_db(project)
    uri = f"file:{db}?mode=ro"

    last_error: Exception | None = None
    for _ in range(40):
        try:
            connection = sqlite3.connect(uri, uri=True, timeout=1.0)
        except sqlite3.Error as error:  # pragma: no cover - retry path
            last_error = error
            time.sleep(0.1)
            continue
        try:
            connection.row_factory = sqlite3.Row
            tables = [
                row[0]
                for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'"
                )
            ]
            for table in tables:
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
                    parent = row[parent_column]
                    parent_id = (
                        None
                        if parent in (None, "")
                        else str(parent).strip().rstrip("/").split("/")[-1]
                    )
                    rows[node_id] = {
                        "parent": parent_id,
                        "status": str(row[status_column] or ""),
                    }
                return rows
            raise AssertionError(
                f"no table in {db} carries id/parent/status columns; found {tables}"
            )
        finally:
            connection.close()
    raise AssertionError(f"could not open the SQLite index {db}: {last_error}")


def _is_complete(status: str) -> bool:
    return status.strip().lower() in _COMPLETE_TOKENS


def _is_failed(status: str) -> bool:
    return status.strip().lower() in _FAILED_TOKENS


def _is_pending(status: str) -> bool:
    return status.strip().lower() in _PENDING_TOKENS


# --------------------------------------------------------------------------
# `fractal status` helpers
# --------------------------------------------------------------------------


def _id_pattern(node_id: str) -> re.Pattern[str]:
    return re.compile(r"(?<![0-9A-Za-z_\-])" + re.escape(node_id) + r"(?![0-9A-Za-z_\-])")


def _locate_in_status(
    output: str, node_id: str, status: str
) -> tuple[int, str]:
    """Find the first status line naming this node together with its status."""
    id_pattern = _id_pattern(node_id)
    status_pattern = re.compile(re.escape(status.strip()), re.IGNORECASE)
    for number, line in enumerate(output.splitlines()):
        if id_pattern.search(line) and status_pattern.search(line):
            return number, line
    raise AssertionError(
        f"`fractal status` has no line naming node {node_id!r} with status "
        f"{status!r}\n--- status output ---\n{output}"
    )


def _assert_status_matches_disk(project: dict[str, Any], output: str) -> None:
    disk = _disk_nodes(project)
    index = _index_rows(project)

    disk_ids = [node_id for _, _, node_id in disk]
    assert len(disk_ids) == len(set(disk_ids)), f"duplicate node ids on disk: {disk_ids}"
    assert set(index) == set(disk_ids), (
        "the SQLite index and the on-disk tree disagree; "
        f"index only: {sorted(set(index) - set(disk_ids))}, "
        f"disk only: {sorted(set(disk_ids) - set(index))}"
    )

    # Parent edges in the index must match the directory nesting.
    for depth, path, node_id in disk:
        if depth == 1:
            assert index[node_id]["parent"] in (None, ""), (
                f"root node {node_id!r} must have no parent in the index, "
                f"got {index[node_id]['parent']!r}"
            )
        else:
            expected_parent = path.parent.parent.name
            assert index[node_id]["parent"] == expected_parent, (
                f"node {node_id!r} sits under {expected_parent!r} on disk but the "
                f"index records parent {index[node_id]['parent']!r}"
            )

    placements: list[tuple[int, str, int, str]] = []
    for depth, _, node_id in disk:
        line_number, line = _locate_in_status(output, node_id, index[node_id]["status"])
        placements.append((depth, node_id, line_number, line))

    printed_order = [node_id for _, node_id, _, _ in sorted(placements, key=lambda p: p[2])]
    assert printed_order == disk_ids, (
        "`fractal status` does not print the tree in on-disk order\n"
        f"printed: {printed_order}\nexpected: {disk_ids}\n"
        f"--- status output ---\n{output}"
    )

    indents = {node_id: len(line) - len(line.lstrip()) for _, node_id, _, line in placements}
    for depth, path, node_id in disk:
        if depth == 1:
            continue
        parent_id = path.parent.parent.name
        assert indents[node_id] > indents[parent_id], (
            f"node {node_id!r} is not indented under its parent {parent_id!r}\n"
            f"--- status output ---\n{output}"
        )


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


@pytest.fixture()
def shallow_project(tmp_path: Path) -> dict[str, Any]:
    return _make_project(tmp_path, "shallow")


@pytest.fixture()
def deep_project(tmp_path: Path) -> dict[str, Any]:
    return _make_project(tmp_path, "deep")


# --------------------------------------------------------------------------
# AC0.1
# --------------------------------------------------------------------------


def test_ac0_1(shallow_project: dict[str, Any]) -> None:
    """`fractal init` creates tree/root/ with a valid contract and a pending
    node in the index."""
    _require_ok(_run_cli(shallow_project, ["init", ROOT_GOAL]), "fractal init")

    root = _root_dir(shallow_project)
    assert root.is_dir(), "fractal init did not create tree/root/"
    _assert_node_layout(root)

    contract = (root / "contract.md").read_text(encoding="utf-8")
    assert ROOT_GOAL in contract, (
        "tree/root/contract.md does not carry the goal it was initialised with\n"
        f"--- contract.md ---\n{contract}"
    )

    assert not _children_of(root), "a freshly initialised root must have no children"

    index = _index_rows(shallow_project)
    assert list(index) == ["root"], (
        f"the index must hold exactly the root node after init, got {sorted(index)}"
    )
    assert index["root"]["parent"] in (None, ""), "the root node must have no parent"
    assert _is_pending(index["root"]["status"]), (
        f"the root node must be indexed as pending, got {index['root']['status']!r}"
    )


# --------------------------------------------------------------------------
# AC0.2
# --------------------------------------------------------------------------


def test_ac0_2(shallow_project: dict[str, Any]) -> None:
    """`fractal run` completes the root, with at least one level of children
    and an artifact at every leaf."""
    _require_ok(_run_cli(shallow_project, ["init", ROOT_GOAL]), "fractal init")
    _require_ok(_run_cli(shallow_project, ["run"]), "fractal run")

    index = _index_rows(shallow_project)
    assert _is_complete(index["root"]["status"]), (
        f"the root must be completed after a run, got {index['root']['status']!r}"
    )

    root = _root_dir(shallow_project)
    children = _children_of(root)
    assert len(children) >= 1, "the run produced no children under the root"

    nodes = _disk_nodes(shallow_project)
    assert max(depth for depth, _, _ in nodes) >= 2, (
        "the tree has no level of children below the root"
    )

    for _, path, node_id in nodes:
        _assert_node_layout(path)
        assert _is_complete(index[node_id]["status"]), (
            f"node {node_id!r} is {index[node_id]['status']!r}, not complete"
        )

    leaves = _leaf_nodes(shallow_project)
    assert leaves, "the tree has no leaves"
    for leaf in leaves:
        artifacts = [path for path in (leaf / "artifacts").rglob("*") if path.is_file()]
        assert artifacts, f"leaf {leaf.name!r} completed with no artifacts"
        assert any(path.read_bytes().strip() for path in artifacts), (
            f"leaf {leaf.name!r} has only empty artifacts"
        )


# --------------------------------------------------------------------------
# AC0.3
# --------------------------------------------------------------------------


def _wait_for_start_of_call(
    project: dict[str, Any], process: subprocess.Popen[str], call_number: int
) -> None:
    deadline = time.monotonic() + CLI_TIMEOUT
    while time.monotonic() < deadline:
        if any(
            record.get("event") == "start" and record.get("n") == call_number
            for record in _read_fake_log(project["log"])
        ):
            return
        if process.poll() is not None:
            raise AssertionError(
                f"`fractal run` exited (code {process.returncode}) before model call "
                f"{call_number} began; it should still have work to do"
            )
        time.sleep(0.05)
    raise AssertionError(f"model call {call_number} never started within the timeout")


def _wait_for_a_completed_node(project: dict[str, Any]) -> str:
    deadline = time.monotonic() + 30.0
    seen: dict[str, dict[str, Any]] = {}
    while time.monotonic() < deadline:
        try:
            seen = _index_rows(project)
        except AssertionError:
            time.sleep(0.1)
            continue
        completed = [
            node_id for node_id, row in seen.items() if _is_complete(row["status"])
        ]
        if completed:
            return sorted(completed)[0]
        time.sleep(0.1)
    raise AssertionError(
        "no node was durably recorded as complete before the kill; the first "
        f"finished node's state never reached the index. Index held: {seen}"
    )


def test_ac0_3(shallow_project: dict[str, Any]) -> None:
    """A SIGKILL after the first node completes does not lose work: a second
    `fractal run` resumes from disk and completes, and no node that had already
    completed runs again."""
    _require_ok(_run_cli(shallow_project, ["init", ROOT_GOAL]), "fractal init")

    # Call 1 is the root split, call 2 is the first leaf. Blocking call 3 pins
    # the kill to the moment after the first leaf's result has been applied.
    env = dict(shallow_project["env"])
    env["FRACTAL_FAKE_BLOCK_AT"] = "3"
    process = subprocess.Popen(
        _cli_command(["run"]),
        cwd=shallow_project["dir"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        _wait_for_start_of_call(shallow_project, process, 3)
        killed_after = _wait_for_a_completed_node(shallow_project)
        os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        process.wait(timeout=30)
    finally:
        if process.poll() is None:  # pragma: no cover - cleanup path
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
            process.wait(timeout=30)

    assert process.returncode in (-signal.SIGKILL, 128 + signal.SIGKILL), (
        f"the scheduler was not killed by SIGKILL (returncode {process.returncode})"
    )

    log_before_resume = _read_fake_log(shallow_project["log"])
    completed_before_resume = {
        record["tag"]
        for record in log_before_resume
        if record.get("event") == "result" and record.get("verb") == "complete"
    }
    assert completed_before_resume, (
        "no node completed before the kill, so the resume is not being tested"
    )

    # Resume: same project, same append-only call log, no blocking this time.
    resumed = _run_cli(shallow_project, ["run"])
    _require_ok(resumed, "fractal run (resume after SIGKILL)")

    index = _index_rows(shallow_project)
    assert _is_complete(index["root"]["status"]), (
        f"the resumed run did not complete the root, status {index['root']['status']!r}"
    )
    for _, path, node_id in _disk_nodes(shallow_project):
        assert _is_complete(index[node_id]["status"]), (
            f"node {node_id!r} is {index[node_id]['status']!r} after the resumed run"
        )
        if not _children_of(path):
            artifacts = [p for p in (path / "artifacts").rglob("*") if p.is_file()]
            assert artifacts, f"leaf {node_id!r} has no artifacts after the resume"

    # No node runs again once it has completed.
    finished: set[str] = set()
    for record in _read_fake_log(shallow_project["log"]):
        tag = record.get("tag")
        if record.get("event") == "start":
            assert tag not in finished, (
                f"node {tag!r} was executed again after it had already completed "
                f"(the node completed before the kill of {killed_after!r})"
            )
        elif record.get("event") == "result" and record.get("verb") == "complete":
            finished.add(tag)


# --------------------------------------------------------------------------
# AC0.4
# --------------------------------------------------------------------------


def test_ac0_4(deep_project: dict[str, Any]) -> None:
    """A split proposed at depth 3 is rejected; the node is forced to complete
    or fail and the tree never exceeds three levels."""
    _require_ok(_run_cli(deep_project, ["init", ROOT_GOAL]), "fractal init")

    result = _run_cli(deep_project, ["run"])
    assert result.returncode is not None, "fractal run did not terminate"

    log = _read_fake_log(deep_project["log"])
    illegal_attempts = [
        record
        for record in log
        if record.get("event") == "result"
        and record.get("verb") == "split"
        and record.get("level") == 3
    ]
    assert illegal_attempts, (
        "the harness never ran a depth 3 node, so the illegal split was never "
        f"attempted; call log: {log}"
    )

    nodes = _disk_nodes(deep_project)
    max_depth = max(depth for depth, _, _ in nodes)
    assert max_depth <= 3, (
        f"the tree reached depth {max_depth}; the hardcoded limit is 3. "
        f"Nodes: {[(d, i) for d, _, i in nodes]}"
    )

    index = _index_rows(deep_project)

    depth_three = [(path, node_id) for depth, path, node_id in nodes if depth == 3]
    assert depth_three, "the run produced no depth 3 nodes to test the limit against"
    for path, node_id in depth_three:
        assert not _children_of(path), (
            f"the rejected split at depth 3 still created children under {node_id!r}: "
            f"{[child.name for child in _children_of(path)]}"
        )
        status = index[node_id]["status"]
        assert _is_complete(status) or _is_failed(status), (
            f"depth 3 node {node_id!r} was left {status!r}; a rejected split must "
            "force the node to complete or fail"
        )

    root_status = index["root"]["status"]
    assert _is_complete(root_status) or _is_failed(root_status), (
        f"the root was left {root_status!r} after the depth limit was hit"
    )


# --------------------------------------------------------------------------
# AC0.5
# --------------------------------------------------------------------------


def test_ac0_5(shallow_project: dict[str, Any]) -> None:
    """`fractal status` output matches the on-disk tree exactly, both for a
    freshly initialised tree and for a completed one."""
    _require_ok(_run_cli(shallow_project, ["init", ROOT_GOAL]), "fractal init")

    after_init = _require_ok(_run_cli(shallow_project, ["status"]), "fractal status")
    assert after_init.strip(), "`fractal status` printed nothing after init"
    _assert_status_matches_disk(shallow_project, after_init)

    _require_ok(_run_cli(shallow_project, ["run"]), "fractal run")

    after_run = _require_ok(_run_cli(shallow_project, ["status"]), "fractal status")
    _assert_status_matches_disk(shallow_project, after_run)

    nodes = _disk_nodes(shallow_project)
    assert len(nodes) > 1, "the run did not grow the tree, so AC0.5 is untested"
    assert after_run != after_init, (
        "`fractal status` printed identical output before and after the run, so it "
        "is not reading the tree"
    )
