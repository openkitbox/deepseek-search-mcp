use std::{env, fmt, time::Duration};

use crate::error::AppError;

pub const DEFAULT_ENDPOINT: &str = "https://api.deepseek.com";
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RETRIES: u8 = 2;

#[derive(Clone)]
pub struct Config {
    api_key: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        let value = env::var("DEEPSEEK_API_KEY").ok();
        Self::from_value(value.as_deref())
    }

    pub fn from_value(value: Option<&str>) -> Result<Self, AppError> {
        let Some(api_key) = value.filter(|candidate| !candidate.trim().is_empty()) else {
            return Err(AppError::Configuration(
                "DEEPSEEK_API_KEY is missing or blank".to_owned(),
            ));
        };

        Ok(Self {
            api_key: api_key.to_owned(),
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn safe_debug(&self) -> &'static str {
        "Config { api_key: <redacted> }"
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, DEFAULT_ENDPOINT, HTTP_TIMEOUT, MAX_RESPONSE_BODY_BYTES, MAX_RETRIES};

    #[test]
    fn rejects_missing_api_key_without_echoing_secret() {
        let error = Config::from_value(None).expect_err("missing key must fail");
        let message = error.to_string();

        assert!(message.contains("DEEPSEEK_API_KEY"));
        assert!(!message.contains("Bearer"));
        assert!(!message.contains("secret"));
    }

    #[test]
    fn rejects_blank_api_key_without_echoing_input() {
        let error = Config::from_value(Some(" \t\n ")).expect_err("blank key must fail");
        let message = error.to_string();

        assert!(message.contains("DEEPSEEK_API_KEY"));
        assert!(!message.contains("Bearer"));
    }

    #[test]
    fn accepts_non_blank_api_key_without_exposing_it() {
        let secret = "test-secret-value";
        let config = Config::from_value(Some(secret)).expect("key should be accepted");

        assert_eq!(config.api_key(), secret);
        assert!(!config.safe_debug().contains(secret));
    }

    #[test]
    fn runtime_defaults_are_fixed_and_safe() {
        assert_eq!(DEFAULT_ENDPOINT, "https://api.deepseek.com");
        assert_eq!(HTTP_TIMEOUT.as_secs(), 30);
        assert_eq!(MAX_RESPONSE_BODY_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_RETRIES, 2);
    }
}
