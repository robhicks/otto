//! Pure view helpers — formatting and connection state. Browser-free, host-tested.

use otto_protocol::CapabilitiesManifest;
use otto_protocol::EventKind;

use crate::i18n::{t, tf, Locale, Msg};

/// The single connection-state signal that drives the whole UI.
#[derive(Clone, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { session: String },
}

/// A client-side row's payload: authored copy (retranslates on a locale switch) or a passthrough
/// diagnostic (rendered verbatim in every locale — spec §2's boundary rule).
#[derive(Clone, PartialEq, Debug)]
pub enum ClientText {
    Authored(Msg),
    Passthrough(String),
}

/// A rendered row's content, kept STRUCTURED rather than pre-formatted.
///
/// This is the whole point of the shape: formatting at arrival time would freeze each row in the
/// locale that was active when it arrived, so a language switch would leave the event log — the
/// largest text surface in the UI — permanently mixed-language. Formatting happens in
/// `render_row`, at render time, so every row follows the active locale.
///
/// Fields carrying server-originated text (`detail`, `message`) and protocol identifiers (`role`)
/// are rendered verbatim; only the framing around them is localized.
#[derive(Clone, PartialEq, Debug)]
pub enum RowMsg {
    AgentStarted { role: String },
    AgentFinished { role: String },
    FileEdit { path: String, bytes: u64 },
    Verify { ok: bool, detail: String },
    Log { message: String },
    TurnComplete { ok: bool },
    ApprovalRequest { path: String },
    Meter { input: u64, output: u64 },
    ServerError { message: String },
    ClientError(ClientText),
}

/// A single rendered row in the event log. `class` is a CSS class; `msg` is formatted by
/// `render_row` against the active locale.
#[derive(Clone, PartialEq)]
pub struct LogRow {
    pub class: &'static str,
    pub msg: RowMsg,
}

fn row(class: &'static str, msg: RowMsg) -> LogRow {
    LogRow { class, msg }
}

