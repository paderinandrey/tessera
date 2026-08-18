"""Fail when the gateway's layer list and the detector's Layer type disagree.

The gateway decides whether a detection may be cached by asking whether every
layer ran. It holds its own copy of "every layer" for the same reason it holds
its own entity-type vocabulary: asking the detector which layers make a run
complete would be worthless against a detector that answers "the ones I ran".
The copy is only safe while something notices it drifting, and that is this
script.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DETECTOR_RS = ROOT / "gateway" / "src" / "detector.rs"
API_PY = ROOT / "detector" / "src" / "tessera_detector" / "api.py"

GATEWAY = re.compile(r'pub const LAYERS: \[&str; (\d+)\] = \[(.*?)\];', re.DOTALL)
DETECTOR = re.compile(r'^Layer = Literal\[(.*?)\]$', re.MULTILINE | re.DOTALL)
QUOTED = re.compile(r'"([^"]*)"')


def gateway_layers() -> set[str]:
    match = GATEWAY.search(DETECTOR_RS.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{DETECTOR_RS}: no LAYERS declaration found")
    declared, body = int(match.group(1)), match.group(2)
    names = QUOTED.findall(body)
    if len(names) != declared:
        sys.exit(
            f"{DETECTOR_RS}: LAYERS says {declared} entries, {len(names)} parsed"
        )
    return set(names)


def detector_layers() -> set[str]:
    match = DETECTOR.search(API_PY.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{API_PY}: no Layer alias found")
    return set(QUOTED.findall(match.group(1)))


def main() -> None:
    gateway, detector = gateway_layers(), detector_layers()
    if gateway == detector:
        print(f"layers agree: {sorted(gateway)}")
        return
    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"run by the detector, absent from the gateway: {missing}")
        print("  a run using this layer would be treated as complete without it")
    if extra:
        print(f"expected by the gateway, absent from the detector: {extra}")
        print("  no run can ever look complete, so nothing will be cached")
    sys.exit(1)


if __name__ == "__main__":
    main()
