# Sessions

One placeholder per value for the length of a conversation, and what bounds that table.

Within a request an identical value always gets the same placeholder. Across the turns of
a conversation it does too, if the client sends `X-Tessera-Session: <id>` — otherwise each
request gets its own table, which is the behaviour without the header.

The id does not select a session on its own. A session table is a restoration oracle: put
`[PERSON_1]` in a prompt, get it echoed by the model, and the gateway would restore it to
a real name on the way back. So the store keys on a salted fingerprint of the caller's own
credential as well as the id, and a guessed id lands in an empty namespace. The boundary
is the credential, not the id: callers who share one API key share one namespace, and
within it any id is reachable by anyone holding that key.

**Inside a namespace the probe no longer restores — it refuses.** A literal that the
session has already issued is a 400 (`mapping_literal_already_issued`) rather than a value
substituted into text the caller wrote. That closes the read and leaves a thinner oracle in
its place: an issued token refuses where an unissued one is served, so one literal per
request still tells which numbers a session has allocated. That needs the id and the
credential **bytes** rather than a credential the provider still honours — the store keys on
whatever arrives and the refusal happens before the upstream is called, so a revoked key
keeps reading that one bit until the session expires.
Closing that needs an issued token the caller cannot predict, which is #32. The raw id
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
property names carrying them, which cost it nothing.

**Nor the ones the detection cache will answer.** Both bounds are on what the caller waits
for, and a text already detected under the same credential costs no wait — so it is not
charged. That matters most for the traffic these bounds were sized against: the ten pinned
Claude Code tool definitions are byte-identical on every turn, so charging them spent
9 193 of the 20 000 characters and 20 of the 40 calls **permanently**, leaving about
10 800 characters for a new tool result for the life of the session. That, rather than a low
ceiling, is the real mechanism behind "the bounds admit roughly twenty tools".

So what a caller is promised is **this many characters of text the gateway has not seen**,
and a request can be admitted after a turn that warmed the cache and refused after an
eviction. The direction is safe — a refusal is what that request already got — but the
guarantee is the narrow number, not the wide one a warm cache happens to allow.

Both defaults are twice a measurement
taken on ten real Claude Code tool definitions, which charge 9 193 characters across 20
calls on the turn that first sees them. That payload is a **floor** and is stated as one: a
stock session also carries tools the measurement did not, and an MCP server adds more, so a
large enough tool payload is refused rather than served slowly. Issue #28 — making detection fast on a large text — is
the work that lifts the ceiling; until it lands, the honest answer to a payload past these
numbers is a refusal, not a wait long enough that the client's own HTTP timeout would cut
it off anyway.

A request refused before the upstream call leaves its session exactly as it was. Asking for a
session the gateway cannot honour — a malformed id, no credential to namespace it, or
`session_idle_secs = 0` — is refused before the detector runs rather than served without
the coreference it asked for.
