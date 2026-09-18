# The gateway

How the reverse proxy masks a request and restores its response: what it accepts, what it masks in tool traffic, which credentials it forwards, and every shape it refuses.

Configuring the streamed path is in [streaming.md](streaming.md); per-conversation mapping is in [sessions.md](sessions.md); the evidence journal is in [audit.md](audit.md).

## What it accepts

Point a client's base URL at the gateway and personal data stops leaving the process:

```
cd gateway && cargo run -- tessera.example.toml     # 127.0.0.1:8080
```

It accepts the OpenAI shape at `/v1/chat/completions` and the Anthropic shape at
`/v1/messages`, including Anthropic's separate `system` field and both providers'
content-part arrays. Detected spans become typed placeholders — `[PERSON_1]`, `[IBAN_2]` —
and an identical value always gets the same placeholder within a request, because two
placeholders for one person would tell the model there were two. Identifiers outside the
message content are masked too — OpenAI's `user` and per-message `name`, Anthropic's
`metadata.user_id`. The response is restored before it reaches the client, and an upstream
error keeps its own status and body so a rate limit still reads as a rate limit.

## Tool traffic

Tool traffic is masked on the buffered path, for both providers and in both directions: a
tool definition's description and the whole of its schema — `enum` members, `default`,
`title`, `examples`, not `description` alone — a tool call's arguments, and a tool result.
A schema keyword's value is left alone only where the keyword really states an identifier,
which is a question about the string and not only about its container: `{"type": "Martina
Weber"}` names none of JSON Schema's seven types, so it states no type and is masked like
any other value the walk does not recognize, inside a well-formed `allOf` or out of one.
The check runs to wherever the keyword's own draft stops being unambiguous and no further,
because one stricter than the draft breaks a working schema at the caller: a media type is
read down to its RFC 2045 parameters, so `"text/plain; Martina Weber"` states no parameter
and is masked while `"text/plain; charset=utf-8"` is left alone; `$dynamicAnchor` is held
to 2020-12's pattern alone, because 2020-12 is the only draft that defines the keyword,
while `$anchor` keeps the union of both drafts that do; and a `pattern` is checked for
structure — a group that closes, a class that closes, a backslash with something to escape
— rather than parsed, because the parsers available here reject lookaround and
backreferences that ECMA-262 and JSON Schema both allow. Each of those leaves a residual,
and each residual is named where the check is. Only the keywords holding the caller's *own*
vocabulary — `required` and `dependentRequired`, which hold property names — are exempt
entirely, because there is nothing to check them against and a rule invented for them would
mask a schema that was correct.

Names are not. A tool's name, a schema property name and a `tool_call_id` are the client's
own dispatch, matched against strings it authored, so masking one breaks the call and
leaves the client no way to learn why. What is required of them is that they be *strings*:
the argument for leaving the characters alone is an argument about the character set, and
a structured value has no characters — `{"name": {"owner": "Martina Weber"}}` was admitted
and forwarded verbatim until the type was checked. So the residual a name carries is a
string the caller chose, which is smaller than "whatever the caller put under `name`". Arguments are walked as JSON and only string leaves
are touched, so the masker never sees a brace or a quote and cannot hand back a document
the client fails to parse — and a tool call in a response is restored before the client
executes it, the one place here where a failed restoration would be a wrong action rather
than a wrong display.

Masking a definition costs two things, both deliberate and both visible from outside. A
definition is prose the model reads, so a placeholder in one changes how the model chooses
a tool: ten real Claude Code tool definitions carrying no personal data at all yield
thirteen spans — tool names, ordinary English words in capitals, a parameter called
`main` — and every one is masked. `[PERSON_1]` inside a tool description is this working
rather than corruption. And definitions are scanned on a session's first turn, which is
seconds of detector time a caller waits through once; every turn after that is free,
because definitions are byte-identical and the detection cache serves them.

Provider credentials pass through on a per-provider allowlist: `Authorization` and OpenAI's
routing headers go to OpenAI, `x-api-key` and `anthropic-version` go to Anthropic, and
nothing else goes anywhere. A caller holding both sets of credentials does not have one
provider's key posted to the other, and a client's cookies are nobody's business but the
client's. Coming back, the provider's status and its rate-limit headers are preserved, so a
429 still reads as a 429 with its `Retry-After`.

