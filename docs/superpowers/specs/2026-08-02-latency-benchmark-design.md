# Latency Benchmark — Design

**Goal:** measure and publish the detector's latency, per layer and per document size, so
REQ-38's "p95 under 80 ms without the LLM layer" stops being an untested claim.

**Traceability:** REQ-38 (target quality and latency metrics), MVP roadmap Definition of
Done ("p95 latency without the LLM layer — up to 80 ms").

## What measurement already says

Taken before designing, on Apple Silicon CPU over the 130 committed corpus documents
(one sentence each), with the pinned fp32 ONNX graph:

| variant | p95 | quality |
|---|---|---|
| deterministic layer alone | 0.1 ms | no NER |
| one inference pass, 14 labels | 75.8 ms | meets target, loses Article 9 spans |
| two passes, sequential (shipped) | 112–118 ms | correct |
| two passes, two threads | 94.0 ms | correct |
| two passes, int8 graph | 57.7 ms | scores roughly halve; thresholds no longer calibrated |

**The target is not met.** The deterministic layer is free; the entire budget goes to the
model. Every way of meeting the number costs something: the single pass loses the Article 9
data that the per-tier split exists to protect; threads gain only 16% because onnxruntime
already saturates the cores, so the passes compete rather than overlap; and the int8 graph
is fast but depresses every score by roughly half — `Diabetes` 0.948 → 0.476,
`IG Metall` 0.98 → 0.54 — so it preserves ranking while invalidating every threshold
calibrated against fp32.

Publishing the gap is the deliverable. Closing it is separate work with its own tradeoffs.

## Scope

A harness that measures and reports. It never gates, and it does not change detection.

## Components

```
evaluation/benchmark.py    the harness: size classes, timing, report
Makefile                   a `bench` target
detector/tests/test_benchmark.py   unit tests for the pure parts
```

No new dependencies: `time.perf_counter` and `statistics` from the standard library.

## Input sizes

The corpus documents are single sentences, which flatters the number: latency grows with
the number of chunks and token windows. The harness therefore measures three size classes,
each built deterministically by concatenating corpus documents in order — no new fixture
files, no randomness, and the text stays synthetic:

- **sentence** — one corpus document, about 80 characters.
- **paragraph** — about 1 200 characters, which is one chunk.
- **document** — about 6 000 characters, several chunks with their overlap.

The ladder is what shows where cost stops being linear.

## What is reported

For each size class, in each mode the machine can run — deterministic only, and full with
NER when weights are present — the report gives median, p95, p99 and the number of runs,
plus a per-layer split: the deterministic pass, the tier 2 inference pass, and the tier 3
pass. The split is what makes the total explicable rather than merely alarming.

Runs are preceded by warm-up iterations whose timings are discarded: the first calls carry
graph loading, onnxruntime's own optimization passes and tokenizer warm-up, none of which a
running service pays per request.

The target is printed beside each measured p95, with the gap stated rather than implied.
`--json` emits the same data for machine consumption.

## CI

The `ner` job runs the harness with a small repetition count as a smoke test: it fails if
the harness breaks, and asserts nothing about milliseconds. Timing assertions on shared
runners produce flaky builds, not signal — the numbers that matter come from a known
machine, recorded in the README with its hardware.

## Testing

The pure parts are tested: percentile selection, the deterministic construction of the size
classes, and the report rendering. Timings themselves are never asserted — a test that
fails because the machine is busy teaches nothing and trains people to ignore red builds.

## Out of scope

Making the detector faster — an int8 threshold profile, batching, GPU execution — and any
latency gate. The gateway is not built yet, so end-to-end proxy latency cannot be measured.
