use std::path::PathBuf;

use dioxus::prelude::*;
use uuid::Uuid;

use crate::i18n::{t, tf, use_locale, Msg};
use crate::net::view_model::{diff_lines, DiffKind};

/// A pending edit approval surfaced to the user: the correlation id, the path, and the diff
/// inputs (old contents — `None` for a new file — and the proposed contents).
pub type PendingApproval = (Uuid, PathBuf, Option<String>, String);

/// Renders the pending diff (if any) with Approve / Reject buttons. `on_decide` is called with
/// the approval id and the verdict.
#[component]
pub fn ApprovalPanel(
    pending: Signal<Option<PendingApproval>>,
    on_decide: EventHandler<(Uuid, bool)>,
) -> Element {
    // Hooks are positional, so this must run before the early return below — a component that
    // sometimes skips a hook call desynchronizes every hook after it.
    let locale = use_locale();
    let Some((id, path, old, new)) = pending.read().clone() else {
        return rsx! {};
    };
    let lines = diff_lines(locale, old.as_deref(), &new);
    rsx! {
        div { class: "approval-panel",
            div { class: "approval-head",
                {tf(locale, Msg::ApprovalNeeded, &[("path", &path.display().to_string())])}
            }
            pre { class: "approval-diff",
                for l in lines {
                    div {
                        class: match l.kind {
                            DiffKind::Add => "diff-add",
                            DiffKind::Del => "diff-del",
                            DiffKind::Context => "diff-context",
                        },
                        "{l.text}"
                    }
                }
            }
            div { class: "approval-actions",
                button { onclick: move |_| on_decide.call((id, true)), {t(locale, Msg::Approve)} }
                button { onclick: move |_| on_decide.call((id, false)), {t(locale, Msg::Reject)} }
            }
        }
    }
}
