"""What a request shares with every other request, and why each one is safe.

**The question nobody had asked, until it cost 17% of requests.** `api.detect`
is `def`, not `async def`, so Starlette runs it in a threadpool: one `Detector`,
built once at startup, serves every request concurrently. Everything reachable
from it is shared, and until #59 that included a HuggingFace fast tokenizer
whose `set_truncation_and_padding` mutates state PyO3 lends out one borrower at
a time.

That object had been there for months. Nothing was wrong with the reasoning that
put it there; there was simply no place where the reasoning had to be written
down, so nobody wrote it and nobody checked it.

This is that place, and it borrows the shape `provider.rs` uses for its
request-key allowlist: an entry may exist only if it says *why*. The tests below
enumerate the attributes that actually exist and fail when one appears with no
answer beside it, so the next shared object is a failing test the day it is
added rather than a 500 under load some time later.

**It cannot prove thread safety**, and says so rather than implying otherwise.
An entry is a claim by whoever wrote it. What is enforced is that the claim
exists, which is the part that was missing.
"""

from __future__ import annotations

import threading
import time

import pytest

# The serve group is optional, so the API half of this file must skip rather
# than fail collection under a bare `uv run pytest`.
fastapi = pytest.importorskip("fastapi")
from fastapi.testclient import TestClient  # noqa: E402

from tessera_detector.api import create_app  # noqa: E402
from tessera_detector.deterministic import DeterministicDetector  # noqa: E402
from tessera_detector.pipeline import Detector, build_detector  # noqa: E402
from tessera_detector.spans import Span  # noqa: E402

# Why each shared attribute is safe to share. One sentence, and it has to be an
# argument rather than a restatement of the name.
SHARED: dict[type, dict[str, str]] = {
    Detector: {
        "deterministic": "a `DeterministicDetector`, whose own entries are below",
        "recognizer": "a `GlinerRecognizer` or `None`; its entries are below too",
        "model_id": "a string, assigned once in `__init__` and never written again",
        "ner_off_reason": "a string or `None`, assigned once in `__init__`",
    },
    DeterministicDetector: {
        "catalog_text": "the bytes the rules were parsed from, assigned once and read",
        "rules": (
            "frozen rules holding compiled patterns. `re.Pattern` is safe for "
            "concurrent matching, nothing rebinds the list, and `detect` builds a "
            "local result — measured at four threads returning the serial answer"
        ),
    },
}

# `GlinerRecognizer` is described apart because importing it drags in the `ner`
# dependency group, which the base install deliberately does not carry.
GLINER_SHARED: dict[str, str] = {
    "_model": (
        "the ONNX session. Concurrent `Run` is safe by ONNX Runtime's own "
        "contract, and inference against inference ran clean on four threads"
    ),
    "_tokenizer": (
        "**this repository's own copy, and the copy is the reason**. Sharing the "
        "model's tokenizer let `set_truncation_and_padding` mutate an object "
        "another thread was borrowing: `RuntimeError: Already borrowed`, on 11 "
        "of 64 requests at eight concurrent (#59)"
    ),
    "_by_label": "a dict built in `__init__` and only ever read",
    "_token_budget": "an int, assigned once in `__init__`",
    "passes": "a tuple of frozen `InferencePass` values",
    "types": "a tuple of frozen `NerType` values",
    "specificity": "a mapping built in `__init__` and only ever read",
    "model_path": "a `Path`, assigned once in `__init__`",
    "dependency_digest": "a string, assigned once in `__init__`",
}


def test_every_shared_attribute_says_why_it_is_safe() -> None:
    detector = build_detector(ner=False)
    for owner, described in (
        (detector, Detector),
        (detector.deterministic, DeterministicDetector),
    ):
        actual = {name for name in vars(owner) if not name.startswith("__")}
        answered = set(SHARED[described])
        missing = actual - answered
        assert not missing, (
            f"{described.__name__} shares {sorted(missing)} with every concurrent "
            "request and says nothing about why that is safe. Add an entry to "
            "`SHARED` in this file — an argument, not a restatement of the name."
        )
        stale = answered - actual
        assert not stale, (
            f"{described.__name__} no longer has {sorted(stale)}; the answer outlived "
            "its question, which is how a table like this stops being read"
        )


