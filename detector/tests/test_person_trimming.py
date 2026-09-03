"""Trimming what a `PERSON` span swallows in front of the name (#20).

No model: the rule is a text function and its edges are where the risk is.
"""

from tessera_detector.ner import trim_leading_words

WORDS = frozenset({"herr", "kunde", "salarié", "client"})
ARTICLES = frozenset({"der", "die", "das", "le", "la"})


def trim(text: str, start: int = 0, end: int | None = None) -> str:
    # `piece_starts_a_word` is what the caller knows and the function cannot:
    # whether offset zero of this text is also a word boundary in the document
    # it was cut from. These texts are whole documents, so it is.
    at, to = trim_leading_words(
        text,
        start,
        len(text) if end is None else end,
        WORDS,
        ARTICLES,
        piece_starts_a_word=True,
    )
    return text[at:to]


def test_an_article_alone_is_never_trimmed() -> None:
    # `Le` is a Vietnamese family name and `Das` a Bengali one. Trimming a
    # leading article on its own sends a real name component to the provider in
    # clear — `Le Thi Mai` arriving as `Le [PERSON_1]` — which is worse than the
    # over-wide span this rule exists to narrow.
    #
    # An article goes only in front of a word that goes anyway, which is the
    # only shape the corpus ever shows.
    assert trim("Le Thi Mai") == "Le Thi Mai"
    assert trim("Das Gupta") == "Das Gupta"
    assert trim("Le salarié Gallet") == "Gallet"
    assert trim("Der Kunde Karz") == "Karz"


def test_a_span_left_holding_only_punctuation_is_not_trimmed() -> None:
    # The model puts trailing punctuation inside a span often enough that
    # `Herr !` reaches the rule. Counting the `!` as remaining content let the
    # trim keep the punctuation and send `Herr` to the provider in clear — the
    # never-empty clause asked about token count when it had to ask about
    # content.
    assert trim("Herr !") == "Herr !"
    assert trim("Der Kunde :") == "Der Kunde :"
    assert trim("Der Kunde Karz !") == "Karz !"


def test_a_window_that_begins_inside_a_word_is_not_a_boundary() -> None:
    # A chunk can start mid-word, and this function is handed the window rather
    # than the document, so offset zero looks like a boundary when it is the
    # tail of `Alexander`. The caller says which it is; a caller that does not
    # know says nothing and nothing is trimmed.
    piece = "der Kunde Karz"

    assert trim_leading_words(piece, 0, len(piece), WORDS, ARTICLES) == (0, len(piece))
    assert trim_leading_words(
        piece, 0, len(piece), WORDS, ARTICLES, piece_starts_a_word=True
    ) == (10, len(piece))


def test_trimming_needs_a_word_boundary_at_the_start() -> None:
    # A token window can begin inside a word, and `der` at the end of
    # `Alexander` is not an article. Trimming there would expose the rest of a
    # name the span was at least partly covering.
    text = "Alexander Weber"

    assert trim(text, 6) == "der Weber"
    assert trim("Der Kunde Karz", 0) == "Karz"


def test_trimming_walks_runs_of_any_whitespace() -> None:
    # Two spaces made an empty token, which is on no list, so the walk stopped
    # there and the span still covered `Kunde Karz` — and an all-listed
    # `Der  Kunde` slipped past the never-empty guard, exposing `Der`.
    assert trim("Der  Kunde Karz") == "Karz"
    assert trim("Der\tKunde Karz") == "Karz"
    assert trim("Der\u00a0Kunde Karz") == "Karz"
    assert trim("Der  Kunde") == "Der  Kunde"


def test_trimming_never_empties_a_span() -> None:
    # A rule that can empty a span is a rule that can unmask a name. A person
    # really called `Herr`, or a surname that happens to be on the list, is
    # exactly that case — and it is the same clause that protects a span made
    # entirely of listed words.
    assert trim("Herr") == "Herr"
    assert trim("Der Kunde") == "Der Kunde"
    assert trim("Der Kunde Karz") == "Karz"


def test_trimming_matches_case_insensitively_and_ignores_a_trailing_dot() -> None:
    # `Dr.`, `Dr` and `dr` are one entry rather than three, so a catalog that
    # spells one of them is not quietly missing the others.
    words = frozenset({"dr", "frau"})

    assert trim_leading_words(
        "Dr. Martina Weber", 0, 17, words, piece_starts_a_word=True
    ) == (4, 17)
    assert trim_leading_words(
        "FRAU Martina Weber", 0, 18, words, piece_starts_a_word=True
    ) == (5, 18)


def test_trimming_leaves_a_span_with_no_listed_word_alone() -> None:
    assert trim("Martina Weber") == "Martina Weber"
    assert trim_leading_words(
        "Martina Weber", 0, 13, frozenset(), frozenset(), piece_starts_a_word=True
    ) == (0, 13)


def test_trimming_only_reads_inside_the_span() -> None:
    # The span is a window into a larger text, and a word after `end` belongs to
    # somebody else's span or to no span at all. Only the front is trimmed,
    # because the corpus shows no trailing over-capture.
    text = "Herr Martina Weber Herr"

    assert trim(text, 0, 18) == "Martina Weber"
    assert trim(text, 5) == "Martina Weber Herr"


def test_the_rule_works_with_no_articles_at_all() -> None:
    # The documented fallback: deleting `trim_leading_articles` from the
    # catalog leaves a rule that still fixes the case in the issue and takes
    # the whole class of article-versus-name collisions with it. Measured at
    # seven spans of the nineteen, and no change in what goes unmasked.
    #
    # It is a catalog edit rather than a code change, so it has to work with an
    # empty set and that is what this holds.
    def trimmed(text: str) -> str:
        at, to = trim_leading_words(
            text, 0, len(text), WORDS, frozenset(), piece_starts_a_word=True
        )
        return text[at:to]

    assert trimmed("Herr Börner") == "Börner"
    assert trimmed("Le Thi Mai") == "Le Thi Mai"
    # The price: an article in front of a role noun keeps both.
    assert trimmed("Der Kunde Karz") == "Der Kunde Karz"
