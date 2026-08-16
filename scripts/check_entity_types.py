"""Fail when the gateway's entity-type vocabulary and the detector's catalogs
disagree.

The gateway holds its own copy on purpose — asking the detector which types it
emits would be worthless against a detector that lies. The copy is only safe
while something notices it drifting, and that is this script.
"""

import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAPPING = ROOT / "gateway" / "src" / "mapping.rs"
CATALOGS = (
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "identifiers.yaml",
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "ner.yaml",
)

DECLARATION = re.compile(
    r"pub const ENTITY_TYPES: \[&str; \d+\] = \[(.*?)\];", re.DOTALL
)
NAME = re.compile(r'"([A-Z_]+)"')


def gateway_types() -> set[str]:
    match = DECLARATION.search(MAPPING.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{MAPPING}: no ENTITY_TYPES declaration found")
    return set(NAME.findall(match.group(1)))


def detector_types() -> set[str]:
    found: set[str] = set()
    for path in CATALOGS:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        for section in ("identifiers", "entities"):
            for entry in document.get(section) or ():
                found.add(entry["entity_type"])
    return found


def main() -> None:
    gateway, detector = gateway_types(), detector_types()
    if gateway == detector:
        print(f"entity types agree: {len(gateway)} declared on both sides")
        return

    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"declared by the detector, absent from the gateway: {missing}")
    if extra:
        print(f"declared by the gateway, absent from the detector: {extra}")
    sys.exit(
        "entity types have drifted; the gateway would mask the difference as "
        "REDACTED without saying why"
    )


if __name__ == "__main__":
    main()
