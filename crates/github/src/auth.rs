//! Who the client speaks as. The App: a JWT it signs with its private key,
//! exchanged for an installation token that is reused until a minute before
//! GitHub says it expires. The user: the one identity a deployment configures,
//! whose PAT `@dependabot` commands are posted under — or nobody, when the
//! operator minted none, and every command is refused with the variable to set.

use async_trait::async_trait;
use dependaboard_core::{UserId, unix_seconds};
use jsonwebtoken::{Algorithm, Header, encode};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::{
    API_VERSION, GithubClient, GithubError,
    rest::{parse_response, parse_timestamp},
};

#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn user_token(&self, user: &UserId) -> Result<SecretString, GithubError>;
}

/// The one user identity a deployment configures, with its PAT if the operator minted
/// one. Without a PAT there is no token for anyone, and every command is refused with
/// the variable to set.
pub(crate) struct StaticTokenProvider {
    pub(crate) user: UserId,
    pub(crate) token: Option<SecretString>,
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn user_token(&self, user: &UserId) -> Result<SecretString, GithubError> {
        let token = self.token.clone().ok_or_else(no_user_token)?;
        if user == &self.user {
            Ok(token)
        } else {
            Err(GithubError::Config(format!(
                "no GitHub user token is configured for {user}"
            )))
        }
    }
}

/// The refusal a command meets when the deployment has no user identity at all.
pub(crate) fn no_user_token() -> GithubError {
    GithubError::Config(
        "no GitHub user token is configured; set GITHUB_USER_PAT to post @dependabot commands"
            .to_owned(),
    )
}

#[derive(Clone)]
pub(crate) struct CachedToken {
    value: SecretString,
    expires_at: u64,
}

impl GithubClient {
    pub(crate) async fn installation_token(
        &self,
        installation_id: u64,
        force_refresh: bool,
    ) -> Result<SecretString, GithubError> {
        let now = unix_seconds();
        let mut tokens = self.tokens.lock().await;
        if !force_refresh
            && let Some(token) = tokens.get(&installation_id)
            && token.expires_at > now + 60
        {
            return Ok(token.value.clone());
        }
        let jwt = self.app_jwt()?;
        let response = self
            .http
            .post(format!(
                "{}/app/installations/{installation_id}/access_tokens",
                self.config.api_url
            ))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(jwt)
            .send()
            .await
            .map_err(GithubError::Transport)?;
        let token: InstallationToken = parse_response(response).await?;
        let expires_at = parse_timestamp(&token.expires_at)?;
        let value = SecretString::from(token.token);
        tokens.insert(
            installation_id,
            CachedToken {
                value: value.clone(),
                expires_at,
            },
        );
        Ok(value)
    }

    fn app_jwt(&self) -> Result<String, GithubError> {
        #[derive(Serialize)]
        struct Claims {
            iat: u64,
            exp: u64,
            iss: String,
        }

        let now = unix_seconds();
        encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iat: now.saturating_sub(60),
                exp: now + 9 * 60,
                iss: self.config.app_id.to_string(),
            },
            &self.app_key,
        )
        .map_err(|error| GithubError::Config(format!("cannot sign GitHub App JWT: {error}")))
    }
}

/// GitHub's answer to `POST /app/installations/{id}/access_tokens`.
#[derive(Debug, Deserialize)]
struct InstallationToken {
    token: String,
    expires_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_token_provider_is_scoped_to_the_configured_user() {
        let provider = StaticTokenProvider {
            user: UserId::new("dependaboard"),
            token: Some(SecretString::from("secret")),
        };
        assert!(
            provider
                .user_token(&UserId::new("dependaboard"))
                .await
                .is_ok()
        );
        assert!(
            provider
                .user_token(&UserId::new("someone-else"))
                .await
                .is_err()
        );
    }
}
