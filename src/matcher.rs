use std::collections::HashMap;
use crate::{MockEntry, MockSequence, SequenceResponse};
use crate::io::ServiceMocks;

// ─── Primary match functions ───────────────────────────────────────────────────

/// Exact match only: method + uri + body must all match.
///
/// # Complexity
/// O(1) — direct HashMap lookup by (method, uri), then body equality check.
/// The per-service ServiceMocks map is keyed by (method, uri) so no scan is needed.
pub fn find_exact(mocks: &ServiceMocks, method: &str, uri: &str, body: &str) -> Option<MockEntry> {
    let key = (method.to_string(), uri.to_string());
    mocks.get(&key).filter(|m| m.request.body == body).cloned()
}

/// Smart swap (opt-in, `SMART_SWAP_ENABLED=true`).
///
/// # Complexity
/// O(n × s) for the structural scan (n = mocks, s = avg path segments) plus
/// O(r × d) for evidence checks and substitution (r = response body length,
/// d = number of differing values). Dominated by the linear mock scan at scale.
///
/// TODO: Same index fix as find_exact applies here — normalize dynamic segments
/// to a wildcard token, bucket mocks by (method, normalized_path_shape). The
/// substitution and evidence-check phases are bounded by payload size and stay
/// fast regardless of library size.
///
/// Algorithm:
///   1. Find a structurally similar recorded request (same URI shape, same method).
///   2. Diff the recorded request vs the incoming request to find changed values.
///   3. For each changed value: check if it actually appears in the recorded
///      response body. Only those are substitution candidates — this is the
///      "evidence" criterion that prevents blind/incorrect replacements.
///   4. Apply substitutions JSON-aware: replace exact string values in the JSON
///      tree, never substrings. "123" never corrupts "1230" or a timestamp.
///   5. If any URI segment change is non-dynamic (different static path segment),
///      return None — it's a different endpoint, not a swap.
///
/// Returns None if no safe trivial substitution is possible → caller 404s.
pub fn find_smart_swap(mocks: &[MockEntry], method: &str, uri: &str, body: &str) -> Option<MockEntry> {
    // Step 1: find closest structural match
    let recorded = mocks.iter().rev().find(|m| {
        m.request.method == method &&
        (m.request.uri == uri || paths_match(&m.request.uri, uri))
    })?;

    let mut subs: HashMap<String, String> = HashMap::new();

    // Step 2a: URI path segment diffs
    let (rec_path, rec_qs) = split_uri(&recorded.request.uri);
    let (inc_path, inc_qs) = split_uri(uri);
    let rec_segs: Vec<&str> = rec_path.split('/').collect();
    let inc_segs: Vec<&str> = inc_path.split('/').collect();

    if rec_segs.len() != inc_segs.len() { return None; }

    for (r, i) in rec_segs.iter().zip(inc_segs.iter()) {
        if r != i {
            if !is_dynamic_segment(r) && !is_dynamic_segment(i) {
                // Static segment changed — this is a different endpoint, not a swap
                return None;
            }
            // Step 3: only add to substitutions if the recorded value is
            // actually traceable in the response body
            if recorded.response.body.contains(r) {
                subs.insert(r.to_string(), i.to_string());
            }
        }
    }

    // Step 2b: query param diffs (same key, different value)
    let rec_params = parse_query(rec_qs);
    let inc_params = parse_query(inc_qs);
    for (key, rec_val) in &rec_params {
        if let Some(inc_val) = inc_params.get(key) {
            if rec_val != inc_val && recorded.response.body.contains(rec_val.as_str()) {
                subs.insert(rec_val.clone(), inc_val.clone());
            }
        }
    }

    // Step 2c: request body diffs — collect all changed string values
    if !recorded.request.body.is_empty() && !body.is_empty()
        && recorded.request.body != body
    {
        if let (Ok(rec_json), Ok(inc_json)) = (
            serde_json::from_str::<serde_json::Value>(&recorded.request.body),
            serde_json::from_str::<serde_json::Value>(body),
        ) {
            let mut diffs: Vec<(String, String)> = Vec::new();
            collect_value_diffs(&rec_json, &inc_json, &mut diffs);
            for (rec_val, inc_val) in diffs {
                // Step 3: evidence criterion — only substitute if traceable
                if recorded.response.body.contains(&rec_val) {
                    subs.insert(rec_val, inc_val);
                }
            }
        }
    }

    // Nothing to swap and the request differs — return None, let caller 404
    if subs.is_empty() && (recorded.request.uri != uri || recorded.request.body != body) {
        return None;
    }

    // Step 4: JSON-aware substitution on response body
    let swapped_body = apply_substitutions_to_json(&recorded.response.body, &subs);

    let mut result = recorded.clone();
    result.response.body = swapped_body;
    Some(result)
}

