# GDPR Article 9 Categories Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the eight GDPR Article 9 special categories detected through the existing zero-shot NER layer at threshold 0.30, with a recall gate of their own.

**Architecture:** Configuration first — the categories are eight new entries in `catalog/ner.yaml`, riding the inference pass that already exists. The only code changes are a recall-gate helper beside the existing precision one, its wiring into the metrics runner, and corpus templates that give the gate something to measure.

**Tech Stack:** Python 3.14, the existing GLiNER/onnxruntime adapter, PyYAML config, Faker corpus generator. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-02-art9-categories-design.md`

## Global Constraints

- Eight entity types: `HEALTH`, `BIOMETRIC`, `GENETIC`, `ETHNICITY`, `POLITICAL_OPINION`, `RELIGION`, `TRADE_UNION`, `SEXUAL_ORIENTATION`. Every one: threshold `0.30`, tier `3`, specificity `35`.
- Specificity 35 is deliberate: above the quasi-identifiers (PERSON 30, LOCATION 20, ORG 10), below every catalog identifier (40–90).
- Article 9 recall gate: `>= 0.95`, binding when the model is available, skipped with an explicit note when it is not. Article 9 precision is reported and never gated — the requirement says false positives are tolerable and misses are not.
- The corpus must gain entity-free templates in the same register as the positive ones, or the reported precision at threshold 0.30 means nothing.
- The seeded corpus must regenerate byte-identically across runs; CI gates on that.
- ruff line-length 100; gates from the repo root: `make test`, `make lint`, `make evaluate`. mypy is strict.
- Commit message style: one-line `Art9: <what>` with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run tests from `detector/`: `uv run pytest tests/test_ner_config.py -v`.

---

### Task 1: The eight categories in configuration

**Files:**
- Modify: `detector/src/tessera_detector/catalog/ner.yaml`
- Modify: `detector/tests/test_ner_config.py`

**Interfaces:**
- Consumes: `load_ner_types()` and `NerType` from `tessera_detector.ner` — unchanged, already validating thresholds, tiers, specificity and the uniqueness of types and labels.
- Produces: the eight configured types. Tasks 2–4 rely on their exact names.

- [ ] **Step 1: Write the failing tests**

Append to `detector/tests/test_ner_config.py`:

```python
ARTICLE_9_TYPES = {
    "HEALTH": "medical condition",
    "BIOMETRIC": "biometric data",
    "GENETIC": "genetic data",
    "ETHNICITY": "ethnic origin",
    "POLITICAL_OPINION": "political affiliation",
    "RELIGION": "religion",
    "TRADE_UNION": "trade union membership",
    "SEXUAL_ORIENTATION": "sexual orientation",
}


def test_article_9_categories_are_configured() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    for entity_type, label in ARTICLE_9_TYPES.items():
        assert entity_type in by_type, f"{entity_type} missing from ner.yaml"
        assert by_type[entity_type].label == label


def test_article_9_uses_the_aggressive_threshold() -> None:
    # REQ-3's acceptance criterion: misses are not tolerable here, so the
    # threshold is far below the quasi-identifiers'.
    by_type = {t.entity_type: t for t in load_ner_types()}
    for entity_type in ARTICLE_9_TYPES:
        assert by_type[entity_type].threshold == 0.30


def test_article_9_sits_in_its_own_tier() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    assert {by_type[t].tier for t in ARTICLE_9_TYPES} == {3}
    assert by_type["PERSON"].tier == 2


def test_article_9_outranks_quasi_identifiers_but_not_identifiers() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    article_9 = {by_type[t].specificity for t in ARTICLE_9_TYPES}
    assert article_9 == {35}
    assert max(by_type[t].specificity for t in ("PERSON", "LOCATION", "ORG")) < 35
    # 40 is the lowest specificity in the identifier catalog.
    assert 35 < 40
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd detector && uv run pytest tests/test_ner_config.py -v`
Expected: the four new tests FAIL with `KeyError`/assertion on the missing types; the existing config tests still pass.

- [ ] **Step 3: Add the categories**

Append to `detector/src/tessera_detector/catalog/ner.yaml`, after the ORG entry:

```yaml
  # GDPR Article 9 special categories (REQ-3). These have no format, only
  # meaning, so the NER layer is the only thing that can find them — and the
  # sanctions behind them make a miss far worse than a false positive. Hence
  # tier 3 of their own and a threshold well below the quasi-identifiers'.
  # Specificity 35 puts them above PERSON/LOCATION/ORG and below every
  # checksum-backed identifier.
  - entity_type: HEALTH
    label: medical condition
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: BIOMETRIC
    label: biometric data
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: GENETIC
    label: genetic data
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: ETHNICITY
    label: ethnic origin
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: POLITICAL_OPINION
    label: political affiliation
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: RELIGION
    label: religion
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: TRADE_UNION
    label: trade union membership
    threshold: 0.30
    tier: 3
    specificity: 35

  - entity_type: SEXUAL_ORIENTATION
    label: sexual orientation
    threshold: 0.30
    tier: 3
    specificity: 35
