"""Test configuration.

`evaluation/` and `scripts/` hold runnable scripts rather than packages, so
their modules are not importable by name. The benchmark's pure helpers and the
source digest the published-metrics gate relies on are worth unit-testing, so
both directories join the path here instead of in each test file.
"""

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "evaluation"))
sys.path.insert(0, str(ROOT / "scripts"))
