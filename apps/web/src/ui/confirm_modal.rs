//! The confirmation dialog a bulk action passes through before it is queued.

use dependaboard_core::{BulkActionKind, MergeMethod, PrTarget, RepoRecord};
use dioxus::prelude::*;

use crate::components::alert_dialog::{
    AlertDialog, AlertDialogAction, AlertDialogActions, AlertDialogCancel, AlertDialogDescription,
    AlertDialogDescriptionAppearance, AlertDialogTitle, AlertDialogTitleAppearance,
};
use crate::ui::PendingAction;
use crate::ui::format::pull_requests;

/// The confirmation for a bulk action. Always mounted with a controlled open
/// state so the dialog restores focus when it closes; both buttons close the
/// dialog before their handler runs, so `oncancel` also fires ahead of
/// `onconfirm` and must not hold anything the latter needs.
///
/// `repositories` is the dashboard's repository list, consulted for a merge
/// so the dialog can name the repositories that disallow the configured
/// merge method and the method each will use instead.
#[component]
pub(crate) fn ConfirmModal(
    pending: Option<PendingAction>,
    repositories: Vec<RepoRecord>,
    oncancel: EventHandler<()>,
    onconfirm: EventHandler<PendingAction>,
) -> Element {
    rsx! {
        AlertDialog {
            id: "confirm-bulk-action",
            class: "confirm-box",
            open: Some(pending.is_some()),
            on_open_change: move |open: bool| {
                if !open {
                    oncancel.call(());
                }
            },
            if let Some(pending) = pending {
                {
                    let action = pending.action;
                    let count = pull_requests(pending.targets.len() as u64);
                    let overrides = merge_method_overrides(&pending.targets, &repositories);
                    rsx! {
                        span { class: "eyebrow", "Durable bulk action" }
                        AlertDialogTitle { appearance: AlertDialogTitleAppearance::None,
                            "{action} {count}?"
                        }
                        AlertDialogDescription { appearance: AlertDialogDescriptionAppearance::None,
                            "The selected head SHAs are captured now. Moved or ineligible pull requests will be rejected, not silently retried against new code."
                        }
                        match action {
                            BulkActionKind::Merge => rsx! {
                                if overrides.is_empty() {
                                    div { class: "notice", "Uses the configured merge method." }
                                } else {
                                    div { class: "notice",
                                        "Uses the configured merge method where the repository allows it."
                                        ul { class: "merge-method-overrides",
                                            for (repository, method) in overrides {
                                                li { key: "{repository}",
                                                    code { "{repository}" }
                                                    " disallows it and will use "
                                                    code { "{method}" }
                                                    "."
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            BulkActionKind::Rebase => rsx! {
                                div { class: "notice", "Rebase is requested by an idempotent @dependabot comment using the configured user token." }
                            },
                            BulkActionKind::UpdateBranch => rsx! {
                                div { class: "notice",
                                    "The App merges the base branch into the pull request, so checks re-run against it. Dependabot stops rebasing a pull request once another commit lands on it."
                                }
                            },
                        }
                        AlertDialogActions {
                            AlertDialogCancel { class: "btn-ghost btn-sm", "Cancel" }
                            AlertDialogAction {
                                class: "btn-sm confirm-button",
                                on_click: move |_| onconfirm.call(pending.clone()),
                                "Queue {action}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The repositories among `targets` whose last sync resolved a merge method
/// other than the configured preference, each named once as `owner/repo`
/// with that method, in the order the targets first mention them.
fn merge_method_overrides(
    targets: &[PrTarget],
    repositories: &[RepoRecord],
) -> Vec<(String, MergeMethod)> {
    let mut overrides: Vec<(String, MergeMethod)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for target in targets {
        if !seen.insert(target.repository_id) {
            continue;
        }
        let Some(method) = repositories
            .iter()
            .find(|repository| repository.repository_id == target.repository_id)
            .and_then(|repository| repository.merge_method)
        else {
            continue;
        };
        overrides.push((format!("{}/{}", target.owner, target.repo), method));
    }
    overrides
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{MergeMethod, PrTarget, RepoRecord};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::grouped_row;

    fn repository(id: u64, repo: &str, merge_method: Option<MergeMethod>) -> RepoRecord {
        RepoRecord {
            repository_id: id,
            installation_id: 1,
            owner: "acme".to_owned(),
            repo: repo.to_owned(),
            merge_method,
            synced_at: 0,
        }
    }

    fn target_in(repository: &RepoRecord, number: u64) -> PrTarget {
        let mut row = grouped_row();
        row.repository_id = repository.repository_id;
        row.repo = repository.repo.clone();
        row.number = number;
        pr_target(&row)
    }

    fn render(
        action: BulkActionKind,
        targets: Vec<PrTarget>,
        repositories: Vec<RepoRecord>,
    ) -> String {
        #[component]
        fn Fixture(
            action: BulkActionKind,
            targets: Vec<PrTarget>,
            repositories: Vec<RepoRecord>,
        ) -> Element {
            rsx! {
                ConfirmModal {
                    pending: Some(PendingAction { action, targets }),
                    repositories,
                    oncancel: move |_| {},
                    onconfirm: move |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(
            Fixture,
            FixtureProps {
                action,
                targets,
                repositories,
            },
        );
        dom.rebuild_in_place();
        // The dialog primitive mounts its content from an effect that runs
        // after the first render, so one more pass is what puts it in the DOM.
        dom.render_immediate_to_vec();
        dioxus::ssr::render(&dom)
    }

    #[test]
    fn a_merge_names_each_repository_that_will_not_use_the_preferred_method() {
        let api = repository(7, "api", None);
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let docs = repository(9, "docs", Some(MergeMethod::Rebase));
        let html = render(
            BulkActionKind::Merge,
            // Two pull requests in `web` name it once; `docs` is not in the batch.
            vec![target_in(&api, 1), target_in(&web, 2), target_in(&web, 3)],
            vec![api, web, docs],
        );

        assert_eq!(html.matches("acme/web").count(), 1, "{html}");
        assert!(
            html.contains("<code>acme/web</code> disallows it and will use <code>merge</code>"),
            "{html}"
        );
        assert!(!html.contains("acme/docs"), "{html}");
        assert!(!html.contains("acme/api"), "{html}");
    }

    #[test]
    fn a_merge_where_every_repository_allows_the_preferred_method_says_so_plainly() {
        let api = repository(7, "api", None);
        let html = render(
            BulkActionKind::Merge,
            vec![target_in(&api, 1)],
            vec![api, repository(8, "web", Some(MergeMethod::Merge))],
        );

        assert!(html.contains("Uses the configured merge method."), "{html}");
        assert!(!html.contains("disallows"), "{html}");
    }

    #[test]
    fn a_rebase_never_mentions_merge_methods() {
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let html = render(BulkActionKind::Rebase, vec![target_in(&web, 2)], vec![web]);

        assert!(html.contains("@dependabot"), "{html}");
        assert!(!html.contains("merge method"), "{html}");
    }

    #[test]
    fn a_branch_update_explains_it_is_the_app_merging_the_base_in_not_a_dependabot_rebase() {
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let html = render(
            BulkActionKind::UpdateBranch,
            vec![target_in(&web, 2)],
            vec![web],
        );

        assert!(html.contains("update branch 1 pull request?"), "{html}");
        assert!(html.contains("Queue update branch"), "{html}");
        assert!(
            html.contains("merges the base branch into the pull request"),
            "{html}"
        );
        assert!(html.contains("stops rebasing"), "{html}");
        assert!(!html.contains("@dependabot"), "{html}");
        assert!(!html.contains("merge method"), "{html}");
    }

    #[test]
    fn the_title_counts_one_pull_request_in_the_singular() {
        let web = repository(8, "web", None);

        let one = render(BulkActionKind::Merge, vec![target_in(&web, 2)], vec![]);
        assert!(one.contains("merge 1 pull request?"), "{one}");

        let two = render(
            BulkActionKind::Rebase,
            vec![target_in(&web, 2), target_in(&web, 3)],
            vec![],
        );
        assert!(two.contains("rebase 2 pull requests?"), "{two}");
    }
}
