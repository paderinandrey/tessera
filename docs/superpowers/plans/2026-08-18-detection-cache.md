# Detection Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop rescanning a conversation's whole history on every request by caching detector spans, keyed so that a hit is never worse than a fresh call.

**Architecture:** The detector starts declaring a version digest over its weights and catalogs. The gateway parses that version plus `layers_run`, and `DetectorClient` gains an in-memory cache keyed by `(version, credential digest, text digest)`. Only complete runs are stored; the cache holds no submitted text, and a miss can never fail a request.

**Tech Stack:** Rust (axum, reqwest, sha2, wiremock), Python 3.14 (FastAPI, pytest), TOML config.

## Global Constraints

- Design doc: `docs/superpowers/specs/2026-08-18-detection-cache-design.md`. Every decision below comes from it.
- **No new Rust or Python dependencies.** `sha2` and `getrandom` are already in `gateway/Cargo.toml`; nothing else is needed. An LRU crate is explicitly rejected in the spec.
- **The cache stores no submitted text.** Key fields are `[u8; 32]`, never `String`.
- **A miss is never a refusal.** Every failure path in the cache degrades to "call the detector".
- Rust edits must pass `cargo fmt --check` and `cargo clippy --all-targets` with no warnings.
- Python edits must pass `uv run ruff check .` and `uv run mypy src` from `detector/`.
- Comments explain *why*, matching the surrounding code's density. Test names are declarative sentences, matching `mapping.rs` and `detector.rs`.
- Each **distinct invariant** is proved by mutation: break it, watch the test guarding it fail — and confirm the tests guarding other invariants still pass — then restore. Record the observed failure. Tests that approach one invariant from several angles share a mutation; tests guarding different properties each need their own. Where a task's steps list fewer mutations than it has invariants, the rule above governs and the implementer adds the missing ones.

## File Structure

| File | Responsibility |
|---|---|
| `detector/src/tessera_detector/version.py` | **Create.** Pure `version_from()` plus a thin `detector_version()` that reads the real catalogs. |
| `detector/tests/test_version.py` | **Create.** Version is stable, and changes when weights or either catalog change. |
| `detector/src/tessera_detector/api.py` | **Modify.** `DetectResponse` gains `version`; the endpoint fills it. |
| `docs/api/openapi.json` | **Regenerate.** Committed schema must match the new response. |
| `gateway/src/detection_cache.rs` | **Create.** The store: key type, salt, LRU-by-scan eviction, poisoned-lock degradation. |
| `gateway/src/detector.rs` | **Modify.** Parse `version`/`layers_run`, own the cache, new `detect` signature. |
| `gateway/src/config.rs` | **Modify.** `detection_cache_entries`. |
| `gateway/src/proxy.rs` | **Modify.** Thread the credential into `mask_all`; build the cache from config. |
| `gateway/src/main.rs` | **Modify.** Declare the new module. |
| `gateway/tessera.example.toml`, `deploy/tessera.container.toml`, `deploy/tessera.demo.toml` | **Modify.** Document the new key. |
| `scripts/check_layers.py`, `Makefile`, `.github/workflows/ci.yml` | **Create/modify.** Guard the gateway's copy of the layer list. |

---

### Task 1: The detector declares what determines its output

**Files:**
- Create: `detector/src/tessera_detector/version.py`
- Create: `detector/tests/test_version.py`
- Modify: `detector/src/tessera_detector/api.py`
- Regenerate: `docs/api/openapi.json`

**Interfaces:**
- Produces: `version_from(model_id: str, catalogs: Iterable[bytes]) -> str` and `detector_version() -> str`, a 32-character hex string. `DetectResponse` gains `version: str`.

- [ ] **Step 1: Write the failing test**

Create `detector/tests/test_version.py`:

```python
"""The version is what the gateway keys its span cache by, so it must change
whenever the same text would produce different spans."""

from tessera_detector.version import detector_version, version_from


def test_the_same_inputs_give_the_same_version():
    first = version_from("gliner@abc123", [b"identifiers", b"ner"])
    second = version_from("gliner@abc123", [b"identifiers", b"ner"])
    assert first == second


def test_changing_the_weights_changes_the_version():
    pinned = version_from("gliner@abc123", [b"identifiers", b"ner"])
    bumped = version_from("gliner@def456", [b"identifiers", b"ner"])
    assert pinned != bumped


def test_editing_either_catalog_changes_the_version():
    # A threshold edit changes what is detected without touching HF_REVISION.
    # If the version missed that, the gateway would serve spans from the old
    # thresholds until its cache aged out.
    base = version_from("gliner@abc123", [b"identifiers", b"ner"])
    first_edited = version_from("gliner@abc123", [b"identifiers!", b"ner"])
    second_edited = version_from("gliner@abc123", [b"identifiers", b"ner!"])
    assert base != first_edited
    assert base != second_edited
    assert first_edited != second_edited


def test_catalog_order_is_not_a_concatenation_accident():
    # Hashing each catalog before folding it in means a byte moving across the
    # boundary between two catalogs cannot leave the version unchanged.
    assert version_from("m", [b"ab", b"c"]) != version_from("m", [b"a", b"bc"])


def test_the_real_detector_reports_a_stable_version():
    assert detector_version() == detector_version()
    assert len(detector_version()) == 32
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cd detector && uv run pytest tests/test_version.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'tessera_detector.version'`

