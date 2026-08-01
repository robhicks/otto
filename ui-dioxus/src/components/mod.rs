mod approval_panel;
mod connection_form;
mod event_log;
mod file_tree;
mod language_picker;
mod prompt_bar;
mod status_line;

pub use approval_panel::{ApprovalPanel, PendingApproval};
pub use connection_form::ConnectionForm;
pub use event_log::EventLog;
pub use file_tree::FileTree;
// No longer web-gated: `set_document_lang` is implemented on both real targets (see its doc — the
// desktop webview has a real `documentElement` too), with a no-op only in the seam-check build.
pub use language_picker::{set_document_lang, LanguagePicker};
pub use prompt_bar::PromptBar;
pub use status_line::StatusLine;
