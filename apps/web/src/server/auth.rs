//! The dashboard's auth edge: HTTP basic authentication, after which every
//! request carries the validated [`UserId`] as a request extension, and the
//! refusal of state-changing requests another site made the browser send.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, Method, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dependaboard_core::UserId;
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;

use crate::server::config::Credentials;

/// The credentials arrive as middleware state, resolved once at startup, so
/// no request re-reads them from the environment.
pub(crate) async fn require_dashboard_auth(
    State(credentials): State<Credentials>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let authenticated_user = authenticated_dashboard_user(
        request.headers(),
        &credentials.username,
        credentials.password.expose_secret(),
    );
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

/// Refuses a request another site made the browser send, credentials and
/// all; see [`cross_site`]. Sits inside the authentication layer, so the
/// refusal is only ever told to a caller who could have made the request
/// legitimately.
pub(crate) async fn refuse_cross_site(request: Request<Body>, next: Next) -> Response {
    if cross_site(&request) {
        tracing::warn!(
            method = %request.method(),
            path = request.uri().path(),
            site = ?request.headers().get(&SEC_FETCH_SITE),
            origin = ?request.headers().get(header::ORIGIN),
            "cross-site request refused"
        );
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    next.run(request).await
}

/// The browser's word on where a request came from, relative to the site it
/// is bound for.
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

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
            // Both comparisons run whatever the first one found, and `&` on
            // the choices, unlike `&&`, does not short-circuit: a wrong
            // username is not told from a wrong password by the time the
            // answer takes. What `ct_eq` does give away is a length mismatch.
            let user_matches = user.as_bytes().ct_eq(username.as_bytes());
            let pass_matches = pass.as_bytes().ct_eq(password.as_bytes());
            bool::from(user_matches & pass_matches)
        })
        .map(|(user, _)| UserId::new(user))
}

/// Whether `request` was sent from another site. The browser attaches the
/// dashboard's Basic credentials to any request bound for it, so a page on
/// another site could otherwise change state here by making the browser POST
/// on its behalf. The browser also says where the request came from, and the
/// dashboard's own scripts are the only sender allowed to change anything.
fn cross_site<B>(request: &Request<B>) -> bool {
    // Every server function is a POST; a GET only reads, and following a
    // link from another site must still open the dashboard.
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return false;
    }
    let headers = request.headers();
    match headers.get(&SEC_FETCH_SITE).map(|v| v.as_bytes()) {
        Some(b"same-origin" | b"none") => return false,
        Some(_) => return true,
        None => {}
    }
    // Without fetch metadata, a browser still names the page's origin on a
    // POST; a client naming neither is not a browser carrying a page's
    // request. The origin's scheme is left out of the comparison: a proxy
    // terminating TLS in front of the server hides it, and the host the
    // browser sent the request to is what tells the origins apart.
    let Some(origin) = headers.get(header::ORIGIN) else {
        return false;
    };
    let origin_authority = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, authority)| authority);
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        });
    match (origin_authority, host) {
        (Some(origin), Some(host)) => !origin.eq_ignore_ascii_case(host),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(credentials: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(credentials))
                .parse()
                .unwrap(),
        );
        headers
    }

    #[test]
    fn authenticated_user_comes_from_validated_basic_credentials() {
        let headers = basic("dependaboard:secret");
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "secret"),
            Some(UserId::new("dependaboard"))
        );
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "wrong"),
            None
        );
    }

    /// Both halves must match: neither a right password under another name
    /// nor a right name with a longer or shorter password gets in.
    #[test]
    fn a_partial_match_on_either_credential_is_refused() {
        assert_eq!(
            authenticated_dashboard_user(&basic("someone:secret"), "dependaboard", "secret"),
            None
        );
        assert_eq!(
            authenticated_dashboard_user(
                &basic("dependaboard:secret-longer"),
                "dependaboard",
                "secret"
            ),
            None
        );
        assert_eq!(
            authenticated_dashboard_user(&basic("dependaboard:"), "dependaboard", "secret"),
            None
        );
        assert_eq!(
            authenticated_dashboard_user(&basic("dependaboard"), "dependaboard", "secret"),
            None
        );
    }

    fn post(headers: &[(&str, &str)]) -> Request<()> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/api/submit_batch")
            .header(header::HOST, "dashboard.example");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(()).unwrap()
    }

    /// The browser says where a fetch came from; the dashboard's own scripts
    /// are the only sender allowed to change anything.
    #[test]
    fn fetch_metadata_tells_the_dashboards_own_requests_from_another_sites() {
        assert!(!cross_site(&post(&[("sec-fetch-site", "same-origin")])));
        assert!(cross_site(&post(&[("sec-fetch-site", "cross-site")])));
    }

    /// A sibling host under the same registrable domain is another origin
    /// all the same; a request the user started themselves, outside any
    /// page, is not a site's doing.
    #[test]
    fn a_sibling_site_is_refused_and_a_user_initiated_request_is_not() {
        assert!(cross_site(&post(&[("sec-fetch-site", "same-site")])));
        assert!(!cross_site(&post(&[("sec-fetch-site", "none")])));
    }

    /// A browser without fetch metadata still names the page's origin on a
    /// POST, so that is held against the host the request was sent to. A
    /// client that names neither is not a browser carrying a page's request.
    #[test]
    fn without_fetch_metadata_the_origin_is_held_against_the_host() {
        assert!(!cross_site(&post(&[(
            "origin",
            "https://dashboard.example"
        )])));
        assert!(!cross_site(&post(&[(
            "origin",
            "http://dashboard.example"
        )])));
        assert!(cross_site(&post(&[("origin", "https://evil.example")])));
        assert!(cross_site(&post(&[(
            "origin",
            "https://dashboard.example.evil.example"
        )])));
        assert!(cross_site(&post(&[("origin", "null")])));
        assert!(!cross_site(&post(&[])));
    }

    /// Following a link from another site is a cross-site GET, and it must
    /// still open the dashboard: only a request that can change state is
    /// held to the policy.
    #[test]
    fn a_request_that_cannot_change_state_is_never_refused() {
        let mut page = post(&[("sec-fetch-site", "cross-site")]);
        *page.method_mut() = Method::GET;
        assert!(!cross_site(&page));

        let mut probe = post(&[("sec-fetch-site", "cross-site")]);
        *probe.method_mut() = Method::HEAD;
        assert!(!cross_site(&probe));
    }
}
