"""Trimming what a `PERSON` span swallows in front of the name (#20).

No model: the rule is a text function and its edges are where the risk is.
"""

from tessera_detector.ner import trim_leading_words


def test_trimming_never_empties_a_span() -> None:
    # A rule that can empty a span is a rule that can unmask a name. A person
    # really called `Herr`, or a surname that happens to be on the list, is
    # exactly that case — and it is the same clause that protects a span made
    # entirely of listed words.
    words = frozenset({"herr", "der", "kunde"})

    assert trim_leading_words("Herr", 0, 4, words) == (0, 4)
    assert trim_leading_words("Der Kunde", 0, 9, words) == (0, 9)
    assert trim_leading_words("Der Kunde Karz", 0, 14, words) == (10, 14)


def test_trimming_matches_case_insensitively_and_ignores_a_trailing_dot() -> None:
    # `Dr.`, `Dr` and `dr` are one entry rather than three, so a catalog that
    # spells one of them is not quietly missing the others.
    words = frozenset({"dr", "frau"})

    assert trim_leading_words("Dr. Martina Weber", 0, 17, words) == (4, 17)
    assert trim_leading_words("FRAU Martina Weber", 0, 18, words) == (5, 18)


def test_trimming_leaves_a_span_with_no_listed_word_alone() -> None:
    assert trim_leading_words("Martina Weber", 0, 13, frozenset({"herr"})) == (0, 13)
    assert trim_leading_words("Martina Weber", 0, 13, frozenset()) == (0, 13)


def test_trimming_only_reads_inside_the_span() -> None:
    # The span is a window into a larger text, and a word before `start` or
    # after `end` belongs to somebody else's span or to no span at all.
    text = "Herr Herr Martina Weber Herr"
    words = frozenset({"herr"})

    # Starting at the second `Herr`, the first is not the rule's business.
    assert trim_leading_words(text, 5, 23, words) == (10, 23)
    # And the trailing one is not either: only the front is trimmed, because
    # the corpus shows no trailing over-capture at all.
    assert trim_leading_words(text, 10, 28, words) == (10, 28)
