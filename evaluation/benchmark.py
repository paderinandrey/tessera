"""Latency benchmark for the detector (REQ-38).

Measures and reports; never gates. The target — p95 under 80 ms without the LLM
layer — is printed beside the measurement so the gap is visible rather than
implied.

Run from the repository root:  uv run --project detector python evaluation/benchmark.py
"""

import statistics
import time
from collections.abc import Callable
from dataclasses import dataclass
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
            # The separator only exists between parts, so count it only when
            # one is actually added — otherwise the result lands short.
            length += len(document) + (1 if parts else 0)
            parts.append(document)
            index += 1
        sizes[name] = " ".join(parts)
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


__all__ = [
    "SIZE_CLASSES",
    "TARGET_P95_MS",
    "Timing",
    "build_sizes",
    "measure",
    "percentile",
    "render",
]
