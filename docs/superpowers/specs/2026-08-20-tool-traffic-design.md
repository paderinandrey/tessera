# Tool traffic, so a coding agent can use this gateway at all

## The problem

Before this slice, Tessera refused any request carrying tool definitions or
tool traffic. Every one of those refusals was correct then: tool arguments were
not masked, and forwarding them would send arbitrary strings past the masker.
But coding agents are built on tool use — Claude Code declares Read, Edit and
Bash in its first message, and Codex CLI, Cursor, Aider, Continue and Zed all
do the equivalent.
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

**The cut has to be enforced, not merely intended.** `request_pointers` runs
before `proxy.rs` looks at `stream`, so relaxing the tool-field refusal admits
streamed tool requests as readily as buffered ones — and `stream_slots`,
unchanged, would then reject the tool events *after* the upstream call, spending
the caller's tokens to return a broken stream. So `stream: true` together with
tool traffic is refused before the upstream call, explicitly, and that refusal is
deleted by the second slice rather than by accident.

`reject_streamed_tools` decides that from the **slots**, not from a field name.
A continuation carrying an earlier call and its result need not repeat `tools`,
and Anthropic's `mcp_servers` grants tools without a `tools` array at all, so
asking "does the body have a tool field" was the wrong question; asking "did any
location we describe turn out to be tool traffic" covers both, and covers a
location added later on the day it is described.

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

### Why restoration needs no new primitive, but does need the same seam

`Mapping::restore_value` already walks a JSON document and restores every string
leaf, leaving keys and structure alone. It was written for upstream envelopes,
so `Mapping` gains only `mask_value`, its mirror, and nothing else.

The interface around it is a different question, and an earlier draft of this
document got it wrong by checking only `Mapping`. The buffered response loop
iterates `response_pointers` and calls `read_pointer`, which accepts a JSON
string and nothing else — `tool_use.input` is an object and would fail there.
So `response_pointers` returns `Slot` exactly as `request_pointers` does, and
the loop dispatches on the kind: `Text` through `restore`, `Json` through
`restore_value`.

OpenAI needs no parsing on this side even so: a placeholder inside a string
holding a document is ordinary text, and `restore` finds it there.

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

A JSON leaf can be a number, and a credit card, a German tax ID or a French NIR
is digits alone. The leaf walk masks strings, so those would pass untouched.

Replacing one with `[CREDIT_CARD_1]` changes the leaf's type, and a tool schema
that declared a number may reject a string — or the model, seeing a string where
the schema promised a number, answers with one, and restoration then hands the
client the wrong type.

Numeric leaves are therefore rendered as text, detected, and the request is
refused for any span found in one — **except** a span carrying one of the
fourteen types the NER layer labels. That exemption is the whole of the
narrowing, and it is written as a hole in the refusal rather than as the refusal
itself, because the argument for it is an argument about what those fourteen
labels mean. `identifiers.yaml`'s eight refuse. A type in neither half — a
version-skewed, misconfigured or compromised detector reporting a name no
catalog declares — refuses too: *a label known to be ungrounded on digits is not
the same thing as a label nobody can weigh at all*, and on a string leaf such a
span has always masked, as `[REDACTED_1]`. Types stay as the client wrote them,
and the cost is stated plainly: an agent sending a card number as a JSON number
gets a refusal with no way around it.

**Why the fourteen are exempt, and not all twenty-two.** The first implementation
refused on any span at all, and the measurement that changed it was taken
against the running detector: `"9007199254740991\n\n9007199254740991"` comes
back labelled `PERSON` at 0.723. That number is `Number.MAX_SAFE_INTEGER`, and
it appears twice in this repo's own transcribed Claude Code tool payload, as the
`maximum` of `limit` and of `offset`. A catalog hit is grounded in the value —
`4111111111111111` is a card because it passes Luhn and `9007199254740991` is
not because it fails — while an NER label on a bare digit run is the model
reading a shape it has no context for. Refusing on a type that cannot be decided
from the digits themselves buys no detection and spends real requests:
`{"invoice_ids": [98765432109876, 98765432109877]}` is an ordinary tool call.

