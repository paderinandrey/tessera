# NER Layer — Design

**Goal:** the Tier 2 detection layer from the MVP roadmap — a GLiNER model on
onnxruntime contributing PERSON, LOCATION and ORG spans alongside the deterministic
catalog, with one shared conflict resolution over both layers.

**Traceability:** REQ-1 (multi-layer detection: deterministic, NER, optional LLM),
REQ-4 (indirect and quasi-identifiers), REQ-38 (precision on ORG and LOCATION ≥ 0.8),
REQ-6/48 (offsets in original-text coordinates), MVP roadmap Release 1.

## Decisions made during brainstorming

- Scope is PERSON / LOCATION / ORG. GDPR Article 9 special categories (REQ-3) are a
  separate iteration: with a zero-shot model they are a label-list change, but they
  need their own corpus and taxonomy work.
- Model weights are downloaded to a local cache, never committed. Tests that need the
  model are marked and skipped when it is absent; the fast CI job stays as it is and a
  separate cached job runs the model-backed tests.
- The NER layer is on automatically when the model is present, off (with a note in the
  report) when it is not.

## Runtime choice

GLiNER on onnxruntime, per the MVP roadmap. Zero-shot labelling is the lever: Article 9
and community-contributed entity types become configuration rather than a new engine.
Its dependencies are heavy, so they live in an optional `ner` dependency group, the way
`eval` already works — the base install stays light.

Rejected: spaCy per-language models (two models for two languages, weaker, and no
zero-shot path for Article 9) and Presidio (brings its own recognizer registry and
conflict resolution, duplicating the catalog and `resolve()` we already have).

## Architecture

```
detector/src/tessera_detector/
  pipeline.py    Detector: runs both layers, resolves the union once
  ner.py         NerRecognizer protocol + GlinerRecognizer (ONNX), chunking
  models.py      weight lookup: env var, cache path, availability check
  catalog/ner.yaml   entity types: model label, threshold, tier, specificity
```

### Composition and the resolution refactor

`DeterministicDetector.detect()` currently calls `resolve()` itself. With a second layer
that is wrong: an NER span must be able to lose to an overlapping checksum span, which
only works if resolution sees both layers at once.

- `DeterministicDetector.detect()` returns **unresolved** spans.
- `Detector.detect(text)` collects deterministic spans plus NER spans (when the model is
  loaded) and calls `resolve()` once over the union, with a specificity map merged from
  the identifier catalog and `ner.yaml`.
- Existing tests that assert resolution behaviour move to the pipeline tests.
- `cli.py` and `evaluation/evaluate.py` switch from `DeterministicDetector` to `Detector`.

`Detector` exposes `ner_available: bool` so callers can report which layers ran.

### Offsets

The deterministic layer runs on normalized text and maps offsets back through
`normalize()`. The NER layer runs on the **original text**, so model offsets are used
directly — no mapping, no normalization. This keeps every span in original-text
coordinates (REQ-6/48).

Texts longer than the model's token window are split on paragraph boundaries with a
character overlap; each chunk's offsets are shifted by its start, and duplicate spans at
the seams are collapsed by the same `resolve()` pass that handles cross-layer conflicts.

### Configuration

`catalog/ner.yaml` lists each entity type with its model label, threshold, tier and
specificity. Validation mirrors the identifier catalog's strictness: thresholds are
explicit and range-checked, no silent defaults. Model scores below the type's threshold
never become spans.

### Weights

Lookup order: `TESSERA_NER_MODEL` environment variable, then the cache directory
`~/.cache/tessera/models/<model-name>`. A `make model` target downloads them. When no
weights are found, `Detector` runs the deterministic layer alone and reports
`ner_available = False`; nothing raises.

## CLI and evaluation

- `tessera scan` enables NER automatically when weights are present. `--no-ner` disables
  it; `--ner` requires it and exits 2 with a diagnostic when weights are missing. When
  the layer did not run, the report says so — a quiet deterministic-only scan must not
  read as a complete one.
- `make evaluate` reports NER types alongside the catalog types. The Tier 1 recall gate
  stays. A precision gate for ORG and LOCATION (≥ 0.8, REQ-38) applies only when the
  model is available; otherwise it prints an explicit skip so the fast CI stays green.

## Corpus

The generator currently inserts `{person}` and `{city}` into text without annotating
them, and has no organizations at all — so ORG and LOCATION precision cannot be measured
today. This iteration annotates `{person}` as PERSON and `{city}` as LOCATION, adds
templates using `faker.company()` for ORG, and regenerates the corpus.

The README states plainly that synthetic slot-filled entities inflate NER metrics: the
model sees names and cities in predictable positions. The privately annotated corpus
remains the honest signal, reported separately.

## Testing

Most of the layer is tested without weights, through a fake recognizer:

- union resolution across layers, including a checksum span beating an overlapping PERSON
- per-type thresholds and specificity
- `ner_available` false path: deterministic-only results, report note
- chunk offset arithmetic on synthetic chunk boundaries

Model-backed tests carry a `ner` pytest marker and skip when weights are absent: FR/DE
smoke detection, chunking over a long document, exact offsets on accented text.

CI keeps the current fast job unchanged and adds a separate job that caches the weights
and runs the marked tests plus the NER metrics.

## Out of scope

GDPR Article 9 categories (REQ-3), the p95 latency measurement (REQ-38 — needs its own
benchmark harness), per-tenant thresholds (REQ-9), and the optional LLM pass (REQ-1).