def test_every_shared_answer_is_an_answer() -> None:
    # A one-word entry satisfies the test above and tells the next reader
    # nothing, which is the failure mode of every table like this.
    for described in (*SHARED.values(), GLINER_SHARED):
        for name, why in described.items():
            assert len(why.split()) >= 5, f"{name}: {why!r} is not an argument"


@pytest.mark.ner
def test_every_shared_attribute_of_the_recognizer_says_why() -> None:
    detector = build_detector()
    if detector.recognizer is None:
        pytest.skip(f"NER is not provisioned ({detector.ner_off_reason})")
    actual = {name for name in vars(detector.recognizer) if not name.startswith("__")}
    missing = actual - set(GLINER_SHARED)
    assert not missing, (
        f"the recognizer shares {sorted(missing)} with every concurrent request "
        "and says nothing about why that is safe"
    )
    stale = set(GLINER_SHARED) - actual
    assert not stale, f"the recognizer no longer has {sorted(stale)}"


def test_the_deterministic_layer_agrees_with_itself_across_threads() -> None:
    """Not raising is half the question.

    Two threads can each come back without an exception and one of them be
    wrong, so this compares every concurrent answer against the serial one. It
    is what turned "the deterministic layer looks pure" into something measured,
    and it needs no model.
    """
    detector = build_detector(ner=False)
    texts = [
        "IBAN DE44 5001 0517 5407 3249 31 und Steuernummer 419/130/29933.",
        "Carte 4111 1111 1111 1111, NIR 1 71 07 10 830 660 47.",
        "AHV 756.1234.5678.97 und Steuer-ID 44 123 456 789.",
    ]

    def answer(text: str) -> list[tuple[str, int, int, str, float, bool]]:
        return sorted(
            (s.entity_type, s.start, s.end, s.recognizer, round(s.confidence, 9), s.boosted)
            for s in detector.deterministic.detect(text)
        )

    expected = {text: answer(text) for text in texts}
    wrong: list[str] = []
    stop = threading.Event()

    def worker() -> None:
        index = 0
        while not stop.is_set():
            text = texts[index % len(texts)]
            index += 1
            try:
                if answer(text) != expected[text]:
                    wrong.append(f"a different answer for {text[:32]!r}")
                    return
            except Exception as error:
                wrong.append(f"{type(error).__name__}: {error}")
                return

    threads = [threading.Thread(target=worker) for _ in range(4)]
    for thread in threads:
        thread.start()
    time.sleep(2)
    stop.set()
    for thread in threads:
        thread.join(timeout=30)

    assert not wrong, wrong[0]


TEXTS = [
    "Contact: anna.keller@example.ch about IBAN CH93 0076 2011 6238 5295 7",
    "Steuernummer 419/130/29933, Rechnung vom 14.03.",
    "Le NIR 2 84 11 20 102 728 71 figure au dossier de Fischer.",
    "Bitte an m.wolf@example.de senden, Kundennummer 88213.",
    # A shape the service *refuses*, kept in the run on purpose — and worth
    # exactly what it is worth, no more. `DetectRequest.text` has
    # `min_length=1`, so pydantic rejects this before the endpoint body runs and
    # before `Depends(get_detector)` resolves: **it never touches the shared
    # detector at all.** What it does check is that a refusal stays a refusal
    # under load, which is a claim about the service and not about sharing. It
    # is one of five texts rather than the point of the fixture, and the
    # docstring above should not be read as saying otherwise.
    "",
]


