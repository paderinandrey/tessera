# Tessera

**Tessera** is a self-hosted privacy gateway for LLM traffic. It sits as a transparent
reverse proxy between your application and an LLM provider (OpenAI- or
Anthropic-compatible APIs): personal data is replaced with placeholders on the way to
the model and restored in the response. Your application changes one thing — the base URL.

> In ancient Rome, a *tessera* was a token that stood in for an identity.
> Tessera replaces identities with controlled tokens — and puts them back.

## Why

Companies in regulated environments (banks, law firms, fiduciaries, insurers, medical
networks) want to use LLMs but cannot send client data to a third party. Tessera is a
**technical measure of pseudonymization** (GDPR Art. 32(1)(a)): the mapping table never
leaves your perimeter, the provider only ever sees placeholders.

**What Tessera does not claim:** it does not take you out of GDPR scope, and it is not
anonymization. Pseudonymized data remains personal data for you as the controller.
Tessera reduces exposure and gives your DPO a measurable, evidence-backed argument —
nothing more, nothing less.

## Design principles

- **Detection quality first.** The moat is high-quality PII detection in **French and
  German** (with code-switching), Swiss and EU identifiers with checksum validation,
  GDPR Art. 9 special categories, and quasi-identifiers — measured on a reproducible
  benchmark.
- **Fail-closed by default.** An unparsed request body or a lost mapping aborts the
  request; it never silently forwards raw data.
- **Nothing leaves the perimeter.** Self-hosted, no telemetry, no license phone-home.
  Audit logs never contain original values.
- **Minimal infrastructure.** Two containers (Rust gateway + Python detector), config in
  TOML/YAML, in-memory session mapping, append-only JSONL audit. No database, no Redis.

## Architecture

```
tessera/
  detector/    Python detection service: deterministic recognizers with checksum
               validation, NER (GLiNER/ONNX), context boosting. Stable HTTP contract.
  gateway/     Rust reverse proxy: drop-in base URL for OpenAI- and Anthropic-shaped
               requests, placeholder substitution and restoration, buffered and
               streamed, with per-conversation sessions and an audit journal built in.
  evaluation/  Public synthetic corpus and metrics harness. The manually annotated
               corpus stays private and never enters this repository.
```

## Running it

Two containers, and nothing else to install:

```
docker compose run --rm weights           # once: 2 GB of NER weights
docker compose up -d --build              # gateway on 127.0.0.1:${TESSERA_PORT:-8080}
```

**Download the weights first.** The detector builds its pipeline once, at
startup, because loading the model takes seconds and paying that per request
would be absurd — so a detector that started without weights stays
deterministic-only until it is restarted, however much you download afterwards.
Adding them to a running stack therefore needs `docker compose restart detector`,
and skipping that restart is the one way to end up with a successful 2 GB
download, a gateway that looks installed, and names, places and health mentions
still reaching the provider unmasked.

The gateway does serve before the weights are there, deliberately. Without them
the detector runs its deterministic layer alone — checksum-validated
identifiers, the layer that scores 1.000 — and a partial install stays visible
rather than silent: the detector's own `GET /health` reports `ner: false` and
why, though it answers only on the compose network, never on the host. The
download is a separate command on purpose: a gateway that belongs inside your
perimeter should not reach the internet on its own, and a first start that
blocks for minutes is indistinguishable from one that has hung. It is the same
download `make model` does for a local, non-containerized detector; here it lands on a named volume,
so it is a one-time cost per host rather than a cost of every `up`.

Only the gateway is published, and on `${TESSERA_PORT:-8080}` rather than a
fixed `8080` — a variable, because a host that already has something bound to
8080 should not have to edit a file to try Tessera; set `TESSERA_PORT` before
`up` to use another port instead. The host address is a variable too, and
defaults to loopback: the gateway authenticates no caller, it forwards
whatever credential arrives, so reaching it from beyond this host is a
deliberate act — `TESSERA_BIND=0.0.0.0` — and never a side effect of the
default. It holds no key of its own, so what you publish is not your provider
credit but an unauthenticated relay out through your egress and into a journal
whose worth is that it records your traffic and not a stranger's: strangers can
fill the session table until legitimate callers are refused with a 503, and
anyone who already holds one of your callers' keys can guess a session id — they
are chosen by the client and need not be secret — and read that conversation's
real values back out of its table, which going to the provider directly would
never have given them. Put an authenticating proxy in front of it before you do.
The detector answers on the compose network and nowhere else: `POST /detect`
takes arbitrary text and authenticates nobody, so exposing it would be a way to
run text through the model outside the gateway, and therefore outside the audit
journal.

