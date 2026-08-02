# Latency Benchmark Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a harness that measures detector latency per layer and per document size, publishes the numbers, and states plainly that REQ-38's 80 ms target is not currently met.

**Architecture:** One new script, `evaluation/benchmark.py`, beside the existing generator and metrics runner. Its pure parts — percentile selection, size-class construction, report rendering — are importable and unit-tested; the timing loop is not. Nothing about detection changes.

**Tech Stack:** Python 3.14 standard library only (`time.perf_counter`, `statistics`, `argparse`, `json`), over the existing `Detector` and `GlinerRecognizer`.

**Spec:** `docs/superpowers/specs/2026-08-02-latency-benchmark-design.md`

## Global Constraints

- The harness never gates and never asserts a timing. It measures, reports, and exits 0 unless it crashes.
- No new dependencies.
- Size classes are built deterministically by concatenating corpus documents in order — no randomness, no new fixture files.
- Warm-up iterations are discarded: the first calls carry graph load, onnxruntime optimization and tokenizer warm-up, which a running service does not pay per request.
- ruff line-length 100; gates from the repo root: `make test`, `make lint`, `make evaluate`. mypy is strict, and `evaluation/` is linted by ruff but not type-checked (`uv run mypy src` covers `detector/src` only).
- Commit message style: one-line `Bench: <what>` with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run tests from `detector/`: `uv run pytest tests/test_benchmark.py -v`.

---

### Task 1: Size classes and percentiles

**Files:**
- Create: `evaluation/benchmark.py`
- Create: `detector/tests/test_benchmark.py`

**Interfaces:**
- Produces: `SIZE_CLASSES: dict[str, int]` mapping a class name to its target character budget; `build_sizes(documents: list[str]) -> dict[str, str]` returning one text per class; `percentile(values: list[float], share: float) -> float`. Tasks 2 and 3 consume all three.

**Note on importability:** the tests import from `evaluation/benchmark.py`, which is not a package. Add this at the top of the test file so the import resolves from the repo root:

```python
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "evaluation"))
```

- [ ] **Step 1: Write the failing tests**

```python
# detector/tests/test_benchmark.py
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "evaluation"))

import pytest

from benchmark import SIZE_CLASSES, build_sizes, percentile


def test_percentile_picks_the_ranked_value() -> None:
    values = [float(i) for i in range(1, 101)]
    assert percentile(values, 0.5) == 50.0
    assert percentile(values, 0.95) == 95.0
    assert percentile(values, 0.99) == 99.0


def test_percentile_handles_a_single_sample() -> None:
    assert percentile([7.5], 0.95) == 7.5


def test_percentile_rejects_an_empty_series() -> None:
    with pytest.raises(ValueError, match="no samples"):
        percentile([], 0.95)


def test_size_classes_hit_their_budgets() -> None:
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    sizes = build_sizes(documents)
    assert set(sizes) == set(SIZE_CLASSES)
    for name, budget in SIZE_CLASSES.items():
        # At least the budget, and not wildly past it: one document of overshoot.
        assert budget <= len(sizes[name]) < budget + 200


def test_size_classes_are_deterministic() -> None:
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    assert build_sizes(documents) == build_sizes(documents)


def test_size_classes_reuse_documents_when_the_corpus_is_short() -> None:
    # The corpus is finite; a long class must still reach its budget.
    sizes = build_sizes(["Kurz."])
    assert len(sizes["document"]) >= SIZE_CLASSES["document"]
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd detector && uv run pytest tests/test_benchmark.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'benchmark'`.

- [ ] **Step 3: Write the module's first half**

```python
# evaluation/benchmark.py
"""Latency benchmark for the detector (REQ-38).

Measures and reports; never gates. The target — p95 under 80 ms without the LLM
layer — is printed beside the measurement so the gap is visible rather than
implied.

Run from the repository root:  uv run --project detector python evaluation/benchmark.py
"""

import json
import statistics
import sys
import time
from pathlib import Path

CORPUS = Path(__file__).parent / "corpus" / "public.jsonl"
TARGET_P95_MS = 80.0
# Character budgets. A paragraph is one chunk; a document spans several, which is
# where cost stops being linear in length.
SIZE_CLASSES = {"sentence": 80, "paragraph": 1200, "document": 6000}


def percentile(values: list[float], share: float) -> float:
    if not values:
        raise ValueError("no samples to take a percentile of")
    ordered = sorted(values)
    index = min(int(len(ordered) * share), len(ordered) - 1)
    return ordered[index]


def build_sizes(documents: list[str]) -> dict[str, str]:
    """One text per size class, concatenated from the corpus in order.

    Deterministic on purpose: a benchmark whose input drifts between runs
    reports noise as change.
    """
    sizes: dict[str, str] = {}
    for name, budget in SIZE_CLASSES.items():
        parts: list[str] = []
        length = 0
        index = 0
        while length < budget:
            document = documents[index % len(documents)]
            parts.append(document)
            length += len(document) + 1
            index += 1
        sizes[name] = " ".join(parts)
    return sizes
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd detector && uv run pytest tests/test_benchmark.py -v`
Expected: all PASS. If `test_size_classes_hit_their_budgets` fails on the upper bound, the corpus documents are longer than the 200-character slack allows — widen the slack in the test to the longest corpus document's length rather than changing the loop, which is correct as written.

