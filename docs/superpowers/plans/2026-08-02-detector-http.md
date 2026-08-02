# Detector HTTP Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `POST /detect` and `GET /health` over the existing detector, plus a committed OpenAPI document that becomes the schema both implementations share.

**Architecture:** A new module `detector/src/tessera_detector/api.py` holding the FastAPI application and its models. The detector is built once in a lifespan handler and reached through a dependency, so tests inject a fake and never load a model. `tessera serve` starts uvicorn.

**Tech Stack:** FastAPI and uvicorn in a new optional `serve` dependency group, over the existing `Detector`, `build_detector` and `Span`.

**Spec:** `docs/superpowers/specs/2026-08-02-detector-http-design.md`

## Global Constraints

- Two endpoints only: `POST /detect`, `GET /health`. Nothing else is added on speculation.
- Every `/detect` response carries `layers_run`. A deterministic-only result must never be indistinguishable from a full scan.
- A layer requested but unavailable is **503 with the reason**, never a quiet downgrade.
- `layers` may only narrow what the server can run (REQ-40): asking for something the server does not have is an error, not an escalation.
- **No submitted text in any response body or log line, at any level** — including error paths, which is where it is easiest to forget.
- The detector is constructed once at startup, never per request.
- Dependencies go in an optional `serve` group only; `import tessera_detector.api` must not be required for the base install to work, and nothing outside `api.py` may import FastAPI.
- ruff line-length 100; gates from the repo root: `make test`, `make lint`, `make evaluate`. mypy is strict.
- Commit message style: one-line `API: <what>` with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run tests from `detector/`: `uv run --group serve pytest tests/test_api.py -v`.

---

### Task 1: The application, its models and the two endpoints

**Files:**
- Create: `detector/src/tessera_detector/api.py`
- Create: `detector/tests/test_api.py`
- Modify: `detector/pyproject.toml` (the `serve` dependency group)

**Interfaces:**
- Consumes: `Detector` (`.detect(text) -> list[Span]`, `.ner_available`, `.ner_off_reason`) and `build_detector` from `tessera_detector.pipeline`; `Span` from `tessera_detector.spans`.
- Produces: `DetectRequest`, `DetectResponse`, `HealthResponse` (pydantic models); `create_app(detector: Detector | None = None) -> FastAPI`; `get_detector()` dependency. Task 2 exports the schema from `create_app()`, Task 3 serves it.

- [ ] **Step 1: Declare the dependency group**

In `detector/pyproject.toml`, add to `[dependency-groups]`:

```toml
serve = [
    "fastapi>=0.120",
    "uvicorn>=0.40",
    "httpx>=0.28",
]
```

`httpx` is what FastAPI's `TestClient` needs; it is a test-time dependency of the same group so the tests can run wherever the server can.

- [ ] **Step 2: Write the failing tests**

```python
# detector/tests/test_api.py
import pytest
from fastapi.testclient import TestClient

from tessera_detector.api import create_app
from tessera_detector.pipeline import Detector
from tessera_detector.spans import Span

TEXT = "Contact: anna.keller@example.ch"


class FakeDetector:
    """Stands in for Detector: the API contract is what is under test here."""

    def __init__(self, *, ner_available: bool = True, ner_off_reason: str | None = None) -> None:
        self.ner_available = ner_available
        self.ner_off_reason = ner_off_reason
        self.seen: list[str] = []

    def detect(self, text: str) -> list[Span]:
        self.seen.append(text)
        return [
            Span(
                entity_type="EMAIL",
                start=9,
                end=31,
                confidence=0.95,
                recognizer="catalog:email",
                tier=2,
            )
        ]


def client(detector: object) -> TestClient:
    return TestClient(create_app(detector))


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
    body = client(FakeDetector()).post(
        "/detect", json={"text": TEXT, "layers": ["deterministic"]}
    ).json()
    assert body["layers_run"] == ["deterministic"]


def test_an_unknown_layer_is_rejected() -> None:
    response = client(FakeDetector()).post(
        "/detect", json={"text": TEXT, "layers": ["llm"]}
    )
    assert response.status_code == 422


def test_empty_text_is_rejected() -> None:
    assert client(FakeDetector()).post("/detect", json={"text": ""}).status_code == 422


def test_errors_never_echo_the_submitted_text() -> None:
    # Originals are forbidden in logs and bodies at every level, and error
    # paths are where that is easiest to forget.
    secret = "Mandant Weber, IBAN CH9300762011623852957"
    detector = FakeDetector(ner_available=False, ner_off_reason="no weights")
    response = client(detector).post(
        "/detect", json={"text": secret, "layers": ["ner"]}
    )
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd detector && uv sync --group serve && uv run --group serve pytest tests/test_api.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'tessera_detector.api'`.

