# The audit journal

An append-only record of what was masked, for a reader who has the journal and nothing else.

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
`session_disabled`, `session_no_credential`, `caller_not_served` and
`mapping_literal_already_issued` are the caller's. `caller_not_served` means the
credential is not one this deployment accepts, and is only ever written where an
operator configured `accepted_credentials`; a run of them is either a client
with the wrong key or somebody trying keys, and the `tenant` digest on the line
is what tells those apart. `mapping_literal_already_issued` means the request
wrote a placeholder literally that an earlier turn of the same session had
already issued to a value, which this gateway cannot tell apart from its own
token by shape, so it refuses rather than substituting that value into text the
caller wrote. The line names neither the token nor the value — a client should
never see a placeholder — though that is not what denies the caller the
knowledge, since they chose the token: an issued one refuses where an unissued
one is served, so a run of these from one tenant and session is somebody reading
that one bit;
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
