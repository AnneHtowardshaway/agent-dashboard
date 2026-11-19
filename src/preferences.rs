//! Shared preference storage. Mirrors the schema written by
//! compat/preferences.mjs so the desktop, IDE and multi-host surfaces agree.

use crate::json::{obj, parse, to_string, Value};
use std::fs;
use std::path::Path;

const KEYS: [&str; 5] = ["language", "languageGuideDismissed", "usageGuideDismissed", "speedStyle", "usageScope"];

pub fn read(file: &Path) -> Value {
    let fallback = obj(vec![
        ("language", Value::Null),
        ("languageGuideDismissed", false.into()),
        ("usageGuideDismissed", false.into()),
        ("speedStyle", Value::Null),
        ("revision", 0.0.into()),
    ]);
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(_) => return fallback,
    };
    let document = match parse(text.trim_start_matches('\u{feff}')) {
        Ok(value) => value,
        Err(_) => return fallback,
    };
    if document.get("schema").and_then(Value::as_f64) != Some(1.0) {
        return fallback;
    }
    let language = match document.get("language").and_then(Value::as_str) {
        Some(value) if ["en", "zh-CN", "zh-TW"].contains(&value) => Value::from(value),
        _ => Value::Null,
    };
    let speed_style = match document.get("speedStyle").and_then(Value::as_str) {
        Some(value) if ["minimal", "cat"].contains(&value) => Value::from(value),
        _ => Value::Null,
    };
    let usage_scope = document
        .get("usageScope")
        .and_then(Value::as_str)
        .filter(|value| ["local", "account"].contains(value))
        .map(Value::from);
    let revision = document.get("revision").and_then(Value::as_f64).unwrap_or(0.0);
    let mut items = vec![
        ("language", language),
        (
            "languageGuideDismissed",
            document.get("languageGuideDismissed").and_then(Value::as_bool).unwrap_or(false).into(),
        ),
        (
            "usageGuideDismissed",
            document.get("usageGuideDismissed").and_then(Value::as_bool).unwrap_or(false).into(),
        ),
        ("speedStyle", speed_style),
        ("revision", revision.into()),
    ];
    if let Some(scope) = usage_scope {
        items.push(("usageScope", scope));
    }
    obj(items)
}

pub fn update(file: &Path, input: &Value) -> Result<Value, String> {
    let entries = match input {
        Value::Obj(items) => items,
        _ => return Err("Invalid preference input".to_string()),
    };
    if entries.is_empty() {
        return Ok(read(file));
    }
    for (key, value) in entries {
        if !KEYS.contains(&key.as_str()) {
            return Err(format!("Unknown preference '{}'", key));
        }
        match key.as_str() {
            "language" => {
                if !matches!(value.as_str(), Some("en") | Some("zh-CN") | Some("zh-TW")) {
                    return Err("Invalid language preference".to_string());
                }
            }
            "speedStyle" => {
                if !matches!(value.as_str(), Some("minimal") | Some("cat")) {
                    return Err("Invalid speed style preference".to_string());
                }
            }
            "usageScope" => {
                if !matches!(value.as_str(), Some("local") | Some("account")) {
                    return Err("Invalid usage scope preference".to_string());
                }
            }
            "languageGuideDismissed" | "usageGuideDismissed" => {
                if value.as_bool() != Some(true) {
                    return Err("Invalid guide preference".to_string());
                }
            }
            _ => {}
        }
    }

    let previous = read(file);
    let pick = |key: &str| entries.iter().find(|(name, _)| name == key).map(|(_, value)| value.clone());
    let language = pick("language").unwrap_or_else(|| previous.get("language").cloned().unwrap_or(Value::Null));
    let language_guide = previous.get("languageGuideDismissed").and_then(Value::as_bool).unwrap_or(false)
        || entries.iter().any(|(name, _)| name == "language")
        || pick("languageGuideDismissed").and_then(|value| value.as_bool()) == Some(true);
    let usage_guide = previous.get("usageGuideDismissed").and_then(Value::as_bool).unwrap_or(false)
        || pick("usageGuideDismissed").and_then(|value| value.as_bool()) == Some(true);
    let speed_style = pick("speedStyle").unwrap_or_else(|| previous.get("speedStyle").cloned().unwrap_or(Value::Null));
    let usage_scope = pick("usageScope").or_else(|| previous.get("usageScope").cloned());
    let prior_revision = previous.get("revision").and_then(Value::as_f64).unwrap_or(0.0);
    let revision = (crate::providers::now_ms() as f64).max(prior_revision + 1.0);

    let mut items = vec![
        ("schema", 1.0.into()),
        ("language", language),
        ("languageGuideDismissed", language_guide.into()),
        ("usageGuideDismissed", usage_guide.into()),
        ("speedStyle", speed_style),
        ("revision", revision.into()),
    ];
    if let Some(scope) = usage_scope {
        items.push(("usageScope", scope));
    }
    let document = obj(items);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = file.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temporary, format!("{}\n", to_string(&document))).map_err(|error| error.to_string())?;
    fs::rename(&temporary, file).map_err(|error| error.to_string())?;
    Ok(read(file))
}
