"""Make ``src/`` importable so tests can drive the harness in-process.

test_phase0.py does not need this — it drives the CLI as a subprocess and puts
``src/`` on that process's PYTHONPATH itself — but a test that imports
``fractal`` directly does, and AGENTS.md prescribes a bare ``pytest -x -q``.
"""

from __future__ import annotations

import sys
from pathlib import Path

SRC_DIR = Path(__file__).resolve().parents[1] / "src"

if str(SRC_DIR) not in sys.path:
    sys.path.insert(0, str(SRC_DIR))