def hammer(detector: object, rounds: int = 16) -> list[str]:
    """Ask the service the same questions alone and then all at once, and return
    the texts whose answer changed.

    **It compares answers rather than status codes**, which is the whole design.
    A hammer checking for 200s catches a shared object that *raises* — which is
    what the tokenizer did, by luck of PyO3 panicking rather than returning
    nonsense. A shared object that quietly hands one request another's state
    gives 200s all day. The sequential answers are taken first, so the
    comparison is against a known value rather than against whatever the threads
    agreed on among themselves.
    """
    app = create_app(detector)
    with TestClient(app) as sequential:
        alone = {}
        for text in TEXTS:
            response = sequential.post("/detect", json={"text": text})
            alone[text] = (response.status_code, response.json())

    changed: list[str] = []
    lock = threading.Lock()

    def ask(text: str) -> None:
        # A client each, so the concurrency is in the service rather than in one
        # client's connection pool.
        with TestClient(app) as one:
            response = one.post("/detect", json={"text": text})
        with lock:
            if (response.status_code, response.json()) != alone[text]:
                changed.append(text)

    threads = [
        threading.Thread(target=ask, args=(TEXTS[i % len(TEXTS)],))
        for i in range(rounds * len(TEXTS))
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return changed


class SharesItsState:
    """A detector with the defect this whole file exists for, kept so the hammer
    above has something it is known to catch.

    #60 asks for a concurrency test and then says why the obvious one is not
    enough: **a 24-request load test passed against the build that was failing
    17% of requests by hand.** A hammer nobody has ever seen fail is a hammer
    tuned to nothing. This is the known-unsafe build the issue says has to be
    kept around, and it is three lines rather than a git revert.

    The shape is the one that actually happened: per-call state parked on an
    object that outlives the call.
    """

    ner_available = False
    ner_off_reason = "no weights in this fixture"
    model_id = "unsafe-fixture@0"
    catalog_text = "unsafe-fixture"

    def __init__(self) -> None:
        self._text = ""

    def _detect(self, text: str) -> list[Span]:
        self._text = text
        # Long enough that another thread lands between the write and the read,
        # which is what makes this deterministic rather than a coin toss.
        time.sleep(0.01)
        return [
            Span(
                entity_type="EMAIL",
                start=0,
                end=len(self._text),
                confidence=0.9,
                recognizer="catalog:email",
                tier=2,
            )
        ]

    def detect(self, text: str) -> list[Span]:
        return self._detect(text)

    def deterministic_only(self, text: str) -> list[Span]:
        return self._detect(text)


def test_the_hammer_catches_a_detector_that_shares_its_state() -> None:
    """The gate on the gate. Without this, the tests below have never failed and
    nobody knows what it would take.

    **What it proves is that the comparison is right, not that the hammer is
    sensitive.** `SharesItsState` is caught because it holds the window open for
    10ms; take the sleep out and this test fails against a detector that is still
    genuinely broken. That is the honest shape of every concurrency test and it
    is why #59's real race needed 64 calls to show itself.

    So: a defect that corrupts an answer will be *reported correctly* rather than
    passed over as a 200. Whether a given defect is hit at all remains luck, and
    no arrangement of N and M changes that.
    """
    changed = hammer(SharesItsState(), rounds=4)
    assert changed, "the hammer did not notice a detector parking per-call state on itself"


def test_the_api_answers_the_same_under_concurrency_as_alone() -> None:
    """**The gap this file's own docstring left open**: nothing sent the service
    two requests at once.

    The tests above enumerate what a request shares and exercise the
    *deterministic layer* across threads. Neither goes through `api.detect`,
    which is where the sharing actually happens — `def`, not `async def`, so
    Starlette runs it in a threadpool over one `Detector` built at startup.

    **This does not prove thread safety and no hammer does.** #59's race needed
    64 calls at eight concurrent, and a 24-call load test passed against the
    unfixed build. What it closes is the "nothing exercises the endpoint
    concurrently at all" gap.

    Unmarked, so it runs in the job that has no weights: there it is the
    deterministic path, and it costs 0.8s. The NER path gets
    `test_the_api_answers_the_same_under_concurrency_with_ner` below, because a
    test that changes which code it covers depending on what is in a cache is
    not one anybody can reason about from CI.
    """
    detector = build_detector()
    if detector.ner_available:
        pytest.skip("weights are present, so the marked test below is the one that applies")
    assert hammer(detector) == []


@pytest.mark.ner
def test_the_api_answers_the_same_under_concurrency_with_ner() -> None:
    """The same hammer over the layer that actually broke.

    #59's race was in the recognizer's tokenizer, not in the catalog, so the
    deterministic run above would not have caught it whatever N and M were. This
    is the half #60 priced at 55s and declined — it is a few seconds now,
    because the hammer reuses one application rather than building a detector
    per request, and because it asks a question it can answer cheaply rather
    than trying to reproduce a race by volume.
    """
    detector = build_detector()
    if detector.recognizer is None:
        pytest.skip(f"NER is not provisioned ({detector.ner_off_reason})")
    assert hammer(detector, rounds=8) == []
