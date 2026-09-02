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

**The change was designed to be purely additive, and it is not.** That was the
goal — coverage grows, nothing else moves — and it held for the sweep, which
never refuses. It stopped holding for described fields once the loss handling
below arrived: a `content` or an `arguments` whose round trip cannot reproduce
what the upstream sent now raises `MappingError::Unrestorable` and returns 502,
under the class `mapping_lossy_document`, where plain substitution used to serve
it. Serving it was never correct — those are the responses that were being
corrupted silently — but it is a behaviour change and calling it additive would
license a reader to require the old outcome.

What survives of the goal is narrower and worth keeping: **nothing that refuses
today stops refusing**, and every new refusal is a response that was previously
served altered. The costs are enumerated with the loss handling and priced
again in `README.md`.

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
document**, `tool_calls[].function.arguments`. `embedded: true` parses,
restores per leaf and re-serializes, and `write_document` escapes correctly.

**This paragraph used to say `restore_value` on a string "does a plain
substitution", and that was true of the design and false of the code by the time
the recursion fix landed.** Its string arm is `restore_in_string_strictly`, which
carries the whole escaping rule — inert values substituted, non-inert ones
restored structurally or refused. Implementing the old sentence would put the
quote and single-quote injections straight back, which is why it is replaced
here rather than footnoted: a reader implements the definition, not the note
under it.

The streamed path is unchanged. **The error-body path is not, and saying it was
hid a real compatibility change.** It restores whole and always did, but the arm
it restores *with* moved underneath it: `restore_value`'s string leaves take
`restore_in_string_strictly` now, and the non-JSON error body takes it too. So a
provider error carrying a document-shaped string can be re-serialized, or
refused with a 502 in place of the provider's own status, where it used to be
substituted into. That path needs acceptance coverage of its own, and has it —
`an_error_body_this_reader_rejects_is_not_injected_into` is the case that found
it.

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
`arguments`, which is why that one has a slot. Substituting a value that is not
wholly inert into such a string corrupts it — a quote of either kind, a
backtick, a backslash, a control character or a slash — so a response served
today would come back malformed. This is exactly the argument used below
for keeping Anthropic's unknown blocks refused, and OpenAI's unknown fields need
it too.

**So the rule is about the inserted value first, and the string's shape
second.**

Substituting into text is byte-safe when **every character of the inserted
value is inert** — when none of them can end the string it lands in, start an
escape inside it, or be a character the format forbids there raw. A comma or a
brace inside a string is an ordinary character and cannot reach the structure
around it, which is why both are inert; a quote of either kind, a backtick, a
backslash, a control character and a slash are not. **A value that is wholly
inert is substituted in place**, whatever the surrounding string is, and the
document's formatting is preserved exactly.

