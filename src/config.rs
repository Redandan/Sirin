use std::env;

/// AppConfig struct to unify management of LLM, Telegram, and Followup configurations.
#[derive(Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub llm_model: String,
    pub telegram_token: Option<String>,
    pub followup_url: Option<String>,
}

impl AppConfig {
    /// Loads configuration from environment variables.
    /// Returns `Ok(AppConfig)` on success, or `Err(String)` if a required variable is missing.
    pub fn load() -> Result<Self, String> {
        let llm_model = env::var("LLM_MODEL")
            .map_err(|_| "LLM_MODEL environment variable not set.".to_string())?;

        let telegram_token = env::var("TELEGRAM_TOKEN").ok();
        let followup_url = env::var("FOLLOWUP_URL").ok();

        Ok(Self {
            llm_model,
            telegram_token,
            followup_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to set environment variables for tests
    fn set_env(key: &str, value: &str) {
        env::set_var(key, value);
    }

    // Helper to unset environment variables after tests
    fn unset_env(key: &str) {
        env::remove_var(key);
    }

    #[test]
    fn test_app_config_load_all_set() {
        set_env("LLM_MODEL", "test-llm-model");
        set_env("TELEGRAM_TOKEN", "test-telegram-token");
        set_env("FOLLOWUP_URL", "http://test-followup-url.com");

        let config = AppConfig::load().unwrap();

        assert_eq!(config.llm_model, "test-llm-model");
        assert_eq!(config.telegram_token, Some("test-telegram-token".to_string()));
        assert_eq!(config.followup_url, Some("http://test-followup-url.com".to_string()));

        unset_env("LLM_MODEL");
        unset_env("TELEGRAM_TOKEN");
        unset_env("FOLLOWUP_URL");
    }

    #[test]
    fn test_app_config_load_only_required() {
        set_env("LLM_MODEL", "required-llm");
        unset_env("TELEGRAM_TOKEN");
        unset_env("FOLLOWUP_URL");

        let config = AppConfig::load().unwrap();

        assert_eq!(config.llm_model, "required-llm");
        assert_eq!(config.telegram_token, None);
        assert_eq!(config.followup_url, None);

        unset_env("LLM_MODEL");
    }

    #[test]
    fn test_app_config_load_missing_llm_model() {
        unset_env("LLM_MODEL");
        set_env("TELEGRAM_TOKEN", "some-token");

        let config_result = AppConfig::load();

        assert!(config_result.is_err());
        assert_eq!(config_result.unwrap_err(), "LLM_MODEL environment variable not set.");

        unset_env("TELEGRAM_TOKEN");
    }

    #[test]
    fn test_app_config_load_empty_strings() {
        set_env("LLM_MODEL", "");
        set_env("TELEGRAM_TOKEN", "");
        set_env("FOLLOWUP_URL", "");

        let config = AppConfig::load().unwrap();

        assert_eq!(config.llm_model, "");
        assert_eq!(config.telegram_token, Some("".to_string()));
        assert_eq!(config.followup_url, Some("".to_string()));

        unset_env("LLM_MODEL");
        unset_env("TELEGRAM_TOKEN");
        unset_env("FOLLOWUP_URL");
    }
}