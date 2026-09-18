//! Which callers this gateway serves.
//!
//! The gateway authenticated nobody until this existed, and the cost was not
//! the obvious one. A stranger holding one of your callers' provider keys could
//! reach the mapping table, and a session id is chosen by the client rather than
//! issued — so a guessed id plus a stolen key reads real values back out of a
//! conversation, which going to the provider directly would never have given
//! them. See #91, and #32 for the read itself, which this narrows rather than
//! closes.
//!
//! **The credential is the provider's own header**, already sent by every
//! client because the provider requires it. This adds a fourth role to a value
//! that already carries three — session namespace, audit tenant, detection-cache
//! bucket — rather than a second identity.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

/// How many characters a SHA-256 digest is, written as lowercase hex.
pub const DIGEST_CHARS: usize = 64;

/// The callers this gateway will serve.
///
/// `Anyone` is the behaviour that predates this module and stays the default:
/// a deployment that has not configured the control is not silently given a
/// half of one.
#[derive(Debug, Clone)]
pub enum Callers {
    Anyone,
    /// Digests, never the credentials themselves. A configuration file holding
    /// working provider keys is one whose leak costs money and buys inference
    /// elsewhere; one holding their digests costs nothing.
    ///
    /// **Unsalted, deliberately.** A salt defends a dictionary of low-entropy
    /// secrets from being recognised; a provider API key is not one, and a salt
    /// here would only mean the operator could not compute the value to put in
    /// the file. The audit journal's own `digest` is salted with a
    /// per-deployment secret for a different purpose and is unusable for this.
    Accepted(BTreeSet<String>),
}

impl Callers {
    /// Whether this credential is served.
    ///
    /// No credential is not served once a list exists: absent is not on it.
    ///
    /// **Not constant time, and it does not need to be.** What is compared is a
    /// digest rather than the secret, so what timing could disclose is a prefix
    /// of the digest of a key the caller already holds.
    pub fn admits(&self, credential: Option<&[u8]>) -> bool {
        let accepted = match self {
            Callers::Anyone => return true,
            Callers::Accepted(accepted) => accepted,
        };
        let Some(credential) = credential else {
            return false;
        };
        accepted.contains(&digest_of(credential))
    }

    /// Whether a list was configured at all. The journal records a refusal the
    /// same way either way; this is for the startup line that tells an operator
    /// which of the two deployments they are running.
    pub fn is_closed(&self) -> bool {
        matches!(self, Callers::Accepted(_))
    }
}

/// Lowercase hex SHA-256, which is what `shasum -a 256` prints and so what an
/// operator can produce without this binary.
pub fn digest_of(credential: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(credential).into();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether an entry is the shape this accepts: 64 lowercase hex characters.
///
/// Uppercase is rejected rather than folded. Two spellings of one digest would
/// both work and only one would match what `shasum` prints, so the second
/// spelling is a trap that pays off as "the key I added does nothing".
pub fn is_digest(entry: &str) -> bool {
    entry.len() == DIGEST_CHARS
        && entry
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"sk-ant-secret";

    #[test]
    fn an_open_gateway_admits_a_caller_with_no_credential_at_all() {
        // The behaviour that predates this module, pinned: a deployment that
        // has not configured the control keeps working exactly as it did.
        assert!(Callers::Anyone.admits(Some(KEY)));
        assert!(Callers::Anyone.admits(None));
        assert!(!Callers::Anyone.is_closed());
    }

    #[test]
    fn a_configured_gateway_admits_the_listed_credential_and_nothing_else() {
        let callers = Callers::Accepted(BTreeSet::from([digest_of(KEY)]));
        assert!(callers.admits(Some(KEY)));
        assert!(!callers.admits(Some(b"sk-ant-other")));
        assert!(
            !callers.admits(None),
            "a request with no credential was served by a gateway with a list"
        );
        assert!(callers.is_closed());
    }

    #[test]
    fn the_digest_is_the_one_shasum_prints() {
        // The whole point of unsalted hex: an operator produces the value with
        // a command rather than with this binary. Fixed vector for the empty
        // string, which is NIST's and which any implementation agrees on.
        assert_eq!(
            digest_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(digest_of(KEY).len(), DIGEST_CHARS);
        assert!(is_digest(&digest_of(KEY)));
    }

    #[test]
    fn a_digest_is_rejected_unless_it_is_lowercase_hex_of_the_right_length() {
        let good = digest_of(KEY);
        assert!(is_digest(&good));
        assert!(!is_digest(&good.to_uppercase()), "uppercase was folded");
        assert!(!is_digest(&good[..DIGEST_CHARS - 1]), "short digest passed");
        assert!(!is_digest(&format!("{good}0")), "an over-long entry passed");
        assert!(
            !is_digest("sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "a raw credential passed as a digest"
        );
        // `g` is a letter and not a hex one. `char::is_alphanumeric` would
        // admit it, which is the mistake this spells out to avoid.
        assert!(!is_digest(&format!("g{}", &good[1..])));
    }
}
