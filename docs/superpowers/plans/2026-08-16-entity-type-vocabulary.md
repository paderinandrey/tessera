# Entity type vocabulary implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A detector that returns a submitted value as an entity type can no longer send that value to the provider inside a placeholder name.

**Architecture:** The gateway holds the twenty-two entity types its detector declares, as a constant it owns. `Mapping::placeholder_for` checks membership instead of grammar; a type outside the list masks as `[REDACTED_n]` rather than refusing. A CI job fails when the constant and the detector's catalogs disagree.

**Tech Stack:** Rust (gateway), Python (the drift check, run from CI), the existing `detector/src/tessera_detector/catalog/*.yaml`.

**Spec:** `docs/superpowers/specs/2026-08-16-entity-type-vocabulary-design.md`

## Global Constraints

- **The vocabulary comes from nowhere the detector can influence at runtime.** Not from `/detect`, not from a capability endpoint, not from configuration. A compromised detector asked to declare its own types would simply declare `WEBER` one.
- **An unknown type is masked, never refused.** Confidentiality is identical either way; refusing would break a working gateway on the day someone widens detection.
- **The warning names the count, never the type.** The unknown name came from outside the perimeter and is exactly what must not be written down.
- **`REDACTED` is the gateway's own and must not appear in the detector's catalogs**, or a detector returning it would be indistinguishable from the gateway's fallback.
- **The audit module's own check stays.** `Record::detected` keeps bucketing illegible keys under `unvalidated`; masking and audit must not depend on each other.
- `cd gateway && cargo test` and `cargo fmt --check && cargo clippy --all-targets -- -D warnings` must pass.
- Comments explain *why* a rule exists, not what the line does, and must never describe a mechanism the code does not have.
- Commit messages follow the repository style: `fix(gateway):`, `test(gateway):`, `ci:`, `docs:`.
- If `git commit` fails with a 1Password or signing-agent error, do not work around it — leave the work staged, put the message in your report, and say so. This has happened repeatedly on this repository.

## File structure

| path | responsibility |
|---|---|
| `gateway/src/mapping.rs` | the vocabulary constant, the membership check, the `REDACTED` fallback, and their tests |
| `gateway/src/proxy.rs` | the warning, once per request that contained an unknown type |
| `scripts/check_entity_types.py` | compares the constant against the two catalogs |
| `.github/workflows/ci.yml` | runs the drift check |
| `Makefile` | a target for the drift check, so it is runnable by hand |

---

### Task 1: the vocabulary and the fallback