/// Format a row for display in `locale`.
pub fn render_row(locale: Locale, msg: &RowMsg) -> String {
    match msg {
        RowMsg::AgentStarted { role } => tf(locale, Msg::RowAgentStarted, &[("role", role)]),
        RowMsg::AgentFinished { role } => tf(locale, Msg::RowAgentFinished, &[("role", role)]),
        RowMsg::FileEdit { path, bytes } => tf(
            locale,
            Msg::RowFileEdit,
            &[("path", path), ("bytes", &bytes.to_string())],
        ),
        RowMsg::Verify { ok, detail } => {
            // An empty detail has always rendered the authored word "ok" — including when `ok` is
            // false ("✗ Verify ok"). Preserved exactly; only the word itself is now localized.
            let detail = if detail.is_empty() {
                t(locale, Msg::VerifyOk)
            } else {
                detail.as_str()
            };
            tf(
                locale,
                Msg::RowVerify,
                &[("mark", if *ok { "✓" } else { "✗" }), ("detail", detail)],
            )
        }
        // Server-originated: the glyph is framing, the message passes through untranslated.
        RowMsg::Log { message } => format!("· {message}"),
        RowMsg::TurnComplete { ok } => t(
            locale,
            if *ok {
                Msg::RowTurnCompleteOk
            } else {
                Msg::RowTurnCompleteFailed
            },
        )
        .to_string(),
        RowMsg::ApprovalRequest { path } => tf(locale, Msg::RowApprovalNeeded, &[("path", path)]),
        RowMsg::Meter { input, output } => tf(
            locale,
            Msg::RowMeter,
            &[
                ("input", &input.to_string()),
                ("output", &output.to_string()),
            ],
        ),
        RowMsg::ServerError { message } => tf(locale, Msg::RowServerError, &[("message", message)]),
        RowMsg::ClientError(text) => {
            let message = match text {
                ClientText::Authored(m) => t(locale, *m).to_string(),
                ClientText::Passthrough(s) => s.clone(),
            };
            tf(locale, Msg::RowClientError, &[("message", &message)])
        }
    }
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
pub fn diff_lines(locale: Locale, old: Option<&str>, new: &str) -> Vec<DiffLine> {
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
                (DiffKind::Add, t(locale, Msg::DiffTrailingNewlineAdded))
            } else {
                (DiffKind::Del, t(locale, Msg::DiffTrailingNewlineRemoved))
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
    pub label: String,
    pub value: String,
    pub degraded: bool,
}

/// Derive the engine/LLM/sandbox segments from the manifest. The two degradations the strip
/// exists to surface: a fully-offline (deterministic) LLM, and an absent sandbox (bash off).
pub fn capability_segments(locale: Locale, m: &CapabilitiesManifest) -> Vec<CapSegment> {
    let engine = CapSegment {
        label: t(locale, Msg::CapEngine).to_string(),
        value: t(
            locale,
            if m.engine_remote {
                Msg::CapRemote
            } else {
                Msg::CapLocal
            },
        )
        .to_string(),
        degraded: false,
    };
    let (llm_value, llm_degraded) = match (m.local_llm, m.remote_llm) {
        (true, true) => (Msg::CapLocalRemote, false),
        (false, true) => (Msg::CapRemote, false),
        (true, false) => (Msg::CapLocal, false),
        (false, false) => (Msg::CapOffline, true),
    };
    let llm = CapSegment {
        label: t(locale, Msg::CapLlm).to_string(),
        value: t(locale, llm_value).to_string(),
        degraded: llm_degraded,
    };
    let sandbox = CapSegment {
        label: t(locale, Msg::CapSandbox).to_string(),
        value: t(locale, if m.sandbox { Msg::CapOn } else { Msg::CapOff }).to_string(),
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
pub fn format_meter(locale: Locale, input_tokens: u64, output_tokens: u64) -> String {
    tf(
        locale,
        Msg::Meter,
        &[
            ("input", &input_tokens.to_string()),
            ("output", &output_tokens.to_string()),
        ],
    )
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
pub fn status_label(locale: Locale, c: &ConnState) -> &'static str {
    t(
        locale,
        match c {
            ConnState::Disconnected => Msg::StatusDisconnected,
            ConnState::Connecting => Msg::StatusConnecting,
            ConnState::Connected { .. } => Msg::StatusConnected,
        },
    )
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

/// Classify one engine event into a structured log row. Formatting happens later, in `render_row`.
pub fn describe_event(kind: &EventKind) -> LogRow {
    match kind {
        EventKind::AgentStarted { role } => row(
            "row-agent",
            RowMsg::AgentStarted {
                role: format!("{role:?}"),
            },
        ),
        EventKind::AgentFinished { role } => row(
            "row-agent",
            RowMsg::AgentFinished {
                role: format!("{role:?}"),
            },
        ),
        EventKind::FileEdit {
            path,
            bytes_written,
        } => row(
            "row-edit",
            RowMsg::FileEdit {
                path: path.display().to_string(),
                bytes: *bytes_written,
            },
        ),
        EventKind::VerifyResult { ok, detail } => row(
            "row-verify",
            RowMsg::Verify {
                ok: *ok,
                detail: detail.clone(),
            },
        ),
        EventKind::Log { message } => row(
            "row-log",
            RowMsg::Log {
                message: message.clone(),
            },
        ),
        EventKind::TurnComplete { ok } => row("row-turn", RowMsg::TurnComplete { ok: *ok }),
        EventKind::ApprovalRequest { path, .. } => row(
            "row-approval",
            RowMsg::ApprovalRequest {
                path: path.display().to_string(),
            },
        ),
        EventKind::TokenCostMeter {
            input_tokens,
            output_tokens,
        } => row(
            "row-meter",
            RowMsg::Meter {
                input: *input_tokens,
                output: *output_tokens,
            },
        ),
    }
}

/// A server-sent `Error` frame as a row. The message is engine-originated and passes through.
pub fn error_row(message: &str) -> LogRow {
    row(
        "row-error",
        RowMsg::ServerError {
            message: message.to_string(),
        },
    )
}

/// A client-side problem as a row — authored copy or a passthrough diagnostic (spec §2).
pub fn client_error_row(text: ClientText) -> LogRow {
    row("row-error", RowMsg::ClientError(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use otto_protocol::Role;
    use std::path::PathBuf;

    #[test]
    fn short_session_truncates_long_ids() {
        assert_eq!(short_session("3f9a1b2c-dead"), "3f9a…");
        assert_eq!(short_session("ab"), "ab");
    }

    #[test]
    fn status_labels() {
        assert_eq!(
            status_label(Locale::En, &ConnState::Disconnected),
            "disconnected"
        );
        assert_eq!(
            status_label(
                Locale::En,
                &ConnState::Connected {
                    session: "x".into()
                }
            ),
            "connected"
        );
    }

    #[test]
    fn status_labels_localize() {
        assert_eq!(
            status_label(Locale::De, &ConnState::Disconnected),
            "getrennt"
        );
    }

    #[test]
    fn describe_file_edit() {
        let r = describe_event(&EventKind::FileEdit {
            path: PathBuf::from("src/main.rs"),
            bytes_written: 42,
        });
        assert_eq!(r.class, "row-edit");
        assert_eq!(
            render_row(Locale::En, &r.msg),
            "✎ FileEdit src/main.rs (+42 bytes)"
        );
    }

    #[test]
    fn describe_turn_complete_and_verify() {
        assert_eq!(
            render_row(
                Locale::En,
                &describe_event(&EventKind::TurnComplete { ok: true }).msg
            ),
            "● TurnComplete ok"
        );
        assert_eq!(
            render_row(
                Locale::En,
                &describe_event(&EventKind::VerifyResult {
                    ok: false,
                    detail: "boom".into()
                })
                .msg
            ),
            "✗ Verify boom"
        );
    }

    #[test]
    fn verify_with_no_detail_renders_the_authored_ok_word() {
        // Preserves today's behavior exactly, including the `!ok && detail.is_empty()` case, which
        // has always rendered "✗ Verify ok".
        assert_eq!(
            render_row(
                Locale::En,
                &describe_event(&EventKind::VerifyResult {
                    ok: true,
                    detail: String::new()
                })
                .msg
            ),
            "✓ Verify ok"
        );
        assert_eq!(
            render_row(
                Locale::En,
                &describe_event(&EventKind::VerifyResult {
                    ok: false,
                    detail: String::new()
                })
                .msg
            ),
            "✗ Verify ok"
        );
    }

    #[test]
    fn describe_agent_uses_role_name() {
        let r = describe_event(&EventKind::AgentStarted {
            role: Role::Planner,
        });
        assert_eq!(render_row(Locale::En, &r.msg), "▸ Planner started");
    }

    #[test]
    fn rows_retranslate_when_the_locale_changes() {
        // The property the whole deferred-formatting refactor exists for: a row built once renders
        // differently per locale, so a picker switch retranslates already-received rows.
        let plain = describe_event(&EventKind::TurnComplete { ok: false });
        assert_ne!(
            render_row(Locale::En, &plain.msg),
            render_row(Locale::Es, &plain.msg)
        );
        let parameterized = describe_event(&EventKind::AgentStarted { role: Role::Coder });
        assert_ne!(
            render_row(Locale::En, &parameterized.msg),
            render_row(Locale::De, &parameterized.msg)
        );
        let boolean = describe_event(&EventKind::FileEdit {
            path: PathBuf::from("a.rs"),
            bytes_written: 1,
        });
        assert_ne!(
            render_row(Locale::En, &boolean.msg),
            render_row(Locale::ZhHans, &boolean.msg)
        );
    }

    #[test]
    fn server_originated_payloads_are_never_translated() {
        // Spec §2: the engine's own text passes through byte-identically in every locale; only the
        // framing around it is localized.
        let log = describe_event(&EventKind::Log {
            message: "engine says hi".into(),
        });
        assert!(render_row(Locale::En, &log.msg).contains("engine says hi"));
        assert!(render_row(Locale::ZhHans, &log.msg).contains("engine says hi"));

        let verify = describe_event(&EventKind::VerifyResult {
            ok: false,
            detail: "cargo test failed".into(),
        });
        assert!(render_row(Locale::ZhHans, &verify.msg).contains("cargo test failed"));

        // A protocol identifier survives too.
        let agent = describe_event(&EventKind::AgentStarted {
            role: Role::Verifier,
        });
        assert!(render_row(Locale::ZhHans, &agent.msg).contains("Verifier"));
    }

    #[test]
    fn client_error_rows_carry_authored_or_passthrough_text() {
        // Authored copy retranslates…
        let authored = client_error_row(ClientText::Authored(Msg::UrlAndTokenRequired));
        assert_ne!(
            render_row(Locale::En, &authored.msg),
            render_row(Locale::De, &authored.msg)
        );
        // …a transport diagnostic does not (spec §2's boundary rule).
        let passthrough = client_error_row(ClientText::Passthrough("socket closed".into()));
        assert!(render_row(Locale::De, &passthrough.msg).contains("socket closed"));
    }

    #[test]
    fn capability_segments_localize_labels_and_values() {
        let en = capability_segments(Locale::En, &manifest(false, true, false, false));
        let de = capability_segments(Locale::De, &manifest(false, true, false, false));
        // Shape is locale-invariant…
        assert_eq!(en.len(), de.len());
        for (a, b) in en.iter().zip(de.iter()) {
            assert_eq!(a.degraded, b.degraded);
        }
        // …but the copy is not.
        assert_eq!(en[2].value, "off");
        assert_eq!(de[2].value, "aus");
        assert_eq!(de[0].label, "Engine");
    }

    #[test]
    fn diff_trailing_newline_markers_localize() {
        let en = diff_lines(Locale::En, Some("a\nb\n"), "a\nb");
        let de = diff_lines(Locale::De, Some("a\nb\n"), "a\nb");
        let en_marker = en.iter().find(|l| l.kind == DiffKind::Del).unwrap();
        let de_marker = de.iter().find(|l| l.kind == DiffKind::Del).unwrap();
        assert_ne!(en_marker.text, de_marker.text);
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
        let segs = capability_segments(Locale::En, &manifest(false, false, false, true));
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
        let segs = capability_segments(Locale::En, &manifest(false, false, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn local_and_remote_llm_labels_both() {
        let segs = capability_segments(Locale::En, &manifest(false, true, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "local+remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn local_only_llm_labels_local() {
        let segs = capability_segments(Locale::En, &manifest(false, true, false, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "local");
        assert!(!llm.degraded);
    }

    #[test]
    fn sandbox_off_is_degraded() {
        let segs = capability_segments(Locale::En, &manifest(false, true, false, false));
        let sandbox = segs.iter().find(|s| s.label == "sandbox").unwrap();
        assert_eq!(sandbox.value, "off");
        assert!(sandbox.degraded);
    }

    #[test]
    fn engine_remote_labels_remote() {
        let segs = capability_segments(Locale::En, &manifest(true, true, false, true));
        let engine = segs.iter().find(|s| s.label == "engine").unwrap();
        assert_eq!(engine.value, "remote");
        assert!(!engine.degraded);
    }

    #[test]
    fn diff_new_file_is_all_adds() {
        let d = diff_lines(Locale::En, None, "a\nb\n");
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| l.kind == DiffKind::Add));
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn diff_identical_is_all_context() {
        let d = diff_lines(Locale::En, Some("a\nb\n"), "a\nb\n");
        assert!(d.iter().all(|l| l.kind == DiffKind::Context));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn diff_middle_change_keeps_context_head_and_tail() {
        let d = diff_lines(Locale::En, Some("a\nB\nc\n"), "a\nX\nc\n");
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
        let d = diff_lines(Locale::En, Some("a\n"), "a\nb\n");
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[1].kind, DiffKind::Add);
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn diff_trailing_newline_removed_is_visible() {
        // Same line content, only the final newline differs — must not render as an empty diff.
        let d = diff_lines(Locale::En, Some("a\nb\n"), "a\nb");
        assert!(d.iter().any(|l| l.kind == DiffKind::Del));
    }

    #[test]
    fn diff_trailing_newline_added_is_visible() {
        let d = diff_lines(Locale::En, Some("a\nb"), "a\nb\n");
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
        assert!(render_row(Locale::En, &r.msg).contains("src/main.rs"));
    }

    #[test]
    fn format_meter_shows_both_counts() {
        assert_eq!(format_meter(Locale::En, 12, 34), "↑12 ↓34 tok");
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
        assert!(render_row(Locale::En, &r.msg).contains("↑7"));
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
        let connected = ConnState::Connected {
            session: "s".into(),
        };
        assert!(can_promote(&connected, &Some(caps(false)), false));
        // not while a turn runs
        assert!(!can_promote(&connected, &Some(caps(false)), true));
        // not when already remote
        assert!(!can_promote(&connected, &Some(caps(true)), false));
        // not when disconnected / caps unknown
        assert!(!can_promote(
            &ConnState::Disconnected,
            &Some(caps(false)),
            false
        ));
        assert!(!can_promote(&connected, &None, false));
    }

    #[test]
    fn can_demote_only_when_connected_remote_and_idle() {
        let connected = ConnState::Connected {
            session: "s".into(),
        };
        assert!(can_demote(&connected, &Some(caps(true)), false));
        assert!(!can_demote(&connected, &Some(caps(true)), true));
        assert!(!can_demote(&connected, &Some(caps(false)), false));
        assert!(!can_demote(
            &ConnState::Disconnected,
            &Some(caps(true)),
            false
        ));
        assert!(!can_demote(&connected, &None, false));
    }
}
