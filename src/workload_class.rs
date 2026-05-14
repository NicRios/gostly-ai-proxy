//! Agent-traffic workload classification (Obs1).
//!
//! Tags every recorded request with one of four [`WorkloadClass`] values so
//! downstream consumers — analytics, fidelity-score sample sizing, billing,
//! the dashboard's traffic filter — can distinguish a human developer hitting
//! the proxy from a CI run, an automated AI coding agent, or genuinely
//! unknown traffic.
//!
//! # Why this exists
//!
//! Customers are already running automated agents (Claude Code, Cursor,
//! Copilot, etc.) against APIs through Gostly. Lumping all four populations
//! into one "request" bucket distorts every observability signal that depends
//! on it (most notably the fidelity score's "real users would have hit this"
//! denominator). This module is the single inference point — the classifier
//! is a pure function of the request headers + User-Agent, so the same
//! request always classifies the same way and the result is trivially
//! testable.
//!
//! # Vocabulary
//!
//! We adopt the OTEL `gen_ai` semantic-conventions vocabulary
//! (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>) so a future OTEL
//! exporter is a zero-translation passthrough — `workload_class` maps 1:1 to
//! the emerging `workload.kind` attribute.
//!
//! # Inference order (first match wins)
//!
//! 1. **CI** — known runner User-Agent or a CI-specific marker header.
//! 2. **Agent** — known AI-coding-tool User-Agent, an explicit
//!    `x-gostly-workload-class: agent` override, or a headless heuristic.
//! 3. **Human** — browser User-Agent signature without any CI/agent marker.
//! 4. **Unknown** — anything else (curl, wget, Python `requests`, …). This is
//!    deliberately reserved for "we genuinely don't know" traffic, not a
//!    catch-all default.
//!
//! Explicit overrides (`x-gostly-workload-class`) win over heuristics so a
//! customer can correctly tag traffic from a tool we haven't seen yet
//! without waiting on a release. The override is ONLY honoured for the
//! `agent` value today — it's the single direction where we want to give
//! customers a knob (the rest of the classes are inferred reliably from
//! standard headers).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::HeaderMap;

// ─── Public types ─────────────────────────────────────────────────────────────

/// One of four workload classes a request can fall into.
///
/// `Display` and `as_str()` produce the lower-case wire form used in JSONL
/// records, Prometheus labels, log fields, and the Postgres CHECK constraint —
/// keeping all five surfaces in lockstep with one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkloadClass {
    /// Real human developer — browser User-Agent signature, no CI/agent marker.
    Human,
    /// Continuous-integration runner — GitHub Actions, GitLab CI, CircleCI, …
    Ci,
    /// AI coding agent — Claude Code, Cursor, Copilot, OpenAI/Anthropic SDKs, …
    Agent,
    /// Genuinely unknown — curl, wget, Python `requests`, etc.
    Unknown,
}

impl WorkloadClass {
    /// Stable lower-case wire form. This is what we serialise into JSONL,
    /// emit as a Prometheus label, and match against in the Postgres CHECK
    /// constraint. Do not change without a migration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Human   => "human",
            Self::Ci      => "ci",
            Self::Agent   => "agent",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the wire form back into the enum, defaulting to `Unknown` on any
    /// unrecognised input. Tolerant by design — a JSONL line written by an
    /// older agent that lacks `workload_class` lands here as the empty
    /// string, which becomes [`WorkloadClass::Unknown`].
    ///
    /// Currently only exercised in tests; kept on the public surface so
    /// external code (the OSS sync tooling, future analytics) can call it.
    #[allow(dead_code)]
    pub fn from_str_or_unknown(raw: &str) -> Self {
        match raw {
            "human" => Self::Human,
            "ci"    => Self::Ci,
            "agent" => Self::Agent,
            _       => Self::Unknown,
        }
    }
}

impl std::fmt::Display for WorkloadClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Inference rule data ──────────────────────────────────────────────────────
//
// Each list is matched case-insensitively as a substring of the User-Agent.
// Keep these alphabetised to make diffs easy to review. Adding a new pattern
// is a one-line PR.

/// User-Agent substrings that uniquely identify a CI runner. Match is
/// case-insensitive on the full User-Agent string.
const CI_USER_AGENTS: &[&str] = &[
    "Bamboo",
    "BuildKite",   // historical capitalisation
    "Buildkite",
    "CircleCI",
    "Drone",
    "Bitrise",
    "GitHub-Hookshot",
    "GitLab-Runner",
    "Jenkins",
    "TeamCity",
    "Travis",
];

