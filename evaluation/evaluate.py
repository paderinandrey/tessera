"""Run the detector over the public corpus and report per-type metrics (REQ-38).

Exits non-zero when Tier 1 recall drops below the target so CI can gate on it.

Run from the repository root:  uv run --project detector python evaluation/evaluate.py
Pass --require-ner where the NER layer is provisioned: without it a broken
runtime would skip the NER gates and still report success.
"""

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path

from tessera_detector.evaluation import (
    EvalEntity,
    evaluate_document,
    group_coverage,
    overmasking_counts,
    precision_gate_failures,
    summarize,
    unmasked_words,
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
# REQ-38 targets 0.8 precision on ORG and LOCATION as an irritation metric —
# below it, clients complain the service ruins their text. The gate measures
# over-masking rather than strict per-type precision: every LOCATION false
# positive on this corpus is a French surname that is also a place name
# (Lenoir, Fontaine, Mercier), marking the very span the gold calls PERSON.
# That span is redacted either way, so it irritates nobody; what irritates is
# masking text that holds no personal data. Both strict numbers stay in the
# table. ORG remains advisory: at 0.208 its over-masking is real, not a
# labelling disagreement ("Le laboratoire", "service juridique").
BINDING_OVERMASKING_TYPES = {"LOCATION"}
ADVISORY_PRECISION_TYPES = {"ORG", "LOCATION"}
# Article 9 (REQ-3) is gated on coverage, not per-category recall: the model
# reads "maghrébine" as religion rather than ethnicity, and that span is still
# redacted. What must never happen is a special-category mention going
# unnoticed by every one of the eight labels.
ARTICLE_9_TARGET = 0.95
# Annotated entities this configuration is known to leave partly unmasked, each
# with the words it leaves and the reason it leaves them.
#
# **A named allowlist rather than a count, because a count cancels.** A bound of
# "no more than three" lets a fixed leak pay for a new one: `Tessier SA` starts
# being detected, some other name stops, the total holds and CI is green. That
# is the same cross-entity cancellation the joined-detection gate spent three
# revisions removing, and it would have been reintroduced here in the same pull
# request. Raised in review.
#
# **The words are part of the key**, so a shortfall that grows is a new fact and
# not a matching entry: if `un diabète de type 2` begins leaving `diabète` too,
# this stops matching and the gate fails.
#
# **Five of the eight are an annotation convention and three are real.** The
# convention is the gold span including a leading article the detector does not
# predict — `un diabète de type 2` masked as `diabète de type 2` — and `un` is
# not personal data, the same argument the `PERSON` trimming rule makes for
# `Der Kunde`. An earlier version encoded that as a list of article words
# dropped from *any* position under *any* type, which reports a `PERSON`
# annotated `Le Thi Mai` as masked when only `Thi Mai` is predicted — and `Le`
# is a Vietnamese family name that `ner.py` protects by name. Naming the
# entities instead means each forgiveness is a sentence somebody wrote, and
# `Le Thi Mai` would simply not be in this table.
KNOWN_UNMASKED: dict[tuple[str, str], tuple[frozenset[str], str]] = {
    ("HEALTH", "un diabète de type 2"): (
        frozenset({"un"}),
        "the gold includes the article; the condition is masked",
    ),
    ("HEALTH", "une sclérose en plaques"): (
        frozenset({"une"}),
        "the gold includes the article; the condition is masked",
    ),
    ("HEALTH", "eine Hepatitis-B-Infektion"): (
        frozenset({"eine"}),
        "the gold includes the article; the condition is masked",
    ),
    ("SEX_LIFE", "une interruption de grossesse"): (
        frozenset({"une"}),
        "the gold includes the article; the mention is masked as HEALTH",
    ),
    ("SEX_LIFE", "eine Kinderwunschbehandlung"): (
        frozenset({"eine"}),
        "the gold includes the article; the mention is masked as HEALTH",
    ),
    ("ORG", "Tessier SA"): (
        frozenset({"Tessier", "SA"}),
        "organization 0.697 against ORG's bar of 0.75 — a near miss on its own label",
    ),
    ("PERSON", "Texier"): (
        frozenset({"Texier"}),
        "claimed by `location` at 0.585, whose bar is 0.7; asked alone, `person` "
        "scores it 0.704 — issue #46",
    ),
    ("GENETIC", "test génétique"): (
        frozenset({"test", "génétique"}),
        "genetic data 0.288 against a bar of 0.30 — a near miss by twelve thousandths",
    ),
}

ARTICLE_9_TYPES = {
    "HEALTH",
    "BIOMETRIC",
    "GENETIC",
    "ETHNICITY",
    "POLITICAL_AFFILIATION",
    "POLITICAL_OPINION",
    "RELIGION",
    "TRADE_UNION",
    "SEXUAL_ORIENTATION",
    "PHILOSOPHICAL_BELIEF",
    "SEX_LIFE",
}


def unmasked_entities(
    text: str, entities: list[EvalEntity], predictions: list
) -> list[tuple[str, str, frozenset[str]]]:
    """Annotated entities with words no prediction covers completely.

    `unmasked_words` is in the package rather than here because it has three
    callers — this gate, the joined-detection gate, and the tests that pin both.
    A second copy would be two definitions of what counts as a leak.
    """
    leaked = []
    for entity in entities:
        carrying = unmasked_words(text, entity.start, entity.end, predictions)
        if carrying:
            leaked.append(
                (entity.entity_type, text[entity.start : entity.end], frozenset(carrying))
            )
    return leaked


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
    # Bucketed by (language, category): a pooled ratio lets a category go dark
    # in one language while the aggregate stays above target.
    article_9_buckets: dict[tuple[str, str], list[int]] = defaultdict(lambda: [0, 0])
    overmasking: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    unmasked: list[tuple[str, str]] = []
    for line in CORPUS.read_text(encoding="utf-8").splitlines():
        document = json.loads(line)
        entities = [EvalEntity(**e) for e in document["entities"]]
        predictions = detector.detect(document["text"])
        per_document.append(evaluate_document(entities, predictions))
        for entity_type, covered in group_coverage(
            entities, predictions, types=ARTICLE_9_TYPES
        ):
            bucket = article_9_buckets[(document["lang"], entity_type)]
            bucket[0] += int(covered)
            bucket[1] += 1
        for entity_type, (kept, total) in overmasking_counts(
            entities, predictions, types=BINDING_OVERMASKING_TYPES
        ).items():
            counts = overmasking[entity_type]
            counts[0] += kept
            counts[1] += total
        unmasked.extend(unmasked_entities(document["text"], entities, predictions))
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
    if article_9_present and detector.ner_available:
        print("\nArticle 9 special categories")
        _rows(article_9_present)
    elif article_9_present:
        # Without the model these rows are all-zero gold false negatives, which
        # reads as "evaluated and found nothing" rather than "never ran".
        print("\nArticle 9 special categories: not evaluated (NER layer off)")
    print(f"\nTier 1 recall: {summary.tier1_recall:.4f} (target >= {TIER1_TARGET})")
    if summary.tier1_recall < TIER1_TARGET:
        print("FAIL: Tier 1 recall below target", file=sys.stderr)
        return 1
    if not detector.ner_available:
        print(
            f"NER layer off ({detector.ner_off_reason}): the Article 9 coverage "
            "and LOCATION over-masking gates are skipped."
        )
        return 0
    # Before the type-shaped gates, because it is the one that asks what the
    # gateway is for. Every entry is checked against `KNOWN_UNMASKED` by
    # identity rather than counted: a count lets a fixed leak pay for a new one.
    # A known entry that stops appearing is an improvement and passes silently;
    # anything else fails and says what it is.
    # Occurrences and distinct entities both, because the two numbers answer
    # different questions and quoting one where the other is meant is how a
    # README and a gate come to disagree: `eine Hepatitis-B-Infektion` appears
    # twice in the corpus and is one entry in the table.
    distinct = {(entity_type, value) for entity_type, value, _ in unmasked}
    print(
        f"\nAnnotated entities with words reaching the provider: {len(unmasked)} "
        f"occurrences of {len(distinct)} entities"
    )
    surprises = []
    for entity_type, value, words in sorted(unmasked):
        known = KNOWN_UNMASKED.get((entity_type, value))
        if known is not None and known[0] == words:
            print(f"  {entity_type} {value!r}: {sorted(words)} — {known[1]}")
        else:
            surprises.append((entity_type, value, words, known))
    for entity_type, value, words, known in surprises:
        if known is None:
            print(
                f"FAIL: {entity_type} {value!r} reaches the provider as {sorted(words)} "
                f"and is not a tracked defect",
                file=sys.stderr,
            )
        else:
            print(
                f"FAIL: {entity_type} {value!r} reaches the provider as {sorted(words)}, "
                f"where {sorted(known[0])} is recorded",
                file=sys.stderr,
            )
    unmasked_over = bool(surprises)

    covered_total = sum(bucket[0] for bucket in article_9_buckets.values())
    gold_total = sum(bucket[1] for bucket in article_9_buckets.values())
    overall = covered_total / gold_total if gold_total else 0.0
    print(
        f"Article 9 coverage: {overall:.4f} ({covered_total}/{gold_total}, "
        f"target >= {ARTICLE_9_TARGET} overall, and no blank language/category)"
    )
    # Two gates, because one cannot do both jobs. The pooled ratio is the
    # quality bar; it cannot see a category going dark in a single language.
    # The per-bucket check catches exactly that, and asks only for a pulse:
    # buckets hold a handful of spans, where a 0.95 ratio is unmeasurable.
    dark = [
        (language, entity_type, bucket)
        for (language, entity_type), bucket in sorted(article_9_buckets.items())
        if bucket[0] == 0
    ]
    for language, entity_type, bucket in dark:
        print(
            f"FAIL: Article 9 coverage for {entity_type} in {language} is "
            f"0/{bucket[1]} — the category is not detected in that language at all",
            file=sys.stderr,
        )
    if overall < ARTICLE_9_TARGET:
        print(
            f"FAIL: Article 9 coverage {overall:.4f} below target {ARTICLE_9_TARGET}",
            file=sys.stderr,
        )
    article_9_missed = bool(dark) or overall < ARTICLE_9_TARGET
    overmasking_failures = []
    for entity_type, (kept, total) in sorted(overmasking.items()):
        rate = kept / total if total else 1.0
        print(
            f"{entity_type} over-masking precision: {rate:.4f} ({kept}/{total} predictions "
            f"land on real data, target >= {PRECISION_TARGET})"
        )
        if total and rate < PRECISION_TARGET:
            overmasking_failures.append(entity_type)
            print(
                f"FAIL: {entity_type} over-masking precision {rate:.4f} below "
                f"target {PRECISION_TARGET}",
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
    return 1 if overmasking_failures or article_9_missed or unmasked_over else 0


if __name__ == "__main__":
    raise SystemExit(main())
