"""Text normalization for matching, with offsets mapping back to the original (REQ-6).

Normalization exists only so recognizers see a canonical form (NFKC, unified spaces
and hyphens); every normalized character keeps a pointer to the original characters it
came from, and spans are always reported in original coordinates.

Normalization runs per combining sequence (a base character plus its combining marks),
not per code point: NFKC only composes "e" + U+0301 into "é" when it sees the whole
sequence. Each output character maps to the original segment it was produced from, so
both expansions (ligatures) and contractions (composition) stay mappable.
"""

import unicodedata
from dataclasses import dataclass

# Unicode hyphen variants that NFKC does not fold to ASCII "-": hyphen,
# non-breaking hyphen, figure dash, en dash, minus sign.
_HYPHENS = frozenset("\u2010\u2011\u2012\u2013\u2212")
# Separators NFKC leaves alone (NBSP and narrow NBSP are already NFKC-folded):
# line separator, paragraph separator.
_SPACES = frozenset("\u2028\u2029")


@dataclass(frozen=True, slots=True)
class NormalizedText:
    text: str
    _orig_start: tuple[int, ...]
    _orig_end: tuple[int, ...]

    def to_original(self, start: int, end: int) -> tuple[int, int]:
        """Map a [start, end) span in normalized coordinates to original coordinates."""
        if not 0 <= start < end <= len(self.text):
            raise ValueError(
                f"invalid normalized span [{start}, {end}) for text of length {len(self.text)}"
            )
        return self._orig_start[start], self._orig_end[end - 1]


def _segments(original: str) -> list[tuple[int, int]]:
    """Split into combining sequences: [start, end) where marks attach to their base."""
    bounds: list[tuple[int, int]] = []
    for i, ch in enumerate(original):
        if bounds and unicodedata.combining(ch):
            bounds[-1] = (bounds[-1][0], i + 1)
        else:
            bounds.append((i, i + 1))
    return bounds


def normalize(original: str) -> NormalizedText:
    chars: list[str] = []
    orig_start: list[int] = []
    orig_end: list[int] = []
    for seg_start, seg_end in _segments(original):
        segment = original[seg_start:seg_end]
        if segment in _HYPHENS:
            folded = "-"
        elif segment in _SPACES:
            folded = " "
        else:
            folded = unicodedata.normalize("NFKC", segment)
        for out in folded:
            chars.append(out)
            orig_start.append(seg_start)
            orig_end.append(seg_end)
    return NormalizedText(
        text="".join(chars), _orig_start=tuple(orig_start), _orig_end=tuple(orig_end)
    )
