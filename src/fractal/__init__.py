"""The fractal harness: a persistent task tree executed by ephemeral agents.

Phase 0 (contracts/phase-0.md) implements the state layer (SPEC.md 4.1), the
node runner (4.2) and a sequential scheduler (4.3) with the two verbs
``split`` and ``complete``.
"""

from __future__ import annotations

__all__ = ["__version__"]

__version__ = "0.1.0"
