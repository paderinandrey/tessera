//! The audit journal: append-only JSONL evidence that a request was
//! pseudonymized before it reached the provider.
//!
//! Nothing here ever writes a submitted value, a hash of one, an offset or a
//! placeholder name. What it writes is counts, a fixed vocabulary of error
//! classes, and salted digests of the caller's credential and session.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;
use sha2::{Digest as _, Sha256};
use time::format_description::well_known::Rfc3339;

/// What the client is told when the journal cannot be written. It names no
/// path: the filesystem layout is of no use to the caller and of some use to
/// an attacker. The detail goes to `tracing` at the call site.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit unavailable")]
    Unavailable,
}

/// How many bytes of digest reach the journal. `SessionKey::digest` keeps four,
/// which is ample for a debug label on a store that dies with the process and
/// not for a journal read over quarters: at 100 000 distinct sessions some pair
/// of 32-bit digests collides with probability 0.69, and a collision merges two
/// callers into one audit identity — corrupting exactly what the field is for.
const DIGEST_BYTES: usize = 16;

const SALT_BYTES: usize = 32;

/// What a detected type is counted as when its name is not one. Lowercase, so
/// it cannot collide with a real type: the grammar admits capitals and
/// underscores only, and a bucket that a detector could also name would hide
/// the very thing it exists to flag.
const UNVALIDATED_TYPE: &str = "unvalidated";

/// The grammar a type name must match to reach a journal line, shared with the
/// placeholder grammar in `mapping` so the two cannot drift into disagreeing
/// about what a type is.
fn is_entity_type(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= crate::mapping::MAX_ENTITY_TYPE
        && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
}

/// A salted digest, and the only thing `Record::attribute` accepts.
///
/// The inner string is private and `Audit::digest` is the sole constructor, so
/// a raw credential or a client-chosen session id cannot reach a journal line
/// by being handed to the wrong parameter. Both call sites already passed a
/// digest; this is what makes that a property of the type rather than a fact
/// about the two call sites as they happen to read today.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Digest(String);

/// Where a journal line goes. A trait rather than a bare `File` because the
/// guarantee this slice exists for — a journal that cannot be written refuses
/// the request — is not provokable through the filesystem: an already-open
/// descriptor survives a read-only directory, a chmod and an unlink. A sink
/// that fails on demand is the only honest way to test it.
pub(crate) trait Sink: Send {
    fn write_line(&mut self, line: &str) -> std::io::Result<()>;
    fn sync(&mut self) -> std::io::Result<()>;
}

impl Sink for File {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        // One `write_all` for the line and its terminator together: two calls
        // would let a failure between them leave a line with no newline, and
        // the next record would append to it.
        self.write_all(format!("{line}\n").as_bytes())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_all()
    }
}

pub struct Audit {
    /// A mutex rather than a bet on `O_APPEND` atomicity: a record with a long
    /// `types` map can exceed the size at which that atomicity holds, and
    /// interleaved lines in an evidence base are worse than a lock held for
    /// microseconds.
    sink: Mutex<Box<dyn Sink>>,
    salt: [u8; SALT_BYTES],
}

