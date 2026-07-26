from tessera_detector.normalize import normalize


def test_ascii_text_is_identity() -> None:
    original = "Jean Dupont, IBAN DE89 3704 0044 0532 0130 00"
    norm = normalize(original)
    assert norm.text == original
    assert norm.to_original(0, len(original)) == (0, len(original))
    assert norm.to_original(5, 11) == (5, 11)


def test_nbsp_becomes_space_and_offsets_map_back() -> None:
    # IBAN groups separated by U+00A0 — common in copy-pasted bank statements.
    original = "IBAN: DE89\u00a03704\u00a00044\u00a00532\u00a00130\u00a000."
    norm = normalize(original)
    assert "\u00a0" not in norm.text
    assert norm.text == "IBAN: DE89 3704 0044 0532 0130 00."
    # The IBAN occupies the same character positions in both strings here.
    start = norm.text.index("DE89")
    end = norm.text.index(".", start)
    assert norm.to_original(start, end) == (start, end)
    assert original[start:end] == "DE89\u00a03704\u00a00044\u00a00532\u00a00130\u00a000"


def test_narrow_nbsp_becomes_space() -> None:
    # French digit grouping uses U+202F (narrow no-break space).
    original = "2\u202f95\u202f10\u202f99"
    norm = normalize(original)
    assert norm.text == "2 95 10 99"


def test_unicode_hyphen_unified() -> None:
    # U+2011 (non-breaking hyphen) in a double-barrelled name.
    original = "Anne\u2011Marie"
    norm = normalize(original)
    assert norm.text == "Anne-Marie"
    assert norm.to_original(0, len("Anne-Marie")) == (0, len(original))


def test_ligature_expansion_keeps_offsets_mappable() -> None:
    # NFKC expands U+FB01 (ﬁ) to "fi": normalized text is longer than the original.
    original = "conﬁdentiel dossier"
    norm = normalize(original)
    assert norm.text == "confidentiel dossier"
    # "dossier" sits after the expansion point; its normalized span must map back
    # to the correct original coordinates.
    n_start = norm.text.index("dossier")
    o_start, o_end = norm.to_original(n_start, n_start + len("dossier"))
    assert original[o_start:o_end] == "dossier"
    # A span covering the whole normalized word maps to the whole original word.
    o_start, o_end = norm.to_original(0, len("confidentiel"))
    assert original[o_start:o_end] == "conﬁdentiel"


def test_span_inside_expanded_char_maps_to_that_char() -> None:
    original = "ﬁn"  # normalizes to "fin"
    norm = normalize(original)
    assert norm.text == "fin"
    # Both "f" and "i" come from the single original ligature at position 0.
    assert norm.to_original(0, 1) == (0, 1)
    assert norm.to_original(1, 2) == (0, 1)
    assert norm.to_original(2, 3) == (1, 2)


def test_decomposed_sequences_compose() -> None:
    # NFC/NFKC must see the whole combining sequence, not code points one by one:
    # "Cafe" + U+0301 composes to "Café" (one char shorter than the original).
    original = "Cafe\u0301 Zu\u0308rich"
    norm = normalize(original)
    assert norm.text == "Caf\u00e9 Z\u00fcrich"
    # Span over composed "Café" covers the full original sequence incl. the accent.
    assert norm.to_original(0, 4) == (0, 5)
    # "Zürich" after the composition point maps back to its original coordinates.
    n_start = norm.text.index("Z")
    o_start, o_end = norm.to_original(n_start, n_start + 6)
    assert original[o_start:o_end] == "Zu\u0308rich"
