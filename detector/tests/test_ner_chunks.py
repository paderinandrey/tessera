from itertools import pairwise

import pytest

from tessera_detector.ner import chunks, token_windows


def test_short_text_is_one_chunk() -> None:
    assert chunks("hello", size=100, overlap=10) == [(0, "hello")]


def test_empty_text_yields_nothing() -> None:
    assert chunks("", size=100, overlap=10) == []


def test_every_chunk_offset_locates_it_in_the_original() -> None:
    text = "".join(f"paragraph {i} with some filler text.\n\n" for i in range(40))
    for offset, chunk in chunks(text, size=200, overlap=40):
        assert text[offset : offset + len(chunk)] == chunk


def test_chunks_cover_the_whole_text() -> None:
    text = "".join(f"paragraph {i} with some filler text.\n\n" for i in range(40))
    covered = bytearray(len(text))
    for offset, chunk in chunks(text, size=200, overlap=40):
        for i in range(offset, offset + len(chunk)):
            covered[i] = 1
    assert all(covered), "chunking dropped part of the text"


def test_seams_overlap_so_entities_are_not_split_away() -> None:
    text = "x" * 500
    produced = chunks(text, size=200, overlap=40)
    assert len(produced) > 1
    for (start_a, chunk_a), (start_b, _) in pairwise(produced):
        assert start_b < start_a + len(chunk_a), "consecutive chunks must overlap"


def test_paragraph_boundary_is_preferred_over_a_hard_cut() -> None:
    head = "a" * 150
    tail = "b" * 150
    chunk = chunks(f"{head}\n\n{tail}", size=200, overlap=20)[0][1]
    assert chunk.endswith("a") or chunk.endswith("\n"), "cut should land on the boundary"
    assert "b" not in chunk


@pytest.mark.parametrize(("size", "overlap"), [(0, 0), (100, 100), (100, 200)])
def test_invalid_windows_are_rejected(size: int, overlap: int) -> None:
    with pytest.raises(ValueError):
        chunks("some text", size=size, overlap=overlap)


def test_token_windows_keeps_short_input_whole() -> None:
    offsets = [(0, 3), (4, 8), (9, 12)]
    assert token_windows(offsets, budget=10, overlap=2) == [(0, 12)]


def test_token_windows_splits_when_over_budget() -> None:
    offsets = [(i * 2, i * 2 + 1) for i in range(10)]
    windows = token_windows(offsets, budget=4, overlap=1)
    assert len(windows) > 1
    assert windows[0] == (0, 7)
    assert all(start < end for start, end in windows)


def test_token_windows_cover_every_token() -> None:
    offsets = [(i * 3, i * 3 + 2) for i in range(25)]
    windows = token_windows(offsets, budget=6, overlap=2)
    for start, end in offsets:
        assert any(w_start <= start and end <= w_end for w_start, w_end in windows), (
            f"token ({start}, {end}) fell outside every window"
        )


def test_token_windows_overlap_at_the_seams() -> None:
    # Without overlap an entity split by a window edge is lost twice over.
    offsets = [(i * 2, i * 2 + 1) for i in range(20)]
    windows = token_windows(offsets, budget=5, overlap=2)
    for (start_a, end_a), (start_b, _) in pairwise(windows):
        assert start_b < end_a, f"windows {(start_a, end_a)} and {start_b} do not overlap"


def test_token_windows_handles_no_tokens() -> None:
    assert token_windows([], budget=4, overlap=1) == []
