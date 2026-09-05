# A stream cannot parse, which is not the same as cannot decide — design

Closes #55, which #54 filed as its own deliberate residual.

## What #54 left open, and why it thought it had to

`stream: true` beside a declared `response_format` is refused before the upstream
call. What remained was a stream the caller never declared: the reply is a
document, the gateway was not told so, and `RestoreBuffer` substituted values
into it as text. `x","admin":true,"pad":"` — or an apostrophe, which closes a
string for a permissive reader just as well — landed in the client's document
unescaped, with the bytes gone before anything could reconsider.

Three options were weighed and all three rejected: refuse on the value (refuses
most streamed prose, since `json_string_inert` excludes the apostrophe), buffer
the whole run (silently stops streaming, unbounded memory), or track JSON
structure across fragments (a second parser on the one path where a mistake
cannot be taken back).

## The mistake in that reasoning

**A delta cannot be parsed. That was read as "a stream cannot decide anything",
and it does not follow.**

The buffered path's rule is not one question but two, asked in order:

1. does a substituted value carry a character that could close a string?
2. was a container opened before the token — `structure_encloses_a_token`?

The first is a property of the *value*, which a stream has in full. The second
is a property of *text already gone past*, which is precisely what a stream has
and a parse is not needed for: a `{` or `[` seen earlier stays seen. Only the
**repair** — parse the document, place the value in a leaf, escape it on the way
out — needs the whole string.

So a stream can reach the same verdict and simply cannot perform the same
remedy. It refuses instead.

## The change

`Mapping::restore_in_stream(text, opened)` replaces `Mapping::restore` in
`RestoreBuffer`. It walks `pieces` exactly as `restore` does, sets `opened` on
any text run carrying `{` or `[`, and refuses a substitution when `opened` holds
and the value is not wholly `json_string_inert`.

`opened` lives on the buffer, so it is scoped to one text run — the granularity
at which `stream::handle` keys buffers, and the same granularity at which the
buffered path restores a slot. It is never reset, exactly as
`structure_encloses_a_token` never resets its own local.

| | buffered | streamed |
|---|---|---|
| value wholly inert | substitute | substitute |
| non-inert, no container opened | substitute (prose) | substitute (prose) |
| non-inert, container opened, parses | restore structurally | **refuse** |
| non-inert, container opened, will not parse | refuse | refuse |

**One row differs and it is the price of the missing parse.** A stream is
therefore strictly more likely to end than a buffered response is to fail.

## What it costs

**A stream that ends where the buffered path would have succeeded.** The
conjunction is a bracket before a token, and a value carrying a character
outside the inert set. In practice: a markdown list or a fenced example beside a
name like `O'Brien`.

That is a real regression against today's behaviour for that text — today it is
substituted, correctly, because it is prose. It is *not* a new rule invented
here: the buffered path already refuses exactly that text, in the fourth row
above, and six earlier readings that each claimed something about what a
permissive reader accepts were each defeated in turn. Sharing a hard-won rule is
worth more than being cleverer about brackets on one path only.

The client sees a truncated answer and an `error` event — the mechanism the
README already describes for a token with no mapping — rather than a document
altered under them.

**#54's admission refusal is not made redundant.** It refuses before the
upstream call and costs the caller nothing, which is possible only because the
caller *said* the reply would be a document. This refuses mid-stream, after
tokens are spent, which is the best available answer when nobody said.

## A consequence worth naming

**`Mapping::restore` — the plain textual substitution — has no production caller
left.** It was the streamed path's restoration and the last place in the gateway
that substituted a value into a caller's text without first asking whether the
text was a document. Every path asks now: the sweep through `Lenient`, the
described slots through `Strict`, the stream through `opened`. It is kept
`#[cfg(test)]` for the tests that state the substitution contract on its own,
where the document question would be noise.

## Testing

Nothing in the existing suite noticed the change — 556 tests passed before and
after — which is the second time in two changes that a rule about admission or
restoration was rewritten without a single test objecting.

One test per row of the table, plus the two properties the flag exists for:

- a non-inert value inside an opened structure is refused;
- **the structure and the token arrive in different fragments** — the whole
  reason this is a flag and not a scan of the fragment in hand;
- the same value in prose is substituted;
- an inert value inside a structure is substituted;
- a container opened *after* the token does not enclose it, **both halves in one
  push** — split across two, this passes under a rule that pre-scans each
  fragment, and that mutation survived when the test was written that way;
- one run's structure does not bind another.

Mutations:

- **drop the refusal** → three tests fail;
- **ignore `opened` and refuse on the value alone** → three fail, including the
  prose one, which is the rejected design stated as a test;
- **reset `opened` per fragment** → the split-fragment test fails;
- **pre-scan the whole fragment for brackets before restoring it** → three fail,
  including the ordering test in its strengthened form.

A fifth mutation — moving `*opened = true` after `out.push_str(run)` — survived,
and correctly: within one run nothing reads the flag between those two lines, so
the two orderings are the same program. It is recorded because the test it was
meant to challenge was, at that moment, too weak for a different reason, and the
survival is what sent me to look.
