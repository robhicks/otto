//! Localization for the Dioxus UI: a dependency-free, compile-time-verified message catalog
//! plus locale resolution and persistence.
//!
//! The boundary this module enforces (design spec §2): **actionable copy addressed to the user is
//! translated; technical failure detail is not.** Server-originated payloads (`EventKind::Log`,
//! `VerifyResult.detail`, `ServerMessage::Error.message`), protocol identifiers (`Role` names,
//! `FileEdit`/`Verify`/`TurnComplete`), and this crate's transport/boot diagnostics render verbatim
//! in every locale — they share a surface with permanently-English engine output, and their
//! audience is a bug report.

use dioxus::prelude::*;

mod catalog;

pub use catalog::{t, tf, Msg};

/// A shipped UI language. Adding a variant is a compile error until every catalog entry supplies
/// it — see the `messages!` macro in `catalog.rs`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Locale {
    En,
    De,
    Es,
    Hi,
    ZhHans,
}

impl Locale {
    pub const ALL: [Locale; 5] = [
        Locale::En,
        Locale::De,
        Locale::Es,
        Locale::Hi,
        Locale::ZhHans,
    ];

    /// The BCP-47 tag persisted to storage and written to `document.documentElement.lang`.
    pub fn tag(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::De => "de",
            Locale::Es => "es",
            Locale::Hi => "hi",
            Locale::ZhHans => "zh-Hans",
        }
    }

    /// The language's own name for itself. Deliberately NOT a catalog entry: the picker must be
    /// usable by a reader currently stuck in a language they cannot read.
    pub fn endonym(self) -> &'static str {
        match self {
            Locale::En => "English",
            Locale::De => "Deutsch",
            Locale::Es => "Español",
            Locale::Hi => "हिन्दी",
            Locale::ZhHans => "中文（简体）",
        }
    }

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
