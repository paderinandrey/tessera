import pytest
from benchmark import SIZE_CLASSES, Timing, build_sizes, measure, percentile, render


def test_percentile_picks_the_ranked_value() -> None:
    values = [float(i) for i in range(1, 101)]
    assert percentile(values, 0.5) == 51.0
    assert percentile(values, 0.95) == 96.0
    assert percentile(values, 0.99) == 100.0


def test_percentile_handles_a_single_sample() -> None:
    assert percentile([7.5], 0.95) == 7.5


def test_percentile_rejects_an_empty_series() -> None:
    with pytest.raises(ValueError, match="no samples"):
        percentile([], 0.95)


def test_size_classes_are_exactly_their_budgets() -> None:
    # Exact, not "at least": one character past CHUNK_SIZE turns the paragraph
    # class into two chunks and doubles the work it is meant to measure.
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    sizes = build_sizes(documents)
    assert set(sizes) == set(SIZE_CLASSES)
    for name, budget in SIZE_CLASSES.items():
        assert len(sizes[name]) == budget


def test_paragraph_class_stays_within_one_chunk() -> None:
    from tessera_detector.ner import CHUNK_SIZE

    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    assert len(build_sizes(documents)["paragraph"]) <= CHUNK_SIZE


def test_size_classes_are_deterministic() -> None:
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    assert build_sizes(documents) == build_sizes(documents)


def test_size_classes_reuse_documents_when_the_corpus_is_short() -> None:
    # The corpus is finite; a long class must still reach its budget.
    sizes = build_sizes(["Kurz."])
    assert len(sizes["document"]) == SIZE_CLASSES["document"]


def test_measure_discards_the_warm_up_runs() -> None:
    calls: list[str] = []
    samples = measure(lambda text: calls.append(text), "hello", runs=5, warmup=2)
    assert len(calls) == 7
    assert len(samples) == 5
    assert all(sample >= 0.0 for sample in samples)


def test_timing_exposes_the_percentiles() -> None:
    timing = Timing(name="total", size="sentence", samples=[float(i) for i in range(1, 101)])
    assert timing.median == 50.5
    assert timing.p95 == 96.0
    assert timing.p99 == 100.0


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
