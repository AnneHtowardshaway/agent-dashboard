//! Codex provider: parses local rollout JSONL files written by Codex Desktop
//! and the Codex CLI. The files contain per-request usage records, so speed is
//! derived from the generation window between the previous tool boundary and
//! the moment the usage record is committed.

use super::*;
use crate::json::{parse, Value};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn session_roots() -> Vec<PathBuf> {
    let home = crate::paths::home();
    let mut roots = vec![home.join(".codex/sessions"), home.join(".codex/archived_sessions")];
    // Cockpit nests one more level: instances/codex/<instance>/sessions.
    // Missing that layer made a second Codex window show no tok/s at all.
    collect_session_dirs(&home.join(".antigravity_cockpit/instances"), 0, &mut roots);
    collect_session_dirs(&home.join(".cockpit_tools/instances"), 0, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

fn collect_session_dirs(directory: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
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
        if path.file_name().and_then(|value| value.to_str()) == Some("sessions") {
            out.push(path);
        } else {
            collect_session_dirs(&path, depth + 1, out);
        }
    }
}

pub fn collect(conversation: Option<&str>) -> HostSnapshot {
    let file = match find_rollout(conversation) {
        Some(file) => file,
        None => {
            return HostSnapshot::offline(
                "codex",
                if conversation.is_some() {
                    "No Codex rollout for this conversation yet"
                } else {
                    "No Codex rollout found; start a Codex session first"
                },
            )
        }
    };
    match parse_rollout(&file, conversation) {
        Ok((speed, groups, updated_at, quota_updated_at, _)) => HostSnapshot {
            host: "codex".to_string(),
            connection: "live".to_string(),
            error: None,
            speed: Some(speed),
            groups,
            updated_at,
            quota_updated_at,
            source: "Codex rollout JSONL".to_string(),
        },
        Err(error) => HostSnapshot::offline("codex", &error),
    }
}

fn find_rollout(conversation: Option<&str>) -> Option<PathBuf> {
    let mut best: Option<(i64, PathBuf)> = None;
    for root in session_roots() {
        walk(&root, 0, &mut |path| {
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                return;
            }
            let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
            if let Some(id) = conversation {
                if !name.to_ascii_lowercase().contains(&id.to_ascii_lowercase()) {
                    return;
                }
            }
            let modified = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0);
            if best.as_ref().map(|(best_time, _)| modified > *best_time).unwrap_or(true) {
                best = Some((modified, path.to_path_buf()));
            }
        });
    }
    best.map(|(_, path)| path)
}

fn walk(root: &Path, depth: usize, visit: &mut dyn FnMut(&Path)) {
    if depth > 6 {
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, depth + 1, visit);
        } else {
            visit(&path);
        }
    }
}

/// 当前会话的请求统计。总量覆盖全部记录，速度曲线只保留最近 16 个有效值。
#[derive(Default)]
struct RequestStats {
    requests: usize,
    measured: usize,
    missing: usize,
    unreliable: usize,
    input_tokens: f64,
    cached_tokens: f64,
    cache_write_tokens: f64,
    output_tokens: f64,
    sampled_tokens: f64,
    streaming_seconds: f64,
    model_seconds: f64,
    tool_seconds: f64,
    previous_ms: Option<i64>,
    latest_tps: Option<f64>,
    latest_status: &'static str,
    latest_short: bool,
    peak_tps: Option<f64>,
    recent_tps: VecDeque<f64>,
}

