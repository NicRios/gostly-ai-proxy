//! Chaos injection — schema, glob matcher, weighted error draw, secure path scrub.
//!
//! Backward-compatible: the legacy flat fields (`latency_ms`, `error_status`, `error_body`)
//! still work. The new fields (`latency_jitter`, `error_codes`, `endpoint_rules`) take
//! precedence when set.
//!
//! Security: the chaos event log includes only the path component (no query string),
//! with UUID and long-numeric segments normalized to `{id}`. Headers, bodies, and
//! upstream URLs are never included.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-request injection model. "uniform" = legacy per-request random draw.
/// "markov" = two-state Markov (healthy ↔ degraded) with exponential dwells.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ChaosModel {
    #[default]
    Uniform,
    Markov,
}

/// Per-service chaos config. Recursively used by `EndpointRule.config` for per-endpoint
/// overrides. All fields default-construct so old flat configs still deserialize.
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ChaosConfig {
    #[serde(default)] pub enabled:        bool,

    // Legacy single-value fields — kept for backward compatibility.
    #[serde(default)] pub latency_ms:     u64,
    #[serde(default)] pub error_rate:     f64,
    #[serde(default)] pub error_status:   u16,
    #[serde(default)] pub error_body:     String,

    /// Latency jitter — overrides `latency_ms` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_jitter: Option<LatencyJitter>,

    /// Weighted error dictionary — overrides `error_status`/`error_body` when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_codes:    Vec<ErrorCodeEntry>,

    /// Per-endpoint rules. First matching rule wins. Empty = service-level config applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoint_rules: Vec<EndpointRule>,

    /// Which proxy modes chaos fires in. Empty = both. Values: "MOCK", "PASSTHROUGH".
    #[serde(default) ] pub modes:         Vec<String>,

    /// Display-only label for which preset (if any) populated this config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset:         Option<String>,

    /// Per-request injection model. "uniform" = legacy per-request random draw.
    /// "markov" = two-state Markov (healthy ↔ degraded) with exponential dwells.
    #[serde(default)]
    pub chaos_model:    ChaosModel,

    /// Markov-only knobs. Ignored when `chaos_model == Uniform`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markov:         Option<MarkovConfig>,
}

/// Markov chaos tuning. All defaults match the "realistic-outage" preset.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct MarkovConfig {
    /// Mean dwell in healthy state (ms). Larger = more time spent serving cleanly.
    #[serde(default = "default_dwell_healthy_ms")]
    pub mean_dwell_healthy_ms:   u64,
    /// Mean dwell in degraded state (ms). Larger = longer outages once they start.
    #[serde(default = "default_dwell_degraded_ms")]
    pub mean_dwell_degraded_ms:  u64,
    /// Error rate while in degraded state. Healthy state fires zero errors.
    #[serde(default = "default_degraded_error_rate")]
    pub degraded_error_rate:     f64,
    /// Latency multiplier applied to the chosen latency (jitter or flat) while degraded.
    #[serde(default = "default_degraded_latency_mult")]
    pub degraded_latency_mult:   f64,
}

impl Default for MarkovConfig {
    fn default() -> Self {
        Self {
            mean_dwell_healthy_ms:  default_dwell_healthy_ms(),
            mean_dwell_degraded_ms: default_dwell_degraded_ms(),
            degraded_error_rate:    default_degraded_error_rate(),
            degraded_latency_mult:  default_degraded_latency_mult(),
        }
    }
}

fn default_dwell_healthy_ms()      -> u64 { 30_000 }
fn default_dwell_degraded_ms()     -> u64 {  5_000 }
fn default_degraded_error_rate()   -> f64 { 0.6 }
fn default_degraded_latency_mult() -> f64 { 5.0 }

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LatencyJitter {
    pub min_ms:       u64,
    pub max_ms:       u64,
    /// "uniform" | "normal" — defaults to "uniform" if unrecognized.
    #[serde(default = "default_distribution")]
    pub distribution: String,
}

