//! The web process's own settings, read from the environment once at startup
//! so a missing or malformed value stops the process instead of failing the
//! first request that needs it. The read-model connection settings are the
//! store crate's (`StoreConfig::from_env`) and are read next to these in
//! `main`.

use secrecy::SecretString;
use thiserror::Error;

/// Why the environment does not configure the process. Each names the
/// variable, since setting it is the operator's fix; `main` panics with the
/// message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum ConfigError {
    /// A required variable is unset or blank.
    #[error("{0} must be configured")]
    Missing(&'static str),
    /// A variable is set to something other than the integer it must be.
    #[error("{0} must be an integer")]
    NotAnInteger(&'static str),
}

pub(crate) struct Config {
    pub(crate) credentials: Credentials,
    pub(crate) webhook_secret: SecretString,
    /// The installation the dashboard is bound to; a per-PR sync or a batch
    /// target from any other is refused.
    pub(crate) installation_id: u64,
    pub(crate) restate: RestateConfig,
}

/// The one Basic Auth identity the dashboard accepts.
#[derive(Clone)]
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: SecretString,
}

/// Where the Restate ingress is and how to authenticate to it.
#[derive(Clone)]
pub(crate) struct RestateConfig {
    /// The ingress root without a trailing slash.
    pub(crate) base: String,
    pub(crate) token: Option<SecretString>,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        Self::parse(|name| std::env::var(name).ok())
    }

    /// Resolves the settings from `lookup`, an environment variable reader.
    /// A blank value counts as unset.
    fn parse(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let present = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let required = |name: &'static str| present(name).ok_or(ConfigError::Missing(name));
        let credentials = Credentials {
            username: present("DASHBOARD_USERNAME").unwrap_or_else(|| "dependaboard".to_owned()),
            password: SecretString::from(required("DASHBOARD_PASSWORD")?),
        };
        let webhook_secret = SecretString::from(required("GITHUB_WEBHOOK_SECRET")?);
        let installation_id = required("GITHUB_INSTALLATION_ID")?
            .parse::<u64>()
            .map_err(|_| ConfigError::NotAnInteger("GITHUB_INSTALLATION_ID"))?;
        let restate = RestateConfig {
            base: present("RESTATE_INGRESS_URL")
                .unwrap_or_else(|| "http://127.0.0.1:8080".to_owned())
                .trim_end_matches('/')
                .to_owned(),
            token: present("RESTATE_AUTH_TOKEN")
                .or_else(|| present("RESTATE_API_KEY"))
                .map(SecretString::from),
        };
        Ok(Self {
            credentials,
            webhook_secret,
            installation_id,
            restate,
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        }
    }

    const COMPLETE: &[(&str, &str)] = &[
        ("DASHBOARD_USERNAME", "octocat"),
        ("DASHBOARD_PASSWORD", "hunter2"),
        ("GITHUB_WEBHOOK_SECRET", "whsec"),
        ("GITHUB_INSTALLATION_ID", "42"),
        ("RESTATE_INGRESS_URL", "https://ingress.example/"),
        ("RESTATE_AUTH_TOKEN", "token"),
    ];

    fn token(config: &Config) -> Option<&str> {
        config
            .restate
            .token
            .as_ref()
            .map(ExposeSecret::expose_secret)
    }

    #[test]
    fn a_complete_environment_parses_into_the_settings() {
        let config = Config::parse(env(COMPLETE)).unwrap();

        assert_eq!(config.credentials.username, "octocat");
        assert_eq!(config.credentials.password.expose_secret(), "hunter2");
        assert_eq!(config.webhook_secret.expose_secret(), "whsec");
        assert_eq!(config.installation_id, 42);
        assert_eq!(config.restate.base, "https://ingress.example");
        assert_eq!(token(&config), Some("token"));
    }

    fn without(name: &str) -> Vec<(&'static str, &'static str)> {
        COMPLETE
            .iter()
            .copied()
            .filter(|(candidate, _)| *candidate != name)
            .collect()
    }

    #[test]
    fn a_missing_required_variable_is_named_in_the_error() {
        for name in [
            "DASHBOARD_PASSWORD",
            "GITHUB_WEBHOOK_SECRET",
            "GITHUB_INSTALLATION_ID",
        ] {
            let error = Config::parse(env(&without(name))).err().unwrap();
            assert_eq!(error, ConfigError::Missing(name));
            assert!(error.to_string().contains(name), "{name}: {error}");

            let mut blank = without(name);
            blank.push((name, "  "));
            let error = Config::parse(env(&blank)).err().unwrap();
            assert_eq!(error, ConfigError::Missing(name), "blank {name}");
        }
    }

    #[test]
    fn the_username_and_ingress_have_defaults_and_the_token_is_optional() {
        let minimal = env(&[
            ("DASHBOARD_PASSWORD", "hunter2"),
            ("GITHUB_WEBHOOK_SECRET", "whsec"),
            ("GITHUB_INSTALLATION_ID", "42"),
        ]);

        let config = Config::parse(minimal).unwrap();

        assert_eq!(config.credentials.username, "dependaboard");
        assert_eq!(config.restate.base, "http://127.0.0.1:8080");
        assert_eq!(token(&config), None);
    }

    #[test]
    fn an_installation_id_that_is_not_a_number_is_refused() {
        let mut pairs = without("GITHUB_INSTALLATION_ID");
        pairs.push(("GITHUB_INSTALLATION_ID", "forty-two"));

        let error = Config::parse(env(&pairs)).err().unwrap();

        assert_eq!(error, ConfigError::NotAnInteger("GITHUB_INSTALLATION_ID"));
        assert!(
            error.to_string().contains("GITHUB_INSTALLATION_ID"),
            "{error}"
        );
    }

    #[test]
    fn the_restate_api_key_stands_in_for_an_absent_or_blank_auth_token() {
        let mut pairs = without("RESTATE_AUTH_TOKEN");
        pairs.push(("RESTATE_API_KEY", "key"));
        assert_eq!(token(&Config::parse(env(&pairs)).unwrap()), Some("key"));

        pairs.push(("RESTATE_AUTH_TOKEN", ""));
        assert_eq!(token(&Config::parse(env(&pairs)).unwrap()), Some("key"));
    }
}
