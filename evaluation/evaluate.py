"""Run the detector over the public corpus and report per-type metrics (REQ-38).

Exits non-zero when Tier 1 recall drops below the target so CI can gate on it.

Run from the repository root:  uv run --project detector python evaluation/evaluate.py
Pass --require-ner where the NER layer is provisioned: without it a broken
runtime would skip the NER gates and still report success.
"""

import argparse
import json
import sys
from pathlib import Path

from tessera_detector.evaluation import (
    EvalEntity,
    evaluate_document,
    group_recall,
    precision_gate_failures,
    summarize,
)
from tessera_detector.models import ModelUnavailable
from tessera_detector.pipeline import build_detector

CORPUS = Path(__file__).parent / "corpus" / "public.jsonl"
TIER1_TARGET = 0.99
PRECISION_TARGET = 0.8
# Both are REQ-38 targets, but only LOCATION is enforceable on the synthetic
# corpus: an ORG here is a Faker company name in a fixed slot, while the model
# also finds the institutions real prose is full of, which the gold cannot
# enumerate. ORG is reported and warned about until the private corpus can
# judge it.
BINDING_PRECISION_TYPES = {"LOCATION"}
ADVISORY_PRECISION_TYPES = {"ORG"}
# Article 9 (REQ-3) is gated on coverage, not per-category recall: the model
# reads "maghrébine" as religion rather than ethnicity, and that span is still
# redacted. What must never happen is a special-category mention going
# unnoticed by every one of the eight labels.
ARTICLE_9_TARGET = 0.95
ARTICLE_9_TYPES = {
    "HEALTH",
    "BIOMETRIC",
    "GENETIC",
    "ETHNICITY",
    "POLITICAL_OPINION",
    "RELIGION",
    "TRADE_UNION",
    "SEXUAL_ORIENTATION",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--require-ner",
        action="store_true",
        help="fail instead of skipping the NER gates when the layer cannot run",
    )
    args = parser.parse_args(argv)
    try:
        detector = build_detector(ner=True if args.require_ner else None)
    except (ModelUnavailable, ValueError) as error:
        print(f"FAIL: --require-ner but the layer cannot run: {error}", file=sys.stderr)
        return 1
    tier1_types = {rule.entity_type for rule in detector.deterministic.rules if rule.tier == 1}
    per_document = []
    article_9_covered = article_9_total = 0
    for line in CORPUS.read_text(encoding="utf-8").splitlines():
        document = json.loads(line)
        entities = [EvalEntity(**e) for e in document["entities"]]
        predictions = detector.detect(document["text"])
        per_document.append(evaluate_document(entities, predictions))
        covered, total = group_recall(entities, predictions, types=ARTICLE_9_TYPES)
        article_9_covered += covered
        article_9_total += total
    summary = summarize(per_document, tier1_types=tier1_types)

    width = max(len(t) for t in summary.per_type)
    print(f"{'type'.ljust(width)}  prec   rec    f1     tp   fp   fn")
    def _rows(types: list[str]) -> None:
        for entity_type in types:
            m = summary.per_type[entity_type]
            print(
                f"{entity_type.ljust(width)}  {m.precision:.3f}  {m.recall:.3f}  {m.f1:.3f}"
                f"  {m.tp:4d} {m.fp:4d} {m.fn:4d}"
            )

    _rows(sorted(t for t in summary.per_type if t not in ARTICLE_9_TYPES))
    article_9_present = sorted(t for t in summary.per_type if t in ARTICLE_9_TYPES)
    if article_9_present:
        print("\nArticle 9 special categories")
        _rows(article_9_present)
    print(f"\nTier 1 recall: {summary.tier1_recall:.4f} (target >= {TIER1_TARGET})")
    if summary.tier1_recall < TIER1_TARGET:
        print("FAIL: Tier 1 recall below target", file=sys.stderr)
        return 1
    if not detector.ner_available:
        print(
            f"NER layer off ({detector.ner_off_reason}): "
            "the LOCATION precision gate is skipped."
        )
        return 0
    article_9_recall = article_9_covered / article_9_total if article_9_total else 0.0
    print(
        f"Article 9 coverage: {article_9_recall:.4f} "
        f"({article_9_covered}/{article_9_total}, target >= {ARTICLE_9_TARGET})"
    )
    article_9_missed = article_9_total and article_9_recall < ARTICLE_9_TARGET
    if article_9_missed:
        print(
            f"FAIL: Article 9 coverage {article_9_recall:.4f} below target {ARTICLE_9_TARGET}",
            file=sys.stderr,
        )
    advisory = precision_gate_failures(
        summary.per_type, types=ADVISORY_PRECISION_TYPES, target=PRECISION_TARGET
    )
    for entity_type, precision in advisory:
        print(
            f"WARN: {entity_type} precision {precision:.4f} below target {PRECISION_TARGET} "
            "(advisory on the synthetic corpus)"
        )
    failures = precision_gate_failures(
        summary.per_type, types=BINDING_PRECISION_TYPES, target=PRECISION_TARGET
    )
    for entity_type, precision in failures:
        print(
            f"FAIL: {entity_type} precision {precision:.4f} below target {PRECISION_TARGET}",
            file=sys.stderr,
        )
    return 1 if failures or article_9_missed else 0


if __name__ == "__main__":
    raise SystemExit(main())
