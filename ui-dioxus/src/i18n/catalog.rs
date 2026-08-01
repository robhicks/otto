//! The message catalog: one entry per key, all five locales side by side.
//!
//! Adding a key requires all five translations at the call site or the macro does not parse;
//! adding a `Locale` variant without extending the macro fails `t`'s exhaustive match. An ABSENT
//! translation — a missing key or a missing locale — is therefore a COMPILE error, not a runtime
//! gap. A *stub* is not: `de: ""` compiles fine, so it is caught by test instead
//! (`no_message_is_empty_in_any_locale`).
//!
//! Keys whose template embeds a protocol identifier (`FileEdit`, `Verify`, `TurnComplete`) keep
//! that identifier byte-identical in every locale — spec §2, enforced by
//! `protocol_identifiers_survive_translation`.

use super::Locale;

macro_rules! messages {
    ($($key:ident { en: $en:expr, de: $de:expr, es: $es:expr, hi: $hi:expr, zh: $zh:expr $(,)? })+) => {
        /// A catalog key. One variant per message; see this module's `messages!` block.
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        pub enum Msg { $($key),+ }

        impl Msg {
            /// Every key, for the catalog-integrity tests.
            ///
            /// `cfg(test)` because it exists to be asserted against, not consumed: production code
            /// names the variants it needs. Without the gate it is dead code in every real build.
            #[cfg(test)]
            pub const ALL: &'static [Msg] = &[$(Msg::$key),+];
        }

        /// Look up a message. Exhaustive over `(Locale, Msg)` with NO wildcard arm — that is what
        /// makes an unhandled locale a compile error. **Never add one**; see the warning above the
        /// `messages!` invocation below, which is where rustc's E0004 diagnostic points.
        pub fn t(locale: Locale, m: Msg) -> &'static str {
            match (locale, m) {
                $(
                    (Locale::En, Msg::$key) => $en,
                    (Locale::De, Msg::$key) => $de,
                    (Locale::Es, Msg::$key) => $es,
                    (Locale::Hi, Msg::$key) => $hi,
                    (Locale::ZhHans, Msg::$key) => $zh,
                )+
            }
        }
    };
}