The journal and its salt share the `audit` volume and must stay together — a
journal with records whose salt has gone missing refuses to start rather than
silently renumbering every tenant beneath you. Back up that volume, not just
the file. `docker compose down` stops the stack and keeps the journal, its
salt and the downloaded weights; add `-v` only when you mean to discard them
— journal and salt together, since one without the other is what refuses to
start back up.

### Seeing it work without an API key

```
export TESSERA_PORT=${TESSERA_PORT:-8080}
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up -d --build
curl -X POST http://127.0.0.1:${TESSERA_PORT}/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer sk-demo' \
  -d '{"messages":[{"role":"user","content":"Meine IBAN lautet CH9300762011623852957."}]}'
```

The overlay replaces the provider with a stand-in that records what reached it,
so one request shows both directions at once:

```
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml \
  exec mock-provider cat /received/received.json
```

The answer you got back carries the real IBAN. What the provider received
carries a placeholder in its place, never the value itself. `make compose-smoke`
asserts exactly that, plus that the journal recorded the request without
quoting any of it.

## CLI

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

## Gateway

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
read into, or a document this gateway's reader rejects and a client's would accept. The list is
open on purpose; the rule is what holds. These are still places a placeholder can reach the
client from.

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

### Sessions

Within a request an identical value always gets the same placeholder. Across the turns of
a conversation it does too, if the client sends `X-Tessera-Session: <id>` — otherwise each
request gets its own table, which is the behaviour without the header.

The id does not select a session on its own. A session table is a restoration oracle: put
`[PERSON_1]` in a prompt, get it echoed by the model, and the gateway would restore it to
a real name on the way back. So the store keys on a salted fingerprint of the caller's own
credential as well as the id, and a guessed id lands in an empty namespace. The boundary
is the credential, not the id: callers who share one API key share one namespace, and
within it any id is reachable by anyone holding that key. The raw id
never reaches a log either — a client may well name its session after the person in it.

The table holds real values in memory between requests. It is the only place in the
gateway where that is true — the detection cache below retains data across requests too,
but never a value, only a span's type and offsets — so the session table is bounded
three ways: `session_idle_secs`, `max_sessions` and `max_session_values`. Reaching
`max_session_values` or the idle TTL costs coreference,
never protection. The client holds restored text and sends the history again, so a
session that was evicted is rebuilt from scratch by the next request — `[PERSON_3]`
becomes `[PERSON_1]` and nothing else changes. Past `max_session_values` a value is
still masked and still restored; it is simply not remembered.

Reaching `max_sessions` is the one bound that can cost a request rather than a
coreference. A session table is only ever reclaimed from a conversation that has
no request holding it; when every table in a full store is in flight, a request
asking for a *new* session is refused with a 503 rather than served by evicting a
live one. Evicting one would leave that conversation with two unsynchronized
tables, and two concurrent requests can then give one placeholder to two
different values — which is a wrong name in a response, not a lost coreference.
A request for a session the store already holds is never refused.

Values are never evicted from within a live session: one that came back from the model
would end a request with nothing to restore to.

Detection itself is cached, separately from the session table above and whether or not
a session is attached: a text is scanned once per detector version and credential, and
every repeat after that — the whole history a client resends on each turn — is served
from memory instead of calling the detector, which is what turns a conversation's cost
from growing with the square of its length back to growing with it linearly. What is
remembered is a span's type and two offsets, never the text or the value, keyed on
digests of the detector's version, a salted fingerprint of the credential, and the text
itself — so a hit never reveals, even through timing, that one tenant sent what another
tenant sent before it. The session stabilises a placeholder that detection produced,
cached or not; it is never asked to find personal data on its own.

