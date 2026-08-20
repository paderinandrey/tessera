# Tool traffic, so a coding agent can use this gateway at all

## The problem

Tessera refuses any request carrying tool definitions or tool traffic. Every one
of those refusals is correct today: tool arguments are not masked, and
forwarding them would send arbitrary strings past the masker. But coding agents
are built on tool use — Claude Code declares Read, Edit and Bash in its first
message, and Codex CLI, Cursor, Aider, Continue and Zed all do the equivalent.
They send a request without `tools` almost never.

All of them point at a custom base URL with one environment variable, which is
exactly this gateway's shape. So the integration is one variable away and then
fails on the first request. An entire class of client cannot use the product.

The case for serving them is the product's own case. A developer pasting a
customer record into a chat window is the scenario this already serves; a
developer running an agent across a repository that holds customer records is
the same exposure with more volume and less human review.

## What this slice is for

Masking tool traffic on the non-streaming path: tool definitions, tool call
arguments and tool results, both providers, in both directions.

Streaming is the second slice. The cut follows the code — `proxy.rs` against
`stream.rs` — and nothing here has to be rebuilt for it. It is also the harder
half: `input_json_delta` can split a placeholder across deltas *and* land it
inside a partially written JSON value, so the hold-back buffer cannot know the
document is well formed until the block closes.

Being honest about what this opens: an agent whose results are small works end
to end after this slice, and one that reads a large file does not (see
**Latency**). The category opens properly when this and the latency work have
both landed.

## The decisions

1. **Values are masked, keys never are.** A tool name, a schema property name
   and a `tool_call_id` are the client's dispatch. Rewriting one breaks the
   call, and the client would have no way to tell why.

2. **Structure is parsed, not pattern-matched.** Arguments are walked as JSON
   and only string leaves are masked, so the masker never sees a brace or a
   quote and cannot produce a document the client fails to parse.

3. **A value that cannot be masked is refused, never forwarded.** This is the
   existing posture and it extends unchanged.

4. **The existing seams carry it.** Providers describe locations; masking and
   restoration are written once against them. That sentence is already in
   `provider.rs`, and this slice is the case it was written for.

### Why a slot gains a kind rather than the walk gaining a caller

`request_pointers` returns locations where a string lives. Tool arguments are
not a string: on Anthropic `input` is an object, and on OpenAI `arguments` is a
string holding a JSON document.

Both are the same instruction — *mask the values in this structure* — so both
get one representation. A slot becomes `Text(pointer)` or `Json { pointer,
embedded }`, where `embedded` distinguishes a document from a string holding
one. One flag, not two code paths.

The alternative was to let providers enumerate a pointer per leaf, which
`serde_json`'s pointers can address at any depth and which would need no new
masking code for Anthropic. It was rejected because OpenAI cannot be expressed
that way at all, so the two providers would get two different mechanisms for
one idea. Two places describing the same thing is how they drift.

### Why masking needs a new function and restoration does not

`Mapping::restore_value` already walks a JSON document and restores every string
leaf, leaving keys and structure alone. It was written for upstream envelopes.
This slice adds its mirror, `mask_value`, and nothing else on that side.

Restoration needs no new code even for OpenAI: a placeholder inside a string
holding a document is ordinary text, and `restore` finds it there without
parsing anything.

### Why the walk needs a depth bound now, when it never did before

`restore_value` recurses without a limit. Today it only ever walks a provider's
own response envelope, so its depth is the provider's business. `mask_value`
makes the same recursion reachable from a *client's* JSON, which is untrusted
input: an argument nested ten thousand deep would end the process by exhausting
the stack.

So the walk is bounded on depth and on node count, and refuses past either.
Refusing is the only option that keeps the promise — a document too deep to walk
is a document whose values were not examined.

### Why a number that looks like a phone number is refused rather than masked

A JSON leaf can be a number, and a phone number, an insurance number or an IBAN
without separators is a plausible number. The leaf walk masks strings, so those
would pass untouched.

Replacing one with `[PHONE_1]` changes the leaf's type, and a tool schema that
declared a number may reject a string — or the model, seeing a string where the
schema promised a number, answers with one, and restoration then hands the
client the wrong type.

Numeric leaves are therefore rendered as text, detected, and the request is
refused when a span is found. Types stay as the client wrote them, nothing
unmasked leaves, and the cost is stated plainly: an agent sending personal data
as a JSON number gets a refusal with no way around it.

### Why tool definitions are masked like anything else

A tool description is prose the model reads, written by the client's software —
and an agent that generates its tool definitions from a customer schema writes
personal data into it. The product's promise makes no exception for text that
happens to be configuration.

The cost is real and worth naming: a placeholder inside an instruction changes
how the model chooses a tool. It is accepted because the alternative is a hole
the client controls, in the one guarantee this gateway sells. Tool names and
schema property names remain untouched — they are dispatch, not prose.

Repeated scanning costs nothing after the first request: definitions are
byte-identical every turn, which is exactly what the detection cache serves.

### Why an image inside a tool result follows the rule images already follow

`UNSCANNED_PART_TYPES` forwards image and audio parts unscanned in ordinary
chat content. A screenshot in a tool result is the same exposure through a
different field, not a new one.

Extending the existing rule keeps one policy for images rather than two. It
inherits that rule's hole knowingly, and narrowing it belongs to a decision
about images in general rather than to this slice.

