//! The sidebar search box. Typing is debounced: the query reaches the filter,
//! and so the store, once the user has paused, not on every keystroke.

use std::time::Duration;

use dependaboard_core::PrFilter;
use dioxus::core::Task;
use dioxus::prelude::*;

use crate::ui::sleep;

/// How long typing has to pause before the query is applied.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// The box edits a draft; the draft becomes `filter.query` after
/// [`SEARCH_DEBOUNCE`] of quiet, and paging restarts with it. When the query
/// changes from elsewhere (the clear buttons), the box follows it and drops
/// any edit still waiting, so a clear is not undone by what was typed just
/// before it.
#[component]
pub(crate) fn SearchBox(
    mut filter: Signal<PrFilter>,
    mut cursor: Signal<Option<String>>,
) -> Element {
    let committed = use_memo(move || filter.read().query.clone().unwrap_or_default());
    let mut draft = use_signal(|| committed.peek().clone());
    let mut pending = use_signal(|| None::<Task>);
    use_effect(move || {
        let committed = committed();
        // Our own commit clears `pending` before it lands, so there is only
        // something to cancel here when the change came from elsewhere.
        if let Some(task) = pending.take() {
            task.cancel();
        }
        draft.set(committed);
    });
    rsx! {
        div { class: "search-wrap",
            span { "/" }
            input {
                class: "input input-sm",
                value: draft(),
                placeholder: "dependency, repo, title...",
                oninput: move |event| {
                    let value = event.value();
                    draft.set(value.clone());
                    if let Some(task) = pending.take() {
                        task.cancel();
                    }
                    pending.set(Some(spawn(async move {
                        sleep(SEARCH_DEBOUNCE).await;
                        pending.set(None);
                        let query = (!value.trim().is_empty()).then_some(value);
                        if filter.peek().query != query {
                            filter.write().query = query;
                            cursor.set(None);
                        }
                    })));
                }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn the_box_starts_out_showing_the_query_in_force() {
        #[component]
        fn Fixture() -> Element {
            let filter = use_signal(|| PrFilter {
                query: Some("serde".to_owned()),
                ..Default::default()
            });
            let cursor = use_signal(|| None);
            rsx! { SearchBox { filter, cursor } }
        }
        let mut dom = VirtualDom::new(Fixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert!(html.contains(r#"value="serde""#), "{html}");
    }
}
