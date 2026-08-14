//! Pure rendering of `EventKind` into terminal lines. No I/O, no global state, no locale.

use otto_protocol::{EventKind, Role};

const DIM: &str = "\u{1b}[2m";
const GREEN: &str = "\u{1b}[32m";
const RED: &str = "\u{1b}[31m";
const RESET: &str = "\u{1b}[0m";

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn role_name(role: &Role) -> String {
    match role {
        Role::Planner => "planner".to_string(),
        Role::ContextFinder => "context".to_string(),
        Role::Coder => "coder".to_string(),
        Role::Verifier => "verifier".to_string(),
        Role::Custom(n) => n.clone(),
    }
}

/// Render one event as terminal lines. Pure: no I/O, no global state, no locale — which is what
/// makes every branch unit-testable with no TTY.
///
/// `Log` and `VerifyResult.detail` are server-originated diagnostics and are reproduced
/// verbatim; the CLI does not reword or interpret them.
///
/// The match is exhaustive with no `_` arm: a future `EventKind` variant is a compile error
/// here, not a silently unrendered event.
pub fn render(kind: &EventKind, color: bool) -> Vec<String> {
    match kind {
        EventKind::AgentStarted { role } => {
            vec![paint(&format!("• {}", role_name(role)), DIM, color)]
        }
        EventKind::AgentFinished { role } => {
            vec![paint(
                &format!("  {} finished", role_name(role)),
                DIM,
                color,
            )]
        }
        EventKind::FileEdit {
            path,
            bytes_written,
        } => vec![format!(
            "  edited {} ({bytes_written} bytes)",
            path.display()
        )],
        EventKind::ApprovalRequest { path, .. } => vec![paint(
            &format!("  edit to {} needs approval — skipped", path.display()),
            DIM,
            color,
        )],
        EventKind::VerifyResult { ok, detail } => {
            let head = if *ok {
                paint("  verify passed", GREEN, color)
            } else {
                paint("  verify failed", RED, color)
            };
            let mut out = vec![head];
            if !detail.is_empty() {
                out.push(format!("  {detail}"));
            }
            out
        }
        EventKind::Log { message } => vec![paint(&format!("  {message}"), DIM, color)],
        EventKind::TokenCostMeter {
            input_tokens,
            output_tokens,
        } => vec![paint(
            &format!("  {input_tokens} in / {output_tokens} out"),
            DIM,
            color,
        )],
        EventKind::TurnComplete { ok } => vec![if *ok {
            paint("done", GREEN, color)
        } else {
            paint("turn failed", RED, color)
        }],
    }
}

/// Whether to emit ANSI, honoring `NO_COLOR` and non-TTY stdout.
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use otto_protocol::{EventKind, Role};
    use uuid::Uuid;

    use super::render;

    #[test]
    fn renders_every_event_kind_without_debug_formatting() {
        // Every `EventKind` variant, so a future addition to the enum (caught by the
        // exhaustive match in `render`) also gets covered here.
        let cases = vec![
            EventKind::AgentStarted {
                role: Role::Planner,
            },
            EventKind::AgentFinished {
                role: Role::Planner,
            },
            EventKind::FileEdit {
                path: PathBuf::from("src/a.rs"),
                bytes_written: 42,
            },
            EventKind::ApprovalRequest {
                id: Uuid::from_u128(1),
                path: PathBuf::from("src/b.rs"),
                old: Some("old\n".to_string()),
                new: "new\n".to_string(),
            },
            EventKind::VerifyResult {
                ok: true,
                detail: "3 passed".to_string(),
            },
            EventKind::Log {
                message: "planned 2 milestone(s)".to_string(),
            },
            EventKind::TokenCostMeter {
                input_tokens: 100,
                output_tokens: 50,
            },
            EventKind::TurnComplete { ok: true },
        ];
        for kind in cases {
            let lines = render(&kind, false);
            assert!(
                !lines.is_empty(),
                "every EventKind must render something: {kind:?}"
            );
            for l in &lines {
                assert!(!l.contains("EventKind"), "must not fall back to Debug: {l}");
                assert!(
                    !l.contains('\u{1b}'),
                    "color=false must emit no ANSI escapes"
                );
            }
        }
    }

    #[test]
    fn color_true_emits_ansi_and_color_false_does_not() {
        let kind = EventKind::VerifyResult {
            ok: false,
            detail: "1 failed".to_string(),
        };
        assert!(render(&kind, true).iter().any(|l| l.contains('\u{1b}')));
        assert!(render(&kind, false).iter().all(|l| !l.contains('\u{1b}')));
    }

    #[test]
    fn server_diagnostics_render_verbatim() {
        // Log and VerifyResult.detail are server-originated; the CLI must not reword them.
        let lines = render(
            &EventKind::Log {
                message: "exact server text".to_string(),
            },
            false,
        );
        assert!(lines.iter().any(|l| l.contains("exact server text")));
    }
}
