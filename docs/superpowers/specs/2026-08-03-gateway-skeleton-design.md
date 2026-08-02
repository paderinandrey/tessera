# Gateway Skeleton — Design

**Goal:** the Rust reverse proxy from Release 2, non-streaming: a drop-in base URL for
OpenAI- and Anthropic-shaped requests that masks personal data before it leaves, restores
it in the response, and refuses the request when it cannot.

**Traceability:** MVP roadmap Release 2 ("drop-in via base URL; OpenAI and Anthropic, JSON
and SSE" — SSE is the next slice), the fail-closed rule ("including an unparsed request
body and a lost mapping"), consistent placeholders, and the ban on originals in logs at any
level.

## Decisions made during brainstorming

- **Both providers in this slice.** The abstraction over request shapes is validated by two
  implementations rather than by one and a hope.
- **The full detection layer by default**, with a configurable timeout. Exceeding it refuses
  the request; it never forwards unmasked text. Measurement puts the full pipeline near a
  second per 1 200 characters, and a conversation history is longer than that, so a tight
  timeout would turn protection into a denial of service.

## Stack

axum on tokio, reqwest upstream, serde_json for body rewriting. Hyper alone would mean
more code for no gain on a JSON proxy; pingora is built for proxying at scale but makes
body rewriting awkward, and scale is not the current problem.

## Layout

```
gateway/
  Cargo.toml
  src/
    main.rs        binary: load config, bind, serve
    config.rs      TOML: bind address, detector URL, upstream per provider, timeout
    provider.rs    the abstraction: texts out of a body, masked texts back in
    openai.rs      chat/completions shape
    anthropic.rs   messages shape, with its separate system field
    detector.rs    client for POST /detect
    mapping.rs     placeholder assignment and restoration
    proxy.rs       the handler: extract, detect, mask, forward, restore
```

The provider abstraction is four operations: take the texts out of a request, put masked
texts back, take the texts out of a response, put restored texts back. Two implementations
keep the OpenAI shape from leaking into the interface.

## Placeholders

`[TYPE_N]` — `[PERSON_1]`, `[IBAN_2]`. Numbering is per request, and an identical value
always receives the same placeholder: without that the model sees two people where the text
had one. Coreference across requests belongs to the session slice.

Restoration scans the upstream response for placeholders and substitutes the originals.
Tolerant matching is what streaming needs; exact matching is enough here.

## Fail-closed

This is the central rule rather than error handling around the edges. The request is
refused, and never forwarded, when:

- the detector errors or exceeds the timeout,
- the body does not parse into the expected provider shape,
- a placeholder appears in the upstream response that is not in the mapping — a lost
  mapping breaks the request rather than handing `[PERSON_1]` to the client in place of a
  name.

No response body and no log line on any of these paths carries the original text.

## Configuration

TOML: bind address, detector URL, an upstream base URL per provider, and the detector
timeout, defaulting to 30 seconds.

## Testing

`wiremock` stands in for both the upstream and the detector, so the handler is exercised
over real HTTP: that masked text reached the upstream, that the restored response reached
the client, that a detector failure breaks the request, that a lost mapping breaks the
request, and that no error body contains the original. Unit tests cover placeholder
assignment, the repeated-value case, and both body shapes.

## CI

A new `gateway` job: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`. The
existing jobs are untouched.

## Out of scope

SSE streaming restoration, session mapping and coreference, audit, metrics, shadow mode,
masking of tool-call arguments and RAG context, and authentication. Each is its own slice.
