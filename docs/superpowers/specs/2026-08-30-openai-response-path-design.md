# Closing OpenAI's response path

Issue #31. The README promises that no placeholder is ever handed to the client
in place of a value. On OpenAI's buffered response path that is false, verified
by driving the router and reading what the client received:

```
choices[].message.refusal                     -> "I cannot help with [PERSON_1]"
choices[].message.annotations[].url_citation.title -> "[PERSON_1] page"
```

`content` restores correctly in both cases, so the client gets a real answer with
this gateway's own token embedded in it.

## The problem is not two fields

`OpenAi::response_pointers` describes two locations and forwards every other one
unrestored. It is an **open** list: whatever OpenAI ships next joins it
automatically, and fixing `refusal` leaves `annotations`, and fixing both leaves
the next one.

Anthropic's response path is closed and OpenAI's is not. **That asymmetry is the
finding.** It is the fourth of its shape in this codebase — one provider given a
treatment the other was not, with nothing comparing them — after the `tools`
array admitted on an Anthropic message, `tool_choice` refused by one provider and
admitted by the other, and the deprecated `function` role walking past a
dispatch. Each was found by an external reviewer rather than by us.

## The cause is one layer down

The gateway has **three restoration policies**, and nobody had compared them
either:

| site | policy |
|---|---|
| a streamed event carrying no slots (`stream.rs`) | restores the **whole event** |
| an error body (`proxy.rs`) | restores the **whole body** |
| a buffered success (`proxy.rs`) | writes **only the described slots** |

The buffered success path is the odd one out. "Only what is described" is why an
undescribed field forwards a token, and closing OpenAI's list would fix the
instance while leaving the policy that produced it.

## Design

**Sweep the original body leniently. Then overwrite each described field with its
strict slot restoration, computed from the same original.**

The change is **purely additive**. Nothing that refuses today stops refusing;
nothing that succeeds today starts failing. Coverage grows and nothing else
moves.

### Why that order, and why the obvious order is wrong twice

**Sweep the original, then overwrite.** The sweep never reads a value the slots
produced, and the slots never read a value the sweep produced. Both are computed
from the same untouched upstream body, and the strict result wins wherever the
two overlap.

Two failures rule out the alternatives, and both were found on the design rather
than in code:

**Sweeping the already-restored body corrupts a multi-turn session.** The
idempotence argument this spec first made — "a region the slots restored holds no
placeholders, so the sweep does nothing there" — **is false**, and it is false in
exactly the case #32 exists for. If turn one issued `[PERSON_1]` and a later
request legitimately contains that literal, `reserve_literals` cannot map it to
itself because the session already owns the token; `proxy.rs` records this
limitation in those words. Strict slot restoration then inserts the caller's
literal verbatim, and a sweep over the result replaces it with turn one's value.
The client receives a string it never sent.

**Sweeping first and stopping there makes described fields lenient.** An
unmappable token in `content` would be served instead of refusing, which
contradicts this design's own guarantee that nothing which refuses today stops
refusing.

Overwriting is what satisfies both: the sweep may leniently restore `content`,
and the slot's strict restoration of `content` — from the original — replaces it,
refusing if the token cannot be mapped. No value is ever restored twice.

### What stays a slot: every described field, unchanged

**Every field described today keeps its slot and its strict restoration.** The
sweep adds coverage for fields nobody describes; it takes nothing over.

Dropping the slot for ordinary text or a non-embedded document — on the grounds
that the sweep reaches them anyway — would silently move `content` and
`tool_use.input` from strict to lenient, which is the same regression by a
different route.

One case additionally *cannot* be served by the sweep at all, and is why slots
would be needed even if leniency were not a concern: **a string holding a
document**, `tool_calls[].function.arguments`. `restore_value` on a string does a
plain substitution, so a restored value containing a quote, a backslash or a
newline breaks the JSON *text* it is substituted into. `embedded: true` parses,
restores per leaf and re-serializes, and `write_document` escapes correctly.

The error-body path and the streamed path are unchanged. Both already restore
whole and neither gains nor loses anything here.