## What it refuses

**Every failure refuses the request**, and refuses it *before* the upstream call wherever
the problem is visible there. A detector that errors or exceeds its timeout; a body whose
shape the gateway has no rule for, including Anthropic's
extended thinking, OpenAI's `logprobs`, whose token strings are the masked output again, and
OpenAI's audio output, whose transcript no restoration can reconcile with the recording; an identifier field present in a form that cannot be masked; a
span the detector reports at a position that cannot be applied — inverted, past the end of the
text, or overlapping another; and a placeholder that no mapping knows in a response field this
gateway *describes* — each of these ends the request. Once a stream has begun there is nothing
left to refuse, so it ends mid-flight instead; the rule it protects is the same. No text
this gateway scans is forwarded unmasked. No error body or log line carries the submitted
text.

That last refusal used to be stated without the qualification, and it was true because the
gateway looked nowhere else. It looks everywhere now — see the next section — and an unknown
token in a field nobody describes is deliberately *served* rather than refused. Refusing on
it would turn a response forwarded verbatim yesterday into a 502 today, which is a request
that succeeds starting to fail; the gain in coverage is not worth paying for in traffic that
already works.

Restoration is narrower than that, and the difference is measured rather than assumed.
Anthropic's response path is a closed list of block *types* — a block whose type it cannot read
refuses the response rather than handing a placeholder over. Two things were measured wrong on
that half and both are now closed. The type check ran *second*, after a check for a `text`
field, so a block carrying a `text` never reached the closed list at all: `{"type": "tool_use",
"text": "ok", "input": {...}}` had its text described and its arguments forwarded unrestored,
which is a placeholder reaching a client that *executes* it. And the list was closed on the
type alone, so a field the block's type does not define was handed over as the provider wrote
it — measured, at 200, with `citations[].cited_text` carrying `[PERSON_1]` into what the client
received. Anthropic's response blocks now have a closed list of *fields* as well as of types,
and a field outside it refuses the response.

OpenAI's response path had neither, and #31 is the measurement: it restored a choice's
`content` and forwarded every other field of the message as the provider sent it, so a
`refusal` — which any OpenAI refusal populates — and an `annotations[].url_citation.title` both
reached the client **with this gateway's own placeholder in them**, at 200. It no longer
describes only what it names. Before the slots are written, the whole upstream body is swept
for the placeholders *this request* issued, and the slot loop then overwrites what it describes
with its own strict result, so a field nobody described is restored rather than forwarded. The
promise, in both its clauses: **a placeholder issued by this gateway does not reach the client
from a field the gateway describes. Elsewhere everything this request issued and the caller did
not write is restored, except where restoring it would drop something the upstream sent**,
which is served exactly as it came. Both clauses are load-bearing and neither is decoration.
The sweep will not claim a token the *caller* wrote itself, because a caller that puts
`[PERSON_1]` into a later turn of a session and has the model echo it back has to get its own
text returned rather than turn one's value; a token that is both issued and written is
ambiguous by construction and is left alone, and #32 is what separates the two and lets the
first sentence be stated without its qualification.

The second clause is one rule and not a list of cases — what cannot be re-serialized
faithfully is left, never guessed at. An object whose restored keys would collide —
`{"[PERSON_1]": "a", "Weber": "b"}`, where restoring the key would drop one of the two fields —
keeps every key and value it arrived with, which is one object rather than the body around it.
And a string that is itself a serialized document is left whole whenever restoring it would
need re-serializing and the gateway cannot write the document back as it came: two members of
the same name — `{"mode":"safe","mode":"admin","name":"[PERSON_1]"}` — which the parse
collapses before any restoring happens, a number carrying more precision than the double it is
read into, or a text this gateway cannot rule out being a document a client's reader would
accept. That last one is wider than it sounds, and deliberately: ruling it out would mean
knowing which prefixes, literals and quote characters some other parser tolerates, and six
attempts to know that were each defeated by the seventh thing. So the test is only whether a
`{` or `[` was opened before the placeholder — which also catches ordinary prose that opens a
bracket, `- [x] [PERSON_1] said "hi"` among it. The list is open on purpose; the rule is what
holds. These are still places a placeholder can reach the client from.

