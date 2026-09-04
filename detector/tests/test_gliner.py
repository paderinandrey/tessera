import pytest

from tessera_detector.models import find_model
from tessera_detector.ner import GlinerRecognizer, load_ner_types

pytestmark = pytest.mark.ner


@pytest.fixture(scope="module")
def recognizer() -> GlinerRecognizer:
    path = find_model()
    if path is None:
        pytest.skip("no NER weights: run `make model` or set TESSERA_NER_MODEL")
    try:
        return GlinerRecognizer(path)
    except ImportError:
        # Weights can be cached (e.g. from an earlier `make model`) even when the
        # ner dependency group isn't installed in the current environment.
        pytest.skip("gliner not installed: run `uv sync --group ner`")


def test_specificity_comes_from_the_config(recognizer: GlinerRecognizer) -> None:
    assert recognizer.specificity == {t.entity_type: t.specificity for t in load_ner_types()}


def test_finds_a_french_person_with_exact_offsets(recognizer: GlinerRecognizer) -> None:
    text = "Le dossier de Madame Amélie Rousseau est complet."
    spans = [s for s in recognizer.detect(text) if s.entity_type == "PERSON"]
    assert spans, "expected a PERSON span"
    span = spans[0]
    assert text[span.start : span.end] in {"Amélie Rousseau", "Madame Amélie Rousseau"}
    assert span.tier == 2
    assert span.recognizer == "ner:gliner"
    assert 0.0 < span.confidence <= 1.0


def test_finds_a_german_organization(recognizer: GlinerRecognizer) -> None:
    # ORG precision is advisory in the metrics gate, so this is the only place
    # a total ORG regression would be caught: assert the type, not "something".
    text = "Die Rechnung wurde von der Siemens AG in München bezahlt."
    spans = recognizer.detect(text)
    orgs = [s for s in spans if s.entity_type == "ORG"]
    assert orgs, f"expected an ORG span, got {[(s.entity_type, s.start, s.end) for s in spans]}"
    assert "Siemens" in text[orgs[0].start : orgs[0].end]
    assert "LOCATION" in {s.entity_type for s in spans}


