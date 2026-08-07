# SSE streaming restoration — design

**Slice C of the gateway.** The proxy currently refuses `stream: true` rather
than forward a response it cannot restore. This slice restores placeholders in
a Server-Sent Events stream and lifts the refusal.

## Why it is not the non-streaming path again

Restoration on the buffered path reads a whole string and hands it to
`Mapping::restore`. A stream has no whole string. `[PERSON_1]` arrives as
`[PER` in one event and `SON_1]` in the next, and the HTTP chunks under those
events break at arbitrary byte offsets — including the middle of a UTF-8
character. Applying restoration per chunk would emit `[PER` to the client and
never recognize the token.

Two decisions were taken before design:

- **Matching is exact, joined across boundaries.** A token is restored only if
  it matches the `[TYPE_N]` grammar byte for byte. Tolerating altered spacing,
  casing or markdown wrappers would widen the grammar, and every widening is
  also a way to restore something that was never our placeholder — putting a
  real name where the model wrote something else. The real problem in a stream
  is the split, not the model rewriting the token.
- **An unrestorable token terminates the stream.** By then the client already
  holds earlier text, but it never holds a placeholder in place of a name.
  This is the buffered path's rule, which refuses the request outright.

## Components

New module `gateway/src/stream.rs`. `proxy.rs` is 784 lines; the streaming path
does not belong in it.

### `RestoreBuffer`

The core, and pure: no I/O, no async, no provider knowledge.

```rust
pub struct RestoreBuffer<'a> {
    mapping: &'a Mapping,
    held: String,
}

impl<'a> RestoreBuffer<'a> {
    pub fn new(mapping: &'a Mapping) -> Self;
    /// Append text and return the prefix that is safe to emit, restored.
    pub fn push(&mut self, text: &str) -> Result<String, MappingError>;
    /// Emit whatever is still held. The text run has ended.
    pub fn finish(&mut self) -> Result<String, MappingError>;
}
```

**Hold-back rule.** A placeholder matching `[TYPE_N]` — upper-case type,
underscore, digits — contains no `[`. So the only text that can be the start of
a placeholder is the text from the **last** `[` that has no `]` after it.
Everything before that point is complete: it is passed to `Mapping::restore`
and returned. Everything from it is held for the next push.

**Hold cap.** `MAX_HELD = 64` characters. A `[` that never closes would
otherwise suspend the stream forever — a model writing `[note` followed by
three paragraphs. Past the cap the leading `[` cannot begin a placeholder: it
is emitted as ordinary text and the scan resumes after it.

**Failure.** `push` propagates `MappingError::Unknown` when a complete
placeholder has no mapping. The token is fully assembled in the buffer before
this is decided, so nothing unrestorable has been emitted.

### `SseFramer`

Byte level. HTTP body chunks split anywhere — mid-line, mid-UTF-8-character.
The framer accumulates bytes and yields only complete events, delimited by a
blank line. Incomplete trailing bytes stay in the framer.

```rust
pub struct SseFramer { buffer: Vec<u8> }

impl SseFramer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent>;
    /// Bytes left over when the body ended, if any.
    pub fn finish(&mut self) -> Option<SseEvent>;
}

pub struct SseEvent {
    pub name: Option<String>,   // the `event:` line, if present
    pub data: String,           // the `data:` lines, joined
}
```

`data: [DONE]` is passed through as it arrives; it is not JSON.

### `Provider::stream_pointers`

A fourth method on the existing trait, in the shape of the other three:

```rust
fn stream_pointers(&self, event: &Value) -> Result<Vec<String>, ShapeError>;
```

- **OpenAI**: `/choices/{i}/delta/content` for every choice whose delta carries
  a string `content`. A chunk with no content — the one bearing `finish_reason`
  — yields none.
- **Anthropic**: `/delta/text` on `content_block_delta`, and
  `/content_block/text` on a `content_block_start` whose block is a text block.
  Other event types yield none.

An event whose shape is recognized but whose text field is not a string is
refused, matching how `request_pointers` treats an unscannable message. An
event type we do not know yields no pointers and is forwarded unchanged: the
protocols add event types over time, and `ping` must not break a stream.

## Data flow

Restored text does not have the length of the masked text, so a delta is
rewritten whole rather than patched. That leaves one problem: text held back at
the last text-bearing event has nowhere to go, because that event would already
have been sent. Hence **a one-event delay**.

State: `pending: Option<SseEvent>` — the most recent text-bearing event, already
rewritten — and a `RestoreBuffer` per JSON pointer, so `n > 1` on OpenAI and
several content blocks on Anthropic do not share a buffer.