That last case is reached only when the value being restored carries a character that could
end the string it lands in, or start an escape inside it — a quote of either kind, a backtick, a backslash, a control
character, a slash. Names, addresses, e-mails, IBANs and phone numbers do not, and take the
plain path untouched; `O'Brien` does, and so does a `Steuernummer` spelled `21/815/08150`.
Neither loses anything inside a document that parses, where the restoration is structural and
escaped, nor in ordinary prose, which opens no container — only in prose that opens one. So it
is a narrow price for a guarantee that assumes nothing about the client's parser.

That guarantee covers a client that **parses** the response as data. It does not cover one that
**evaluates** it as code, and no rule of this kind could: under evaluation a comma or a colon is
enough on its own.

**Leaving is an answer the second clause gives, and the first clause never gives it.** The same
shapes inside a field the gateway describes — `arguments` a client dispatches on, `content` it
parses — refuse the response with a 502 instead, under the class `mapping_lossy_document`.
Leaving the bytes there is not the harmless answer it is elsewhere: those bytes still hold the
placeholder, which is the one thing the first clause promises they will not. Serving them
re-serialized is worse again, since it hands a client a document to execute with a member
dropped, a key renamed or a number rounded. So neither is served, and the rule the two clauses
share is the narrower one: what cannot be re-serialized faithfully is never guessed at.

Two things it does not scan, and both are worth knowing before you rely on *no text this
gateway scans is forwarded unmasked* above — a claim about the way up, which the two paragraphs
before this one are not. **Image and audio parts are forwarded untouched**, including a
screenshot inside a tool result, which is the same exposure through a different field rather
than a new one. Nothing here reads pixels, so a photograph of an identity document reaches the
provider as the client sent it. And **the body and the message levels are not allowlisted at
all** — see below, because that is the other half of the closed-allowlist claim.