fn default_distribution() -> String { "uniform".to_string() }

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ErrorCodeEntry {
    pub status: u16,
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body:    Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct EndpointRule {
    /// Glob pattern — `*` matches one segment, `**` matches any depth.
    pub uri_pattern: String,
    pub config:      ChaosConfig,
}

// ─── Glob matcher ─────────────────────────────────────────────────────────────

/// Match `path` against a glob `pattern` where `*` = one path segment and `**` = any depth.
/// Both inputs are split on `/`. Empty pattern matches empty path only.
///
/// Examples:
///   "/api/orders/*"       matches "/api/orders/42" but not "/api/orders/42/items"
///   "/api/orders/**"      matches "/api/orders/42" AND "/api/orders/42/items"
///   "/api/*/items"        matches "/api/orders/items" but not "/api/orders/v2/items"
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p_segs: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let s_segs: Vec<&str> = path   .trim_matches('/').split('/').collect();
    glob_match_segs(&p_segs, &s_segs)
}

fn glob_match_segs(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None,    None)    => true,
        (None,    Some(_)) => false,
        (Some(p), None)    => *p == "**" && pattern.len() == 1,
        (Some(p), Some(s)) => {
            if *p == "**" {
                // ** matches zero or more segments. Try each split.
                if pattern.len() == 1 { return true; }   // trailing **
                for split in 0..=path.len() {
                    if glob_match_segs(&pattern[1..], &path[split..]) { return true; }
                }
                false
            } else if *p == "*" || *p == *s {
                glob_match_segs(&pattern[1..], &path[1..])
            } else {
                false
            }
        }
    }
}

/// First matching `EndpointRule` for the given `path_query` (path component only).
pub fn find_endpoint_rule<'a>(rules: &'a [EndpointRule], path_query: &str) -> Option<&'a ChaosConfig> {
    let path = path_query.split('?').next().unwrap_or(path_query);
    rules.iter().find(|r| glob_match(&r.uri_pattern, path)).map(|r| &r.config)
}

// ─── Latency draw ─────────────────────────────────────────────────────────────

/// Compute the latency to apply (in ms) for a given config. Returns 0 if no latency set.
pub fn compute_latency_ms(cc: &ChaosConfig) -> u64 {
    if let Some(j) = cc.latency_jitter.as_ref() {
        if j.max_ms == 0 { return 0; }
        let lo = j.min_ms.min(j.max_ms);
        let hi = j.max_ms.max(j.min_ms);
        let r  = match j.distribution.as_str() {
            "normal" => (rand::random::<f64>() + rand::random::<f64>()) / 2.0,
            _        => rand::random::<f64>(),
        };
        let span = (hi - lo) as f64;
        lo + (span * r).round() as u64
    } else {
        cc.latency_ms
    }
}

// ─── Error draw ───────────────────────────────────────────────────────────────

/// Drawn error: status, body bytes, optional headers. Used by the request handler to
/// build the chaos response without re-walking the config.
pub struct DrawnError {
    pub status:  u16,
    pub body:    String,
    pub headers: HashMap<String, String>,
}

/// Pick an error from the weighted dictionary; fall back to the legacy single error.
/// Returns `None` if no error is configured (neither `error_codes` nor `error_status`).
pub fn draw_error(cc: &ChaosConfig) -> Option<DrawnError> {
    if !cc.error_codes.is_empty() {
        let total: f64 = cc.error_codes.iter().map(|e| e.weight.max(0.0)).sum();
        if total > 0.0 {
            let r = rand::random::<f64>() * total;
            let mut acc = 0.0;
            for e in &cc.error_codes {
                acc += e.weight.max(0.0);
                if r <= acc {
                    return Some(DrawnError {
                        status:  e.status,
                        body:    e.body.clone().unwrap_or_default(),
                        headers: e.headers.clone().unwrap_or_default(),
                    });
                }
            }
            // Fall through if rounding leaves us short — use the last entry.
            if let Some(last) = cc.error_codes.last() {
                return Some(DrawnError {
                    status:  last.status,
                    body:    last.body.clone().unwrap_or_default(),
                    headers: last.headers.clone().unwrap_or_default(),
                });
            }
        }
    }
    if cc.error_status > 0 {
        return Some(DrawnError {
            status:  cc.error_status,
            body:    cc.error_body.clone(),
            headers: HashMap::new(),
        });
    }
    None
}

// ─── Path scrub for event log ─────────────────────────────────────────────────

/// Normalize `path_query` for the chaos event log:
///   1. Strip query string entirely
///   2. Replace UUIDs (`8-4-4-4-12` hex) with `{id}`
///   3. Replace numeric segments of 5+ digits with `{id}`
///
/// Never include: query string, headers, body, upstream URL, client IP.
pub fn scrub_path(path_query: &str) -> String {
    let path = path_query.split('?').next().unwrap_or(path_query);
    let mut out = String::with_capacity(path.len());
    if path.starts_with('/') {
        out.push('/');
    }
    let mut wrote_segment = false;
    for seg in path.split('/') {
        if seg.is_empty() { continue; }
        if wrote_segment { out.push('/'); }
        if is_uuid(seg) || is_long_numeric(seg) {
            out.push_str("{id}");
        } else {
            out.push_str(seg);
        }
        wrote_segment = true;
    }
    if out.is_empty() { out.push('/'); }
    out
}

