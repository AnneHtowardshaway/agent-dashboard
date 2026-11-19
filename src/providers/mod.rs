//! Shared metrics schema for every supported host.
//! Each provider converts its own on-disk format into this neutral shape so
//! the inline widget, CLI and web dashboard render identical numbers.

pub mod agy;
pub mod codex;

use crate::json::{obj, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static CACHE: Mutex<Option<HashMap<String, (Instant, HostSnapshot)>>> = Mutex::new(None);
static LIST_CACHE: Mutex<Option<HashMap<String, (Instant, Vec<Value>)>>> = Mutex::new(None);

/// Short-lived snapshot cache. The dashboard polls every couple of seconds and
/// re-parsing a large rollout for each poll is wasteful.
pub fn collect_snapshot(host: &str, conversation: Option<&str>) -> HostSnapshot {
    let key = format!("{}|{}", host, conversation.unwrap_or(""));
    if let Ok(guard) = CACHE.lock() {
        if let Some(map) = guard.as_ref() {
            if let Some((at, snapshot)) = map.get(&key) {
                if at.elapsed() < Duration::from_millis(1200) {
                    return snapshot.clone();
                }
            }
        }
    }
    let snapshot = match host {
        "agy" => agy::collect(conversation),
        "codex" => codex::collect(conversation),
        _ => HostSnapshot::offline(host, "Unknown host"),
    };
    if let Ok(mut guard) = CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if map.len() > 64 {
            map.clear();
        }
        map.insert(key, (Instant::now(), snapshot.clone()));
    }
    snapshot
}

/// Recent sessions per host. The dashboard uses this to show every open
/// window, so multi-instance hosts stay visible instead of collapsing to
/// whichever session was written last.
pub fn list_conversations(host: &str, limit: usize) -> Vec<Value> {
    let key = format!("list|{}", host);
    if let Ok(guard) = LIST_CACHE.lock() {
        if let Some(map) = guard.as_ref() {
            if let Some((at, entries)) = map.get(&key) {
                if at.elapsed() < Duration::from_secs(8) {
                    return entries.clone();
                }
            }
        }
    }
    let entries = match host {
        "agy" => agy::list(limit),
        "codex" => codex::list(limit),
        _ => Vec::new(),
    };
    if let Ok(mut guard) = LIST_CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if map.len() > 16 {
            map.clear();
        }
        map.insert(key, (Instant::now(), entries.clone()));
    }
    entries
}

/// Return freed allocator pages to the OS on macOS so a long-lived daemon
/// keeps a small resident set after parsing large session files.
pub fn release_memory() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TokenBreakdown {
    pub input_tokens: f64,
    pub cached_tokens: f64,
    pub cache_write_tokens: f64,
    pub uncached_tokens: f64,
    pub output_tokens: f64,
    pub total_tokens: f64,
    pub measured_requests: f64,
    pub input_requests: f64,
    pub output_requests: f64,
    pub uncached_requests: f64,
    pub cached_requests: f64,
    pub cache_write_requests: f64,
    pub output_available: bool,
    pub complete: bool,
}

impl TokenBreakdown {
    pub fn to_json(&self) -> Value {
        obj(vec![
            ("inputTokens", self.input_tokens.into()),
            ("cachedTokens", self.cached_tokens.into()),
            ("cacheWriteTokens", self.cache_write_tokens.into()),
            ("uncachedTokens", self.uncached_tokens.into()),
            ("outputTokens", self.output_tokens.into()),
            ("totalTokens", self.total_tokens.into()),
            ("measuredRequests", self.measured_requests.into()),
            ("inputRequests", self.input_requests.into()),
            ("outputRequests", self.output_requests.into()),
            ("missingInputRequests", 0.0.into()),
            ("missingOutputRequests", 0.0.into()),
            ("uncachedRequests", self.uncached_requests.into()),
            ("cachedRequests", self.cached_requests.into()),
            ("cacheWriteRequests", self.cache_write_requests.into()),
            ("outputAvailable", self.output_available.into()),
            ("coverageUnknown", false.into()),
            ("complete", self.complete.into()),
        ])
    }
}

#[derive(Clone, Debug, Default)]
pub struct CacheMetrics {
    pub input_tokens: f64,
    pub cached_tokens: f64,
    pub cache_write_tokens: f64,
    pub uncached_tokens: f64,
    pub output_tokens: f64,
    pub measured_requests: f64,
    pub hit_rate: Option<f64>,
}

impl CacheMetrics {
    pub fn to_json(&self) -> Value {
        obj(vec![
            ("inputTokens", self.input_tokens.into()),
            ("cachedTokens", self.cached_tokens.into()),
            ("cacheWriteTokens", self.cache_write_tokens.into()),
            ("uncachedTokens", self.uncached_tokens.into()),
            ("outputTokens", self.output_tokens.into()),
            ("measuredRequests", self.measured_requests.into()),
            ("missingRequests", 0.0.into()),
            ("totalTokens", (self.input_tokens + self.output_tokens).into()),
            ("hitRate", Value::num_or_null(self.hit_rate)),
            ("complete", true.into()),
        ])
    }
}