impl Audit {
    /// Open the journal for appending and load the salt beside it, creating
    /// either if absent. Returns an error rather than degrading: a gateway
    /// that cannot write evidence does not start.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        // A newly created file's directory entry is what a machine failure
        // would lose, and the entry is not covered by fsyncing the file.
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            File::open(parent)?.sync_all()?;
        }
        // Emptiness by length, not by existence: the line above just created
        // the file, so by here it always exists. An empty journal is the one
        // state in which minting a salt renumbers nothing.
        let journal_is_empty = file.metadata()?.len() == 0;
        let salt = Self::load_salt(&salt_path(path), journal_is_empty)?;
        Ok(Self {
            sink: Mutex::new(Box::new(file)),
            salt,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_sink(sink: Box<dyn Sink>, salt: [u8; SALT_BYTES]) -> Self {
        Self {
            sink: Mutex::new(sink),
            salt,
        }
    }

    /// The salt beside the journal, minted on a first run and reused after.
    ///
    /// A salt that exists but is not exactly 32 bytes refuses, and so does a
    /// salt that is *absent* beside a journal that already has lines: the
    /// consequence is the same either way — every `tenant` written from here
    /// on disagrees with every line above it, with no marker at the boundary —
    /// so a partial restore that lost only the salt must not look like a first
    /// run. `journal_is_empty` is what tells the two apart, and it is also what
    /// keeps external rotation working: a journal moved aside leaves an empty
    /// one beside the kept salt, and the digests carry on unchanged.
    fn load_salt(path: &Path, journal_is_empty: bool) -> std::io::Result<[u8; SALT_BYTES]> {
        let mut salt = [0u8; SALT_BYTES];
        match File::open(path) {
            Ok(mut file) => {
                // `read_exact` also rejects a longer file's tail being ignored:
                // anything but exactly the right size is a file we did not write.
                file.read_exact(&mut salt)?;
                if file.read(&mut [0u8; 1])? != 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "the audit salt file is longer than 32 bytes",
                    ));
                }
                Ok(salt)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !journal_is_empty {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "the audit salt file is missing beside a journal that already has lines; \
                         restore it, or move the journal aside to start a new one",
                    ));
                }
                getrandom::getrandom(&mut salt)
                    .map_err(|_| std::io::Error::other("the OS must provide randomness"))?;
                let mut options = OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(path)?;
                file.write_all(&salt)?;
                file.sync_all()?;
                Ok(salt)
            }
            Err(error) => Err(error),
        }
    }

    /// A salted digest of the given parts, 32 lowercase hex characters.
    ///
    /// Parts are length-prefixed so that `["ab", "c"]` and `["a", "bc"]` do not
    /// collide: a tenant whose credential ends where another's session id
    /// begins would otherwise share an identity.
    pub fn digest(&self, parts: &[&[u8]]) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        Digest(
            digest[..DIGEST_BYTES]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    /// Append one line. `sync` is true only for the `masked` record, which must
    /// be durable before the upstream call; the outcome line does not fsync,
    /// because by the time it is written there is nothing left to refuse.
    pub(crate) fn append(&self, line: &str, sync: bool) -> std::io::Result<()> {
        // A thread that panicked mid-write is not reachable: a line is one
        // `write_all` under this lock. Recovering costs less than refusing to
        // serve because some other thread panicked.
        let mut sink = self.sink.lock().unwrap_or_else(|error| {
            tracing::warn!("the audit lock was poisoned; recovering");
            error.into_inner()
        });
        sink.write_line(line)?;
        if sync {
            sink.sync()?;
        }
        Ok(())
    }
}

/// A handle to one request's audit state. Cloning is cheap and the `outcome`
/// line is written when the **last** handle drops — which is the `openai` /
/// `anthropic` wrapper on the buffered path and the stream on the streamed one.
///
/// The guard guarantees that a line is written. It does not guess what the line
/// says: every ending signals what happened, and an ending that signals nothing
/// is recorded as `aborted` rather than assumed to be a success.
#[derive(Clone)]
pub struct Record(Arc<Inner>);

struct Inner {
    audit: Arc<Audit>,
    id: String,
    started: Instant,
    provider: &'static str,
    route: &'static str,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    tenant: Option<Digest>,
    session: Option<Digest>,
    stream: bool,
    texts: usize,
    spans: usize,
    types: BTreeMap<String, usize>,
    upstream: bool,
    outcome: Option<(&'static str, u16, Option<&'static str>)>,
}

impl Record {
    pub fn new(audit: Arc<Audit>, provider: &'static str, route: &'static str) -> Self {
        Self(Arc::new(Inner {
            audit,
            // Eight random bytes rather than a hash of anything in the request:
            // an id derived from content would be a fingerprint of content.
            id: random_id(),
            started: Instant::now(),
            provider,
            route,
            state: Mutex::new(State::default()),
        }))
    }

    fn with<R>(&self, edit: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        edit(&mut state)
    }

    /// The caller's salted digests. `session` is `None` when the request asked
    /// for no session; `tenant` is always present, because it is a digest of
    /// the credential alone.
    pub fn attribute(&self, tenant: Digest, session: Option<Digest>) {
        self.with(|state| {
            state.tenant = Some(tenant);
            state.session = session;
        });
    }

