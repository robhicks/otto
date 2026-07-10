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

/// The full lockfile: marketplace name -> its lock entry, plus materialized remote-sourced
/// plugins keyed by their `"<plugin>@<marketplace>"` enable-key. `BTreeMap`s keep serialized
/// output in sorted-key order, so the on-disk file stays git-diff-friendly across updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplaceLockfile {
    /// Installed marketplaces, keyed by declared marketplace name.
    pub entries: BTreeMap<String, MarketplaceLock>,
    /// Materialized remote-sourced plugins, keyed by the `"<plugin>@<marketplace>"` enable-key.
    pub plugins: BTreeMap<String, MarketplaceLock>,
}

/// Parse a `name -> lock` object into a `BTreeMap`, skipping any entry missing a required field
/// (matching every other `.claude/` reader in this crate — tolerant, never fatal).
fn parse_lock_map(
    map: Option<&serde_json::Map<String, Value>>,
) -> BTreeMap<String, MarketplaceLock> {
    let mut out = BTreeMap::new();
    let Some(map) = map else {
        return out;
    };
    for (name, v) in map {
        let (Some(url), Some(git_ref), Some(commit), Some(updated_at_unix)) = (
            v.get("url").and_then(|x| x.as_str()),
            v.get("ref").and_then(|x| x.as_str()),
            v.get("commit").and_then(|x| x.as_str()),
            v.get("updated_at_unix").and_then(|x| x.as_u64()),
        ) else {
            continue;
        };
        out.insert(
            name.clone(),
            MarketplaceLock {
                url: url.to_string(),
                git_ref: git_ref.to_string(),
                commit: commit.to_string(),
                updated_at_unix,
            },
        );
    }
    out
}

impl MarketplaceLockfile {
    /// Parse a lockfile document. Malformed JSON or a non-object top level yields an empty
    /// lockfile (never fatal — matches every other `.claude/` reader in this crate). An entry
    /// missing a required field, or with a non-string/non-number value, is skipped.
    pub fn parse(json: &str) -> Self {
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
            return Self::default();
        };
        // Nested format: a top-level object carrying a `marketplaces` and/or `plugins` object.
        // Otherwise treat the whole object as the legacy flat `name -> lock` marketplaces map.
        // (Edge case: a legacy marketplace literally named "marketplaces"/"plugins" — accepted as
        // out of scope; these are single-user pre-release lockfiles.)
        let nested = root
            .get("marketplaces")
            .map(Value::is_object)
            .unwrap_or(false)
            || root.get("plugins").map(Value::is_object).unwrap_or(false);
        if nested {
            Self {
                entries: parse_lock_map(root.get("marketplaces").and_then(Value::as_object)),
                plugins: parse_lock_map(root.get("plugins").and_then(Value::as_object)),
            }
        } else {
            Self {
                entries: parse_lock_map(Some(&root)),
                plugins: BTreeMap::new(),
            }
        }
    }

    /// Serialize to pretty JSON. `BTreeMap` iteration order (sorted keys) is preserved by
    /// `serde_json::Map` (this workspace does not enable the `preserve_order` feature, so
    /// `serde_json::Map` is itself `BTreeMap`-backed).
    pub fn to_json(&self) -> String {
        let to_obj = |map: &BTreeMap<String, MarketplaceLock>| {
            let mut o = serde_json::Map::new();
            for (name, lock) in map {
                o.insert(
                    name.clone(),
                    serde_json::json!({
                        "url": lock.url,
                        "ref": lock.git_ref,
                        "commit": lock.commit,
                        "updated_at_unix": lock.updated_at_unix,
                    }),
                );
            }
            Value::Object(o)
        };
        let mut root = serde_json::Map::new();
        root.insert("marketplaces".to_string(), to_obj(&self.entries));
        root.insert("plugins".to_string(), to_obj(&self.plugins));
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

    #[test]
    fn nested_round_trip_with_marketplaces_and_plugins() {
        let mut lf = MarketplaceLockfile::default();
        lf.entries.insert(
            "acme".to_string(),
            MarketplaceLock {
                url: "u".to_string(),
                git_ref: "main".to_string(),
                commit: "c".to_string(),
                updated_at_unix: 1,
            },
        );
        lf.plugins.insert(
            "foo@acme".to_string(),
            MarketplaceLock {
                url: "g".to_string(),
                git_ref: "v1".to_string(),
                commit: "d".to_string(),
                updated_at_unix: 2,
            },
        );
        let back = MarketplaceLockfile::parse(&lf.to_json());
        assert_eq!(back, lf);
    }

    #[test]
    fn flat_legacy_format_parses_as_marketplaces_only() {
        let legacy = r#"{"acme":{"url":"u","ref":"main","commit":"c","updated_at_unix":1}}"#;
        let lf = MarketplaceLockfile::parse(legacy);
        assert_eq!(lf.entries.len(), 1);
        assert_eq!(lf.entries["acme"].git_ref, "main");
        assert!(lf.plugins.is_empty());
    }

    #[test]
    fn nested_with_only_plugins_section_parses() {
        let json =
            r#"{"plugins":{"foo@acme":{"url":"g","ref":"v1","commit":"d","updated_at_unix":2}}}"#;
        let lf = MarketplaceLockfile::parse(json);
        assert!(lf.entries.is_empty());
        assert_eq!(lf.plugins.len(), 1);
        assert_eq!(lf.plugins["foo@acme"].commit, "d");
    }
}