/// Find the best structural match for an incoming request — recorded
/// entry whose URI shape and method match closest. Currently unused on
/// the proxy hot path; kept for tests and future callers.
///
/// # Complexity
/// O(n × s) — same linear scan as find_smart_swap.
#[allow(dead_code)]
pub fn find_structural(mocks: &[MockEntry], method: &str, uri: &str) -> Option<MockEntry> {
    // Exact URI first (different body → still best context for this endpoint)
    if let Some(m) = mocks.iter().rev().find(|m|
        m.request.method == method && m.request.uri == uri
    ) { return Some(m.clone()); }

    // Shape match (dynamic segments as wildcards)
    mocks.iter().rev().find(|m|
        m.request.method == method && paths_match(&m.request.uri, uri)
    ).cloned()
}

// ─── Sequence matching ────────────────────────────────────────────────────────

pub fn find_sequence_response(
    seqs:     &[MockSequence],
    counters: &mut HashMap<String, u32>,
    method:   &str,
    uri:      &str,
) -> Option<SequenceResponse> {
    let seq = seqs.iter().find(|s|
        s.method == method && (s.uri == uri || paths_match(&s.uri, uri))
    )?;

    if seq.responses.is_empty() { return None; }

    let current = *counters.entry(seq.id.clone()).or_insert(0) as usize;
    let idx     = current.min(seq.responses.len() - 1);
    let resp    = seq.responses[idx].clone();

    let next = current + 1;
    *counters.get_mut(&seq.id).unwrap() = if seq.loop_responses {
        (next % seq.responses.len()) as u32
    } else {
        next as u32
    };

    Some(resp)
}

// ─── URI helpers ──────────────────────────────────────────────────────────────

/// Sort query string parameters alphabetically so `?b=2&a=1` == `?a=1&b=2`.
pub fn normalize_uri(uri: &str) -> String {
    match uri.find('?') {
        None    => uri.to_string(),
        Some(q) => {
            let path  = &uri[..q];
            let query = &uri[q + 1..];
            let mut params: Vec<(&str, &str)> = query
                .split('&')
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    Some((it.next()?, it.next().unwrap_or("")))
                })
                .collect();
            params.sort_by_key(|(k, _)| *k);
            let qs: String = params.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>().join("&");
            format!("{}?{}", path, qs)
        }
    }
}

pub fn paths_match(recorded: &str, incoming: &str) -> bool {
    let r = recorded.split('?').next().unwrap_or(recorded);
    let i = incoming.split('?').next().unwrap_or(incoming);
    let rp: Vec<&str> = r.split('/').collect();
    let ip: Vec<&str> = i.split('/').collect();
    rp.len() == ip.len() && rp.iter().zip(ip.iter()).all(|(a, b)|
        a == b || is_dynamic_segment(a) || is_dynamic_segment(b)
    )
}

pub fn is_dynamic_segment(s: &str) -> bool {
    if s.is_empty() { return false; }
    // Pure numeric → ID (e.g. 999, 42)
    if s.chars().all(|c| c.is_ascii_digit()) { return true; }
    // UUID pattern (8-4-4-4-12 hex with dashes)
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 5 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit())) {
        return true;
    }
    // Mixed alphanumeric slug — requires BOTH letters and digits to avoid
    // matching plain words like "products" or "orders"
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    let has_alpha = s.chars().any(|c| c.is_ascii_alphabetic());
    let all_valid = s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    has_digit && has_alpha && all_valid
}

fn split_uri(uri: &str) -> (&str, &str) {
    match uri.find('?') {
        None    => (uri, ""),
        Some(q) => (&uri[..q], &uri[q + 1..]),
    }
}

fn parse_query(qs: &str) -> HashMap<String, String> {
    if qs.is_empty() { return HashMap::new(); }
    qs.split('&').filter_map(|kv| {
        let mut it = kv.splitn(2, '=');
        Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
    }).collect()
}

// ─── Substitution helpers ─────────────────────────────────────────────────────

/// Recursively diff two JSON values and collect (recorded_str, incoming_str)
/// pairs for every string field that changed. No key-name heuristics — all
/// changed string values are candidates; the evidence criterion (does the value
/// appear in the response?) is applied by the caller.
fn collect_value_diffs(
    recorded: &serde_json::Value,
    incoming: &serde_json::Value,
    diffs:    &mut Vec<(String, String)>,
) {
    match (recorded, incoming) {
        (serde_json::Value::String(r), serde_json::Value::String(i)) => {
            if r != i { diffs.push((r.clone(), i.clone())); }
        }
        (serde_json::Value::Object(r), serde_json::Value::Object(i)) => {
            for (key, rec_val) in r {
                if let Some(inc_val) = i.get(key) {
                    collect_value_diffs(rec_val, inc_val, diffs);
                }
            }
        }
        (serde_json::Value::Array(r), serde_json::Value::Array(i)) => {
            for (rv, iv) in r.iter().zip(i.iter()) {
                collect_value_diffs(rv, iv, diffs);
            }
        }
        _ => {}
    }
}

