import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from tessera_detector.api import create_app
from tessera_detector.spans import Span

TEXT = "Contact: anna.keller@example.ch"
OPENAPI = Path(__file__).resolve().parents[2] / "docs" / "api" / "openapi.json"


class FakeDetector:
    """Stands in for Detector: the API contract is what is under test here."""

    def __init__(self, *, ner_available: bool = True, ner_off_reason: str | None = None) -> None:
        self.ner_available = ner_available
        self.ner_off_reason = ner_off_reason
        self.seen: list[str] = []

    def _span(self) -> Span:
        return Span(
            entity_type="EMAIL",
            start=9,
            end=31,
            confidence=0.95,
            recognizer="catalog:email",
            tier=2,
        )

    def detect(self, text: str) -> list[Span]:
        self.seen.append(text)
        return [self._span()]

    def deterministic_only(self, text: str) -> list[Span]:
        self.seen.append(text)
        return [self._span()]


def client(detector: object) -> TestClient:
    return TestClient(create_app(detector))  # type: ignore[arg-type]


def test_detect_returns_spans_and_the_layers_that_ran() -> None:
    response = client(FakeDetector()).post("/detect", json={"text": TEXT})
    assert response.status_code == 200
    body = response.json()
    assert body["spans"] == [
        {
            "entity_type": "EMAIL",
            "start": 9,
            "end": 31,
            "confidence": 0.95,
            "recognizer": "catalog:email",
            "tier": 2,
            "boosted": False,
        }
    ]
    assert body["layers_run"] == ["deterministic", "ner"]


def test_detect_reports_a_deterministic_only_run() -> None:
    detector = FakeDetector(ner_available=False, ner_off_reason="no weights")
    body = client(detector).post("/detect", json={"text": TEXT}).json()
    assert body["layers_run"] == ["deterministic"]


def test_requesting_an_unavailable_layer_is_503() -> None:
    # A quiet downgrade is how unredacted text reaches a provider while the
    # caller believes otherwise.
    detector = FakeDetector(ner_available=False, ner_off_reason="no weights")
    response = client(detector).post(
        "/detect", json={"text": TEXT, "layers": ["deterministic", "ner"]}
    )
    assert response.status_code == 503
    assert "no weights" in response.json()["detail"]


def test_narrowing_to_the_deterministic_layer_is_allowed() -> None:
    body = (
        client(FakeDetector())
        .post("/detect", json={"text": TEXT, "layers": ["deterministic"]})
        .json()
    )
    assert body["layers_run"] == ["deterministic"]


def test_an_unknown_layer_is_rejected() -> None:
    response = client(FakeDetector()).post("/detect", json={"text": TEXT, "layers": ["llm"]})
    assert response.status_code == 422


def test_empty_text_is_rejected() -> None:
    assert client(FakeDetector()).post("/detect", json={"text": ""}).status_code == 422


def test_errors_never_echo_the_submitted_text() -> None:
    # Originals are forbidden in logs and bodies at every level, and error
    # paths are where that is easiest to forget.
    secret = "Mandant Weber, IBAN CH9300762011623852957"
    detector = FakeDetector(ner_available=False, ner_off_reason="no weights")
    response = client(detector).post("/detect", json={"text": secret, "layers": ["ner"]})
    assert response.status_code == 503
    assert secret not in response.text
    assert "Weber" not in response.text


def test_health_reports_the_ner_layer() -> None:
    assert client(FakeDetector()).get("/health").json() == {
        "status": "ok",
        "ner": True,
        "ner_off_reason": None,
    }
    off = FakeDetector(ner_available=False, ner_off_reason="no runtime")
    assert client(off).get("/health").json() == {
        "status": "ok",
        "ner": False,
        "ner_off_reason": "no runtime",
    }


def test_the_detector_is_reused_across_requests() -> None:
    detector = FakeDetector()
    test_client = client(detector)
    test_client.post("/detect", json={"text": TEXT})
    test_client.post("/detect", json={"text": TEXT})
    assert len(detector.seen) == 2


@pytest.mark.parametrize("payload", [{}, {"text": None}, {"layers": ["deterministic"]}])
def test_malformed_bodies_are_rejected(payload: dict[str, object]) -> None:
    assert client(FakeDetector()).post("/detect", json=payload).status_code == 422


def test_committed_schema_matches_the_application() -> None:
    # REQ-44: one file in the repository, and it cannot drift from the code
    # without this failing.
    assert OPENAPI.exists(), "run `make openapi`"
    committed = json.loads(OPENAPI.read_text(encoding="utf-8"))
    assert committed == create_app(FakeDetector()).openapi()  # type: ignore[arg-type]


def test_schema_describes_the_span_shape() -> None:
    schemas = json.loads(OPENAPI.read_text(encoding="utf-8"))["components"]["schemas"]
    span = schemas["Span"]["properties"]
    assert set(span) == {
        "entity_type",
        "start",
        "end",
        "confidence",
        "recognizer",
        "tier",
        "boosted",
    }


@pytest.mark.ner
def test_the_real_detector_serves_detection() -> None:
    pytest.importorskip("gliner")
    from tessera_detector.models import find_model
    from tessera_detector.pipeline import build_detector

    if find_model() is None:
        pytest.skip("no NER weights: run `make model`")
    test_client = TestClient(create_app(build_detector()))
    body = test_client.post(
        "/detect", json={"text": "Der Mitarbeiter Weber leidet an Diabetes."}
    ).json()
    found = {span["entity_type"] for span in body["spans"]}
    assert "HEALTH" in found
    assert body["layers_run"] == ["deterministic", "ner"]


@pytest.mark.ner
def test_health_reports_a_loaded_layer() -> None:
    pytest.importorskip("gliner")
    from tessera_detector.models import find_model
    from tessera_detector.pipeline import build_detector

    if find_model() is None:
        pytest.skip("no NER weights: run `make model`")
    body = TestClient(create_app(build_detector())).get("/health").json()
    assert body == {"status": "ok", "ner": True, "ner_off_reason": None}