Tool traffic is masked now, so what it still refuses is worth stating on its own.
**Streamed tool calls**, which the buffered path's masking does not reach: a document
arriving a delta at a time is not well formed until its block closes, so masking it means
buffering the block first, which the streamed path does not do yet. A
**tool-field shape the gateway has no rule for** — and the rule is a *closed allowlist*, so
an unrecognized content-block type, or a field beside the ones each tool structure is
described by, is refused rather than forwarded. That is deliberately the expensive
direction: a field no slot addresses would travel to the provider exactly as the caller
wrote it, so a provider feature shipped tomorrow refuses here instead of leaking through.
**The allowlist admits a shape and not only a key**, which is the other half of that and
was the later half: every entry records why admitting it is safe, and a field the provider
constrains to a boolean, an enum or a fixed literal has that value checked. **And a block
type is admitted in a position, not in general** — the later half again, one layer out:
the same walk reads OpenAI's message content, Anthropic's message content, Anthropic's
`system` and a `tool_result`'s own content, and it admitted every block type in all four.
An Anthropic `tool_use` therefore rode in an OpenAI message with its `name` — dispatch, so
scanned by nothing — and tool blocks were accepted in a `system` prompt that takes text
blocks only. Each position now carries the set its provider publishes for it. `"strict":
"Martina Weber"`, `"is_error": "Martina Weber"` and `cache_control: {"type": "Martina
Weber"}` were each admitted by a list and each reached the provider verbatim, under
comments that stated the shapes correctly. **A field can also be admitted by not being
refused**, which is the case the allowlist rule does not cover: `tool_choice` and OpenAI's
`parallel_tool_calls` are body fields, and there is no allowlist at body level for them to
be entries of, so they were admitted by absence from the denylists and read by nothing.
Both are described now — each provider's published `tool_choice` shapes, and a boolean —
with one exception stated plainly: OpenAI's newer `tool_choice: {"type": "allowed_tools",
…}` nests tool definitions this gateway does not describe there, and is refused rather
than forwarded, which is a 400 where there used to be a 200. **And a field can be admitted
by an allowlist that something else selects**, which is the third way and the narrowest:
OpenAI's `tool_call_id` is on the allowlist for a `role: "tool"` message, that allowlist is
chosen by the role, and the denylist running for every message did not carry the field — so
`{"role": "user", "content": "hi", "tool_call_id": {"owner": "Martina Weber"}}` was
addressed by nothing and forwarded. It is refused on every other role now, in the same
`if` that selects the allowlist, because a denylist entry would refuse the tool message the
field belongs to.

**That closure is scoped to the tool structures, and the body and the message levels have
no allowlist at all.** It is true of a tool definition, a tool call, a tool result and a
content block, where it was hard-won; it is false one field over. A body field this gateway
describes no slot for travels to the provider exactly as the caller wrote it — verified for
OpenAI's `response_format.json_schema.schema`, which is *the same artifact* as a tool's
`parameters` (a client-authored JSON Schema whose `description`, `enum` and `default` are
prose the model reads); `prediction.content`, the Predicted Outputs field editor clients
fill with whole file contents; `metadata`; `stop`; a top-level `safety_identifier`; and
Anthropic's `stop_sequences`. A field invented on a *message* travels the same way. If you
are deciding whether to point an agent at a customer folder: the prompt, the tool
definitions, the tool arguments and the tool results are covered, and the request envelope
around them is not.
It is also what closes two things a caller may miss. Anthropic's **citations** are refused
on the request path — a `text` block may carry `cited_text`, which is quoted source
material, and clients echo assistant turns back as history, so a conversation that used
citations refuses on its next turn. And **every tool Anthropic runs itself** goes with
them — `web_search_*`, `web_fetch_*`, `code_execution_*`, the tool-search tools, the
advisor — because the answer to one is a `server_tool_use` block and a result block of the
tool's own, and this gateway describes neither. It used to refuse those *after* the model
had run: a bare `{"name": "t", "type": "code_execution_20250522"}` passed the definition
gate, the request was forwarded, the tokens were spent, and the caller received a 502.
The type is checked before the call now, so the same refusal costs nothing. What passes is
the tools the **caller** runs — `bash_*`, `text_editor_*`, `computer_*`, `memory_*` — whose
results come back as the ordinary `tool_result` the caller sends, and a version of one of
those is admitted the day Anthropic ships it, so the coding-agent category is unaffected.
The fields a definition may carry are read **per type**, because they differ per type:
`computer_*` carries the display Anthropic documents as required (`display_width_px`,
`display_height_px`, and optionally `display_number` and `enable_zoom`) and
`text_editor_*` carries `max_characters`, each checked as a number, and each refused on a
type that does not define it.
Describing those response blocks is the follow-up; refusing is not the finished feature. Anthropic's **`mcp_servers`** is refused for a sharper
version of the same reason: it grants the model tools this gateway never described, so
their calls and results arrive shaped by a server it cannot account for — and it carries
the caller's own `authorization_token` besides. A **number that carries personal data** is
refused rather than masked, because replacing `4111111111111111` with `[CREDIT_CARD_1]`
turns a JSON number into a string and a schema that declared a number may reject it; that
refusal steps aside for exactly one thing: a span carrying one of the fourteen NER types,
since an NER label on a bare digit run is a judgement about meaning where there is no
meaning to judge, so a number those labels alone find is forwarded. The eight deterministic
identifiers refuse, and so does a type in **neither** half — a label this gateway does not
recognize is a detector reporting personal data of a kind nobody here can weigh, which is
not the same thing as a label known to be ungrounded on digits. And a
request whose tool structures exceed `max_tool_chars` or `max_tool_calls` is refused before
the detector is called at all.

Placeholders carry the type the detector reported, but only when it is one this gateway
declares — twenty-two of them, the catalog's eight deterministic identifiers plus the
fourteen the NER layer can label. A type outside that list is masked as `[REDACTED_1]`
instead. Syntax cannot tell a type name from a value shaped like one: a detector returning
`WEBER` as the type of a span covering `WEBER` would otherwise put that value in the token
the provider receives. The gateway keeps its own copy of the list rather than asking the
detector, since the detector's answer is what the check defends against, and CI fails if the
two drift apart.
