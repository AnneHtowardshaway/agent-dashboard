//! Antigravity provider: reads the snapshots the existing sidecar writes under
//! ~/.gemini/antigravity/sidecar_data/antigravity-pulse/panel/data/history-v1.
//! No Antigravity process or credential is touched.

use super::*;
use crate::json::{parse, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub fn data_root() -> PathBuf {
    crate::paths::home().join(".gemini/antigravity/sidecar_data/antigravity-pulse/panel/data")
}

/// The plugin writes its snapshots to the HOME based sidecar directory even
/// when Cockpit Tools launches extra Antigravity / IDE instances (only the
/// Electron user-data dir is redirected). Scan cockpit roots as well so a
/// future per-instance data dir is still discovered.
pub fn history_roots() -> Vec<PathBuf> {
    static CACHE: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut roots = vec![data_root().join("history-v1")];
            let home = crate::paths::home();
            for base in [home.join(".antigravity_cockpit"), home.join(".cockpit_tools")] {
                find_sidecar_roots(&base, 3, &mut roots);
            }
            roots.sort();
            roots.dedup();
            roots
        })
        .clone()
}

fn find_sidecar_roots(directory: &Path, depth: i32, out: &mut Vec<PathBuf>) {
    if depth < 0 {
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
        if name == "sidecar_data" {
            let candidate = path.join("antigravity-pulse/panel/data/history-v1");
            if candidate.is_dir() {
                out.push(candidate);
            }
        } else if depth > 0 && !name.starts_with('.') && name != "node_modules" {
            find_sidecar_roots(&path, depth - 1, out);
        }
    }
}

pub fn collect(conversation: Option<&str>) -> HostSnapshot {
    let file = match find_snapshot(history_roots(), conversation) {
        Some(file) => file,
        None => {
            return HostSnapshot::offline(
                "agy",
                if conversation.is_some() {
                    "No Antigravity snapshot for this conversation yet"
                } else {
                    "No Antigravity snapshot found; open a conversation in Antigravity first"
                },
            )
        }
    };
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => return HostSnapshot::offline("agy", &format!("Cannot read Antigravity snapshot: {}", error)),
    };
    let document = match parse(&text) {
        Ok(value) => value,
        Err(error) => return HostSnapshot::offline("agy", &format!("Invalid Antigravity snapshot: {}", error)),
    };
    let metrics = match document.get("metrics") {
        Some(value) => value,
        None => return HostSnapshot::offline("agy", "Antigravity snapshot has no metrics"),
    };
    let saved_at = document.get("savedAt").and_then(Value::as_str).map(str::to_string);
    let age_ms = saved_at
        .as_deref()
        .and_then(parse_iso_ms)
        .map(|saved| now_ms() - saved)
        .unwrap_or(0);
    let connection = if age_ms > 30 * 60 * 1000 { "stale" } else { "live" };

    let speed = convert(metrics, conversation);
    HostSnapshot {
        host: "agy".to_string(),
        connection: connection.to_string(),
        error: None,
        speed: Some(speed),
        groups: Vec::new(),
        updated_at: saved_at.clone().unwrap_or_else(iso_now),
        quota_updated_at: None,
        source: "Antigravity request metadata".to_string(),
    }
}