**Files:**
- Modify: `gateway/src/mapping.rs` — the constant, `placeholder_for` (currently at `:147`), and `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub const ENTITY_TYPES: [&str; 22]`, `pub const REDACTED_TYPE: &str = "REDACTED"`, and `Mapping::mask` returning `[REDACTED_n]` for a type outside the list. `MappingError::BadEntityType` stops being constructed — Task 2 deals with what that leaves behind.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/mapping.rs`:

```rust
    #[test]
    fn a_value_masquerading_as_a_type_does_not_reach_the_placeholder() {
        // The leak this slice exists for: a detector that returns the span's
        // own value as its type would otherwise put that value in the token
        // the provider receives.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("WEBER", &[span("WEBER", 0, 5)])
            .expect("an unknown type is masked, not refused");

        assert_eq!(masked, "[REDACTED_1]");
        assert!(
            !masked.contains("WEBER"),
            "the submitted value reached the placeholder: {masked}"
        );
    }

    #[test]
    fn every_declared_type_keeps_its_own_name() {
        // Without this, a fix that rejects everything passes the test above.
        for entity_type in ENTITY_TYPES {
            let mut mapping = Mapping::new();
            let masked = mapping
                .mask("Weber", &[span(entity_type, 0, 5)])
                .expect("a declared type masks");
            assert_eq!(
                masked,
                format!("[{entity_type}_1]"),
                "{entity_type} did not keep its name"
            );
        }
    }

    #[test]
    fn two_unknown_types_stay_distinguishable() {
        // REDACTED draws from the shared counter, so two values do not collapse
        // into one token and tell the model they are the same thing.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "WEBER MEIER",
                &[span("WEBER", 0, 5), span("MEIER", 6, 11)],
            )
            .expect("both are masked");

        assert_eq!(masked, "[REDACTED_1] [REDACTED_2]");
    }

    #[test]
    fn an_unknown_type_restores_to_its_value() {
        // Masking under a generic name must not cost restoration.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("WEBER", &[span("WEBER", 0, 5)])
            .expect("masked");
        assert_eq!(mapping.restore(&masked).expect("restores"), "WEBER");
    }

    #[test]
    fn redacted_is_not_a_type_the_detector_can_claim() {
        // A detector returning REDACTED would be indistinguishable from the
        // gateway's own fallback, so the vocabulary must not contain it.
        assert!(!ENTITY_TYPES.contains(&REDACTED_TYPE));
    }

    #[test]
    fn every_declared_type_fits_a_streamed_placeholder() {
        // MAX_ENTITY_TYPE stops being an input check and becomes an assertion
        // about this list: a longer name would be released as ordinary text by
        // the stream's hold-back buffer and reach the client unrestored.
        for entity_type in ENTITY_TYPES {
            assert!(
                entity_type.len() <= MAX_ENTITY_TYPE,
                "{entity_type} is too long to survive a stream"
            );
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test mapping::`
Expected: FAIL to compile — `ENTITY_TYPES` and `REDACTED_TYPE` are not defined.

- [ ] **Step 3: Add the vocabulary**

In `gateway/src/mapping.rs`, immediately below the existing `MAX_ENTITY_TYPE` constant:

```rust
/// The entity types this gateway's detector declares — eight from its
/// identifier catalog and fourteen from its NER configuration.
///
/// The list lives here, and not behind a question to the detector, because the
/// detector's response is what it defends against: a compromised one asked to
/// declare its own vocabulary would simply declare a submitted value to be a
/// type. `scripts/check_entity_types.py` fails CI when this list and the
/// catalogs disagree, so adding a type stays a deliberate change in two places
/// rather than a silent divergence.
pub const ENTITY_TYPES: [&str; 22] = [
    // Deterministic, checksum-validated (identifiers.yaml)
    "CH_AVS",
    "CREDIT_CARD",
    "DE_STEUERNUMMER",
    "DE_STEUER_ID",
    "EMAIL",
    "FR_NIF",
    "FR_NIR",
    "IBAN",
    // Quasi-identifiers (ner.yaml)
    "LOCATION",
    "ORG",
    "PERSON",
    // GDPR Article 9 special categories (ner.yaml)
    "BIOMETRIC",
    "ETHNICITY",
    "GENETIC",
    "HEALTH",
    "PHILOSOPHICAL_BELIEF",
    "POLITICAL_AFFILIATION",
    "POLITICAL_OPINION",
    "RELIGION",
    "SEXUAL_ORIENTATION",
    "SEX_LIFE",
    "TRADE_UNION",
];

/// What a span masks as when its type is not one of ours. The value is hidden
/// either way; what is lost is the model knowing what kind of thing it was.
/// Deliberately absent from the detector's catalogs — a detector returning it
/// would be indistinguishable from this fallback.
pub const REDACTED_TYPE: &str = "REDACTED";
```

- [ ] **Step 4: Replace the grammar check with membership**

In `placeholder_for`, replace the whole `if entity_type.is_empty() || …` block and the line that follows it with:

```rust
        // Syntax cannot tell a type name from a value shaped like one, and
        // `WEBER` for a span covering WEBER passes any grammar. So the name is
        // taken only when it is one we declared; anything else is still masked,
        // under a name that carries nothing of the value.
        let entity_type = if ENTITY_TYPES.contains(&entity_type) {
            entity_type
        } else {
            REDACTED_TYPE
        };
```

The `loop` below it is unchanged — it already builds `[{entity_type}_{n}]` from whatever `entity_type` now holds.

- [ ] **Step 5: Run the tests**

Run: `cd gateway && cargo test`
Expected: the six new tests PASS. **Two existing tests now fail**, and that is correct: `an_entity_type_outside_the_grammar_is_refused` (`mapping.rs:387`) and `an_entity_type_too_long_to_survive_a_stream_is_refused` (`:397`) assert a refusal this slice deliberately removed. Do not delete them — Task 2 rewrites them into what they should now assert. If anything *else* fails, stop and report it.

- [ ] **Step 6: Commit**

```bash
cd gateway && cargo fmt
cd .. && git add gateway/src/mapping.rs
git commit -m "fix(gateway): take an entity type only from a list we declared

A detector that returns a span's own value as its type — WEBER for a span
covering WEBER — passed the grammar check and put that value in the token
sent to the provider. The check was written to answer whether a type can
be restored, not what the string means, and syntax cannot tell a type name
from a value shaped like one.

The vocabulary lives in the gateway rather than behind a question to the
detector, because the detector's answer is what it defends against. An
unrecognised type still masks, as REDACTED: confidentiality is the same
either way, and refusing would break a working gateway on the day someone
widens detection."
```

---

### Task 2: retire the refusal this made unreachable

**Files:**
- Modify: `gateway/src/mapping.rs` — `MappingError` (`:27`), the two tests at `:387` and `:397`
- Modify: `gateway/src/proxy.rs` — `audit_class` (`:80`)

**Interfaces:**
- Consumes: `ENTITY_TYPES`, `REDACTED_TYPE` from Task 1.
- Produces: `MappingError` without `BadEntityType`, and `ProxyError::audit_class` without `mapping_bad_entity_type`.

`BadEntityType` can no longer be constructed after Task 1. Leaving it is worse than dead code: it is an error variant a reader will assume is reachable, and a vocabulary in the journal that nothing can ever emit.

- [ ] **Step 1: Rewrite the two tests to assert what now happens**

Replace `an_entity_type_outside_the_grammar_is_refused` and `an_entity_type_too_long_to_survive_a_stream_is_refused` in `gateway/src/mapping.rs` with:

```rust
    #[test]
    fn a_type_outside_the_grammar_is_masked_rather_than_refused() {
        // It used to be refused. Masking is the same protection and does not
        // break a gateway whose detector has grown a type it does not know.
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping
                .mask("Weber", &[span("person", 0, 5)])
                .expect("masked, not refused"),
            "[REDACTED_1]"
        );
    }

    #[test]
    fn a_type_too_long_to_survive_a_stream_is_masked_rather_than_refused() {
        // A name longer than MAX_ENTITY_TYPE cannot be in the vocabulary — the
        // test above proves that of the list — so it takes the same path as any
        // other unknown type and never reaches the stream's hold-back buffer.
        let long = "A".repeat(MAX_ENTITY_TYPE + 1);
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping
                .mask("Weber", &[span(&long, 0, 5)])
                .expect("masked, not refused"),
            "[REDACTED_1]"
        );
    }
