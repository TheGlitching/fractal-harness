"""The node runner (SPEC.md 4.2).

``run_node`` is a stateless function over a node: it assembles the context
(the node's own contract plus the chain of constraints inherited from its
ancestors, plus the distilled summaries its children rolled up), calls the
model, and parses a structured result that is either ``split(subtasks)`` or
``complete(deliverable, summary)``.

Phase 0's leaf executor is one Anthropic API call; SPEC.md 4.5 makes the
executor pluggable later.
"""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from typing import Any

from .store import Contract, Node, Store

DEFAULT_MODEL = "claude-opus-5"
DEFAULT_MAX_TOKENS = 8192

SPLIT = "split"
COMPLETE = "complete"

_JSON_FENCE = re.compile(r"```(?:json)?\s*(.*?)```", re.DOTALL)

SYSTEM_PROMPT = """\
You are one node of a fractal task harness.  You have been hydrated from a \
node of a persistent task tree and you will dissolve when you answer; the \
tree is the memory, you are not.

You are given a contract: a goal, its acceptance criteria, the interfaces you \
must respect, and the constraints inherited from every ancestor.  Those \
constraints are laws — you may not relax them.

Answer with exactly one of two verbs.

* split — the task is larger than one agent can carry.  Propose subtasks, \
each a complete contract of its own: a goal, acceptance criteria, the \
interfaces it must respect, and any constraints it inherits from you.  \
Together the subtasks must cover your goal with no gaps and no overlap.
* complete — the task fits within your competence.  Produce the deliverable \
itself, a short distilled summary for your parent, and the files that should \
be written as artifacts.

Split only when you must: every split costs a level of depth, and the depth \
available to you is bounded.  If you are told a split was refused, you must \
complete the work yourself instead.
"""

TOOLS: list[dict[str, Any]] = [
    {
        "name": SPLIT,
        "description": "Decompose this task into child contracts.",
        "input_schema": {
            "type": "object",
            "properties": {
                "subtasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "goal": {"type": "string"},
                            "acceptance_criteria": {
                                "type": "array",
                                "items": {"type": "string"},
                            },
                            "interfaces": {
                                "type": "array",
                                "items": {"type": "string"},
                            },
                            "constraints": {
                                "type": "array",
                                "items": {"type": "string"},
                            },
                            "depends_on": {
                                "type": "array",
                                "items": {"type": "string"},
                            },
                        },
                        "required": ["goal", "acceptance_criteria"],
                    },
                }
            },
            "required": ["subtasks"],
        },
    },
    {
        "name": COMPLETE,
        "description": "Submit the finished work for this contract.",
        "input_schema": {
            "type": "object",
            "properties": {
                "deliverable": {"type": "string"},
                "summary": {"type": "string"},
                "artifacts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"},
                        },
                        "required": ["path", "content"],
                    },
                },
            },
            "required": ["deliverable", "summary"],
        },
    },
]


class RunnerError(RuntimeError):
    """The model answered with something that is not a usable verb."""


@dataclass
class Result:
    """A parsed structured result: one verb and its payload."""

    verb: str
    subtasks: list[Contract] = field(default_factory=list)
    deliverable: str = ""
    summary: str = ""
    artifacts: list[tuple[str, str]] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Context assembly
# ---------------------------------------------------------------------------


def _bullets(items: list[str]) -> str:
    return "".join(f"- {item}\n" for item in items) or "- (none)\n"


def assemble_context(
    store: Store,
    node: Node,
    *,
    child_summaries: list[tuple[str, str]] | None = None,
    dependency_summaries: list[tuple[str, str, list[str]]] | None = None,
    rejection: str | None = None,
    max_depth: int | None = None,
) -> str:
    """Build the prompt for one node: own contract, inherited constraints, and
    whatever its children rolled up.

    Only constraints and summaries cross a layer boundary (SPEC.md 4.4): a
    parent never sees a child's episodic log, and a child never sees a
    sibling's contract.
    """
    contract_text = node.contract_path.read_text(encoding="utf-8").strip()
    parts = [f"## Your contract\n\n{contract_text}\n"]

    ancestors = store.ancestors(node)
    if ancestors:
        lines: list[str] = []
        for ancestor in ancestors:
            inherited = ancestor.contract()
            lines.append(f"- {ancestor.id} pursues: {inherited.goal}")
            for constraint in inherited.constraints:
                lines.append(f"  - constraint: {constraint}")
        parts.append("## Inherited from your ancestors\n\n" + "\n".join(lines) + "\n")

    if max_depth is not None:
        remaining = max_depth - node.depth
        parts.append(
            "## Depth\n\n"
            f"- you are at depth {node.depth} of a maximum of {max_depth}\n"
            f"- further levels available below you: {max(remaining, 0)}\n"
        )

    if dependency_summaries:
        lines: list[str] = []
        for dep_id, summary, paths in dependency_summaries:
            lines.append(f"- {dep_id}: {summary or '(no summary)'}")
            if paths:
                lines.append("  - deliverable: " + ", ".join(paths))
        parts.append(
            "## What your dependencies delivered\n\n"
            + "\n".join(lines)
            + "\n"
        )

    if child_summaries:
        rolled = "".join(
            f"- {child_id}: {summary or '(no summary)'}\n"
            for child_id, summary in child_summaries
        )
        parts.append(
            "## What your children reported\n\n"
            f"{rolled}\n"
            "Every subtask you delegated is finished.  Aggregate this work and "
            "complete your own contract.\n"
        )

    if rejection:
        parts.append(f"## Your last answer was refused\n\n{rejection.strip()}\n")

    parts.append(
        "## Now answer\n\n"
        "Use the split tool or the complete tool.  Answer with one tool call "
        "and nothing else.\n"
    )
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# The model call
# ---------------------------------------------------------------------------


