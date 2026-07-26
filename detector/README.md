# tessera-detector

PII detection service for [Tessera](../README.md). Stateless, stable contract.

Layers: deterministic recognizers with checksum validation (Tier 1, catalog-driven),
NER (GLiNER/ONNX), context boosting. Spans always point into the original text.

```
uv sync
uv run pytest
```