```

- [ ] **Step 2: Run them**

Run: `cd gateway && cargo test mapping::`
Expected: PASS. The whole `mapping` module is green at this point; `proxy` still compiles because `BadEntityType` still exists.

- [ ] **Step 3: Remove the variant**

In `gateway/src/mapping.rs`, delete the `BadEntityType` variant from `MappingError` along with its `#[error(...)]` attribute.

In `gateway/src/proxy.rs`, delete the `audit_class` arm:

```rust
            ProxyError::Mapping(MappingError::BadEntityType(_)) => "mapping_bad_entity_type",
```

`audit_class` has no `_` fallback for `MappingError`, so the compiler will confirm the remaining arms are exhaustive rather than silently accepting a hole.

- [ ] **Step 4: Run the full suite**

Run: `cd gateway && cargo test`
Expected: PASS. If the compiler reports another consumer of `BadEntityType` that this plan did not name, stop and report it — the plan searched for them and found only these two.

- [ ] **Step 5: Commit**

```bash
cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings
cd .. && git add gateway/src/mapping.rs gateway/src/proxy.rs
git commit -m "fix(gateway): retire the entity-type refusal nothing can reach

With an unrecognised type masked as REDACTED, BadEntityType can no longer
be constructed. Leaving it would be worse than dead code: an error variant
a reader assumes is reachable, and an audit class the journal can never
emit. The two tests that asserted the refusal now assert the masking that
replaced it."
```

---

### Task 3: say when it happened

**Files:**
- Modify: `gateway/src/mapping.rs` — a counter on `Mapping`
- Modify: `gateway/src/proxy.rs` — the warning, in `mask_all`

**Interfaces:**
- Consumes: Task 1's fallback.
- Produces: `Mapping::redacted_count(&self) -> usize`.

An unknown type means the detector and the gateway disagree about what a type is. That is worth knowing before an auditor finds `unvalidated` in the journal months later.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `gateway/src/mapping.rs`:

```rust
    #[test]
    fn the_mapping_counts_what_it_had_to_redact() {
        let mut mapping = Mapping::new();
        mapping
            .mask("WEBER Weber", &[span("WEBER", 0, 5), span("PERSON", 6, 11)])
            .expect("masked");

        assert_eq!(mapping.redacted_count(), 1, "one unknown type, one count");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd gateway && cargo test mapping::the_mapping_counts`
Expected: FAIL to compile — no method `redacted_count`.

