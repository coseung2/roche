//! Codex model-catalog discovery, configuration parsing, and fallbacks.

use std::{ffi::OsStr, path::PathBuf};

use serde_json::Value;

use super::types::{CodexCatalogModel, CodexReasoningLevel};

pub(super) fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let profile = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_default();
            profile.join(".codex")
        })
}

pub(super) fn configured_model_catalog_path(config_toml: &str) -> Option<PathBuf> {
    config_toml.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("model_catalog_json")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let value = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
        let value = value
            .strip_suffix('"')
            .or_else(|| value.strip_suffix('\''))?;
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

pub(super) fn parse_catalog_models(root: &Value) -> Vec<CodexCatalogModel> {
    let Some(models) = root.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut catalog = models
        .iter()
        .filter_map(|entry| {
            if let Some(slug) = entry.as_str() {
                return Some(CodexCatalogModel {
                    slug: slug.to_owned(),
                    display_name: slug.to_owned(),
                    description: None,
                    default_reasoning_level: None,
                    supported_reasoning_levels: Vec::new(),
                    priority: None,
                });
            }
            let slug = entry
                .get("slug")
                .or_else(|| entry.get("id"))
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)?;
            let display_name = entry
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(slug);
            Some(CodexCatalogModel {
                slug: slug.to_owned(),
                display_name: display_name.to_owned(),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                default_reasoning_level: entry
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                supported_reasoning_levels: entry
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| {
                                let effort = level.get("effort").and_then(Value::as_str)?;
                                Some(CodexReasoningLevel {
                                    effort: effort.to_owned(),
                                    description: level
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                priority: entry.get("priority").and_then(Value::as_i64),
            })
        })
        .collect::<Vec<_>>();
    catalog.sort_by_key(|model| std::cmp::Reverse(model.priority));
    catalog
}

pub(super) fn read_codex_catalog_models() -> Result<(String, Vec<CodexCatalogModel>), String> {
    let home = codex_home();
    let configured = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| configured_model_catalog_path(&text));

    let mut candidates = Vec::new();
    if let Some(path) = configured {
        candidates.push(path);
    }
    for name in ["opencodex-catalog.json", "models_cache.json"] {
        candidates.push(home.join(name));
    }
    for name in ["codex-plus-opencode-go.json", "opencode-go.json"] {
        candidates.push(home.join("model-catalogs").join(name));
    }

    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let models = parse_catalog_models(&root);
        if models.is_empty() {
            continue;
        }
        let source = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("codex catalog")
            .to_owned();
        return Ok((source, models));
    }

    Err(format!(
        "no readable model catalog under {}",
        home.display()
    ))
}
