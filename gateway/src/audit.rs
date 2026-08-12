//! The audit journal: append-only JSONL evidence that a request was
//! pseudonymized before it reached the provider.
//!
//! Nothing here ever writes a submitted value, a hash of one, an offset or a
//! placeholder name. What it writes is counts, a fixed vocabulary of error
//! classes, and salted digests of the caller's credential and session.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// What the client is told when the journal cannot be written. It names no
/// path: the filesystem layout is of no use to the caller and of some use to
/// an attacker. The detail goes to `tracing` at the call site.
// Not yet raised: the per-request `Record` guard that returns it is Task 3.
#[allow(dead_code)]
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

// Not yet constructed outside tests: `main.rs` opens the journal in Task 4.
#[allow(dead_code)]
pub struct Audit {
    /// A mutex rather than a bet on `O_APPEND` atomicity: a record with a long
    /// `types` map can exceed the size at which that atomicity holds, and
    /// interleaved lines in an evidence base are worse than a lock held for
    /// microseconds.
    sink: Mutex<Box<dyn Sink>>,
    salt: [u8; SALT_BYTES],
}

// Not yet called outside tests: `main.rs` wires these in Task 4.
#[allow(dead_code)]
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
        let salt = Self::load_salt(&salt_path(path))?;
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

    fn load_salt(path: &Path) -> std::io::Result<[u8; SALT_BYTES]> {
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
    pub fn digest(&self, parts: &[&[u8]]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        digest[..DIGEST_BYTES]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
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
        assert_eq!(digest.len(), 32);
        assert!(digest
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
}
