import os
import threading
import time
from collections.abc import Callable

import pytest

import tessera_detector.ner as ner_module
from tessera_detector.models import find_model
from tessera_detector.ner import GlinerRecognizer, load_ner_types
from tessera_detector.spans import Span

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


def test_windowing_and_inference_may_run_at_the_same_time(
    recognizer: GlinerRecognizer,
) -> None:
    """The service is a sync route, so requests already arrive on threads.

    `api.detect` is `def`, not `async def`, so Starlette runs it in a threadpool
    and two requests share one `GlinerRecognizer`. That was safe until you look
    at what it shares: one HuggingFace fast tokenizer. `transformers` calls
    `set_truncation_and_padding` before each encode, which **mutates** it when
    the strategy changes — and the two callers here want different strategies:
    `_windows` asks for offset mappings unpadded, GLiNER pads for batching.

    So every call flips the state, every flip is a mutable borrow of a Rust
    object, and a concurrent reader dies with `RuntimeError: Already borrowed`.
    Measured through the real app before the fix: 11 of 64 requests failed at
    eight concurrent, 22 of 128 at sixteen.

    **Neither caller races with itself**, because repeat calls in one strategy
    skip the mutation. Isolating either one found nothing across hundreds of
    calls, and a load test through `detect` — 24 requests on eight threads —
    passed against the unfixed code too. It takes the two *interleaved* at rate,
    so this drives that directly instead of hoping a load test lands on it: the
    same loop reproduces in about a tenth of a second when the object is shared.
    """
    text = "Sehr geehrter Herr Röhrdanz, die Kundin Martina Weber aus Zürich rief an."
    failures: list[str] = []
    stop = threading.Event()

    def until_stopped(work: Callable[[], object]) -> Callable[[], None]:
        def run() -> None:
            while not stop.is_set():
                try:
                    work()
                except Exception as error:
                    failures.append(f"{type(error).__name__}: {error}")
                    return

        return run

    threads = [
        threading.Thread(target=until_stopped(lambda: list(recognizer.windows(text)))),
        threading.Thread(
            target=until_stopped(
                lambda: list(recognizer._spans_from(0, text, recognizer.passes[0]))
            )
        ),
    ]
    for thread in threads:
        thread.start()
    # Long enough that the shared-object failure is a near certainty and short
    # enough to sit in a test suite. Two orders of magnitude over the observed
    # time to first failure.
    time.sleep(2)
    stop.set()
    for thread in threads:
        thread.join(timeout=30)

    assert not failures, f"windowing and inference cannot run together: {failures[0]}"


def test_the_windowing_tokenizer_is_not_the_model_s(recognizer: GlinerRecognizer) -> None:
    # The contract the test above rests on, stated where someone refactoring
    # `__init__` will trip over it. Making these one object again is what the
    # concurrency failure was.
    assert recognizer._tokenizer is not recognizer._model.data_processor.transformer_tokenizer


def test_dispatching_windows_to_threads_changes_no_answer(
    recognizer: GlinerRecognizer,
) -> None:
    """The same list, in order — not merely the same set.

    `resolve` folds over spans in sorted order, and a fold is order-dependent
    unless its operation is associative, which #39 established this one is not.
    So "the same spans" is not the claim that matters; `map` preserving
    submission order is, and this checks it on a text long enough to fill
    several batches.
    """
    text = "Sehr geehrter Herr Röhrdanz, die Kundin Martina Weber aus Zürich rief an. " * 20

    def without_the_pool() -> list[Span]:
        spans: list[Span] = []
        for base, piece in recognizer.windows(text):
            at_boundary = base == 0 or text[base - 1].isspace()
            for inference in recognizer.passes:
                spans.extend(
                    recognizer._spans_from(base, piece, inference, at_boundary=at_boundary)
                )
        return spans

    def shape(spans: list[Span]) -> list[tuple[str, int, int, float, bool]]:
        return [(s.entity_type, s.start, s.end, s.confidence, s.boosted) for s in spans]

    assert shape(recognizer.detect(text)) == shape(without_the_pool())


def test_one_text_never_submits_more_than_the_bound(
    recognizer: GlinerRecognizer, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The bound applied, not the bound advertised.

    A large document queueing its whole self ahead of the next request is what
    this exists to stop: measured before the bound existed, four small requests
    arriving behind one large one went from 0.47s to 3.31s.

    **An earlier version of this test asserted a relationship between two
    constants**, which is a statement about arithmetic and not about the
    executor. It passed while `detect` submitted `_IN_FLIGHT + passes - 1` jobs
    at a time, because the drain sat after the pass loop rather than inside it —
    the cap real in the comment and absent from the queue. Found in review of
    #62, and this is the version that would have caught it.
    """
    submitted: list[int] = []
    real_map = ner_module._INFERENCE_POOL.map

    def counting_map(fn, jobs, *args, **kwargs):  # type: ignore[no-untyped-def]
        jobs = list(jobs)
        submitted.append(len(jobs))
        return real_map(fn, jobs, *args, **kwargs)

    monkeypatch.setattr(ner_module._INFERENCE_POOL, "map", counting_map)
    # Long enough for several batches; short enough to stay a unit test.
    recognizer.detect("Frau Martina Weber aus Zürich rief an. " * 40)

    assert submitted, "the pool was never used"
    assert max(submitted) <= ner_module._IN_FLIGHT, (
        f"one text submitted {max(submitted)} inferences at once against a bound of "
        f"{ner_module._IN_FLIGHT}; the batch was measured after a whole window's "
        "passes were appended rather than while they were"
    )
    # No lower bound asserted here. `_in_flight` deliberately returns 1 on a
    # single-CPU deployment, and a `>= 2` left behind from an earlier round
    # would fail the whole NER suite in exactly the environment the sizing was
    # taught to support. Found in review of #63.


def test_the_pool_is_sized_by_what_this_process_may_use() -> None:
    # The sizing rule itself — every wrong answer it has had, and why each was
    # untestable from the machine the tests run on — is in
    # `test_pool_sizing.py`, which drives the parser and the override rather
    # than looking at whatever this host happens to be.
    #
    # What is left here is the relationship, asserted so it says something true
    # on this laptop and on a two-CPU container alike.
    # **Unless a deployment said otherwise**, which beats every detected limit
    # by design — so asserting the pool is no larger than this host's CPUs makes
    # the suite fail on a four-CPU runner with `TESSERA_DETECT_WORKERS=8`,
    # against a rule the same change documents. Found in review of #63.
    if os.environ.get(ner_module._WORKERS_ENV) is None:
        allowed = os.process_cpu_count() or 1
        assert max(1, allowed) >= ner_module._POOL_SIZE
    assert max(1, ner_module._POOL_SIZE // 2) >= ner_module._IN_FLIGHT
    assert ner_module._IN_FLIGHT >= 1