impl RequestStats {
    fn record(&mut self, usage: &Value, at_ms: i64, boundary_ms: i64) {
        let count = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let output = count("output_tokens");
        self.requests += 1;
        self.input_tokens += count("input_tokens");
        self.cached_tokens += count("cached_input_tokens");
        self.cache_write_tokens += count("cache_write_input_tokens");
        self.output_tokens += output;

        // 工具时间取相邻请求的非负间隔；生成时间还需要扣除中途的工具输出。
        let start = self.previous_ms.unwrap_or(0).max(boundary_ms);
        let seconds = ((at_ms - start) as f64 / 1000.0).max(0.0);
        if let Some(previous) = self.previous_ms.replace(at_ms) {
            self.tool_seconds += ((at_ms - previous) as f64 / 1000.0).max(0.0);
        }
        self.model_seconds += seconds;
        let (tps, status) = sample_rate(output, seconds);
        self.latest_tps = tps;
        self.latest_status = status;
        self.latest_short = status == "ok" && seconds < 1.0;
        if let Some(rate) = tps {
            self.measured += 1;
            self.sampled_tokens += output;
            self.streaming_seconds += seconds;
            self.peak_tps = Some(self.peak_tps.map_or(rate, |peak| peak.max(rate)));
            if self.recent_tps.len() == 16 {
                self.recent_tps.pop_front();
            }
            self.recent_tps.push_back((rate * 10.0).round() / 10.0);
        } else if status == "missing" {
            self.missing += 1;
        } else {
            self.unreliable += 1;
        }
    }
}