```

Also update the file's header comment, whose first line currently reads `# Tier 2 NER types (REQ-4).`, to:

```yaml
# NER types: tier 2 quasi-identifiers (REQ-4) and tier 3 Article 9 special
# categories (REQ-3). Labels are the zero-shot prompts handed to the model;
# specificity stays below every catalog identifier (40-90) so a checksum-backed
# identifier wins a partial overlap with a model guess.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd detector && uv run pytest tests/test_ner_config.py -q`, then `uv run ruff check . ../evaluation && uv run mypy src`.
Expected: all pass, lint and mypy clean.

- [ ] **Step 5: Commit**

```bash
git add detector/src/tessera_detector/catalog/ner.yaml detector/tests/test_ner_config.py
git commit -m "Art9: eight special categories at the aggressive threshold

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Resolution ordering against the other layers

**Files:**
- Modify: `detector/tests/test_pipeline.py`

**Interfaces:**
- Consumes: `Detector`, the `FakeRecognizer` already defined at the top of `test_pipeline.py`, and `Span`.
- Produces: no new code — this task pins the ordering Task 1's specificity choice buys, with no weights needed.

- [ ] **Step 1: Write the tests**

Append to `detector/tests/test_pipeline.py`:

```python
def article_9(start: int, end: int, confidence: float = 0.4) -> Span:
    return Span(
        entity_type="TRADE_UNION",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=3,
    )


def org(start: int, end: int, confidence: float = 0.8) -> Span:
    return Span(
        entity_type="ORG",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=2,
    )


