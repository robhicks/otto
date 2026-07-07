//! Pure logic for the `otto plugin` install action: the marketplace lockfile model and the
//! `settings.json` `enabledPlugins` merge function. No filesystem or process I/O — those live at
//! the CLI edge in `crates/engine/src/plugin_cli.rs`, matching this crate's existing convention
//! (discovery's `home` is always an explicit parameter; parsing functions take strings and return
//! data, never touch disk). Mirrors `plugin_def.rs`/`marketplace_def.rs`'s style of hand-rolled
//! `serde_json::Value` parsing rather than `#[derive(Deserialize)]` (this crate has no `serde`
//! dependency, only `serde_json`).

use std::collections::BTreeMap;

use serde_json::Value;

/// One locked marketplace: where it came from, what ref/commit it's pinned to, and when it was
/// last installed/updated. `updated_at_unix` is seconds since the Unix epoch (read at the CLI
/// edge via `SystemTime::now()`, never inside this pure module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceLock {
    pub url: String,
    pub git_ref: String,
    pub commit: String,
    pub updated_at_unix: u64,
}

/// The full lockfile: marketplace name -> its lock entry. A `BTreeMap` keeps serialized output in
/// sorted-key order, so the on-disk file stays git-diff-friendly across updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplaceLockfile {
    pub entries: BTreeMap<String, MarketplaceLock>,
}

impl MarketplaceLockfile {
    /// Parse a lockfile document. Malformed JSON or a non-object top level yields an empty
    /// lockfile (never fatal — matches every other `.claude/` reader in this crate). An entry
    /// missing a required field, or with a non-string/non-number value, is skipped.
    pub fn parse(json: &str) -> Self {
        let mut entries = BTreeMap::new();
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
            return Self { entries };
        };
        for (name, v) in root {
            let Some(url) = v.get("url").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(git_ref) = v.get("ref").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(commit) = v.get("commit").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(updated_at_unix) = v.get("updated_at_unix").and_then(|x| x.as_u64()) else {
                continue;
            };
            entries.insert(
                name,
                MarketplaceLock {
                    url: url.to_string(),
                    git_ref: git_ref.to_string(),
                    commit: commit.to_string(),
                    updated_at_unix,
                },
            );
        }
        Self { entries }
    }

    /// Serialize to pretty JSON. `BTreeMap` iteration order (sorted keys) is preserved by
    /// `serde_json::Map` (this workspace does not enable the `preserve_order` feature, so
    /// `serde_json::Map` is itself `BTreeMap`-backed).
    pub fn to_json(&self) -> String {
        let mut root = serde_json::Map::new();
        for (name, lock) in &self.entries {
            root.insert(
                name.clone(),
                serde_json::json!({
                    "url": lock.url,
                    "ref": lock.git_ref,
                    "commit": lock.commit,
                    "updated_at_unix": lock.updated_at_unix,
                }),
            );
        }
        serde_json::to_string_pretty(&Value::Object(root)).unwrap()
    }
}

/// Insert, remove, or flip an `"<plugin>@<marketplace>"` key in a `settings.json` document's
/// `enabledPlugins` object, returning the rewritten JSON. Every other top-level key (`hooks`,
/// `permissions`, other `enabledPlugins` entries, …) is preserved untouched.
///
/// - `enabled = Some(b)` inserts/overwrites `key` with the bool `b`.
/// - `enabled = None` removes `key` entirely (used by `uninstall`, so `settings.json` doesn't
///   accumulate dead entries for plugins that were never re-enabled).
///
/// Malformed or absent input is treated as `{}` (never fatal — the CLI is creating this file for
/// the first time in the common case).
pub fn set_enabled_plugin(settings_json: &str, key: &str, enabled: Option<bool>) -> String {
    let mut root: Value = serde_json::from_str(settings_json)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let root_obj = root.as_object_mut().expect("filtered to object above");

    let enabled_plugins = root_obj
        .entry("enabledPlugins".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !enabled_plugins.is_object() {
        *enabled_plugins = Value::Object(serde_json::Map::new());
    }
    let map = enabled_plugins
        .as_object_mut()
        .expect("just ensured object");

    match enabled {
        Some(b) => {
            map.insert(key.to_string(), Value::Bool(b));
        }
        None => {
            map.remove(key);
        }
    }

    serde_json::to_string_pretty(&root).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut lf = MarketplaceLockfile::default();
        lf.entries.insert(
            "acme".to_string(),
            MarketplaceLock {
                url: "https://example.com/acme.git".to_string(),
                git_ref: "main".to_string(),
                commit: "abc123".to_string(),
                updated_at_unix: 1_720_000_000,
            },
        );
        let json = lf.to_json();
        let back = MarketplaceLockfile::parse(&json);
        assert_eq!(back, lf);
    }

    #[test]
    fn parse_empty_or_malformed_is_empty() {
        assert!(MarketplaceLockfile::parse("").entries.is_empty());
        assert!(MarketplaceLockfile::parse("not json").entries.is_empty());
        assert!(MarketplaceLockfile::parse("[]").entries.is_empty());
        assert!(MarketplaceLockfile::parse("{}").entries.is_empty());
    }

    #[test]
    fn entry_missing_a_required_field_is_skipped() {
        let json = r#"{
            "good": {"url":"u","ref":"main","commit":"c","updated_at_unix":1},
            "bad":  {"url":"u","ref":"main"}
        }"#;
        let lf = MarketplaceLockfile::parse(json);
        assert_eq!(lf.entries.len(), 1);
        assert!(lf.entries.contains_key("good"));
    }

    #[test]
    fn to_json_sorts_keys() {
        let mut lf = MarketplaceLockfile::default();
        for name in ["zeta", "alpha", "mid"] {
            lf.entries.insert(
                name.to_string(),
                MarketplaceLock {
                    url: "u".to_string(),
                    git_ref: "main".to_string(),
                    commit: "c".to_string(),
                    updated_at_unix: 1,
                },
            );
        }
        let json = lf.to_json();
        let alpha = json.find("alpha").unwrap();
        let mid = json.find("mid").unwrap();
        let zeta = json.find("zeta").unwrap();
        assert!(
            alpha < mid && mid < zeta,
            "expected sorted key order: {json}"
        );
    }

    #[test]
    fn set_enabled_plugin_inserts_into_empty_settings() {
        let out = set_enabled_plugin("", "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_preserves_other_top_level_keys() {
        let existing = r#"{"hooks":{"PreToolUse":[]},"permissions":{"allow":["Read(**)"]}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("hooks").is_some(), "hooks key must survive: {out}");
        assert!(
            v.get("permissions").is_some(),
            "permissions key must survive: {out}"
        );
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_preserves_other_enabled_plugins_entries() {
        let existing = r#"{"enabledPlugins":{"bar@acme":true}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["bar@acme"], Value::Bool(true));
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_none_removes_the_key() {
        let existing = r#"{"enabledPlugins":{"foo@acme":true,"bar@acme":true}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["enabledPlugins"].get("foo@acme").is_none());
        assert_eq!(v["enabledPlugins"]["bar@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_tolerates_malformed_input() {
        let out = set_enabled_plugin("not json", "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_replaces_non_object_enabled_plugins() {
        // A hand-edited settings.json with a bogus `enabledPlugins` value must not panic.
        let existing = r#"{"enabledPlugins": "oops"}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }
}