- [ ] **Step 3: Implement the minimal code to make the test pass**

Create `detector/src/tessera_detector/version.py`:

```python
"""What determines this detector's output, as one digest.

The gateway caches spans and keys them by this value, so it has to change
whenever the same text would produce different spans. That is the pinned
weights *and* both catalogs: a threshold edit in `ner.yaml` changes what is
detected without touching `HF_REVISION`, and a cache that missed it would keep
serving the old thresholds.
"""

import hashlib
from collections.abc import Iterable
from importlib import resources

from .models import HF_REVISION, MODEL_NAME

CATALOGS = ("identifiers.yaml", "ner.yaml")


def version_from(model_id: str, catalogs: Iterable[bytes]) -> str:
    """The digest, over inputs the caller supplies. Pure, so it is testable
    without a populated model cache or a rewritten package resource."""
    digest = hashlib.sha256()
    digest.update(model_id.encode("utf-8"))
    for blob in catalogs:
        # Each catalog is hashed before it is folded in, so that a byte moving
        # from the end of one to the start of the next cannot leave the total
        # unchanged the way plain concatenation would.
        digest.update(hashlib.sha256(blob).digest())
    return digest.hexdigest()[:32]


def detector_version() -> str:
    catalog_dir = resources.files("tessera_detector") / "catalog"
    return version_from(
        f"{MODEL_NAME}@{HF_REVISION}",
        [(catalog_dir / name).read_bytes() for name in CATALOGS],
    )


__all__ = ["CATALOGS", "detector_version", "version_from"]
```

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cd detector && uv run pytest tests/test_version.py -v`
Expected: PASS, 5 tests.

- [ ] **Step 5: Prove the tests by mutation**

Temporarily change `digest.update(hashlib.sha256(blob).digest())` to `digest.update(blob)`.
Run: `cd detector && uv run pytest tests/test_version.py -v`
Expected: `test_catalog_order_is_not_a_concatenation_accident` FAILS, the other four pass.
Restore the line.

- [ ] **Step 6: Put the version in the detect response**

In `detector/src/tessera_detector/api.py`, add the import beside the others:

```python
from .version import detector_version
```

Change `DetectResponse`:

```python
class DetectResponse(BaseModel):
    spans: list[Span]
    layers_run: list[Layer] = Field(description="Layers that actually ran for this request")
    version: str = Field(
        description=(
            "Digest of the weights and catalogs that produced these spans. A caller "
            "that caches spans must key them by this: it changes whenever the same "
            "text would detect differently."
        )
    )
```

In the `detect` endpoint, pass it where the response is built:

```python
        return DetectResponse(
            spans=spans,
            layers_run=layers_run,
            version=detector_version(),
        )
```

Read the endpoint before editing — keep the existing `spans`/`layers_run` expressions exactly as they are and add only the third argument.

- [ ] **Step 7: Write the failing API test**

Append to `detector/tests/test_api.py`:

```python
def test_detect_reports_the_version_that_produced_the_spans():
    # The gateway keys its span cache by this; without it every cached entry
    # would outlive the model that justified it.
    client = TestClient(create_app(detector=build_detector(ner=False)))
    response = client.post("/detect", json={"text": "Weber", "layers": ["deterministic"]})
    assert response.status_code == 200
    assert response.json()["version"] == detector_version()
```

Add to that file's imports: `from tessera_detector.version import detector_version`.
If `build_detector` or `create_app` are imported under different names in that file, use the names already there — read the top of the file first.

- [ ] **Step 8: Run the detector suite**

Run: `cd detector && uv run pytest -q && uv run ruff check . ../evaluation && uv run mypy src`
Expected: all pass.

- [ ] **Step 9: Regenerate the committed schema**

Run: `make openapi`
Then confirm the diff touches `docs/api/openapi.json` and contains `version`:
Run: `git diff --stat docs/api/openapi.json`
Expected: the file changed.

- [ ] **Step 10: Commit**

```bash
git add detector/src/tessera_detector/version.py detector/tests/test_version.py \
        detector/src/tessera_detector/api.py detector/tests/test_api.py docs/api/openapi.json
git commit -m "feat(detector): report the version that determines detection

The gateway is about to cache spans, and a cache that outlives the model
that justified it serves worse detection than a fresh call would. The
version covers the pinned weights and both catalogs, because a threshold
edit in ner.yaml changes detections without touching HF_REVISION.

