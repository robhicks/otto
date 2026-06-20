//! Pure view helpers — formatting and connection state. Browser-free, host-tested.

use otto_protocol::CapabilitiesManifest;
use otto_protocol::EventKind;

/// The single connection-state signal that drives the whole UI.
#[derive(Clone, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { session: String },
}

/// A single rendered row in the event log. `class` is a CSS class; `text` is the line.
#[derive(Clone, PartialEq)]
pub struct LogRow {
    pub class: &'static str,
    pub text: String,
}

fn row(class: &'static str, text: String) -> LogRow {
    LogRow { class, text }
}

/// The role of a line in a rendered diff.
#[derive(Clone, PartialEq, Debug)]
pub enum DiffKind {
    Context,
    Add,
    Del,
}

/// One line in a rendered diff.
#[derive(Clone, PartialEq, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Line diff of `old` → `new`: a common prefix and suffix render as `Context`; the divergent
/// middle renders as `Del` (old) then `Add` (new). `old == None` means a new file (all `Add`).
/// A minimal, dependency-free diff sufficient for the diff-first approval surface.
pub fn diff_lines(old: Option<&str>, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.map(|s| s.lines().collect()).unwrap_or_default();
    let new_lines: Vec<&str> = new.lines().collect();

    // Common prefix.
    let mut start = 0;
    while start < old_lines.len() && start < new_lines.len() && old_lines[start] == new_lines[start]
    {
        start += 1;
    }
    // Common suffix (not overlapping the prefix).
    let mut end_old = old_lines.len();
    let mut end_new = new_lines.len();
    while end_old > start && end_new > start && old_lines[end_old - 1] == new_lines[end_new - 1] {
        end_old -= 1;
        end_new -= 1;
    }

    let mut out = Vec::new();
    for line in &old_lines[..start] {
        out.push(DiffLine {
            kind: DiffKind::Context,
            text: (*line).to_string(),
        });
    }
    for line in &old_lines[start..end_old] {
        out.push(DiffLine {
            kind: DiffKind::Del,
            text: (*line).to_string(),
        });
    }
    for line in &new_lines[start..end_new] {
        out.push(DiffLine {
            kind: DiffKind::Add,
            text: (*line).to_string(),
        });
    }
    for line in &new_lines[end_new..] {
        out.push(DiffLine {
            kind: DiffKind::Context,
            text: (*line).to_string(),
        });
    }

    // `.lines()` discards a trailing newline (and `\r`), so a change confined to the final newline
    // would otherwise render as an all-context, visually-identical diff — and the user could
    // approve a file that differs from what's shown. If the rendered diff is pure context but the
    // raw contents differ, surface the change so it is never invisible.
    if let Some(old) = old {
        if old != new && out.iter().all(|l| l.kind == DiffKind::Context) {
            let (kind, text) = if new.ends_with('\n') {
                (DiffKind::Add, "(trailing newline added)")
            } else {
                (DiffKind::Del, "(trailing newline removed)")
            };
            out.push(DiffLine {
                kind,
                text: text.to_string(),
            });
        }
    }
    out
}

/// One capability segment in the status strip: a label, its current value, and whether it
/// represents a degraded/lost capability (rendered in the warning style).
#[derive(Clone, PartialEq, Debug)]
pub struct CapSegment {
    pub label: &'static str,
    pub value: String,
    pub degraded: bool,
}

/// Derive the engine/LLM/sandbox segments from the manifest. The two degradations the strip
/// exists to surface: a fully-offline (deterministic) LLM, and an absent sandbox (bash off).
pub fn capability_segments(m: &CapabilitiesManifest) -> Vec<CapSegment> {
    let engine = CapSegment {
        label: "engine",
        value: if m.engine_remote { "remote" } else { "local" }.to_string(),
        degraded: false,
    };
    let (llm_value, llm_degraded) = match (m.local_llm, m.remote_llm) {
        (true, true) => ("local+remote", false),
        (false, true) => ("remote", false),
        (true, false) => ("local", false),
        (false, false) => ("offline (deterministic)", true),
    };
    let llm = CapSegment {
        label: "LLM",
        value: llm_value.to_string(),
        degraded: llm_degraded,
    };
    let sandbox = CapSegment {
        label: "sandbox",
        value: if m.sandbox { "on" } else { "off" }.to_string(),
        degraded: !m.sandbox,
    };
    vec![engine, llm, sandbox]
}