/// Marker headers that any CI provider injects. Presence of any of these is
/// sufficient to classify the request as `ci` regardless of User-Agent.
const CI_MARKER_HEADERS: &[&str] = &[
    "x-ci-build-id",
    "gitlab-ci-build-id",
    "github-actions-job",
    // env-injected `CI=true` showing through as a header (some runners
    // forward env vars as headers via gateway middleware).
    "x-ci",
];

/// User-Agent substrings that uniquely identify an AI coding agent or agent
/// SDK. Match is case-insensitive on the full User-Agent string.
const AGENT_USER_AGENTS: &[&str] = &[
    "aider",
    "anthropic-ai/sdk",
    "claude-code",
    "cline",
    "continue",
    "copilot-cli",
    "cursor",
    "github-copilot",
    "langchain/",
    "llamaindex/",
    "OpenAI/",
    "replit-agent",
    "roo-code",
    "windsurf",
];

/// Browser User-Agent signatures. Presence of any of these is necessary (but
/// not sufficient — a CI/agent marker still wins) to classify as `human`.
const HUMAN_BROWSER_SIGNATURES: &[&str] = &[
    "Chrome/",
    "Edge/",
    "Firefox/",
    "Mozilla/",
    "Safari/",
];

/// Header an operator can set explicitly to override the heuristic. Only the
/// `agent` value is honoured today — see module docs for the rationale.
const EXPLICIT_OVERRIDE_HEADER: &str = "x-gostly-workload-class";
const EXPLICIT_OVERRIDE_AGENT_VALUE: &str = "agent";

/// Header an agent runtime sets when the User-Agent itself is empty. Combined
/// with an empty UA this is treated as the headless-agent heuristic catch-all.
const HEADLESS_AGENT_RUNTIME_HEADER: &str = "x-runtime";
const HEADLESS_AGENT_RUNTIME_VALUE: &str = "nodejs";

// ─── Public inference function ────────────────────────────────────────────────

/// Classify a request by header + User-Agent. Pure function — no I/O, no
/// allocation beyond the case-folded UA copy. First-match-wins in the order
/// CI → Agent → Human → Unknown documented in the module header.
///
/// # Arguments
///
/// * `headers` — the inbound HTTP headers map, used for marker detection and
///   the explicit override.
/// * `ua` — the User-Agent string. Pass `""` if the request has no UA header.
pub fn infer_workload_class(headers: &HeaderMap, ua: &str) -> WorkloadClass {
    // Cheap once: case-fold the UA so substring matches don't have to do it
    // per-pattern. This avoids ~20 to_lowercase() calls in the hot path.
    let ua_lower = ua.to_lowercase();

    // ── 1. CI: marker header OR runner User-Agent ─────────────────────────────
    if has_any_header(headers, CI_MARKER_HEADERS) {
        return WorkloadClass::Ci;
    }
    if matches_any_substring(&ua_lower, CI_USER_AGENTS) {
        return WorkloadClass::Ci;
    }

    // ── 2. Agent: explicit override → known UA → headless heuristic ──────────
    if explicit_agent_override(headers) {
        return WorkloadClass::Agent;
    }
    if matches_any_substring(&ua_lower, AGENT_USER_AGENTS) {
        return WorkloadClass::Agent;
    }
    if ua.is_empty() && header_value_eq(
        headers,
        HEADLESS_AGENT_RUNTIME_HEADER,
        HEADLESS_AGENT_RUNTIME_VALUE,
    ) {
        return WorkloadClass::Agent;
    }

    // ── 3. Human: a browser UA, no CI/agent marker has fired by here ─────────
    if matches_any_substring(&ua_lower, HUMAN_BROWSER_SIGNATURES) {
        return WorkloadClass::Human;
    }

    // ── 4. Unknown: curl/wget/python-requests/anything without a marker ──────
    WorkloadClass::Unknown
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// True iff `headers` contains any of the names in `keys`. Comparison is
/// case-insensitive — Axum's HeaderMap already lower-cases the keys, but we
/// fold the needle anyway so the call site can pass mixed-case literals.
fn has_any_header(headers: &HeaderMap, keys: &[&str]) -> bool {
    keys.iter().any(|k| headers.contains_key(*k))
}

/// True iff any pattern in `needles` is a substring of the case-folded
/// haystack. `needles` is matched case-insensitively by lower-casing each
/// before comparison — keeps the constant arrays human-readable while still
/// matching real-world UAs that vary in case (e.g. `BuildKite/` vs
/// `Buildkite/`).
fn matches_any_substring(haystack_lower: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|n| haystack_lower.contains(&n.to_lowercase()))
}