This is stated as a property and tested as an allowlist, `json_string_inert`,
because the failure directions are not symmetric: a character wrongly called
inert is an injection, and one wrongly called dangerous is a restoration lost.
**An earlier version of this paragraph defined the rule as the absence of `"`,
`\` and control characters** — a blocklist — and a value of
`x','admin':true,'pad':'y` walked past it and turned `{'name':'[PERSON_1]'}`
into a JSON5 object carrying a member the upstream never sent.

**A value that is not wholly inert, into a string that parses as JSON, forces
structural restoration** — parsed, restored leaf by leaf, re-serialized, the
`embedded: true` handling a described document already gets. Reformatting is the
price of not corrupting, and it is paid only in that case.

**"Into a string that is not JSON there is no structure to break" was the next
sentence here, and it is the mistake this design was most wrong about.** Not
JSON *to this reader* is not the same as not JSON, and the branch where those
two differ is exactly where an unescaped substitution is dangerous: a byte order
mark, a comment, a single-quoted key or a trailing comma each produce a text
`serde_json` refuses and a client's parser accepts. Nine readings of that
question are recorded below; what stands is `structure_encloses_a_token`, which
asks whether a `{` or `[` was opened before the token and nothing else.



**"Parses as JSON" means any JSON value, not only an object or an array.** An
undescribed field may carry the serialized scalar `"[PERSON_1]"`, which is a
complete JSON document, and a naive substitution breaks it exactly as it breaks
an object. A draft of this rule said "object or array" and missed that.

**A restored key that collides keeps its own object.** A nested document can
carry both the key `"[PERSON_1]"` and the key that token restores to.
Structural restoration builds a map, which cannot hold both, so one would be
**silently discarded** — a tool argument lost, where today's textual substitution
serves both. Neither restoring nor substituting is safe there: substitution
yields duplicate property names, whose meaning is ambiguous.

So on a collision *that map* is handed back exactly as it came — every field
present, every key unrestored — and nothing around it is touched. The ambiguity
is a fact about one map's key set, and the fields beside it are not implicated
by it. That loses restoration for one object, refuses nothing, and cannot lose a
field. It is the same rule the sweep follows everywhere else: what cannot be
restored safely is left, not guessed at.

**An earlier draft of this section specified the whole string, and that was the
defect rather than a simplification.** Leaving the string means reporting the
collision upward, which is what returning `None` from the object arm does; the
`?`s at both propagation points then carried it to the top, and one ambiguous
object became an off switch for the entire body — a `meta` object with a
colliding key left `refusal` holding the very placeholder #31 is about.
`restore_document` returns `Value` rather than `Option<Value>` exactly so that
fallback has nowhere to travel. Anyone reading the old sentence to understand
the design would build #31 back, which is why the correction is recorded here
rather than silently applied.
`a_collision_costs_its_own_object_and_nothing_beside_it` and
`a_colliding_key_costs_its_own_object_and_nothing_around_it` are what hold the
fallback to its size.

The string as a whole comes back verbatim in one narrow case that is not the
fallback: when the restored document compares equal to the parsed one, every
restorable token in it having happened to sit inside a colliding object. Nothing
changed, so the caller gets its own bytes rather than a re-serialized equivalent
of them. That is byte preservation, not a scope.

**A document that already carried two members of the same name is left whole,
and here the unit really is the string.** The parse that opens the structural
path collapses those two before any restoring happens, so re-serializing emits a
document the provider did not send — and one that two readers disagree about,
since `serde_json` keeps the last member and a reader keeping the first sees a
different document. No later step can put the lost member back, and by the time
an object could be named the member is already gone, so there is nothing smaller
than the string to hand back. It is the same rule again: what cannot be
re-serialized faithfully is left.

The test is `carries_duplicate_members`, a second pass over the same bytes with
a visitor that sees member names one at a time. It runs *behind* the escaping
test, not in front of it, so a document is only ever left for a loss it was
about to take: a value needing no escaping still substitutes into such a
document textually and keeps both members, because nothing is re-serialized on
that path. Hoisting the check answers `true` for every plain string too and
turns the sweep off body-wide — the same defect as the paragraph above, reached
by a different route.

**And a duplicate member is not the only thing the round trip drops, which this
section said it was.** The sentence "the parse is lossy on exactly one input"
was written from what had been noticed, and two more were found by looking:
a number is held as an `i64`, a `u64` or an `f64`, so a lexeme carrying more
precision than a double — or an integer past 64 bits — comes back rounded, in a
document a client executes and beside the name the pass was restoring; and the
parse can *refuse* a text a client accepts, a document past `serde_json`'s
recursion limit or an exponent it calls out of range, which the code read as
"not a document" and answered with the unescaped substitution the whole branch
exists not to emit.

So the claim is now made from what a JSON text is made of, which is closed,
rather than from what has been noticed, which is not. Structure and literals
have one spelling each. A string comes back with whatever escapes `serde_json`
prefers, which is a different spelling of the same string and is what the
client's own parse undoes. Objects lose a member when two share a name.
Numbers lose their lexeme. Key order and whitespace are the reformatting the
section above already prices. That is every token class in the grammar, and
`carries_duplicate_members` and `carries_an_unstable_number` are the two tests
it leaves. The third guard answers the refusal case, which is not a loss but a
reader disagreeing with a reader — and it took nine attempts to write, which
the section below records because the shape of the failure is the lesson.

**The third guard was written nine times, and every version but the last made
a claim it had no standing to make.** It decides whether an unescaped
substitution may go into a text this reader refuses and another may accept, so
every version that described *what the other reader accepts* was guessing in
the one place guessing is unaffordable. In order, the reading was: the first
byte; the first byte after whitespace; the first thing that is neither
whitespace nor one of our tokens; whether a container had ever been opened,
gated on a list of what may follow a `[`; that list inverted; a bracket count.
They were defeated, in order, by a byte order mark, a Markdown checkbox, a
comment, a JSON5 single quote, `NaN`, and a `]` inside a string.

What ended it was asking only about bytes we hold: **was a `{` or `[` opened
before the token**. Closers are not counted, because telling a structural
closer from one inside a string means knowing which delimiters open a string in
the reader we do not have. Nothing reads a dialect, so there is no list left to
be found wanting — and the cost, Markdown prose that opens a bracket before the
placeholder, is paid openly and fixed in a test.

**A tenth finding then landed above all of it**, on the escaping test that
decides whether any of this runs. It was a blocklist — `"`, `\`, control
characters — so a value of `x','admin':true,'pad':'y` counted as safe and
`{'name':'[PERSON_1]'}` came back as a JSON5 object carrying a member the
upstream never sent. It is an allowlist now, `json_string_inert`, measured
against the values this gateway actually masks; `holds_no_string` went with it,
being the same claim one level up. The rule the whole sequence produces:
**a predicate whose omissions cause injections must be inverted into one whose
omissions cost restorations.**

**The guards went on the wrong frame first, and that is worth recording because
the mistake reads as coverage.** They were put in `restore_in_string_with`,
which sees the strings *inside* a parsed document — so a document nested one
level down inside `arguments` was covered while `arguments` itself was not.
`read_document` and `write_document` are a `parse → Value → to_string` of their
own, one frame above, and that pair is how a described `arguments` is actually
handled. Measured through it: an `amount` of
`0.12345678901234567890123456789` came back `0.12345678901234568` — the
finding's own example, on the finding's own field, with the guard already
written and passing its test. The test passed because it handed `restore_value`
a `Value` whose `arguments` was a string leaf, which is not how production
routes it. A guard proved on a path production does not take is worse than no
guard, because the next reader stops looking.

Two things follow, and both are in the code now. The guard is asked at
`write_document`'s call site as well, through one shared `round_trip_loses` so
that a third cause reaches both callers. And the pair no longer re-serializes
when there is nothing to put back: most `arguments` carry no placeholder, their
round trip was silently rounding numbers before any of this, and a guard without
that skip would have turned the silent rounding into a refusal bought for
nothing.

**"Nothing to put back" is asked of the wrong thing if it is asked of the
parsed document, and that cost a P1.** The skip compared the restored `Value`
against the read one — and a `Value` is what the parse has already lost
something to. In `{"name":"[PERSON_1]","name":"fixed"}` the parse keeps the last
member, so the only placeholder is absent from *both* sides of that comparison,
the skip concluded there was nothing to restore, and the upstream's own bytes
went to the client with the token still in them. The sweep had left the same
string alone for the same duplicate, so nothing downstream was going to catch
it.

So the skip is taken on the **text's** evidence and needs two conditions to be
declined: a round trip that loses something *and* a placeholder-shaped token in
the bytes. Either alone is not enough — a lossy document with nothing of ours in
it still goes back byte for byte, which is the ordinary case the skip exists
for. The same early return one frame down, in `restore_in_string_with`, had the
identical hole and is fixed the same way; only the described door refuses, the
lenient one keeps the bytes as the duplicate rule says it should.

The general form is the one this design keeps running into: **a loss cannot be
detected with the tool that caused it.**

**The frame above *that* is the whole response body, and it stays open.**
`proxy::handle` reads the body with `from_slice::<Value>` and writes it back
with `Json(restored)`, so every number in every buffered response round-trips —
measured on a body with no placeholder in it at all, `usage.total` of
`123456789012345678901234567890` came back `1.2345678901234568e+29`. That is
pre-existing, provider-wide and older than this slice. Closing it means either
refusing any response carrying a number this reader cannot reproduce — a refusal
on ordinary traffic that no finding asks for — or not representing a response
body as a `Value`, which is a different slice with its own additivity argument.
The prose that claimed "nothing is re-serialized until the round trip is known
to reproduce what it read" was true of one function and false one frame up; it
now says which.

**`serde_json`'s `arbitrary_precision` was considered and declined.** It would
make numbers lossless at the source rather than detected after the fact, which
is the better shape. It is a crate-wide feature that changes how every `Value`
in this gateway holds a number, on the masking path as well as this one; it
routes numbers through `visit_map`, which is the deserializer surface
`DuplicateScan` is built on and would have to be re-argued; and it closes only
one of the three causes, leaving the refusal case and the duplicate case exactly
where they are. A byte scan over text that has already parsed is a smaller
claim, and it is one this slice can prove.

**The recursion fixes escaping and does not extend the key rule.** Parsing a
nested serialized document promotes strings into *key* positions that were plain
text a moment earlier, and applying the strict key rule to them would refuse a
response that succeeds today: a described `arguments` carrying the string
`{"[PERSON_1]":"ok"}` is text to `restore_value` now, is substituted, and is
served. Additivity decides it. At that depth the key is restored exactly as
today's substitution restores it, because that *is* today's behaviour; the key
rule keeps the depth it already has and gains no new one. Anything else trades a
corruption fix for a refusal regression, which is the swap this design exists to
avoid.

**And the rule is recursive, which is a defect in the path that already ships.**
A leaf inside a document may itself be a string holding serialized JSON, and
restoring *that* leaf naively is the same injection one level down. This is not
only the sweep's problem: a described `arguments` is restored with
`restore_value` over the parsed document, and a nested serialized document inside
it is a plain string to that walk. The condition above applies at every depth.

**"In one rule" is what this said, and one rule was not enough to reach both
paths.** The sentence claimed the recursion fixed the sweep and the existing
`embedded: true` path together; as built it fixed only the sweep. The sweep and
the slot loop both run, the loop overwrites the sweep's answer wherever the two
address the same bytes, and the loop's `restore_value` kept a string arm calling
plain `restore` — so a described `arguments` had its safe result computed and
then replaced by the injecting one. The claim was not a description of the code
at any point.

What reaches both paths is one rule and *two* policies, which is what is now
built. The escaping condition and the recursion under it are facts about JSON,
so they live in one function that both paths enter (`restore_in_string_with`,
over `restore_document_with`). The token policy is not a fact about JSON and does
not cross: the sweep stays lenient and provenance-gated, and `restore_value`
stays strict and still raises `Unknown` on a token it cannot map. A
`Restoration` trait carries the difference; `Lenient`'s error type is
`Infallible`, so the shared code compiles to a sweep with no way to fail. The
text slots take the same door as `restore_value` for the same reason — the loop
overwrites the sweep there too, and `content` under `response_format:
json_object` is a document the client parses.

**The two documents that cannot be re-serialized faithfully part company there,
and that is the one behaviour this adds rather than fixes.** A restored key
colliding with one already present, and a document the parse already collapsed
two members of: the sweep leaves the bytes, which costs restoration and corrupts
nothing. A described field cannot take that answer, because those bytes still
hold the placeholder and a placeholder reaching a client from a field it
dispatches on is what the strict path exists to prevent — and substituting
anyway is the corruption it also exists to prevent. So it refuses, with
`MappingError::Unrestorable`, a 502, and the journal class
`mapping_lossy_document`.

**This was written as reachable only once a value needing escaping is already
going in, and that is no longer the bound.** It holds for the guard inside
`restore_in_string_with`, which only runs when something is not inert. It does
not hold one frame up: the `read_document`/`write_document` pair round-trips a
described `arguments` whenever the restoration changed the document at all, so a
wholly inert value into a document carrying duplicate members or an unspellable
number refuses too. The test for it uses a value with no quote in it deliberately
— `an_arguments_document_the_round_trip_would_change_refuses_the_response`. An
implementation that reads the old bound omits the outer guard, which is the frame
the original finding was actually about.

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
everything this request issued and the caller did not write is restored — **except
where restoring it would change something else the provider sent**, which is
served as it came. That exception is one rule with several shapes: an object
whose restored keys would collide, and a string that is itself a serialized
document the round trip cannot reproduce. #32 restores the unqualified first
half; the second clause is the cost of never changing what came in.

**State the rule and not the shapes.** The list was written as closed at two and
has since gone to four, and each time the correction had to be made in seven
files rather than one, because seven sites had copied the enumeration instead of
the rule. The unchanging half is "what cannot be re-serialized faithfully is
never guessed at"; that is the half to search on and the half to quote.

**The exception belongs to the second clause only, and the embedded fix is what
made that distinction real.** Every shape is an answer the *sweep* gives. Inside a
field the gateway describes the same shapes refuse the response — 502,
`mapping_lossy_document` — because serving them as they came would serve the
placeholder the first clause promises the client will not see, and serving them
re-serialized would hand a client a document to execute with a member missing.
So the first clause has no exception, which is a strengthening rather than a
narrowing, and the sites below state the exception under "elsewhere" or state it
wrongly.

**The sentence that used to close this paragraph — "and it is stated wherever
the promise is stated" — was false at the moment it was written**, since this
paragraph listed only the collision exception while claiming the synchronisation
it was breaking. A claim about other files is not evidence about them. The
promise has seven live sites: `README.md`, `docs/frontend-handoff.md`,
`restore_sweep`'s doc, `restore_in_string_with`'s doc, `proxy::handle`'s comment
above the sweep, `a_streamed_block_restores_the_fields_no_slot_addresses`, and
this paragraph. (The step list in `docs/superpowers/plans/` is an eighth
occurrence and is deliberately left stale: it is a historical instruction, not a
live claim.) Three separate rounds each corrected the sites sharing the
correcting reader's vocabulary and missed the one that stated the claim in its
own words — first the streamed-path comment, then the handoff paragraph, then
this one. So: when the clause changes, enumerate the sites and search on the
*unchanging* half of the promise. Sweeping for the wording you are replacing
finds only the sites that already agree with you.

The fourth site moved and the enumeration above is corrected rather than
appended to: the exception clause was stated on `restore_in_string`, and the
embedded fix split that function into a lenient door, a strict door
(`restore_in_string_strictly`) and the shared `restore_in_string_with` that
carries the rule. The clause went with the rule. A reader who greps for the
function name in this list and finds a three-line wrapper has found the wrong
site, which is the failure mode this list exists to prevent.

### Two limits of the issued set, found by attacking it here

Both are properties of the discriminator rather than of this implementation, so
they belong beside it rather than in a commit message. **One of them is
unreachable and the other is live**, and an earlier draft called both unreachable
— which would have kept the live one out of acceptance coverage.

**It assumes the client echoes the conversation.** The set is built from what
*this request* masked, which works because a client resends its history and the
same value is masked again each turn. An API that keeps conversation state on the
provider's side — a request carrying an identifier instead of the prior turns —
would leave the set empty while the model still answers with a token it
remembers, and the sweep would leave it. This gateway serves no such endpoint, so
it is a premise to record rather than a hole to plug, and adding one would
require revisiting this.

**One token can get two treatments in one response — and this one is reachable
today, on `/v1/chat/completions`.** It is this document's opening example run one
step further: `content` is restored while the same token in `refusal` is not. A
described field restores strictly from the table, ambiguity included, which is
today's behaviour and is not changed here; the same token in an undescribed field
is left. So a client sees the value in `content` and the token in `refusal`.

Making them agree means either weakening the described path — a regression — or
#32. It is therefore **not** deferred quietly: it has a test of its own below, so
the behaviour is pinned rather than merely described, and whoever reads the
journal line or the response can find it written down.

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
   backslash and a newline inside `arguments` must produce valid JSON carrying
   the fields it started with. **The mutation is reverting structural
   restoration to plain substitution**, which is the rule the property depends
   on. Two earlier drafts named the *ordering* mutation here and both were
   wrong — the first because the design's order had since flipped, and the
   second because the structural rule made the ordering mutation harmless:
   after strict slot restoration the token is gone, and any token still
   standing is parsed and re-serialized rather than substituted. The ordering
   regression is item 4's job and this item should not duplicate it.
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
9. **The two treatments of one token, pinned rather than described.** A
   multi-turn session where the caller writes a session-owned literal: the same
   token comes back restored in `content` and untouched in `refusal`. This is
   live behaviour on `/v1/chat/completions` today and is what #32 resolves, so
   the test asserts the divergence deliberately and names the issue that will
   change it.
10. **Provider parity.** One response shape driven through **both** providers,
   asserting the same treatment.

Item 10 is the only test here that catches the **class** rather than the instance.
The others prove these two fields are fixed; that one exists so the next pair
does not diverge again — which is how this bug, and three like it, were made.

## Out of scope

- Unifying the three restoration policies into one function parameterised by
  strictness. It is the right observation and the wrong moment: it touches the
  streamed path, which this work does not require, and which is the most delicate
  code in the gateway. Recorded as the follow-up.
- Anthropic's response-side refusals, above.
- Issue #32, above.
