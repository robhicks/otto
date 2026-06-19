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
}