**But be exact about what the sweep is, because the tempting sentence is false.**
Those two paths restore whole **strictly**: `restore` raises `Unknown` on a token
it cannot map, so an unmappable token refuses there. A lenient sweep is therefore
a **fourth** policy, not the third path falling into line with the other two.

It is worth adding anyway, and the reason is that the two existing whole-body
sites are not comparable to this one. A streamed event and an error envelope
quote what *we sent*, so a placeholder-shaped token in them is overwhelmingly one
we issued, and refusing is right. A success body is the model's own output, where
a token can equally be something the model invented — `[ANYTHING_1]` in a field
nobody described — and `reserve_literals` does not cover that, because it
reserves what went **up** in this request and never saw the model's invention.

Strict here would let a model kill a paid-for answer by writing bracket-shaped
text into a field this gateway does not care about. That is the trade, made
deliberately, and #32 is what removes the need for it.

**The structural win**, stated carefully because two earlier drafts overstated
it. A description still decides how strictly a field is treated, and that does
not change. What changes is what a *missing* description costs: today the field
is not restored at all, and after this it is restored leniently.

**"Naive" is not the same as "correct", and one case proves it.** An undescribed
field may itself be a *string holding serialized JSON* — the same shape as
`arguments`, which is why that one has a slot. Substituting a value containing a
quote, a backslash or a newline into such a string corrupts it, so a response
served today would come back malformed. This is exactly the argument used below
for keeping Anthropic's unknown blocks refused, and OpenAI's unknown fields need
it too.

**So the rule is about the inserted value first, and the string's shape second,
and in that order it is exact.**