/// Substitute `{name}` placeholders from `args`.
///
/// Single-pass by construction: the output is built by walking the TEMPLATE and pushing either
/// literal text or a substituted value, so a value that itself contains `{...}` is never rescanned.
/// This matters — `{path}` and `{message}` carry attacker-influenceable content.
///
/// A placeholder with no matching arg is emitted verbatim (`{name}`): visibly wrong and therefore
/// reportable, rather than silently blanking user-facing text.
pub fn tf(locale: Locale, m: Msg, args: &[(&str, &str)]) -> String {
    let template = t(locale, m);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                // Unbalanced brace: emit the remainder verbatim rather than dropping it.
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

// ============================================================================================
// IF YOU LANDED HERE FROM `error[E0004]: non-exhaustive patterns` — READ THIS FIRST.
//
// You added a `Locale` variant (say `Fr`) in `mod.rs`, and rustc is pointing at this invocation
// and proposing:
//
//     (Locale::Fr, _) => todo!()
//
// **Do not take that suggestion.** The exhaustive `(Locale, Msg)` match with no wildcard arm is
// the entire mechanism that makes a missing translation a compile error rather than a runtime
// gap. A single wildcard arm — anywhere, `todo!()` or not — silently disarms it for EVERY key at
// once, and the next contributor adds a message with four translations and ships it.
//
// The error is not a defect to be patched; it is the design working. It is telling you the
// catalog is incomplete. The fix is to add `fr: "…"` to every entry in the block below (and an
// `fr` arm to the `messages!` pattern + `t`'s match at the top of this file), which is exactly
// as much work as the language actually costs.
// ============================================================================================
messages! {
    // ---- Buttons and actions -------------------------------------------------------------
    Connect { en: "Connect", de: "Verbinden", es: "Conectar", hi: "कनेक्ट करें", zh: "连接" }
    Connecting { en: "Connecting…", de: "Verbinde…", es: "Conectando…", hi: "कनेक्ट हो रहा है…", zh: "连接中…" }
    Disconnect { en: "Disconnect", de: "Trennen", es: "Desconectar", hi: "डिस्कनेक्ट करें", zh: "断开连接" }
    Send { en: "Send", de: "Senden", es: "Enviar", hi: "भेजें", zh: "发送" }
    Pause { en: "Pause", de: "Pausieren", es: "Pausar", hi: "रोकें", zh: "暂停" }
    Resume { en: "Resume", de: "Fortsetzen", es: "Reanudar", hi: "जारी रखें", zh: "继续" }
    Abort { en: "Abort", de: "Abbrechen", es: "Cancelar", hi: "निरस्त करें", zh: "中止" }
    Approve { en: "Approve", de: "Genehmigen", es: "Aprobar", hi: "स्वीकृत करें", zh: "批准" }
    Reject { en: "Reject", de: "Ablehnen", es: "Rechazar", hi: "अस्वीकार करें", zh: "拒绝" }
    RefreshFiles { en: "Refresh files", de: "Dateien aktualisieren", es: "Actualizar archivos", hi: "फ़ाइलें रीफ़्रेश करें", zh: "刷新文件" }
    PromoteToRemote { en: "Promote to remote", de: "Auf Remote hochstufen", es: "Promover a remoto", hi: "रिमोट पर प्रमोट करें", zh: "提升到远程" }
    DemoteToLocal { en: "Demote to local", de: "Auf Lokal herabstufen", es: "Degradar a local", hi: "लोकल पर डिमोट करें", zh: "降级到本地" }

    // ---- Form fields ---------------------------------------------------------------------
    // The URL field's placeholder (`ws://127.0.0.1:8787`) is an example VALUE, not copy — it is
    // deliberately not a catalog key (spec §2).
    TokenPlaceholder { en: "token", de: "Token", es: "token", hi: "टोकन", zh: "令牌" }

    // ---- Status strip --------------------------------------------------------------------
    StatusDisconnected { en: "disconnected", de: "getrennt", es: "desconectado", hi: "डिस्कनेक्ट किया गया", zh: "已断开" }
    StatusConnecting { en: "connecting…", de: "verbinde…", es: "conectando…", hi: "कनेक्ट हो रहा है…", zh: "连接中…" }
    StatusConnected { en: "connected", de: "verbunden", es: "conectado", hi: "कनेक्ट किया गया", zh: "已连接" }
    SeqLabel { en: "seq {seq}", de: "Seq {seq}", es: "sec {seq}", hi: "क्रम {seq}", zh: "序号 {seq}" }

    // ---- Capability segments -------------------------------------------------------------
    // Values are translated, not left as diagnostics: this strip exists so lost capability is
    // visible, and a user who cannot read "off" cannot see that the sandbox is off (spec §2).
    CapEngine { en: "engine", de: "Engine", es: "motor", hi: "इंजन", zh: "引擎" }
    CapLlm { en: "LLM", de: "LLM", es: "LLM", hi: "एलएलएम", zh: "大模型" }
    CapSandbox { en: "sandbox", de: "Sandbox", es: "entorno aislado", hi: "सैंडबॉक्स", zh: "沙箱" }
    CapLocal { en: "local", de: "lokal", es: "local", hi: "लोकल", zh: "本地" }
    CapRemote { en: "remote", de: "remote", es: "remoto", hi: "रिमोट", zh: "远程" }
    CapLocalRemote { en: "local+remote", de: "lokal+remote", es: "local+remoto", hi: "लोकल+रिमोट", zh: "本地+远程" }
    CapOffline { en: "offline (deterministic)", de: "offline (deterministisch)", es: "sin conexión (determinista)", hi: "ऑफ़लाइन (नियतात्मक)", zh: "离线（确定性）" }
    CapOn { en: "on", de: "an", es: "activado", hi: "चालू", zh: "开启" }
    CapOff { en: "off", de: "aus", es: "desactivado", hi: "बंद", zh: "关闭" }

    // ---- Token/cost meter ----------------------------------------------------------------
    Meter { en: "↑{input} ↓{output} tok", de: "↑{input} ↓{output} Tok", es: "↑{input} ↓{output} tok", hi: "↑{input} ↓{output} टोकन", zh: "↑{input} ↓{output} 词元" }

    // ---- Editor --------------------------------------------------------------------------
    EditorNoFileOpen { en: "No file open", de: "Keine Datei geöffnet", es: "Ningún archivo abierto", hi: "कोई फ़ाइल खुली नहीं है", zh: "未打开文件" }
    EditorBinary { en: "binary file — not editable", de: "Binärdatei — nicht bearbeitbar", es: "archivo binario — no editable", hi: "बाइनरी फ़ाइल — संपादन योग्य नहीं", zh: "二进制文件 — 不可编辑" }
    EditorTooLarge { en: "file too large to edit", de: "Datei zu groß zum Bearbeiten", es: "archivo demasiado grande para editar", hi: "फ़ाइल संपादित करने के लिए बहुत बड़ी है", zh: "文件过大，无法编辑" }

    // ---- Approval ------------------------------------------------------------------------
    ApprovalNeeded { en: "approval needed: {path}", de: "Genehmigung erforderlich: {path}", es: "se necesita aprobación: {path}", hi: "अनुमोदन आवश्यक: {path}", zh: "需要批准：{path}" }

    // ---- Diff markers --------------------------------------------------------------------
    DiffTrailingNewlineAdded { en: "(trailing newline added)", de: "(abschließender Zeilenumbruch hinzugefügt)", es: "(salto de línea final añadido)", hi: "(अंत में नई पंक्ति जोड़ी गई)", zh: "（已添加行尾换行符）" }
    DiffTrailingNewlineRemoved { en: "(trailing newline removed)", de: "(abschließender Zeilenumbruch entfernt)", es: "(salto de línea final eliminado)", hi: "(अंत की नई पंक्ति हटाई गई)", zh: "（已移除行尾换行符）" }

    // ---- Client-authored, actionable ------------------------------------------------------
    UrlAndTokenRequired { en: "URL and token are required", de: "URL und Token sind erforderlich", es: "se requieren URL y token", hi: "URL और टोकन आवश्यक हैं", zh: "需要 URL 和令牌" }

    // ---- Desktop shell -------------------------------------------------------------------
    ChooseWorkspaceFolder { en: "Choose a workspace folder", de: "Arbeitsbereichsordner auswählen", es: "Elegir una carpeta de espacio de trabajo", hi: "कार्यस्थान फ़ोल्डर चुनें", zh: "选择工作区文件夹" }

    // ---- Language picker -----------------------------------------------------------------
    // The picker's accessible name. The OPTION labels are endonyms (`Locale::endonym`) and are
    // deliberately NOT catalog entries.
    LanguageLabel { en: "Language", de: "Sprache", es: "Idioma", hi: "भाषा", zh: "语言" }

    // ---- Event-log rows ------------------------------------------------------------------
    // `{role}` is a protocol identifier substituted verbatim; `FileEdit`/`Verify`/`TurnComplete`
    // are protocol identifiers embedded in the template and byte-identical across locales.
    RowAgentStarted { en: "▸ {role} started", de: "▸ {role} gestartet", es: "▸ {role} iniciado", hi: "▸ {role} शुरू हुआ", zh: "▸ {role} 已启动" }
    RowAgentFinished { en: "▸ {role} finished", de: "▸ {role} beendet", es: "▸ {role} finalizado", hi: "▸ {role} समाप्त हुआ", zh: "▸ {role} 已完成" }
    RowFileEdit { en: "✎ FileEdit {path} (+{bytes} bytes)", de: "✎ FileEdit {path} (+{bytes} Bytes)", es: "✎ FileEdit {path} (+{bytes} bytes)", hi: "✎ FileEdit {path} (+{bytes} बाइट)", zh: "✎ FileEdit {path} （+{bytes} 字节）" }
    RowVerify { en: "{mark} Verify {detail}", de: "{mark} Verify {detail}", es: "{mark} Verify {detail}", hi: "{mark} Verify {detail}", zh: "{mark} Verify {detail}" }
    VerifyOk { en: "ok", de: "ok", es: "correcto", hi: "ठीक", zh: "正常" }
    RowTurnCompleteOk { en: "● TurnComplete ok", de: "● TurnComplete ok", es: "● TurnComplete correcto", hi: "● TurnComplete ठीक", zh: "● TurnComplete 成功" }
    RowTurnCompleteFailed { en: "● TurnComplete failed", de: "● TurnComplete fehlgeschlagen", es: "● TurnComplete fallido", hi: "● TurnComplete विफल", zh: "● TurnComplete 失败" }
    RowApprovalNeeded { en: "⏸ approval needed: {path}", de: "⏸ Genehmigung erforderlich: {path}", es: "⏸ se necesita aprobación: {path}", hi: "⏸ अनुमोदन आवश्यक: {path}", zh: "⏸ 需要批准：{path}" }
    RowMeter { en: "◷ tokens ↑{input} ↓{output}", de: "◷ Tokens ↑{input} ↓{output}", es: "◷ tokens ↑{input} ↓{output}", hi: "◷ टोकन ↑{input} ↓{output}", zh: "◷ 词元 ↑{input} ↓{output}" }
    // `{message}` is the engine's own text, passed through untranslated; only the `·` framing
    // glyph is the catalog's. It lives here rather than being hardcoded in `render_row` so the
    // framing rule is uniform across every row — and so `zh` can adapt the framing punctuation,
    // as `RowServerError`/`RowClientError`/`RowApprovalNeeded` already do with `：`.
    RowLog { en: "· {message}", de: "· {message}", es: "· {message}", hi: "· {message}", zh: "· {message}" }
    RowServerError { en: "error: {message}", de: "Fehler: {message}", es: "error: {message}", hi: "त्रुटि: {message}", zh: "错误：{message}" }
    RowClientError { en: "client: {message}", de: "Client: {message}", es: "cliente: {message}", hi: "क्लाइंट: {message}", zh: "客户端：{message}" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    /// Collect the `{name}` placeholders in a template, in order of appearance.
    ///
    /// An unterminated `{` PANICS rather than returning what was collected so far. Truncating
    /// silently would let two different malformed templates compare equal in
    /// `placeholder_sets_match_across_locales` — `"a {x} b {y"` and `"a {x} b {z"` both truncate to
    /// `["x"]` — which is the same blindness `every_brace_is_a_closed_placeholder` had. Any caller
    /// reaching this already ran that test's structural scan, so a panic here means a genuine bug.
    fn placeholders(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close) => {
                    out.push(after[..close].to_string());
                    rest = &after[close + 1..];
                }
                None => panic!("unterminated '{{' in template: {s}"),
            }
        }
        out.sort();
        out
    }

    #[test]
    fn no_message_is_empty_in_any_locale() {
        // A missing key or locale is a COMPILE error (the macro requires all five per entry and
        // `t`'s match has no wildcard arm). This catches what the compiler cannot: a stub left as
        // an empty or whitespace-only string.
        for &m in Msg::ALL {
            for loc in Locale::ALL {
                let s = t(loc, m);
                assert!(
                    !s.trim().is_empty(),
                    "empty catalog entry for {m:?} in {loc:?}"
                );
            }
        }
    }

    #[test]
    fn placeholder_sets_match_across_locales() {
        // `tf` substitutes by name, so a locale whose template drops or renames a placeholder
        // would silently render an un-substituted `{name}` to that language's users only.
        for &m in Msg::ALL {
            let expected = placeholders(t(Locale::En, m));
            for loc in Locale::ALL {
                assert_eq!(
                    placeholders(t(loc, m)),
                    expected,
                    "placeholder mismatch for {m:?} in {loc:?}"
                );
            }
        }
    }

    /// Walk a template exactly the way `tf` does, reporting the first structural fault.
    ///
    /// Deliberately NOT a `{`/`}` count comparison. Counting is blind to ORDER, so `"done} at {seq"`
    /// balances (one of each) and passes while `tf` substitutes nothing in it — the `}` is consumed
    /// as literal text and the `{seq}` is left unterminated. The two faults `tf` can hit are:
    ///
    /// (a) a `{` with no following `}` — `tf` emits the rest verbatim, so `{seq` renders raw;
    /// (b) a `}` reached before any `{` opened it — `tf` treats it as literal text, so a template
    ///     whose placeholder braces are transposed silently loses its substitution.
    fn brace_fault(s: &str) -> Option<String> {
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            // Everything before the `{` is literal text as far as `tf` is concerned; a `}` in
            // there was never opened.
            if let Some(stray) = rest[..open].find('}') {
                return Some(format!("'}}' at byte {stray} is not closing any '{{'"));
            }
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                return Some("'{' is never closed".to_string());
            };
            rest = &after[close + 1..];
        }
        // The tail after the last placeholder is literal too.
        rest.find('}')
            .map(|_| "'}' is not closing any '{'".to_string())
    }

    #[test]
    fn every_brace_is_a_closed_placeholder() {
        // `tf` cannot escape a literal brace, so any brace that isn't part of a well-formed
        // `{name}` is always a catalog bug.
        for &m in Msg::ALL {
            for loc in Locale::ALL {
                let s = t(loc, m);
                assert!(
                    brace_fault(s).is_none(),
                    "brace fault for {m:?} in {loc:?}: {} in {s}",
                    brace_fault(s).unwrap()
                );
            }
        }
    }

    #[test]
    fn brace_fault_rejects_what_balanced_counts_accept() {
        // The regression this guardrail exists for: `"done} at {seq"` has one `{` and one `}`, so
        // the old count comparison passed it, while `tf` substitutes nothing in it at all.
        assert_eq!(
            tf(Locale::En, Msg::SeqLabel, &[("seq", "7")]),
            "seq 7",
            "sanity: a well-formed template does substitute"
        );
        assert!(brace_fault("done} at {seq").is_some());
        // Both faults independently.
        assert!(brace_fault("a {x} b {y").is_some()); // unterminated `{`
        assert!(brace_fault("a} b").is_some()); // stray `}` with no `{` at all
        assert!(brace_fault("{x} y}").is_some()); // stray `}` in the tail
                                                  // …and well-formed templates still pass.
        assert!(brace_fault("no placeholders").is_none());
        assert!(brace_fault("{a} and {b}").is_none());
        assert!(brace_fault("").is_none());
    }

    #[test]
    #[should_panic(expected = "unterminated")]
    fn placeholders_panics_rather_than_truncating() {
        // Truncating on an unterminated `{` would make `"a {x} b {y"` and `"a {x} b {z"` both
        // yield `["x"]`, so `placeholder_sets_match_across_locales` could not tell them apart.
        placeholders("a {x} b {y");
    }

    #[test]
    fn protocol_identifiers_survive_translation() {
        // Three templates embed a protocol identifier (spec §2: shared wire vocabulary, never
        // translated). This fails if a future translation pass localizes one of them.
        for loc in Locale::ALL {
            assert!(
                t(loc, Msg::RowFileEdit).contains("FileEdit"),
                "RowFileEdit lost the FileEdit identifier in {loc:?}"
            );
            assert!(
                t(loc, Msg::RowVerify).contains("Verify"),
                "RowVerify lost the Verify identifier in {loc:?}"
            );
            for m in [Msg::RowTurnCompleteOk, Msg::RowTurnCompleteFailed] {
                assert!(
                    t(loc, m).contains("TurnComplete"),
                    "{m:?} lost the TurnComplete identifier in {loc:?}"
                );
            }
        }
    }

    #[test]
    fn tf_substitutes_named_placeholders() {
        assert_eq!(
            tf(Locale::En, Msg::ApprovalNeeded, &[("path", "src/main.rs")]),
            "approval needed: src/main.rs"
        );
    }

    #[test]
    fn tf_substitutes_into_multi_byte_templates() {
        // `tf` slices its template by BYTE offset (`&rest[..open]`, `&after[..close]`). That is
        // correct because `str::find` returns byte offsets that are always char boundaries — but
        // only the `en` cases above demonstrate it, and every `en` template is pure ASCII. So a
        // future "optimization" swapping in char-index arithmetic (or any hand-rolled offset math)
        // would panic or mis-slice for Hindi and Chinese users ONLY, with the suite still green.
        //
        // These are the sharpest templates in the catalog: `zh`'s `RowFileEdit` puts a 3-byte
        // full-width `（` immediately against `{bytes}` with no ASCII separator, and `zh`'s
        // `RowApprovalNeeded` puts a 3-byte `：` immediately against `{path}`.
        assert_eq!(
            tf(
                Locale::ZhHans,
                Msg::RowFileEdit,
                &[("path", "src/lib.rs"), ("bytes", "42")]
            ),
            "✎ FileEdit src/lib.rs （+42 字节）"
        );
        assert_eq!(
            tf(
                Locale::ZhHans,
                Msg::RowApprovalNeeded,
                &[("path", "src/主.rs")]
            ),
            "⏸ 需要批准：src/主.rs"
        );
        assert_eq!(
            tf(
                Locale::Hi,
                Msg::RowFileEdit,
                &[("path", "src/lib.rs"), ("bytes", "42")]
            ),
            "✎ FileEdit src/lib.rs (+42 बाइट)"
        );
        assert_eq!(
            tf(Locale::Hi, Msg::ApprovalNeeded, &[("path", "src/main.rs")]),
            "अनुमोदन आवश्यक: src/main.rs"
        );
        // A multi-byte VALUE substituted into a multi-byte template — the substituted bytes are
        // pushed, never re-scanned, so the offsets that follow stay on the template's boundaries.
        assert_eq!(
            tf(Locale::Hi, Msg::ApprovalNeeded, &[("path", "स्रोत/मुख्य.rs")]),
            "अनुमोदन आवश्यक: स्रोत/मुख्य.rs"
        );
        // …and an unsupplied placeholder still comes back verbatim out of a multi-byte template,
        // which is the path that re-emits the name it just sliced.
        assert_eq!(
            tf(Locale::ZhHans, Msg::RowFileEdit, &[("path", "src/lib.rs")]),
            "✎ FileEdit src/lib.rs （+{bytes} 字节）"
        );
    }

    #[test]
    fn tf_leaves_an_unsupplied_placeholder_verbatim() {
        // Visibly wrong beats silently blank: a missing arg must be reportable, not invisible.
        assert_eq!(
            tf(Locale::En, Msg::ApprovalNeeded, &[]),
            "approval needed: {path}"
        );
    }

    #[test]
    fn tf_never_rescans_substituted_values() {
        // `{path}` and `{message}` carry attacker-influenceable content. A value containing a
        // brace sequence must render verbatim, never trigger a second substitution round.
        assert_eq!(
            tf(Locale::En, Msg::ApprovalNeeded, &[("path", "{path}")]),
            "approval needed: {path}"
        );
        assert_eq!(
            tf(
                Locale::En,
                Msg::ApprovalNeeded,
                &[("path", "a{b}c"), ("b", "BOOM")]
            ),
            "approval needed: a{b}c"
        );
    }

    #[test]
    fn locale_tags_round_trip() {
        for loc in Locale::ALL {
            assert_eq!(Locale::from_tag(loc.tag()), Some(loc), "{loc:?}");
        }
    }

    #[test]
    fn from_tag_normalizes_case_separator_and_whitespace() {
        assert_eq!(Locale::from_tag("en_US"), Some(Locale::En));
        assert_eq!(Locale::from_tag("EN-us"), Some(Locale::En));
        assert_eq!(Locale::from_tag("  en  "), Some(Locale::En));
        assert_eq!(Locale::from_tag("de-AT"), Some(Locale::De));
        assert_eq!(Locale::from_tag("es-419"), Some(Locale::Es));
    }

    #[test]
    fn from_tag_rejects_locales_with_no_catalog() {
        assert_eq!(Locale::from_tag("pt-BR"), None);
        assert_eq!(Locale::from_tag("fr"), None);
        assert_eq!(Locale::from_tag(""), None);
    }

    #[test]
    fn chinese_is_matched_by_script_not_primary_subtag() {
        // Simplified is the only Chinese catalog we ship.
        assert_eq!(Locale::from_tag("zh-Hans"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh-Hans-CN"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh-CN"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh-SG"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh"), Some(Locale::ZhHans));
        // Traditional falls back to en rather than being served the wrong script.
        assert_eq!(Locale::from_tag("zh-Hant"), None);
        assert_eq!(Locale::from_tag("zh-TW"), None);
        assert_eq!(Locale::from_tag("zh-HK"), None);
        assert_eq!(Locale::from_tag("zh-MO"), None);
    }

    #[test]
    fn endonyms_are_distinct_and_untranslated() {
        let names: Vec<&str> = Locale::ALL.iter().map(|l| l.endonym()).collect();
        assert_eq!(
            names,
            ["English", "Deutsch", "Español", "हिन्दी", "中文（简体）"]
        );
    }
}
