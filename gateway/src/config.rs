use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("detector_timeout_secs must be greater than zero")]
    ZeroTimeout,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_detector_url")]
    pub detector_url: String,
    /// Generous on purpose: full detection costs about a second per 1 200
    /// characters, and a conversation history is longer than that. Exceeding
    /// it refuses the request — it never forwards unmasked text.
    #[serde(default = "default_timeout")]
    pub detector_timeout_secs: u64,
    #[serde(default = "default_openai_base")]
    pub openai_base: String,
    #[serde(default = "default_anthropic_base")]
    pub anthropic_base: String,
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

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        if config.detector_timeout_secs == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_a_file() {
        let config = Config::from_toml("").expect("empty config is valid");
        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.detector_url, "http://127.0.0.1:8000");
        assert_eq!(config.detector_timeout_secs, 30);
    }

    #[test]
    fn values_override_the_defaults() {
        let config = Config::from_toml(
            r#"
            bind = "0.0.0.0:9090"
            detector_url = "http://detector:8000"
            detector_timeout_secs = 5
            openai_base = "https://api.openai.com"
            "#,
        )
        .expect("valid config");
        assert_eq!(config.bind, "0.0.0.0:9090");
        assert_eq!(config.detector_timeout_secs, 5);
        assert_eq!(config.openai_base, "https://api.openai.com");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A typo in a security control's configuration must not be silently ignored.
        let error = Config::from_toml("detector_timeoutt_secs = 5").unwrap_err();
        assert!(error.to_string().contains("detector_timeoutt_secs"));
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        assert!(Config::from_toml("detector_timeout_secs = 0").is_err());
    }
}
