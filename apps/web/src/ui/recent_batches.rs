//! The drawer that lists the batches: the audit view. Restate keeps a batch's
//! progress only for the workflow's retention; the projection keeps the
//! finished batch for good, and lists the running ones while they run, and this
//! is where the dashboard shows both — so a tab that lost a batch, or never
//! followed it, can find it and follow it.

use dependaboard_core::{
    BatchList, BatchRecord, MAX_RECENT_BATCHES, RunningBatch, TargetProgressState,
};
use dioxus::prelude::*;

use crate::api::load_recent_batches;
use crate::components::button::{Button, ButtonSize};
use crate::ui::dashboard_state::{Remote, use_dashboard};
use crate::ui::format::{pull_requests, relative_time};
use crate::ui::progress_drawer::TargetRow;
use crate::ui::side_panel::SidePanel;

/// How many finished batches the drawer asks for when it opens, and how many
/// more each **Show older** asks for.
const BATCH_PAGE: u32 = 20;

/// The batches as the dashboard has heard them from the read model.
pub(crate) type BatchesStatus = Remote<BatchList>;

/// The drawer. It asks for every running batch and the newest [`BATCH_PAGE`]
/// finished ones when it opens, each time it opens, so a batch that starts or
/// finishes while it is showing is there the next time; **Show older** asks
/// for a page more, as far back as [`MAX_RECENT_BATCHES`]. `onfollow` is asked
/// with the id of a running batch the user wants to follow.
#[component]
pub(crate) fn RecentBatchesDrawer(
    onclose: EventHandler<()>,
    onfollow: EventHandler<String>,
) -> Element {
    let state = use_dashboard();
    let mut limit = use_signal(|| BATCH_PAGE);
    let batches = use_resource(move || {
        let limit = limit();
        async move { load_recent_batches(limit).await }
    });
    let status = BatchesStatus::from_resource(batches.read().as_ref());
    // A page that came back full may have older batches behind it; a short
    // one is the whole record.
    let may_have_older = limit() < MAX_RECENT_BATCHES
        && status
            .loaded()
            .is_some_and(|batches| batches.finished.len() >= limit() as usize);
    rsx! {
        SidePanel {
            class: "batches-drawer",
            eyebrow: "Audit",
            title: rsx! { "Recent batches" },
            onclose,
            RecentBatchList {
                batches: status,
                now: state.now(),
                may_have_older,
                onolder: move |_| limit.set((limit() + BATCH_PAGE).min(MAX_RECENT_BATCHES)),
                onfollow,
            }
        }
    }
}

/// The drawer's body: the running batches first, each with an offer to follow
/// it, then the finished ones newest first, each folded to a headline until
/// opened, when it lists every target with its verdict. `now` is the
/// dashboard's clock, which the times are read against. While
/// `may_have_older`, the list ends with an offer to show older batches, which
/// asks `onolder`.
#[component]
pub(crate) fn RecentBatchList(
    batches: BatchesStatus,
    now: u64,
    may_have_older: bool,
    onolder: EventHandler<()>,
    onfollow: EventHandler<String>,
) -> Element {
    match batches {
        Remote::Loading => rsx! { p { class: "batches-note", "Loading recent batches..." } },
        Remote::Failed(error) => rsx! { p { class: "batches-note batches-error", "{error}" } },
        Remote::Loaded(batches) if batches.running.is_empty() && batches.finished.is_empty() => {
            rsx! {
                p { class: "batches-note", "No batches have run yet." }
            }
        }
        Remote::Loaded(batches) => rsx! {
            div { class: "batch-list",
                for batch in batches.running {
                    RunningEntry { key: "{batch.batch_id}", batch, now, onfollow }
                }
                for batch in batches.finished {
                    BatchEntry { key: "{batch.batch_id}", batch, now }
                }
            }
            if may_have_older {
                div { class: "progress-footer",
                    Button {
                        size: ButtonSize::Sm,
                        class: "older-batches-button",
                        onclick: move |_| onolder.call(()),
                        "Show older"
                    }
                }
            }
        },
    }
}