- **Text-bearing event.** For each pointer, `push` its text and write the safe
  prefix back into the event. The prefix may be empty; an empty delta is legal
  in both protocols and preserves event count and indices. Emit `pending` if
  there is one, then make this event `pending`.
- **Event with no text** — `finish_reason`, `content_block_stop`,
  `message_stop`, `[DONE]`. The text run has ended: `finish()` every buffer,
  append each remainder to its pointer inside `pending`, emit `pending`, then
  emit this event.
- **End of body.** The same flush.

If a remainder exists but `pending` does not carry that pointer — possible only
when choices interleave — the stream terminates with the error event below.
Dropping the text silently is the failure this project exists to prevent; it is
a documented limitation that refuses rather than loses.

## Errors

`MappingError` from a buffer, or unplaceable remainder, arrives after bytes have
already gone out. The stream emits

```
event: error
data: {"error":{"type":"tessera_restoration_failed","message":"..."}}

```

and closes. The message names the placeholder, never a value:
`MappingError::Unknown` carries the token. Both providers' clients surface an
`error` event; neither will mistake a truncated stream for a complete answer.

Failures that happen before the first byte keep their current behaviour:
detector errors, masking errors and unreadable request shapes return the
existing JSON error response, because nothing has been sent yet. A non-success
upstream status is not a stream and goes down the existing buffered branch —
status, quota headers and restored envelope, unchanged.

## Request side

`handle` no longer refuses on `stream: true`. Masking is untouched: the request
is JSON regardless. `ProxyError::Streaming` is replaced by
`ProxyError::StreamRestore`, raised only from the streaming response path.

## Testing

**Mandatory, per the roadmap:** one recorded SSE response is fed through framer
and buffer at every slicing granularity from one byte to the whole body, and the
concatenated output must equal the fully restored text every time. A restoration
that works on natural chunk boundaries and fails on a one-byte split is the
failure mode this slice exists to remove.

Also:

- a placeholder split across two events is restored once, whole;
- a chunk boundary inside a multi-byte character does not corrupt it;
- `[see [PERSON_1]]` arriving across a boundary restores the inner token;
- an unclosed `[` longer than the cap is emitted rather than suspending output;
- an unknown placeholder produces the error event, and the token never appears
  in the bytes the client received;
- a `ping` event and an unknown event type pass through unchanged;
- an end-to-end wiremock test streams an OpenAI and an Anthropic response
  through the router and asserts the client sees restored values;
- the pre-flight refusal is gone: `stream: true` reaches the upstream.

## Revisions after review

Review found four holes in the design as written; the code carries the fixes:

- **Buffers keyed by JSON pointer were wrong.** OpenAI streams one choice per
  chunk at array position 0 whatever its logical `index`, so `n > 1` gave two
  completions the same buffer and spliced their fragments. `stream_pointers`
  became `stream_slots`, returning a pointer *and* a key: the pointer addresses
  this event, the key identifies the run. A remainder is placed only into an
  event carrying the same key, so a shared pointer can no longer misdirect it.
- **A malformed `data:` payload was forwarded unchanged.** A truncated event
  still holding `[PERSON_1]` would have reached the client. Only `[DONE]` and an
  empty payload pass without parsing; anything else must be JSON or the stream
  ends.
- **Provider error events were forwarded verbatim.** An upstream error quotes
  what we sent it. Events with no text of their own now go through
  `Mapping::restore_value`, as the buffered path's error body already did.
- **Any event without text drained every buffer.** Anthropic sends `ping`
  mid-generation, and treating a keepalive as the end of the run released `[PER`
  as ordinary text with `SON_1]` behind it — the client reassembles exactly the
  token this slice hides. `Provider::stream_terminates` now says what an event
  ends: nothing, named runs, or all of them. `content_block_stop` ends its own
  block; a `finish_reason` ends that choice; `ping` and event types these
  protocols grow later end nothing and are forwarded, held behind the waiting
  event so the provider's order survives.
- **The size cap was checked after the event was assembled.** An oversized event
  arriving complete in one chunk slipped past it. The cap is now enforced while
  framing, before the block is copied.

- **Extended thinking was refused too late.** Anthropic opens a thinking stream
  with a `content_block_start` whose block is `thinking`, which the streaming
  fallback rejected — after the upstream call had already cost the caller its
  tokens. A request carrying `thinking` is now refused before the call, beside
  the tool fields. Restoring the text instead is not a fix: the block's
  signature is computed over the text the provider saw, so restoring it would
  fail verification on the caller's next turn. Masking it is its own slice.
- **A severed connection discarded restored output.** The one-event delay leaves
  the newest rewritten event waiting; the transport-error path emitted only the
  error and returned, losing text that was already safe to serve. It flushes
  first now. Tested against a raw socket that promises more body than it sends
  and then drops — wiremock cannot sever a stream mid-body, and that is the
  whole claim.