    /// What detection found in *this request's* texts: distinct values per
    /// type, and occurrences in total. Never the values.
    ///
    /// The keys arrive from the detector's `span.entity_type`, an unrestricted
    /// string on the wire, and this check is the only thing standing between
    /// that string and a journal line. It is load-bearing on the ordinary
    /// path, not a guard against a state the gateway cannot reach:
    /// `Mapping::placeholder_for` returns the cached placeholder on a
    /// `by_value` hit *before* it validates the type, so every span over a
    /// value that is already mapped — a repeat within one text, or anything
    /// seeded from an earlier turn of a session — passes the mapping with its
    /// type unexamined and arrives here intact. A 200-OK request is enough to
    /// produce one.
    ///
    /// So a key that is not a legible type name is counted under
    /// `UNVALIDATED_TYPE` rather than copied into the evidence. Deleting this
    /// loop puts detector-controlled text into the journal on a successful
    /// request; `a_repeated_value_carries_its_type_past_the_mapping_unvalidated`
    /// in `proxy.rs` is that request.
    pub fn detected(&self, texts: usize, spans: usize, types: BTreeMap<String, usize>) {
        let mut checked: BTreeMap<String, usize> = BTreeMap::new();
        let mut unvalidated = 0usize;
        for (name, count) in types {
            let key = if is_entity_type(&name) {
                name
            } else {
                unvalidated += 1;
                UNVALIDATED_TYPE.to_owned()
            };
            *checked.entry(key).or_default() += count;
        }
        if unvalidated > 0 {
            // The fact, never the key: the key is the untrusted string this
            // function exists to keep out of the evidence, and a log is not a
            // safer place for it than the journal. A detector and a mapping
            // that disagree about what a type is should not wait for an audit
            // to be noticed.
            tracing::warn!(
                count = unvalidated,
                "the detector reported entity types outside the placeholder grammar; \
                 they are counted as unvalidated rather than named"
            );
        }
        self.with(|state| {
            state.texts = texts;
            state.spans = spans;
            state.types = checked;
        });
    }

    pub fn streaming(&self) {
        self.with(|state| state.stream = true);
    }

    /// Write the `masked` line and wait for it to reach the disk. Called once,
    /// immediately before the upstream request is sent: this is the ordering
    /// the whole journal exists for.
    pub async fn masked(&self) -> Result<(), AuditError> {
        let line = self.with(|state| {
            json!({
                "ts": now(),
                "event": "masked",
                "request": self.0.id,
                "provider": self.0.provider,
                "route": self.0.route,
                "tenant": state.tenant,
                "session": state.session,
                "stream": state.stream,
                "texts": state.texts,
                "spans": state.spans,
                "types": state.types,
            })
            .to_string()
        });
        let audit = Arc::clone(&self.0.audit);
        // fsync on a runtime worker would park it for the length of a disk
        // round-trip; every other request on that worker waits behind it.
        tokio::task::spawn_blocking(move || audit.append(&line, true))
            .await
            .map_err(|error| {
                tracing::error!(%error, "the audit writer task failed");
                AuditError::Unavailable
            })?
            .map_err(|error| {
                // The path and the errno stay here. The client is told only
                // that the journal is unavailable.
                tracing::error!(%error, "could not write the audit record");
                AuditError::Unavailable
            })?;
        // Only a written line makes the claim true: an outcome recorded after
        // a failed write here must still say the bytes never left.
        self.with(|state| state.upstream = true);
        Ok(())
    }

    /// Withdraw the claim `masked` made: the provider was never reached, so
    /// nothing this request carried left the perimeter.
    ///
    /// `masked` sets `upstream` before the call is made rather than after it
    /// returns, and deliberately: a request that fails mid-flight did send its
    /// bytes, and a journal that said otherwise would under-report exactly what
    /// it exists to report. Over-reporting is the safe direction, so `true`
    /// stands unless the opposite is *definitively* knowable — which it is only
    /// when no connection was ever established. `reqwest::Error::is_connect` is
    /// that case, and in this version it covers a refused connection and a
    /// failed DNS lookup alike; a timeout or a broken pipe is not it.
    pub fn did_not_reach_upstream(&self) {
        self.with(|state| state.upstream = false);
    }

    /// The gateway produced a whole response and handed it to axum. Not "the
    /// client received it": whether the connection survived afterwards is not
    /// something this gateway observes, and the record claims only what it saw.
    pub fn completed(&self, status: u16) {
        self.outcome("completed", status, None);
    }

    pub fn refused(&self, status: u16, error: &'static str) {
        self.outcome("refused", status, Some(error));
    }

