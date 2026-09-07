//! The confirmation dialog a bulk action passes through before it is queued.

use std::collections::BTreeSet;

use dependaboard_core::{BulkActionKind, MergeMethod, PrRecord, RepoRecord};
use dioxus::prelude::*;

use crate::components::alert_dialog::{
    AlertDialog, AlertDialogAction, AlertDialogActions, AlertDialogCancel, AlertDialogDescription,
    AlertDialogDescriptionAppearance, AlertDialogTitle, AlertDialogTitleAppearance,
};
use crate::ui::dashboard_state::Remote;
use crate::ui::format::{pull_requests, repositories as repository_phrase};
use crate::ui::{PendingAction, repository_count};

/// The confirmation for a bulk action. Always mounted with a controlled open
/// state so the dialog restores focus when it closes; both buttons close the
/// dialog before their handler runs, so `oncancel` also fires ahead of
/// `onconfirm` and must not hold anything the latter needs.
///
/// The dialog sums the rows up: how many, across how many repositories, and,
/// for a merge, how many are not green, since the check status on the
/// dashboard is the read model's last word and not a gate.
///
/// `repositories` is the dashboard's repository list as the summary has it,
/// consulted for a merge so the dialog can name the repositories that
/// disallow the configured merge method and the method each will use
/// instead. Until the list is in hand — still being read, or failed — the
/// dialog cannot say which method a merge would use, and withholds the
/// confirm rather than claim the configured one everywhere.
#[component]
pub(crate) fn ConfirmModal(
    pending: Option<PendingAction>,
    repositories: Remote<Vec<RepoRecord>>,
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
                    let count = pull_requests(pending.rows.len() as u64);
                    let span = repository_phrase(repository_count(&pending.rows));
                    let not_green = not_green_count(&pending.rows);
                    let withheld = action == BulkActionKind::Merge && repositories.loaded().is_none();
                    rsx! {
                        span { class: "eyebrow", "Durable bulk action" }
                        AlertDialogTitle { appearance: AlertDialogTitleAppearance::None,
                            "{action} {count}?"
                        }
                        AlertDialogDescription { appearance: AlertDialogDescriptionAppearance::None,
                            "Across {span}. The selected head SHAs are captured now. Moved or ineligible pull requests will be rejected, not silently retried against new code."
                        }
                        match action {
                            BulkActionKind::Merge => rsx! {
                                if not_green > 0 {
                                    div { class: "notice not-green",
                                        strong { "{not_green_phrase(not_green)}" }
                                        " The check status here is the read model's last word, not a gate: unless branch protection requires the checks, GitHub merges them as they are."
                                    }
                                }
                                MergeMethodNotice { rows: pending.rows.clone(), repositories: repositories.clone() }
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
                                disabled: withheld,
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

/// What a merge of `rows` says about merge methods: which repositories will
/// not use the configured one, or why that cannot be said yet.
#[component]
fn MergeMethodNotice(rows: Vec<PrRecord>, repositories: Remote<Vec<RepoRecord>>) -> Element {
    match &repositories {
        Remote::Loading => rsx! {
            div { class: "notice",
                "The merge method each repository allows is still being read; the merge can be queued once it is known."
            }
        },
        Remote::Failed(error) => rsx! {
            div { class: "notice merge-methods-unknown",
                strong { "The merge method each repository allows could not be read." }
                " "
                code { "{error}" }
                " Retry it from the sidebar before merging: without it, the dialog cannot say which method each repository would use."
            }
        },
        Remote::Loaded(repositories) => {
            let overrides = merge_method_overrides(&rows, repositories);
            rsx! {
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
            }
        }
    }
}

/// How many of `rows` the read model last saw with a rollup other than
/// passing.
fn not_green_count(rows: &[PrRecord]) -> usize {
    rows.iter()
        .filter(|row| !row.check_status.is_green())
        .count()
}

/// "1 of these is not green.", "2 of these are not green."
fn not_green_phrase(count: usize) -> String {
    match count {
        1 => "1 of these is not green.".to_owned(),
        count => format!("{count} of these are not green."),
    }
}

/// The repositories among `rows` whose last sync resolved a merge method
/// other than the configured preference, each named once as `owner/repo`
/// with that method, in the order the rows first mention them.
fn merge_method_overrides(
    rows: &[PrRecord],
    repositories: &[RepoRecord],
) -> Vec<(String, MergeMethod)> {
    let mut overrides: Vec<(String, MergeMethod)> = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        if !seen.insert(row.repository_id) {
            continue;
        }
        let Some(method) = repositories
            .iter()
            .find(|repository| repository.repository_id == row.repository_id)
            .and_then(|repository| repository.merge_method)
        else {
            continue;
        };
        overrides.push((format!("{}/{}", row.owner, row.repo), method));
    }
    overrides
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{CheckStatus, MergeMethod, PrRecord, RepoRecord};

    use super::*;
    use crate::ui::dashboard_state::Remote;
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

    fn row_in(repository: &RepoRecord, number: u64) -> PrRecord {
        let mut row = grouped_row();
        row.id = format!("{}#{number}", repository.repository_id);
        row.repository_id = repository.repository_id;
        row.repo = repository.repo.clone();
        row.number = number;
        row
    }

    fn render(
        action: BulkActionKind,
        rows: Vec<PrRecord>,
        repositories: Vec<RepoRecord>,
    ) -> String {
        render_with(action, rows, Remote::Loaded(repositories))
    }

    fn render_with(
        action: BulkActionKind,
        rows: Vec<PrRecord>,
        repositories: Remote<Vec<RepoRecord>>,
    ) -> String {
        #[component]
        fn Fixture(
            action: BulkActionKind,
            rows: Vec<PrRecord>,
            repositories: Remote<Vec<RepoRecord>>,
        ) -> Element {
            rsx! {
                ConfirmModal {
                    pending: Some(PendingAction {
                        action,
                        rows,
                        retried_from: None,
                    }),
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
                rows,
                repositories,
            },
        );
        dom.rebuild_in_place();
        // The dialog primitive mounts its content from an effect that runs
        // after the first render, so one more pass is what puts it in the DOM.
        dom.render_immediate_to_vec();
        dioxus::ssr::render(&dom)
    }

    /// Which merge method each repository will use is read off the summary.
    /// Until that is in hand — still being read, or failed — the dialog
    /// cannot say, and rather than claim every repository uses the configured
    /// method, it says why it cannot and withholds the confirm. A rebase or
    /// branch update has no merge method to speak of and goes ahead.
    #[test]
    fn a_merge_is_withheld_with_its_reason_while_the_merge_methods_are_unknown() {
        let web = repository(8, "web", Some(MergeMethod::Merge));

        let loading = render_with(
            BulkActionKind::Merge,
            vec![row_in(&web, 2)],
            Remote::Loading,
        );
        assert!(
            loading.contains("merge method each repository allows is still being read"),
            "{loading}"
        );
        assert_eq!(
            loading.matches("disabled=true").count(),
            1,
            "the confirm alone is withheld: {loading}"
        );
        assert!(
            !loading.contains("Uses the configured merge method"),
            "{loading}"
        );

        let failed = render_with(
            BulkActionKind::Merge,
            vec![row_in(&web, 2)],
            Remote::Failed("the store is away".to_owned()),
        );
        assert!(
            failed.contains("merge method each repository allows could not be read"),
            "{failed}"
        );
        assert!(
            failed.contains("<code>the store is away</code>"),
            "{failed}"
        );
        assert!(failed.contains("Retry it from the sidebar"), "{failed}");
        assert_eq!(
            failed.matches("disabled=true").count(),
            1,
            "the confirm alone is withheld: {failed}"
        );
        assert!(
            !failed.contains("Uses the configured merge method"),
            "{failed}"
        );

        let rebase = render_with(
            BulkActionKind::Rebase,
            vec![row_in(&web, 2)],
            Remote::Failed("the store is away".to_owned()),
        );
        assert!(!rebase.contains("disabled"), "{rebase}");
        assert!(!rebase.contains("merge method"), "{rebase}");
    }

    #[test]
    fn a_merge_names_each_repository_that_will_not_use_the_preferred_method() {
        let api = repository(7, "api", None);
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let docs = repository(9, "docs", Some(MergeMethod::Rebase));
        let html = render(
            BulkActionKind::Merge,
            // Two pull requests in `web` name it once; `docs` is not in the batch.
            vec![row_in(&api, 1), row_in(&web, 2), row_in(&web, 3)],
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
            vec![row_in(&api, 1)],
            vec![api, repository(8, "web", Some(MergeMethod::Merge))],
        );

        assert!(html.contains("Uses the configured merge method."), "{html}");
        assert!(!html.contains("disallows"), "{html}");
    }

    #[test]
    fn a_rebase_never_mentions_merge_methods() {
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let html = render(BulkActionKind::Rebase, vec![row_in(&web, 2)], vec![web]);

        assert!(html.contains("@dependabot"), "{html}");
        assert!(!html.contains("merge method"), "{html}");
    }

    #[test]
    fn a_branch_update_explains_it_is_the_app_merging_the_base_in_not_a_dependabot_rebase() {
        let web = repository(8, "web", Some(MergeMethod::Merge));
        let html = render(
            BulkActionKind::UpdateBranch,
            vec![row_in(&web, 2)],
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

        let one = render(BulkActionKind::Merge, vec![row_in(&web, 2)], vec![]);
        assert!(one.contains("merge 1 pull request?"), "{one}");

        let two = render(
            BulkActionKind::Rebase,
            vec![row_in(&web, 2), row_in(&web, 3)],
            vec![],
        );
        assert!(two.contains("rebase 2 pull requests?"), "{two}");
    }

    /// Whatever the action, the dialog says how many repositories the rows
    /// span, since that is what the batch fans out over.
    #[test]
    fn the_dialog_counts_the_repositories_the_rows_span() {
        let api = repository(7, "api", None);
        let web = repository(8, "web", None);

        let two = render(
            BulkActionKind::Rebase,
            vec![row_in(&api, 1), row_in(&web, 2), row_in(&web, 3)],
            vec![],
        );
        assert!(two.contains("Across 2 repositories."), "{two}");

        let one = render(BulkActionKind::UpdateBranch, vec![row_in(&web, 2)], vec![]);
        assert!(one.contains("Across 1 repository."), "{one}");
    }

    /// A merge says how many of its rows are not green, since the check
    /// status on the dashboard is the read model's last word and not a gate;
    /// nothing is said when every row is green. A rebase or branch update is
    /// what one does to a row that is not green, so neither mentions it.
    #[test]
    fn a_merge_warns_how_many_of_its_rows_are_not_green() {
        let web = repository(8, "web", None);
        let mut green = row_in(&web, 2);
        green.check_status = CheckStatus::Success;
        let mut pending = row_in(&web, 3);
        pending.check_status = CheckStatus::Pending;
        let failing = row_in(&web, 4);

        let mixed = render(
            BulkActionKind::Merge,
            vec![green.clone(), pending, failing.clone()],
            vec![],
        );
        assert!(mixed.contains("2 of these are not green."), "{mixed}");

        let one = render(
            BulkActionKind::Merge,
            vec![green.clone(), failing.clone()],
            vec![],
        );
        assert!(one.contains("1 of these is not green."), "{one}");

        let all_green = render(BulkActionKind::Merge, vec![green], vec![]);
        assert!(!all_green.contains("not green"), "{all_green}");

        let rebase = render(BulkActionKind::Rebase, vec![failing.clone()], vec![]);
        assert!(!rebase.contains("not green"), "{rebase}");

        let update = render(BulkActionKind::UpdateBranch, vec![failing], vec![]);
        assert!(!update.contains("not green"), "{update}");
    }
}
