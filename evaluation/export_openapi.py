"""Write the detector's OpenAPI document to docs/api/openapi.json (REQ-44).

The committed file is what the Rust gateway reads: a schema that lives only
inside a test run is not a source of truth for another language.

Run from the repository root:  make openapi
"""

import json
from pathlib import Path
from typing import cast

from tessera_detector.api import create_app
from tessera_detector.pipeline import Detector

TARGET = Path(__file__).resolve().parents[1] / "docs" / "api" / "openapi.json"


def main() -> int:
    TARGET.parent.mkdir(parents=True, exist_ok=True)
    # No real detector is constructed: the schema comes from the models, and
    # building one here would load the model for nothing.
    schema = create_app(cast(Detector, object())).openapi()
    TARGET.write_text(json.dumps(schema, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {TARGET}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
