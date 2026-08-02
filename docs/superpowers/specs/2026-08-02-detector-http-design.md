# Detector HTTP Contract — Design

**Goal:** the detection service the Rust gateway will call — two endpoints, and an OpenAPI
document committed as the schema both implementations share.

**Traceability:** MVP roadmap Release 2 ("Rust gateway, on top of the detector's stable
contract"), REQ-44 (one span-schema file as the source of truth for both implementations),
REQ-40 (a per-request header may only narrow), REQ-1 (multi-layer detection).

## Decisions made during brainstorming

- **Two endpoints only.** `POST /detect` and `GET /health`. The gateway does not exist
  yet, so every additional surface would be guessed rather than required.
- **Layers are chosen per request.** The deterministic layer costs milliseconds and the
  full pipeline costs seconds per document, so the caller states which layers to run and
  the response states which actually ran.

## Runtime choice

FastAPI on uvicorn. Pydantic is already a dependency, so the request and response models
come free, and the generated OpenAPI document is precisely the artefact REQ-44 asks for:
one file in the repository that both implementations read. Starlette alone would mean
hand-rolling validation and the schema — and the schema is half the point of this work.
Flask or the standard library would give up both async and schema generation for a service
that holds a model in memory and serves concurrent requests.

The dependencies live in an optional `serve` group, as `ner` and `eval` already do, so the
base install stays light.

## API

`POST /detect`

```json
{"text": "Der Mandant Weber ist Mitglied der IG Metall.", "layers": ["deterministic", "ner"]}
```

`layers` is optional. The response reports what ran:

```json
{
  "spans": [{"entity_type": "PERSON", "start": 12, "end": 17, "confidence": 0.9,
             "recognizer": "ner:gliner", "tier": 2, "boosted": false}],
  "layers_run": ["deterministic", "ner"]
}
```

`GET /health` reports readiness and the state of the NER layer:

```json
{"status": "ok", "ner": false, "ner_off_reason": "no weights"}
```

### Why `layers_run` is part of the contract

A result produced by the deterministic layer alone must not be indistinguishable from a
full scan. The CLI already carries this in its report; the API carries it in every
response. A caller that asks explicitly for `ner` when no weights are loaded gets 503 with
the reason — never a quiet downgrade, because a quiet downgrade is how unredacted text
reaches a model provider while the caller believes otherwise.

### REQ-40 falls out of the shape

The per-request `layers` list can only narrow what the server already runs. Asking for a
layer the server does not have is an error rather than a way to obtain more, so widening
one's own privileges through the request is not expressible.

## Schema as an artefact

The OpenAPI document generated from the application is committed to the repository, and CI
regenerates and diffs it exactly as it already does for the seeded corpus. The schema
cannot drift from the code unnoticed, and the Rust side gets a source of truth rather than
a description of one.

## Lifecycle

The detector is built once at startup through FastAPI's lifespan, not per request: loading
the model takes seconds. The server starts as `tessera serve`, the second CLI subcommand —
the slot left open when the `scan` subcommand was designed.

## Errors

Empty or missing text is a 422 from pydantic. A requested layer that cannot run is a 503
naming the reason. Anything unexpected is a 500. In none of these does the response body or
any log line carry the submitted text: originals are forbidden in logs at every level, and
an error path is the easiest place to forget that.

## Testing

Most tests use FastAPI's TestClient with a fake detector injected, so they neither touch the
network nor load a model: layer selection, the 503 path, the shape of both responses, and
the absence of input text in error bodies. Tests carrying the `ner` marker cover the real
thing — that the service starts with weights present and that `/health` reports the layer
honestly.

## Out of scope

Authentication (REQ-39 concerns the management API), per-tenant thresholds (REQ-9), batch
requests, Prometheus metrics, and TLS. The gateway itself is the next slice.