**The evidence against that decision, recorded rather than left out.** The
false-positive class is narrow. Paired unix timestamps, millisecond timestamps,
14-digit ids, other large powers of two, byte offsets and `0`/`2000` all come
back with nothing, and *three* repeated bounds come back with nothing where two
fire. The 14-digit ids are the sharpest, because they are the `invoice_ids` case
above and they are clean. Nor does surrounding prose reliably suppress the
label: the same two bounds behind "Maximum number of items" return nothing, but
behind "The maximum number of items to return." still return `PERSON`, at 0.784.
So the argument here is not frequency — it is that labelling
`Number.MAX_SAFE_INTEGER` a person is wrong however rarely it happens, and
unstable enough that "how rarely" is not a number anyone can hold the predicate
to. Someone who weighs an ungrounded refusal as cheaper than a missed one should
widen it back and record why here.

This is not the fail-open direction. A numeric leaf was forwarded verbatim
before this slice; the narrowed predicate is still strictly more than the gateway
had. And it says nothing about NER in general — on prose it is the larger half of
this gateway's coverage, and no text leaf is touched by any of this.

The partition is structure rather than a comment: `mapping::DETERMINISTIC_TYPES`
is held to `identifiers.yaml` exactly, and `ENTITY_TYPES` minus it to `ner.yaml`
exactly, by `scripts/check_entity_types.py`. A ninth identifier fails that check
instead of landing silently on the side of the predicate that does not refuse.
The predicate reads both constants and writes out neither list, so the exempt
fourteen are derived from the two things that check holds to the catalogs rather
than from a third array nothing holds to anything.

**What this does not cover, since an earlier draft of this document claimed
otherwise.** The refusal is only as wide as the vocabulary, and `ENTITY_TYPES`
has no telephone entity — neither catalog defines one. A phone number as a JSON
number is not detected and is forwarded, exactly as a phone number in ordinary
chat text is today. That is a gap in detection coverage rather than in this
slice, and it is not closed here.

Refusing every numeric leaf above some digit length was considered and rejected:
a Unix timestamp is ten digits and a byte offset can be any length, so an agent's
ordinary arguments would be refused constantly. Refusing by key name — `phone`,
`ssn` — was rejected for the reason this codebase has learned four times over: a
hand-written list of what matters is wrong the day someone adds to it.

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

### Why an allowlist entry carries a shape and not only a name

An allowlist admits a **key**. Nothing read the value under it, and a field
whose shape the provider constrains — a boolean, an enum, a fixed literal — is
an unmasked egress channel until something checks that value. Three of them
shipped: `"strict": "Martina Weber"` reached OpenAI, `"is_error": "Martina
Weber"` reached Anthropic, and `cache_control: {"type": "Martina Weber"}`
reached Anthropic through a list added *to check `cache_control`*, which checked
its keys. Each was written by somebody who knew the shape and wrote it in a
comment beside the entry.

Three found in one review pass is evidence about the population, so the answer
is not three checks. Every entry now carries **why admitting that key is safe**,
as a value the code consumes: `Described` (a slot addresses it), `Dispatch` (a
free identifier the caller chooses and the protocol routes on, argued above,
**and a string**),
`Elsewhere` (a named check at the use site decides it), `Unscanned` (forwarded
whole under the images-and-audio policy), `Bool` / `OneOf` / `Token` (the
provider constrains it, so this checks it), `Object` (a nested list). There is
deliberately no answer meaning "forwarded, and nobody has looked at it": a key
with no answer does not go on a list.

`Token` is the one that admits rather than enumerates. Anthropic's tool `type`
names a server tool and the names are dated — `text_editor_20250124`,
`web_search_20250305` — so a list of values refuses the next `bash_*` the day it
ships, which is the coding-agent traffic this slice exists to serve. The grammar
is closed even though the vocabulary is not, so `Token` holds it to that. It
**narrows the channel rather than closing it** — `martina_weber` still passes —
and that residual is the one `name` already carries as dispatch.

### Why `Dispatch` is a string, and why that is not the argument it looked like

The argument for leaving dispatch alone was that the published grammars —
`^[a-zA-Z0-9_-]{1,128}$` on Anthropic, the same at 64 on OpenAI — refuse
characters real clients use: MCP and others spell tool names with `.` and `/`,
so tightening the grammar refuses working traffic. That argument is about the
**character set**. It says nothing about the **type**, and `Dispatch` shared
`known_value`'s first arm with `Described`, `Elsewhere` and `Unscanned` — an
arm that is `Ok(())`. So `{"name": {"owner": "Martina Weber"}}` on a tool
definition was admitted, scanned by nothing and forwarded verbatim (measured, at
200). A structured value has no characters for the argument about characters to
be about.

