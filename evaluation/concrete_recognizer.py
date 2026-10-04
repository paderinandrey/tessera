"""Narrow `Detector.recognizer` to the one these scripts actually measure.

`NerRecognizer` is a deliberately small protocol — `detect` plus `specificity` —
so an application can supply its own recognizer and the pipeline neither knows
nor cares which. The scripts in this directory reach past it, into `passes`,
`types` and `windows`, because measuring the *asking shape* means holding the
thing that asks.

Narrowed in one place, and loudly, so each script says once that it needs the
real one rather than quietly assuming it. Before this existed the scripts simply
indexed attributes the protocol does not declare, which type-checking would have
reported had it ever run over this directory — and it had not, which is how
`threshold_bootstrap.py` called a three-argument function with two for a month.
"""

from tessera_detector.ner import GlinerRecognizer
from tessera_detector.pipeline import Detector


def gliner_recognizer(detector: Detector) -> GlinerRecognizer:
    recognizer = detector.recognizer
    if not isinstance(recognizer, GlinerRecognizer):
        raise SystemExit(
            "this measurement needs the packaged GLiNER recognizer, and the detector "
            f"holds {type(recognizer).__name__}: run with `--group ner` and the pinned "
            f"weights (`make model`){'' if recognizer else f' — {detector.ner_off_reason}'}"
        )
    return recognizer
