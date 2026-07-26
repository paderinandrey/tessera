# Span Conflict Resolution Implementation Plan (REQ-8)

**Goal:** deterministic, documented resolution of overlapping spans, wired into the
detector so `detect()` returns a conflict-free span set. Groundwork for the NER layer:
the same rules will arbitrate checksum vs model spans.

**Traceability:** REQ-8 (documented conflict rules, regression tests on overlaps,
decision trace), REQ-44 (spans stay within the shared schema).

## Rules (in precedence order, from the requirement card)

1. **Checksum spans are untouchable** — a span from the checksum layer is never
   silently dropped in favour of a non-checksum span.
2. **Same type overlapping/nested → union** (max confidence, boosted OR-ed).
3. **Different types, strict containment → outer wins**; exception per rule 1: an
   untouchable inner inside a non-untouchable outer merges to the union bounds and
   takes the more sensitive type, so the identifier keeps its Tier 1 identity in audit.
4. **Different types, partial overlap or equal range →** higher catalog `specificity`
   wins; tie → higher confidence wins; full tie → union with the more sensitive type
   (lower tier).

Every applied rule appends a trace record (kept span, dropped spans, rule name) —
the future sandbox surfaces these (REQ-8 acceptance).

## File structure

```
detector/src/tessera_detector/resolution.py    resolve() + Decision/Resolution types
detector/src/tessera_detector/catalog/identifiers.yaml   + specificity per entry
detector/src/tessera_detector/deterministic.py wire resolve() into detect()
detector/tests/test_resolution.py              unit tests per rule
detector/tests/test_deterministic.py           integration: NIR+Luhn double-validity
```

Specificity is catalog data (generic digit-run rules rank below structured national
identifiers): iban 90, ch_avs/fr_nir/de_steuer_id 80, credit_card 40.

## Tasks (TDD)

1. `test_resolution.py` (fail) → `resolution.py`: dedupe, same-type union,
   containment, untouchable-inner merge, specificity/confidence/tie arbitration,
   trace records, non-overlapping passthrough.
2. Integration test (fail): compact `295100000000754` is simultaneously a valid NIR
   and a Luhn-valid 15-digit run — `detect()` must emit exactly one FR_NIR span.
3. Wire `resolve()` into `DeterministicDetector.detect()`; catalog gets `specificity`.
4. ruff + mypy clean; commit with DCO sign-off.

## Verification

`cd detector && uv run pytest && uv run ruff check . && uv run mypy src`
