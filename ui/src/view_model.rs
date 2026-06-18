//! Pure view helpers — formatting and connection state. Browser-free, host-tested.

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
        EventKind::FileEdit { path, bytes_written } => row(
            "row-edit",
            format!("✎ FileEdit {} (+{} bytes)", path.display(), bytes_written),
        ),
        EventKind::VerifyResult { ok, detail } => row(
            "row-verify",
            format!(
                "{} Verify {}",
                if *ok { "✓" } else { "✗" },
                if detail.is_empty() { "ok".to_string() } else { detail.clone() },
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
            status_label(&ConnState::Connected { session: "x".into() }),
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
            describe_event(&EventKind::VerifyResult { ok: false, detail: "boom".into() }).text,
            "✗ Verify boom"
        );
    }

    #[test]
    fn describe_agent_uses_role_name() {
        let r = describe_event(&EventKind::AgentStarted { role: Role::Planner });
        assert_eq!(r.text, "▸ Planner started");
    }
}