/// True when the Promote button should be enabled: connected, the engine is local, and no turn
/// is running (promoting mid-turn would snapshot partial state, so it is disabled).
pub fn can_promote(
    conn: &ConnState,
    caps: &Option<CapabilitiesManifest>,
    turn_running: bool,
) -> bool {
    matches!(conn, ConnState::Connected { .. })
        && !turn_running
        && matches!(caps, Some(c) if !c.engine_remote)
}

/// True when the Demote button should be enabled: connected, the engine is remote, no turn running.
pub fn can_demote(
    conn: &ConnState,
    caps: &Option<CapabilitiesManifest>,
    turn_running: bool,
) -> bool {
    matches!(conn, ConnState::Connected { .. })
        && !turn_running
        && matches!(caps, Some(c) if c.engine_remote)
}

/// Approximate per-million-token display rates for the default remote model (claude-haiku-4-5).
/// These drive only the UI cost estimate; update freely — they are not load-bearing.
const COST_PER_MTOK_IN: f64 = 0.80;
const COST_PER_MTOK_OUT: f64 = 4.00;

/// Running token counts for the status strip.
pub fn format_meter(input_tokens: u64, output_tokens: u64) -> String {
    format!("↑{input_tokens} ↓{output_tokens} tok")
}

/// Approximate dollar cost, or `None` when no remote (billable) model is configured — in that
/// case the meter shows tokens only. A clearly-labeled estimate: it applies the remote rate to
/// all counted tokens.
pub fn cost_estimate(input_tokens: u64, output_tokens: u64, remote_llm: bool) -> Option<f64> {
    if !remote_llm {
        return None;
    }
    Some(
        input_tokens as f64 / 1_000_000.0 * COST_PER_MTOK_IN
            + output_tokens as f64 / 1_000_000.0 * COST_PER_MTOK_OUT,
    )
}

/// Human label for the status line.
pub fn status_label(c: &ConnState) -> &'static str {
    match c {
        ConnState::Disconnected => "disconnected",
        ConnState::Connecting => "connecting…",
        ConnState::Connected { .. } => "connected",
    }
}

/// Shorten a session id (uuid string) for the status line: first 4 chars + ellipsis.
pub fn short_session(id: &str) -> String {
    let head: String = id.chars().take(4).collect();
    if id.chars().count() > 4 {
        format!("{head}…")
    } else {
        head
    }
}

/// Format one engine event into a log row.
pub fn describe_event(kind: &EventKind) -> LogRow {
    match kind {
        EventKind::AgentStarted { role } => row("row-agent", format!("▸ {role:?} started")),
        EventKind::AgentFinished { role } => row("row-agent", format!("▸ {role:?} finished")),
        EventKind::FileEdit {
            path,
            bytes_written,
        } => row(
            "row-edit",
            format!("✎ FileEdit {} (+{} bytes)", path.display(), bytes_written),
        ),
        EventKind::VerifyResult { ok, detail } => row(
            "row-verify",
            format!(
                "{} Verify {}",
                if *ok { "✓" } else { "✗" },
                if detail.is_empty() {
                    "ok".to_string()
                } else {
                    detail.clone()
                },
            ),
        ),
        EventKind::Log { message } => row("row-log", format!("· {message}")),
        EventKind::TurnComplete { ok } => row(
            "row-turn",
            format!("● TurnComplete {}", if *ok { "ok" } else { "failed" }),
        ),
        EventKind::ApprovalRequest { path, .. } => row(
            "row-approval",
            format!("⏸ approval needed: {}", path.display()),
        ),
        EventKind::TokenCostMeter {
            input_tokens,
            output_tokens,
        } => row(
            "row-meter",
            format!("◷ tokens ↑{input_tokens} ↓{output_tokens}"),
        ),
    }
}