def test_long_text_keeps_absolute_offsets(recognizer: GlinerRecognizer) -> None:
    filler = "Dies ist ein neutraler Satz ohne Namen.\n\n" * 60
    text = f"{filler}Der Mandant heißt Amélie Rousseau."
    spans = [s for s in recognizer.detect(text) if s.entity_type == "PERSON"]
    assert spans, "expected a PERSON span past the first chunk"
    assert all(text[s.start : s.end].strip() for s in spans)
    assert any(s.start > len(filler) // 2 for s in spans)


def test_scores_below_the_threshold_are_dropped(recognizer: GlinerRecognizer) -> None:
    strict = GlinerRecognizer(recognizer.model_path, types=tuple(
        type(t)(t.entity_type, t.label, 1.0, t.tier, t.specificity) for t in load_ner_types()
    ))
    assert strict.detect("Le dossier de Madame Amélie Rousseau est complet.") == []
    # This is also the real second construction in this process finding F's
    # fix has to survive: a sys.modules-diff dependency digest reports
    # nothing new here (everything is already imported) and would disagree
    # with `recognizer`'s own — the exact defect the fix replaced.
    assert strict.dependency_digest == recognizer.dependency_digest


def test_token_dense_text_is_still_inferred_to_its_end(recognizer: GlinerRecognizer) -> None:
    # Dense text packs more tokens per character, so a character-sized chunk can
    # blow past the model's window; the name at the very end must survive.
    dense = "東京大阪名古屋福岡札幌横浜京都神戸仙台広島" * 60
    text = f"{dense}Der Mandant heißt Amélie Rousseau."
    spans = [s for s in recognizer.detect(text) if s.entity_type == "PERSON"]
    assert spans, "the trailing name was never given to the model"
    assert any("Rousseau" in text[s.start : s.end] for s in spans)


def test_windows_stay_inside_the_model_token_budget(recognizer: GlinerRecognizer) -> None:
    # Re-tokenizing a slice can drift by a token or two against the same text
    # tokenized whole, so the invariant worth asserting is the one the model
    # imposes: the window plus the label prompt must fit its input window.
    dense = "東京大阪名古屋福岡札幌横浜京都神戸仙台広島" * 60
    tokenizer = recognizer._tokenizer
    limit = int(recognizer._model.config.max_len)
    prompt = len(tokenizer(" ".join(t.label for t in recognizer.types))["input_ids"])
    for start, end in recognizer._windows(dense):
        count = len(tokenizer(dense[start:end], add_special_tokens=False)["input_ids"])
        assert count + prompt <= limit, f"window {start}-{end} holds {count} tokens"


def test_finds_a_german_health_mention(recognizer: GlinerRecognizer) -> None:
    text = "Der Mitarbeiter Weber leidet an Diabetes und ist krankgeschrieben."
    spans = [s for s in recognizer.detect(text) if s.entity_type == "HEALTH"]
    assert spans, "expected a HEALTH span"
    assert "Diabetes" in text[spans[0].start : spans[0].end]
    assert spans[0].tier == 3


def test_finds_a_french_union_mention(recognizer: GlinerRecognizer) -> None:
    text = "Le salarié Dupont est adhérent de la CGT depuis trois ans."
    found = {s.entity_type for s in recognizer.detect(text)}
    assert "TRADE_UNION" in found, f"expected TRADE_UNION, got {sorted(found)}"


def test_an_article_9_mention_is_never_missed_by_the_whole_group(
    recognizer: GlinerRecognizer,
) -> None:
    # Which of the eight labels wins is second-order; that some label fires is
    # what keeps the span from reaching the model provider unredacted.
    # Taken from the configuration, not hardcoded: a list that drifts from
    # ner.yaml would fail for the wrong reason.
    article_9 = {t.entity_type for t in load_ner_types() if t.tier == 3}
    for text in (
        "Der Mandant Weber ist jüdisch und bittet um Rücksicht.",
        "Le dossier de Dupont, d'origine maghrébine, part au service juridique.",
        "Die Personalakte von Weber vermerkt: Mitglied der Grünen.",
    ):
        found = {s.entity_type for s in recognizer.detect(text)} & article_9
        assert found, f"no Article 9 span at all in: {text}"


def test_a_person_span_drops_the_role_noun_and_title_in_front_of_the_name(
    recognizer: GlinerRecognizer,
) -> None:
    # #20. A session keys on exact value equality, so `Dr. Martina Weber` in one
    # turn and `Frau Martina Weber` in the next are two values, two
    # placeholders, and one person presented to the model as two — the failure
    # the README's stability argument exists to prevent.
    #
    # Both must reduce to the same string. That is the whole fix: the mapping
    # cannot know two strings are one person, so the spans have to agree.
    first = "hier ist Dr. Martina Weber aus Zürich"
    second = "Frau Martina Weber hat die Unterlagen nachgereicht"

    def person(text: str) -> list[str]:
        return [text[s.start : s.end] for s in recognizer.detect(text) if s.entity_type == "PERSON"]

    assert person(first) == ["Martina Weber"]
    assert person(second) == ["Martina Weber"]


def test_a_person_span_drops_an_article_with_its_role_noun(
    recognizer: GlinerRecognizer,
) -> None:
    # Fifteen of the corpus's nineteen over-captures are role nouns rather than
    # titles, usually behind an article, which is why a list of honorifics alone
    # would have reached four of them.
    #
    # One sentence each, because a longer one costs the second name entirely:
    # the model's confidence for a name falls as the text around it grows and
    # `PERSON`'s threshold is a fixed 0.5 (#44). That is not what this test is
    # about, and folding both names into one sentence would have made it fail
    # for a reason it does not hold.
    def person(text: str) -> list[str]:
        return [text[s.start : s.end] for s in recognizer.detect(text) if s.entity_type == "PERSON"]

    assert person("Der Mandant Kuhl hat unterschrieben.") == ["Kuhl"]
    assert person("Sehr geehrter Herr Börner, anbei die Unterlagen.") == ["Börner"]


def test_the_lower_threshold_prices_a_role_phrase_and_still_masks_the_name(
    recognizer: GlinerRecognizer,
) -> None:
    """What `PERSON`'s 0.5 buys and what it costs, on the sentence that shows both.

    `mixed-0008` in the public corpus, verbatim. The model returns two `PERSON`
    spans here: `Hoareau` at 0.842, which the gold annotates, and `Der Kunde` at
    0.552, which it does not. The second exists only because the threshold is
    0.5; at 0.7 it is dropped.

    **The cost is over-masking and not a miss.** An earlier version of this
    claim — written in #48's review request — said the role phrase masks a
    phrase nobody needed masked *and leaves the name beside it*. That is wrong:
    the name has its own span, well clear of the bar, and both assertions are
    here so the distinction cannot quietly stop holding.

    This replaces a test that asserted `trim("Der Kunde") == "Der Kunde"`. That
    one called the trimming helper, read no catalog and asked the model nothing,
    so it passed at 0.5, 0.6 and 0.7 alike and duplicated
    `test_trimming_never_empties_a_span` one file over. It claimed to make the
    threshold's cost a fixture and could not have noticed the cost changing.

    Deliberately sensitive to the threshold: raising it back to 0.7 fails this
    test. A recorded price should have to be re-recorded when the thing being
    priced moves.
    """
    text = (
        "Der Kunde Hoareau de la société Texier S.A.R.L. demande un remboursement "
        "— IBAN FR76 3000 6000 0100 3853 4344 797."
    )
    people = sorted(
        (span.start, span.end)
        for span in recognizer.detect(text)
        if span.entity_type == "PERSON"
    )

    assert (10, 17) in people, "the name the gold annotates is not masked"
    assert text[10:17] == "Hoareau"
    assert (0, 9) in people, (
        "the role phrase the 0.5 threshold admits is gone — if the threshold moved, "
        "the price recorded in `ner.yaml` moved with it and needs re-measuring"
    )
    assert text[0:9] == "Der Kunde"