fn parse_rollout(
    file: &Path,
    conversation: Option<&str>,
) -> Result<(SpeedMetrics, Vec<QuotaGroup>, String, Option<String>, Option<String>), String> {
    let handle = File::open(file).map_err(|error| format!("Cannot open Codex rollout: {}", error))?;
    let mut reader = BufReader::new(handle);

    let mut session_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_activity: i64 = 0;
    let mut last_output_ts: i64 = 0;
    let mut stats = RequestStats::default();
    let mut steps = 0.0;
    let mut rounds = 0.0;
    let mut status = "IDLE".to_string();
    let mut context_window: Option<f64> = None;
    let mut last_context_used: Option<f64> = None;
    // Codex's own thread list reports the sum of token_count.last_token_usage.
    // Prefer it so the widget's totals match the host's native counter exactly.
    let mut native_input = 0.0;
    let mut native_cached = 0.0;
    let mut native_write = 0.0;
    let mut native_output = 0.0;
    let mut native_samples = 0.0;
    let mut rate_limits: Option<Value> = None;
    let mut title: Option<String> = None;
    let mut last_task_started = 0i64;

    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end();
        if !(line.contains("\"type\"") || line.contains("\"payload\"")) {
            continue;
        }
        let document = match parse(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let kind = document.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = document
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or(0);
        if ts > 0 {
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
            last_activity = last_activity.max(ts);
        }
        let payload = match document.get("payload") {
            Some(value) => value,
            None => continue,
        };
        match kind {
            "session_meta" => {
                session_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("session_id").and_then(Value::as_str))
                    .map(str::to_string);
            }
            "turn_context" => {
                if let Some(value) = payload.get("model").and_then(Value::as_str) {
                    model = Some(value.to_string());
                }
            }
            "token_usage_record" => {
                let usage = match payload.get("usage") {
                    Some(value) => value,
                    None => continue,
                };
                stats.record(usage, ts, last_output_ts.max(first_ts.unwrap_or(ts)));
            }
            "event_msg" => {
                let sub = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match sub {
                    "task_started" => {
                        status = "RUNNING".to_string();
                        last_task_started = ts;
                    }
                    "task_complete" | "turn_aborted" => {
                        status = "IDLE".to_string();
                    }
                    "token_count" => {
                        if let Some(info) = payload.get("info") {
                            if let Some(window) = info.get("model_context_window").and_then(Value::as_f64) {
                                context_window = Some(window);
                            }
                            if let Some(usage) = info.get("last_token_usage") {
                                let input = usage.get("input_tokens").and_then(Value::as_f64).unwrap_or(0.0);
                                let cached = usage.get("cached_input_tokens").and_then(Value::as_f64).unwrap_or(0.0);
                                let write = usage.get("cache_write_input_tokens").and_then(Value::as_f64).unwrap_or(0.0);
                                let output = usage.get("output_tokens").and_then(Value::as_f64).unwrap_or(0.0);
                                native_input += input;
                                native_cached += cached;
                                native_write += write;
                                native_output += output;
                                native_samples += 1.0;
                                if input + output > 0.0 {
                                    last_context_used = Some(input + output);
                                }
                            }
                        }
                        if let Some(limits) = payload.get("rate_limits") {
                            if limits.get("primary").map(|v| !v.is_null()).unwrap_or(false)
                                || limits.get("secondary").map(|v| !v.is_null()).unwrap_or(false)
                            {
                                rate_limits = Some(limits.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let sub = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match sub {
                    "message" => {
                        steps += 1.0;
                        if payload.get("role").and_then(Value::as_str) == Some("user") {
                            rounds += 1.0;
                            if title.is_none() {
                                title = user_title(payload);
                            }
                        }
                    }
                    "function_call" | "custom_tool_call" | "tool_search_call" | "reasoning" | "magpie_reasoning" => {
                        steps += 1.0;
                    }
                    "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                        last_output_ts = ts;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if stats.requests == 0 {
        return Err("Codex rollout has no usage records yet".to_string());
    }

    if status == "RUNNING" && now_ms() - last_activity > 10 * 60 * 1000 {
        status = "IDLE".to_string();
    }
    let _ = last_task_started;

    let (input_tokens, cached_tokens, cache_write_tokens, output_tokens) = if native_samples > 0.0 {
        (native_input, native_cached, native_write, native_output)
    } else {
        (stats.input_tokens, stats.cached_tokens, stats.cache_write_tokens, stats.output_tokens)
    };
    let uncached = (input_tokens - cached_tokens - cache_write_tokens).max(0.0);
    let requests = stats.requests as f64;

    let tokens = TokenBreakdown {
        input_tokens,
        cached_tokens,
        cache_write_tokens,
        uncached_tokens: uncached,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        measured_requests: requests,
        input_requests: requests,
        output_requests: requests,
        uncached_requests: requests,
        cached_requests: requests,
        cache_write_requests: requests,
        output_available: true,
        complete: true,
    };
    let cache = CacheMetrics {
        input_tokens,
        cached_tokens,
        cache_write_tokens,
        uncached_tokens: uncached,
        output_tokens,
        measured_requests: requests,
        hit_rate: if input_tokens > 0.0 { Some(cached_tokens / input_tokens) } else { None },
    };

    let context = ContextMetrics {
        used_tokens: last_context_used,
        max_tokens: context_window,
        model: model.clone(),
        source: Some("Codex token_count metadata".to_string()),
        estimated: context_window.is_none(),
    };

    let conversation_id = conversation
        .map(str::to_string)
        .or(session_id);

    let speed = SpeedMetrics {
        conversation_id,
        status: Some(status),
        steps,
        rounds,
        requests,
        measured_requests: stats.measured as f64,
        missing_timing: stats.missing as f64,
        excluded_timing: stats.unreliable as f64,
        rate_warnings: 0.0,
        tokens,
        cache,
        tps: (stats.streaming_seconds > 0.0).then(|| stats.sampled_tokens / stats.streaming_seconds),
        latest_tps: stats.latest_tps,
        peak_tps: stats.peak_tps,
        recent_tps: stats.recent_tps.into(),
        streaming_seconds: stats.streaming_seconds,
        model_seconds: stats.model_seconds,
        tool_seconds: stats.tool_seconds,
        ttft_seconds: None,
        last_active_at: if last_activity > 0 { Some(iso_from_ms(last_activity)) } else { None },
        latest_rate_status: stats.latest_status.to_string(),
        rate_basis: "response".to_string(),
        latest_short_sample: stats.latest_short,
        model,
        context,
        complete: true,
        estimated: false,
    };

    let groups = rate_limits.as_ref().map(quota_groups).unwrap_or_default();
    let updated = if last_activity > 0 { iso_from_ms(last_activity) } else { iso_now() };
    let quota_updated = rate_limits.as_ref().map(|_| updated.clone());
    Ok((speed, groups, updated, quota_updated, title))
}

/// Pick the first readable user prompt as the rollout title.
fn user_title(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_arr()?;
    for item in content {
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| item.get("input_text").and_then(Value::as_str))
            .unwrap_or("");
        let cleaned: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        // Codex prepends AGENTS.md context as a user message; skip it so the
        // title reflects the actual first prompt.
        if cleaned.is_empty() || cleaned.starts_with('<') || cleaned.starts_with("# AGENTS.md") {
            continue;
        }
        return Some(cleaned.chars().take(80).collect());
    }
    None
}

/// Recent rollouts for the multi-instance dashboard.
pub fn list(limit: usize) -> Vec<Value> {
    let mut files: Vec<(i64, PathBuf)> = Vec::new();
    for root in session_roots() {
        walk(&root, 0, &mut |path| {
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                return;
            }
            let modified = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0);
            files.push((modified, path.to_path_buf()));
        });
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut entries = Vec::new();
    for (_, file) in files.into_iter().take(limit) {
        if let Ok((speed, _, updated, _, title)) = parse_rollout(&file, None) {
            let conversation = speed.conversation_id.clone().unwrap_or_default();
            entries.push(obj(vec![
                ("id", conversation.into()),
                ("title", Value::str_or_null(title.as_deref())),
                ("model", Value::str_or_null(speed.model.as_deref())),
                ("updatedAt", updated.into()),
                ("latestTps", Value::num_or_null(speed.latest_tps.or(speed.tps))),
                ("tps", Value::num_or_null(speed.tps)),
                ("peakTps", Value::num_or_null(speed.peak_tps)),
                ("cacheHitRate", Value::num_or_null(speed.cache.hit_rate)),
                ("ctxUsedFraction", Value::num_or_null(speed.context.used_tokens.zip(speed.context.max_tokens).map(|(used, max)| if max > 0.0 { (used / max).clamp(0.0, 1.0) } else { 0.0 }))),
                ("estimated", speed.estimated.into()),
            ]));
        }
    }
    entries
}

fn quota_groups(limits: &Value) -> Vec<QuotaGroup> {
    let mut windows = Vec::new();
    for (key, label) in [("primary", "5h"), ("secondary", "weekly")] {
        let bucket = match limits.get(key) {
            Some(value) if !value.is_null() => value,
            _ => continue,
        };
        let used = bucket.get("used_percent").and_then(Value::as_f64);
        let reset = bucket
            .get("resets_at")
            .and_then(Value::as_f64)
            .map(|seconds| iso_from_ms((seconds * 1000.0) as i64));
        let remaining = used.map(|value| (1.0 - value / 100.0).clamp(0.0, 1.0));
        windows.push((
            label.to_string(),
            QuotaWindow { remaining, reset_at: reset, available: true, disabled: false, used_percent: used },
        ));
    }
    if windows.is_empty() {
        return Vec::new();
    }
    vec![QuotaGroup { id: "codex".to_string(), name: "Codex".to_string(), windows }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_curve_is_bounded_but_totals_and_peak_cover_all_requests() {
        let mut stats = RequestStats::default();
        for second in 1..=20 {
            let output = if second == 1 { 70.0 } else { second as f64 };
            let usage = obj(vec![("input_tokens", 10.0.into()), ("output_tokens", output.into())]);
            stats.record(&usage, second * 1000, 0);
        }
        let expected: VecDeque<f64> = (5..=20).map(f64::from).collect();
        assert_eq!(stats.recent_tps, expected);
        assert_eq!((stats.requests, stats.measured), (20, 20));
        assert_eq!((stats.input_tokens, stats.output_tokens, stats.sampled_tokens), (200.0, 279.0, 279.0));
        assert_eq!((stats.peak_tps, stats.latest_tps), (Some(70.0), Some(20.0)));
    }

    #[test]
    fn invalid_timing_updates_latest_without_displacing_a_valid_rate() {
        let mut stats = RequestStats::default();
        let output = obj(vec![("output_tokens", 12.0.into())]);
        stats.record(&output, 2000, 1750);
        assert!(stats.latest_short);
        stats.record(&Value::Null, 3000, 0);
        assert_eq!(stats.latest_status, "missing");
        stats.record(&output, 2500, 0);
        assert_eq!((stats.latest_tps, stats.latest_status, stats.latest_short), (None, "unreliable", false));
        assert_eq!((stats.measured, stats.missing, stats.unreliable), (1, 1, 1));
        assert_eq!((stats.model_seconds, stats.streaming_seconds, stats.tool_seconds), (1.25, 0.25, 1.0));
        assert_eq!((stats.output_tokens, stats.sampled_tokens), (24.0, 12.0));
        assert_eq!(stats.recent_tps, [48.0]);
    }
}
