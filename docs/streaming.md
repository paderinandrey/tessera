# The streamed path

A streamed response is restored into text that has already gone past, so a value has to be safe where it lands without the gateway knowing what will read it. This is what that costs and how a caller can narrow it.

## Telling the gateway how your application reads a streamed response

**Optional, and worth setting if the thing in front of this gateway parses the
assistant's content as JSON.**

```toml
response_format = "json"        # or json5, or jsonc
```

On the streamed path a value has to be substituted into text that has already
gone past — so when a placeholder sits at a *bare* position, outside a string
and inside a structure, the gateway has to decide what a value may safely
contain there without knowing what will read the result. Undeclared, it assumes
the worst reader it can imagine and admits word characters, a space, a hyphen
and a full stop. **An e-mail address does not pass that**, nor does a German tax
number like `419/130/29933`, nor `Beckmann AG & Co. KG` — and a value that
cannot be restored means the response is refused rather than served corrupted.

Declaring one of them says a JSON-family parser reads the content, where `@`,
`&` and `/` cannot act, and those values are restored. It changes nothing in a
markdown code fence or a comment, whose language nobody has spoken for.

**Inside a string it changes one thing, and only for `json5`.** A string ends
at a raw C0 control for every reader here — JSON forbids U+0000–U+001F
unescaped and nothing else — and for a JSON5 reader it also ends at U+2028 or
U+2029, which JSON and JSONC treat as ordinary characters. So `json` and
`jsonc` keep a valid document with a separator in it working and `json5` does
not, which is why the three names are three settings and not one.

JSONC is grouped with JSON here because it adds comments and leaves JSON's
string production alone.

**It tightens as well as widens, and the tightening is the part to read
twice.** Declaring `json` says the content *is* a document — so text outside
any string is that document's top level rather than prose, and it takes the
same rule. Undeclared, such text is a chat reply with nothing to break and
nothing is refused there; declared, a repairing reader that supplies a brace
the model omitted would be inside an object, so `name: [PERSON_1]` is judged as
a bare position.

The price falls on a caller whose content is not in fact a document. A model
that writes a sentence before its JSON puts names in that sentence, and
`O'Brien` does not pass the bare rule — declared, that sentence is refused where
undeclared it streams. **If your responses mix prose and JSON, do not declare
the format.**

**It is configuration and not a request header, on purpose.** A header would be
sent by whoever calls the gateway — and behind an application proxy that
forwards end-user headers, that is the end user, while the application is the
party whose parser is at risk. The operator running this process is the one who
knows what parses these responses, and the only one who cannot be a stranger.

An unrecognised value is the strict rule rather than an error, so a typo costs
restorations rather than a guarantee.

## How a stream is restored

`stream: true` is served for both providers. A placeholder does not respect event
boundaries — `[PERSON_1]` arrives as `[PER` in one event and `SON_1]` in the next, over HTTP
chunks that break anywhere, including the middle of a UTF-8 character — so the gateway holds
back the text from the last unclosed `[` and emits it once the token is whole or has grown
too long to be one. Everything before that point flows on immediately. Matching is exact:
tolerating altered spacing, casing or markdown around a token would also be a way to put a
real name where the model wrote something else.

Restored text is not the length of the masked text, so a delta is rewritten whole, and
text-bearing events are emitted one behind — the event that carries no text ends the run and
releases what is held into the event waiting behind it. The terminal events, the quota
headers and fields like `id:` reach the client as the provider sent them.

If a token turns out to have no mapping, bytes have already gone out and the request cannot
be refused. The stream ends instead, with an `error` event naming the failure — the client
gets a truncated answer, never a placeholder in place of a name. **Streamed tool calls are served on Anthropic and refused on OpenAI, and the difference is
the protocol rather than the risk.** A document arriving a delta at a time is not well
formed until its block closes, and a placeholder can be split across two deltas *and* land
inside a half-written JSON value at the same time — so there is nothing to parse at the
moment of substitution, and nothing of it is safe to serve. The answer is to stop
substituting: the fragments are accumulated, and when the block closes they are a document,
restored through the same door the buffered path uses. A value carrying a `"` then lands in
a leaf and is escaped on the way out instead of closing the string it was written into.

Anthropic says where a block ends — `content_block_start` with `type: "tool_use"`, then
`input_json_delta`, then `content_block_stop` at that index — so the accumulator has a
boundary to key on. OpenAI's `tool_calls` deltas have none: the end arrives as
`finish_reason` in a later chunk, several calls interleave in one chunk under their own
indices, and `id` and `name` come in the first fragment while `arguments` dribble after. So
a request carrying tool traffic together with `stream: true` is still refused there, before
the upstream call, where it costs no tokens.

**A document is served when its block closes, and on no other signal.** A run still held
when `message_stop` arrives — or `[DONE]`, or an `error` the upstream sent mid-generation —
never saw its own `content_block_stop`, and the stream ends rather than serving it. Truncation
usually leaves JSON that will not parse, so refusing on the parse would catch it nearly
always; *nearly* is the objection. The one truncation that happens to parse would go out as a
finished tool call, and a tool call is an action the client's agent takes rather than text it
displays.

