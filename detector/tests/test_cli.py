from tessera_detector.cli import mask


def test_mask_keeps_first_four_and_last_two() -> None:
    assert mask("FR7630006000011234567890189") == "FR76…89"


def test_mask_hides_short_values_entirely() -> None:
    # 7 chars: revealing 6 of them would leave nothing masked.
    assert mask("a@b.com") == "…"


def test_mask_eight_chars_is_partial() -> None:
    assert mask("ab@cd.ef") == "ab@c…ef"


def test_mask_counts_characters_not_bytes() -> None:
    assert mask("héloïse@exämple.ch") == "hélo…ch"