The cache is bounded on two dimensions, the same relationship `max_sessions` and
`max_session_values` have to each other: `detection_cache_entries` (default 10 000)
bounds how many texts it remembers, and `max_spans_per_entry` (default 250) bounds how
many spans one remembered text's detection may carry — without the second, a single
span-dense text could outweigh thousands of ordinary ones. 250 is sized against measured
density rather than assumed: real text runs roughly 1.0 to 2.5 spans per 1 000
characters, so the default covers prose to about 100 KB, logs to about 188 KB and source
to 250 KB — every realistic single tool result. A detection over the cap is masked,
restored and returned exactly like any other; it is simply not stored, so an oversized
result never becomes a refusal, only a permanent miss. At the shipped defaults that
arithmetic comes to about 118 MB, which is a typical case rather than a ceiling: the 46
bytes per span were measured against real detector output, where a type name is `PERSON`
or `IBAN`, and the cache now declines any entry carrying a span whose type name runs past
40 bytes, so the true ceiling is nearer 200 MB — analytical, from the struct's layout,
rather than re-measured the way the 46 bytes were. Unlike the session table, the cache has no idle TTL — an entry
outlives its conversation and stays reachable for as long as the process runs, until the
detector's
version changes or the cache fills and something else is used more recently. And unlike
the session table, losing an entry costs time, not protection: a full cache evicts
rather than refusing, and a poisoned lock degrades to calling the detector rather than
failing the request. Set `detection_cache_entries = 0` to disable the cache entirely —
the gateway then calls the detector for every text, with no cache in the loop at all.

That coverage is measured against code, logs and prose — a coding agent's traffic. A
uniformly dense text — a contact list or an intake form, not ordinary correspondence,
which is prose-shaped at nearer 2.5 spans per 1 000 characters and is not affected —
crosses the cap at single-digit kilobytes: 8 to 16 KB at the density this repository's
own evaluation corpus annotates, offered here only as an illustration of where the cap
lands on text that is dense throughout, not as a claim about a buyer's traffic — every
row of that corpus is a single rendered sentence under 126 characters, and nothing else
here is shaped like a client document either, so the figure awaits its real measurement:
spans per 1,000 characters over actual gateway traffic. Because
the cache keys per text rather than per document, this bites a dense message arriving as
one text — a conversation *about* a dense file is many short turns that all cache
normally, and a long document is rarely uniform, the way a contract is prose everywhere
but its header and signature block. This is a real limit rather than a number worth
chasing with a bigger default: raising `max_spans_per_entry` is the deliberate lever for
a deployment whose texts really are dense throughout, priced by the formula in
`gateway/tessera.example.toml` (`entries × (264 + spans × 46)`), and recomputed there
against the deployment's own texts.

The tool structures a request newly scans have two bounds of their own, and unlike the
cache's, exceeding either is a refusal rather than a miss: `max_tool_chars` (default
20 000) bounds how many characters they send to the detector, and `max_tool_calls`
(default 40) bounds how many detector round-trips they need. Both are denominated in what
detection costs rather than in what the request weighs. A document is **one call however
many strings are in it**, so a schema of a thousand short values is a single round-trip;
and the characters charged are the ones the detector reads, not the braces, quotes and
property names carrying them, which cost it nothing. Both defaults are twice a measurement
taken on ten real Claude Code tool definitions, which charge 9 193 characters across 20
calls. That payload is a **floor** and is stated as one: a stock session also carries tools
the measurement did not, and an MCP server adds more, so a large enough tool payload is
refused rather than served slowly. Issue #28 — making detection fast on a large text — is
the work that lifts the ceiling; until it lands, the honest answer to a payload past these
numbers is a refusal, not a wait long enough that the client's own HTTP timeout would cut
it off anyway.

A request refused before the upstream call leaves its session exactly as it was. Asking for a
session the gateway cannot honour — a malformed id, no credential to namespace it, or
`session_idle_secs = 0` — is refused before the detector runs rather than served without
the coreference it asked for.

