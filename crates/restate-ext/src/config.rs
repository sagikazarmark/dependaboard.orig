//! Reading a service's whole-seconds settings from the environment.

use std::env;

use tracing::warn;

/// Reads a whole-seconds setting — a debounce, a reconcile interval — from the environment.
pub fn seconds_from_env(name: &str, default: u64) -> u64 {
    let raw = match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        // Not UTF-8 cannot be a number either; surface it as the typo it is.
        Err(env::VarError::NotUnicode(raw)) => Some(raw.to_string_lossy().into_owned()),
    };
    resolve_seconds(name, raw.as_deref(), default)
}

/// Resolves a whole-seconds setting from its raw environment value.
///
/// A typo must not silently become the default: an unparsable value still falls back, but
/// says so and names the variable, so the operator learns at startup rather than from the
/// service's behaviour. Unset or empty means "use the default" and is not worth a line.
///
/// Named apart from [`seconds_from_env`] so it can be asserted without touching the
/// environment, which no test can do without racing every other test in the process.
pub fn resolve_seconds(name: &str, raw: Option<&str>, default: u64) -> u64 {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return default;
    };
    match raw.parse::<u64>() {
        Ok(seconds) => seconds,
        Err(error) => {
            warn!(
                variable = name,
                value = raw,
                default,
                %error,
                "environment setting is not a whole number of seconds; using the default"
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::captured_logs;

    #[test]
    fn an_unparsable_seconds_setting_falls_back_and_warns_naming_the_variable() {
        let mut resolved = None;
        let logs = captured_logs(|| {
            resolved = Some(resolve_seconds("SYNC_DEBOUNCE_SECONDS", Some("20s"), 20));
        });

        assert_eq!(resolved, Some(20));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("SYNC_DEBOUNCE_SECONDS"), "{logs}");
        assert!(logs.contains("20s"), "{logs}");
    }

    #[test]
    fn configured_and_unset_seconds_settings_resolve_silently() {
        let mut resolved = Vec::new();
        let logs = captured_logs(|| {
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some("900"),
                3600,
            ));
            resolved.push(resolve_seconds("RECONCILE_INTERVAL_SECONDS", None, 3600));
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some(""),
                3600,
            ));
        });

        assert_eq!(resolved, vec![900, 3600, 3600]);
        assert!(logs.is_empty(), "{logs}");
    }
}
