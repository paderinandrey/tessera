"""Latency benchmark for the detector (REQ-38).

Measures and reports; never gates. The target — p95 under 80 ms without the LLM
layer — is printed beside the measurement so the gap is visible rather than
implied.

Run from the repository root:  uv run --project detector python evaluation/benchmark.py
"""

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


__all__ = ["SIZE_CLASSES", "TARGET_P95_MS", "build_sizes", "percentile"]