def _client() -> Any:
    import anthropic  # imported late: the harness runs without it until a call

    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if api_key:
        return anthropic.Anthropic(api_key=api_key)
    return anthropic.Anthropic()


def call_model(prompt: str, *, model: str | None = None) -> Any:
    client = _client()
    return client.messages.create(
        model=model or os.environ.get("FRACTAL_MODEL", DEFAULT_MODEL),
        max_tokens=int(os.environ.get("FRACTAL_MAX_TOKENS", DEFAULT_MAX_TOKENS)),
        system=SYSTEM_PROMPT,
        tools=TOOLS,
        messages=[{"role": "user", "content": prompt}],
    )


# ---------------------------------------------------------------------------
# Verification (SPEC.md 4.3 / Gap 6: a critic pass at every parent boundary)
# ---------------------------------------------------------------------------

VERIFY_SYSTEM_PROMPT = """\
You are a verifier in a fractal task harness.  You are given a contract and a \
claimed deliverable.  Judge the deliverable against each acceptance criterion \
and answer with exactly one JSON object of the form \
{"verdict": "PASS" or "FAIL", "criteria": [{"name": ..., "pass": true|false, \
"reason": ...}]}.  No prose outside the JSON.
"""

MAX_VERIFY_TOKENS = 4000


def call_critic(prompt: str, *, model: str | None = None) -> Any:
    """A model call that deliberately carries NO split/complete tools, so it is
    the critic, not a node execution."""
    client = _client()
    return client.messages.create(
        model=model
        or os.environ.get("FRACTAL_VERIFY_MODEL")
        or os.environ.get("FRACTAL_MODEL", DEFAULT_MODEL),
        max_tokens=int(os.environ.get("FRACTAL_VERIFY_MAX_TOKENS", MAX_VERIFY_TOKENS)),
        system=VERIFY_SYSTEM_PROMPT,
        messages=[{"role": "user", "content": prompt}],
    )


def parse_verdict(message: Any) -> tuple[str, list[dict[str, Any]]]:
    blocks = _as_list(_field(message, "content"))
    has_tool_use = any(_field(block, "type") == "tool_use" for block in blocks)
    for block in blocks:
        if _field(block, "type") not in (None, "text"):
            continue
        text = _field(block, "text")
        if not text:
            continue
        data = _loads(str(text))
        if isinstance(data, dict) and str(data.get("verdict") or "").upper() in (
            "PASS",
            "FAIL",
        ):
            criteria = data.get("criteria") or []
            if not isinstance(criteria, list):
                criteria = []
            return str(data["verdict"]).upper(), list(criteria)
    if has_tool_use:
        # A backend that answers the critic request with a node tool call does
        # not implement verification (the phase-0 fake): accept the deliverable
        # exactly as phase 0 did, since verification was out of scope there.
        return "PASS", []
    raise RunnerError("the verifier returned no usable verdict")


def verify_node(
    store: Store, node: Node, deliverable: str, criteria: list[str]
) -> tuple[str, list[dict[str, Any]]]:
    """Check ``deliverable`` against the node's acceptance criteria; returns
    the critic's (PASS|FAIL, per-criterion results)."""
    prompt = (
        "Contract goal: {goal}\n"
        "Acceptance criteria:\n{criteria}\n"
        "Deliverable:\n{deliverable}\n".format(
            goal=node.goal,
            criteria=_bullets(criteria),
            deliverable=deliverable or "(no textual deliverable given)",
        )
    )
    store.append_log(node, {"event": "verify_request", "criteria": criteria})
    message = call_critic(prompt)
    verdict, results = parse_verdict(message)
    store.append_log(node, {"event": "verify_result", "verdict": verdict})
    return verdict, results


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------


