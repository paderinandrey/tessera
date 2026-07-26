# Detector Core Implementation Plan

**Goal:** Wave 0 foundation — the shared span schema, offset-safe normalization, and the
deterministic (Tier 1) detection layer with checksum validation, driven by a YAML
identifier catalog.

**Traceability (Notion requirements DB):** REQ-2 (checksum catalog), REQ-6 (offsets in
original coordinates), REQ-8 (groundwork: checksum spans are untouchable), REQ-44
(shared span schema — single source of truth file), catalog-as-data lever from the MVP
roadmap.

**Non-goals for this PR:** NER layer, context boosting, conflict resolution, CLI report,
FastAPI service, gateway. Each lands in its own PR on top of this foundation.

## File structure

```
detector/
  pyproject.toml                     uv project, Python >=3.14, latest deps
  src/tessera_detector/
    __init__.py
    spans.py                         Span schema (REQ-44) — single source of truth
    normalize.py                     NFKC-per-char normalization + offset map (REQ-6)
    validators.py                    checksum validators (python-stdnum / schwifty)
    catalog/identifiers.yaml         identifier catalog as data (REQ-2)
    deterministic.py                 deterministic layer: catalog regexes + validation
  tests/
    test_spans.py
    test_normalize.py
    test_validators.py
    test_deterministic.py
.github/workflows/ci.yml             pytest + ruff + mypy on every PR
```

## Key decisions

- **Invalid checksum ⇒ no span at all** (REQ-2 acceptance): the validator drops the
  candidate instead of lowering confidence — "11 digits" types would otherwise flood
  false positives.
- **Spans always point into the original text** (REQ-6): normalization (NFKC, NBSP and
  narrow-NBSP → space, Unicode hyphens → `-`) is per-character with an explicit
  normalized→original offset map, so multi-char expansions (ligatures) stay mappable.
- **Catalog is data:** a contributor adds an identifier by appending a YAML entry with a
  pattern and naming a validator — no engine changes.
- **Checksum-passed spans get confidence 1.0** and `tier: 1`; conflict resolution
  (REQ-8) will treat them as untouchable in a later PR.
- **Synthetic test fixtures only:** checksum-valid but non-attributable numbers (stdnum
  documentation examples, test card numbers).

## Tasks (TDD: red → green per module)

1. `pyproject.toml` + empty package; `uv sync` succeeds.
2. `test_spans.py` (fail) → `spans.py`: `Span{entity_type, start, end, confidence,
   recognizer, tier, boosted}` with validation (end > start ≥ 0, 0 ≤ confidence ≤ 1).
3. `test_normalize.py` (fail) → `normalize.py`: `NormalizedText.text`,
   `.to_original(start, end)`; cases: ASCII identity, NBSP inside IBAN, U+2011 hyphen in
   a name, ligature `ﬁ` length change, narrow NBSP (French number grouping).
4. `test_validators.py` (fail) → `validators.py`: iban, luhn card, ch_avs, fr_nir,
   de_idnr — valid and invalid (checksum-broken) examples each.
5. `test_deterministic.py` (fail) → `deterministic.py` + `catalog/identifiers.yaml`:
   detects IBAN with NBSP in original coordinates; broken checksum produces nothing;
   multiple entities in FR/DE mixed text; span fields populated from catalog.
6. `ruff check` + `mypy src` clean; CI workflow; commit(s) with DCO sign-off.

## Verification

`cd detector && uv sync && uv run pytest && uv run ruff check . && uv run mypy src`
