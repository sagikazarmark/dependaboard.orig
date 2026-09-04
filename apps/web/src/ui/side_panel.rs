//! The drawer shell the detail and progress panels slide in on.

use dioxus::prelude::*;

/// The scrim, panel and header shared by the side drawers. Clicking the scrim
/// or the close button closes; clicks inside the panel do not propagate.
#[component]
pub(crate) fn SidePanel(
    class: &'static str,
    eyebrow: String,
    title: Element,
    onclose: EventHandler<()>,
    children: Element,
) -> Element {
    rsx! {
        div { class: "drawer-scrim", onclick: move |_| onclose.call(()),
            section { class: "side-drawer {class}", onclick: move |event| event.stop_propagation(),
                div { class: "drawer-head",
                    div {
                        span { class: "eyebrow", "{eyebrow}" }
                        h2 { {title} }
                    }
                    button { class: "close-button", onclick: move |_| onclose.call(()), "x" }
                }
                {children}
            }
        }
    }
}
