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

from tessera_detector.deterministic import DeterministicDetector
from tessera_detector.pipeline import Detector, build_detector

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
