//! The status bar along the bottom — the line to the read model, who is
//! signed in, and when the projection last heard from GitHub — and the
//! banner over the page for when that line is down.

use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::dashboard_state::{Connection, use_dashboard};
use crate::ui::format::{ago, connection_class, connection_label, relative_time};

/// Reloads the page. With Basic Auth that is what asks for the credentials
/// again once the server has refused the ones the browser holds.
const RELOAD_SCRIPT: &str = "location.reload();";

/// The banner over the page while the line to the read model is down:
/// nothing while the polls are answered. Disconnected, it names the age of
/// the rows — the last poll that was answered, against the dashboard's clock
/// — since a viewer who is not clicking anything would otherwise take them
/// as current. It does not say what is down: the poll cannot tell a server
/// that is away from one that cannot reach its store, and either way the
/// rows are not being refreshed. Signed out, it says what cures that and
/// offers it.
#[component]
pub(crate) fn ConnectionBanner() -> Element {
    let state = use_dashboard();
    match state.connection() {
        Connection::Online => rsx! {},
        Connection::Disconnected => rsx! {
            div { class: "connection-banner", role: "alert",
                strong { "Disconnected from the read model." }
                match state.refreshed_at() {
                    Some(refreshed) => rsx! {
                        span { "Last successful refresh {ago(state.now(), refreshed)}; the rows may be out of date." }
                    },
                    None => rsx! {
                        span { "No refresh has succeeded since the page opened." }
                    },
                }
            }
        },
        Connection::SignedOut => rsx! {
            div { class: "connection-banner", role: "alert",
                strong { "You are no longer signed in." }
                span { "Reload the page to sign in again." }
                Button {
                    size: ButtonSize::Xs,
                    onclick: move |_| {
                        document::eval(RELOAD_SCRIPT);
                    },
                    "Reload"
                }
            }
        },
    }
}

/// The footer; `user` is who the server said is signed in, once it has.
#[component]
pub(crate) fn StatusBar(#[props(default)] user: Option<String>) -> Element {
    let state = use_dashboard();
    let summary = state.summary.read();
    let last_synced_at = summary.loaded().and_then(|summary| summary.last_synced_at);
    let now = state.now();
    let connection = state.connection();
    rsx! {
        footer { class: "statusbar",
            span { class: connection_class(connection) }
            span { {connection_label(connection)} }
            if let Some(user) = user {
                span { class: "status-user", "signed in as {user}" }
            }
            span { class: "status-spacer" }
            if let Some(synced) = last_synced_at {
                span { "last event {relative_time(now, synced)}" }
            } else {
                span { "waiting for first reconciliation" }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::ui::dashboard_state::SummaryStatus;
    use crate::ui::test_support::{DashboardFixture, FIXTURE_NOW, loaded_summary, render};

    #[test]
    fn the_status_bar_dates_the_last_event_once_there_has_been_one() {
        fn Loading() -> Element {
            rsx! {
                DashboardFixture { StatusBar {} }
            }
        }
        let loading = render(Loading);
        assert!(
            loading.contains("waiting for first reconciliation"),
            "{loading}"
        );

        fn Loaded() -> Element {
            rsx! {
                DashboardFixture { summary: SummaryStatus::Loaded(loaded_summary()), StatusBar {} }
            }
        }
        let loaded = render(Loaded);
        assert!(loaded.contains("last event now"), "{loaded}");
        assert!(!loaded.contains("waiting for"), "{loaded}");
    }

    /// The date is read against the dashboard's clock, so it moves on as the
    /// clock ticks, with no new answer from the read model.
    #[test]
    fn the_last_event_ages_with_the_dashboards_clock() {
        fn Later() -> Element {
            rsx! {
                DashboardFixture {
                    summary: SummaryStatus::Loaded(loaded_summary()),
                    now: FIXTURE_NOW + 5 * 60,
                    StatusBar {}
                }
            }
        }
        let later = render(Later);

        assert!(later.contains("last event 5m"), "{later}");
    }

    /// The footer names the identity the server let the page in as, once the
    /// server has said; until then, or if it never does, it says nothing
    /// rather than guess.
    #[test]
    fn the_status_bar_names_who_is_signed_in_once_the_server_has_said() {
        fn Known() -> Element {
            rsx! {
                DashboardFixture { StatusBar { user: Some("octocat".to_owned()) } }
            }
        }
        let known = render(Known);
        assert!(known.contains("signed in as octocat"), "{known}");

        fn Unknown() -> Element {
            rsx! {
                DashboardFixture { StatusBar {} }
            }
        }
        let unknown = render(Unknown);
        assert!(!unknown.contains("signed in as"), "{unknown}");
    }

    /// While the polls are answered nothing is over the page and the dot is
    /// green. Once the line is down a banner says so and names the age of
    /// the last refresh, against the dashboard's clock, so a passive viewer
    /// learns how old the rows are; the dot goes with it.
    #[test]
    fn a_banner_names_the_age_of_the_rows_once_the_line_is_down() {
        fn Online() -> Element {
            rsx! {
                DashboardFixture { refreshed_at: Some(FIXTURE_NOW),
                    ConnectionBanner {}
                    StatusBar {}
                }
            }
        }
        let online = render(Online);
        assert!(!online.contains("connection-banner"), "{online}");
        assert!(online.contains(r#"<span class="status-dot">"#), "{online}");
        assert!(online.contains("<span>connected</span>"), "{online}");

        fn Disconnected() -> Element {
            rsx! {
                DashboardFixture {
                    connection: Connection::Disconnected,
                    refreshed_at: Some(FIXTURE_NOW - 40 * 60),
                    ConnectionBanner {}
                    StatusBar {}
                }
            }
        }
        let down = render(Disconnected);
        assert!(down.contains("connection-banner"), "{down}");
        assert!(down.contains("Disconnected from the read model"), "{down}");
        assert!(down.contains("Last successful refresh 40m ago"), "{down}");
        assert!(
            down.contains(r#"<span class="status-dot offline">"#),
            "{down}"
        );
        assert!(down.contains("<span>disconnected</span>"), "{down}");
        assert!(!down.contains("<span>connected</span>"), "{down}");
    }

    /// A dashboard that never got an answer — the server was down when the
    /// page opened — has no refresh to date, and says so instead.
    #[test]
    fn a_dashboard_never_answered_says_so_rather_than_date_a_refresh() {
        fn Never() -> Element {
            rsx! {
                DashboardFixture { connection: Connection::Disconnected,
                    ConnectionBanner {}
                }
            }
        }
        let never = render(Never);

        assert!(
            never.contains("No refresh has succeeded since the page opened"),
            "{never}"
        );
        assert!(!never.contains("Last successful refresh"), "{never}");
    }

    /// The credentials being refused is not the server being away: the
    /// banner says to sign in again, which for Basic Auth is a reload, and
    /// offers one.
    #[test]
    fn a_refusal_of_the_credentials_asks_the_user_to_sign_in_again() {
        fn SignedOut() -> Element {
            rsx! {
                DashboardFixture { connection: Connection::SignedOut, refreshed_at: Some(FIXTURE_NOW),
                    ConnectionBanner {}
                    StatusBar {}
                }
            }
        }
        let out = render(SignedOut);

        assert!(out.contains("connection-banner"), "{out}");
        assert!(out.contains("no longer signed in"), "{out}");
        assert!(out.contains("Reload</button>"), "{out}");
        assert!(!out.contains("Disconnected from"), "{out}");
        assert!(
            out.contains(r#"<span class="status-dot offline">"#),
            "{out}"
        );
        assert!(out.contains("<span>signed out</span>"), "{out}");
    }
}