- **A failure discarded output that was already correct.** Events rendered
  earlier in the same chunk, and the event waiting behind the one-event delay,
  were dropped along with the failing one. `StreamRestorer::salvage` returns
  them; only the hold-back buffers are dropped untouched, because they may hold
  the very token that could not be restored. Ending the stream is a decision
  about what comes next, not a verdict on what already went out.
- **The queue behind a held event was unbounded.** A stream that stalls after
  one delta and then sends keepalives forever grew memory without limit; the
  per-event cap does not cover it. `MAX_QUEUED_BYTES = 64 KiB`, and the budget
  resets each time the queue is released, so a long healthy stream never trips
  it.

- **`logprobs` put the masked output past restoration.** With logprobs on,
  every choice carries the model's output again as token strings. Joined back
  together they spell the placeholder, and their probabilities describe text the
  client will never see. Restoring them is not meaningful — token boundaries do
  not follow placeholder boundaries — so the request is refused before the call,
  and a provider that sends them unasked ends the stream.
- **Three more paths dropped correct output.** The framer discarded events it
  had already framed when a later event in the same chunk was oversized; the
  final flush discarded what it had released before failing; and the flush lost
  the event it was holding when a remainder could not be placed. All three now
  route through `salvage`, which keeps them in the order they were produced.
- **The map of active runs was unbounded.** An upstream opening new choice or
  block indices forever added a buffer per event for the life of the response,
  which neither of the other caps covers. `MAX_ACTIVE_RUNS = 64`, and a run that
  ends frees its place.

- **`logprobs: false` was refused.** An SDK serializing its default asks for
  nothing; only an enabled value is turned away now, and a streamed `logprobs`
  field ends the stream only when it actually carries token strings — providers
  send `logprobs: null` on every chunk.
- **A byte order mark hid the first data line.** A stream may open with one, and
  an SSE client ignores it. Kept as an unknown field the line was rendered back
  verbatim, so a first delta containing `[PERSON_1]` reached the client
  unrestored. The framer strips it.
- **The media type was matched case-sensitively.** `Text/Event-Stream` is the
  same media type; missing it buffered a live response and failed to parse the
  transcript as JSON. Compared without case, and without any parameters.

- **`thinking: {"type":"disabled"}` was refused.** The same compatibility case
  as `logprobs: false`: explicitly off asks for nothing. Only a mode that
  actually enables thinking is turned away.
- **The framer knew two line-ending conventions, not three.** SSE allows CR, LF
  and CRLF, and a blank line is any two terminators in a row. A CR-only upstream
  produced no events at all while the connection was live. Framing and field
  splitting now use the same rule. Where a trailing `\r` could still become
  `\r\n`, the framer takes the shorter reading rather than waiting: the block
  ends at the same offset either way, and the leftover byte reads as an empty
  line, which is skipped. Waiting would delay every event on a CR-only stream.

- **A malformed `logprobs` field slipped past the check.** The test asked
  whether the field carried tokens, so `"logprobs": "[PERSON_1]"` — not a list,
  so not "carrying" — passed, and the slot path rewrites `delta` rather than
  this field, forwarding it verbatim. Inverted: only a field demonstrably empty
  (null, `{}`, or empty token lists) is forwarded; every other shape, including
  keys we have never seen, ends the stream.

- **Everything in a text event that was not the text went out verbatim.** The
  slot path rewrote the deltas and forwarded the rest of the event untouched, so
  any other string — `logprobs` on a choice with no delta, a field added to the
  protocol next year — carried its placeholder to the client. Review found this
  twice as separate instances; the class is closed instead. A text event now has
  every string outside its slots restored whole, exactly as a pointer-less event
  does. The slots are blanked before that pass so the held-back text stays the
  buffer's to restore. The `logprobs` refusal stays regardless: those are
  token-level fragments, and restoring them would leave the probabilities
  describing text that is no longer there.

- **Whole-envelope restoration is event-local, and a field that streams in
  fragments needs a run.** OpenAI's audio output puts a transcript under
  `delta.audio.transcript`, arriving in pieces beside audio bytes nothing can
  mask. The envelope pass would meet `[PER` and `SON_1]` in separate events and
  pass each untouched. Restoring the transcript would leave it saying a name the
  recording does not say; leaving it alone hands over the placeholder a fragment
  at a time. Audio output is refused before the call. The rule this leaves
  behind: any field whose text arrives incrementally must be declared as a slot,
  because the envelope pass cannot join across events by construction.

## Out of scope

Tool-call arguments in a stream stay refused, as they are on the buffered path.
Session-scoped mappings are slice D.
