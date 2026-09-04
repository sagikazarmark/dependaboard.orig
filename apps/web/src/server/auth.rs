//! HTTP basic authentication for the dashboard: every request through the
//! middleware carries the validated [`UserId`] as a request extension.

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dependaboard_core::UserId;

pub(crate) async fn require_dashboard_auth(mut request: Request<Body>, next: Next) -> Response {
    let username =
        std::env::var("DASHBOARD_USERNAME").unwrap_or_else(|_| "dependaboard".to_owned());
    let password = std::env::var("DASHBOARD_PASSWORD").unwrap_or_default();
    let authenticated_user = authenticated_dashboard_user(request.headers(), &username, &password);
    if let Some(user_id) = authenticated_user {
        request.extensions_mut().insert(user_id);
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"dependaboard\"")],
        "authentication required",
    )
        .into_response()
}

fn authenticated_dashboard_user(
    headers: &HeaderMap,
    username: &str,
    password: &str,
) -> Option<UserId> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|value| String::from_utf8(value).ok())
        .and_then(|credentials| {
            credentials
                .split_once(':')
                .map(|(user, pass)| (user.to_owned(), pass.to_owned()))
        })
        .filter(|(user, pass)| {
            constant_time_eq(user.as_bytes(), username.as_bytes())
                && constant_time_eq(pass.as_bytes(), password.as_bytes())
        })
        .map(|(user, _)| UserId::new(user))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_comparison_handles_different_lengths() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrex"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }

    #[test]
    fn authenticated_user_comes_from_validated_basic_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("dependaboard:secret"))
                .parse()
                .unwrap(),
        );
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "secret"),
            Some(UserId::new("dependaboard"))
        );
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "wrong"),
            None
        );
    }
}