Hashing each catalog before folding it in is load-bearing: with plain
concatenation a byte moving across the boundary between two catalogs
leaves the digest unchanged. Proved by mutation — replacing the inner
hash with a raw update fails test_catalog_order_is_not_a_concatenation_accident
alone."
```

---

### Task 2: The gateway parses version and layers_run

**Files:**
- Modify: `gateway/src/detector.rs`

**Interfaces:**
- Consumes: the `version` field from Task 1.
- Produces: `pub(crate) struct Detection { pub spans: Vec<Span>, pub version: Option<String> }`, where `version` is `Some` only for a complete run from a detector that reported one. `DetectorClient::detect` keeps its current public signature in this task.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `gateway/src/detector.rs`:

```rust
    #[tokio::test]
    async fn a_complete_run_reports_a_version() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic", "ner"],
                "version": "abc123"
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        let detection = client.detect_full("Weber").await.unwrap();
        assert_eq!(detection.version.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn a_partial_run_reports_no_version() {
        // Serving it is correct; remembering it is not. A deterministic-only
        // result cached while NER is down would be replayed after NER is back.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic"],
                "version": "abc123"
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        let detection = client.detect_full("Weber").await.unwrap();
        assert!(detection.version.is_none());
    }

    #[tokio::test]
    async fn a_detector_that_reports_no_version_is_never_cacheable() {
        // An older detector predating the field. Keying everything it returns
        // under one empty version would be worse than not caching at all.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic", "ner"]
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        let detection = client.detect_full("Weber").await.unwrap();
        assert!(detection.version.is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test --quiet detector::`
Expected: FAIL to compile — no method `detect_full`, no type `Detection`.

- [ ] **Step 3: Implement**

In `gateway/src/detector.rs`, replace the `DetectResponse` struct and add the layer list, the `Detection` type and `detect_full`:

```rust
/// Every layer a complete run performs.
///
/// The gateway's own copy, for the same reason `mapping::ENTITY_TYPES` is one:
/// asking the detector which layers make a run complete would be worthless
/// against a detector that answers "the ones I ran". `scripts/check_layers.py`
/// fails CI when this list and the detector's `Layer` type disagree.
pub const LAYERS: [&str; 2] = ["deterministic", "ner"];

#[derive(Debug, Deserialize)]
struct DetectResponse {
    spans: Vec<Span>,
    #[serde(default)]
    layers_run: Vec<String>,
    #[serde(default)]
    version: Option<String>,
}

/// Spans, and whether they may be remembered.
///
/// `version` is `Some` only for a complete run from a detector that named the
/// weights and catalogs behind it. Everything else is served and forgotten, so
/// that a cache hit is never worse than a fresh call.
pub(crate) struct Detection {
    pub spans: Vec<Span>,
    pub version: Option<String>,
}
```

Rename the body of the current `detect` to `detect_full`, returning `Detection`, and keep `detect` as a thin caller:

```rust
    /// Every layer the detector has: the gateway does not narrow detection.
    pub async fn detect(&self, text: &str) -> Result<Vec<Span>, DetectorError> {
        Ok(self.detect_full(text).await?.spans)
    }

    pub(crate) async fn detect_full(&self, text: &str) -> Result<Detection, DetectorError> {
        let response = self
            .client
            .post(format!("{}/detect", self.base_url))
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            // The error carries the transport failure, never the text.
            .map_err(|error| DetectorError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(DetectorError::Status(response.status().as_u16()));
        }
        let parsed: DetectResponse = response
            .json()
            .await
            .map_err(|error| DetectorError::Transport(error.to_string()))?;
        let complete = LAYERS
            .iter()
            .all(|layer| parsed.layers_run.iter().any(|run| run == layer));
        let version = parsed
            .version
            .filter(|version| !version.is_empty())
            .filter(|_| complete);
        Ok(Detection {
            spans: parsed.spans,
            version,
        })
    }
```

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cd gateway && cargo test --quiet detector::`
Expected: PASS, including the pre-existing tests.

- [ ] **Step 5: Prove the tests by mutation**

Temporarily drop `.filter(|_| complete)`.
Run: `cd gateway && cargo test --quiet detector::`
Expected: `a_partial_run_reports_no_version` FAILS; the other two pass.
Restore it, then temporarily drop `.filter(|version| !version.is_empty())` and change `#[serde(default)] version: Option<String>` to `#[serde(default)] version: String` wrapped in `Some(...)`; expected: `a_detector_that_reports_no_version_is_never_cacheable` FAILS. Restore.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/detector.rs
git commit -m "feat(gateway): read the detector's version and layer set

Spans alone do not say whether they are worth remembering. A run without
NER, or from a detector too old to name its version, is served and
forgotten; only a complete run from a detector that identified itself
carries a version forward."
```

---

### Task 3: The configuration key

**Files:**
- Modify: `gateway/src/config.rs`
- Modify: `gateway/tessera.example.toml`, `deploy/tessera.container.toml`, `deploy/tessera.demo.toml`

**Interfaces:**
- Produces: `Config { pub detection_cache_entries: usize, .. }`, default 10 000, zero meaning disabled.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/config.rs`, using the file's existing `with_audit` helper:

```rust
    #[test]
    fn the_detection_cache_has_a_default() {
        let config = Config::from_toml(&with_audit("")).unwrap();
        assert_eq!(config.detection_cache_entries, 10_000);
    }

    #[test]
    fn the_detection_cache_can_be_sized() {
        let config = Config::from_toml(&with_audit("detection_cache_entries = 64")).unwrap();
        assert_eq!(config.detection_cache_entries, 64);
    }

    #[test]
    fn a_zero_detection_cache_is_a_setting_not_an_error() {
        // Unlike max_sessions, where zero would mean "no conversation can be
        // remembered": here it means "do not remember spans", which is exactly
        // today's behaviour and a legitimate memory budget.
        let config = Config::from_toml(&with_audit("detection_cache_entries = 0")).unwrap();
        assert_eq!(config.detection_cache_entries, 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test --quiet config::`
Expected: FAIL to compile — no field `detection_cache_entries`.

- [ ] **Step 3: Implement**

In `gateway/src/config.rs`, add the field to `Config` after `max_session_values`:

```rust
    /// How many detection results may be remembered at once. Zero disables the
    /// cache, which is a memory budget rather than a mistake: the gateway then
    /// calls the detector for every text, as it did before the cache existed.
    #[serde(default = "default_detection_cache_entries")]
    pub detection_cache_entries: usize,
```

And the default beside the others:

```rust
fn default_detection_cache_entries() -> usize {
    10_000
}
```

Add no validation. Zero is meaningful here, and `deny_unknown_fields` already rejects a typo in the key name.

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cd gateway && cargo test --quiet config::`
Expected: PASS.

- [ ] **Step 5: Document the key in all three config files**

Append to `gateway/tessera.example.toml`:

```toml
# How many detection results the gateway may remember at once. Detection runs
# over every text on every request, and a client resends the whole conversation
# each turn, so without this a ten-turn conversation pays for its first turn ten
# times. Entries hold a digest of the text and the spans found in it — never the
# text itself — and are invalidated automatically when the detector's weights or
# catalogs change. Roughly 300 bytes each, so the default is single-digit
# megabytes. Zero disables it.
detection_cache_entries = 10000
```

Append the same key with a one-line comment to `deploy/tessera.container.toml` and `deploy/tessera.demo.toml`:

```toml
# Spans are remembered by digest so a resent conversation is not rescanned.
detection_cache_entries = 10000
```

- [ ] **Step 6: Verify the container config still parses**

Run: `cd gateway && cargo run --quiet -- ../deploy/tessera.container.toml 2>&1 | head -3`
Expected: it fails to *bind* or to reach the detector, not to parse. A `ConfigError::Parse` here means `deny_unknown_fields` rejected the new key — check the field name matches.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/config.rs gateway/tessera.example.toml \
        deploy/tessera.container.toml deploy/tessera.demo.toml
git commit -m "feat(gateway): detection_cache_entries

On by default: the cache changes how fast the gateway answers, not what it
sends, so the reason to reach for the key is a memory budget rather than a
policy. Zero reproduces the behaviour that predates it."
```

---

### Task 4: The cache store

**Files:**
- Create: `gateway/src/detection_cache.rs`
- Modify: `gateway/src/main.rs`

**Interfaces:**
- Consumes: `crate::mapping::Span`.
- Produces:
  - `pub struct DetectionCache`
  - `DetectionCache::new(capacity: usize) -> Self`
  - `DetectionCache::get(&self, credential: Option<&[u8]>, text: &str) -> Option<Vec<Span>>`
  - `DetectionCache::insert(&self, version: &str, credential: Option<&[u8]>, text: &str, spans: &[Span])`
  - `DetectionCache::len(&self) -> usize` (tests and nothing else)

  `get` resolves the version itself, from the last one `insert` was given: the version is only ever learned from a response.

- [ ] **Step 1: Write the failing tests**

Create `gateway/src/detection_cache.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn span(entity_type: &str, start: usize, end: usize) -> Span {
        Span {
            entity_type: entity_type.to_owned(),
            start,
            end,
        }
    }

    const A: Option<&[u8]> = Some(b"Bearer a");
    const B: Option<&[u8]> = Some(b"Bearer b");

    #[test]
    fn a_stored_result_comes_back() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        let found = cache.get(A, "Weber").expect("stored under the known version");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entity_type, "PERSON");
    }

    #[test]
    fn nothing_is_known_before_the_first_insert() {
        // The version is only ever learned from a response, so a cold cache
        // has no version to look under and must miss.
        let cache = DetectionCache::new(4);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn another_credential_does_not_see_the_entry() {
        // Not because the spans would leak — B already has the text it sent —
        // but because the response time would say that A sent it first.
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(B, "Weber").is_none());
    }

    #[test]
    fn a_new_version_hides_everything_stored_under_the_old_one() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        cache.insert("v2", A, "Schmidt", &[span("PERSON", 0, 7)]);
        assert!(cache.get(A, "Weber").is_none());
        assert!(cache.get(A, "Schmidt").is_some());
    }

    #[test]
    fn saturation_evicts_the_least_recently_used() {
        let cache = DetectionCache::new(2);
        cache.insert("v1", A, "first", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "second", &[span("PERSON", 0, 6)]);
        // Touching "first" makes "second" the oldest.
        assert!(cache.get(A, "first").is_some());
        cache.insert("v1", A, "third", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(A, "first").is_some());
        assert!(cache.get(A, "second").is_none());
        assert!(cache.get(A, "third").is_some());
    }

    #[test]
    fn a_disabled_cache_stores_nothing_and_answers_nothing() {
        let cache = DetectionCache::new(0);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 0);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn two_gateways_do_not_agree_on_a_key() {
        // The salt is per process and never persisted, so a digest here names
        // nothing anywhere else — including in a second gateway's memory.
        let first = DetectionCache::new(4);
        let second = DetectionCache::new(4);
        first.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        second.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_ne!(first.key_for("v1", A, "Weber"), second.key_for("v1", A, "Weber"));
    }

    #[test]
    fn a_request_without_a_credential_is_its_own_bucket() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", None, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(None, "Weber").is_some());
        assert!(cache.get(A, "Weber").is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test --quiet detection_cache::`
Expected: FAIL to compile — no `DetectionCache`. (The module is not declared yet either; Step 3 covers both.)

- [ ] **Step 3: Implement the store**

Put this above the test module in `gateway/src/detection_cache.rs`:

```rust
//! Detection results, remembered by digest.
//!
//! Detection runs over every text on every request, and a client resends the
//! whole conversation each turn, so the cost of a conversation grows with the
//! square of its length. Nothing about the rescan is useful: history does not
//! change, and the spans were already computed.
//!
//! Two properties make this safe to add to a gateway whose argument is that
//! personal data lives in exactly one place. Nothing here holds submitted text
//! — keys are digests, values are spans, and a span is a type and two offsets.
//! And a miss is never a refusal: every failure path below degrades to "call
//! the detector", because losing an entry costs time, not correctness. That is
//! the opposite of `SessionStore`, where losing an entry is a confidentiality
//! problem and saturation therefore refuses.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::mapping::Span;

/// A 32-byte digest. Not truncated: a collision on the text digest applies one
/// text's offsets to another, and at equal length `Mapping::mask` accepts them
/// — the wrong ranges are masked and a real value leaves the process. Sixteen
/// more bytes per key is not a trade worth making against that.
type Digest32 = [u8; 32];

#[derive(PartialEq, Eq, Hash)]
struct Key {
    version: Digest32,
    tenant: Digest32,
    text: Digest32,
}

struct Entry {
    spans: Vec<Span>,
    /// Monotonic, from the store's own counter rather than a clock: eviction
    /// needs an order, not a time, and a counter cannot go backwards.
    used: u64,
}

struct Inner {
    entries: HashMap<Key, Entry>,
    clock: u64,
    /// The last version a detector reported. `get` has no other way to know
    /// which version to look under, because the version only ever arrives with
    /// a response.
    known_version: Option<Digest32>,
}

pub struct DetectionCache {
    capacity: usize,
    /// Minted per process and never persisted. The cache must not survive a
    /// restart, so its keys never need to be comparable across runs — and a
    /// per-process salt keeps them from becoming a second stable identifier
    /// for a tenant beside the journal's. Deliberately not the audit salt,
    /// which is on disk precisely so its digests do persist.
    salt: [u8; 32],
    inner: Mutex<Inner>,
}

impl DetectionCache {
    pub fn new(capacity: usize) -> Self {
        let mut salt = [0u8; 32];
        getrandom::getrandom(&mut salt).expect("the OS provides randomness");
        Self {
            capacity,
            salt,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                clock: 0,
                known_version: None,
            }),
        }
    }

    fn digest(&self, bytes: &[u8]) -> Digest32 {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(bytes);
        hasher.finalize().into()
    }

    #[cfg(test)]
    fn key_for(&self, version: &str, credential: Option<&[u8]>, text: &str) -> Digest32 {
        let key = self.key(self.digest(version.as_bytes()), credential, text);
        self.digest(&[key.version, key.tenant, key.text].concat())
    }

    fn key(&self, version: Digest32, credential: Option<&[u8]>, text: &str) -> Key {
        Key {
            version,
            // A request with no credential is its own bucket rather than
            // everyone's: an empty digest is a tenant like any other.
            tenant: self.digest(credential.unwrap_or(b"")),
            text: self.digest(text.as_bytes()),
        }
    }

    pub fn get(&self, credential: Option<&[u8]>, text: &str) -> Option<Vec<Span>> {
        if self.capacity == 0 {
            return None;
        }
        // A poisoned lock means some other request panicked mid-update. That is
        // worth neither failing this request nor propagating: answer "miss".
        let mut inner = self.inner.lock().ok()?;
        let version = inner.known_version?;
        let key = self.key(version, credential, text);
        inner.clock += 1;
        let clock = inner.clock;
        let entry = inner.entries.get_mut(&key)?;
        entry.used = clock;
        Some(entry.spans.clone())
    }

    pub fn insert(
        &self,
        version: &str,
        credential: Option<&[u8]>,
        text: &str,
        spans: &[Span],
    ) {
        if self.capacity == 0 {
            return;
        }
        let version = self.digest(version.as_bytes());
        let key = self.key(version, credential, text);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // A version the store has not seen before makes every entry under the
        // old one unreachable. They are not swept: they simply stop matching
        // and age out through the ceiling, which costs nothing on the path a
        // request is waiting on.
        inner.known_version = Some(version);
        inner.clock += 1;
        let clock = inner.clock;
        if inner.entries.len() >= self.capacity && !inner.entries.contains_key(&key) {
            // One ordered pass, the same shape the session store's eviction
            // scan uses. It runs only when full, and at the default ceiling it
            // is microseconds.
            if let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| Key {
                    version: key.version,
                    tenant: key.tenant,
                    text: key.text,
                })
            {
                inner.entries.remove(&oldest);
            }
        }
        inner.entries.insert(
            key,
            Entry {
                spans: spans.to_vec(),
                used: clock,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|inner| inner.entries.len()).unwrap_or(0)
    }
}
```

Declare the module in `gateway/src/main.rs`, between `mod config;` and `mod detector;` — the existing list is alphabetical and `detection_cache` sorts before `detector`:

```rust
mod detection_cache;
```

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cd gateway && cargo test --quiet detection_cache::`
Expected: PASS, 8 tests.

- [ ] **Step 5: Prove the tests by mutation**

Run each of these one at a time, restoring after each:

1. Drop `tenant` from `Key` (and from `key()`): `another_credential_does_not_see_the_entry` and `a_request_without_a_credential_is_its_own_bucket` FAIL.
2. In `insert`, replace `min_by_key(|(_, entry)| entry.used)` with `max_by_key(...)`: `saturation_evicts_the_least_recently_used` FAILS.
3. In `get`, delete `entry.used = clock;`: `saturation_evicts_the_least_recently_used` FAILS.
4. Replace the random salt with `[0u8; 32]`: `two_gateways_do_not_agree_on_a_key` FAILS.
5. In `get`, replace `let version = inner.known_version?;` with a default of `[0u8; 32]`: `nothing_is_known_before_the_first_insert` FAILS.

Record which mutation broke which test — it goes in the commit message.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/detection_cache.rs gateway/src/main.rs
git commit -m "feat(gateway): a detection cache that holds no text

Keys are three 32-byte digests and values are spans, so the store cannot
hold submitted text even by mistake. The salt is per process and never
persisted: the cache must not survive a restart, and its digests must not
become a second stable name for a tenant beside the journal's.

Saturation evicts rather than refuses, which is the opposite of the
session store and deliberate — losing a cache entry costs time, losing a
session entry costs correctness.

Each test proved by mutation; see the plan for which mutation broke which."
```

---

### Task 5: Wire the cache into the detector client

**Files:**
- Modify: `gateway/src/detector.rs`
- Modify: `gateway/src/proxy.rs`

**Interfaces:**
- Consumes: `DetectionCache` from Task 4, `Detection` from Task 2, `Config::detection_cache_entries` from Task 3.
- Produces: `DetectorClient::new(base_url: String, timeout: Duration, cache_entries: usize) -> Self` and `DetectorClient::detect(&self, text: &str, credential: Option<&[u8]>) -> Result<Vec<Span>, DetectorError>`. `mask_all` gains a `credential: Option<&[u8]>` parameter.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `gateway/src/detector.rs`:

```rust
    #[tokio::test]
    async fn a_repeated_text_does_not_reach_the_detector_twice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
        // `expect(1)` is asserted when the server drops.
    }

    #[tokio::test]
    async fn a_partial_run_is_asked_again_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
    }

    #[tokio::test]
    async fn a_disabled_cache_asks_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 0);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
    }

    #[tokio::test]
    async fn a_failing_detector_is_never_remembered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503).set_body_json(
                serde_json::json!({"detail": "layer(s) ner unavailable: no weights"}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        assert!(client.detect("Weber", credential).await.is_err());
        assert!(client.detect("Weber", credential).await.is_err());
    }
```

Every pre-existing call of `client.detect("...")` in this file needs a second argument; use `None`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test --quiet detector::`
Expected: FAIL to compile — `DetectorClient::new` takes two arguments, `detect` takes one.

- [ ] **Step 3: Implement**

In `gateway/src/detector.rs`, add the field and the constructor parameter:

```rust
pub struct DetectorClient {
    base_url: String,
    client: reqwest::Client,
    cache: crate::detection_cache::DetectionCache,
}

impl DetectorClient {
    pub fn new(base_url: String, timeout: Duration, cache_entries: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builds with a timeout");
        Self {
            base_url,
            client,
            cache: crate::detection_cache::DetectionCache::new(cache_entries),
        }
    }
```

Replace `detect` with the caching wrapper, leaving `detect_full` from Task 2 untouched:

```rust
    /// Every layer the detector has: the gateway does not narrow detection.
    ///
    /// The credential is not used to authenticate anything — the gateway
    /// authenticates nobody — only to keep one tenant's cached results from
    /// answering another's request, which would report through response time
    /// that the two sent the same text.
    pub async fn detect(
        &self,
        text: &str,
        credential: Option<&[u8]>,
    ) -> Result<Vec<Span>, DetectorError> {
        if let Some(spans) = self.cache.get(credential, text) {
            return Ok(spans);
        }
        let detection = self.detect_full(text).await?;
        if let Some(version) = &detection.version {
            self.cache.insert(version, credential, text, &detection.spans);
        }
        Ok(detection.spans)
    }
```

In `gateway/src/proxy.rs`, pass the size through `AppState::from_config`:

```rust
            detector: DetectorClient::new(
                config.detector_url.clone(),
                Duration::from_secs(config.detector_timeout_secs),
                config.detection_cache_entries,
            ),
```

The test harness builds `AppState` by hand and needs the same third argument. In
`state_with` (around line 541):

```rust
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16),
```

Note what this does *not* change: `detector_returning` answers with
`"layers_run": ["deterministic"]`, which is a partial run, so nothing the
existing proxy tests do becomes cacheable and none of their behaviour moves.
Task 7 adds a complete-run mock precisely because of that.

Give `mask_all` the credential and use it:

```rust
async fn mask_all(
    detector: &DetectorClient,
    body: &Value,
    pointers: &[String],
    mapping: &mut Mapping,
    credential: Option<&[u8]>,
) -> Result<(Value, usize, BTreeMap<String, usize>), ProxyError> {
```

and inside the loop:

```rust
        let spans = detector.detect(&text, credential).await?;
```

Update both call sites (around lines 269 and 292) to pass `credential`, which `handle` already holds.

- [ ] **Step 4: Run the whole suite**

Run: `cd gateway && cargo test --quiet`
Expected: PASS. Any other `DetectorClient::new` or `.detect(` in tests needs its new argument.

- [ ] **Step 5: Prove the tests by mutation**

1. In `detect`, delete the `if let Some(spans) = self.cache.get(...)` early return: `a_repeated_text_does_not_reach_the_detector_twice` FAILS on `expect(1)`.
2. Replace `if let Some(version) = &detection.version` with an unconditional insert under a fixed `"v"`: `a_partial_run_is_asked_again_every_time` FAILS on `expect(2)`.

Restore after each.

- [ ] **Step 6: Lint and commit**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets` — expect no warnings.

```bash
git add gateway/src/detector.rs gateway/src/proxy.rs
git commit -m "feat(gateway): serve repeated texts from the detection cache

The cache lives inside DetectorClient rather than beside mask_all, so no
call site can forget it — including the second one that arrives with tool
support.

The credential is threaded down only to separate tenants' entries. The
gateway still authenticates nobody; without the separation, response time
would tell one tenant that another had sent the same document."
```

---

### Task 6: Guard the gateway's copy of the layer list

**Files:**
- Create: `scripts/check_layers.py`
- Modify: `Makefile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `detector::LAYERS` from Task 2, the `Layer` type in `detector/src/tessera_detector/api.py`.

- [ ] **Step 1: Write the script**

Create `scripts/check_layers.py`:

```python
"""Fail when the gateway's layer list and the detector's Layer type disagree.

The gateway decides whether a detection may be cached by asking whether every
layer ran. It holds its own copy of "every layer" for the same reason it holds
its own entity-type vocabulary: asking the detector which layers make a run
complete would be worthless against a detector that answers "the ones I ran".
The copy is only safe while something notices it drifting, and that is this
script.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DETECTOR_RS = ROOT / "gateway" / "src" / "detector.rs"
API_PY = ROOT / "detector" / "src" / "tessera_detector" / "api.py"

GATEWAY = re.compile(r'pub const LAYERS: \[&str; (\d+)\] = \[(.*?)\];', re.DOTALL)
DETECTOR = re.compile(r'^Layer = Literal\[(.*?)\]$', re.MULTILINE | re.DOTALL)
QUOTED = re.compile(r'"([^"]*)"')


def gateway_layers() -> set[str]:
    match = GATEWAY.search(DETECTOR_RS.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{DETECTOR_RS}: no LAYERS declaration found")
    declared, body = int(match.group(1)), match.group(2)
    names = QUOTED.findall(body)
    if len(names) != declared:
        sys.exit(
            f"{DETECTOR_RS}: LAYERS says {declared} entries, {len(names)} parsed"
        )
    return set(names)


def detector_layers() -> set[str]:
    match = DETECTOR.search(API_PY.read_text(encoding="utf-8"))
    if match is None:
        sys.exit(f"{API_PY}: no Layer alias found")
    return set(QUOTED.findall(match.group(1)))


def main() -> None:
    gateway, detector = gateway_layers(), detector_layers()
    if gateway == detector:
        print(f"layers agree: {sorted(gateway)}")
        return
    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"run by the detector, absent from the gateway: {missing}")
        print("  a run using this layer would be treated as complete without it")
    if extra:
        print(f"expected by the gateway, absent from the detector: {extra}")
        print("  no run can ever look complete, so nothing will be cached")
    sys.exit(1)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run it and see it agree**

Run: `python3 scripts/check_layers.py`
Expected: `layers agree: ['deterministic', 'ner']`

- [ ] **Step 3: Prove it by mutation**

Temporarily change `LAYERS` in `gateway/src/detector.rs` to `["deterministic"]` with length 1.
Run: `python3 scripts/check_layers.py`
Expected: exit 1, reporting `ner` as run by the detector and absent from the gateway.
Restore.

- [ ] **Step 4: Add the Makefile target**

In `Makefile`, add `check-layers` to the `.PHONY` line, and the target beside `check-entity-types`:

```makefile
check-layers:
	python3 scripts/check_layers.py
```

Run: `make check-layers`
Expected: agrees.

- [ ] **Step 5: Add it to CI**

In `.github/workflows/ci.yml`, in the `detector` job, immediately after the existing `- run: make -C .. check-entity-types` step, add:

```yaml
      # Guards gateway/src/detector.rs, not the detector — so like the step
      # above, this job must not grow a `paths:` filter.
      - run: make -C .. check-layers
```

- [ ] **Step 6: Commit**

```bash
git add scripts/check_layers.py Makefile .github/workflows/ci.yml
git commit -m "ci: notice when the gateway's layer list drifts

The gateway decides what may be cached by asking whether every layer ran,
from its own copy of the list. A layer added to the detector and not here
would make an incomplete run look complete, and the cache would then hold
results worse than a fresh call — the one thing its design forbids."
```

---

### Task 7: The journal says the same thing for a hit and a miss

**Files:**
- Modify: `gateway/src/proxy.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Add a mock detector whose runs are complete**

The existing `detector_returning` answers `"layers_run": ["deterministic"]`, so
nothing it returns is ever cached — which is why the parity test needs its own
mock. Add beside it in `mod tests`:

```rust
    /// A detector whose runs are complete and identified, so its answers are
    /// eligible for the cache. `detector_returning` deliberately is not: its
    /// partial runs keep every other test in this file cache-free.
    async fn complete_detector_returning(spans: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": spans,
                "layers_run": ["deterministic", "ner"],
                "version": "test-version"
            })))
            .mount(&server)
            .await;
        server
    }
```

- [ ] **Step 2: Write the test**

Add to `mod tests` in `gateway/src/proxy.rs`:

```rust
    #[tokio::test]
    async fn the_journal_says_the_same_for_a_cached_detection() {
        // The evidence layer must not get weaker because an answer came from
        // memory. Two identical requests, the second served from the cache:
        // both masked lines must carry the same counts.
        let detector = complete_detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let body = json!({"messages": [{"role": "user", "content": "Weber schreibt"}]});

        let (first, _) = call(Arc::clone(&state), "/v1/chat/completions", body.clone()).await;
        let (second, _) = call(Arc::clone(&state), "/v1/chat/completions", body).await;
        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::OK);

        let lines = journal(&path);
        let masked: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "masked")
            .collect();
        assert_eq!(masked.len(), 2, "two requests, two masked lines");
        assert_eq!(masked[0]["types"], masked[1]["types"]);
        assert_eq!(masked[0]["spans"], masked[1]["spans"]);
    }
```

`person_span()` is the helper the surrounding tests already use for a PERSON
span; read one of them first and pass whatever it passes. If `call` takes the
state by value, `Arc::clone` as shown; match the existing call sites.

- [ ] **Step 3: Run it and confirm it exercises the cache**

Run: `cd gateway && cargo test --quiet the_journal_says_the_same`
Expected: PASS.

Then confirm the test is not vacuous: temporarily add `.expect(1)` to the mock
in `complete_detector_returning` and re-run. Expected: PASS, proving the second
request never reached the detector. Remove `.expect(1)` afterwards — leaving it
would couple the helper to one test's call count.

- [ ] **Step 4: Prove it by mutation**

Temporarily make `DetectionCache::get` return a truncated list — change
`Some(entry.spans.clone())` to `Some(entry.spans[..0].to_vec())`.
Run: `cd gateway && cargo test --quiet the_journal_says_the_same`
Expected: FAIL — the second masked line reports zero spans where the first
reported one.
Restore.

- [ ] **Step 5: Full verification**

Run from the repository root:

```bash
cd gateway && cargo test --quiet && cargo fmt --check && cargo clippy --all-targets
cd ../detector && uv run pytest -q && uv run ruff check . ../evaluation && uv run mypy src
cd .. && make check-entity-types && make check-layers
```

Expected: everything passes.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/proxy.rs
git commit -m "test(gateway): the journal is identical for a cached detection

Asserted rather than assumed. The counts are computed from spans, and the
spans are the same either way — but 'the evidence did not get weaker' is
exactly the claim a reviewer should not have to take on faith."
```

---

## Self-Review

**Spec coverage.** Every section of the design maps to a task: the version digest and its catalog coverage to Task 1; parsing `version` and `layers_run` and the layer list to Task 2; `detection_cache_entries` and its three config files to Task 3; the key shape, full digests, per-process salt, LRU-by-scan and poisoned-lock degradation to Task 4; the `DetectorClient` seam and credential threading to Task 5; the CI guard promised under "Why a degraded run is dropped" to Task 6; and the journal-parity invariant to Task 7. The seven testing invariants in the spec map to tests in Tasks 3, 4, 5 and 7 — invariant 4 ("a miss is never a refusal") is covered structurally by `get` returning `Option` on a poisoned lock plus `a_failing_detector_is_never_remembered`.

**Out of scope, as the spec says:** `/health` polling, persistence, cross-replica sharing, prefiltering.

**Type consistency.** `DetectorClient::new` takes three arguments from Task 5 onward, and Task 3 supplies the third. `detect` takes `(text, Option<&[u8]>)` from Task 5; Task 2's tests call `detect_full`, which keeps one argument throughout. `Detection.version` is `Option<String>` in Task 2 and is consumed as `&String` in Task 5. `DetectionCache::insert` takes `version: &str` in Task 4 and is called with `version` deref'd from `Option<String>` in Task 5. `Span` is `Clone` already, which `spans.to_vec()` and `entry.spans.clone()` require.

**Known rough edge for the implementer.** Task 7's helpers are named against whatever the audit tests in `proxy.rs` already provide; read that file before writing the test rather than inventing a parallel harness.