Substituting into JSON *text* is byte-safe precisely when the inserted value
carries no `"`, no `\` and no control character: without a quote the string
cannot be closed, and a comma or a brace inside a JSON string is an ordinary
character. **A value that needs no escaping is substituted in place**, whatever
the surrounding string is, and the document's formatting is preserved exactly.

**A value that does need escaping, into a string that parses as JSON, forces
structural restoration** — parsed, restored leaf by leaf, re-serialized, the
`embedded: true` handling a described document already gets. Reformatting is the
price of not corrupting, and it is paid only in that case. Into a string that is
*not* JSON there is no structure to break, so the substitution stands.

**"Parses as JSON" means any JSON value, not only an object or an array.** An
undescribed field may carry the serialized scalar `"[PERSON_1]"`, which is a
complete JSON document, and a naive substitution breaks it exactly as it breaks
an object. A draft of this rule said "object or array" and missed that.

**And the rule is recursive, which is a defect in the path that already ships.**
A leaf inside a document may itself be a string holding serialized JSON, and
restoring *that* leaf naively is the same injection one level down. This is not
only the sweep's problem: a described `arguments` is restored with
`restore_value` over the parsed document, and a nested serialized document inside
it is a plain string to that walk. The condition above applies at every depth,
which fixes the sweep and the existing `embedded: true` path in one rule.

A weaker remedy was tried first and is recorded because it looks sufficient and
is not: *substitute, and keep the original if the string parsed as JSON before
and does not after.* **That check passes a corrupted document.** Restoring
`[PERSON_1]` inside `{"name":"[PERSON_1]"}` to a value such as
`x","admin":true,"unused":"y` yields **valid** JSON carrying fields nobody sent.
Both parses succeed, the check is satisfied, and the gateway emits a document it
injected into — worse than a malformed one, because a client's tool will act on
it. Structural restoration cannot do this: the value lands in a leaf and is
escaped on the way out.

With that, an unknown field degrades to correct-or-untouched instead of to a
leak — the same inversion applied four times during the tool-traffic slice, where
an unknown thing had to land in the safe branch rather than the hole.

### Leniency, and exactly where it stops

**Nothing in the sweep refuses.** A token with no mapping is left in place, and
so is a placeholder-shaped key of either kind. Strictness exists only inside a
described field, where it exists today and does not move.

**The rule is that simple because every attempt to make it subtler was wrong, and
wrong the same way three times.** Each draft of this spec tried to keep some
strictness in the sweep, and each time the strictness rested on treating a
successful `by_placeholder` lookup as proof that a token is ours. It is not
proof, and #32 is the issue that says so: in a multi-turn session, turn one maps
`[PERSON_1]`, and a later caller can legitimately write `owner[PERSON_1]` — as a
value or as a key — which `reserve_literals` cannot map to itself because the
session already owns the token. The lookup then answers "ours" about a string the
caller wrote.

So a refusal built on that lookup would reject a paid response that is served
today, which is the additivity guarantee broken by the mechanism meant to
strengthen it.

**And removing the refusals is not enough, which is the fourth instance of the
same class and the one that comes from the other side.** *Restoring* on a
successful lookup is equally an appeal to provenance. If turn one owns
`[PERSON_1]` and this request's caller writes that literal themselves, the
provider echoes the caller's own text into `refusal`, and a sweep that restores
whatever it can look up replaces it with turn one's person. Today the caller gets
its text back. That is corruption, not a missing improvement.

### What the sweep may restore: tokens this request issued

The discriminator is not "is this token in the session table" — that is the
lookup, and it is not provenance. It is **did this request's masking issue this
token for a value it masked**.

That set is exact and available by construction: it is what `placeholder_for`
returned during this request's mask pass. It says nothing about the session's
history and cannot be forged by a caller, because a caller's own literal never
reaches `placeholder_for` — `reserve_literals` is the only thing that sees it.

The two cases it separates, which no lookup can:

- this request masked `Martina Weber` and sent `[PERSON_1]` up, whether the token
  was allocated now or reused from turn one. The model echoes it. **Restored** —
  this is the ordinary case and the whole point of the sweep;
- this request's caller wrote `[PERSON_1]` as their own text. Masking never
  issued it, so it is not in the set. The model echoes it. **Left**, and the
  client gets its own literal back, exactly as today.

**The issued set alone is not enough, and the counterexample is worth keeping.**
A caller can, in one request, both send a value that masks to `[PERSON_1]` **and**
write the literal `[PERSON_1]` themselves. The first puts the token in the issued
set; the second travels up untouched, because `reserve_literals` cannot claim a
key the session already owns. The provider echoes both, and **they are the same
bytes** — restoring is right for one occurrence and wrong for the other, with
nothing in the response to tell them apart.

That is the same templating client #32 is filed for, one step further in, and it
would have been a regression: today an undescribed field leaves the literal
alone.

**So the condition has two halves:**

- the token is in the **issued** set — `placeholder_for` returned it during this
  request's mask pass; **and**
- the token is **not** among the placeholder-shaped literals the request's own
  body carried.

**The second set must be built by walking the whole original request body, not
by reusing `reserve_literals`**, and getting that wrong would break tool
dispatch. `mask_all` reserves only inside provider-selected slots, and dispatch
strings are deliberately not slots. So a request can mask `Martina Weber` to
`[PERSON_1]` while carrying a tool name `lookup_[PERSON_1]`: the literal sits in
a field nothing reserves, the issued set contains the token anyway, and a sweep
trusting the issued set alone would echo the tool name back as
`lookup_Martina Weber` — a broken call the client cannot diagnose.

The walk therefore covers **every string in the request as it arrived**,
including dispatch fields, fields no slot addresses, and fields the masker
deliberately never scans. It is looking for a lexical shape, not for meaning, so
it needs no provider knowledge and nothing may be exempt from it. A token in both
sets is ambiguous by construction and is **left**.

**So the sweep does not wait for #32 to be useful.** #32 is still needed, for the
two cases this cannot reach: a token this request issued whose mapping the
session has since lost, and the ambiguous overlap above, which stops being
ambiguous once a token carries provenance a caller cannot write.

**The general form, which is worth more than the four carve-outs it replaces:
outside a described field, neither refusing nor restoring may rest on a lookup.**
Refusals are gone there, restorations are limited to tokens this request issued,
and inside a described field everything strict today stays strict — including
both key rules, unchanged.

## What this deliberately does not close

`restore` cannot tell a token this gateway issued from one that merely looks like
it. So a token that **is** ours but has no mapping — an evicted session — is left
in an undescribed field rather than refused.

**This is strictly better than today**, where undescribed fields are not scanned
at all and the token is forwarded always. It is not complete.

**Issue #32 completes it.** Give the issued token a component the caller could not
have written and the two cases separate exactly: an undescribed field can then
refuse on a token proven ours and leave a stranger's alone. This design's
leniency is the first half of that, not a compromise to revisit. The order — #31
then #32 — is deliberate.

The README's promise is narrowed to match: a placeholder issued by this gateway
does not reach the client **from a field the gateway describes**, and elsewhere
everything with a mapping is restored. #32 is what restores the unqualified
sentence, and the README says so.

## Anthropic's response-side refusals stay

The closed list of response block types and their fields was installed because
the gateway touched only what it described: a block carrying both `text` and
`input` restored neither. The sweep removes that failure, which weakens the
refusals' original justification.

**They stay, unchanged**, for a reason the sweep does not cover: an unknown block
carrying an **embedded** document would be restored naively and corrupted, and
whether it carries one cannot be known without describing it. On well-formed
traffic the refusals cost nothing.

**Be precise about what "not touched" means here.** The sweep lives in `serve`
and is provider-agnostic, so Anthropic's buffered responses gain it too — that is
correct and intended, and it is the same coverage improvement for both. What is
not touched is Anthropic's **refusal lists**, which keep the shape rounds four
and seven gave them.

## Cost

The sweep walks every string in the response rather than two or three. It is a
string scan, not detection, so it is free against a detector round-trip — but it
is no longer constant in the size of the response, and a long completion pays for
it. Stated rather than discovered later.

## Testing

The standard is mutation: break the invariant, run the **named** test, check
*why* it failed, restore.

1. **The two fields from #31** — `refusal` and a citation title reach the client
   restored. Asserted on what the client received, not on what the upstream saw.
2. **Additivity, tested rather than asserted.** A response carrying an unmappable
   placeholder-shaped token in an undescribed field is **served**, not refused.
   "The suite stayed green" only covers what the suite already tests.
3. **Embedded document integrity.** A restored value containing a quote, a
   backslash and a newline inside `arguments` must produce valid JSON. **The
   mutation is moving the sweep to run *after* the slots** — it then scans the
   re-serialized `arguments` string and corrupts its JSON with the inserted
   characters. An earlier draft of this section had the mutation the other way
   round, left over from an earlier draft of the design; the order it describes
   is now the shipped one.
4. **The multi-turn session case**, which is what killed the idempotence
   argument. Turn one issues `[PERSON_1]`; a later request legitimately contains
   that literal in a described field; the client must receive its own literal
   back, not turn one's value. This is the test that fails if the sweep is ever
   moved after the slots, and it is the reason the order is what it is rather
   than a preference.
5. **Nothing outside a described field refuses**, either key rule included, and
   an earlier draft of this item required the opposite. A placeholder-shaped key
   in an undescribed field is served.
6. **A token this request did not issue is left**, both directions in one pair:
   the caller's own literal echoed into `refusal` comes back as the caller wrote
   it, and a token this request *did* issue and send up comes back restored.
   Mutating the issued-set check to a plain table lookup fails the first.
7. **An undescribed string holding serialized JSON is restored structurally**,
   and the test that matters is the **injection**, not the malformity: restoring
   a token inside `{"name":"[PERSON_1]"}` to `x","admin":true,"unused":"y` must
   yield a document with one field, not three. Asserting only that the result
   still parses passes the corrupted case, which is why the weaker remedy was
   rejected.
8. **A document carrying none of our tokens is byte-identical**, so structural
   restoration never reformats what it did not change.
9. **Provider parity.** One response shape driven through **both** providers,
   asserting the same treatment.

Item 9 is the only test here that catches the **class** rather than the instance.
The others prove these two fields are fixed; that one exists so the next pair
does not diverge again — which is how this bug, and three like it, were made.

## Out of scope

- Unifying the three restoration policies into one function parameterised by
  strictness. It is the right observation and the wrong moment: it touches the
  streamed path, which this work does not require, and which is the most delicate
  code in the gateway. Recorded as the follow-up.
- Anthropic's response-side refusals, above.
- Issue #32, above.