### Streaming

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
gets a truncated answer, never a placeholder in place of a name. Streamed tool calls
(`tool_calls`, `input_json_delta`) end the stream, and no longer for the reason they once
did — the buffered path masks tool arguments now. What it cannot do in fragments is read
them: a document arriving a delta at a time is not well formed until its block closes, and
a placeholder can be split across two deltas *and* land inside a half-written JSON value
at the same time, so masking one means buffering the whole block first — which is exactly
what the streamed path does not do yet. A request carrying tool traffic together with
`stream: true` is therefore refused before the upstream call, where it costs no tokens,
and a tool block the model produces inside a stream that was allowed ends that stream.
Extended thinking is refused before
the upstream call rather than at its first streamed block, so the refusal costs no tokens.

Whatever was already restored is served before the error event, whether the stream ends
because the connection broke or because a token could not be restored. It was safe to send a
moment earlier, and the failure does not change that; what stays behind is the hold-back
buffer, which may hold the token that failed.

On a cache miss the gateway asks the detector for every layer it has, so that request
costs what the [latency](#latency) section reports; a hit costs a lookup instead.
`detector_timeout_secs` defaults to 30 seconds because a tight timeout would turn
protection into a denial of service. Configuration is
TOML and rejects unknown keys — a typo in a security control should fail loudly rather
than leave a default in place.

### Audit

Every request appends to `audit_path`, one JSON object per line, and the gateway
does not start without it — a control that can be switched off by omitting a
line is worth nothing in a compliance report.

A request leaves two records. The first is written **and fsynced before the
provider is called**, so evidence that a request was pseudonymized cannot be
lost by the crash that follows it; if it cannot be written, the request is
refused with a 503 rather than served unrecorded. The second is written when the
request ends — including minutes later, when a stream does. The examples below
group the fields for reading; on disk each line serializes its keys in
alphabetical order, which nothing depends on.

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"masked","request":"7f3a9c1e04b25d68","provider":"anthropic","route":"/v1/messages","tenant":"a41f9c02…","session":"3bd7e105…","stream":true,"texts":4,"documents":2,"spans":9,"types":{"PERSON":2,"IBAN":1,"HEALTH":1},"redacted":0,"forwarded":0}
{"ts":"2026-08-11T09:14:37.902Z","event":"outcome","request":"7f3a9c1e04b25d68","tenant":"a41f9c02…","session":"3bd7e105…","upstream":true,"status":200,"result":"completed","error":null,"ms":15484}
```

A request refused before the provider is called leaves one line, and it answers
on its own both questions that matter — whether bytes left, and whose request it
was:

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"outcome","request":"91c4a70b6de83f12","tenant":"a41f9c02…","session":null,"upstream":false,"status":503,"result":"refused","error":"audit_write_failed","ms":12}
```

That is why `tenant` and `session` appear on the outcome line as well as the
masked one: a refusal has no masked line to join to, and the redundancy on the
two-line case costs a few bytes.

The record counts and never quotes. Neither the values, hashes of them, their
offsets nor the placeholder names are written. `error` is drawn from a fixed
vocabulary rather than formatted from a message, so no expression in the writer
could interpolate submitted text.

**What the detector was shown.** `texts` counts texts and `documents` counts
tool documents; each is one detection, whatever the number of leaves the
document holds. A document holding no leaves at all — `{}`, or one whose every
field is a boolean — is counted in neither, because nothing about it was
scanned. `texts + documents` is therefore exactly the number of detections this
request asked for — served by the detector, or from its cache when the same text
has already been seen under the same credential. Two identical messages are two
detections and one call, so these are not a count of the detector's traffic.

**What it found.** `types` counts distinct values per type, as the *detector*
named them, and `spans` counts occurrences; the gap between them is a value
found more than once under the same name within the same request, not anything
a session did — detection runs over every text on every request whether or not
one is attached.
A type name the detector reports that is not one of the twenty-two this gateway
declares is counted under `unvalidated` rather than written out, since the name
arrived from outside the perimeter and a name is a place a value can hide.
Seeing that key means the detector and the gateway disagree about what a type
is; the gateway also says so in its own log.