    /// Bytes had already gone out, so the stream ended mid-flight. The status
    /// is the one the client already received.
    ///
    /// Hardcoded rather than carried from the upstream response, and accurate
    /// rather than lucky: `restore_stream` builds a fresh `Response::new(...)`
    /// for the body it streams, so the client's status on this path is always
    /// 200 whatever the provider answered. There is no other value it could
    /// have had by the time a stream can fail.
    pub fn stream_failed(&self, error: &'static str) {
        self.outcome("stream_failed", 200, Some(error));
    }

    fn outcome(&self, result: &'static str, status: u16, error: Option<&'static str>) {
        self.with(|state| state.outcome = Some((result, status, error)));
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        // No signal means the request ended in a way none of the exits reach —
        // in practice a client that disconnected mid-stream.
        let (result, status, error) = state.outcome.unwrap_or(("aborted", 0, None));
        // `tenant` and `session` are repeated from the `masked` line rather
        // than left to a join, because a request refused before `masked` has
        // no `masked` line to join to: without them, the one line a refusal
        // produces cannot say whose request it was, which is the first field
        // anyone investigating a run of refusals asks for. The redundancy on
        // the two-line case costs a few bytes.
        let line = json!({
            "ts": now(),
            "event": "outcome",
            "request": self.id,
            "tenant": state.tenant,
            "session": state.session,
            "upstream": state.upstream,
            "status": status,
            "result": result,
            "error": error,
            "ms": self.started.elapsed().as_millis() as u64,
        })
        .to_string();
        // Nothing left to refuse: the request already happened. A full disk
        // refuses the *next* request at `masked`, so the journal stops the
        // gateway rather than quietly shedding records.
        if let Err(error) = self.audit.append(&line, false) {
            tracing::error!(%error, request = %self.id, "could not write the audit outcome");
        }
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|error| {
            // A 1970 timestamp in an evidence file is a worse artifact than a
            // line in the log beside it: without this, a reader would have to
            // guess whether the epoch means a clock problem or a formatting
            // one, and nothing anywhere would say which.
            tracing::error!(%error, "could not format the audit timestamp");
            "1970-01-01T00:00:00Z".to_owned()
        })
}

fn random_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("the OS must provide randomness");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn salt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".salt");
    PathBuf::from(name)
}

/// A journal whose every write fails. It lives at module scope, not inside the
/// test module, because `proxy.rs` needs it too: the end-to-end refusal is what
/// proves the guarantee, and a sink defined inside `mod tests` would not be
/// reachable from there.
#[cfg(test)]
pub(crate) fn failing_audit_for_tests() -> Audit {
    struct FailingSink;

    impl Sink for FailingSink {
        fn write_line(&mut self, _line: &str) -> std::io::Result<()> {
            Err(std::io::Error::other("the disk is full"))
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("the disk is full"))
        }
    }

    Audit::with_sink(Box::new(FailingSink), [7u8; SALT_BYTES])
}

