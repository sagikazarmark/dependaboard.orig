//! The installation the dashboard is bound to.

use dioxus::prelude::ServerFnError;

pub(crate) fn github_installation_id() -> Result<u64, ServerFnError> {
    std::env::var("GITHUB_INSTALLATION_ID")
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID is not configured"))?
        .parse::<u64>()
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID must be an integer"))
}
