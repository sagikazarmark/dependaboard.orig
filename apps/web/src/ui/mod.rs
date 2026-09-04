//! The dashboard UI: the themed shell, its toast chrome, and the pieces the
//! page components share.

mod confirm_modal;
mod dashboard;
mod detail_drawer;
mod filters;
mod format;
mod pr_row;
mod progress_drawer;
mod side_panel;
#[cfg(all(test, feature = "server"))]
mod test_support;

use dependaboard_core::{BulkActionKind, PrRecord, PrTarget};
use dioxus::prelude::*;

use crate::components::toast::{
    ToastCloseButton, ToastColor, ToastContent, ToastDescription, ToastProps, ToastPropsWithOwner,
    ToastProvider, ToastTitle, ToastTitleAppearance,
};
use crate::ui::dashboard::Dashboard;

/// A bulk action the user has asked for but not yet confirmed.
///
/// The targets are resolved when the request is made, so the head SHAs the
/// confirmation dialog talks about are the ones that get submitted, and the
/// dialog's cancel and confirm paths do not depend on each other's ordering.
#[derive(Clone, PartialEq)]
pub(crate) struct PendingAction {
    pub(crate) action: BulkActionKind,
    pub(crate) targets: Vec<PrTarget>,
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

pub(crate) async fn wait_one_second() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(1_000).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}
