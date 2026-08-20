use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("detector_timeout_secs must be greater than zero")]
    ZeroTimeout,
    #[error("max_sessions must be greater than zero unless session_idle_secs is zero")]
    ZeroSessionLimit,
    #[error("max_session_values must be greater than zero unless session_idle_secs is zero")]
    ZeroSessionValues,
    #[error(
        "max_spans_per_entry must be greater than zero unless detection_cache_entries is zero"
    )]
    ZeroSpansPerEntry,
    #[error("max_tool_bytes must be greater than zero")]
    ZeroToolBytes,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_detector_url")]
    pub detector_url: String,
    /// Where the audit journal is appended. Required: a deployment with no
    /// journal cannot show a DPO that anything was pseudonymized, and a
    /// control that is off by default is not a control. A file that cannot be
    /// opened for appending stops the process rather than starting one that
    /// silently proves nothing.
    pub audit_path: String,
    /// Generous on purpose: full detection costs about a second per 1 200
    /// characters, and a conversation history is longer than that. Exceeding
    /// it refuses the request — it never forwards unmasked text.
    #[serde(default = "default_timeout")]
    pub detector_timeout_secs: u64,
    #[serde(default = "default_openai_base")]
    pub openai_base: String,
    #[serde(default = "default_anthropic_base")]
    pub anthropic_base: String,
    /// How long a session survives without a request. Zero disables sessions
    /// entirely: personal data then never outlives the request that carried it,
    /// which is the right setting for a deployment that wants that guarantee.
    #[serde(default = "default_session_idle_secs")]
    pub session_idle_secs: u64,
    /// How many conversations may hold a table at once. The oldest is dropped
    /// when a new one arrives at the limit.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// How many values one conversation may remember. Past it, values are still
    /// masked and still restored — they are simply not remembered.
    #[serde(default = "default_max_session_values")]
    pub max_session_values: usize,
    /// How many detection results may be remembered at once. Zero disables the
    /// cache, which is a memory budget rather than a mistake: the gateway then
    /// calls the detector for every text, as it did before the cache existed.
    #[serde(default = "default_detection_cache_entries")]
    pub detection_cache_entries: usize,
    /// The cache's other dimension: how many spans a single detection may
    /// carry and still be remembered. `detection_cache_entries` bounds how
    /// many texts the cache holds, not how large one text's detection may
    /// be — the same relationship `max_session_values` has to `max_sessions`.
    /// A detection over the cap is served exactly as any other: masked,
    /// restored, returned. It is simply not stored, so the cache never turns
    /// a large result into a refusal.
    #[serde(default = "default_max_spans_per_entry")]
    pub max_spans_per_entry: usize,
    /// How many bytes of tool structure one request may newly scan, summed
    /// across every tool definition, argument and result in it.
    ///
    /// This key and `detector_timeout_secs` are one constraint written twice.
    /// The detector clears roughly 530 characters per wall-clock second, so
    /// 10 000 characters is about 19 seconds against a 30-second timeout, with
    /// room for a slower machine. Raise the timeout to serve larger results and
    /// this has to rise with it; lower the timeout and this has to fall.
    ///
    /// It is not derived from the timeout automatically on purpose: a derived
    /// default changes behaviour silently when an unrelated key moves.
    ///
    /// The ceiling exists because the detection cache does not help the first
    /// time a text is seen, and a tool result is usually seen once. Issue #28
    /// carries the work that lifts it.
    #[serde(default = "default_max_tool_bytes")]
    pub max_tool_bytes: usize,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_detector_url() -> String {
    "http://127.0.0.1:8000".to_owned()
}
fn default_timeout() -> u64 {
    30
}
fn default_openai_base() -> String {
    "https://api.openai.com".to_owned()
}
fn default_anthropic_base() -> String {
    "https://api.anthropic.com".to_owned()
}
fn default_session_idle_secs() -> u64 {
    1800
}
fn default_max_sessions() -> usize {
    1000
}
fn default_max_session_values() -> usize {
    1000
}
fn default_detection_cache_entries() -> usize {
    10_000
}
// Chosen from measured density, not from the memory side alone: real text
// runs roughly 1.0 to 2.5 spans per 1 000 characters (git log --stat 1.33,
// README prose 2.50, Rust source 1.00 — the evaluation corpus's 28.67 is a
// dense-PII detection benchmark, not a proxy for real traffic). 250 spans
// covers prose to about 100 KB, logs to about 188 KB and source to 250 KB —
// every realistic single tool result — while a lower cap would trim the
// longest, most expensive-to-recompute texts first, which is the wrong end
// to trim on a number that is otherwise arbitrary.
//
// That coverage is a coding agent's traffic. A uniformly dense text — a
// contact list or an intake form, not ordinary correspondence, which is
// prose-shaped at nearer 2.5/1 000 and unaffected — crosses the cap at
// single-digit kilobytes (8-16 KB at the evaluation corpus's measured
// density, offered only as an illustration of a uniformly dense text, never
// as a characterisation of real traffic — every row of that corpus is a
// single rendered sentence under 126 characters, so nothing in this
// repository is actually shaped like a client document; the figure awaits
// its measurement, which is spans per 1 000 characters over real gateway
// traffic). The cache keys per text, so this bites a dense message arriving
// as one text, not a conversation about a dense file (many short turns, all
// cached normally) or a document that is merely dense in places. A real
// limit, not a number to keep raising by default, because the same number
// multiplies against the entry ceiling below. Raising it is the deliberate
// lever for that traffic; see tessera.example.toml for the arithmetic that
// prices it.
//
// At 264 B fixed + 46 B/span (measured, a floor): the worst-case entry is
// 264 + 250 * 46 = 11 764 B, and at the default 10 000 entries the worst-case
// cache is 10 000 * 11 764 B = 117 640 000 B ≈ 118 MB. A text with more
// spans than that is real and is still served correctly; it is simply not
// remembered, the same trade `max_session_values` already makes for a
// session with too many values.
//
// The 46 B/span figure measured real detector output, where `Span.entity_type`
// runs a handful of characters (`PERSON`, `IBAN`). It was never a ceiling on
// that string's length, only an average over what a real detector actually
// sends — a single span with a large type passed this cap on count exactly
// as easily as an ordinary one and was retained in full. `DetectionCache::insert`
// now declines an entry with any span past `mapping::MAX_ENTITY_TYPE` (40
// bytes), closing that gap the same way this cap closes span count. That
// makes the true worst case per span roughly 80 B (a `String`'s own ~24 B
// plus up to 40 B of characters plus two `usize` offsets) rather than 46,
// so the 118 MB figure above is now a typical case again, not an assumed
// ceiling — the actual ceiling is nearer 200 MB. Analytical, from struct
// layout, not re-measured with a counting allocator the way 46 B was.
fn default_max_spans_per_entry() -> usize {
    250
}
fn default_max_tool_bytes() -> usize {
    10_000
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        if config.detector_timeout_secs == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        if config.session_idle_secs > 0 {
            if config.max_sessions == 0 {
                return Err(ConfigError::ZeroSessionLimit);
            }
            if config.max_session_values == 0 {
                return Err(ConfigError::ZeroSessionValues);
            }
        }
        if config.detection_cache_entries > 0 && config.max_spans_per_entry == 0 {
            return Err(ConfigError::ZeroSpansPerEntry);
        }
        if config.max_tool_bytes == 0 {
            return Err(ConfigError::ZeroToolBytes);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every config below needs the required key; naming it once keeps the
    /// tests about the setting each one is actually exercising.
    fn with_audit(body: &str) -> String {
        format!("audit_path = \"/tmp/tessera-test-audit.jsonl\"\n{body}")
    }

    #[test]
    fn audit_path_is_required() {
        // A control that can be switched off by omitting a line is worth
        // nothing in a compliance report.
        let error = Config::from_toml("").unwrap_err();
        assert!(error.to_string().contains("audit_path"));
    }

    #[test]
    fn audit_path_is_read() {
        let config = Config::from_toml(r#"audit_path = "/var/log/tessera/audit.jsonl""#)
            .expect("a config naming an audit log is valid");
        assert_eq!(config.audit_path, "/var/log/tessera/audit.jsonl");
    }

    #[test]
    fn defaults_are_usable_without_a_file() {
        let config = Config::from_toml(&with_audit("")).expect("empty config is valid");
        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.detector_url, "http://127.0.0.1:8000");
        assert_eq!(config.detector_timeout_secs, 30);
    }

    #[test]
    fn values_override_the_defaults() {
        let config = Config::from_toml(&with_audit(
            r#"
            bind = "0.0.0.0:9090"
            detector_url = "http://detector:8000"
            detector_timeout_secs = 5
            openai_base = "https://api.openai.com"
            "#,
        ))
        .expect("valid config");
        assert_eq!(config.bind, "0.0.0.0:9090");
        assert_eq!(config.detector_timeout_secs, 5);
        assert_eq!(config.openai_base, "https://api.openai.com");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A typo in a security control's configuration must not be silently ignored.
        let error = Config::from_toml(&with_audit("detector_timeoutt_secs = 5")).unwrap_err();
        assert!(error.to_string().contains("detector_timeoutt_secs"));
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        assert!(Config::from_toml(&with_audit("detector_timeout_secs = 0")).is_err());
    }

    #[test]
    fn session_defaults_are_bounded() {
        let config = Config::from_toml(&with_audit("")).expect("empty config is valid");
        assert_eq!(config.session_idle_secs, 1800);
        assert_eq!(config.max_sessions, 1000);
        assert_eq!(config.max_session_values, 1000);
    }

    #[test]
    fn session_values_override_the_defaults() {
        let config = Config::from_toml(&with_audit(
            r#"
            session_idle_secs = 60
            max_sessions = 4
            max_session_values = 8
            "#,
        ))
        .expect("valid config");
        assert_eq!(config.session_idle_secs, 60);
        assert_eq!(config.max_sessions, 4);
        assert_eq!(config.max_session_values, 8);
    }

    #[test]
    fn zero_idle_disables_sessions_and_permits_zero_limits() {
        let config = Config::from_toml(&with_audit(
            r#"
            session_idle_secs = 0
            max_sessions = 0
            max_session_values = 0
            "#,
        ))
        .expect("a deployment that wants no personal data in memory between requests");
        assert_eq!(config.session_idle_secs, 0);
    }

    #[test]
    fn a_zero_limit_with_sessions_enabled_is_rejected() {
        // Enabled and unable to hold anything is not a configuration, it is a typo.
        assert!(Config::from_toml(&with_audit("max_sessions = 0")).is_err());
        assert!(Config::from_toml(&with_audit("max_session_values = 0")).is_err());
    }

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

    #[test]
    fn the_span_cap_has_a_default() {
        let config = Config::from_toml(&with_audit("")).unwrap();
        assert_eq!(config.max_spans_per_entry, 250);
    }

    #[test]
    fn the_span_cap_can_be_sized() {
        let config = Config::from_toml(&with_audit("max_spans_per_entry = 8")).unwrap();
        assert_eq!(config.max_spans_per_entry, 8);
    }

    #[test]
    fn a_zero_span_cap_with_the_cache_enabled_is_rejected() {
        // The same mistake `a_zero_limit_with_sessions_enabled_is_rejected`
        // guards against, one level down: an entry cap with nothing an
        // entry may hold is a typo, not a configuration.
        let error = Config::from_toml(&with_audit("max_spans_per_entry = 0")).unwrap_err();
        assert!(error.to_string().contains("max_spans_per_entry"));
    }

    #[test]
    fn a_zero_span_cap_is_permitted_once_the_cache_is_disabled() {
        let config = Config::from_toml(&with_audit(
            r#"
            detection_cache_entries = 0
            max_spans_per_entry = 0
            "#,
        ))
        .expect("a disabled cache does not care what its dead setting says");
        assert_eq!(config.max_spans_per_entry, 0);
    }

    #[test]
    fn the_tool_bound_has_a_default() {
        let config = Config::from_toml(&with_audit("")).unwrap();
        assert_eq!(config.max_tool_bytes, 10_000);
    }

    #[test]
    fn a_zero_tool_bound_is_rejected() {
        let text = with_audit("max_tool_bytes = 0");
        assert!(matches!(
            Config::from_toml(&text),
            Err(ConfigError::ZeroToolBytes)
        ));
    }
}