**What the provider received.** Every *occurrence* — one span, the unit `spans`
counts — is in exactly one of three states, and two of them are counted.
`redacted` counts the occurrences the provider received under a placeholder that
does **not** carry the detector's name for them: usually because the type was
not one this gateway declares, so the value went up as `[REDACTED_n]`, and also
when the value was already carrying a placeholder issued for another type,
whether earlier in this request or in an earlier turn of the session.
`forwarded` counts the occurrences the provider received verbatim: a span on a
numeric leaf of a tool document, which this gateway deliberately does not mask,
because a placeholder there would change the field from a number to a string.
`spans − redacted − forwarded` is what is left, and it went up under the name
`types` gives it. So a line whose `redacted` and `forwarded` are both zero says
the provider received `[PERSON_1]` for every `PERSON` it names, and it is the
only line that says so.

These two count occurrences and not findings, so they compare with `spans` and
not with `types` — a value masked three times is three of them. That is not
bookkeeping taste: one value can be **both** forwarded and masked in one
request, because the same number can be a `maximum` and appear in a
`description` beside it. Its fate is a pair of fates, so a per-value counter
would have to choose one and say the other did not happen; the earlier version
of this field did, and a line read `types: {"PERSON": 1}, forwarded: 1,
redacted: 0` for a request in which the provider received `[PERSON_1]` as well
as the digits. A span lands in exactly one leaf and a leaf is masked or
forwarded whole, so an occurrence has exactly one fate and the three numbers
account for the line.

Both counts describe *this* request. Two turns of a session carrying identical
traffic write identical lines, and what the session bought across them — the
same value keeping the same number — leaves no trace here.

**Whose fault the line says it was.** `error` names the failure, not the party,
and a 502 can mean either the provider or this gateway — so the classes that
mean *a defect here* are worth knowing by name: `shape_pointer` (a pointer this
gateway produced did not resolve in a body it had already walked),
`mapping_unknown_placeholder`, `mapping_bad_span`, `mapping_mask_mismatch` and
`mapping_placeholder_key`. `shape_response`, `upstream_failed` and
`mapping_lossy_document` (a document the provider sent that cannot be restored
without changing something else in it — a member dropped, a key renamed, a
number rounded — refused rather than served changed) are the provider's; `shape_request`, `shape_unsupported`, `tool_arguments_malformed`,
`mapping_too_deep`, `mapping_too_large`, `tool_too_large`,
`tool_too_many_calls`, `tool_numeric_personal_data`, `session_bad_id`,
`session_disabled` and `session_no_credential` are the caller's;
`detector_transport`, `detector_status`, `session_saturated` and
`audit_write_failed` are this deployment's own machinery rather than anybody's
mistake. A run of the first group is worth a page; a run of the second is worth
a look at the provider's status. Every class this gateway can write appears in
one of those four groups, and a test holds this paragraph to the code.

`result` is one of `completed`,
`refused`, `stream_failed` or `aborted`; the last is what an unsignalled record
defaults to on drop — in practice, a client that disconnects while a stream is
still open, recorded as itself rather than as a success nobody observed. On an
`aborted` line `status` is `0` and is not an HTTP code: no status was ever
observed for that request, and the field keeps the shape every other line has
rather than claiming an outcome nobody saw.

`tenant` and `session` are salted digests, never a key and never the raw session
id — a client may well name its session after the person in it. The salt lives
in `<audit_path>.salt`, created on first run with owner-only permissions, so one
credential keeps one identity across restarts. It is evidence too: back it up
with the journal.

Losing it stops the gateway rather than renumbering the journal underneath you.
A salt that exists but is not exactly 32 bytes refuses to start, and so does a
salt that is *missing* beside a journal that already has lines — a partial
restore or a rebuilt container that dropped only the salt would otherwise look
like a first run and start writing a `tenant` that disagrees with every line
above it, with nothing marking the boundary. The error names both remedies:
restore the salt, or move the journal aside to begin a new one.

Only the first of a request's two records is fsynced, so a crash can leave the
journal ending in a half-written second one. The next start appends the newline
that record never got, says so in its log, and serves — no operator, no flag. The
interrupted line stays exactly as short as the crash left it, because a record
that was cut off is itself a fact about that run; what the newline buys is that
every record written after the restart is a whole line of its own, rather than
being glued onto the fragment and lost with it.

A disk that fills in the middle of a record does the same damage without a
restart to repair it, so the running gateway repairs it too: the request whose
record was cut short is refused, and the next record written — once there is room
again — starts on its own line, with a line in the log saying why.

