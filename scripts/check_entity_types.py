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
    # A comment explaining a rename or removal naturally quotes the old name
    # (e.g. `// dropped "OLD_TYPE" — folded into ...`). Matching NAME against
    # the raw span would count that mention as a declaration and the check
    # would wave through exactly the drift it exists to catch. No entity type
    # contains "//" — the grammar is letters and underscores — so stripping
    # each line's comment first is safe here.
    without_comments = "\n".join(
        line.split("//", 1)[0] for line in match.group(1).splitlines()
    )
    return set(NAME.findall(without_comments))


def detector_types() -> set[str]:
    found: set[str] = set()
    for path in CATALOGS:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        # Both catalogs use these two top-level keys today; a third section
        # would be silently invisible here, on both sides of the comparison.
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
