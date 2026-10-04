"""The digest that says which detector produced a measurement.

`check_published_metrics.py` already refuses a measurement taken against a
different corpus or a different pinned model revision. Neither covers the case
Codex raised on #98: an unchanged corpus and an unchanged revision, with an
edited threshold or an edited rule. A `metrics.json` from before that edit still
satisfied both checks, so the gate could pass on numbers the current detector
does not produce.

**This is a walk rather than a list, on purpose.** Enumerating "the files that
affect detection" creates a list that drifts away from the code — the thresholds
live in `catalog/ner.yaml` today, and the next one may not. Everything under the
package is covered instead, so the set cannot be under-inclusive, which is the
only failure that matters here: an over-inclusive digest costs a re-measurement,
an under-inclusive one passes a stale number.

`evaluation/evaluate.py` is included because it computes the published figures.
This module is not: it decides how provenance is recorded, not what the numbers
are. What keeps *it* honest is a test asserting the digest moves when a
threshold moves.

Written to be importable with nothing installed, because
`check_published_metrics.py` runs under plain `python3` like
`check_layers.py` does.
"""

import hashlib
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "detector" / "src" / "tessera_detector"
EVALUATOR = ROOT / "evaluation" / "evaluate.py"


def source_paths() -> list[pathlib.Path]:
    paths = [
        path
        for path in PACKAGE.rglob("*")
        if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc"
    ]
    return [*sorted(paths), EVALUATOR]


def source_digest() -> str:
    digest = hashlib.sha256()
    for path in source_paths():
        digest.update(path.relative_to(ROOT).as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


if __name__ == "__main__":
    for path in source_paths():
        print(path.relative_to(ROOT).as_posix())
    print(source_digest())