/// True iff the `x-gostly-workload-class` header is set to `agent`. Other
/// values are intentionally ignored — see module docs.
fn explicit_agent_override(headers: &HeaderMap) -> bool {
    header_value_eq(headers, EXPLICIT_OVERRIDE_HEADER, EXPLICIT_OVERRIDE_AGENT_VALUE)
}

/// True iff the `name` header is present and its UTF-8 value equals
/// `value` case-insensitively. Returns false on missing header or non-UTF-8
/// value.
fn header_value_eq(headers: &HeaderMap, name: &str, value: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(value))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    // ── Helpers ────────────────────────────────────────────────────────────

    fn empty() -> HeaderMap {
        HeaderMap::new()
    }

    fn with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            // unwrap_used is gated to test code; the `from_*` calls below
            // are infallible for the literal strings we pass.
            #[allow(clippy::unwrap_used)]
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    // ── Display / round-trip ───────────────────────────────────────────────

    #[test]
    fn wire_form_round_trips_for_every_variant() {
        for c in [
            WorkloadClass::Human,
            WorkloadClass::Ci,
            WorkloadClass::Agent,
            WorkloadClass::Unknown,
        ] {
            assert_eq!(WorkloadClass::from_str_or_unknown(c.as_str()), c);
        }
    }

    #[test]
    fn unrecognised_wire_form_decodes_as_unknown() {
        assert_eq!(WorkloadClass::from_str_or_unknown(""),       WorkloadClass::Unknown);
        assert_eq!(WorkloadClass::from_str_or_unknown("HUMAN"),  WorkloadClass::Unknown);
        assert_eq!(WorkloadClass::from_str_or_unknown("robot"),  WorkloadClass::Unknown);
    }

    // ── CI variant ─────────────────────────────────────────────────────────

    #[test]
    fn ci_via_user_agent_github_hookshot() {
        let r = infer_workload_class(&empty(), "GitHub-Hookshot/abc123");
        assert_eq!(r, WorkloadClass::Ci);
    }

    #[test]
    fn ci_via_user_agent_circle_ci() {
        let r = infer_workload_class(&empty(), "CircleCI/2.7.1");
        assert_eq!(r, WorkloadClass::Ci);
    }

    #[test]
    fn ci_via_marker_header() {
        let h = with(&[("x-ci-build-id", "build-42"), ("user-agent", "curl/8.1")]);
        let r = infer_workload_class(&h, "curl/8.1");
        assert_eq!(r, WorkloadClass::Ci);
    }

    #[test]
    fn ci_marker_header_beats_browser_user_agent() {
        // A headless Chrome instance run as part of a CI pipeline still
        // classifies as `ci` — the marker header is the authoritative
        // signal, the UA is decorative.
        let h = with(&[("github-actions-job", "build")]);
        let r = infer_workload_class(
            &h,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0",
        );
        assert_eq!(r, WorkloadClass::Ci);
    }

    // ── Agent variant ──────────────────────────────────────────────────────

    #[test]
    fn agent_via_user_agent_claude_code() {
        let r = infer_workload_class(&empty(), "claude-code/2.5.1");
        assert_eq!(r, WorkloadClass::Agent);
    }

    #[test]
    fn agent_via_user_agent_cursor_case_insensitive() {
        let r = infer_workload_class(&empty(), "Cursor/0.42 (macOS)");
        assert_eq!(r, WorkloadClass::Agent);
    }

    #[test]
    fn agent_via_user_agent_anthropic_sdk() {
        let r = infer_workload_class(&empty(), "anthropic-ai/sdk 0.30.0");
        assert_eq!(r, WorkloadClass::Agent);
    }

    #[test]
    fn agent_via_explicit_override_header() {
        let h = with(&[
            ("user-agent", "curl/8.1"),
            ("x-gostly-workload-class", "agent"),
        ]);
        let r = infer_workload_class(&h, "curl/8.1");
        assert_eq!(r, WorkloadClass::Agent);
    }

    #[test]
    fn agent_via_headless_runtime_with_empty_user_agent() {
        let h = with(&[("x-runtime", "nodejs")]);
        let r = infer_workload_class(&h, "");
        assert_eq!(r, WorkloadClass::Agent);
    }

    // ── Human variant ──────────────────────────────────────────────────────

    #[test]
    fn human_via_chrome_signature() {
        let r = infer_workload_class(
            &empty(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        );
        assert_eq!(r, WorkloadClass::Human);
    }

    #[test]
    fn human_via_firefox_signature() {
        let r = infer_workload_class(
            &empty(),
            "Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/121.0",
        );
        assert_eq!(r, WorkloadClass::Human);
    }

    // ── Unknown variant ────────────────────────────────────────────────────

    #[test]
    fn unknown_for_curl() {
        let r = infer_workload_class(&empty(), "curl/8.1.2");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    #[test]
    fn unknown_for_python_requests() {
        let r = infer_workload_class(&empty(), "python-requests/2.31.0");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    #[test]
    fn unknown_for_no_user_agent_and_no_markers() {
        let r = infer_workload_class(&empty(), "");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    #[test]
    fn unknown_for_wget() {
        let r = infer_workload_class(&empty(), "Wget/1.21");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    // ── False-positive guards ──────────────────────────────────────────────
    //
    // These tests prove the classifier is robust to adversarial-looking
    // inputs: a CI marker header beats a browser UA (covered above), an
    // explicit override beats a curl UA, a browser UA without a marker
    // never lands as `agent` even if the path looks AI-related, and an
    // empty UA without the headless-runtime header is `unknown`, not
    // `agent`.

    #[test]
    fn explicit_override_beats_curl_user_agent() {
        // A customer wraps their AI tool with curl and explicitly tags it.
        // The override must win — it's the contract we promise.
        let h = with(&[
            ("user-agent", "curl/8.1"),
            ("x-gostly-workload-class", "agent"),
        ]);
        let r = infer_workload_class(&h, "curl/8.1");
        assert_eq!(r, WorkloadClass::Agent);
    }

    #[test]
    fn empty_ua_without_runtime_header_is_unknown_not_agent() {
        // The headless-agent heuristic only fires when *both* the UA is
        // empty and the runtime marker is present. Either alone is not
        // enough.
        let h = with(&[("content-type", "application/json")]);
        let r = infer_workload_class(&h, "");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    #[test]
    fn runtime_header_with_browser_ua_is_human_not_agent() {
        // A real browser running through a Node.js gateway shouldn't get
        // re-tagged as agent — the headless heuristic only fires when the
        // UA is *empty*. A populated browser UA still wins as human.
        let h = with(&[("x-runtime", "nodejs")]);
        let r = infer_workload_class(
            &h,
            "Mozilla/5.0 (Macintosh) AppleWebKit/537.36 Chrome/120.0",
        );
        assert_eq!(r, WorkloadClass::Human);
    }

    #[test]
    fn explicit_override_with_unknown_value_does_not_misclassify() {
        // `x-gostly-workload-class: human` is NOT a supported override —
        // the classifier ignores anything except the literal `agent`
        // value. A curl UA with a bogus override stays `unknown`.
        let h = with(&[
            ("user-agent", "curl/8.1"),
            ("x-gostly-workload-class", "human"),
        ]);
        let r = infer_workload_class(&h, "curl/8.1");
        assert_eq!(r, WorkloadClass::Unknown);
    }

    #[test]
    fn ci_marker_beats_agent_user_agent_first_match_wins() {
        // A claude-code agent run from inside a CI pipeline: the spec's
        // first-match-wins rule says CI takes precedence. (Future work:
        // a richer "primary class" + "secondary class" might surface
        // both, but v1 commits to a single label.)
        let h = with(&[("x-ci-build-id", "ci-99")]);
        let r = infer_workload_class(&h, "claude-code/2.5.1");
        assert_eq!(r, WorkloadClass::Ci);
    }

    #[test]
    fn jenkins_user_agent_classifies_as_ci_even_with_browser_substring() {
        // Some Jenkins User-Agents include a browser fragment (e.g.
        // a plugin that proxies through a headless Chromium). CI must
        // still win because the runner pattern is checked first.
        let r = infer_workload_class(
            &empty(),
            "Jenkins/2.426 Mozilla/5.0 Chrome/120.0",
        );
        assert_eq!(r, WorkloadClass::Ci);
    }

    // ── Property: zero-info request is always Unknown ──────────────────────

    #[test]
    fn property_no_headers_no_ua_is_always_unknown() {
        // Spec acceptance evidence #4. Promote to a property test if/when
        // we add a quickcheck-style harness — for now the assertion is
        // strong enough.
        for _ in 0..16 {
            assert_eq!(
                infer_workload_class(&empty(), ""),
                WorkloadClass::Unknown,
            );
        }
    }
}