- [ ] **Step 3: Add the counter**

Add a field to `struct Mapping`:

```rust
    /// How many spans arrived with a type outside our vocabulary. Reported once
    /// per request rather than per span: a detector that disagrees about types
    /// disagrees about all of them, and one line per span would be a flood.
    redacted: usize,
```

Increment it in `placeholder_for`, in the `else` branch that selects `REDACTED_TYPE`, and expose it:

```rust
    /// How many spans this mapping had to mask under the generic type.
    pub fn redacted_count(&self) -> usize {
        self.redacted
    }
```

Note `Mapping` derives `Default`, so the new field needs no constructor change. `absorb` copies values between mappings and must **not** carry this count across — it describes one request, and a session's mapping outlives the request.

- [ ] **Step 4: Run the test**

Run: `cd gateway && cargo test mapping::`
Expected: PASS.

- [ ] **Step 5: Emit the warning**

In `gateway/src/proxy.rs`, in `mask_all`, after the loop over pointers and before the function returns its tuple:

```rust
    if mapping.redacted_count() > 0 {
        // The count, never the name: the name is the untrusted string this
        // check exists to keep out of anything we write down. A detector and a
        // gateway that disagree about what a type is should not wait for an
        // audit to be noticed.
        tracing::warn!(
            count = mapping.redacted_count(),
            "the detector reported entity types outside this gateway's vocabulary"
        );
    }
```

- [ ] **Step 6: Run the full suite and commit**

Run: `cd gateway && cargo test`
Expected: PASS.

```bash
cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings
cd .. && git add gateway/src/mapping.rs gateway/src/proxy.rs
git commit -m "feat(gateway): warn when a type had to be redacted

A type outside the vocabulary means the detector and the gateway disagree
about what a type is. Reported once per request with a count and never the
name — the name is the untrusted string the check exists to keep out of
anything we write down."
```

---

### Task 4: fail CI when the list drifts

**Files:**
- Create: `scripts/check_entity_types.py`
- Modify: `Makefile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `ENTITY_TYPES` from Task 1, and the detector's two catalog files.
- Produces: `make check-entity-types`, exit 0 when they agree and non-zero naming the difference when they do not.

Without this the constant rots at the first type anyone adds, and the same class of defect returns in a new place.

- [ ] **Step 1: Write the checker**

Create `scripts/check_entity_types.py`:

```python
"""Fail when the gateway's entity-type vocabulary and the detector's catalogs
disagree.

The gateway holds its own copy on purpose — asking the detector which types it
emits would be worthless against a detector that lies. The copy is only safe
while something notices it drifting, and that is this script.
"""

import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAPPING = ROOT / "gateway" / "src" / "mapping.rs"
CATALOGS = (
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "identifiers.yaml",
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "ner.yaml",
)

DECLARATION = re.compile(
    r"pub const ENTITY_TYPES: \[&str; \d+\] = \[(.*?)\];", re.DOTALL
)
NAME = re.compile(r'"([A-Z_]+)"')


def gateway_types() -> set[str]:
    match = DECLARATION.search(MAPPING.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{MAPPING}: no ENTITY_TYPES declaration found")
    return set(NAME.findall(match.group(1)))


def detector_types() -> set[str]:
    found: set[str] = set()
    for path in CATALOGS:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        for section in ("identifiers", "entities"):
            for entry in document.get(section) or ():
                found.add(entry["entity_type"])
    return found


def main() -> None:
    gateway, detector = gateway_types(), detector_types()
    if gateway == detector:
        print(f"entity types agree: {len(gateway)} declared on both sides")
        return

    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"declared by the detector, absent from the gateway: {missing}")
    if extra:
        print(f"declared by the gateway, absent from the detector: {extra}")
    sys.exit(
        "entity types have drifted; the gateway would mask the difference as "
        "REDACTED without saying why"
    )


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run it, and watch it pass**

Run: `uv run --project detector python scripts/check_entity_types.py`
Expected: `entity types agree: 22 declared on both sides`.

`uv run --project detector` is how this repository runs Python scripts that need the detector's dependencies — `pyyaml` is one of them. The existing `Makefile` targets use the same form.

- [ ] **Step 3: Prove it discriminates, in both directions**

A drift check nobody has watched fail is a guess. Run each of these, confirm the failure, then restore:

1. Change one name in `ENTITY_TYPES` — say `IBAN` to `IBANN`. Expected: exit non-zero, naming `IBAN` as missing from the gateway and `IBANN` as absent from the detector.
2. Delete one entry from `ENTITY_TYPES`. Expected: exit non-zero naming it as missing from the gateway.
3. Add `"NONSENSE"` to `ENTITY_TYPES`. Expected: exit non-zero naming it as absent from the detector.

Report the three observed failures. If any of them passes, the script is not doing its job and fixing it is part of this task.

- [ ] **Step 4: Add the Makefile target**

Add to `Makefile`, and add `check-entity-types` to the `.PHONY` line:

```make
check-entity-types:
	uv run --project detector python scripts/check_entity_types.py
```

- [ ] **Step 5: Add it to CI**

In `.github/workflows/ci.yml`, add a step to the existing `detector` job, after `uv run mypy src`:

```yaml
      # The gateway keeps its own copy of the entity-type vocabulary, because
      # asking the detector which types it emits would be worthless against a
      # detector that lies. The copy is only safe while something notices it
      # drifting.
      - run: uv run python ../scripts/check_entity_types.py
```

The `detector` job already has `working-directory: detector` and a synced environment, so the script runs there with `pyyaml` available and no new CI cost. Do not add a separate job.

- [ ] **Step 6: Commit**

```bash
git add scripts/check_entity_types.py Makefile .github/workflows/ci.yml
git commit -m "ci: fail when the entity-type vocabulary drifts

The gateway holds its own copy of the vocabulary because asking the
detector which types it emits would be worthless against a detector that
lies. A copy is only safe while something notices it drifting: without
this, the constant rots at the first type anyone adds and the same class
of defect returns in a new place."
```

---

### Task 5: record what changed

**Files:**
- Modify: `README.md` — the `## Gateway` section

**Interfaces:**
- Consumes: everything above. Produces nothing code depends on.

- [ ] **Step 1: Check what the README currently claims**

Run: `grep -n "span the detector reports that cannot be applied" README.md`

That clause sits in the paragraph listing what refuses a request. A span whose type is unusable is no longer among them, so the sentence is now wrong.

- [ ] **Step 2: Correct it and say what happens instead**

Amend that clause so it no longer implies an unusable type refuses the request, and add to the same section:

```markdown
Placeholders carry the type the detector reported, but only when it is one this
gateway declares — twenty-two of them, the catalog's eight checksum-validated
identifiers plus the fourteen the NER layer can label. A type outside that list
is masked as `[REDACTED_1]` instead. Syntax cannot tell a type name from a value
shaped like one: a detector returning `WEBER` as the type of a span covering
`WEBER` would otherwise put that value in the token the provider receives. The
gateway keeps its own copy of the list rather than asking the detector, since
the detector's answer is what the check defends against, and CI fails if the two
drift apart.
```

Match the surrounding register: flowing prose, reasons attached to rules, no bullet lists.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: say which entity types reach a placeholder

The refusal list no longer includes an unusable span type, and the README
still said it did."
```

---

## Self-review

Checked against `docs/superpowers/specs/2026-08-16-entity-type-vocabulary-design.md`:

| Spec requirement | Task |
|---|---|
| Vocabulary in the gateway, not from the detector | 1 |
| List replaces the grammar in `placeholder_for` | 1 |
| Unknown type masks as `[REDACTED_n]` | 1 |
| `REDACTED` absent from the catalogs | 1 (asserted) |
| `MAX_ENTITY_TYPE` becomes an assertion about the list | 1 |
| Shared counter keeps two unknowns distinguishable | 1 |
| Restoration unaffected | 1 |
| Warning with the count, never the name | 3 |
| The journal's own check stays | — (nothing removes it; Task 2 touches only `BadEntityType`) |
| CI fails on drift | 4 |
| All twenty-two known types keep their names | 1 |
| Drift check proven to fail | 4 |

Two things this plan adds that the spec did not name, both because leaving them out would ship a hole:

- **Task 2 exists at all.** The spec does not mention `MappingError::BadEntityType`, but making an unknown type mask means nothing can construct it any more. A dead error variant and a dead audit class would outlive the change and mislead the next reader.
- **Task 3's counter must not survive `absorb`.** A session's mapping outlives the request, so a count copied into it would report an old request's disagreement on every later turn. Named in the step rather than left to be discovered.

One boundary worth stating: `Mapping::restore` is untouched. It matches placeholders by the grammar `[A-Z_]+_\d+`, which `REDACTED` satisfies, so restoration needs no change — and Task 1's fourth test is there to prove that rather than assume it.
