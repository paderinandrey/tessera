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