Requiring a string costs nothing: every one of the ten dispatch fields is a
string in both providers' own definitions. The permissive grammar stays exactly
as permissive. **What it does not close, and this has to keep being said:**
`"name": "Martina Weber"` still forwards — that is what dispatch means. But the
residual is now *a string the caller chose*, which is a smaller and more precise
claim than the one the variant was making.

### Why `tool_choice` is described, and how it went unchecked

`tool_choice` is the one tool field admitted by **absence from a denylist**
rather than by presence on an allowlist. The sweep that gave every allowlist
entry a reason it was safe to admit covered the allowlists; a field on no list
is invisible to it, and what `tool_choice` was admitted on was a comment beside
the denylists arguing about its `name`. The name is dispatch and the argument
holds; everything around it travelled unchecked, so `{"type": "auto", "note":
"Martina Weber"}` reached both providers verbatim (measured, at 200).

Both providers publish a closed shape. Anthropic has four objects and no bare
string: `none` alone, `auto` and `any` with `disable_parallel_tool_use`, `tool`
with that and a `name`. OpenAI has the bare modes `none` / `auto` / `required`,
**or** an object — `{"type": "function", "function": {"name": …}}` or `{"type":
"custom", "custom": {"name": …}}`.

That union is the part `Admits` does not express, and it does not need to. A
body field is on no allowlist — `Admits` answers *why is this key on this list
safe*, and there is no list at body level for `tool_choice` to be an entry of —
so the union lives at the use site beside `logprobs`, `audio` and `thinking`,
which are the other body fields a named check decides. Each *object* below the
union is a `Field` list read by the same `known_fields` as everything else, and
which list is chosen by dispatching on `type`, exactly as `content_block_fields`
does. Bending a string-or-object union into `Admits::Object` would have needed a
variant meaning "or", and one answer per entry is the whole of the enum's shape.

