//! The dashboard UI: the themed shell, its toast chrome, and the pieces the
//! page components share.

mod action_bar;
mod active_batch;
mod batch;
mod confirm_modal;
mod dashboard;
mod dashboard_state;
mod detail_drawer;
mod filters;
mod format;
mod live;
mod pr_row;
mod pr_table;
mod progress_drawer;
mod repo_tree;
mod search_box;
mod side_panel;
mod sidebar;
mod status_bar;
#[cfg(all(test, feature = "server"))]
mod test_support;
mod top_bar;
mod url_state;
mod url_sync;

use std::collections::BTreeSet;
use std::time::Duration;

use dependaboard_core::{BulkActionKind, PrRecord, PrTarget};
use dioxus::prelude::*;

use crate::components::toast::{
    ToastCloseButton, ToastColor, ToastContent, ToastDescription, ToastOptions, ToastProps,
    ToastPropsWithOwner, ToastProvider, ToastTitle, ToastTitleAppearance,
};
use crate::ui::dashboard::Dashboard;

/// A bulk action the user has asked for but not yet confirmed.
///
/// The rows are resolved when the request is made, so the head SHAs the
/// confirmation dialog talks about are the ones that get submitted, and the
/// dialog's cancel and confirm paths do not depend on each other's ordering.
/// They are kept as rows rather than targets so the dialog can also say what
/// state they are in.
#[derive(Clone, PartialEq)]
pub(crate) struct PendingAction {
    pub(crate) action: BulkActionKind,
    pub(crate) rows: Vec<PrRecord>,
}

impl PendingAction {
    /// The rows as the targets the batch submits.
    pub(crate) fn targets(&self) -> Vec<PrTarget> {
        self.rows.iter().map(pr_target).collect()
    }
}

/// Errors stay until dismissed; every other toast auto-dismisses.
pub(crate) fn sticky() -> ToastOptions {
    ToastOptions::new().permanent(true)
}

pub(crate) fn App() -> Element {
    let dark = use_signal(|| true);
    let theme = if dark() {
        "dependaboard-dark"
    } else {
        "dependaboard-light"
    };

    rsx! {
        document::Link { rel: "preconnect", href: "https://fonts.googleapis.com" }
        document::Link { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "anonymous" }
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap"
        }
        document::Stylesheet { href: asset!("/assets/main.css") }

        // The toast region renders where the provider sits, so it has to be
        // inside the themed shell for daisyUI's colour variables to reach it.
        div { class: "app-shell", "data-theme": theme,
            ToastProvider {
                render_toast: Callback::new(|props: ToastPropsWithOwner| rsx! { AppToast { ..props } }),
                Dashboard { dark }
            }
        }
    }
}

/// The registry toast with this app's chrome: the message is the title, set in
/// the same weight as the rest of the UI, and `toast-message` carries the look.
fn AppToast(props: ToastProps) -> Element {
    let color = ToastColor::of(props.toast_type).class();
    rsx! {
        dioxus_primitives::toast::Toast {
            id: props.id,
            index: props.index,
            title: props.title,
            description: props.description,
            toast_type: props.toast_type,
            on_close: props.on_close,
            permanent: props.permanent,
            duration: props.duration,
            class: "alert {color} toast-message",
            ToastContent {
                ToastTitle { appearance: ToastTitleAppearance::None }
                ToastDescription {}
            }
            ToastCloseButton {}
        }
    }
}

pub(crate) fn pr_target(row: &PrRecord) -> PrTarget {
    PrTarget {
        repository_id: row.repository_id,
        owner: row.owner.clone(),
        repo: row.repo.clone(),
        number: row.number,
        expected_sha: row.head_sha.clone(),
        title: row.title.clone(),
        html_url: row.html_url.clone(),
    }
}

/// How many distinct repositories `rows` belong to: what a bulk action over
/// them fans out across.
pub(crate) fn repository_count<'a>(rows: impl IntoIterator<Item = &'a PrRecord>) -> usize {
    rows.into_iter()
        .map(|row| row.repository_id)
        .collect::<BTreeSet<_>>()
        .len()
}

/// How often the dashboard asks the server about work it is waiting on.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) async fn sleep(duration: Duration) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(
        u32::try_from(duration.as_millis()).unwrap_or(u32::MAX),
    )
    .await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(duration).await;
}

/// The text a failed server call shows the user. The server has already
/// reduced its own failures to a message that names the component, so that
/// message is shown as is; a round trip that never produced one gets a fixed
/// line. The error itself goes to the log in full either way.
pub(crate) fn user_facing(error: &ServerFnError) -> String {
    dioxus::logger::tracing::warn!(%error, "server call failed");
    match error {
        ServerFnError::ServerError { message, .. } => message.clone(),
        _ => "The dashboard server could not be reached".to_owned(),
    }
}
