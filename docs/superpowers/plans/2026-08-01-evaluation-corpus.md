# Evaluation Corpus & Metrics Implementation Plan

**Goal:** the public half of the Wave 0 evaluation toolkit — a Faker-based synthetic
FR/DE corpus generator, a gold-annotation format shared with future implementations
(REQ-44), a metrics runner (precision/recall/F1 per type, Tier 1 recall separately),
and `make evaluate` reproducible by anyone.

**Traceability:** REQ-37 (corpus in CI), REQ-38 (target metrics), REQ-5 (FR/DE plus
code-switching section), MVP roadmap "Evaluation Toolkit". The manually annotated
private part stays outside the repository forever.

## Layout

```
evaluation/
  generate.py       synthetic corpus generator (Faker fr_FR/de_DE/de_CH, seeded)
  evaluate.py       runner: corpus JSONL -> per-type P/R/F1 + tier-1 recall report
  corpus/public.jsonl   generated, committed for reproducibility diffing
detector/tests/test_evaluation.py   unit tests for matching/metrics
Makefile            evaluate target at repo root
```

## Gold format (one JSON object per line)

```json
{"id": "fr-0001", "lang": "fr", "text": "...", "entities": [
  {"entity_type": "IBAN", "start": 12, "end": 39}
]}
```

Spans in original-text character offsets — the same convention as detector output.

## Design decisions

- **Deterministic generation:** fixed seed; the committed corpus regenerates
  byte-identical (`make corpus` + git diff clean).
- **Matching:** a gold entity counts as found when a predicted span of the same type
  overlaps it by IoU >= 0.5 (partial-credit debates deferred; strict-overlap flag).
- **Templates carry code-switching:** a dedicated section mixes FR/DE in one text
  (REQ-5 acceptance) using checksum-valid identifiers generated via stdnum/schwifty
  helpers, not Faker's format-only providers.
- **Report:** per-type precision/recall/F1 + aggregate Tier 1 recall; exits non-zero
  when Tier 1 recall drops below 0.99 (REQ-38) so CI can gate on it.

## Tasks (TDD)

1. `test_evaluation.py` (fail) -> `evaluate.py`: span matching (exact, overlap, type
   mismatch, double-count guards), metric math on a toy corpus, tier-1 gate.
2. `generate.py`: template pools per language + identifier injectors; seeded run
   emits `corpus/public.jsonl`; determinism test (two runs identical).
3. Run detector over the corpus, fix template bugs until Tier 1 recall = 1.0 on
   synthetic data (it must be - the corpus only contains catalog-covered types).
4. `Makefile` (`corpus`, `evaluate`), CI step, README metrics table.

## Verification

`make evaluate` from a clean checkout; CI green with the new gate.
