"""Text normalization for matching, with offsets mapping back to the original (REQ-6).

Normalization exists only so recognizers see a canonical form (NFKC, unified spaces
and hyphens); every normalized character keeps a pointer to the original character it
came from, and spans are always reported in original coordinates.
"""

import unicodedata
from dataclasses import dataclass

# Unicode hyphen variants that NFKC does not fold to ASCII "-".
_HYPHENS = frozenset("‐‑‒–−")
# Space variants NFKC leaves alone (NBSP and narrow NBSP are already NFKC-folded).
_SPACES = frozenset("  ")


@dataclass(frozen=True, slots=True)
class NormalizedText:
    text: str
    _orig_index: tuple[int, ...]
    _orig_len: int

    def to_original(self, start: int, end: int) -> tuple[int, int]:
        """Map a [start, end) span in normalized coordinates to original coordinates."""
        if not 0 <= start < end <= len(self.text):
            raise ValueError(f"invalid normalized span [{start}, {end}) for text of length {len(self.text)}")
        return self._orig_index[start], self._orig_index[end - 1] + 1


def normalize(original: str) -> NormalizedText:
    chars: list[str] = []
    orig_index: list[int] = []
    for i, ch in enumerate(original):
        if ch in _HYPHENS:
            folded = "-"
        elif ch in _SPACES:
            folded = " "
        else:
            folded = unicodedata.normalize("NFKC", ch)
        for out in folded:
            chars.append(out)
            orig_index.append(i)
    return NormalizedText(text="".join(chars), _orig_index=tuple(orig_index), _orig_len=len(original))