**Refused rather than described:** OpenAI's `{"type": "allowed_tools",
"allowed_tools": {"mode": …, "tools": […]}}`. Its `tools` are tool definitions,
and describing a tool definition is `tool_definition_slots`' job — it produces
slots, masks the prose and charges the tool bounds. A second, name-only reading
of a tool definition here would be a second answer to a question this file
already answers, and two answers to one question in this file have drifted
twice. This **narrows behaviour** for a client using `allowed_tools`: it was a
200 and is now a 400. Describing it is a slice, not a fix.

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

`proxy.rs`'s `mask_all` learns the two slot kinds and nothing else. The
tool-field refusal loses the fields this slice masks; what remains refused stays
refused. It is three lists rather than the one this section first imagined —
`OPENAI_BODY_TOOL_FIELDS`, `OPENAI_MESSAGE_TOOL_FIELDS` and
`ANTHROPIC_TOOL_FIELDS` — because a field is only described where the protocol
puts it, and relaxing both levels together would have let a `tools` array
smuggled onto a *message* carry its descriptions past the masker.

### What is masked, by provider

A schema is walked as a `Json` slot like everything else, rather than by naming
the keywords that carry prose. An earlier draft named `description` alone, which
would have forwarded `default`, `const`, `enum`, `examples`, `title` and
`$comment` untouched — every one of them a client-controlled string. Naming what
to scan is the mistake this codebase has now made four times; the walk scans
every string value and the exclusions are named instead.

**An exclusion names a keyword *and* a shape, which is the same correction the
tool allowlists took and it arrived later here.** A keyword is excluded because
of what it holds — `required` holds property names, `type` holds a type name —
so consulting the list from the key alone excludes whatever the value happens to
be. `{"required": {"owner": "Martina Weber"}}` states no property name anywhere
and was copied into the egress untouched. Three arms of one `match` made that
mistake and each was corrected separately, on three different rounds:
`dependencies`, the name-stating keywords under `propertyNames`, and the
fourteen identifier keywords. They ask one shared question now — a name is a
string, a list of names is an array of them — and each identifier keyword is
held to the shape its own draft defines rather than to the union of the four,
because the union admits `{"$ref": {"note": ["Martina Weber"]}}`. A value that
is not the shape is not an identifier, so it is the client's data and is
scanned; the cost of being wrong is an over-masked schema the caller can see.

**Anthropic.** `tools[].description` and `input_schema` as a whole;
`tool_use.input` as `Json`; `tool_result.content` as text when it is a string and
as content blocks when it is a list.

**OpenAI.** `tools[].function.description` and `parameters` as a whole;
`tool_calls[].function.arguments` as `Json { embedded: true }`; messages with
role `tool` as ordinary text.

**Excluded, each because it is dispatch rather than prose:** tool names, schema
property names, `tool_call_id`, and `tool_choice`'s selector — meaning the
selector's *name string* and not the object holding it, which is checked like
any other shape. Keys are never masked by construction — the walk touches values
only — so property names need no special handling; the others are values and are
excluded by name, which is a list, and is therefore written next to the reason
it is allowed to be one: these four are matched by the *client* against strings
it authored, so masking one breaks dispatch rather than protecting anything.
Each of them is a string, and that much *is* checked: see "Why `Dispatch` is a
string" above.

### Restoration

`tool_use.input` and `tool_calls[].function.arguments` are restored before the
response reaches the client, because the client *executes* them. This is the
one place in the gateway where a restoration failure is not a display problem.

A placeholder the model invented, which the mapping never issued, does **not**
reach the client: `Mapping::restore` returns `MappingError::Unknown` for a
placeholder-shaped token it cannot resolve, and the response is refused. An
earlier draft of this document asserted the opposite without reading the
function, and the truth is the better behaviour — a literal `[PERSON_9]` handed
to a client's tool would be a wrong action taken on the gateway's authority.

The cost is real and belongs in **Known limits**: a model that hallucinates a
placeholder costs the caller the whole response, after the upstream tokens are
spent. Tool arguments make that more likely than prose does, because a model
copying an identifier between fields can copy a placeholder into one the mapping
never issued.

## Latency

**The detection cache does not help the first time a text is seen**, and a tool
result is usually seen once.

Two measurements exist and they are not the same measurement, which is worth
saying before either is used. The README's latency table is wall-clock on a
named machine — an Apple M3 Pro, 11 cores, CPU only — and reports 1 200
characters at 950 ms and 6 000 at 5 524 ms. The detection-cache design reports
20.3 CPU-seconds per 1 200 characters on the compose stack, which on the same 11
cores is about 1.85 wall-seconds for that text. Native against containerised,
roughly twofold apart, and neither figure carries its setup where a reader would
look for it.

Either way the conclusion is the same and does not depend on choosing between
them: 50 KB costs somewhere between 46 and 77 seconds of wall clock, against a
`detector_timeout_secs` that defaults to 30. So a `Read` of a 1500-line file is a
503, and no amount of tool-field support changes that.

This slice answers it by bounding rather than by accelerating: the tool
structures newly scanned in a request are refused past a configured size, in
aggregate. **Arguments count, not only results** — `Write` and `Edit` carry
whole file contents in their arguments, and an earlier draft bounded results
alone, which would have left the larger half open.

Arguments are the sharper case for a reason worth stating: a tool call the model
produced is restored to real values and echoed back by the client in the next
turn's history, and that text has never been detected — the cache holds the
masked request text, not the restored response. So a large generated argument is
first-seen on the following request, which is exactly when the timeout bites.

The ceiling as shipped is 20 000 characters and 40 detector calls, set from the
measurement in **Configuration** rather than from the timeout — which bounds one
call and never their sum, so it was never the quantity to derive this from. That
passes ordinary agent traffic — a 200-line file read, `grep` output, a small
edit, short `bash` output — and refuses a large file read or a whole-file write
honestly. An earlier draft put the ceiling "near 10 KB, at the default timeout",
which was both the superseded default and the wrong reason for it.

Making large results fast is its own work, filed as issue #28, with its own
measurements: chunking
across replicas with overlap and offset arithmetic, where a span missed at a
seam is raw egress; or the int8 and fp16 graphs that already sit beside the fp32
one in the model directory, whose accuracy cost the evaluation gates can
actually measure. Neither belongs in a slice about masking structure. The
detection cache slice was good because it did not also try to support tools;
this one should not try to also make the detector three times faster.

The ceiling rises when that work lands, and the configuration key it moves is
already in place.

### Why a document is one detector call rather than one per leaf

Detection costs almost nothing per character and a great deal per call: two NER
passes run whatever the text's length, so a two-character string costs nearly
what an eighty-character one does. Measured per call — 108 ms native from the
README's own 80-character row, 265–410 ms containerised.

Walking a document leaf by leaf therefore multiplies the dominant cost by however
many strings a client happened to write. On ten real tool definitions — 77 calls,
9 005 characters of text — that is **54.9 seconds containerised on a session's
first turn**, of which 57% is call overhead rather than text. Measured, not
derived: the payload's own leaves, taken through the same `json_leaves` walk the
gateway uses, replayed through `/detect` in sequence three times with salted text
so nothing could come from a cache. A correct payload from a correct client,
refused by nothing, and unusable.

The **native** equivalent is about 15 seconds and is **derived**, not measured:
this host's detector venv has no onnxruntime, so its detector runs the
deterministic layer alone. It comes from the README bench's 80-character row
(109 ms, per-call cost and almost nothing else) plus the 1 200-character row for
the marginal rate, and stays an estimate until someone runs the replay on a host
with the NER extras installed.

An earlier draft of this section reported **52 seconds containerised as
measured**. It was derived — characters over a throughput taken on 1 200- and
6 000-character texts, applied to leaves averaging 116, where per-call overhead
dominates and throughput barely applies — and it was replaced by the replay above
at `53e94cb`. It landed within 5%, which that commit records as luck rather than
as method.

No pair of bounds fixes that. Lowering them converts the wait into a refusal of a
client that did nothing wrong; raising them makes the wait longer. So the leaves
of one document are detected in one call, and the cost scales with text rather
than with how a client chose to divide it: the same payload becomes **20 calls
rather than 77**, and the 57 calls that go away were costing 265–410 ms each, so
fifteen to twenty-three seconds of pure overhead leave with them. That last
figure is derived from the two measurements either side of it rather than
replayed — the post-batching wall clock has not been re-measured. The dominant
term is gone.

This is not issue #28's work, which is about making detection faster on a large
text. It is about not making seventy-seven calls where one will do, and the
per-leaf pattern is one this slice introduced.

**The seam is the risk, and it is a milder one than chunking.** Reassembling
spans across concatenated leaves means offset arithmetic, and a span missed at a
boundary is raw egress. But the boundaries here are chosen rather than
discovered: the gateway knows exactly where each leaf begins and ends, so a span
that straddles one is detectable and is refused rather than silently dropped.
Issue #28's chunking has no such luxury.

## Errors

Every failure is a refusal, and the vocabulary is the existing one. A document
past the depth or node bound, a numeric leaf carrying a span of any type but the
fourteen NER ones, a content block whose shape this gateway does not
understand, a tool field it has no rule for, `mcp_servers`, tool structures
summing past either bound, and tool traffic arriving with `stream: true` are all
refused before the upstream call.

Refusing *before* it is the whole point in each case: every one of these is
knowable from the request alone, and a refusal issued after the upstream call
costs the caller tokens for an answer they will not receive.

Nothing here degrades. A partially masked argument is worse than no answer,
because the client would execute it.

## Configuration

**Two keys, not one — an earlier draft of this section named only the first, and
batching is why there are two.** Both apply to the sum of the tool structures a
request newly scans rather than to any one of them.

`max_tool_chars` defaults to **20 000** and bounds the characters those
structures send to the detector. `max_tool_calls` defaults to **40** and bounds
the detector round-trips they need. The second exists because the first cannot
stop what it used to stop: once a document's leaves are detected in one call
(see **Why a document is one detector call**), ten thousand tool definitions
each carrying a one-character description are ten thousand characters — inside
the character bound — and ten thousand sequential calls. The key was
`max_tool_leaves` and counted strings, which stopped being what a call costs the
day batching landed; it is renamed to what it now counts rather than left
pointing at a quantity that no longer prices anything.

**Both defaults are twice a measurement, and the measurement is a floor.** Ten
real Claude Code tool definitions, pinned in
`gateway/src/testdata/claude_code_tools.json` and asserted by
`mapping::tests::a_real_tool_payload_fits_the_bounds_this_gateway_ships_with`:
**13 177 bytes serialized against 9 193 characters charged**, in **20 calls**
(ten descriptions, ten schemas). The 9 193 is 9 005 characters of text, 50 of
numbers and 138 of the separators the join inserts, all three of which the
detector reads. So charging serialized size would charge 1.43 characters for
every one detection actually costs — braces, quotes and property names, none of
which the detector sees.

The doubling is asserted rather than described, because the same rule stated in
a comment was already quietly false once: twice the payload is 18 386 characters
and 40 calls, against defaults of 20 000 and 40. An earlier pair, 18 000 and
18 010, failed by ten characters and was raised. The call bound holds at 40 ≥ 40
exactly, with no headroom, so one more tool in the testdata breaks the
assertion — which is the assertion working. A floor that has moved is a
measurement to retake and a default to reconsider, not a rule to relax.

The figures this section carried before were **10 000**, and 10 970 bytes
against 7 379 characters from an **eight**-tool payload. Both were superseded
during the slice — the default at `53e94cb`, the measurement when the payload
grew to ten tools and again when numeric leaves and join separators started
being charged — and the section was not updated with them. Recorded because a
configuration section quoting a superseded default is trusted precisely for
being specific.

**`detector_timeout_secs` is a third thing, not a restatement of the first — an
earlier draft of this document had that wrong too.** It becomes a per-request
timeout on each HTTP call to the detector, so it bounds one call and never their
sum: a request making forty calls of a hundred characters can spend a minute
without any single call approaching it. No cumulative deadline exists anywhere on
the request path. These two bounds are what cap how long a caller waits for the
whole request.

Deriving either default automatically was rejected: a derived default would
silently change behaviour when an unrelated key moved.

Both are in `gateway/tessera.example.toml`, `deploy/tessera.container.toml` and
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
4. A numeric leaf carrying a span of a deterministic type refuses the request
   rather than forwarding it, and one carrying an NER label alone does not.
5. A document past the depth bound refuses rather than recursing.
6. A tool call restored for the client carries the original values, not
   placeholders.
7. A tool result over the size bound refuses before the upstream call, and so
   does a tool argument, and so does the two of them summing past it.
8. A request carrying both `stream: true` and tool traffic refuses before the
   upstream call.
9. The journal's counts include spans found in tool traffic, and record no key
   name and no argument value.
10. A schema keyword that is not `description` — `enum`, `default`, `title` —
    has its values masked like any other string.

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

**A hallucinated placeholder costs the whole response.** See **Restoration**.

**A phone number sent as a JSON number is forwarded.** The vocabulary has no
telephone entity, so nothing detects it — in tool arguments or in ordinary text.
See the numeric-leaf section; closing it is detection-quality work, like issue
#20.

**A number an NER label alone finds is forwarded.** A numeric leaf is exempted
by the fourteen NER types and by nothing else, so a name or a place written as a
JSON number goes through. That is deliberate and argued in the numeric-leaf
section, with the measurement on both sides of it. The exemption is written as a
hole in the refusal rather than as the refusal itself: the eight deterministic
types refuse, and so does a type in neither half of the vocabulary, because the
argument for the exemption is an argument about what those fourteen labels
*mean* and says nothing about a name no catalog declares.

**A large tool result is refused, not served slowly.** See **Latency**, and
issue #28, which exists to raise this ceiling.

**The closed allowlist refuses citations and two of Anthropic's server tools.**
A `text` block carrying `citations` is refused on the request path, and clients
echo assistant turns back as history, so a conversation that used citations
refuses on its next turn. `computer_*` (display dimensions) and `web_search_*`
(a `user_location` whose `city` is a `LOCATION` in this vocabulary) carry
configuration the allowlist has no rule for and are refused with it;
`text_editor_*` and `bash_*` declare a name and a type and nothing else, so they
pass and the coding-agent category is unaffected. This is the allowlist working
in the direction it was chosen for — a field no slot addresses would otherwise
travel exactly as the caller wrote it — and the follow-up for citations is to
describe `cited_text` as a slot rather than leave the feature closed.

The **response** path refuses them too, and that is a second narrowing recorded
rather than assumed: a response block now carries a closed list of fields as
well as of types, so a populated `citations` is a 502 where it used to be a 200
with the gateway's own placeholder inside `cited_text`. The refusal cannot fire
on traffic this gateway accepted — no request the allowlist admits can enable
citations — so its cost falls only on a provider answering in a shape we did not
ask for, where the alternative was handing the placeholder to the client. The
`"citations": null` Anthropic sends on **every** response text block is admitted,
because a list that omitted the key would refuse every well-formed response.
Describing `cited_text` as a slot is one change across three sites: the request
allowlist, the response dispatch, and the response field list.

**Masking tool definitions degrades tool selection, measurably.** The decision
above accepted this in principle; here is the number. Ten real Claude Code tool
definitions, containing no personal data whatsoever, yield **thirteen spans** —
tool names, ordinary English words in capitals, a parameter called `main`. Every
one is masked, so the model chooses among tools whose descriptions have been
partly replaced by placeholders.

This is not a detector defect to be tuned away: `main` and `ORG`-shaped words are
exactly what a general-purpose recogniser is built to catch, and a name really
can appear in a tool description. It is the cost of the promise, paid in a place
where the text is instructions rather than content. Recorded here because an
operator seeing `[PERSON_1]` inside a tool description should find it explained
rather than report it as corruption.

Batching relocates these rather than reducing them: measured across the same
payload, per-leaf and joined detection both found thirteen, differing in four —
and all four differences were false positives on both sides.
