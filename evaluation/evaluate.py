"""Run the detector over the public corpus and report per-type metrics (REQ-38).

Exits non-zero when Tier 1 recall drops below the target so CI can gate on it.

Run from the repository root:  uv run --project detector python evaluation/evaluate.py
"""

import json
import sys
from pathlib import Path

from tessera_detector.evaluation import EvalEntity, evaluate_document, summarize
from tessera_detector.pipeline import Detector

CORPUS = Path(__file__).parent / "corpus" / "public.jsonl"
TIER1_TARGET = 0.99


def main() -> int:
    detector = Detector()
    tier1_types = {rule.entity_type for rule in detector.deterministic.rules if rule.tier == 1}
    per_document = []
    for line in CORPUS.read_text(encoding="utf-8").splitlines():
        document = json.loads(line)
        entities = [EvalEntity(**e) for e in document["entities"]]
        predictions = detector.detect(document["text"])
        per_document.append(evaluate_document(entities, predictions))
    summary = summarize(per_document, tier1_types=tier1_types)

    width = max(len(t) for t in summary.per_type)
    print(f"{'type'.ljust(width)}  prec   rec    f1     tp   fp   fn")
    for entity_type in sorted(summary.per_type):
        m = summary.per_type[entity_type]
        print(
            f"{entity_type.ljust(width)}  {m.precision:.3f}  {m.recall:.3f}  {m.f1:.3f}"
            f"  {m.tp:4d} {m.fp:4d} {m.fn:4d}"
        )
    print(f"\nTier 1 recall: {summary.tier1_recall:.4f} (target >= {TIER1_TARGET})")
    if summary.tier1_recall < TIER1_TARGET:
        print("FAIL: Tier 1 recall below target", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
