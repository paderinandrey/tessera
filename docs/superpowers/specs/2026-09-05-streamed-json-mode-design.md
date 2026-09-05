# A streamed response the caller declares will be a document — design

Closes #36.

> Decisions were taken without the user in the room, on a standing instruction.
> #36 says explicitly that this is "a behaviour change a human should sign off
> on", so the section worth their attention is **what it costs clients**, and
> the one after it — the residual the guard deliberately leaves open.

## The defect

The buffered path restores a text slot with `restore_in_string_strictly`. When a
substituted value carries a character that could close a string, that function
stops substituting textually: it parses the string as a document, puts the value
in the leaf, and re-serializes so the value is escaped on the way out. If the
string will not parse but a container was opened before the token, it refuses
rather than emit.

The streamed path restores with `Mapping::restore`, the plain textual
substitution, and it has neither route available. A delta is a *fragment* of a
document, so there is nothing to parse; and `safe_prefix_len` holds text back
only far enough not to split a placeholder, so the boundary it releases on is a
`[` with no relation to where a document begins.

So on a streamed completion, a value like `x","admin":true,"pad":"` goes into
the client's document unescaped and mid-flight, and the bytes are gone before
anything can reconsider. The client's agent may then act on a field the upstream
never sent.

**Who supplies the payload matters.** Not the caller attacking themselves: the
realistic path is a third party's text — a support ticket, a forwarded email —
flowing through the caller's request, detected, masked, and restored into a
document the caller's own agent reads.

## What was already closed

`reject_streamed_tools` refuses `stream: true` on any request carrying tool
traffic, so a described `arguments` — the case the escaping work was written for
— never streams. This is the other half.

## The change

**Refuse `stream: true` beside a `response_format` of `json_object` or
`json_schema`**, in `request_pointers`, next to `reject_streamed_tools`. A 400
before the upstream call, so the caller pays nothing.

`json_schema` as well as `json_object`: Structured Outputs declares a document
exactly as JSON mode does, and #36 named only the older of the two.

Called for Anthropic as well, where it is inert today — the Messages API asks
for structured output through tools, which the other refusal already covers. It
is there so the day Anthropic grows the field is not the day the unwatched half
silently admits it.

## Why the guard reads a declaration and not a value **[the interesting part]**

The first design refused whenever the request's mapping held a value that fails
`json_string_inert`. That is the same predicate the buffered path uses, the
mapping is a per-request snapshot detached from the session before the upstream
call, so it is decidable and precise — and it is **wrong**.

`json_string_inert` is an allowlist: alphanumerics and a short list of
punctuation. It excludes the apostrophe. So `O'Brien` fails it, and a *streamed
prose reply* mentioning that name would have been refused. Prose is most of what
streams.

The buffered path can afford that predicate because it does something the
streamed path cannot: it **parses**. The parse is what separates its documents
from its prose, and `json_string_inert` only decides whether to attempt one.
Lifting the predicate without the parse lifts half a rule.

`response_format` is the one signal that separates them without a parser, and it
separates them because the **caller supplied it**. They are telling us the reply
is a document they will parse. That is a claim we can act on; the shape of the
bytes is not one we can read in fragments.

Pinned as `the_streamed_document_refusal_reads_a_declaration_and_not_a_value`,
because the tighter guard is exactly what the next reader will reach for.

## What it costs clients

**A client streaming structured output must stop streaming it.** That is a real
pattern and a real cost, and it is the reason #36 called this a decision rather
than a fix. The alternatives were measured against it:

- **buffer a JSON-mode stream whole and emit at the end** — correct, and it
  turns streaming into non-streaming for exactly the requests that asked for it,
  silently. It also holds a whole response in memory, which is a bound this
  gateway does not currently need;
- **track JSON structure across fragments** — a second parser of ours beside
  `serde_json`, on the one path where a mistake cannot be taken back.

Refusing costs the client an integration change they can see. The other two cost
them latency they cannot, or correctness we cannot guarantee. Buffered
JSON-mode is unaffected and restores correctly, so the fix for an affected
client is one field.

## The residual, stated because the guard is on a declaration

**A streamed reply whose content happens to be JSON while the request declared
no `response_format` is still restored as text.** The hazard is identical; the
declaration is missing.

That is deliberate and it is the price of not having a parser here. The caller
has not claimed a document contract, and the buffered path's protection for that
case comes from a parse this path cannot perform. Closing it means one of the
two alternatives above, for a case where the client did not say what they were
doing.

It is recorded here rather than left to be rediscovered, and it is in the
README's streaming section too — the place a client reads before deciding how to
integrate.

## Testing

- both declared types refused on a streamed request, and **neither half alone**:
  a declared document without streaming is the case the buffered path restores
  structurally, and streaming without a declaration is an ordinary completion.
  Refusing either would cost requests handled correctly today;
- `{"type": "text"}` — the default written out — admitted, since it declares the
  opposite of a document;
- a streamed prose request carrying a non-inert character admitted, which is the
  rejected design stated as a test;
- Anthropic asked the same question, though it has no field to answer with;
- end to end: the payload from the buffered injection test, streamed and
  declared, returns 400, the error names what it refused, and
  **`upstream.received_requests()` is empty** — the property that makes this
  free for the caller.

Mutations, restored by inverse substitution:

- **drop `json_schema` from the match** → the unit test fails, naming the type
  that was admitted;
- **drop the `streaming` half** → the unit test fails on the buffered request it
  would now refuse;
- **drop the Anthropic call** → the Anthropic test fails;
- **drop the OpenAI call** → both the unit test and the end-to-end test fail,
  and the end-to-end failure is the informative one: the request reaches the
  upstream and dies on the response shape, which is the caller's tokens spent to
  return an error.

A first attempt at these mutations edited `gateway/src/provider.rs` from inside
`gateway/`, so the path did not exist, every substitution silently did nothing,
and three "all tests pass" results were three runs of unmutated code. Recorded
because it is the second time in two days that a probe reported no bug without
having reproduced the condition.
