//! Localization for the Dioxus UI: a dependency-free, compile-time-verified message catalog
//! plus locale resolution and persistence.
//!
//! The boundary this module enforces (design spec §2): **interface copy — the words that describe
//! the app's own state, its controls, and what the user should do — is translated. Failure
//! diagnostics carried on the transport's `Result<_, SeamError>` seam are not.** The line is
//! *where the text comes from*, not whether it is actionable: `No file open`, `binary file — not
//! editable`, `disconnected`, `off`, and `seq {n}` are purely informational yet all correctly
//! translated, because each describes the app's own state.
//! Server-originated payloads (`EventKind::Log`,
//! `VerifyResult.detail`, `ServerMessage::Error.message`), protocol identifiers (`Role` names,
//! `FileEdit`/`Verify`/`TurnComplete`), and this crate's transport diagnostics render verbatim
//! in every locale — they share a surface with permanently-English engine output, and their
//! audience is a bug report.
//!
//! That last carve-out is enforced by type rather than by convention: `transport::SeamError`'s
//! constructor is `pub(in crate::transport)`, so `ClientText::Passthrough` cannot be handed prose
//! authored anywhere else. What the type does NOT decide is whether text written *inside*
//! `transport/` is a diagnostic or interface copy — that stays a review question, so do not put
//! user-facing instructions there.
//!
//! **The desktop boot diagnostic is the one deliberate exception.** The sidecar-spawn failure is
//! not on the transport seam — it is produced before any socket exists, and it tells the user
//! auto-connect did not happen and to use the manual form — so it is interface copy: the sentence
//! is localized (`Msg::SidecarSpawnFailed`) while its `{bin}` and `{detail}` payloads pass through
//! byte-identically. Amended 2026-08-01; full reasoning in
//! `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` §3.

use dioxus::prelude::*;

mod catalog;
mod resolve;
mod store;

pub use catalog::{t, tf, Msg};
pub use resolve::{env_locale_tags, resolve_locale};
pub use store::{load_persisted_locale, store_persisted_locale};

/// The locale to start in: a persisted choice if there is one, else the environment's preference,
/// else English. The single call the app makes at startup.
pub fn initial_locale() -> Locale {
    resolve_locale(load_persisted_locale().as_deref(), &env_locale_tags())
}

/// Emit `Locale`, its `ALL` roster, `tag()`, and `endonym()` from ONE variant list.
///
/// Hand-maintaining these four in parallel does not work, and the failure is silent in the worst
/// direction. `ALL` is a value, not a match, so the compiler has nothing to check it against:
/// adding a variant to the enum produced three errors (`t`, `tag`, `endonym`) and **none** from
/// `ALL`. A locale forgotten there compiles clean, disappears from the picker, and — because every
/// catalog-integrity test iterates `Locale::ALL` — is silently skipped by the very tests that
/// exist to catch a missing translation, `lists_every_language_endonymically` included. Same
/// technique `messages!` in `catalog.rs` already establishes for the same reason.
macro_rules! locales {
    ($($variant:ident { tag: $tag:expr, endonym: $endonym:expr $(,)? })+) => {
        /// A shipped UI language. Adding a variant is a compile error until every catalog entry
        /// supplies it — see the `messages!` macro in `catalog.rs`.
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        pub enum Locale { $($variant),+ }

        impl Locale {
            /// Every shipped language, in picker order. Derived from the same list as the enum, so
            /// it cannot fall out of step with it.
            pub const ALL: [Locale; [$(Locale::$variant),+].len()] = [$(Locale::$variant),+];

            /// The BCP-47 tag persisted to storage and written to `document.documentElement.lang`.
            pub fn tag(self) -> &'static str {
                match self { $(Locale::$variant => $tag),+ }
            }

            /// The language's own name for itself. Deliberately NOT a catalog entry: the picker
            /// must be usable by a reader currently stuck in a language they cannot read.
            pub fn endonym(self) -> &'static str {
                match self { $(Locale::$variant => $endonym),+ }
            }
        }
    };
}

locales! {
    En { tag: "en", endonym: "English" }
    De { tag: "de", endonym: "Deutsch" }
    Es { tag: "es", endonym: "Español" }
    Hi { tag: "hi", endonym: "हिन्दी" }
    ZhHans { tag: "zh-Hans", endonym: "中文（简体）" }
}

impl Locale {
    /// Parse a BCP-47-ish tag. `None` for anything with no shipped catalog — the caller falls back
    /// to `En` rather than guessing a near-match.
    pub fn from_tag(tag: &str) -> Option<Locale> {
        let t = tag.trim().replace('_', "-").to_ascii_lowercase();
        // Chinese is script-sensitive and must NOT be matched on the primary subtag alone: we ship
        // Simplified only, and serving it to a Traditional reader is worse than falling back to en.
        if t == "zh" || t.starts_with("zh-hans") {
            return Some(Locale::ZhHans);
        }
        if let Some(region) = t.strip_prefix("zh-") {
            return match region {
                "cn" | "sg" | "my" => Some(Locale::ZhHans),
                _ => None, // zh-hant, zh-tw, zh-hk, zh-mo, and anything unrecognized
            };
        }
        let primary = t.split('-').next().unwrap_or("");
        match primary {
            "en" => Some(Locale::En),
            "de" => Some(Locale::De),
            "es" => Some(Locale::Es),
            "hi" => Some(Locale::Hi),
            _ => None,
        }
    }
}

/// The active locale for the calling component.
///
/// **This is the only supported way for a component to read the locale.** It is a hook: call it
/// unconditionally at the top of the component body, above any early return or match arm.
///
/// `try_use_context` (not `use_context`) is load-bearing: plain `use_context` **panics** when no
/// provider is in scope, and `editor/dirty.rs`'s render tests mount `Editor` in a bare `VirtualDom`
/// with no provider. Falling back to `En` keeps those tests — and every future provider-less
/// component test — working, instead of making each component re-decide.
///
/// Reading the signal (`sig()`) is a TRACKED read, which is what subscribes the component so a
/// picker write actually re-renders it. A `.peek()`/write-guard access would not subscribe.
pub fn use_locale() -> Locale {
    match try_use_context::<Signal<Locale>>() {
        Some(sig) => sig(),
        None => Locale::En,
    }
}
