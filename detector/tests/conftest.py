"""Test configuration.

`evaluation/` holds runnable scripts rather than a package, so its modules are
not importable by name. The benchmark's pure helpers are worth unit-testing, so
the directory joins the path here instead of in each test file.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "evaluation"))
