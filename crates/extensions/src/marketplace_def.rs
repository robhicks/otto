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

/// A `PluginSource::Remote` descriptor resolved to a `git clone` target. Pure data — the CLI edge
/// consumes this to clone into the repos cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClone {
    pub url: String,
    /// An optional pin (commit/tag/branch/ref) checked out after clone. `None` = the default branch.
    pub git_ref: Option<String>,
}

/// A single URL/path segment is safe iff it is non-empty, not `-`-prefixed (argv injection), not
/// `.`/`..`, and contains no `/` or `\`.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
}

/// Resolve a `Remote` plugin source (`{"source":"github","repo":"owner/name"}` or
/// `{"source":"git","url":"…"}`) to a clone URL plus optional ref. Pure — no I/O. Errors, naming
/// the unsupported shape, on an unknown `source` kind or a malformed descriptor.
///
/// Pin precedence, most-specific first: `commit` > `tag` > `branch` > `ref`.
pub fn resolve_remote_source(src: &Value) -> anyhow::Result<RemoteClone> {
    let obj = src
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("remote plugin source must be a JSON object"))?;
    let kind = obj
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("remote plugin source missing string `source` field"))?;

    let git_ref = ["commit", "tag", "branch", "ref"]
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .map(|s| s.to_string());

    let url = match kind {
        "github" => {
            let repo = obj
                .get("repo")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("github source missing string `repo` field"))?;
            let (owner, name) = repo.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("github `repo` must be 'owner/name', got '{repo}'")
            })?;
            if !valid_segment(owner) || !valid_segment(name) {
                anyhow::bail!("github `repo` has an invalid segment: '{repo}'");
            }
            format!("https://github.com/{owner}/{name}")
        }
        "git" => obj
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("git source missing string `url` field"))?
            .to_string(),
        other => {
            anyhow::bail!("unsupported remote source kind '{other}' (supported: github, git)")
        }
    };

    Ok(RemoteClone { url, git_ref })
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

    #[test]
    fn resolve_github_source_builds_https_url() {
        let v = serde_json::json!({"source": "github", "repo": "acme/foo"});
        let rc = resolve_remote_source(&v).unwrap();
        assert_eq!(rc.url, "https://github.com/acme/foo");
        assert_eq!(rc.git_ref, None);
    }

    #[test]
    fn resolve_git_source_is_verbatim_url() {
        let v = serde_json::json!({"source": "git", "url": "https://x.example/y.git"});
        let rc = resolve_remote_source(&v).unwrap();
        assert_eq!(rc.url, "https://x.example/y.git");
        assert_eq!(rc.git_ref, None);
    }

    #[test]
    fn resolve_ref_precedence_commit_wins_then_tag_branch_ref() {
        let all = serde_json::json!({
            "source": "git", "url": "u",
            "ref": "r", "branch": "b", "tag": "t", "commit": "c"
        });
        assert_eq!(
            resolve_remote_source(&all).unwrap().git_ref.as_deref(),
            Some("c")
        );

        let no_commit =
            serde_json::json!({"source":"git","url":"u","ref":"r","branch":"b","tag":"t"});
        assert_eq!(
            resolve_remote_source(&no_commit)
                .unwrap()
                .git_ref
                .as_deref(),
            Some("t")
        );

        let only_ref = serde_json::json!({"source":"git","url":"u","ref":"r"});
        assert_eq!(
            resolve_remote_source(&only_ref).unwrap().git_ref.as_deref(),
            Some("r")
        );
    }

    #[test]
    fn resolve_empty_higher_precedence_pin_falls_through() {
        let v = serde_json::json!({"source":"git","url":"u","commit":"","tag":"t"});
        assert_eq!(
            resolve_remote_source(&v).unwrap().git_ref.as_deref(),
            Some("t")
        );
    }

    #[test]
    fn resolve_unknown_kind_errors_naming_the_kind() {
        let v = serde_json::json!({"source": "gitlab", "repo": "a/b"});
        let e = resolve_remote_source(&v).unwrap_err();
        assert!(e.to_string().contains("gitlab"), "got: {e}");
    }

    #[test]
    fn resolve_github_rejects_malformed_repo() {
        for bad in [
            serde_json::json!({"source":"github","repo":"noslash"}),
            serde_json::json!({"source":"github","repo":"../escape/x"}),
            serde_json::json!({"source":"github","repo":"-flag/x"}),
            serde_json::json!({"source":"github"}),
        ] {
            assert!(resolve_remote_source(&bad).is_err(), "should reject {bad}");
        }
    }
}