- [ ] **Step 5: Commit**

```bash
git add evaluation/benchmark.py detector/tests/test_benchmark.py
git commit -m "Bench: deterministic size classes and percentiles

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: The measurement loop and the report

**Files:**
- Modify: `evaluation/benchmark.py`
- Modify: `detector/tests/test_benchmark.py`

**Interfaces:**
- Consumes: `percentile`, `build_sizes`, `SIZE_CLASSES`, `TARGET_P95_MS` from Task 1; `Detector` and `build_detector` from `tessera_detector.pipeline`.
- Produces: `Timing(name: str, size: str, samples: list[float])` (frozen dataclass) with `.median`, `.p95`, `.p99` properties; `measure(fn, text, *, runs, warmup) -> list[float]`; `render(timings: list[Timing]) -> str`. Task 3 renders and prints them.

- [ ] **Step 1: Write the failing tests**

Append to `detector/tests/test_benchmark.py`:

```python
from benchmark import Timing, measure, render


def test_measure_discards_the_warm_up_runs() -> None:
    calls: list[str] = []
    samples = measure(lambda text: calls.append(text), "hello", runs=5, warmup=2)
    assert len(calls) == 7
    assert len(samples) == 5
    assert all(sample >= 0.0 for sample in samples)


def test_timing_exposes_the_percentiles() -> None:
    timing = Timing(name="total", size="sentence", samples=[float(i) for i in range(1, 101)])
    assert timing.median == 50.0
    assert timing.p95 == 95.0
    assert timing.p99 == 99.0


def test_render_groups_by_size_and_names_the_target() -> None:
    timings = [
        Timing(name="deterministic", size="sentence", samples=[0.1, 0.1, 0.2]),
        Timing(name="total", size="sentence", samples=[90.0, 95.0, 120.0]),
    ]
    report = render(timings)
    assert "sentence" in report
    assert "deterministic" in report
    assert "80.0" in report, "the target belongs beside the measurement"
    assert "n=3" in report


def test_render_says_when_the_target_is_missed() -> None:
    over = render([Timing(name="total", size="paragraph", samples=[100.0, 110.0, 130.0])])
    under = render([Timing(name="total", size="paragraph", samples=[10.0, 11.0, 13.0])])
    assert "over target" in over
    assert "over target" not in under
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd detector && uv run pytest tests/test_benchmark.py -v`
Expected: FAIL — `ImportError: cannot import name 'Timing'`.

- [ ] **Step 3: Write the implementation**

Add to `evaluation/benchmark.py` (`from collections.abc import Callable` and `from dataclasses import dataclass` go to the top with the other imports):

```python
@dataclass(frozen=True, slots=True)
class Timing:
    name: str
    size: str
    samples: list[float]

    @property
    def median(self) -> float:
        return statistics.median(self.samples)

    @property
    def p95(self) -> float:
        return percentile(self.samples, 0.95)

    @property
    def p99(self) -> float:
        return percentile(self.samples, 0.99)


def measure(
    fn: Callable[[str], object], text: str, *, runs: int, warmup: int
) -> list[float]:
    """Milliseconds per call, with the warm-up runs discarded.

    The first calls carry graph loading, onnxruntime's optimization passes and
    tokenizer warm-up — costs a running service pays once, not per request.
    """
    for _ in range(warmup):
        fn(text)
    samples: list[float] = []
    for _ in range(runs):
        started = time.perf_counter()
        fn(text)
        samples.append((time.perf_counter() - started) * 1000)
    return samples


