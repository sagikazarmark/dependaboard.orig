//! The bar across the top: the sidebar toggle, the view switch, the theme
//! button, and the sync button.

use dioxus::prelude::*;

use crate::api::request_sync;
use crate::components::button::{Button, ButtonSize};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::dashboard_state::use_dashboard;
use crate::ui::{POLL_INTERVAL, sleep, sticky, user_facing};

/// The top bar; `dark` is the theme in force and `aside_open` whether the
/// sidebar is showing, both toggled from here.
#[component]
pub(crate) fn TopBar(mut dark: Signal<bool>, mut aside_open: Signal<bool>) -> Element {
    let toast = use_toast();
    let mut state = use_dashboard();
    let needs_attention = state.filter().needs_attention;
    rsx! {
        header { class: "topbar",
            div { class: "brand",
                button {
                    class: "icon-button",
                    title: "Show or hide filters",
                    onclick: move |_| aside_open.toggle(),
                    "="
                }
                span { class: "brand-mark" }
                strong { "dependabot" }
                span { class: "muted mono account-label", "/ installations" }
            }
            div { class: "view-switch",
                button {
                    class: if !needs_attention { "active" } else { "" },
                    onclick: move |_| state.update_filter(|filter| filter.needs_attention = false),
                    "All open"
                }
                button {
                    class: if needs_attention { "active" } else { "" },
                    onclick: move |_| state.update_filter(|filter| filter.needs_attention = true),
                    "Needs attention"
                }
            }
            div { class: "topbar-spacer" }
            Button {
                size: ButtonSize::Xs,
                class: "btn-ghost theme-button",
                title: "Toggle color theme",
                onclick: move |_| dark.toggle(),
                if dark() { "light" } else { "dark" }
            }
            Button {
                size: ButtonSize::Sm,
                class: "sync-button",
                onclick: move |_| {
                    spawn(async move {
                        if let Err(error) = request_sync().await {
                            toast.error(format!("Sync failed: {}", user_facing(&error)), sticky());
                        } else {
                            toast.info("Reconciliation queued".to_owned(), ToastOptions::new());
                            sleep(POLL_INTERVAL).await;
                            state.reload.call(());
                        }
                    });
                },
                span { class: "sync-glyph", "+" }
                "Sync"
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::PrFilter;

    use super::*;
    use crate::ui::test_support::{DashboardFixture, render};

    #[test]
    fn the_view_switch_marks_the_view_in_force_and_the_theme_button_offers_the_other() {
        fn Fixture() -> Element {
            let filter = PrFilter {
                needs_attention: true,
                ..Default::default()
            };
            let dark = use_signal(|| true);
            let aside_open = use_signal(|| true);
            rsx! {
                DashboardFixture { filter, TopBar { dark, aside_open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<button class="">All open</button>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<button class="active">Needs attention</button>"#),
            "{html}"
        );
        assert!(html.contains(">light</button>"), "{html}");
        assert!(html.contains("Sync"), "{html}");
    }
}
