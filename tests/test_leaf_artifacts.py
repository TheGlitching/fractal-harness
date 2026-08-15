"""A leaf may not be recorded complete with nothing to show for it.

contracts/phase-0.md puts "verification beyond 'artifact exists'" out of
scope, which leaves the artifact-exists check itself in scope, and AC0.2
requires an artifact at every leaf.  test_phase0.py can only observe that
requirement through a fake that always returns artifacts, so these tests pin
the case it cannot reach: what the harness does when the model claims to be
finished having produced nothing.

They drive the scheduler in-process with a stand-in for the model call, so
they exercise the real parsing, the real refusal loop and the real store.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

import pytest

from fractal import runner, scheduler
from fractal.store import COMPLETE, FAILED, Store

GOAL = "write a documented fizzbuzz library"

_NODE_LINE = re.compile(r"^- node: (\S+)$", re.MULTILINE)


class _Block:
    """Enough of a content block for runner.parse_message to read."""

    def __init__(self, **fields: Any) -> None:
        self.__dict__.update(fields)


def _passing_critic(prompt: str, *, model: str | None = None) -> _Block:
    """Stand-in for the verifier: everything passes, so the leaf-artifact rules
    under test are what get exercised, not the (out-of-scope) verify pass."""
    return _Block(
        content=[
            _Block(
                type="text",
                text=(
                    '{"verdict": "PASS", "criteria": '
                    '[{"name": "ok", "pass": true}]}'
                ),
            )
        ]
    )


def _message(payload: dict[str, Any]) -> _Block:
    tool_input = {key: value for key, value in payload.items() if key != "verb"}
    return _Block(
        content=[
            _Block(
                type="tool_use",
                id="toolu_test",
                name=payload["verb"],
                input=tool_input,
            )
        ]
    )


def _node_of(prompt: str) -> str:
    """The id of the node whose contract this prompt was assembled from."""
    match = _NODE_LINE.search(prompt)
    assert match, f"no node id in the assembled prompt:\n{prompt}"
    return match.group(1)


def _artifact_files(node_path: Path) -> list[Path]:
    return [path for path in (node_path / "artifacts").rglob("*") if path.is_file()]


@pytest.fixture()
def store(tmp_path: Path) -> Store:
    store = Store(tmp_path)
    store.init(GOAL)
    return store


def test_leaf_completing_with_nothing_is_refused_and_fails(
    store: Store, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A completion carrying no artifact and no deliverable is refused every
    time it is offered, and the node ends failed rather than complete."""
    prompts: list[str] = []

    def empty_completion(prompt: str, *, model: str | None = None) -> Any:
        prompts.append(prompt)
        return _message(
            {
                "verb": "complete",
                "deliverable": "   ",
                "summary": "all done",
                "artifacts": [],
            }
        )

    monkeypatch.setattr(runner, "call_model", empty_completion)

    report = scheduler.run(store)

    root = store.get("root")
    assert root.status == FAILED, (
        f"an empty completion left the root {root.status!r}; a leaf that "
        "delivered nothing must not be recorded as complete"
    )
    assert not _artifact_files(root.path), "a refused completion wrote artifacts"
    assert len(prompts) == scheduler.MAX_ATTEMPTS, (
        f"the node was asked {len(prompts)} time(s); a refusal must be put back "
        f"to the model up to {scheduler.MAX_ATTEMPTS} times"
    )
    assert "refused" in prompts[1], (
        f"the second prompt does not tell the node its answer was refused:\n{prompts[1]}"
    )
    assert report.refused == scheduler.MAX_ATTEMPTS
    assert not report.ok


def test_leaf_completing_with_only_blank_artifacts_is_refused(
    store: Store, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Artifacts that are present but empty do not count as delivery."""

    def blank_artifacts(prompt: str, *, model: str | None = None) -> Any:
        return _message(
            {
                "verb": "complete",
                "deliverable": "",
                "summary": "all done",
                "artifacts": [{"path": "fizzbuzz.py", "content": "  \n"}],
            }
        )

    monkeypatch.setattr(runner, "call_model", blank_artifacts)

    scheduler.run(store)

    root = store.get("root")
    assert root.status == FAILED
    assert not _artifact_files(root.path), (
        "an empty artifact file was written; only content counts as delivery"
    )


def test_leaf_completing_with_only_a_deliverable_is_accepted(
    store: Store, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The check is delivery, not shape: a deliverable with no artifacts list
    still leaves a file behind, so it must not be refused."""

    def deliverable_only(prompt: str, *, model: str | None = None) -> Any:
        return _message(
            {
                "verb": "complete",
                "deliverable": "def fizzbuzz(n: int) -> str: ...\n",
                "summary": "the library",
                "artifacts": [],
            }
        )

    monkeypatch.setattr(runner, "call_model", deliverable_only)
    monkeypatch.setattr(runner, "call_critic", _passing_critic)

    report = scheduler.run(store)

    root = store.get("root")
    assert root.status == COMPLETE
    assert (root.path / "artifacts" / "deliverable.md").is_file()
    assert report.refused == 0
    assert report.ok


def test_a_parent_may_complete_without_artifacts_of_its_own(
    store: Store, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Only leaves are held to the artifact rule.  A parent aggregating its
    children carries their summary, and the artifacts live below it."""
    split_by: set[str] = set()

    def split_then_aggregate(prompt: str, *, model: str | None = None) -> Any:
        node_id = _node_of(prompt)
        if node_id == "root" and node_id not in split_by:
            split_by.add(node_id)
            return _message(
                {
                    "verb": "split",
                    "subtasks": [
                        {
                            "goal": "write the function",
                            "acceptance_criteria": ["fizzbuzz.py exists"],
                        }
                    ],
                }
            )
        if node_id == "root":
            return _message(
                {
                    "verb": "complete",
                    "deliverable": "",
                    "summary": "aggregated the one part",
                    "artifacts": [],
                }
            )
        return _message(
            {
                "verb": "complete",
                "deliverable": "",
                "summary": "wrote the function",
                "artifacts": [{"path": "fizzbuzz.py", "content": "x = 1\n"}],
            }
        )

    monkeypatch.setattr(runner, "call_model", split_then_aggregate)
    monkeypatch.setattr(runner, "call_critic", _passing_critic)

    report = scheduler.run(store)

    nodes = {node.id: node for node in store.walk()}
    assert nodes["root"].status == COMPLETE, (
        "a parent with finished children was refused for having no artifacts "
        "of its own"
    )
    assert not _artifact_files(nodes["root"].path)
    leaf = nodes["root-01"]
    assert leaf.status == COMPLETE
    assert [path.name for path in _artifact_files(leaf.path)] == ["fizzbuzz.py"]
    assert report.refused == 0
    assert report.ok