/// A batch still running: what was asked, by whom, since when, and over how
/// many pull requests, with **Follow** to make it the batch the dashboard
/// follows. Its verdicts are Restate's to give, and following it is how they
/// show.
#[component]
fn RunningEntry(batch: RunningBatch, now: u64, onfollow: EventHandler<String>) -> Element {
    let batch_id = batch.batch_id.clone();
    rsx! {
        div { class: "batch-entry batch-running",
            div { class: "batch-headline",
                div {
                    strong { class: "batch-action", "{batch.action}" }
                    span { class: "batch-tally", "running" }
                }
                small { class: "batch-meta",
                    "{pull_requests(batch.target_count)} · by {batch.requested_by} · started {relative_time(now, batch.started_at)}"
                }
            }
            div { class: "progress-footer",
                Button {
                    size: ButtonSize::Sm,
                    class: "follow-batch-button",
                    onclick: move |_| onfollow.call(batch_id.clone()),
                    "Follow"
                }
            }
        }
    }
}

#[component]
fn BatchEntry(batch: BatchRecord, now: u64) -> Element {
    let total = batch.targets.len() as u64;
    rsx! {
        details { class: "batch-entry",
            summary { class: "batch-headline",
                div {
                    strong { class: "batch-action", "{batch.action}" }
                    span { class: "batch-tally",
                        "{batch.succeeded} succeeded, {batch.rejected} rejected, {batch.failed} failed"
                    }
                }
                small { class: "batch-meta",
                    "{pull_requests(total)} · by {batch.requested_by} · finished {relative_time(now, batch.completed_at)}"
                }
            }
            div { class: "progress-list",
                for target in &batch.targets {
                    TargetRow {
                        owner: target.owner.clone(),
                        repo: target.repo.clone(),
                        number: target.number,
                        html_url: target.html_url.clone(),
                        state: TargetProgressState::from(target.outcome.clone()),
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{
        BatchTargetRecord, BulkActionKind, RejectReason, RunningBatch, TargetOutcome, UserId,
    };

    use super::*;
    use crate::ui::test_support::{FIXTURE_NOW, render};

    /// A merge of three pull requests `carol` asked for five minutes ago,
    /// still running.
    fn running_batch() -> RunningBatch {
        RunningBatch {
            batch_id: "batch-3".to_owned(),
            action: BulkActionKind::Merge,
            requested_by: UserId::new("carol"),
            started_at: FIXTURE_NOW - 5 * 60,
            target_count: 3,
        }
    }

    /// [`recent_batches`] as the server lists them, with nothing running.
    fn finished() -> BatchList {
        BatchList {
            running: Vec::new(),
            finished: recent_batches(),
        }
    }

    /// Two finished batches: a merge from two hours ago in which one pull request was
    /// merged and one rejected as stale, and a rebase from just now that failed.
    fn recent_batches() -> Vec<BatchRecord> {
        let target = BatchTargetRecord {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            title: "Bump serde".to_owned(),
            html_url: "https://github.example/acme/api/pull/9".to_owned(),
            outcome: TargetOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        };
        vec![
            BatchRecord {
                batch_id: "batch-2".to_owned(),
                action: BulkActionKind::Rebase,
                requested_by: UserId::new("bob"),
                started_at: FIXTURE_NOW - 40,
                completed_at: FIXTURE_NOW - 10,
                succeeded: 0,
                rejected: 0,
                failed: 1,
                targets: vec![BatchTargetRecord {
                    number: 12,
                    html_url: "https://github.example/acme/api/pull/12".to_owned(),
                    outcome: TargetOutcome::Failed {
                        detail: "GitHub mutation failed with HTTP 500: Internal Server Error"
                            .to_owned(),
                    },
                    ..target.clone()
                }],
            },
            BatchRecord {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                started_at: FIXTURE_NOW - 2 * 3_600 - 30,
                completed_at: FIXTURE_NOW - 2 * 3_600,
                succeeded: 1,
                rejected: 1,
                failed: 0,
                targets: vec![
                    target.clone(),
                    BatchTargetRecord {
                        number: 10,
                        html_url: "https://github.example/acme/api/pull/10".to_owned(),
                        outcome: TargetOutcome::Rejected {
                            reason: RejectReason::StaleSha {
                                expected: "abc1234def".to_owned(),
                                actual: "def4567abc".to_owned(),
                            },
                        },
                        ..target
                    },
                ],
            },
        ]
    }

    #[test]
    fn each_batch_is_listed_with_its_tally_its_requester_its_age_and_a_link_per_target() {
        fn Fixture() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Loaded(finished()),
                    now: FIXTURE_NOW,
                    may_have_older: false,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let html = render(Fixture);

        assert_eq!(
            html.matches("<details class=\"batch-entry\"").count(),
            2,
            "{html}"
        );
        let rebase = html.find("rebase").expect("the rebase is listed");
        let merge = html.find(">merge<").expect("the merge is listed");
        assert!(rebase < merge, "newest first: {html}");
        assert!(html.contains("0 succeeded, 0 rejected, 1 failed"), "{html}");
        assert!(html.contains("1 succeeded, 1 rejected, 0 failed"), "{html}");
        assert!(
            html.contains("1 pull request · by bob · finished now"),
            "{html}"
        );
        assert!(
            html.contains("2 pull requests · by alice · finished 2h"),
            "{html}"
        );
        for href in [
            "https://github.example/acme/api/pull/9",
            "https://github.example/acme/api/pull/10",
            "https://github.example/acme/api/pull/12",
        ] {
            assert!(
                html.contains(&format!(
                    r#"<a class="github-pr-link" href="{href}" target="_blank" rel="noreferrer">"#
                )),
                "{href} is linked: {html}"
            );
        }
        assert!(html.contains("acme/api#10"), "{html}");
        assert!(
            html.contains("head moved from abc1234 to def4567"),
            "the rejected target carries its reason: {html}"
        );
        assert!(
            html.contains("GitHub mutation failed with HTTP 500: Internal Server Error"),
            "the failed target carries its reason: {html}"
        );
        assert!(
            html.contains(r#"class="progress-state state-rejected""#),
            "{html}"
        );
        assert!(
            !html.contains(OLDER_BUTTON),
            "nothing older to show: {html}"
        );
    }

    /// The list is a page of the newest; a batch from before the page is still on
    /// record and reachable, so the list offers to go further back while it is full.
    #[test]
    fn a_full_page_offers_to_show_older_batches() {
        fn Fixture() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Loaded(finished()),
                    now: FIXTURE_NOW,
                    may_have_older: true,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(OLDER_BUTTON), "{html}");
        assert!(html.contains(">Show older<"), "{html}");
    }

    const OLDER_BUTTON: &str = "older-batches-button";

    /// A tab that lost a batch, or never followed it, finds it here: a batch
    /// still running is listed above the finished ones, said to be running,
    /// with who asked for it and when, and offers to be followed. Its
    /// verdicts are not yet its own to list; following it is how they show.
    #[test]
    fn a_running_batch_is_listed_first_as_running_and_offers_to_be_followed() {
        fn Fixture() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Loaded(BatchList {
                        running: vec![running_batch()],
                        finished: recent_batches(),
                    }),
                    now: FIXTURE_NOW,
                    may_have_older: false,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let html = render(Fixture);

        let running = html
            .find("3 pull requests · by carol · started 5m")
            .expect("the running batch is listed");
        let rebase = html.find("rebase").expect("the rebase is listed");
        assert!(running < rebase, "running first: {html}");
        assert!(
            html.contains(r#"<span class="batch-tally">running</span>"#),
            "{html}"
        );
        assert!(html.contains(FOLLOW_BUTTON), "{html}");
        assert!(html.contains(">Follow<"), "{html}");
        assert_eq!(
            html.matches(FOLLOW_BUTTON).count(),
            1,
            "a finished batch is not offered to follow: {html}"
        );
    }

    const FOLLOW_BUTTON: &str = "follow-batch-button";

    #[test]
    fn an_empty_history_says_so_and_a_loading_one_says_it_is_loading() {
        fn Empty() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Loaded(BatchList::default()),
                    now: FIXTURE_NOW,
                    may_have_older: false,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let empty = render(Empty);
        assert!(empty.contains("No batches have run yet."), "{empty}");
        assert!(!empty.contains("batch-entry"), "{empty}");

        fn Loading() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Loading,
                    now: FIXTURE_NOW,
                    may_have_older: false,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let loading = render(Loading);
        assert!(loading.contains("Loading recent batches..."), "{loading}");

        fn Failed() -> Element {
            rsx! {
                RecentBatchList {
                    batches: Remote::Failed("The read model is unavailable".to_owned()),
                    now: FIXTURE_NOW,
                    may_have_older: false,
                    onolder: move |_| {},
                    onfollow: move |_| {},
                }
            }
        }
        let failed = render(Failed);
        assert!(
            failed.contains(
                r#"<p class="batches-note batches-error">The read model is unavailable</p>"#
            ),
            "{failed}"
        );
    }
}