/// Recent Antigravity snapshots for the multi-instance dashboard.
pub fn list(limit: usize) -> Vec<Value> {
    let mut files: Vec<(i64, PathBuf)> = Vec::new();
    for root in history_roots() {
        if let Ok(accounts) = fs::read_dir(&root) {
            for account in accounts.flatten() {
                let directory = account.path();
                if !directory.is_dir() {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(&directory) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|v| v.to_str()) != Some("json") {
                            continue;
                        }
                        let modified = entry
                            .metadata()
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_millis() as i64)
                            .unwrap_or(0);
                        files.push((modified, path));
                    }
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut entries = Vec::new();
    for (_, file) in files.into_iter().take(limit) {
        let Ok(text) = fs::read_to_string(&file) else { continue };
        let Ok(document) = parse(&text) else { continue };
        let metrics = match document.get("metrics") {
            Some(value) => value,
            None => continue,
        };
        let num = |key: &str| metrics.get(key).and_then(Value::as_f64);
        let context = metrics.get("context");
        let used_fraction = context.and_then(|value| value.get("usedFraction")).and_then(Value::as_f64);
        entries.push(obj(vec![
            ("id", Value::str_or_null(metrics.get("conversationId").and_then(Value::as_str))),
            ("title", Value::Null),
            ("model", Value::str_or_null(metrics.get("model").and_then(Value::as_str))),
            ("updatedAt", Value::str_or_null(document.get("savedAt").and_then(Value::as_str))),
            ("latestTps", Value::num_or_null(num("latestTps").or(num("tps")))),
            ("tps", Value::num_or_null(num("tps"))),
            ("peakTps", Value::num_or_null(num("peakTps"))),
            ("cacheHitRate", Value::num_or_null(metrics.get("cache").and_then(|value| value.get("hitRate")).and_then(Value::as_f64))),
            ("ctxUsedFraction", Value::num_or_null(used_fraction)),
            ("estimated", false.into()),
        ]));
    }
    entries
}

fn find_snapshot(roots: Vec<PathBuf>, conversation: Option<&str>) -> Option<PathBuf> {
    let mut best: Option<(i64, PathBuf)> = None;
    for root in roots {
        let accounts = match fs::read_dir(&root) {
            Ok(accounts) => accounts,
            Err(_) => continue,
        };
        for account in accounts.flatten() {
            let directory = account.path();
            if !directory.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) != Some("json") {
                    continue;
                }
                if let Some(id) = conversation {
                    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
                    if !stem.eq_ignore_ascii_case(id) {
                        continue;
                    }
                }
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                if best.as_ref().map(|(best_time, _)| modified > *best_time).unwrap_or(true) {
                    best = Some((modified, path));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

fn convert(metrics: &Value, conversation: Option<&str>) -> SpeedMetrics {
    let num = |key: &str| metrics.get(key).and_then(Value::as_f64);
    let text = |key: &str| metrics.get(key).and_then(Value::as_str).map(str::to_string);
    let tokens_json = metrics.get("tokens");
    let cache_json = metrics.get("cache");
    let context_json = metrics.get("context");

    let input_tokens = tokens_json.and_then(|v| v.get("inputTokens")).and_then(Value::as_f64).unwrap_or(0.0);
    let cached_tokens = tokens_json.and_then(|v| v.get("cachedTokens")).and_then(Value::as_f64).unwrap_or(0.0);
    let cache_write_tokens = tokens_json.and_then(|v| v.get("cacheWriteTokens")).and_then(Value::as_f64).unwrap_or(0.0);
    let uncached_tokens = tokens_json.and_then(|v| v.get("uncachedTokens")).and_then(Value::as_f64).unwrap_or(0.0);
    let output_tokens = tokens_json.and_then(|v| v.get("outputTokens")).and_then(Value::as_f64).unwrap_or(0.0);
    let total_tokens = tokens_json.and_then(|v| v.get("totalTokens")).and_then(Value::as_f64).unwrap_or(input_tokens + output_tokens);
    let measured = tokens_json.and_then(|v| v.get("measuredRequests")).and_then(Value::as_f64).unwrap_or(0.0);

    let tokens = TokenBreakdown {
        input_tokens,
        cached_tokens,
        cache_write_tokens,
        uncached_tokens,
        output_tokens,
        total_tokens,
        measured_requests: measured,
        input_requests: tokens_json.and_then(|v| v.get("inputRequests")).and_then(Value::as_f64).unwrap_or(measured),
        output_requests: tokens_json.and_then(|v| v.get("outputRequests")).and_then(Value::as_f64).unwrap_or(measured),
        uncached_requests: tokens_json.and_then(|v| v.get("uncachedRequests")).and_then(Value::as_f64).unwrap_or(measured),
        cached_requests: tokens_json.and_then(|v| v.get("cachedRequests")).and_then(Value::as_f64).unwrap_or(measured),
        cache_write_requests: tokens_json.and_then(|v| v.get("cacheWriteRequests")).and_then(Value::as_f64).unwrap_or(measured),
        output_available: tokens_json.and_then(|v| v.get("outputAvailable")).and_then(Value::as_bool).unwrap_or(true),
        complete: tokens_json.and_then(|v| v.get("complete")).and_then(Value::as_bool).unwrap_or(false),
    };
    let cache = CacheMetrics {
        input_tokens: cache_json.and_then(|v| v.get("inputTokens")).and_then(Value::as_f64).unwrap_or(input_tokens),
        cached_tokens: cache_json.and_then(|v| v.get("cachedTokens")).and_then(Value::as_f64).unwrap_or(cached_tokens),
        cache_write_tokens: cache_json.and_then(|v| v.get("cacheWriteTokens")).and_then(Value::as_f64).unwrap_or(cache_write_tokens),
        uncached_tokens: cache_json.and_then(|v| v.get("uncachedTokens")).and_then(Value::as_f64).unwrap_or(uncached_tokens),
        output_tokens: cache_json.and_then(|v| v.get("outputTokens")).and_then(Value::as_f64).unwrap_or(output_tokens),
        measured_requests: cache_json.and_then(|v| v.get("measuredRequests")).and_then(Value::as_f64).unwrap_or(measured),
        hit_rate: cache_json.and_then(|v| v.get("hitRate")).and_then(Value::as_f64),
    };
    let context = ContextMetrics {
        used_tokens: context_json.and_then(|v| v.get("usedTokens")).and_then(Value::as_f64),
        max_tokens: context_json.and_then(|v| v.get("maxTokens")).and_then(Value::as_f64),
        model: context_json.and_then(|v| v.get("model")).and_then(Value::as_str).map(str::to_string),
        source: context_json.and_then(|v| v.get("source")).and_then(Value::as_str).map(str::to_string),
        estimated: context_json.and_then(|v| v.get("estimated")).and_then(Value::as_bool).unwrap_or(true),
    };
    let recent = metrics
        .get("recentTps")
        .and_then(Value::as_arr)
        .map(|items| items.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
        .unwrap_or_default();
    SpeedMetrics {
        conversation_id: conversation
            .map(str::to_string)
            .or_else(|| text("conversationId")),
        status: text("status"),
        steps: num("steps").unwrap_or(0.0),
        rounds: num("rounds").unwrap_or(0.0),
        requests: num("requests").unwrap_or(0.0),
        measured_requests: num("measuredRequests").unwrap_or(0.0),
        missing_timing: num("missingTiming").unwrap_or(0.0),
        excluded_timing: num("excludedTiming").unwrap_or(0.0),
        rate_warnings: num("rateWarnings").unwrap_or(0.0),
        tokens,
        cache,
        tps: num("tps"),
        latest_tps: num("latestTps"),
        peak_tps: num("peakTps"),
        recent_tps: recent,
        streaming_seconds: num("streamingSeconds").unwrap_or(0.0),
        model_seconds: num("modelSeconds").unwrap_or(0.0),
        tool_seconds: num("toolSeconds").unwrap_or(0.0),
        ttft_seconds: num("ttftSeconds"),
        last_active_at: text("lastActiveAt"),
        latest_rate_status: text("latestRateStatus").unwrap_or_else(|| "missing".to_string()),
        rate_basis: text("rateBasis").unwrap_or_else(|| "response".to_string()),
        latest_short_sample: metrics.get("latestShortSample").and_then(Value::as_bool).unwrap_or(false),
        model: text("model"),
        context,
        complete: metrics.get("complete").and_then(Value::as_bool).unwrap_or(false),
        estimated: false,
    }
}
