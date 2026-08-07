//! Session identity and the store that holds one table per conversation.
//!
//! A session table is a restoration oracle: a caller who writes a placeholder
//! into a prompt gets it echoed by the model, and the gateway restores it on
//! the way back. So a client-chosen id never selects a session on its own — it
//! is namespaced by a fingerprint of the caller's own credential.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::provider::Provider;

/// The header a client uses to say two requests belong to one conversation.
#[allow(dead_code)]
pub const SESSION_HEADER: &str = "x-tessera-session";

/// A client-chosen id becomes part of a map key and, hashed, part of a log
/// line. Bounding it keeps both from having to cope with arbitrary input.
#[allow(dead_code)]
const MAX_SESSION_ID: usize = 128;

#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "session id must be 1 to 128 characters of A-Z a-z 0-9 . _ : -; the request is \
         refused rather than served under an id that was quietly altered"
    )]
    BadId,
    #[error(
        "sessions are disabled (session_idle_secs = 0) but the request asked for one; the \
         request is refused rather than served without the coreference it asked for"
    )]
    Disabled,
    #[error(
        "a session needs the {0} header to namespace it; the request is refused rather \
         than putting every caller who sent no credential into one shared session"
    )]
    NoCredential(&'static str),
}

/// Random per process: the store must hold nothing from which a credential can
/// be recovered. Sessions do not survive a restart in any case — the store is
/// in memory — so a salt that does not either costs nothing.
#[allow(dead_code)]
fn salt() -> &'static [u8; 32] {
    static SALT: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    SALT.get_or_init(|| {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("the OS must provide randomness");
        bytes
    })
}

/// What the store keys on: a salted fingerprint of the caller's credential,
/// and the id the caller chose.
#[allow(dead_code)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    credential: [u8; 32],
    id: String,
}

// `derive(Debug)` would print `id` verbatim, and `id` is client-chosen: it may
// itself be personal data, the same as the raw id `digest()` exists to keep
// out of logs. A type with a safe printable form and an unsafe one only
// needs one caller to reach into the wrong one, so there is exactly one
// `Debug` implementation and it prints only the digest.
impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionKey").field(&self.digest()).finish()
    }
}

impl SessionKey {
    #[allow(dead_code)]
    fn new(credential: &[u8], id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(salt());
        hasher.update(credential);
        Self {
            credential: hasher.finalize().into(),
            id: id.to_owned(),
        }
    }

    /// The only form of a key that may reach a log. The raw id is chosen by the
    /// client, so it may itself be personal data: `patient-Weber-2026` is a
    /// plausible id and an unacceptable log line.
    #[allow(dead_code)]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.credential);
        hasher.update(self.id.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[allow(dead_code)]
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Which session a request asks for. `Ok(None)` means it asked for none, which
/// is the behaviour that predates sessions: a mapping scoped to the request.
///
/// Every refusal here happens before detection, so a malformed header costs
/// nothing rather than a second per 1 200 characters.
#[allow(dead_code)]
pub fn key_from(
    headers: &HeaderMap,
    provider: &dyn Provider,
    enabled: bool,
) -> Result<Option<SessionKey>, SessionError> {
    let Some(raw) = headers.get(SESSION_HEADER) else {
        return Ok(None);
    };
    let id = raw.to_str().map_err(|_| SessionError::BadId)?;
    if !valid_id(id) {
        return Err(SessionError::BadId);
    }
    if !enabled {
        return Err(SessionError::Disabled);
    }
    let name = provider.credential_header();
    let credential = headers
        .get(name)
        .map(|value| value.as_bytes())
        .filter(|value| !value.is_empty())
        .ok_or(SessionError::NoCredential(name))?;
    Ok(Some(SessionKey::new(credential, id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::OpenAi;
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                // from_bytes, not parse: a header value may legally carry bytes
                // that are not UTF-8, and refusing those is `key_from`'s job
                // rather than the helper's.
                axum::http::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn no_header_asks_for_no_session() {
        let asked = key_from(&headers(&[("authorization", "Bearer k")]), &OpenAi, true);
        assert!(matches!(asked, Ok(None)));
    }

    #[test]
    fn an_id_outside_the_grammar_is_refused() {
        for id in ["", "conv 1", "conv/1", &"c".repeat(129)] {
            let asked = key_from(
                &headers(&[("authorization", "Bearer k"), (SESSION_HEADER, id)]),
                &OpenAi,
                true,
            );
            assert!(
                matches!(asked, Err(SessionError::BadId)),
                "accepted the id {id:?}"
            );
        }
    }

    #[test]
    fn an_id_that_is_not_text_is_refused() {
        // A header value may carry arbitrary bytes. `to_str` is what catches
        // this, before the grammar ever runs.
        let mut raw = HeaderMap::new();
        raw.insert("authorization", "Bearer k".parse().unwrap());
        raw.insert(
            axum::http::HeaderName::from_static(SESSION_HEADER),
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(matches!(
            key_from(&raw, &OpenAi, true),
            Err(SessionError::BadId)
        ));
    }

    #[test]
    fn the_grammar_accepts_what_a_client_actually_uses() {
        let asked = key_from(
            &headers(&[
                ("authorization", "Bearer k"),
                (SESSION_HEADER, "conv:2026-08-07_ab.12-x"),
            ]),
            &OpenAi,
            true,
        );
        assert!(matches!(asked, Ok(Some(_))));
    }

    #[test]
    fn asking_for_a_session_on_a_disabled_gateway_is_refused() {
        let asked = key_from(
            &headers(&[("authorization", "Bearer k"), (SESSION_HEADER, "conv1")]),
            &OpenAi,
            false,
        );
        assert!(matches!(asked, Err(SessionError::Disabled)));
    }

    #[test]
    fn a_session_without_a_credential_is_refused() {
        for pairs in [
            vec![(SESSION_HEADER, "conv1")],
            // Present but empty: an empty string is not a namespace, it is
            // every caller who sent nothing.
            vec![("authorization", ""), (SESSION_HEADER, "conv1")],
        ] {
            let asked = key_from(&headers(&pairs), &OpenAi, true);
            assert!(
                matches!(asked, Err(SessionError::NoCredential("authorization"))),
                "accepted a session with {pairs:?}"
            );
        }
    }

    #[test]
    fn one_id_under_two_credentials_is_two_keys() {
        let one = key_from(
            &headers(&[("authorization", "Bearer k1"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let two = key_from(
            &headers(&[("authorization", "Bearer k2"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let again = key_from(
            &headers(&[("authorization", "Bearer k1"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        assert_ne!(one, two, "a guessed id reached another caller's namespace");
        assert_eq!(one, again, "the same caller lost its own session");
    }

    #[test]
    fn what_may_be_logged_carries_neither_the_id_nor_the_credential() {
        // A client picks its own id, so it may itself be personal data.
        let key = key_from(
            &headers(&[
                ("authorization", "Bearer sekrit"),
                (SESSION_HEADER, "patient.Weber-2026"),
            ]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let digest = key.digest();
        assert!(!digest.contains("Weber"));
        assert!(!digest.contains("sekrit"));
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn debug_formatting_carries_neither_the_id_nor_the_credential() {
        // `digest()` is not the only way a `SessionKey` can reach a log line —
        // `{:?}` is another, and a derived `Debug` would print the raw id.
        let key = key_from(
            &headers(&[
                ("authorization", "Bearer sekrit"),
                (SESSION_HEADER, "patient.Weber-2026"),
            ]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let formatted = format!("{key:?}");
        assert!(!formatted.contains("Weber"));
        assert!(!formatted.contains("sekrit"));
    }
}
