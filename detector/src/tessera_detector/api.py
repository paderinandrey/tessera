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
from fastapi.encoders import jsonable_encoder
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field, model_validator

from .pipeline import Detector, build_detector
from .spans import Span

Layer = Literal["deterministic", "ner"]


class DetectRequest(BaseModel):
    text: str = Field(min_length=1, description="Text to scan, in original coordinates")
    layers: list[Layer] | None = Field(
        default=None,
        description=(
            "Layers to run, omit for all. May only narrow what the server runs, and must "
            "always include 'deterministic': the catalog layer costs microseconds and its "
            "checksum spans are what keep a model guess from displacing an identifier."
        ),
    )

    @model_validator(mode="after")
    def _deterministic_is_mandatory(self) -> DetectRequest:
        if self.layers is None:
            return self
        if not self.layers:
            raise ValueError("layers must not be empty; omit the field to run every layer")
        if "deterministic" not in self.layers:
            raise ValueError(
                "layers must include 'deterministic': the pipeline resolves NER spans "
                "against catalog spans, so it cannot report an NER-only run truthfully"
            )
        return self


class DetectResponse(BaseModel):
    spans: list[Span]
    layers_run: list[Layer] = Field(description="Layers that actually ran for this request")


class ErrorDetail(BaseModel):
    """Body of a fail-closed response: a reason, never the submitted text."""

    detail: str


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

    @app.exception_handler(RequestValidationError)
    async def validation_error(request: Request, error: RequestValidationError) -> JSONResponse:
        # FastAPI's default body reports the offending input, which on this
        # service is the text a client asked us to scan. Keep the location and
        # the message; drop everything that carries a value.
        redacted = [
            {key: value for key, value in item.items() if key not in {"input", "ctx"}}
            for item in error.errors()
        ]
        return JSONResponse(status_code=422, content=jsonable_encoder({"detail": redacted}))

    @app.post(
        "/detect",
        response_model=DetectResponse,
        responses={
            503: {
                "model": ErrorDetail,
                "description": "A requested layer cannot run; the reason is named.",
            }
        },
    )
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


__all__ = [
    "DetectRequest",
    "DetectResponse",
    "ErrorDetail",
    "HealthResponse",
    "create_app",
    "get_detector",
]