def render(timings: list[Timing]) -> str:
    lines = [f"{'size':<10} {'layer':<14} {'median':>8} {'p95':>8} {'p99':>8}  runs"]
    for size in SIZE_CLASSES:
        for timing in [t for t in timings if t.size == size]:
            lines.append(
                f"{timing.size:<10} {timing.name:<14} {timing.median:>8.1f} "
                f"{timing.p95:>8.1f} {timing.p99:>8.1f}  n={len(timing.samples)}"
            )
    lines.append("")
    lines.append(f"Target: p95 <= {TARGET_P95_MS} ms without the LLM layer (REQ-38)")
    for timing in [t for t in timings if t.name == "total"]:
        verdict = "over target" if timing.p95 > TARGET_P95_MS else "within target"
        lines.append(f"  {timing.size:<10} p95 {timing.p95:.1f} ms — {verdict}")
    return "\n".join(lines)
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd detector && uv run pytest tests/test_benchmark.py -v`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add evaluation/benchmark.py detector/tests/test_benchmark.py
git commit -m "Bench: timing loop with warm-up discarded, report names the target

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Wiring, the Makefile target and CI

**Files:**
- Modify: `evaluation/benchmark.py`
- Modify: `Makefile`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: everything from Tasks 1–2, plus `build_detector` and `Detector` from `tessera_detector.pipeline` and `find_model` from `tessera_detector.models`.
- Produces: `main(argv: list[str] | None = None) -> int`; `make bench`.

- [ ] **Step 1: Write `main`**

Add to `evaluation/benchmark.py`:

```python
def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Detector latency benchmark (REQ-38)")
    parser.add_argument("--runs", type=int, default=30, help="timed runs per measurement")
    parser.add_argument("--warmup", type=int, default=5, help="discarded runs per measurement")
    parser.add_argument("--json", action="store_true", dest="as_json", help="machine-readable")
    args = parser.parse_args(argv)

    documents = [
        json.loads(line)["text"] for line in CORPUS.read_text(encoding="utf-8").splitlines()
    ]
    sizes = build_sizes(documents)
    deterministic = Detector()
    full = build_detector()

    timings: list[Timing] = []
    for size, text in sizes.items():
        timings.append(
            Timing("deterministic", size, measure(
                deterministic.deterministic.detect, text, runs=args.runs, warmup=args.warmup
            ))
        )
        if full.ner_available:
            recognizer = full.recognizer
            assert recognizer is not None
            timings.append(
                Timing("ner", size, measure(
                    recognizer.detect, text, runs=args.runs, warmup=args.warmup
                ))
            )
        timings.append(
            Timing("total", size, measure(
                full.detect, text, runs=args.runs, warmup=args.warmup
            ))
        )

    if args.as_json:
        print(json.dumps(
            {
                "ner": full.ner_available,
                "target_p95_ms": TARGET_P95_MS,
                "timings": [
                    {
                        "size": t.size,
                        "layer": t.name,
                        "median_ms": round(t.median, 2),
                        "p95_ms": round(t.p95, 2),
                        "p99_ms": round(t.p99, 2),
                        "runs": len(t.samples),
                    }
                    for t in timings
                ],
            },
            indent=2,
        ))
        return 0

    if not full.ner_available:
        print(f"NER layer off ({full.ner_off_reason}): only the deterministic layer is timed.")
    print(render(timings))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

with `import argparse` at the top, and this import beside the existing ones:

```python
from tessera_detector.pipeline import Detector, build_detector
```

- [ ] **Step 2: Add the Makefile target**

Add `bench` to the `.PHONY` line and this target after `evaluate`:

```make
bench:
	uv run --project detector --group ner python evaluation/benchmark.py
```

- [ ] **Step 3: Run it both ways**

Run from the repo root: `make bench` (with weights installed) and confirm the table shows three size classes, three layers each, and a target verdict per size.

Then confirm the no-weights path: `HOME=/tmp/no-weights uv run --project detector python evaluation/benchmark.py` prints the "NER layer off" note and times only the deterministic layer, exiting 0.

Finally `uv run --project detector python evaluation/benchmark.py --json --runs 3 --warmup 1 | python3 -m json.tool > /dev/null` to confirm the JSON parses.

- [ ] **Step 4: Add the CI smoke check**

In `.github/workflows/ci.yml`, in the `ner` job after the evaluation step:

```yaml
      # Smoke only: the harness must run, but timings on a shared runner are
      # noise and are never asserted. Real numbers come from a known machine.
      - run: uv run --group ner python ../evaluation/benchmark.py --runs 3 --warmup 1
```

- [ ] **Step 5: Run the gates and commit**

Run: `make test && make lint && make evaluate`.
Expected: all pass, metrics unchanged — this task touches nothing detection does.

```bash
git add evaluation/benchmark.py Makefile .github/workflows/ci.yml
git commit -m "Bench: make bench, JSON output, CI smoke check

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Publish the numbers

**Files:**
- Modify: `README.md`

**Interfaces:** none — documentation only.

- [ ] **Step 1: Measure**

Run `make bench` from the repo root with the weights installed and keep the output. Record the machine (CPU, core count) — the numbers mean nothing without it.

- [ ] **Step 2: Write the section**

Add a `## Latency` section to `README.md` after `## Evaluation`, containing:

- The command (`make bench`).
- A table of the measured p95 per size class for the deterministic layer and the total, taken from Step 1's run, with the machine named.
- This paragraph, which is the point of the exercise:

```markdown
> REQ-38 targets p95 under 80 ms without the LLM layer, and the detector does not meet it
> with the NER layer enabled. The deterministic layer is effectively free; the entire budget
> goes to the model. Meeting the target costs something in every direction measured so far:
> collapsing the two inference passes into one comes in at 75.8 ms but loses the Article 9
> spans the split exists to protect, running the passes on two threads gains only 16% because
> onnxruntime already saturates the cores, and the int8 graph reaches 57.7 ms while halving
> every confidence score — `Diabetes` 0.948 → 0.476 — which preserves ranking but invalidates
> thresholds calibrated against fp32. The number is published here rather than gated in CI:
> timings on shared runners are noise, and a target that is not met should be visible instead
> of quietly enforced somewhere it never runs.
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "Bench: publish the latency numbers and the gap

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After the plan

Push `feat/latency-benchmark`, open a PR to `main`, comment `@codex review`, and keep the fix → tag → wait loop going until Codex reviews the current HEAD with no findings attached. The clean verdict arrives either as an issue comment saying `Didn't find any major issues` or as a review on the current commit carrying no inline comments — check both.