/// A server-sent `Error` frame as a row.
pub fn error_row(message: &str) -> LogRow {
    row("row-error", format!("error: {message}"))
}

/// A client-side problem (parse failure, refused connection) as a row.
pub fn client_error_row(message: &str) -> LogRow {
    row("row-error", format!("client: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_protocol::Role;
    use std::path::PathBuf;

    #[test]
    fn short_session_truncates_long_ids() {
        assert_eq!(short_session("3f9a1b2c-dead"), "3f9a…");
        assert_eq!(short_session("ab"), "ab");
    }

    #[test]
    fn status_labels() {
        assert_eq!(status_label(&ConnState::Disconnected), "disconnected");
        assert_eq!(
            status_label(&ConnState::Connected {
                session: "x".into()
            }),
            "connected"
        );
    }

    #[test]
    fn describe_file_edit() {
        let r = describe_event(&EventKind::FileEdit {
            path: PathBuf::from("src/main.rs"),
            bytes_written: 42,
        });
        assert_eq!(r.class, "row-edit");
        assert_eq!(r.text, "✎ FileEdit src/main.rs (+42 bytes)");
    }

    #[test]
    fn describe_turn_complete_and_verify() {
        assert_eq!(
            describe_event(&EventKind::TurnComplete { ok: true }).text,
            "● TurnComplete ok"
        );
        assert_eq!(
            describe_event(&EventKind::VerifyResult {
                ok: false,
                detail: "boom".into()
            })
            .text,
            "✗ Verify boom"
        );
    }

    #[test]
    fn describe_agent_uses_role_name() {
        let r = describe_event(&EventKind::AgentStarted {
            role: Role::Planner,
        });
        assert_eq!(r.text, "▸ Planner started");
    }

    fn manifest(
        engine_remote: bool,
        local_llm: bool,
        remote_llm: bool,
        sandbox: bool,
    ) -> CapabilitiesManifest {
        CapabilitiesManifest {
            engine_remote,
            local_llm,
            remote_llm,
            sandbox,
        }
    }

    #[test]
    fn offline_engine_marks_llm_segment_degraded() {
        let segs = capability_segments(&manifest(false, false, false, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "offline (deterministic)");
        assert!(llm.degraded);
        let engine = segs.iter().find(|s| s.label == "engine").unwrap();
        assert_eq!(engine.value, "local");
        assert!(!engine.degraded);
        let sandbox = segs.iter().find(|s| s.label == "sandbox").unwrap();
        assert!(!sandbox.degraded);
    }

    #[test]
    fn remote_llm_is_not_degraded() {
        let segs = capability_segments(&manifest(false, false, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn local_and_remote_llm_labels_both() {
        let segs = capability_segments(&manifest(false, true, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "local+remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn local_only_llm_labels_local() {
        let segs = capability_segments(&manifest(false, true, false, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "local");
        assert!(!llm.degraded);
    }

    #[test]
    fn sandbox_off_is_degraded() {
        let segs = capability_segments(&manifest(false, true, false, false));
        let sandbox = segs.iter().find(|s| s.label == "sandbox").unwrap();
        assert_eq!(sandbox.value, "off");
        assert!(sandbox.degraded);
    }

    #[test]
    fn engine_remote_labels_remote() {
        let segs = capability_segments(&manifest(true, true, false, true));
        let engine = segs.iter().find(|s| s.label == "engine").unwrap();
        assert_eq!(engine.value, "remote");
        assert!(!engine.degraded);
    }

    #[test]
    fn diff_new_file_is_all_adds() {
        let d = diff_lines(None, "a\nb\n");
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| l.kind == DiffKind::Add));
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn diff_identical_is_all_context() {
        let d = diff_lines(Some("a\nb\n"), "a\nb\n");
        assert!(d.iter().all(|l| l.kind == DiffKind::Context));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn diff_middle_change_keeps_context_head_and_tail() {
        let d = diff_lines(Some("a\nB\nc\n"), "a\nX\nc\n");
        // a = context, B = del, X = add, c = context
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].kind, DiffKind::Del);
        assert_eq!(d[1].text, "B");
        assert_eq!(d[2].kind, DiffKind::Add);
        assert_eq!(d[2].text, "X");
        assert_eq!(d[3].kind, DiffKind::Context);
        assert_eq!(d[3].text, "c");
    }

    #[test]
    fn diff_pure_append() {
        let d = diff_lines(Some("a\n"), "a\nb\n");
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[1].kind, DiffKind::Add);
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn diff_trailing_newline_removed_is_visible() {
        // Same line content, only the final newline differs — must not render as an empty diff.
        let d = diff_lines(Some("a\nb\n"), "a\nb");
        assert!(d.iter().any(|l| l.kind == DiffKind::Del));
    }

    #[test]
    fn diff_trailing_newline_added_is_visible() {
        let d = diff_lines(Some("a\nb"), "a\nb\n");
        assert!(d.iter().any(|l| l.kind == DiffKind::Add));
    }

    #[test]
    fn describe_approval_request_row() {
        let r = describe_event(&EventKind::ApprovalRequest {
            id: uuid::Uuid::from_u128(0),
            path: PathBuf::from("src/main.rs"),
            old: None,
            new: "x".into(),
        });
        assert_eq!(r.class, "row-approval");
        assert!(r.text.contains("src/main.rs"));
    }

    #[test]
    fn format_meter_shows_both_counts() {
        assert_eq!(format_meter(12, 34), "↑12 ↓34 tok");
    }

    #[test]
    fn cost_is_none_without_remote_model() {
        assert_eq!(cost_estimate(1_000, 1_000, false), None);
    }

    #[test]
    fn cost_uses_remote_rates() {
        let c = cost_estimate(1_000_000, 1_000_000, true).unwrap();
        assert!((c - (0.80 + 4.00)).abs() < 1e-9);
    }

    #[test]
    fn describe_token_cost_meter_row() {
        let r = describe_event(&EventKind::TokenCostMeter {
            input_tokens: 7,
            output_tokens: 9,
        });
        assert_eq!(r.class, "row-meter");
        assert!(r.text.contains("↑7"));
    }

    fn caps(engine_remote: bool) -> CapabilitiesManifest {
        CapabilitiesManifest {
            engine_remote,
            local_llm: true,
            remote_llm: false,
            sandbox: true,
        }
    }

    #[test]
    fn can_promote_only_when_connected_local_and_idle() {
        let connected = ConnState::Connected { session: "s".into() };
        assert!(can_promote(&connected, &Some(caps(false)), false));
        // not while a turn runs
        assert!(!can_promote(&connected, &Some(caps(false)), true));
        // not when already remote
        assert!(!can_promote(&connected, &Some(caps(true)), false));
        // not when disconnected / caps unknown
        assert!(!can_promote(&ConnState::Disconnected, &Some(caps(false)), false));
        assert!(!can_promote(&connected, &None, false));
    }

    #[test]
    fn can_demote_only_when_connected_remote_and_idle() {
        let connected = ConnState::Connected { session: "s".into() };
        assert!(can_demote(&connected, &Some(caps(true)), false));
        assert!(!can_demote(&connected, &Some(caps(true)), true));
        assert!(!can_demote(&connected, &Some(caps(false)), false));
        assert!(!can_demote(&ConnState::Disconnected, &Some(caps(true)), false));
        assert!(!can_demote(&connected, &None, false));
    }
}