Two limits stay honest. A salt *replaced* by a different valid 32-byte salt
cannot be detected by anything — it is indistinguishable from the real one. And
rotation still works: retention and rotation are the operator's, done externally
around a restart by moving the journal aside and keeping the salt, which starts
normally and carries the same digests across. The file itself is opened for
appending only.

## Evaluation

The public synthetic corpus (FR/DE plus code-switching, checksum-valid synthetic
identifiers, seeded generation) lives in `evaluation/` and is reproducible by anyone:

```
make corpus     # regenerates evaluation/corpus/public.jsonl byte-identically
make evaluate   # per-type precision/recall/F1 + the Tier 1 recall gate (>= 0.99)
```

Current results on the public corpus, with the NER weights installed:

| Type | Precision | Recall | F1 |
|---|---|---|---|
| CH_AVS | 1.000 | 1.000 | 1.000 |
| CREDIT_CARD | 1.000 | 1.000 | 1.000 |
| DE_STEUERNUMMER | 1.000 | 1.000 | 1.000 |
| DE_STEUER_ID | 1.000 | 1.000 | 1.000 |
| EMAIL | 1.000 | 1.000 | 1.000 |
| FR_NIF | 1.000 | 1.000 | 1.000 |
| FR_NIR | 1.000 | 1.000 | 1.000 |
| IBAN | 1.000 | 1.000 | 1.000 |
| LOCATION | 0.667 | 1.000 | 0.800 |
| ORG | 0.154 | 0.333 | 0.211 |
| PERSON | 0.785 | 0.671 | 0.723 |

Article 9 special categories, detected by the same layer at a lower threshold:

| Type | Precision | Recall | F1 |
|---|---|---|---|
| BIOMETRIC | 1.000 | 1.000 | 1.000 |
| ETHNICITY | 1.000 | 1.000 | 1.000 |
| GENETIC | 1.000 | 0.750 | 0.857 |
| HEALTH | 0.667 | 1.000 | 0.800 |
| PHILOSOPHICAL_BELIEF | 0.000 | 0.000 | 0.000 |
| POLITICAL_AFFILIATION | 0.800 | 1.000 | 0.889 |
| POLITICAL_OPINION | 0.000 | 0.000 | 0.000 |
| RELIGION | 0.500 | 1.000 | 0.667 |
| SEXUAL_ORIENTATION | 1.000 | 1.000 | 1.000 |
| SEX_LIFE | 0.000 | 0.000 | 0.000 |
| TRADE_UNION | 0.296 | 1.000 | 0.457 |

**Article 9 coverage: 0.9783 (45/46)** — nearly every special-category mention in the
corpus is caught by at least one Article 9 label, in both languages. Article 9 is split
across eleven detector types rather than eight: the regulation protects political opinions,
a stated view that names no party needs a label separate from `political party`, and the
regulation names philosophical beliefs beside religious ones and sex life beside sexual
orientation — each clause gets its own label.

> Perfect scores on the catalog types mean the deterministic layer covers its own
> catalog, nothing more: those corpus entries are checksum-valid identifiers with clean
> formatting. The numbers become meaningful as the corpus grows adversarial cases —
> noisy formatting, near-misses, uncovered types.
>
> Article 9 is gated on **coverage**, not on per-category recall, and the ETHNICITY row
> shows why: the model reads "maghrébine" as religion rather than ethnicity. That span is
> still redacted, which is what REQ-3's "misses are not tolerable" is actually about — a
> special-category mention reaching the model provider unredacted. Which of the eight
> labels wins is second-order, so the report shows it and the gate does not. Their
> precision is left ungated on purpose: at threshold 0.30 the layer over-reports by
> design, because over-redaction is the safe failure for this category.
>
> The NER rows are flattered in one direction and penalised in another. Names, cities
> and companies sit in fixed template slots rather than in the shapes real text
> produces, which makes them easier to find than they would be in a real document; at
> the same time a model that correctly spots an entity the synthetic gold does not
> enumerate is scored as wrong. So `make evaluate` enforces the Tier 1 recall gate
> (≥ 0.99), the Article 9 coverage gate (≥ 0.95) and the LOCATION over-masking gate
> (≥ 0.8), while the strict per-type precisions are reported and warned about rather than
> enforced. PERSON precision is the honest weak spot and is not gated.
>
> REQ-38's 0.8 precision target is an irritation metric — below it, clients say the service
> ruins their text — so the binding gate measures **over-masking**: predictions that land on
> no personal data at all. LOCATION scores 1.000 there (12/12 predictions cover a real gold
> span) while its strict per-type precision reads 0.667, and the gap is entirely French
> surnames that are also place names — Lenoir, Fontaine, Mercier — where the model marks the
> very span the gold calls PERSON. That span is redacted either way; only the placeholder's
> type differs. ORG stays advisory because its over-masking is real rather than a labelling
> disagreement: "Le laboratoire" and "service juridique" match no gold entity at all.
>
> ORG precision fell from 0.950 to 0.154 when this corpus gained entity-free
> business prose: the model finds organizations in "die Apotheke" and "convention
> collective" that the gold does not enumerate. That is the negative examples doing their
> job — the earlier number was measured on a corpus with almost nothing to get wrong.
> The privately annotated corpus on real texts is the measure that counts, and it is
> reported separately.