- [ ] **Step 4: Write the application**

```python
# detector/src/tessera_detector/api.py
"""HTTP contract for the detector (MVP roadmap Release 2, REQ-44).

Two endpoints, and a response that always says which layers actually ran: a
deterministic-only result must not be mistakable for a full scan, because that
mistake is how unredacted text reaches a model provider.

FastAPI lives here and nowhere else in the package, so the base install stays
free of it.
"""

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Annotated, Literal

from fastapi import Depends, FastAPI, HTTPException, Request
from pydantic import BaseModel, Field

from .pipeline import Detector, build_detector
from .spans import Span

Layer = Literal["deterministic", "ner"]


class DetectRequest(BaseModel):
    text: str = Field(min_length=1, description="Text to scan, in original coordinates")
    layers: list[Layer] | None = Field(
        default=None,
        description="Layers to run. May only narrow what the server runs; omit for all.",
    )


class DetectResponse(BaseModel):
    spans: list[Span]
    layers_run: list[Layer] = Field(description="Layers that actually ran for this request")


class HealthResponse(BaseModel):
    status: Literal["ok"]
    ner: bool
    ner_off_reason: str | None


def get_detector(request: Request) -> Detector:
    detector: Detector = request.app.state.detector
    return detector


def create_app(detector: Detector | None = None) -> FastAPI:
    @asynccontextmanager
    async def lifespan(app: FastAPI) -> AsyncIterator[None]:
        # Built once: loading the model takes seconds, and a per-request build
        # would pay that on every call.
        app.state.detector = detector if detector is not None else build_detector()
        yield

    app = FastAPI(
        title="Tessera detector",
        version="0.1.0",
        summary="PII detection over text, in original coordinates",
        lifespan=lifespan,
    )

    @app.post("/detect", response_model=DetectResponse)
    def detect(
        body: DetectRequest, detector: Annotated[Detector, Depends(get_detector)]
    ) -> DetectResponse:
        available: list[Layer] = ["deterministic"]
        if detector.ner_available:
            available.append("ner")
        requested = body.layers if body.layers is not None else available
        missing = [layer for layer in requested if layer not in available]
        if missing:
            # Fail closed, and name the reason rather than the text.
            raise HTTPException(
                status_code=503,
                detail=(
                    f"layer(s) {', '.join(missing)} unavailable: "
                    f"{detector.ner_off_reason or 'not configured'}"
                ),
            )
        if "ner" in requested:
            spans = detector.detect(body.text)
        else:
            spans = detector.deterministic_only(body.text)
        return DetectResponse(spans=spans, layers_run=list(requested))

    @app.get("/health", response_model=HealthResponse)
    def health(detector: Annotated[Detector, Depends(get_detector)]) -> HealthResponse:
        return HealthResponse(
            status="ok",
            ner=detector.ner_available,
            ner_off_reason=detector.ner_off_reason,
        )

    return app


__all__ = ["DetectRequest", "DetectResponse", "HealthResponse", "create_app", "get_detector"]
```

- [ ] **Step 5: Add the narrowed detection path**

`Detector` resolves over whichever layers it holds; running the deterministic layer alone still has to resolve, or the caller gets overlapping spans the pipeline promised to arbitrate. Add to `detector/src/tessera_detector/pipeline.py`, inside `Detector`:

```python
    def deterministic_only(self, text: str) -> list[Span]:
        """Resolved spans from the catalog layer alone.

        Narrowing to one layer must not also drop conflict resolution: the
        caller asked for fewer detectors, not for raw overlapping spans.
        """
        spans = self.deterministic.detect(text)
        return resolve(spans, specificity=self.deterministic.specificity).spans
```

and add a test to `detector/tests/test_pipeline.py`:

```python
def test_deterministic_only_still_resolves_conflicts() -> None:
    detector = Detector(recognizer=FakeRecognizer([person(0, 5)]))
    spans = detector.deterministic_only("Keller a écrit à anna.keller@example.ch")
    assert [s.entity_type for s in spans] == ["EMAIL"], "the NER span must not appear"
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd detector && uv run --group serve pytest tests/test_api.py tests/test_pipeline.py -v`
Expected: all PASS. Then `uv run ruff check . ../evaluation && uv run mypy src`.

