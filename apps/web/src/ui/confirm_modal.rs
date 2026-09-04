//! The confirmation dialog a bulk action passes through before it is queued.

use dependaboard_core::BulkActionKind;
use dioxus::prelude::*;

use crate::components::alert_dialog::{
    AlertDialog, AlertDialogAction, AlertDialogActions, AlertDialogCancel, AlertDialogDescription,
    AlertDialogDescriptionAppearance, AlertDialogTitle, AlertDialogTitleAppearance,
};
use crate::ui::PendingAction;

/// The confirmation for a bulk action. Always mounted with a controlled open
/// state so the dialog restores focus when it closes; both buttons close the
/// dialog before their handler runs, so `oncancel` also fires ahead of
/// `onconfirm` and must not hold anything the latter needs.
#[component]
pub(crate) fn ConfirmModal(
    pending: Option<PendingAction>,
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
                    let count = pending.targets.len();
                    rsx! {
                        span { class: "eyebrow", "Durable bulk action" }
                        AlertDialogTitle { appearance: AlertDialogTitleAppearance::None,
                            "{action} {count} pull requests?"
                        }
                        AlertDialogDescription { appearance: AlertDialogDescriptionAppearance::None,
                            "The selected head SHAs are captured now. Moved or ineligible pull requests will be rejected, not silently retried against new code."
                        }
                        if action == BulkActionKind::Merge {
                            div { class: "notice", "Uses the globally configured merge method." }
                        } else {
                            div { class: "notice", "Rebase is requested by an idempotent @dependabot comment using the configured user token." }
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