## Latency

```
make bench      # per-layer p95 across three document sizes
```

Measured on an Apple M3 Pro (11 cores, CPU only), with the pinned fp32 ONNX weights, over
text concatenated deterministically from the public corpus:

| Size | Deterministic | Chunk + tokenize | NER tier 2 | NER tier 3 | Total (median) | Total (p95) |
|---|---|---|---|---|---|---|
| sentence (80 chars) | 0.0 ms | 0.0 ms | 42 ms | 66 ms | 109 ms | 116 ms |
| paragraph (1 200 chars, one chunk) | 0.6 ms | 0.3 ms | 467 ms | 491 ms | 950 ms | 1 108 ms |
| document (6 000 chars, several chunks) | 3.3 ms | 1.9 ms | 2 556 ms | 3 180 ms | 5 524 ms | 6 086 ms |

Per-layer figures are medians and account for the total: preprocessing is shared across the
inference passes and timed once, so the parts sum to within a few percent of the whole. The
p95 column is the number REQ-38 asks about; on a developer machine under load it carries
contention as well as detector behaviour — the document row has measured between 4 854 ms
and 9 008 ms p95 across runs while its median moved under 10%. Treat the medians as the
stable signal and p95 as an upper bound until these run on dedicated hardware.

> REQ-38 targets p95 under 80 ms without the LLM layer, and the detector does not meet it
> with the NER layer enabled — not by a margin that tuning closes. The deterministic layer
> is effectively free at every size; the entire budget goes to the model, and it costs
> roughly a second per 1 200 characters on this CPU. Measuring one-sentence documents alone
> would have reported 116 ms and hidden that, which is why the harness uses a size ladder.
>
> The split says two useful things. Chunking and tokenization are free — 1.9 ms on a 6 000
> character document — so the cost is inference and nothing else. And three quasi-identifier
> labels cost about as much as eleven Article 9 labels: the price is paid per inference pass,
> not per label, so adding categories to an existing tier is nearly free while adding a tier
> is not.
>
> Every route to the target measured so far costs something. Collapsing the two inference
> passes into one comes in faster but loses the Article 9 spans the split exists to protect.
> Running the passes on two threads gains only 16%, because onnxruntime already saturates
> the cores and the passes compete rather than overlap. The int8 graph is the most promising
> — roughly half the latency — but it halves every confidence score too (`Diabetes`
> 0.948 → 0.476, `IG Metall` 0.98 → 0.54), preserving ranking while invalidating every
> threshold calibrated against fp32, so adopting it means recalibrating and re-measuring the
> quality gates.
>
> The number is published here rather than gated in CI: timings on shared runners are noise,
> and a target that is not met should be visible rather than quietly enforced somewhere it
> never runs. CI runs the harness only to prove it still works.

## Status

Early development. The detector, the gateway — sessions, streaming, the audit
journal, tool traffic on the buffered path — and the two-container stack above
all work end to end. Not ready for
production use: the gateway authenticates no caller, and nothing here has been
run in anger.

## License

[Apache-2.0](LICENSE). Contributions are accepted under the
[Developer Certificate of Origin](CONTRIBUTING.md).
