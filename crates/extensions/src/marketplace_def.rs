//! Parses a Claude Code marketplace manifest (`.claude-plugin/marketplace.json`): the marketplace
//! `name` plus the list of plugins it offers. A plugin's `source` is either a local path (relative
//! to the marketplace root, resolvable on disk) or a remote descriptor (not materialized by this
//! slice). No I/O here — pure parsing.

use serde_json::Value;

/// Where a plugin's files live, per its marketplace entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A path relative to the marketplace root, e.g. `./plugins/foo`.
    LocalPath(String),
    /// A remote source (github/git/…). Kept verbatim; this slice does not materialize it.
    Remote(Value),
}

/// One plugin offered by a marketplace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: PluginSource,
    pub description: Option<String>,
}

/// A parsed `marketplace.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    pub name: String,
    pub plugins: Vec<MarketplaceEntry>,
}

/// Parse a `marketplace.json` document. Errors on invalid JSON or a missing top-level
/// `name`/`plugins`. An empty `plugins` array is valid. A plugin entry missing a non-empty `name`
/// or a usable `source` is skipped. A string `source` is a `LocalPath`; an object `source` is
/// `Remote`; any other `source` shape skips the entry.
pub fn parse_marketplace_json(json: &str) -> anyhow::Result<Marketplace> {
    let v: Value = serde_json::from_str(json)?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("marketplace.json missing `name`"))?
        .to_string();
    let plugins_val = v
        .get("plugins")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow::anyhow!("marketplace.json missing `plugins` array"))?;

    let mut plugins = Vec::new();
    for entry in plugins_val {
        let Some(pname) = entry
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(src_val) = entry.get("source") else {
            continue;
        };
        let source = match src_val {
            Value::String(s) if !s.is_empty() => PluginSource::LocalPath(s.clone()),
            Value::Object(_) => PluginSource::Remote(src_val.clone()),
            _ => continue,
        };
        let description = entry
            .get("description")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string());
        plugins.push(MarketplaceEntry {
            name: pname.to_string(),
            source,
            description,
        });
    }
    Ok(Marketplace { name, plugins })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_local_and_remote_sources() {
        let json = r#"{
            "name": "acme",
            "owner": { "name": "x" },
            "plugins": [
                { "name": "foo", "source": "./plugins/foo", "description": "d" },
                { "name": "bar", "source": { "source": "github", "repo": "acme/bar" } }
            ]
        }"#;
        let mp = parse_marketplace_json(json).unwrap();
        assert_eq!(mp.name, "acme");
        assert_eq!(mp.plugins.len(), 2);
        assert_eq!(mp.plugins[0].name, "foo");
        assert_eq!(
            mp.plugins[0].source,
            PluginSource::LocalPath("./plugins/foo".to_string())
        );
        assert_eq!(mp.plugins[0].description.as_deref(), Some("d"));
        assert!(matches!(mp.plugins[1].source, PluginSource::Remote(_)));
    }

    #[test]
    fn empty_plugins_is_ok() {
        let mp = parse_marketplace_json(r#"{"name":"acme","plugins":[]}"#).unwrap();
        assert!(mp.plugins.is_empty());
    }

    #[test]
    fn missing_name_or_plugins_errors() {
        assert!(parse_marketplace_json(r#"{"plugins":[]}"#).is_err());
        assert!(parse_marketplace_json(r#"{"name":"acme"}"#).is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_marketplace_json("{ not json").is_err());
    }

    #[test]
    fn entry_missing_name_or_source_is_skipped() {
        let json = r#"{"name":"acme","plugins":[
            { "source": "./x" },
            { "name": "ok", "source": "./ok" },
            { "name": "nosrc" }
        ]}"#;
        let mp = parse_marketplace_json(json).unwrap();
        let names: Vec<_> = mp.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ok"]);
    }
}