#[derive(Clone, Debug, Default)]
pub struct ContextMetrics {
    pub used_tokens: Option<f64>,
    pub max_tokens: Option<f64>,
    pub model: Option<String>,
    pub source: Option<String>,
    pub estimated: bool,
}

impl ContextMetrics {
    pub fn to_json(&self) -> Value {
        let used_fraction = match (self.used_tokens, self.max_tokens) {
            (Some(used), Some(max)) if max > 0.0 => Some((used / max).clamp(0.0, 1.0)),
            _ => None,
        };
        let remaining_tokens = match (self.used_tokens, self.max_tokens) {
            (Some(used), Some(max)) => Some((max - used).max(0.0)),
            _ => None,
        };
        obj(vec![
            ("usedTokens", Value::num_or_null(self.used_tokens)),
            ("maxTokens", Value::num_or_null(self.max_tokens)),
            ("remainingTokens", Value::num_or_null(remaining_tokens)),
            ("usedFraction", Value::num_or_null(used_fraction)),
            ("remainingFraction", Value::num_or_null(used_fraction.map(|v| 1.0 - v))),
            ("capacitySource", Value::str_or_null(self.source.as_deref())),
            ("providerMaxTokens", Value::Null),
            ("providerCapacityLabel", Value::Null),
            ("providerCapacitySource", Value::Null),
            ("providerCapacityKind", Value::Null),
            ("providerCapacityCheckedAt", Value::Null),
            ("compressionLimit", Value::Null),
            ("compressionFraction", Value::Null),
            ("compressionSource", Value::Null),
            ("model", Value::str_or_null(self.model.as_deref())),
            ("sampledAt", Value::Null),
            ("source", Value::str_or_null(self.source.as_deref())),
            ("estimated", self.estimated.into()),
        ])
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpeedMetrics {
    pub conversation_id: Option<String>,
    pub status: Option<String>,
    pub steps: f64,
    pub rounds: f64,
    pub requests: f64,
    pub measured_requests: f64,
    pub missing_timing: f64,
    pub excluded_timing: f64,
    pub rate_warnings: f64,
    pub tokens: TokenBreakdown,
    pub cache: CacheMetrics,
    pub tps: Option<f64>,
    pub latest_tps: Option<f64>,
    pub peak_tps: Option<f64>,
    pub recent_tps: Vec<f64>,
    pub streaming_seconds: f64,
    pub model_seconds: f64,
    pub tool_seconds: f64,
    pub ttft_seconds: Option<f64>,
    pub last_active_at: Option<String>,
    pub latest_rate_status: String,
    pub rate_basis: String,
    pub latest_short_sample: bool,
    pub model: Option<String>,
    pub context: ContextMetrics,
    pub complete: bool,
    pub estimated: bool,
}

impl SpeedMetrics {
    pub fn to_json(&self) -> Value {
        let counts = obj(vec![
            ("input", self.tokens.uncached_tokens.into()),
            ("output", self.tokens.output_tokens.into()),
            ("responseOutput", self.tokens.output_tokens.into()),
            ("thinkingOutput", 0.0.into()),
            ("cacheRead", self.tokens.cached_tokens.into()),
            ("cacheWrite", self.tokens.cache_write_tokens.into()),
        ]);
        obj(vec![
            ("conversationId", Value::str_or_null(self.conversation_id.as_deref())),
            ("status", Value::str_or_null(self.status.as_deref())),
            ("steps", self.steps.into()),
            ("rounds", self.rounds.into()),
            ("requests", self.requests.into()),
            ("measuredRequests", self.measured_requests.into()),
            ("missingTiming", self.missing_timing.into()),
            ("excludedTiming", self.excluded_timing.into()),
            ("rateWarnings", self.rate_warnings.into()),
            ("counts", counts),
            ("tokens", self.tokens.to_json()),
            ("cache", self.cache.to_json()),
            ("tps", Value::num_or_null(self.tps)),
            ("latestTps", Value::num_or_null(self.latest_tps)),
            ("peakTps", Value::num_or_null(self.peak_tps)),
            ("recentTps", Value::Arr(self.recent_tps.iter().map(|v| (*v).into()).collect())),
            ("streamingSeconds", self.streaming_seconds.into()),
            ("modelSeconds", self.model_seconds.into()),
            ("toolSeconds", self.tool_seconds.into()),
            ("ttftSeconds", Value::num_or_null(self.ttft_seconds)),
            ("lastActiveAt", Value::str_or_null(self.last_active_at.as_deref())),
            ("latestRateStatus", self.latest_rate_status.clone().into()),
            ("rateBasis", self.rate_basis.clone().into()),
            ("latestShortSample", self.latest_short_sample.into()),
            ("model", Value::str_or_null(self.model.as_deref())),
            ("context", self.context.to_json()),
            ("complete", self.complete.into()),
            ("estimated", self.estimated.into()),
        ])
    }
}

#[derive(Clone, Debug, Default)]
pub struct QuotaWindow {
    pub remaining: Option<f64>,
    pub reset_at: Option<String>,
    pub available: bool,
    pub disabled: bool,
    pub used_percent: Option<f64>,
}

impl QuotaWindow {
    pub fn to_json(&self) -> Value {
        obj(vec![
            ("remaining", Value::num_or_null(self.remaining)),
            ("resetAt", Value::str_or_null(self.reset_at.as_deref())),
            ("available", self.available.into()),
            ("disabled", self.disabled.into()),
            ("usedPercent", Value::num_or_null(self.used_percent)),
        ])
    }
}

#[derive(Clone, Debug, Default)]
pub struct QuotaGroup {
    pub id: String,
    pub name: String,
    pub windows: Vec<(String, QuotaWindow)>,
}

impl QuotaGroup {
    pub fn to_json(&self) -> Value {
        let windows = obj(self
            .windows
            .iter()
            .map(|(key, window)| (key.as_str(), window.to_json()))
            .collect());
        obj(vec![("id", self.id.clone().into()), ("name", self.name.clone().into()), ("windows", windows)])
    }
}

#[derive(Clone, Debug)]
pub struct HostSnapshot {
    pub host: String,
    pub connection: String,
    pub error: Option<String>,
    pub speed: Option<SpeedMetrics>,
    pub groups: Vec<QuotaGroup>,
    pub updated_at: String,
    pub quota_updated_at: Option<String>,
    pub source: String,
}

impl HostSnapshot {
    pub fn offline(host: &str, message: &str) -> HostSnapshot {
        HostSnapshot {
            host: host.to_string(),
            connection: "offline".to_string(),
            error: Some(message.to_string()),
            speed: None,
            groups: Vec::new(),
            updated_at: iso_now(),
            quota_updated_at: None,
            source: String::new(),
        }
    }

    pub fn to_json(&self) -> Value {
        obj(vec![
            ("host", self.host.clone().into()),
            ("connection", self.connection.clone().into()),
            ("error", Value::str_or_null(self.error.as_deref())),
            ("speed", self.speed.as_ref().map(|s| s.to_json()).unwrap_or(Value::Null)),
            ("groups", Value::Arr(self.groups.iter().map(|g| g.to_json()).collect())),
            ("updatedAt", self.updated_at.clone().into()),
            ("quotaUpdatedAt", Value::str_or_null(self.quota_updated_at.as_deref())),
            ("serverTime", iso_now().into()),
            ("source", self.source.clone().into()),
        ])
    }
}

/// Classify a generation window into a rate sample.
/// Very short or absurdly long windows cannot produce a trustworthy tok/s.
pub fn sample_rate(tokens: f64, seconds: f64) -> (Option<f64>, &'static str) {
    if !(tokens.is_finite() && tokens > 0.0) {
        return (None, "missing");
    }
    if !(seconds.is_finite() && seconds >= 0.2) || seconds > 900.0 {
        return (None, "unreliable");
    }
    let tps = tokens / seconds;
    if !tps.is_finite() || tps <= 0.0 {
        return (None, "unreliable");
    }
    (Some(tps), "ok")
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn iso_now() -> String {
    iso_from_ms(now_ms())
}

pub fn iso_from_ms(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = seconds.div_euclid(86400);
    let rem = seconds.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Parse the ISO-8601 timestamps used by Codex and Antigravity.
/// Fractional seconds and explicit offsets are both accepted.
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let number = |start: usize, end: usize| -> Option<i64> {
        std::str::from_utf8(bytes.get(start..end)?).ok()?.parse::<i64>().ok()
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let mut millis = 0i64;
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        let start = index + 1;
        let mut end = start;
        while matches!(bytes.get(end), Some(b'0'..=b'9')) {
            end += 1;
        }
        let digits = &text[start..end];
        let padded = format!("{:0<3}", digits);
        millis = padded.get(0..3)?.parse::<i64>().ok()?;
        index = end;
    }
    let mut offset_seconds = 0i64;
    match bytes.get(index) {
        Some(b'Z') | Some(b'z') | None => {}
        Some(b'+') | Some(b'-') => {
            let sign = if bytes[index] == b'-' { -1 } else { 1 };
            let oh = number(index + 1, index + 3)?;
            let om = number(index + 4, index + 6).unwrap_or(0);
            offset_seconds = sign * (oh * 3600 + om * 60);
        }
        _ => return None,
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86400 + hour * 3600 + minute * 60 + second - offset_seconds) * 1000 + millis)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let day_of_era = z - era * 146097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trip() {
        let ms = parse_iso_ms("2026-11-18T11:11:29.185Z").unwrap();
        assert_eq!(iso_from_ms(ms), "2026-11-18T11:11:29.185Z");
        let offset = parse_iso_ms("2026-11-18T19:11:29+08:00").unwrap();
        assert_eq!(iso_from_ms(offset), "2026-11-18T11:11:29.000Z");
    }

    #[test]
    fn rate_rejects_short_windows() {
        assert_eq!(sample_rate(100.0, 0.1).1, "unreliable");
        assert_eq!(sample_rate(100.0, 2.0).0, Some(50.0));
    }
}