If mypy objects to `app.state.detector` being untyped, keep the annotation in `get_detector` and leave `state` alone — `Starlette.state` is deliberately dynamic and casting it there is the smallest honest fix.

- [ ] **Step 7: Commit**

```bash
git add detector/src/tessera_detector/api.py detector/src/tessera_detector/pipeline.py \
        detector/tests/test_api.py detector/tests/test_pipeline.py \
        detector/pyproject.toml detector/uv.lock
git commit -m "API: detect and health, every response names the layers that ran

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: The OpenAPI document as a committed artefact

**Files:**
- Create: `docs/api/openapi.json`
- Create: `evaluation/export_openapi.py`
- Modify: `Makefile`
- Modify: `.github/workflows/ci.yml`
- Modify: `detector/tests/test_api.py`

**Interfaces:**
- Consumes: `create_app` from Task 1.
- Produces: `docs/api/openapi.json`, regenerated by `make openapi` and diffed in CI.

**Why a script rather than a test fixture:** REQ-44 wants one file in the repository that is the source of truth for both implementations. A file that only exists inside a test run cannot be read by a Rust build.

- [ ] **Step 1: Write the failing test**

Append to `detector/tests/test_api.py`:

```python
import json
from pathlib import Path

OPENAPI = Path(__file__).resolve().parents[2] / "docs" / "api" / "openapi.json"


def test_committed_schema_matches_the_application() -> None:
    # REQ-44: one file in the repository, and it cannot drift from the code
    # without this failing.
    assert OPENAPI.exists(), "run `make openapi`"
    committed = json.loads(OPENAPI.read_text(encoding="utf-8"))
    assert committed == create_app(FakeDetector()).openapi()


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
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd detector && uv run --group serve pytest tests/test_api.py -k schema -v`
Expected: FAIL on the missing file.

- [ ] **Step 3: Write the exporter**

```python
# evaluation/export_openapi.py
"""Write the detector's OpenAPI document to docs/api/openapi.json (REQ-44).

The committed file is what the Rust gateway reads: a schema that lives only
inside a test run is not a source of truth for another language.

Run from the repository root:  make openapi
"""

import json
from pathlib import Path

from tessera_detector.api import create_app

TARGET = Path(__file__).resolve().parents[1] / "docs" / "api" / "openapi.json"