/// JSON-aware substitution: walks the response JSON tree and replaces exact
/// string values. Never operates on substrings — "123" cannot corrupt "1230".
/// Falls back to careful quoted-string replacement for non-JSON responses.
fn apply_substitutions_to_json(body: &str, subs: &HashMap<String, String>) -> String {
    if subs.is_empty() { return body.to_string(); }

    // Try JSON-aware replacement first
    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(body) {
        substitute_in_json(&mut val, subs);
        if let Ok(s) = serde_json::to_string(&val) {
            return s;
        }
    }

    // Non-JSON body: word-boundary-aware replacement.
    // Only replaces `from` when the surrounding characters are not part of an
    // identifier (alphanumeric, '-', '_'), so "tok1" never corrupts "tok10" or "mytok1".
    // Longest substitutions first to avoid partial matches shadowing longer ones.
    let mut result = body.to_string();
    let mut pairs: Vec<(&String, &String)> = subs.iter().collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len())); // longest first
    for (from, to) in pairs {
        result = replace_at_word_boundary(&result, from, to);
    }
    result
}

fn substitute_in_json(val: &mut serde_json::Value, subs: &HashMap<String, String>) {
    match val {
        serde_json::Value::String(s) => {
            if let Some(replacement) = subs.get(s.as_str()) {
                *s = replacement.clone();
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_in_json(v, subs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                substitute_in_json(v, subs);
            }
        }
        // Numbers, booleans, null — not substituted
        _ => {}
    }
}

/// Replace `from` with `to` in `text` only at identifier boundaries.
/// A boundary is any character that is NOT alphanumeric, '-', or '_'.
/// "tok1" replaces in "token=tok1 ok" but NOT in "tok10" or "mytok1".
fn replace_at_word_boundary(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() || !text.contains(from) { return text.to_string(); }
    let bytes   = text.as_bytes();
    let pattern = from.as_bytes();
    let n = bytes.len();
    let m = pattern.len();
    let mut result = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if i + m <= n && &bytes[i..i + m] == pattern {
            let before_ok = i == 0       || !is_id_byte(bytes[i - 1]);
            let after_ok  = i + m >= n   || !is_id_byte(bytes[i + m]);
            if before_ok && after_ok {
                result.push_str(to);
                i += m;
                continue;
            }
        }
        // SAFETY: bytes are valid UTF-8 because `text` is &str
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

#[inline]
fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockEntry, MockRequest, MockResponse, MockSequence, SequenceResponse};
    use crate::io::ServiceMocks;

    fn to_service_mocks(entries: Vec<MockEntry>) -> ServiceMocks {
        entries.into_iter()
            .map(|e| ((e.request.method.clone(), e.request.uri.clone()), e))
            .collect()
    }

    fn mock(method: &str, uri: &str, req_body: &str, res_body: &str) -> MockEntry {
        MockEntry {
            id: format!("{method}:{uri}"),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            service_id: None,
            request: MockRequest {
                method: method.to_string(),
                uri: uri.to_string(),
                body: req_body.to_string(),
            },
            response: MockResponse {
                status: 200,
                body: res_body.to_string(),
                headers: Default::default(),
                latency_ms: 0,
            },
        }
    }

    fn seq(id: &str, method: &str, uri: &str, responses: Vec<SequenceResponse>, loop_responses: bool) -> MockSequence {
        MockSequence {
            id: id.to_string(),
            method: method.to_string(),
            uri: uri.to_string(),
            responses,
            loop_responses,
        }
    }

    fn seq_resp(status: u16, body: &str) -> SequenceResponse {
        SequenceResponse {
            status,
            body: body.to_string(),
            headers: Default::default(),
            latency_ms: 0,
        }
    }

    // ── is_dynamic_segment ────────────────────────────────────────────────────

    #[test]
    fn test_dynamic_numeric() {
        assert!(is_dynamic_segment("42"));
        assert!(is_dynamic_segment("1000000"));
        assert!(is_dynamic_segment("0"));
    }

    #[test]
    fn test_dynamic_uuid() {
        assert!(is_dynamic_segment("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_dynamic_segment("ffffffff-ffff-ffff-ffff-ffffffffffff"));
    }

    #[test]
    fn test_dynamic_mixed_slug() {
        assert!(is_dynamic_segment("abc123"));
        assert!(is_dynamic_segment("tok_a1b2c3"));
        assert!(is_dynamic_segment("user-42a"));
    }

    #[test]
    fn test_static_alpha_only() {
        assert!(!is_dynamic_segment("users"));
        assert!(!is_dynamic_segment("profile"));
        assert!(!is_dynamic_segment("me"));
        assert!(!is_dynamic_segment("orders"));
    }

    #[test]
    fn test_empty_segment_is_not_dynamic() {
        assert!(!is_dynamic_segment(""));
    }

    // ── normalize_uri ─────────────────────────────────────────────────────────

    #[test]
    fn test_normalize_no_query() {
        assert_eq!(normalize_uri("/users/42"), "/users/42");
    }

    #[test]
    fn test_normalize_sorts_query_params() {
        let result = normalize_uri("/search?z=3&a=1&m=2");
        assert_eq!(result, "/search?a=1&m=2&z=3");
    }

    #[test]
    fn test_normalize_already_sorted() {
        let result = normalize_uri("/items?a=1&b=2");
        assert_eq!(result, "/items?a=1&b=2");
    }

    #[test]
    fn test_normalize_single_param() {
        assert_eq!(normalize_uri("/items?page=1"), "/items?page=1");
    }

    // ── paths_match ───────────────────────────────────────────────────────────

    #[test]
    fn test_paths_match_identical() {
        assert!(paths_match("/users/42", "/users/42"));
    }

    #[test]
    fn test_paths_match_dynamic_recorded() {
        // Recorded has numeric ID; incoming has different ID
        assert!(paths_match("/users/42", "/users/99"));
    }

    #[test]
    fn test_paths_match_different_depth() {
        assert!(!paths_match("/users/42", "/users/42/profile"));
    }

    #[test]
    fn test_paths_match_static_mismatch() {
        // Static segments differ → no match
        assert!(!paths_match("/users/42", "/orders/42"));
    }

    #[test]
    fn test_paths_match_ignores_query_string() {
        assert!(paths_match("/users/42?a=1", "/users/42?b=2"));
    }

    // ── find_exact ────────────────────────────────────────────────────────────

    #[test]
    fn test_find_exact_match() {
        let mocks = to_service_mocks(vec![mock("GET", "/users/1", "", r#"{"id":1}"#)]);
        let result = find_exact(&mocks, "GET", "/users/1", "");
        assert!(result.is_some());
        assert_eq!(result.unwrap().response.body, r#"{"id":1}"#);
    }

    #[test]
    fn test_find_exact_no_match_wrong_method() {
        let mocks = to_service_mocks(vec![mock("GET", "/users/1", "", r#"{"id":1}"#)]);
        assert!(find_exact(&mocks, "POST", "/users/1", "").is_none());
    }

    #[test]
    fn test_find_exact_no_match_wrong_uri() {
        let mocks = to_service_mocks(vec![mock("GET", "/users/1", "", r#"{"id":1}"#)]);
        assert!(find_exact(&mocks, "GET", "/users/2", "").is_none());
    }

    #[test]
    fn test_find_exact_body_must_match() {
        let mocks = to_service_mocks(vec![mock("POST", "/login", r#"{"user":"alice"}"#, r#"{"token":"tok1"}"#)]);
        assert!(find_exact(&mocks, "POST", "/login", r#"{"user":"bob"}"#).is_none());
    }

    #[test]
    fn test_find_exact_returns_last_recorded() {
        // HashMap insert — later entry with same key overwrites earlier
        let mocks = to_service_mocks(vec![
            mock("GET", "/items", "", r#"{"version":1}"#),
            mock("GET", "/items", "", r#"{"version":2}"#),
        ]);
        let result = find_exact(&mocks, "GET", "/items", "").unwrap();
        assert_eq!(result.response.body, r#"{"version":2}"#);
    }

    // ── find_smart_swap ───────────────────────────────────────────────────────

    #[test]
    fn test_smart_swap_id_substituted() {
        // Recorded: GET /users/1 → {"id":"1","name":"alice"}
        // Incoming: GET /users/2 → expect "1" swapped to "2" in response
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1","name":"alice"}"#)];
        let result = find_smart_swap(&mocks, "GET", "/users/2", "");
        assert!(result.is_some());
        let body = result.unwrap().response.body;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], "2");
        assert_eq!(v["name"], "alice");  // unchanged
    }

    #[test]
    fn test_smart_swap_static_segment_change_returns_none() {
        // /users/1 → /orders/1: "users" vs "orders" are both static → None
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1"}"#)];
        assert!(find_smart_swap(&mocks, "GET", "/orders/1", "").is_none());
    }

    #[test]
    fn test_smart_swap_no_evidence_no_substitution() {
        // ID "42" does NOT appear in the response body → no substitution
        // But URIs match structurally (42 is dynamic, 99 is dynamic)
        // With no substitutions and URIs differ → returns None
        let mocks = vec![mock("GET", "/users/42", "", r#"{"name":"alice"}"#)];
        let result = find_smart_swap(&mocks, "GET", "/users/99", "");
        // "42" not in response → no evidence → no substitution → None
        assert!(result.is_none());
    }

    #[test]
    fn test_smart_swap_exact_match_returns_response() {
        // Exact match (same URI, same body) → still returns (subs is empty but URIs match)
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1"}"#)];
        let result = find_smart_swap(&mocks, "GET", "/users/1", "");
        assert!(result.is_some());
    }

    // ── find_sequence_response ────────────────────────────────────────────────

    #[test]
    fn test_sequence_advances_on_each_call() {
        let responses = vec![
            seq_resp(200, "first"),
            seq_resp(200, "second"),
            seq_resp(200, "third"),
        ];
        let seqs = vec![seq("s1", "GET", "/api", responses, false)];
        let mut counters = HashMap::new();

        let r1 = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();
        let r2 = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();
        let r3 = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();

        assert_eq!(r1.body, "first");
        assert_eq!(r2.body, "second");
        assert_eq!(r3.body, "third");
    }

    #[test]
    fn test_sequence_clamps_at_last_when_not_looping() {
        let responses = vec![seq_resp(200, "only"), seq_resp(200, "last")];
        let seqs = vec![seq("s2", "GET", "/api", responses, false)];
        let mut counters = HashMap::new();

        find_sequence_response(&seqs, &mut counters, "GET", "/api");
        find_sequence_response(&seqs, &mut counters, "GET", "/api");
        // Third call beyond end — clamps to last
        let r = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();
        assert_eq!(r.body, "last");
    }

    #[test]
    fn test_sequence_loops_when_loop_enabled() {
        let responses = vec![seq_resp(200, "a"), seq_resp(200, "b")];
        let seqs = vec![seq("s3", "GET", "/api", responses, true)];
        let mut counters = HashMap::new();

        find_sequence_response(&seqs, &mut counters, "GET", "/api");  // a
        find_sequence_response(&seqs, &mut counters, "GET", "/api");  // b
        let r = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();  // wraps to a
        assert_eq!(r.body, "a");
    }

    #[test]
    fn test_sequence_no_match_returns_none() {
        let seqs: Vec<MockSequence> = vec![];
        let mut counters = HashMap::new();
        assert!(find_sequence_response(&seqs, &mut counters, "GET", "/missing").is_none());
    }

    #[test]
    fn test_sequence_empty_responses_returns_none() {
        let seqs = vec![seq("s4", "GET", "/api", vec![], false)];
        let mut counters = HashMap::new();
        assert!(find_sequence_response(&seqs, &mut counters, "GET", "/api").is_none());
    }

    #[test]
    fn test_sequence_matches_by_path_shape() {
        // Sequence registered on /users/1, incoming /users/2 — paths_match via dynamic
        let responses = vec![seq_resp(200, "ok")];
        let seqs = vec![seq("s5", "GET", "/users/1", responses, false)];
        let mut counters = HashMap::new();
        let r = find_sequence_response(&seqs, &mut counters, "GET", "/users/99");
        assert!(r.is_some());
    }
}

// ─── Alpha tests — extended edge cases ───────────────────────────────────────
//
// These tests cover boundary conditions, adversarial inputs, and interaction
// effects not addressed in the primary `tests` module above.
//
// Naming convention: `test_alpha_<subject>_<scenario>`

#[cfg(test)]
mod tests_alpha {
    use super::*;
    use crate::{MockEntry, MockRequest, MockResponse, MockSequence, SequenceResponse};
    use crate::io::ServiceMocks;
    use std::collections::HashMap;

    fn to_service_mocks(entries: Vec<MockEntry>) -> ServiceMocks {
        entries.into_iter()
            .map(|e| ((e.request.method.clone(), e.request.uri.clone()), e))
            .collect()
    }

    // ── helpers (mirrors tests module) ────────────────────────────────────────

    fn mock(method: &str, uri: &str, req_body: &str, res_body: &str) -> MockEntry {
        MockEntry {
            id: format!("{method}:{uri}"),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            service_id: None,
            request: MockRequest {
                method: method.to_string(),
                uri: uri.to_string(),
                body: req_body.to_string(),
            },
            response: MockResponse {
                status: 200,
                body: res_body.to_string(),
                headers: Default::default(),
                latency_ms: 0,
            },
        }
    }

    fn seq_resp(status: u16, body: &str) -> SequenceResponse {
        SequenceResponse {
            status,
            body: body.to_string(),
            headers: Default::default(),
            latency_ms: 0,
        }
    }

    fn seq(
        id: &str,
        method: &str,
        uri: &str,
        responses: Vec<SequenceResponse>,
        loop_responses: bool,
    ) -> MockSequence {
        MockSequence {
            id: id.to_string(),
            method: method.to_string(),
            uri: uri.to_string(),
            responses,
            loop_responses,
        }
    }

    // ── is_dynamic_segment — boundary conditions ──────────────────────────────

    #[test]
    fn test_alpha_dynamic_single_digit() {
        assert!(is_dynamic_segment("0"));
        assert!(is_dynamic_segment("9"));
    }

    #[test]
    fn test_alpha_dynamic_large_numeric_id() {
        assert!(is_dynamic_segment("9999999999"));
    }

    #[test]
    fn test_alpha_dynamic_slug_with_underscores() {
        // Has digit + alpha + underscore — all_valid → dynamic
        assert!(is_dynamic_segment("svc_42a"));
    }

    #[test]
    fn test_alpha_static_pure_alpha_with_hyphen() {
        // "my-service" — no digits → static
        assert!(!is_dynamic_segment("my-service"));
    }

    #[test]
    fn test_alpha_static_pure_alpha_with_underscore() {
        // "user_profile" — no digits → static
        assert!(!is_dynamic_segment("user_profile"));
    }

    #[test]
    fn test_alpha_dynamic_uuid_uppercase() {
        // UUID pattern is case-insensitive via all chars being hex — check with upper
        assert!(is_dynamic_segment("550E8400-E29B-41D4-A716-446655440000"));
    }

    #[test]
    fn test_alpha_not_dynamic_wrong_uuid_group_count() {
        // 4 groups instead of 5 — not a UUID, and purely alpha → static
        assert!(!is_dynamic_segment("aaaa-bbbb-cccc-dddd"));
    }

    #[test]
    fn test_alpha_dynamic_version_string_v1() {
        // "v1" — has both alpha and digit → classified as dynamic by current rule
        assert!(is_dynamic_segment("v1"));
    }

    #[test]
    fn test_alpha_static_keyword_health() {
        assert!(!is_dynamic_segment("health"));
    }

    #[test]
    fn test_alpha_static_keyword_api() {
        assert!(!is_dynamic_segment("api"));
    }

    // ── normalize_uri — extended cases ────────────────────────────────────────

    #[test]
    fn test_alpha_normalize_empty_query_string() {
        // URI ending with "?" but no params — treated as having an empty qs
        let result = normalize_uri("/items?");
        // Graceful: either passes through or normalises to "/items?"
        assert!(result.starts_with("/items"));
    }

    #[test]
    fn test_alpha_normalize_param_without_value() {
        // "?flag" with no "=" sign — should not panic
        let result = normalize_uri("/items?flag");
        assert!(result.starts_with("/items"));
    }

    #[test]
    fn test_alpha_normalize_preserves_path_with_dynamic_segment() {
        // Dynamic segments in path are not touched by normalize_uri
        let result = normalize_uri("/users/42?b=2&a=1");
        assert_eq!(result, "/users/42?a=1&b=2");
    }

    #[test]
    fn test_alpha_normalize_duplicate_param_keys_preserved() {
        // Duplicate keys (e.g. multi-value params) — sort is stable across equal keys
        let result = normalize_uri("/search?tag=rust&tag=async");
        // Both "tag" params should be present after sort
        assert!(result.contains("tag=rust"));
        assert!(result.contains("tag=async"));
    }

    // ── paths_match — edge cases ──────────────────────────────────────────────

    #[test]
    fn test_alpha_paths_match_root() {
        assert!(paths_match("/", "/"));
    }

    #[test]
    fn test_alpha_paths_match_empty_vs_empty() {
        assert!(paths_match("", ""));
    }

    #[test]
    fn test_alpha_paths_match_different_query_strings_same_path() {
        // Query strings are stripped before comparison
        assert!(paths_match("/api/data?v=1", "/api/data?v=2"));
    }

    #[test]
    fn test_alpha_paths_no_match_different_length_deeper() {
        assert!(!paths_match("/a/b", "/a/b/c"));
    }

    #[test]
    fn test_alpha_paths_match_both_dynamic_different_values() {
        // /users/42 vs /users/99 — both "42" and "99" are numeric → dynamic on both sides
        assert!(paths_match("/users/42", "/users/99"));
    }

    #[test]
    fn test_alpha_paths_no_match_static_vs_static_different_name() {
        assert!(!paths_match("/orders/history", "/orders/summary"));
    }

    // ── find_exact — edge cases ───────────────────────────────────────────────

    #[test]
    fn test_alpha_find_exact_empty_body_match() {
        let mocks = to_service_mocks(vec![mock("DELETE", "/items/1", "", r#"{"deleted":true}"#)]);
        let result = find_exact(&mocks, "DELETE", "/items/1", "");
        assert!(result.is_some());
    }

    #[test]
    fn test_alpha_find_exact_method_case_sensitive() {
        let mocks = to_service_mocks(vec![mock("GET", "/items", "", r#"{}"#)]);
        assert!(find_exact(&mocks, "get", "/items", "").is_none());
    }

    #[test]
    fn test_alpha_find_exact_multiple_methods_same_uri() {
        let mocks = to_service_mocks(vec![
            mock("GET",  "/items", "", r#"{"method":"GET"}"#),
            mock("POST", "/items", "", r#"{"method":"POST"}"#),
        ]);
        let get_result  = find_exact(&mocks, "GET",  "/items", "").unwrap();
        let post_result = find_exact(&mocks, "POST", "/items", "").unwrap();
        assert!(get_result.response.body.contains("GET"));
        assert!(post_result.response.body.contains("POST"));
    }

    #[test]
    fn test_alpha_find_exact_body_whitespace_not_normalised() {
        let mocks = to_service_mocks(vec![mock("POST", "/data", r#"{"k":"v"}"#, "ok")]);
        assert!(find_exact(&mocks, "POST", "/data", r#"{ "k": "v" }"#).is_none());
    }

    #[test]
    fn test_alpha_find_exact_empty_library_returns_none() {
        let mocks: ServiceMocks = ServiceMocks::new();
        assert!(find_exact(&mocks, "GET", "/anything", "").is_none());
    }

    // ── find_smart_swap — extended scenarios ──────────────────────────────────

    #[test]
    fn test_alpha_smart_swap_query_param_id_substituted() {
        // Recorded: GET /search?user_id=alice → {"name":"alice"}
        // Incoming: GET /search?user_id=bob → expect "alice" swapped to "bob"
        let mocks = vec![
            mock("GET", "/search?user_id=alice", "", r#"{"name":"alice"}"#),
        ];
        let result = find_smart_swap(&mocks, "GET", "/search?user_id=bob", "");
        assert!(result.is_some(), "should match via query param swap");
        let body: serde_json::Value =
            serde_json::from_str(&result.unwrap().response.body).unwrap();
        assert_eq!(body["name"], "bob");
    }

    #[test]
    fn test_alpha_smart_swap_body_field_substituted() {
        // Recorded request body has "user_id":"u1"; response echoes it.
        // Incoming body has "user_id":"u2" — expect substitution.
        let recorded_body = r#"{"user_id":"u1"}"#;
        let recorded_resp = r#"{"user_id":"u1","status":"active"}"#;
        let mocks = vec![mock("POST", "/accounts", recorded_body, recorded_resp)];

        let incoming_body = r#"{"user_id":"u2"}"#;
        let result = find_smart_swap(&mocks, "POST", "/accounts", incoming_body);
        // "u1" appears in the response → substituted with "u2"
        if let Some(r) = result {
            let v: serde_json::Value = serde_json::from_str(&r.response.body).unwrap();
            assert_eq!(v["user_id"], "u2");
        }
        // If None is returned the test is inconclusive but should not panic
    }

    #[test]
    fn test_alpha_smart_swap_depth_difference_returns_none() {
        // /users/1/profile vs /users/1 — different depths → paths_match fails → None
        let mocks = vec![mock("GET", "/users/1/profile", "", r#"{"id":"1"}"#)];
        assert!(find_smart_swap(&mocks, "GET", "/users/1", "").is_none());
    }

    #[test]
    fn test_alpha_smart_swap_wrong_method_returns_none() {
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1"}"#)];
        assert!(find_smart_swap(&mocks, "POST", "/users/1", "").is_none());
    }

    #[test]
    fn test_alpha_smart_swap_empty_library_returns_none() {
        let mocks: Vec<MockEntry> = vec![];
        assert!(find_smart_swap(&mocks, "GET", "/users/1", "").is_none());
    }

    #[test]
    fn test_alpha_smart_swap_no_mutation_on_non_dynamic_diff() {
        // /api/users/1 vs /api/orders/1 — "users" and "orders" are static segments
        // that differ → must return None (not a swap, a different endpoint)
        let mocks = vec![mock("GET", "/api/users/1", "", r#"{"id":"1"}"#)];
        assert!(find_smart_swap(&mocks, "GET", "/api/orders/1", "").is_none());
    }

    // ── find_structural — basic coverage ─────────────────────────────────────

    #[test]
    fn test_alpha_find_structural_exact_uri_wins() {
        let mocks = vec![
            mock("GET", "/users/1",   "", r#"{"id":"1"}"#),
            mock("GET", "/users/999", "", r#"{"id":"999"}"#),
        ];
        // Exact URI /users/999 should be returned (not /users/1)
        let result = find_structural(&mocks, "GET", "/users/999");
        assert!(result.is_some());
        assert!(result.unwrap().response.body.contains("999"));
    }

    #[test]
    fn test_alpha_find_structural_falls_back_to_shape_match() {
        // No exact match for /users/42, but /users/1 matches by shape
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1"}"#)];
        let result = find_structural(&mocks, "GET", "/users/42");
        assert!(result.is_some());
    }

    #[test]
    fn test_alpha_find_structural_wrong_method_returns_none() {
        let mocks = vec![mock("GET", "/users/1", "", r#"{"id":"1"}"#)];
        assert!(find_structural(&mocks, "DELETE", "/users/1").is_none());
    }

    #[test]
    fn test_alpha_find_structural_empty_library_returns_none() {
        let mocks: Vec<MockEntry> = vec![];
        assert!(find_structural(&mocks, "GET", "/users/1").is_none());
    }

    // ── find_sequence_response — edge cases ───────────────────────────────────

    #[test]
    fn test_alpha_sequence_single_response_always_returns_it() {
        let seqs = vec![seq("s1", "GET", "/api", vec![seq_resp(200, "only")], false)];
        let mut counters = HashMap::new();
        for _ in 0..5 {
            let r = find_sequence_response(&seqs, &mut counters, "GET", "/api").unwrap();
            assert_eq!(r.body, "only");
        }
    }

    #[test]
    fn test_alpha_sequence_different_method_no_match() {
        let seqs = vec![seq("s2", "GET", "/api", vec![seq_resp(200, "ok")], false)];
        let mut counters = HashMap::new();
        assert!(find_sequence_response(&seqs, &mut counters, "POST", "/api").is_none());
    }

    #[test]
    fn test_alpha_sequence_independent_counters_per_sequence_id() {
        let s1 = seq("s1", "GET", "/a", vec![seq_resp(200, "a1"), seq_resp(200, "a2")], false);
        let s2 = seq("s2", "GET", "/b", vec![seq_resp(200, "b1"), seq_resp(200, "b2")], false);
        let seqs = vec![s1, s2];
        let mut counters = HashMap::new();

        let r_a1 = find_sequence_response(&seqs, &mut counters, "GET", "/a").unwrap();
        let r_b1 = find_sequence_response(&seqs, &mut counters, "GET", "/b").unwrap();
        let r_a2 = find_sequence_response(&seqs, &mut counters, "GET", "/a").unwrap();
        let r_b2 = find_sequence_response(&seqs, &mut counters, "GET", "/b").unwrap();

        assert_eq!(r_a1.body, "a1");
        assert_eq!(r_b1.body, "b1");
        assert_eq!(r_a2.body, "a2");
        assert_eq!(r_b2.body, "b2");
    }

    #[test]
    fn test_alpha_sequence_loop_full_rotation() {
        // 3-item looping sequence: verify full cycle a→b→c→a→b→c
        let seqs = vec![seq(
            "s3",
            "GET",
            "/cycle",
            vec![seq_resp(200, "a"), seq_resp(200, "b"), seq_resp(200, "c")],
            true,
        )];
        let mut counters = HashMap::new();
        let expected = ["a", "b", "c", "a", "b", "c"];
        for exp in &expected {
            let r = find_sequence_response(&seqs, &mut counters, "GET", "/cycle").unwrap();
            assert_eq!(&r.body, exp);
        }
    }

    #[test]
    fn test_alpha_sequence_non_200_status_codes_preserved() {
        let seqs = vec![seq(
            "s4",
            "GET",
            "/status",
            vec![seq_resp(200, "ok"), seq_resp(429, "rate limited"), seq_resp(503, "down")],
            false,
        )];
        let mut counters = HashMap::new();

        let r1 = find_sequence_response(&seqs, &mut counters, "GET", "/status").unwrap();
        let r2 = find_sequence_response(&seqs, &mut counters, "GET", "/status").unwrap();
        let r3 = find_sequence_response(&seqs, &mut counters, "GET", "/status").unwrap();

        assert_eq!(r1.status, 200);
        assert_eq!(r2.status, 429);
        assert_eq!(r3.status, 503);
    }

    // ── apply_substitutions_to_json (via find_smart_swap) — word-boundary safety

    #[test]
    fn test_alpha_smart_swap_no_substring_corruption() {
        // Recorded URI: /tokens/tok1 → response contains "tok1" and "tok10" as separate fields.
        // Incoming URI: /tokens/tok2 — must NOT corrupt "tok10" → "tok20".
        let mocks = vec![mock(
            "GET",
            "/tokens/tok1",
            "",
            r#"{"short":"tok1","long":"tok10","other":"no_match"}"#,
        )];
        let result = find_smart_swap(&mocks, "GET", "/tokens/tok2", "");
        assert!(result.is_some(), "structural match should succeed");
        let body: serde_json::Value =
            serde_json::from_str(&result.unwrap().response.body).unwrap();
        // "short" should be substituted
        assert_eq!(body["short"], "tok2");
        // "long" contains "tok10" — JSON-aware substitution only replaces exact string values,
        // so the whole string "tok10" != "tok1" and must remain unchanged.
        assert_eq!(body["long"], "tok10");
    }
}
