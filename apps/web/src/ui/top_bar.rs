//! The bar across the top: the sidebar toggle, the view switch, the theme
//! button, the recent batches button, and the sync button.

use dioxus::prelude::*;

use crate::api::request_sync;
use crate::components::button::{Button, ButtonSize};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::dashboard_state::use_dashboard;
use crate::ui::{sticky, user_facing};

/// The top bar; `dark` is the theme in force, `aside_open` whether the
/// sidebar is showing, and `batches_open` whether the recent batches drawer
/// is, all toggled from here.
#[component]
pub(crate) fn TopBar(
    mut dark: Signal<bool>,
    mut aside_open: Signal<bool>,
    mut batches_open: Signal<bool>,
) -> Element {
    let toast = use_toast();
    let mut state = use_dashboard();
    let needs_attention = state.filter().needs_attention;
    let syncing = state.syncing();
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
                class: "batches-button",
                title: "The batches that have run",
                onclick: move |_| batches_open.toggle(),
                "Batches"
            }
            // The sync is one-way: Restate takes it and the sweep runs on
            // its own. The glyph spins until the rows reload, which the live
            // refresh sees to once the sweep's first change lands.
            Button {
                size: ButtonSize::Sm,
                class: "sync-button",
                disabled: syncing,
                onclick: move |_| {
                    state.begin_sync();
                    spawn(async move {
                        match request_sync().await {
                            Ok(()) => toast.info("Reconciliation queued".to_owned(), ToastOptions::new()),
                            Err(error) => {
                                state.end_sync();
                                toast.error(format!("Sync failed: {}", user_facing(&error)), sticky());
                            }
                        }
                    });
                },
                span { class: if syncing { "sync-glyph spinning" } else { "sync-glyph" }, "⟳" }
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
            let batches_open = use_signal(|| false);
            rsx! {
                DashboardFixture { filter, TopBar { dark, aside_open, batches_open } }
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
        assert!(html.contains(r#"<span class="sync-glyph">"#), "{html}");
        assert!(!html.contains("disabled"), "{html}");
    }

    /// The batches that have run are a click away from anywhere on the page.
    #[test]
    fn the_top_bar_offers_the_recent_batches() {
        fn Fixture() -> Element {
            let dark = use_signal(|| true);
            let aside_open = use_signal(|| true);
            let batches_open = use_signal(|| false);
            rsx! {
                DashboardFixture { TopBar { dark, aside_open, batches_open } }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("batches-button"), "{html}");
        assert!(html.contains("Batches</button>"), "{html}");
    }

    /// While a manual sync is in flight the glyph spins and the button will
    /// not queue another.
    #[test]
    fn the_sync_glyph_spins_while_a_sync_is_in_flight() {
        fn Fixture() -> Element {
            let dark = use_signal(|| true);
            let aside_open = use_signal(|| true);
            let batches_open = use_signal(|| false);
            rsx! {
                DashboardFixture { syncing: true, TopBar { dark, aside_open, batches_open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<span class="sync-glyph spinning">"#),
            "{html}"
        );
        assert!(html.contains("disabled"), "{html}");
    }
}
