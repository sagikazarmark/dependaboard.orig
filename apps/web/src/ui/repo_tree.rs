//! The sidebar's repository tree: the repositories with pull requests under
//! the other filters, grouped by owner, with a box to search them. A
//! repository toggles itself in the filter; an owner's checkbox selects or
//! deselects every repository of the owner, or, under a search, every one
//! the search shows.

use std::collections::BTreeSet;

use dependaboard_core::RepoFacet;
use dioxus::prelude::*;

use crate::ui::dashboard_state::use_dashboard;

/// The tree for `repositories`, the store's repository facet: every
/// repository in owner then name order, each with its count under the other
/// filters.
#[component]
pub(crate) fn RepoTree(repositories: Vec<RepoFacet>) -> Element {
    let mut state = use_dashboard();
    let mut query = use_signal(String::new);
    let mut collapsed = use_signal(BTreeSet::<String>::new);
    // A search opens every owner, so what it finds is never folded away.
    let searching = !query.read().trim().is_empty();
    let groups = owner_groups(&repositories, &state.filter().repos, &query.read());
    rsx! {
        div { class: "repo-search",
            input {
                class: "input input-sm",
                value: query(),
                placeholder: "filter repositories...",
                oninput: move |event| query.set(event.value()),
            }
        }
        div { class: "repo-tree",
            for group in groups {
                {
                    let owner = group.owner.clone();
                    let open = searching || !collapsed.read().contains(&owner);
                    let checkbox = group.checkbox();
                    let scope = group.scope.clone();
                    let select = checkbox != Checkbox::Checked;
                    let meta = group.meta();
                    rsx! {
                        div {
                            key: "{group.owner}",
                            class: if group.selected > 0 { "owner-group selected" } else { "owner-group" },
                            div { class: "owner-row",
                                button {
                                    class: checkbox.class(),
                                    aria_label: "select all {group.owner} repositories",
                                    onclick: move |_| state.select_repos(scope.clone(), select),
                                    "{checkbox.mark()}"
                                }
                                button {
                                    class: "owner-toggle",
                                    onclick: move |_| {
                                        let mut collapsed = collapsed.write();
                                        if !collapsed.remove(&owner) {
                                            collapsed.insert(owner.clone());
                                        }
                                    },
                                    span { class: "caret", if open { "v" } else { ">" } }
                                    span { class: "owner-name", "{group.owner}" }
                                    span { class: "owner-meta", "{meta}" }
                                }
                            }
                            if open {
                                for repository in group.repositories {
                                    {
                                        let full_name = repository.full_name.clone();
                                        let checkbox = if repository.selected { Checkbox::Checked } else { Checkbox::Unchecked };
                                        rsx! {
                                            button {
                                                key: "{repository.repository_id}",
                                                class: if repository.selected { "repo-filter active" } else { "repo-filter" },
                                                onclick: move |_| state.toggle_filter(|filter| &mut filter.repos, full_name.clone()),
                                                span { class: checkbox.class(), "{checkbox.mark()}" }
                                                span { class: "repo-name", "{repository.name}" }
                                                code { "{repository.count}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One owner's row of the tree and the repositories shown under it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnerGroup {
    owner: String,
    /// The repositories on show, in the order they arrived.
    repositories: Vec<RepoItem>,
    /// The repositories (as `owner/repo`) the owner's checkbox speaks for:
    /// every one of the owner's without a search, on show or not, and just
    /// the matches on show under one, so the box acts on what the user sees.
    scope: Vec<String>,
    /// How many of `scope` are selected.
    selected: usize,
    /// The owner's selected repositories a search has hidden, so the row can
    /// say the checkbox does not speak for them.
    hidden_selected: usize,
}

/// One repository of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RepoItem {
    repository_id: u64,
    /// `owner/repo`, the form the filter names repositories in.
    full_name: String,
    name: String,
    count: u64,
    selected: bool,
}

/// The state of an owner's checkbox, over the repositories in its scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkbox {
    Unchecked,
    /// Some of the repositories are selected, not all.
    Mixed,
    Checked,
}

impl Checkbox {
    fn class(self) -> &'static str {
        match self {
            Self::Unchecked => "selection-box",
            Self::Mixed => "selection-box mixed",
            Self::Checked => "selection-box checked",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Unchecked => "",
            Self::Mixed => "-",
            Self::Checked => "x",
        }
    }
}

impl OwnerGroup {
    fn new(owner: &str) -> Self {
        Self {
            owner: owner.to_owned(),
            repositories: Vec::new(),
            scope: Vec::new(),
            selected: 0,
            hidden_selected: 0,
        }
    }

    fn checkbox(&self) -> Checkbox {
        if self.selected == 0 {
            Checkbox::Unchecked
        } else if self.selected == self.scope.len() {
            Checkbox::Checked
        } else {
            Checkbox::Mixed
        }
    }

    /// What the row says after the owner: how many repositories are shown, or
    /// how many of the box's scope are selected once any is; how many pull
    /// requests the shown ones have between them; and how many selected
    /// repositories the search is hiding.
    fn meta(&self) -> String {
        let total: u64 = self
            .repositories
            .iter()
            .map(|repository| repository.count)
            .sum();
        let mut meta = if self.selected > 0 {
            format!("{}/{} · {total}", self.selected, self.scope.len())
        } else {
            let shown = self.repositories.len();
            let noun = if shown == 1 { "repo" } else { "repos" };
            format!("{shown} {noun} · {total}")
        };
        if self.hidden_selected > 0 {
            meta.push_str(&format!(" +{} hidden", self.hidden_selected));
        }
        meta
    }
}

/// Groups `repositories` under their owners, in the order they arrive, which
/// the store makes owner then name order. A repository is shown when it has
/// pull requests to show or is `selected` (as `owner/repo`): a selected one
/// never disappears from under the user. When `query` is not blank, only
/// repositories whose `owner/repo` contains it, case-insensitively, are
/// shown. An owner with nothing to show is left out.
fn owner_groups(repositories: &[RepoFacet], selected: &[String], query: &str) -> Vec<OwnerGroup> {
    let query = query.trim().to_lowercase();
    let searching = !query.is_empty();
    let mut groups: Vec<OwnerGroup> = Vec::new();
    for facet in repositories {
        let repository = &facet.repository;
        let full_name = format!("{}/{}", repository.owner, repository.repo);
        let is_selected = selected.contains(&full_name);
        let group = match groups
            .iter()
            .position(|group| group.owner == repository.owner)
        {
            Some(index) => &mut groups[index],
            None => {
                groups.push(OwnerGroup::new(&repository.owner));
                groups.last_mut().expect("just pushed")
            }
        };
        let matches = !searching || full_name.to_lowercase().contains(&query);
        let shown = matches && (facet.count > 0 || is_selected);
        let in_scope = if searching { shown } else { true };
        if in_scope {
            group.scope.push(full_name.clone());
            group.selected += usize::from(is_selected);
        } else if is_selected {
            group.hidden_selected += 1;
        }
        if shown {
            group.repositories.push(RepoItem {
                repository_id: repository.repository_id,
                full_name,
                name: repository.repo.clone(),
                count: facet.count,
                selected: is_selected,
            });
        }
    }
    groups.retain(|group| !group.repositories.is_empty());
    groups
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::PrFilter;

    use super::*;
    use crate::ui::dashboard_state::SummaryStatus;
    use crate::ui::test_support::{DashboardFixture, loaded_summary, render};

    fn fixture_repositories() -> Vec<RepoFacet> {
        loaded_summary().facets.repositories
    }

    fn names(group: &OwnerGroup) -> Vec<&str> {
        group
            .repositories
            .iter()
            .map(|repository| repository.name.as_str())
            .collect()
    }

    #[test]
    fn repositories_group_under_their_owner_and_an_empty_one_is_left_out() {
        let groups = owner_groups(&fixture_repositories(), &[], "");

        assert_eq!(
            groups
                .iter()
                .map(|group| (group.owner.as_str(), names(group)))
                .collect::<Vec<_>>(),
            [("acme", vec!["api", "web"]), ("beta", vec!["api"])],
            "acme/docs has no pull requests to show"
        );
        assert_eq!(groups[0].checkbox(), Checkbox::Unchecked);
        assert_eq!(groups[0].meta(), "2 repos · 52");
        assert_eq!(groups[1].meta(), "1 repo · 3");
        assert_eq!(groups[0].repositories[0].full_name, "acme/api");
        assert_eq!(
            groups[0].scope,
            ["acme/api", "acme/docs", "acme/web"],
            "the owner's box speaks for the whole account, the hidden repository included"
        );
    }

    #[test]
    fn a_selected_repository_is_shown_even_without_pull_requests() {
        let selected = ["acme/docs".to_owned(), "acme/web".to_owned()];

        let groups = owner_groups(&fixture_repositories(), &selected, "");

        assert_eq!(names(&groups[0]), ["api", "docs", "web"]);
        assert_eq!(groups[0].selected, 2);
        assert_eq!(groups[0].checkbox(), Checkbox::Mixed);
        assert_eq!(groups[0].meta(), "2/3 · 52");
        assert!(groups[0].repositories[1].selected);
        assert!(!groups[0].repositories[0].selected);
    }

    /// Without a search the box stands for every repository of the owner, so
    /// it is checked only once they are all selected, a hidden one at zero
    /// included; selecting just the ones on show leaves it mixed.
    #[test]
    fn an_owner_is_checked_only_when_every_one_of_its_repositories_is_selected() {
        let repositories = fixture_repositories();

        let beta = owner_groups(&repositories, &["beta/api".to_owned()], "");
        assert_eq!(beta[1].checkbox(), Checkbox::Checked);
        assert_eq!(beta[1].meta(), "1/1 · 3");
        assert_eq!(beta[0].checkbox(), Checkbox::Unchecked);

        let shown_only = ["acme/api".to_owned(), "acme/web".to_owned()];
        let acme = owner_groups(&repositories, &shown_only, "");
        assert_eq!(acme[0].checkbox(), Checkbox::Mixed);
        assert_eq!(acme[0].meta(), "2/3 · 52");

        let whole_account = [
            "acme/api".to_owned(),
            "acme/docs".to_owned(),
            "acme/web".to_owned(),
        ];
        let acme = owner_groups(&repositories, &whole_account, "");
        assert_eq!(acme[0].checkbox(), Checkbox::Checked);
        assert_eq!(acme[0].meta(), "3/3 · 52");
    }

    /// The search matches `owner/repo` case-insensitively, so it can narrow
    /// to a repository name or to an owner; an owner with no match drops out;
    /// and under a search the box acts on exactly what is on screen, so a
    /// selected repository the search hides is counted separately, and the
    /// box is checked once the matches alone are selected.
    #[test]
    fn the_search_narrows_the_tree_and_scopes_the_owners_box_to_what_it_shows() {
        let repositories = fixture_repositories();
        let selected = ["acme/web".to_owned()];

        let by_name = owner_groups(&repositories, &selected, " API ");
        assert_eq!(
            by_name
                .iter()
                .map(|group| (group.owner.as_str(), names(group)))
                .collect::<Vec<_>>(),
            [("acme", vec!["api"]), ("beta", vec!["api"])]
        );
        assert_eq!(by_name[0].scope, ["acme/api"]);
        assert_eq!(by_name[0].hidden_selected, 1);
        assert_eq!(by_name[0].checkbox(), Checkbox::Unchecked);
        assert_eq!(by_name[0].meta(), "1 repo · 31 +1 hidden");
        assert_eq!(by_name[1].hidden_selected, 0);

        let matches_selected = owner_groups(&repositories, &["acme/api".to_owned()], "api");
        assert_eq!(matches_selected[0].checkbox(), Checkbox::Checked);
        assert_eq!(matches_selected[0].meta(), "1/1 · 31");

        let by_owner = owner_groups(&repositories, &selected, "beta");
        assert_eq!(
            by_owner
                .iter()
                .map(|group| group.owner.as_str())
                .collect::<Vec<_>>(),
            ["beta"]
        );

        assert!(owner_groups(&repositories, &selected, "nothing here").is_empty());
    }

    #[test]
    fn the_tree_marks_the_selected_repository_and_mixes_its_owners_checkbox() {
        fn Fixture() -> Element {
            let filter = PrFilter {
                repos: vec!["acme/web".to_owned(), "beta/api".to_owned()],
                ..Default::default()
            };
            rsx! {
                DashboardFixture { filter, summary: SummaryStatus::Loaded(loaded_summary()),
                    RepoTree { repositories: fixture_repositories() }
                }
            }
        }
        let html = render(Fixture);

        assert_eq!(html.matches(r#"class="owner-group"#).count(), 2, "{html}");
        assert_eq!(
            html.matches(r#"class="owner-group selected""#).count(),
            2,
            "{html}"
        );
        assert_eq!(
            html.matches(r#"class="selection-box mixed""#).count(),
            1,
            "{html}"
        );
        assert_eq!(
            html.matches(r#"class="selection-box checked""#).count(),
            3,
            "beta's box, acme/web, and beta/api: {html}"
        );
        assert_eq!(
            html.matches(r#"class="repo-filter active""#).count(),
            2,
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="repo-name">web</span><code>21</code>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="owner-meta">1/3 · 52</span>"#),
            "acme has three repositories, one selected: {html}"
        );
        assert!(!html.contains("docs"), "{html}");
        assert!(
            html.contains(r#"placeholder="filter repositories...""#),
            "{html}"
        );
    }
}