## Shape

`provider.rs`:

```rust
pub enum Slot {
    Text(String),
    Json { pointer: String, embedded: bool },
}

fn request_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError>;
```

`mapping.rs` gains one function, the mirror of `restore_value`:

```rust
pub fn mask_value(&mut self, value: &Value, spans: &SpansByLeaf) -> Result<Value, MappingError>
```

Detection is asynchronous and `Mapping` is not, so the leaves are collected and
detected first and the walk is handed their spans — the same split `mask_all`
already makes between `detect` and `Mapping::mask`.

`proxy.rs`'s `mask_all` learns the two slot kinds and nothing else. `TOOL_FIELDS`
loses the fields this slice masks; what remains refused stays refused.

### What is masked, by provider

**Anthropic.** `tools[].description` and every `description` inside
`input_schema`; `tool_use.input` as `Json`; `tool_result.content` as text when it
is a string and as content blocks when it is a list.

**OpenAI.** `tools[].function.description` and every `description` inside
`parameters`; `tool_calls[].function.arguments` as `Json { embedded: true }`;
messages with role `tool` as ordinary text.

Untouched on both: tool names, schema property names, `tool_call_id`,
`tool_choice`'s selector.

### Restoration

`tool_use.input` and `tool_calls[].function.arguments` are restored before the
response reaches the client, because the client *executes* them. This is the
one place in the gateway where a restoration failure is not a display problem.

A placeholder the model invented, which the mapping never issued, is left as
written — `restore`'s existing behaviour. The client receives a literal
`[PERSON_9]` and its tool most likely errors. That is a wrong action rather than
a leak, and it is recorded under **Known limits** rather than defended against.

## Latency

**The detection cache does not help the first time a text is seen**, and a tool
result is usually seen once.

Measured: 59 characters per CPU-second, and the detector already parallelises
across cores — 50 KB costs roughly 850 CPU-seconds, about 92 seconds of
wall-clock on nine cores. `detector_timeout_secs` defaults to 30. So a `Read` of
a 1500-line file is a 503, and no amount of tool-field support changes that.

This slice answers it by bounding rather than by accelerating: a tool result
above a configured size is refused. At the default timeout the ceiling lands
near 10 KB, which passes ordinary agent traffic — a 200-line file read, `grep`
output, an edit, short `bash` output — and refuses a large file read honestly.

Making large results fast is its own work, with its own measurements: chunking
across replicas with overlap and offset arithmetic, where a span missed at a
seam is raw egress; or the int8 and fp16 graphs that already sit beside the fp32
one in the model directory, whose accuracy cost the evaluation gates can
actually measure. Neither belongs in a slice about masking structure. The
detection cache slice was good because it did not also try to support tools;
this one should not try to also make the detector three times faster.

The ceiling rises when that work lands, and the configuration key it moves is
already in place.

## Errors

Every failure is a refusal, and the vocabulary is the existing one. A document
past the depth or node bound, a numeric leaf carrying a detected span, a content
block whose shape this gateway does not understand, and a tool result over the
size bound are all refused before the upstream call.

Nothing here degrades. A partially masked argument is worse than no answer,
because the client would execute it.

## Configuration

One key, `max_tool_result_bytes`, defaulting to **10 000**. The arithmetic is
the point rather than the number: at 59 characters per CPU-second across nine
cores, roughly 530 characters clear per wall-clock second, so 10 000 characters
is about 19 seconds against a 30-second `detector_timeout_secs` — inside it with
room for a slower machine.

The two keys are one constraint written twice, and the config comment says so:
an operator who raises the timeout to serve larger results has to raise this as
well, and one who lowers the timeout has to lower this. Deriving it
automatically was rejected because a derived default would silently change
behaviour when an unrelated key moved.

Added to `gateway/tessera.example.toml`, `deploy/tessera.container.toml` and
`deploy/tessera.demo.toml`.

## Testing

Invariants the tests must pin, each proved by breaking it and watching the test
fail:

1. A tool name, a schema property name and a `tool_call_id` are byte-identical
   after masking.
2. A string leaf inside `input` is masked; the surrounding structure is
   unchanged.
3. OpenAI's `arguments` survives a round trip as valid JSON the client can
   parse.
4. A numeric leaf carrying a detected span refuses the request rather than
   forwarding it.
5. A document past the depth bound refuses rather than recursing.
6. A tool call restored for the client carries the original values, not
   placeholders.
7. A tool result over the size bound refuses before the upstream call.
8. The journal's counts include spans found in tool traffic, and record no key
   name and no argument value.

Store mechanics are unit tests in `mapping.rs`; provider shapes are unit tests
in `provider.rs`; the round trip goes through `wiremock` in `proxy.rs` against a
request shaped like the one Claude Code actually sends.

## Out of scope

Streaming (the second slice), extended thinking (below), making large results
fast (above), and images, which keep the policy they have.

## Known limits

**Extended thinking is still refused.** `provider.rs` rejects a request carrying
`thinking` because a thinking block's signature is computed over the text the
provider saw, so restoring it breaks verification on the caller's next turn.
Claude Code uses extended thinking routinely, so an agent with it enabled is
refused after this slice as before it. That is a separate problem with its own
hard trade-off, and folding it in here would hide it.

**A hallucinated placeholder reaches the client's tool.** See **Restoration**.

**A large tool result is refused, not served slowly.** See **Latency**.