/// A journal whose lines reach the sink but are never confirmed durable:
/// `write_line` succeeds while `sync` always fails. This is what an fsync
/// failure on an otherwise healthy filesystem looks like, unlike
/// `failing_audit_for_tests` above, where nothing is ever written at all —
/// and it is the case that must not let `Record::masked` claim success.
#[cfg(test)]
pub(crate) fn sync_failing_audit_for_tests(path: &Path) -> Audit {
    struct SyncFailingSink(File);

    impl Sink for SyncFailingSink {
        fn write_line(&mut self, line: &str) -> std::io::Result<()> {
            Sink::write_line(&mut self.0, line)
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("fsync failed"))
        }
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("opens");
    Audit::with_sink(Box::new(SyncFailingSink(file)), [7u8; SALT_BYTES])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_creates_the_journal_and_a_private_salt() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).expect("opens");

        assert!(path.exists(), "the journal is created on open");
        let salt = dir.path().join("audit.jsonl.salt");
        assert!(salt.exists(), "the salt is created beside it");
        assert_eq!(
            std::fs::read(&salt).expect("readable").len(),
            32,
            "the salt is 32 bytes"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&salt).expect("stat").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the salt is readable by its owner only"
            );
        }
        drop(audit);
    }

    #[test]
    fn a_digest_is_thirty_two_hex_characters() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let audit = Audit::open(&dir.path().join("audit.jsonl")).expect("opens");
        let digest = audit.digest(&[b"sk-secret"]);
        assert_eq!(digest.0.len(), 32);
        assert!(digest
            .0
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn a_digest_survives_a_restart_and_separates_its_parts() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");

        let first = Audit::open(&path).expect("opens");
        let tenant = first.digest(&[b"sk-secret"]);
        let session = first.digest(&[b"sk-secret", b"chat-1"]);
        drop(first);

        // The whole point of the salt file: a journal read after a restart
        // must attribute yesterday's requests to today's tenant.
        let second = Audit::open(&path).expect("reopens");
        assert_eq!(second.digest(&[b"sk-secret"]), tenant);
        assert_ne!(tenant, session, "a tenant is not its own session");

        // Length-prefixed parts, so ("ab","c") and ("a","bc") differ.
        assert_ne!(
            second.digest(&[b"ab", b"c"]),
            second.digest(&[b"a", b"bc"]),
            "concatenation must not be ambiguous"
        );
    }

    #[test]
    fn two_audits_on_two_paths_do_not_share_a_salt() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let one = Audit::open(&dir.path().join("one.jsonl")).expect("opens");
        let two = Audit::open(&dir.path().join("two.jsonl")).expect("opens");
        assert_ne!(one.digest(&[b"sk-secret"]), two.digest(&[b"sk-secret"]));
    }

    #[test]
    fn a_truncated_salt_refuses_to_open() {
        // Regenerating silently would renumber every tenant in the middle of
        // the journal, which is worse than not starting.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(dir.path().join("audit.jsonl.salt"), b"short").expect("writes");
        assert!(Audit::open(&path).is_err());
    }

    #[test]
    fn an_oversized_salt_refuses_to_open() {
        // Thirty-three bytes is not a file we wrote, and reading the first 32
        // of it would attribute the journal with a salt nobody chose.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(dir.path().join("audit.jsonl.salt"), [0u8; SALT_BYTES + 1]).expect("writes");
        assert!(Audit::open(&path).is_err());
    }

    #[test]
    fn a_lost_salt_beside_a_written_journal_refuses_to_open() {
        // The asymmetry this closes: a truncated salt refused already, while
        // an absent one silently minted a replacement — same consequence,
        // every tenant below the boundary renumbered and nothing marking it.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        Audit::open(&path)
            .expect("opens")
            .append(r#"{"event":"outcome"}"#, true)
            .expect("appends");
        std::fs::remove_file(dir.path().join("audit.jsonl.salt")).expect("removes");

        let message = match Audit::open(&path) {
            Ok(_) => panic!("a journal that renumbers its tenants must not start"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("restore") && message.contains("move the journal aside"),
            "the operator is told both remedies: {message}"
        );
    }

    #[test]
    fn a_lost_salt_beside_an_empty_journal_is_an_ordinary_first_run() {
        // Nothing has been attributed yet, so there is nothing to disagree
        // with. This is every first run, and it must not need a salt to exist.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        Audit::open(&path).expect("a first run mints its own salt");
        std::fs::remove_file(dir.path().join("audit.jsonl.salt")).expect("removes");

        Audit::open(&path).expect("an empty journal is still a first run");
        assert_eq!(
            std::fs::read(dir.path().join("audit.jsonl.salt"))
                .expect("readable")
                .len(),
            SALT_BYTES
        );
    }

    #[test]
    fn external_rotation_keeps_the_salt_and_so_keeps_the_digests() {
        // Rotation is out of scope and therefore external: the operator moves
        // the journal aside around a restart and keeps the salt. That leaves
        // an empty journal beside a present salt, which must start normally
        // and go on attributing the same credential to the same tenant —
        // otherwise the rotation the design relies on splits the identities it
        // was supposed to preserve.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let first = Audit::open(&path).expect("opens");
        first
            .append(r#"{"event":"outcome"}"#, true)
            .expect("appends");
        let tenant = first.digest(&[b"sk-secret"]);
        drop(first);

        std::fs::rename(&path, dir.path().join("audit.jsonl.1")).expect("rotates");

        let second = Audit::open(&path).expect("a rotated journal starts normally");
        assert_eq!(
            second.digest(&[b"sk-secret"]),
            tenant,
            "rotation must not renumber the tenants it carries across"
        );
    }

    #[test]
    fn a_journal_that_cannot_be_opened_is_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("no-such-directory").join("audit.jsonl");
        assert!(Audit::open(&path).is_err());
    }

    #[test]
    fn a_sink_that_cannot_write_surfaces_the_error() {
        assert!(failing_audit_for_tests()
            .append(r#"{"a":1}"#, true)
            .is_err());
    }

    #[test]
    fn appending_writes_one_line_per_call() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).expect("opens");
        audit.append(r#"{"a":1}"#, true).expect("appends");
        audit.append(r#"{"b":2}"#, false).expect("appends");

        let text = std::fs::read_to_string(&path).expect("readable");
        assert_eq!(text, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn reopening_appends_rather_than_truncates() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        Audit::open(&path)
            .expect("opens")
            .append(r#"{"a":1}"#, true)
            .expect("appends");
        Audit::open(&path)
            .expect("reopens")
            .append(r#"{"b":2}"#, true)
            .expect("appends");

        let text = std::fs::read_to_string(&path).expect("readable");
        assert_eq!(text.lines().count(), 2, "a restart must not erase evidence");
    }

    use std::sync::Arc;

    fn lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("readable")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect()
    }

    fn fixture() -> (tempfile::TempDir, Arc<Audit>, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(Audit::open(&path).expect("opens"));
        (dir, audit, path)
    }

    fn types(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect()
    }

    #[tokio::test]
    async fn a_masked_record_names_what_was_found() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "anthropic", "/v1/messages");
        let tenant = Digest("a41f9c02".repeat(4));
        let session = Digest("3bd7e105".repeat(4));
        record.attribute(tenant, Some(session));
        record.detected(4, 9, types(&[("PERSON", 2), ("IBAN", 1)]));
        record.streaming();
        record.masked().await.expect("writes");
        record.completed(200);
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "masked");
        assert_eq!(lines[0]["provider"], "anthropic");
        assert_eq!(lines[0]["route"], "/v1/messages");
        assert_eq!(lines[0]["stream"], true);
        assert_eq!(lines[0]["texts"], 4);
        assert_eq!(lines[0]["spans"], 9);
        assert_eq!(lines[0]["types"]["PERSON"], 2);
        assert_eq!(lines[0]["session"], "3bd7e105".repeat(4));
        assert!(lines[0]["ts"].as_str().expect("a timestamp").ends_with('Z'));
    }

    #[tokio::test]
    async fn a_type_name_that_is_not_one_never_reaches_a_line() {
        // The keys come from the detector's `entity_type`, an unrestricted
        // string on the wire. A detector that echoed the text it found into
        // that field would put submitted text in the evidence — which is the
        // one thing the journal may never contain — and this module must not
        // depend on `mapping.mask` having refused it two calls earlier.
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "openai", "/v1/chat/completions");
        record.detected(
            1,
            4,
            types(&[
                ("PERSON", 1),
                ("Weber, Hauptstrasse 4", 1),
                ("person", 1),
                (&"A".repeat(crate::mapping::MAX_ENTITY_TYPE + 1), 1),
            ]),
        );
        record.masked().await.expect("writes");
        record.completed(200);
        drop(record);

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            !text.contains("Weber"),
            "a detector's type name reached the journal verbatim: {text}"
        );

        let lines = lines(&path);
        assert_eq!(lines[0]["types"]["PERSON"], 1, "a legible type is kept");
        assert_eq!(
            lines[0]["types"][UNVALIDATED_TYPE], 3,
            "the three illegible names are counted, not quoted"
        );
        assert!(
            !UNVALIDATED_TYPE.chars().any(|c| c.is_ascii_uppercase()),
            "the bucket must be a name no detector's type could also be"
        );
    }

    #[tokio::test]
    async fn the_two_records_share_one_request_id() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "openai", "/v1/chat/completions");
        record.masked().await.expect("writes");
        record.completed(200);
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines[0]["request"], lines[1]["request"]);
        assert_eq!(lines[1]["event"], "outcome");
        assert_eq!(lines[1]["result"], "completed");
        assert_eq!(lines[1]["upstream"], true);
        assert_eq!(lines[1]["status"], 200);
        assert!(lines[1]["error"].is_null());
    }

    #[tokio::test]
    async fn two_records_do_not_share_a_request_id() {
        let (_dir, audit, path) = fixture();
        for _ in 0..2 {
            let record = Record::new(Arc::clone(&audit), "openai", "/v1/chat/completions");
            record.completed(200);
        }
        let lines = lines(&path);
        assert_ne!(lines[0]["request"], lines[1]["request"]);
    }

    #[tokio::test]
    async fn a_refusal_before_the_upstream_call_is_one_line() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "openai", "/v1/chat/completions");
        record.attribute(
            Digest("a41f9c02".repeat(4)),
            Some(Digest("3bd7e105".repeat(4))),
        );
        record.refused(502, "detector_timeout");
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines.len(), 1, "nothing was sent, so nothing was masked");
        assert_eq!(lines[0]["event"], "outcome");
        // The one line is the whole record, so it carries the attribution
        // there is no `masked` line to join to for.
        assert_eq!(lines[0]["tenant"], "a41f9c02".repeat(4));
        assert_eq!(lines[0]["session"], "3bd7e105".repeat(4));
        assert_eq!(
            lines[0]["upstream"], false,
            "the central question, answerable without a join"
        );
        assert_eq!(lines[0]["result"], "refused");
        assert_eq!(lines[0]["error"], "detector_timeout");
        assert_eq!(lines[0]["status"], 502);
    }

    #[tokio::test]
    async fn a_refusal_after_the_upstream_call_still_says_bytes_left() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "openai", "/v1/chat/completions");
        record.masked().await.expect("writes");
        record.refused(502, "mapping_unknown_placeholder");
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines[1]["result"], "refused");
        assert_eq!(lines[1]["upstream"], true);
    }

    #[tokio::test]
    async fn an_unsignalled_drop_is_aborted_rather_than_completed() {
        // A guard that assumed success on an unsignalled drop would report
        // `completed` for every abandoned stream.
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "anthropic", "/v1/messages");
        record.masked().await.expect("writes");
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines[1]["result"], "aborted");
        assert!(lines[1]["error"].is_null());
    }

    #[tokio::test]
    async fn a_stream_failure_keeps_the_status_the_client_already_got() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "anthropic", "/v1/messages");
        record.masked().await.expect("writes");
        record.stream_failed("stream_unrestorable");
        drop(record);

        let lines = lines(&path);
        assert_eq!(lines[1]["result"], "stream_failed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[1]["error"], "stream_unrestorable");
    }

    #[tokio::test]
    async fn the_outcome_waits_for_the_last_handle() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "anthropic", "/v1/messages");
        record.masked().await.expect("writes");
        let carried = record.clone();
        record.completed(200);
        drop(record);
        assert_eq!(lines(&path).len(), 1, "the stream still holds a handle");

        carried.stream_failed("stream_broken");
        drop(carried);
        let lines = lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["result"], "stream_failed", "the last signal wins");
    }

    #[tokio::test]
    async fn only_one_outcome_is_written_however_many_handles() {
        let (_dir, audit, path) = fixture();
        let record = Record::new(audit, "openai", "/v1/chat/completions");
        let clones: Vec<Record> = (0..5).map(|_| record.clone()).collect();
        record.completed(200);
        drop(record);
        drop(clones);
        assert_eq!(lines(&path).len(), 1);
    }

    #[tokio::test]
    async fn a_failed_masked_write_refuses_rather_than_returning_ok() {
        // The guarantee the slice exists for. If this returned Ok, an
        // unrecorded request would reach the provider.
        let record = Record::new(
            Arc::new(failing_audit_for_tests()),
            "openai",
            "/v1/chat/completions",
        );
        let error = record
            .masked()
            .await
            .expect_err("a journal that cannot be written refuses");
        assert_eq!(error.to_string(), "audit unavailable");
        assert!(
            !error.to_string().contains('/'),
            "the client is told nothing about the filesystem"
        );
    }

    #[tokio::test]
    async fn a_masked_write_that_fails_to_sync_leaves_upstream_false() {
        // `upstream` states whether bytes left the perimeter. A `masked` call
        // that returns `Err` must not let a later outcome line claim they did,
        // even though the line was queued before the fsync failed.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let record = Record::new(
            Arc::new(sync_failing_audit_for_tests(&path)),
            "openai",
            "/v1/chat/completions",
        );
        record
            .masked()
            .await
            .expect_err("a journal that cannot confirm durability refuses");
        drop(record);

        let lines = lines(&path);
        let outcome = lines.last().expect("the drop wrote something");
        assert_eq!(outcome["event"], "outcome");
        assert_eq!(
            outcome["upstream"], false,
            "the masked write was never confirmed durable"
        );
    }

    #[tokio::test]
    async fn a_failed_outcome_write_does_not_panic() {
        // Nothing left to refuse: the request already happened. Dropping the
        // record must not take the process down with it.
        let record = Record::new(
            Arc::new(failing_audit_for_tests()),
            "openai",
            "/v1/chat/completions",
        );
        record.completed(200);
        drop(record);
    }
}
