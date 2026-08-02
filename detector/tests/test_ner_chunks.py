from itertools import pairwise

import pytest

from tessera_detector.ner import chunks


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
