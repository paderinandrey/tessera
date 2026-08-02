import pytest
from benchmark import SIZE_CLASSES, build_sizes, percentile


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


def test_size_classes_hit_their_budgets() -> None:
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    sizes = build_sizes(documents)
    assert set(sizes) == set(SIZE_CLASSES)
    longest = max(len(d) for d in documents)
    for name, budget in SIZE_CLASSES.items():
        assert budget <= len(sizes[name]) < budget + longest + 1


def test_size_classes_are_deterministic() -> None:
    documents = [f"Dokument {i} mit etwas Text darin." for i in range(400)]
    assert build_sizes(documents) == build_sizes(documents)


def test_size_classes_reuse_documents_when_the_corpus_is_short() -> None:
    # The corpus is finite; a long class must still reach its budget.
    sizes = build_sizes(["Kurz."])
    assert len(sizes["document"]) >= SIZE_CLASSES["document"]