**What the trade costs.** A refusal spent nothing; an accumulator spends the caller's tokens
and can still end the stream mid-flight, on a document past
`MAX_TOOL_DOCUMENT_BYTES`, one that does not parse, or one whose block never closed. The client also
sees nothing for the duration of a tool call and then the whole document at once, because
half a document is not a document and has no safe prefix to release.
Extended thinking is refused before
the upstream call rather than at its first streamed block, so the refusal costs no tokens.

**A streamed response the caller declares will be a JSON document is refused the same way.**
`stream: true` beside any `response_format` other than `{"type": "text"}` is a 400 before
the upstream call — an unfamiliar type is refused rather than admitted, because a
`response_format` the gateway cannot read is more likely to declare a document than prose,
and an omission in this predicate must cost a refusal rather than an injection. The reason is the restoration path: a buffered `content` that parses as a
document is restored *structurally*, so a value carrying a `"` — or an apostrophe, which
closes a string for a permissive reader just as well — lands inside a leaf and is escaped on
the way out. A delta is a fragment of a document and there is nothing to parse, so the
streamed path substitutes as text and such a value would land in the client's document
unescaped, mid-flight, with the bytes gone before anything could reconsider.

The guard reads the caller's **declaration** rather than the values the detector found.
Refusing whenever a masked value could close a string sounds tighter and is worse: a name
like `O'Brien` qualifies, and a streamed prose reply mentioning one is neither a document
nor a hazard.

**A stream that was never declared a document decides for itself, mid-flight.** It cannot
parse — a delta is a fragment of a document — but it can carry one fact about the text
already gone past: whether a `{` or a `[` was seen before the token being substituted. Where
one was, and the value could close a string, the stream ends with an `error` event rather
than writing it. That is the same question the buffered path asks of a whole string; the two
answer alike except in one case, where the buffered path can parse the document, put the
value in a leaf and escape it on the way out. A stream has no document to put it in, so it
refuses where the buffered path would have succeeded, and a truncated answer the client can
see is the better half of that trade against a silently altered one their agent may act on.

What counts as "inside" is answered by a **lexer** — not a parser: three string delimiters,
escapes carried across fragments, block and line comments, and no grammar or nesting. It
answers one question, *what kind of place is the next character in*, and the hazard test
differs by place because the ways out do:

- **inside a string** — the delimiter that opened it, the escape, and characters the format
  forbids raw. Only that delimiter: an apostrophe is a literal inside `"…"`, so `O'Brien` in
  a JSON object streams normally;
- **inside a backtick region** — the bare rule, because a backtick cannot be classified.
  Read as a string delimiter, an unclosed markdown fence — what every streamed fenced block
  looks like until it closes — hid the JSON object after it. Read as ordinary text, a `"`
  inside `` `…` `` opened a string that is not one and a value carrying backticks injected
  members a backtick-aware repairing parser reads. Both were measured; whichever reading is
  right, a value that can act structurally can act, so the region takes word characters only
  and closes on the next backtick;
- **inside a comment** — `*` or `/`, since a value ending `*` before a carrier `/` is the
  same escape as `*/` in the value itself;
- **in a bare position** — inside a container but outside any string, only alphanumerics and
  a few word marks pass. Nothing weaker works: `{safe:false,value:[PERSON_1]}` is valid JSON5
  and `null,admin:true` adds a member out of characters that must stay inert, because an
  e-mail address needs `@` and a date needs `:`. **Containers are counted, not flagged** — a
  reply is full of JSON snippets, and a flag that never came down refused a name after every
  one of them;
- **in prose** — nothing. No structure has been seen, so there is nothing to close, and prose
  is most of what streams.

Values feed the lexer as well as the model's own text: a value restoring to `{` opens a
structure exactly as a brace would.

Three rounds of review took this from a single boolean to the above, and each round found a
document shape the previous one could not see — a top-level string, a quote inside a
comment, a single-quoted string, an escape split across two fragments, and finally a bare
member position, which needs no hazardous character at all. That path's
allowlist is conservative because being wrong there costs it a parse it was happy to make;
on a stream being wrong costs the answer, so the test is the closed set of ways out of a
string — a delimiter, the escape, a character the format forbids raw. Measured on the public
corpus, the buffered allowlist would have ended a stream for 3.1% of detected values, all of
them `/` in a German tax number or `&` in a company name, neither of which can close a
string anywhere. The narrower test rejects none of them.

The cost that remains is a stream that ends when a bracket and a genuine delimiter meet in
one reply — a markdown list beside a name like `O'Brien` — which is behaviour the buffered
path already had for the same text.

Whatever was already restored is served before the error event, whether the stream ends
because the connection broke or because a token could not be restored. It was safe to send a
moment earlier, and the failure does not change that; what stays behind is the hold-back
buffer, which may hold the token that failed.

On a cache miss the gateway asks the detector for every layer it has, so that request
costs what [latency](latency.md) reports; a hit costs a lookup instead.
`detector_timeout_secs` defaults to 30 seconds because a tight timeout would turn
protection into a denial of service. Configuration is
TOML and rejects unknown keys — a typo in a security control should fail loudly rather
than leave a default in place.
