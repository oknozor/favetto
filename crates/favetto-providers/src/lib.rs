//! Provider/model catalog for the one-shot task wizard.
//!
//! Providers are the ones the operator has authenticated with opencode, read from
//! its credentials file (`~/.local/share/opencode/auth.json`). Each provider's
//! available models come from the models.dev catalog
//! (`https://models.dev/api.json`, overridable with `OPENCODE_MODELS_URL`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default models.dev catalog endpoint.
pub const DEFAULT_CATALOG_URL: &str = "https://models.dev/api.json";

/// A model advertised by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
}

/// A configured provider and its available models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub models: Vec<Model>,
}

/// Opencode's credentials file, e.g. `~/.local/share/opencode/auth.json`.
pub fn auth_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("opencode").join("auth.json"))
}

/// Provider ids present in `auth.json` (i.e. configured/authenticated).
pub fn configured_providers() -> anyhow::Result<Vec<String>> {
    let path = auth_path().ok_or_else(|| anyhow::anyhow!("cannot locate the data directory"))?;
    configured_providers_from(&path)
}

/// Provider ids present in a specific `auth.json`.
pub fn configured_providers_from(path: &Path) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    let map: BTreeMap<String, serde_json::Value> = serde_json::from_str(&raw)?;
    Ok(map.into_keys().collect())
}

/// Fetch the catalog and keep only `configured` providers.
pub async fn fetch_catalog(
    client: &reqwest::Client,
    configured: &[String],
    url: &str,
) -> anyhow::Result<Vec<Provider>> {
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_catalog(&body, configured)
}

/// Parse a models.dev catalog body, keeping only `configured` providers.
pub fn parse_catalog(body: &str, configured: &[String]) -> anyhow::Result<Vec<Provider>> {
    let all: BTreeMap<String, CatalogProvider> = serde_json::from_str(body)?;

    let mut providers = Vec::new();
    for id in configured {
        let Some(p) = all.get(id) else {
            continue;
        };
        let mut models: Vec<Model> = p
            .models
            .iter()
            .map(|(mid, m)| Model {
                id: non_empty(&m.id, mid),
                name: non_empty(&m.name, mid),
            })
            .collect();
        models.sort_by_key(|m| m.name.to_lowercase());
        providers.push(Provider {
            id: id.clone(),
            name: non_empty(&p.name, id),
            models,
        });
    }
    providers.sort_by_key(|p| p.name.to_lowercase());
    Ok(providers)
}

fn non_empty(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[derive(Deserialize)]
struct CatalogProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "deepseek": { "name": "DeepSeek", "models": {
            "deepseek-v4-flash": { "id": "deepseek-v4-flash", "name": "DeepSeek V4 Flash" },
            "deepseek-v4-pro":   { "id": "deepseek-v4-pro",   "name": "DeepSeek V4 Pro" }
        }},
        "mistral": { "name": "Mistral", "models": {
            "mistral-large": { "name": "Mistral Large" }
        }},
        "unconfigured": { "name": "Nope", "models": {} }
    }"#;

    #[test]
    fn parse_keeps_only_configured_providers() {
        let providers =
            parse_catalog(SAMPLE, &["deepseek".to_string(), "mistral".to_string()]).unwrap();
        assert_eq!(providers.len(), 2);
        // Sorted by display name: DeepSeek, Mistral.
        assert_eq!(providers[0].id, "deepseek");
        assert_eq!(providers[0].name, "DeepSeek");
        assert_eq!(
            providers[0]
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
        // Missing `id`/`name` fall back to the map key / display name.
        assert_eq!(providers[1].models[0].id, "mistral-large");
        assert_eq!(providers[1].models[0].name, "Mistral Large");
    }

    #[test]
    fn configured_providers_reads_auth_json_keys() {
        let dir = std::env::temp_dir().join(format!("favetto-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"deepseek":{"type":"api","key":"x"},"mistral":{"type":"api","key":"y"}}"#,
        )
        .unwrap();
        let mut ids = configured_providers_from(&path).unwrap();
        ids.sort();
        assert_eq!(ids, vec!["deepseek", "mistral"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
