//! Which callers this gateway serves.
//!
//! **What this buys, stated precisely, because an earlier version of this
//! comment overclaimed.** It refuses a caller whose credential is not on the
//! list. That is everyone who can reach the port and is not one of yours: no
//! detector time, no session table entry, no relay out through your egress, and
//! a journal that records your traffic rather than a stranger's.
//!
//! **It does not defend against a stolen key that is on the list.** If an
//! attacker holds one of your callers' provider keys, its digest is necessarily
//! accepted, `admits` succeeds, and `session::key_from` namespaces the mapping
//! table by that same key plus a session id the client chooses — so the
//! restoration oracle is exactly as reachable as it was before. The population
//! who can attempt it narrows from "anyone who can reach this port" to "your
//! own callers, and whoever holds one of their keys". That is worth having and
//! it is not the same claim.
//!
//! Closing the stolen-key case needs a credential this gateway issues, separate
//! from the provider's — and then `SessionKey` has to choose which of the two it
//! namespaces by, which is #32's question. See #91 for why that is deliberately
//! not here.
//!
//! **The credential is the provider's own header**, already sent by every
//! client because the provider requires it. This adds a fourth role to a value
//! that already carries three — session namespace, audit tenant, detection-cache
//! bucket — rather than a second identity.
//!

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
    /// **Unsalted, and that rests on an assumption worth naming.** A salt and a
    /// slow hash defend a *guessable* secret; a key issued by OpenAI or
    /// Anthropic is not one, and a salt here would only mean the operator could
    /// not compute the value with `shasum`. The audit journal's own `digest` is
    /// salted with a per-deployment secret for a different purpose and is
    /// unusable for this.
    ///
    /// **The assumption does not hold everywhere this gateway can point.** The
    /// upstream bases are configuration, so a deployment in front of a
    /// self-hosted or OpenAI-compatible model may authenticate with a token the
    /// operator chose — and `secret` or `team-key-2026` falls to a dictionary
    /// in moments if this file leaks. Nothing here can tell the two apart: the
    /// gateway sees a digest at startup and a credential at request time, and
    /// neither says how it was generated.
    ///
    /// So the requirement is on the operator and it is written where they set
    /// the key: if you choose the credential rather than receive it, generate
    /// it randomly. A per-entry salted slow verifier would remove the
    /// requirement at the cost of the `shasum` property; that is #95.
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
