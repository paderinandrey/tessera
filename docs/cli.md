# CLI

Point the detector at a folder (or file) of texts to see what would be redacted:

```
uv run --project detector tessera scan path/to/texts            # human-readable, values masked
uv run --project detector tessera scan path/to/texts --json     # machine-readable
```

Found values are masked by default (`FR76…89`) so a saved report is not itself
a PII leak; `--show-values` prints them verbatim.

The NER layer (PERSON, LOCATION, ORG) runs automatically when its weights are present
(`make model`, 2 GB, cached under `~/.cache/tessera/models`).
Without them the scan runs the deterministic layer alone and says so; `--ner` makes their
absence an error, `--no-ner` skips the layer even when they are installed.

Article 9 special categories (health, biometrics, genetics, ethnic origin, political
opinion, religion, trade union membership, sexual orientation) are detected by the same
layer at a deliberately low threshold, so expect visible false positives —
over-redaction is the safe failure for this category.

The same binary serves the detection HTTP contract the gateway will call:

```
uv run --project detector --group serve tessera serve      # 127.0.0.1:8000
```

`POST /detect` takes `{"text": "...", "layers": ["deterministic", "ner"]}` — `layers` is
optional and may only narrow what the server runs — and every response reports
`layers_run`, so a deterministic-only result is never mistakable for a full scan. Asking
for a layer the server cannot run is a 503 naming the reason rather than a quiet
downgrade. `GET /health` reports whether the NER layer is loaded and why it is not. The
committed OpenAPI document at `docs/api/openapi.json` is the schema both implementations
share (REQ-44); `make openapi` regenerates it and CI fails if it drifts.
