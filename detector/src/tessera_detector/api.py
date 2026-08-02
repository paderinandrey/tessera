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
        if not hasattr(app.state, "detector"):
            app.state.detector = build_detector()
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
        spans = (
            detector.detect(body.text)
            if "ner" in requested
            else detector.deterministic_only(body.text)
        )
        return DetectResponse(spans=spans, layers_run=list(requested))

    @app.get("/health", response_model=HealthResponse)
    def health(detector: Annotated[Detector, Depends(get_detector)]) -> HealthResponse:
        return HealthResponse(
            status="ok",
            ner=detector.ner_available,
            ner_off_reason=detector.ner_off_reason,
        )

    if detector is not None:
        # An injected detector is already built, so there is nothing to defer:
        # assigning it here also lets a caller drive the app without running
        # the lifespan, which is how the tests avoid loading a model.
        app.state.detector = detector
    return app


__all__ = ["DetectRequest", "DetectResponse", "HealthResponse", "create_app", "get_detector"]