def main() -> int:
    TARGET.parent.mkdir(parents=True, exist_ok=True)
    # No detector is constructed: the schema comes from the models, and
    # building one here would load the model for nothing.
    schema = create_app(detector=object()).openapi()  # type: ignore[arg-type]
    TARGET.write_text(json.dumps(schema, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {TARGET}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Add the Makefile target**

Add `openapi` to `.PHONY` and this target after `bench`:

```make
openapi:
	uv run --project detector --group serve python evaluation/export_openapi.py
```

- [ ] **Step 5: Generate and verify**

Run from the repo root: `make openapi`, then `cd detector && uv run --group serve pytest tests/test_api.py -v`.
Expected: all pass. Run `make openapi` twice and `git diff --exit-code docs/api` — the output must be byte-identical between runs, which `sort_keys=True` is there to guarantee.

- [ ] **Step 6: Gate it in CI**

In `.github/workflows/ci.yml`, in the `detector` job after the corpus determinism check:

```yaml
      # The committed OpenAPI document is the schema the gateway will read
      # (REQ-44); it must not drift from the application.
      - run: uv run --group serve python ../evaluation/export_openapi.py
      - run: git diff --exit-code ../docs/api
```

and add `--group serve` to that job's `uv sync --locked` line so the group is installed.

- [ ] **Step 7: Commit**

```bash
git add docs/api/openapi.json evaluation/export_openapi.py Makefile \
        .github/workflows/ci.yml detector/tests/test_api.py
git commit -m "API: commit the OpenAPI document, CI gates its drift

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `tessera serve`

**Files:**
- Modify: `detector/src/tessera_detector/cli.py`
- Modify: `detector/tests/test_cli.py`
- Modify: `README.md`

**Interfaces:**
- Consumes: `create_app` from Task 1.
- Produces: the `serve` subcommand.

- [ ] **Step 1: Write the failing tests**

Append to `detector/tests/test_cli.py`:

```python
def test_serve_subcommand_exists_and_takes_host_and_port(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    started: dict[str, object] = {}

    def fake_run(app: object, *, host: str, port: int) -> None:
        started["host"] = host
        started["port"] = port

    monkeypatch.setattr("uvicorn.run", fake_run)
    assert main(["serve", "--host", "127.0.0.1", "--port", "9001"]) == 0
    assert started == {"host": "127.0.0.1", "port": 9001}


def test_serve_defaults_to_localhost(monkeypatch: pytest.MonkeyPatch) -> None:
    # Binding every interface by default would expose a service that sees
    # personal data to whatever network the machine is on.
    started: dict[str, object] = {}
    monkeypatch.setattr(
        "uvicorn.run",
        lambda app, *, host, port: started.update(host=host, port=port),
    )
    assert main(["serve"]) == 0
    assert started["host"] == "127.0.0.1"
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd detector && uv run --group serve pytest tests/test_cli.py -k serve -v`
Expected: FAIL — argparse exits 2 on the unknown `serve` command.

- [ ] **Step 3: Add the subcommand**

In `detector/src/tessera_detector/cli.py`, inside `main()` after the `scan` subparser is configured:

```python
    serve_parser = subparsers.add_parser("serve", help="run the detection HTTP service")
    serve_parser.add_argument("--host", default="127.0.0.1", help="interface to bind")
    serve_parser.add_argument("--port", type=int, default=8000, help="port to bind")
```

and, at the start of the command dispatch — before the `scan`-specific handling that reads `args.path`:

```python
    if args.command == "serve":
        # Imported here: the serve group is optional, and `tessera scan` must
        # keep working without it.
        import uvicorn

        from .api import create_app

        uvicorn.run(create_app(), host=args.host, port=args.port)
        return 0
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd detector && uv run --group serve pytest tests/test_cli.py -v`, then the whole suite without the group: `uv run pytest tests/test_cli.py -q`.
Expected: with the group, all pass; without it, the two serve tests fail on the missing `uvicorn` import — so mark them `@pytest.mark.serve` and register the marker in `pyproject.toml` beside `ner`:

```toml
markers = [
    "ner: needs GLiNER weights (skipped when absent)",
    "serve: needs the serve dependency group",
]
```

and skip them when uvicorn is absent, with the same shape the `ner` fixture uses:

```python
pytest.importorskip("uvicorn")
```

placed inside each serve test.

- [ ] **Step 5: Document it**

Add to `README.md`, in the CLI section after the scan paragraphs:

```markdown
The same binary serves the detection HTTP contract the gateway will call:

```
uv run --project detector --group serve tessera serve      # 127.0.0.1:8000
```

`POST /detect` takes `{"text": "...", "layers": ["deterministic", "ner"]}` — `layers` is
optional and may only narrow what the server runs — and every response reports
`layers_run`, so a deterministic-only result is never mistakable for a full scan.
`GET /health` reports whether the NER layer is loaded and why it is not. The committed
OpenAPI document at `docs/api/openapi.json` is the schema both implementations share
(REQ-44); `make openapi` regenerates it and CI fails if it drifts.
```

- [ ] **Step 6: Run the gates and commit**

Run from the repo root: `make test && make lint && make evaluate`.
Expected: all pass; the metrics are untouched by this task.

```bash
git add detector/src/tessera_detector/cli.py detector/tests/test_cli.py \
        detector/pyproject.toml README.md
git commit -m "API: tessera serve, bound to localhost by default

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: The service against real weights

**Files:**
- Modify: `detector/tests/test_api.py`

**Interfaces:** consumes `create_app` and `build_detector`; adds no production code.

- [ ] **Step 1: Write the tests**

Append to `detector/tests/test_api.py`:

```python
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
```

- [ ] **Step 2: Run them**

Run: `cd detector && uv run --group serve --group ner pytest tests/test_api.py -m ner -v` with weights installed.
Expected: both pass. Without weights they skip; without the serve group the file does not import, so add `--group serve` when running them.

- [ ] **Step 3: Add them to the CI ner job**

In `.github/workflows/ci.yml`, change the `ner` job's sync to `uv sync --locked --group ner --group serve` and its marked-test step to `uv run --group ner --group serve pytest -m ner`.

- [ ] **Step 4: Run every gate and commit**

Run: `make test && make lint && make evaluate`, plus `make openapi && git diff --exit-code docs/api`.

```bash
git add detector/tests/test_api.py .github/workflows/ci.yml
git commit -m "API: model-backed service tests

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After the plan

Push `feat/detector-http`, open a PR to `main`, comment `@codex review`, and keep the fix → tag → wait loop going until Codex reviews the current HEAD with no findings attached. The clean verdict arrives either as an issue comment saying `Didn't find any major issues` or as a review on the current commit carrying no inline comments — check both.
