"""Latency benchmark for the detector (REQ-38).

Measures and reports; never gates. The target — p95 under 80 ms without the LLM
layer — is printed beside the measurement so the gap is visible rather than
implied.

Run from the repository root:  uv run --project detector python evaluation/benchmark.py
"""

import argparse
import json
import math
import statistics
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from tessera_detector.models import find_model
from tessera_detector.ner import load_ner_types
from tessera_detector.pipeline import Detector, build_detector

CORPUS = Path(__file__).parent / "corpus" / "public.jsonl"
TARGET_P95_MS = 80.0
# Character budgets. A paragraph is one chunk; a document spans several, which is
# where cost stops being linear in length.
SIZE_CLASSES = {"sentence": 80, "paragraph": 1200, "document": 6000}


def percentile(values: list[float], share: float) -> float:
    """Nearest-rank percentile: the smallest value at or above the given share.

    `int(n * share)` would land one observation late whenever `n * share` is a
    whole number — p95 of 1..100 would report 96 — which overstates latency and
    can flip a verdict against the target.
    """
    if not values:
        raise ValueError("no samples to take a percentile of")
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * share) - 1)
    return ordered[index]


def build_sizes(documents: list[str]) -> dict[str, str]:
    """One text per size class, concatenated from the corpus in order.

    Deterministic on purpose: a benchmark whose input drifts between runs
    reports noise as change. Each class is truncated to exactly its budget:
    overshooting by even one character pushes the paragraph class past
    CHUNK_SIZE into a second chunk, doubling the work and inflating the number
    the class exists to report.
    """
    sizes: dict[str, str] = {}
    for name, budget in SIZE_CLASSES.items():
        parts: list[str] = []
        length = 0
        index = 0
        while length < budget:
            document = documents[index % len(documents)]
            # The separator only exists between parts, so count it only when
            # one is actually added — otherwise the result lands short.
            length += len(document) + (1 if parts else 0)
            parts.append(document)
            index += 1
        sizes[name] = " ".join(parts)[:budget]
    return sizes


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


def measure(fn: Callable[[str], object], text: str, *, runs: int, warmup: int) -> list[float]:
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
    model_path = find_model() if full.ner_available else None
    if model_path is not None:
        from tessera_detector.ner import GlinerRecognizer

    timings: list[Timing] = []
    for size, text in sizes.items():
        timings.append(
            Timing(
                "deterministic",
                size,
                measure(
                    deterministic.deterministic.detect,
                    text,
                    runs=args.runs,
                    warmup=args.warmup,
                ),
            )
        )
        if full.ner_available:
            # One recognizer per tier, built from the public `types=` parameter
            # and released before the next: the passes are what a regression
            # lands in, and holding three ONNX sessions at once is needless.
            for tier in sorted({t.tier for t in load_ner_types()}):
                per_tier = GlinerRecognizer(
                    model_path, types=tuple(t for t in load_ner_types() if t.tier == tier)
                )
                timings.append(
                    Timing(
                        f"ner tier {tier}",
                        size,
                        measure(per_tier.detect, text, runs=args.runs, warmup=args.warmup),
                    )
                )
                del per_tier
        timings.append(
            Timing("total", size, measure(full.detect, text, runs=args.runs, warmup=args.warmup))
        )

    if args.as_json:
        print(
            json.dumps(
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
            )
        )
        return 0

    if not full.ner_available:
        print(f"NER layer off ({full.ner_off_reason}): only the deterministic layer is timed.")
    print(render(timings))
    return 0


__all__ = [
    "SIZE_CLASSES",
    "TARGET_P95_MS",
    "Timing",
    "build_sizes",
    "main",
    "measure",
    "percentile",
    "render",
]


if __name__ == "__main__":
    raise SystemExit(main())