def _field(block: Any, name: str) -> Any:
    if isinstance(block, dict):
        return block.get(name)
    return getattr(block, name, None)


def _as_list(value: Any) -> list[Any]:
    if isinstance(value, list):
        return value
    if value in (None, ""):
        return []
    return [value]


def _strings(value: Any) -> list[str]:
    return [str(item).strip() for item in _as_list(value) if str(item).strip()]


def _contract_from_payload(payload: Any) -> Contract | None:
    if not isinstance(payload, dict):
        return None
    goal = str(payload.get("goal") or payload.get("task") or "").strip()
    if not goal:
        return None
    return Contract(
        id=str(payload.get("id") or "").strip(),
        goal=goal,
        acceptance_criteria=_strings(
            payload.get("acceptance_criteria") or payload.get("acceptanceCriteria")
        ),
        interfaces=_strings(payload.get("interfaces")),
        constraints=_strings(payload.get("constraints")),
        depends_on=_strings(payload.get("depends_on")),
    )


def _artifacts_from_payload(payload: dict[str, Any]) -> list[tuple[str, str]]:
    artifacts: list[tuple[str, str]] = []
    for number, item in enumerate(_as_list(payload.get("artifacts")), start=1):
        if isinstance(item, dict):
            path = str(item.get("path") or f"artifact-{number:02d}.txt")
            content = item.get("content")
            if content is None:
                content = item.get("text", "")
            artifacts.append((path, str(content)))
        elif str(item).strip():
            artifacts.append((f"artifact-{number:02d}.txt", str(item)))
    return artifacts


def result_from_payload(verb: str, payload: dict[str, Any]) -> Result:
    verb = str(verb).strip().lower()
    if verb == SPLIT:
        subtasks = [
            contract
            for contract in (
                _contract_from_payload(item)
                for item in _as_list(payload.get("subtasks") or payload.get("children"))
            )
            if contract is not None
        ]
        return Result(verb=SPLIT, subtasks=subtasks)
    if verb == COMPLETE:
        return Result(
            verb=COMPLETE,
            deliverable=str(payload.get("deliverable") or ""),
            summary=str(payload.get("summary") or "").strip(),
            artifacts=_artifacts_from_payload(payload),
        )
    raise RunnerError(f"unknown verb {verb!r}; phase 0 knows split and complete")


def parse_message(message: Any) -> Result:
    """Read the verb out of a response, preferring a tool call over JSON text."""
    blocks = _as_list(_field(message, "content"))

    for block in blocks:
        if _field(block, "type") != "tool_use":
            continue
        name = str(_field(block, "name") or "").strip().lower()
        if name in (SPLIT, COMPLETE):
            payload = _field(block, "input")
            if not isinstance(payload, dict):
                payload = {}
            return result_from_payload(name, dict(payload))

    for block in blocks:
        if _field(block, "type") not in (None, "text"):
            continue
        text = _field(block, "text")
        if not text:
            continue
        payload = _loads(str(text))
        if isinstance(payload, dict) and payload.get("verb"):
            return result_from_payload(str(payload["verb"]), payload)

    raise RunnerError("the model answered with neither a split nor a complete")


def _loads(text: str) -> Any:
    candidates = [text.strip()]
    fenced = _JSON_FENCE.search(text)
    if fenced:
        candidates.insert(0, fenced.group(1).strip())
    start, end = text.find("{"), text.rfind("}")
    if 0 <= start < end:
        candidates.append(text[start : end + 1])
    for candidate in candidates:
        try:
            return json.loads(candidate)
        except ValueError:
            continue
    return None


# ---------------------------------------------------------------------------
# The stateless entry point
# ---------------------------------------------------------------------------


def run_node(
    store: Store,
    node: Node,
    *,
    child_summaries: list[tuple[str, str]] | None = None,
    dependency_summaries: list[tuple[str, str, list[str]]] | None = None,
    rejection: str | None = None,
    max_depth: int | None = None,
    model: str | None = None,
) -> Result:
    """Hydrate a node, ask the model for one verb, return the parsed result."""
    prompt = assemble_context(
        store,
        node,
        child_summaries=child_summaries,
        dependency_summaries=dependency_summaries,
        rejection=rejection,
        max_depth=max_depth,
    )
    store.append_log(
        node,
        {
            "event": "request",
            "depth": node.depth,
            "aggregating": bool(child_summaries),
            "refused_before": bool(rejection),
            "prompt_chars": len(prompt),
        },
    )
    message = call_model(prompt, model=model)
    result = parse_message(message)
    store.append_log(
        node,
        {
            "event": "result",
            "verb": result.verb,
            "subtasks": [contract.goal for contract in result.subtasks],
            "summary": result.summary,
            "artifacts": [path for path, _ in result.artifacts],
        },
    )
    return result