fn is_uuid(s: &str) -> bool {
    if s.len() != 36 { return false; }
    let bytes = s.as_bytes();
    let dash_positions = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        let expect_dash = dash_positions.contains(&i);
        let is_dash = *b == b'-';
        if expect_dash != is_dash { return false; }
        if !is_dash && !b.is_ascii_hexdigit() { return false; }
    }
    true
}

fn is_long_numeric(s: &str) -> bool {
    s.len() >= 5 && s.bytes().all(|b| b.is_ascii_digit())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn glob_exact()           { assert!(glob_match("/api/orders", "/api/orders")); }
    #[test] fn glob_one_segment()     { assert!(glob_match("/api/orders/*", "/api/orders/42")); }
    #[test] fn glob_one_segment_no()  { assert!(!glob_match("/api/orders/*", "/api/orders/42/items")); }
    #[test] fn glob_deep()            { assert!(glob_match("/api/orders/**", "/api/orders/42/items")); }
    #[test] fn glob_deep_zero_segs()  { assert!(glob_match("/api/orders/**", "/api/orders")); }
    #[test] fn glob_middle_wildcard() { assert!(glob_match("/api/*/items", "/api/orders/items")); }
    #[test] fn glob_no_match()        { assert!(!glob_match("/api/orders", "/api/users")); }

    #[test] fn scrub_uuid() {
        assert_eq!(scrub_path("/api/orders/8f3a1b2c-1234-5678-9abc-def012345678"),
                   "/api/orders/{id}");
    }
    #[test] fn scrub_numeric() {
        assert_eq!(scrub_path("/api/orders/123456"), "/api/orders/{id}");
    }
    #[test] fn scrub_short_numeric_kept() {
        assert_eq!(scrub_path("/api/v2/orders"), "/api/v2/orders");
    }
    #[test] fn scrub_query_stripped() {
        assert_eq!(scrub_path("/api/orders?token=secret&user=42"), "/api/orders");
    }
    #[test] fn scrub_root() {
        assert_eq!(scrub_path("/"), "/");
    }

    #[test] fn scrub_no_double_leading_slash() {
        // Regression: path starting with '/' must not produce '//'.
        assert_eq!(scrub_path("/api/v2/orders"), "/api/v2/orders");
        assert!(!scrub_path("/api").starts_with("//"));
        assert!(!scrub_path("/").starts_with("//"));
    }
    #[test] fn scrub_relative_path() {
        // Path without leading '/' stays relative (no slash injected).
        assert_eq!(scrub_path("api/orders"), "api/orders");
    }
    #[test] fn scrub_empty() {
        assert_eq!(scrub_path(""), "/");
    }

    #[test] fn draw_error_weighted() {
        let cc = ChaosConfig {
            error_codes: vec![
                ErrorCodeEntry { status: 500, weight: 1.0, body: None, headers: None },
            ],
            ..Default::default()
        };
        let d = draw_error(&cc).expect("should draw");
        assert_eq!(d.status, 500);
    }

    #[test] fn draw_error_legacy_fallback() {
        let cc = ChaosConfig { error_status: 503, error_body: "down".into(), ..Default::default() };
        let d = draw_error(&cc).expect("should draw legacy");
        assert_eq!(d.status, 503);
        assert_eq!(d.body, "down");
    }

    #[test] fn draw_error_none() {
        let cc = ChaosConfig::default();
        assert!(draw_error(&cc).is_none());
    }

    #[test] fn latency_legacy() {
        let cc = ChaosConfig { latency_ms: 100, ..Default::default() };
        assert_eq!(compute_latency_ms(&cc), 100);
    }

    #[test] fn latency_jitter_in_range() {
        let cc = ChaosConfig {
            latency_jitter: Some(LatencyJitter { min_ms: 100, max_ms: 200, distribution: "uniform".into() }),
            ..Default::default()
        };
        for _ in 0..50 {
            let v = compute_latency_ms(&cc);
            assert!(v >= 100 && v <= 200, "got {v}");
        }
    }
}
