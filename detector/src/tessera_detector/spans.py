"""Span schema — the single source of truth for detection output (REQ-44).

Spans reference the original text by character offsets only and never carry the
matched value, so they are safe to log and audit.
"""

from typing import Self

from pydantic import BaseModel, ConfigDict, Field, model_validator


class Span(BaseModel):
    model_config = ConfigDict(frozen=True)

    entity_type: str
    start: int = Field(ge=0, description="Character offset in the original text, inclusive")
    end: int = Field(ge=1, description="Character offset in the original text, exclusive")
    confidence: float = Field(ge=0.0, le=1.0)
    recognizer: str
    tier: int = Field(ge=1, le=3)
    boosted: bool = False

    @model_validator(mode="after")
    def _end_after_start(self) -> Self:
        if self.end <= self.start:
            raise ValueError(f"span end ({self.end}) must be greater than start ({self.start})")
        return self
