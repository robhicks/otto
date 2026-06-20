use std::path::PathBuf;

use leptos::prelude::*;
use uuid::Uuid;

use crate::view_model::{diff_lines, DiffKind};

/// A pending edit approval surfaced to the user: the correlation id, the path, and the diff
/// inputs (old contents — `None` for a new file — and the proposed contents).
pub type PendingApproval = (Uuid, PathBuf, Option<String>, String);

/// Renders the pending diff (if any) with Approve / Reject buttons. `on_decide` is called with
/// the approval id and the verdict.
#[component]
pub fn ApprovalPanel(
    pending: Signal<Option<PendingApproval>>,
    on_decide: Callback<(Uuid, bool)>,
) -> impl IntoView {
    move || {
        pending.get().map(|(id, path, old, new)| {
            let lines = diff_lines(old.as_deref(), &new);
            let rows = lines
                .into_iter()
                .map(|l| {
                    let cls = match l.kind {
                        DiffKind::Add => "diff-add",
                        DiffKind::Del => "diff-del",
                        DiffKind::Context => "diff-ctx",
                    };
                    view! { <div class=cls>{l.text}</div> }
                })
                .collect_view();
            // Both closures capture `id` (Copy) and `on_decide` (Copy in Leptos 0.8).
            let id_a = id;
            let id_b = id;
            view! {
                <div class="approval">
                    <div class="approval-head">
                        {format!("Approve edit to {}?", path.display())}
                    </div>
                    <div class="approval-diff">{rows}</div>
                    <div class="approval-actions">
                        <button
                            class="approve-btn"
                            on:click=move |_| on_decide.run((id_a, true))
                        >
                            "Approve"
                        </button>
                        <button
                            class="reject-btn"
                            on:click=move |_| on_decide.run((id_b, false))
                        >
                            "Reject"
                        </button>
                    </div>
                </div>
            }
        })
    }
}