def test_article_9_span_outranks_an_overlapping_org() -> None:
    # "Die Gewerkschaft ver.di": knowing it is a union membership mention is
    # the more sensitive reading, and specificity 35 beats ORG's 10.
    text = "Die Gewerkschaft ver.di hat geantwortet."
    recognizer = FakeRecognizer(
        [org(4, 23), article_9(4, 23)],
        specificity={"ORG": 10, "TRADE_UNION": 35},
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [s.entity_type for s in spans] == ["TRADE_UNION"]


def test_checksum_identifier_still_outranks_an_article_9_span() -> None:
    text = "mail: anna.keller@example.ch"
    recognizer = FakeRecognizer(
        [article_9(6, 28, confidence=0.9)], specificity={"TRADE_UNION": 35}
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [(s.entity_type, s.start, s.end) for s in spans] == [("EMAIL", 6, 28)]
```

- [ ] **Step 2: Run them**

Run: `cd detector && uv run pytest tests/test_pipeline.py -v`
Expected: both PASS — the ordering follows from Task 1's configuration and the existing resolution rules. If either fails, the specificity values in `ner.yaml` are wrong, not the test: fix the configuration.

- [ ] **Step 3: Commit**

```bash
git add detector/tests/test_pipeline.py
git commit -m "Art9: pin the resolution ordering against both neighbouring layers

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Corpus templates, positive and clean

**Files:**
- Modify: `evaluation/generate.py`
- Modify: `evaluation/corpus/public.jsonl` (regenerated, committed)
- Modify: `detector/tests/test_corpus.py`

**Interfaces:**
- Produces: gold annotations of the eight Article 9 types in the corpus. Task 4's recall gate reads them.

- [ ] **Step 1: Write the failing test**

Append to `detector/tests/test_corpus.py`:

```python
ARTICLE_9 = {
    "HEALTH",
    "BIOMETRIC",
    "GENETIC",
    "ETHNICITY",
    "POLITICAL_OPINION",
    "RELIGION",
    "TRADE_UNION",
    "SEXUAL_ORIENTATION",
}


def test_corpus_covers_every_article_9_category() -> None:
    found = {e["entity_type"] for doc in _documents() for e in doc["entities"]}
    missing = ARTICLE_9 - found
    assert not missing, f"no gold annotation for {sorted(missing)}"


def test_corpus_keeps_entity_free_documents() -> None:
    # At threshold 0.30 a corpus of positives only would hide the noise the
    # threshold buys, and the reported precision would mean nothing.
    empty = [doc for doc in _documents() if not doc["entities"]]
    assert len(empty) >= 10, f"only {len(empty)} entity-free documents"
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd detector && uv run pytest tests/test_corpus.py -v`
Expected: `test_corpus_covers_every_article_9_category` FAILS listing all eight; the entity-free count may also be short.

- [ ] **Step 3: Add the slot renderer**

In `evaluation/generate.py`, the `render()` function handles the `person`/`city`/`org` slots in an `elif name in ("person", "city", "org"):` branch. Add a second branch immediately after that one — still inside the `if token.startswith("{")` block, before the outer `else: text += token`:

```python
            elif name in ARTICLE_9_SLOTS:
                entity_type, choices = ARTICLE_9_SLOTS[name]
                value = rng.choice(choices)
                entities.append(
                    {"entity_type": entity_type, "start": len(text), "end": len(text) + len(value)}
                )
                text += value
```

and define the slot table above the template lists:

```python
# GDPR Article 9 categories (REQ-3). Values are ordinary vocabulary, not real
# people's data; the corpus never leaves synthetic ground.
ARTICLE_9_SLOTS = {
    "health": ("HEALTH", ["Diabetes", "eine Hepatitis-B-Infektion", "un cancer du sein",
                          "une sclérose en plaques", "Bluthochdruck"]),
    "biometric": ("BIOMETRIC", ["Fingerabdruck", "empreinte digitale",
                                "Gesichtsscan", "reconnaissance faciale"]),
    "genetic": ("GENETIC", ["DNA-Analyse", "test génétique",
                            "Erbgutuntersuchung", "séquençage ADN"]),
    "ethnicity": ("ETHNICITY", ["Roma", "origine maghrébine",
                                "kurdischer Herkunft", "origine sénégalaise"]),
    "political": ("POLITICAL_OPINION", ["Mitglied der Grünen", "militant socialiste",
                                        "CDU-Mitglied", "sympathisant écologiste"]),
    "religion": ("RELIGION", ["katholisch", "de confession musulmane",
                              "jüdischen Glaubens", "protestante"]),
    "union": ("TRADE_UNION", ["Mitglied der IG Metall", "adhérent de la CGT",
                              "ver.di-Mitglied", "syndiqué à la CFDT"]),
    "orientation": ("SEXUAL_ORIENTATION", ["homosexuell", "homosexuel",
                                           "bisexuell", "bisexuelle"]),
}
```

- [ ] **Step 4: Add the templates**

To `FR_TEMPLATES`:

```python
    "Le dossier médical de {person} mentionne {health} depuis 2019.",
    "{person}, {religion}, demande un aménagement d'horaire.",
    "Le salarié {person} est {union} et conteste la sanction.",
    "Note RH : {person} est {political} et a demandé un congé militant.",
    "Le laboratoire a transmis le {genetic} concernant {person}.",
```

To `DE_TEMPLATES`:

```python
    "Der Mitarbeiter {person} leidet an {health} und ist krankgeschrieben.",
    "{person} ist {union} und nimmt an der Betriebsversammlung teil.",
    "Die Personalakte von {person} vermerkt: {religion}, {ethnicity}.",
    "Für den Zugang wurde ein {biometric} von {person} erfasst.",
    "Der Antrag von {person} nennt die Angabe {orientation}.",
```

To the code-switching list `MIXED_TEMPLATES`:

```python
    "Der Mandant {person} ist {union}, le dossier mentionne aussi {health}.",
```

To `CLEAN_TEMPLATES` — entity-free prose in the same register, which is what makes the
precision number mean something:

```python
    "Die Lieferung der Medikamente an die Apotheke verzögert sich um zwei Tage.",
    "Le laboratoire ouvre un nouveau site de production en janvier.",
    "Die Betriebsversammlung findet am Donnerstag im großen Saal statt.",
    "La convention collective sera renégociée au printemps prochain.",
    "Das Formular für den Zugang zum Gebäude liegt am Empfang bereit.",
    "Le service des ressources humaines publiera le calendrier lundi.",
    "Die Aufzeichnungen der Sitzung werden im Intranet veröffentlicht.",
]
```

(Keep the templates already in each list; these are additions.)

- [ ] **Step 5: Regenerate and verify**

Run from the repo root: `make corpus`, then `cd detector && uv run pytest tests/test_corpus.py -v`.
Expected: all corpus tests pass, including the pre-existing ones asserting that every annotation slices to non-empty text and that annotations do not overlap.

Then verify the generator is still deterministic — CI gates on this:

```bash
cp evaluation/corpus/public.jsonl /tmp/run1.jsonl
make corpus
diff -q /tmp/run1.jsonl evaluation/corpus/public.jsonl
```

Expected: no difference. Then `make evaluate` — Tier 1 recall must still be 1.0000.

- [ ] **Step 6: Commit**

```bash
git add evaluation/generate.py evaluation/corpus/public.jsonl detector/tests/test_corpus.py
git commit -m "Art9: corpus templates per category, plus entity-free counterweight

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Recall gate, report grouping, README

**Files:**
- Modify: `detector/src/tessera_detector/evaluation.py`
- Modify: `detector/tests/test_evaluation.py`
- Modify: `evaluation/evaluate.py`
- Modify: `README.md`

**Interfaces:**
- Consumes: `Metrics`, `precision_gate_failures` (the existing sibling helper), `summarize`.
- Produces: `recall_gate_failures(per_type: dict[str, Metrics], *, types: set[str], target: float) -> list[tuple[str, float]]`.

- [ ] **Step 1: Write the failing tests**

Append to `detector/tests/test_evaluation.py`:

```python
def test_recall_gate_passes_when_above_target() -> None:
    from tessera_detector.evaluation import recall_gate_failures

    per_type = {"HEALTH": Metrics(tp=19, fn=1), "RELIGION": Metrics(tp=10, fn=0)}
    assert recall_gate_failures(per_type, types={"HEALTH", "RELIGION"}, target=0.95) == []


def test_recall_gate_reports_each_type_below_target() -> None:
    from tessera_detector.evaluation import recall_gate_failures

    per_type = {"HEALTH": Metrics(tp=5, fn=5), "RELIGION": Metrics(tp=10, fn=0)}
    failures = recall_gate_failures(per_type, types={"HEALTH", "RELIGION"}, target=0.95)
    assert [name for name, _ in failures] == ["HEALTH"]


def test_recall_gate_ignores_types_absent_from_the_run() -> None:
    from tessera_detector.evaluation import recall_gate_failures

    assert recall_gate_failures({"IBAN": Metrics(tp=3)}, types={"HEALTH"}, target=0.95) == []
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd detector && uv run pytest tests/test_evaluation.py -v`
Expected: FAIL — `ImportError: cannot import name 'recall_gate_failures'`.

- [ ] **Step 3: Add the helper**

In `detector/src/tessera_detector/evaluation.py`, beside `precision_gate_failures`:

```python
def recall_gate_failures(
    per_type: dict[str, Metrics], *, types: set[str], target: float
) -> list[tuple[str, float]]:
    """Types present in the run whose recall falls below target (REQ-3, REQ-38)."""
    return [
        (entity_type, per_type[entity_type].recall)
        for entity_type in sorted(types)
        if entity_type in per_type and per_type[entity_type].recall < target
    ]
```

and add `"recall_gate_failures"` to `__all__`, keeping it alphabetically sorted.

- [ ] **Step 4: Wire the gate into the runner**

In `evaluation/evaluate.py`, add `recall_gate_failures` to the `tessera_detector.evaluation` import, and define beside the existing targets:

```python
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
```

Group the printed table so the two kinds of number are not read as one. Replace the existing loop over `sorted(summary.per_type)`:

```python
    for entity_type in sorted(summary.per_type):
        m = summary.per_type[entity_type]
        print(
            f"{entity_type.ljust(width)}  {m.precision:.3f}  {m.recall:.3f}  {m.f1:.3f}"
            f"  {m.tp:4d} {m.fp:4d} {m.fn:4d}"
        )
```

with:

```python
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
```

Then, immediately before the `advisory = precision_gate_failures(...)` block that already exists, add the gate itself:

```python
    article_9_misses = recall_gate_failures(
        summary.per_type, types=ARTICLE_9_TYPES, target=ARTICLE_9_TARGET
    )
    for entity_type, recall in article_9_misses:
        print(
            f"FAIL: {entity_type} recall {recall:.4f} below target {ARTICLE_9_TARGET}",
            file=sys.stderr,
        )
```

and change the final `return 1 if failures else 0` to:

```python
    return 1 if failures or article_9_misses else 0
```

- [ ] **Step 5: Run the gates**

Run from the repo root: `make test && make lint && make evaluate`.
Expected: the suite passes, lint and mypy clean, Tier 1 recall 1.0000, and — with the model installed — an Article 9 section in the table.

Record the Article 9 recall numbers in your report. If a category falls below 0.95, report it with the numbers; do not raise the target or lower the threshold to make the gate pass. A category the model genuinely cannot find is a finding about the model or the corpus, and it is the reviewer's call which.

- [ ] **Step 6: Update the README**

In `README.md`, extend the metrics table with the Article 9 rows measured in Step 5 (the same columns as the existing rows), under their own sub-heading line in the surrounding prose, and add to the caveat paragraph:

```markdown
> Article 9 categories run at a deliberately low threshold (0.30): for health, religion,
> union membership and the rest, a miss is far more costly than a false positive, so the
> layer over-reports on purpose and `make evaluate` gates their recall (≥ 0.95) while
> leaving their precision ungated.
```

In the CLI section, after the paragraph about the NER layer, add:

```markdown
Article 9 special categories (health, biometrics, genetics, ethnic origin, political
opinion, religion, trade union membership, sexual orientation) are detected by the same
layer at a lower threshold, so expect visible false positives — over-redaction is the
safe failure for this category.
```

- [ ] **Step 7: Commit**

```bash
git add detector/src/tessera_detector/evaluation.py detector/tests/test_evaluation.py \
        evaluation/evaluate.py README.md
git commit -m "Art9: recall gate, grouped report, README

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Model-backed detection tests

**Files:**
- Modify: `detector/tests/test_gliner.py`

**Interfaces:**
- Consumes: the `recognizer` fixture already defined in that file, which skips when weights or the `ner` dependency group are absent.

- [ ] **Step 1: Write the tests**

Append to `detector/tests/test_gliner.py`:

```python
def test_finds_a_german_health_mention(recognizer: GlinerRecognizer) -> None:
    text = "Der Mitarbeiter Weber leidet an Diabetes und ist krankgeschrieben."
    spans = [s for s in recognizer.detect(text) if s.entity_type == "HEALTH"]
    assert spans, "expected a HEALTH span"
    assert "Diabetes" in text[spans[0].start : spans[0].end]
    assert spans[0].tier == 3


def test_finds_a_french_union_mention(recognizer: GlinerRecognizer) -> None:
    text = "Le salarié Dupont est adhérent de la CGT depuis trois ans."
    found = {s.entity_type for s in recognizer.detect(text)}
    assert "TRADE_UNION" in found, f"expected TRADE_UNION, got {sorted(found)}"
```

- [ ] **Step 2: Run them**

Run: `cd detector && uv run --group ner pytest tests/test_gliner.py -v` (weights must be installed; `make model` from the repo root if not).
Expected: both PASS alongside the existing marked tests.

If a category the model genuinely cannot find at threshold 0.30 turns up here, report the failure with the model's actual output rather than weakening the assertion — the point of these two tests is that the eight labels do something, and a label that finds nothing is worth knowing about before merge.

- [ ] **Step 3: Confirm the skip path still works**

Run: `cd detector && uv run pytest tests/test_gliner.py -q` without the `ner` group synced.
Expected: skips, no errors.

- [ ] **Step 4: Commit**

```bash
git add detector/tests/test_gliner.py
git commit -m "Art9: model-backed detection tests for health and union mentions

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After the plan

Push `feat/art9-categories`, open a PR to `main`, comment `@codex review`, and keep the fix → tag → wait loop going until Codex posts `Codex Review: Didn't find any major issues.` naming the current HEAD commit. The clean verdict arrives as an issue comment, not a review comment: poll `gh api repos/paderinandrey/tessera/issues/<num>/comments`.
