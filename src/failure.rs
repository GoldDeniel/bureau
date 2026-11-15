//! Failure classification: turn a failed agent run into a *kind*.
//!
//! Why this exists: an agent CLI writes its real error at the very end of its
//! output, after thousands of lines of narration. Bureau used to store that
//! whole blob as one string and run `contains()` over it, so a 429 buried
//! 14 KB deep could not be told apart from a genuine review verdict. Every
//! failure then looked the same on the board, and nothing could decide whether
//! a retry was even worth attempting.
//!
//! The detector looks at the *tail* first (where CLIs put their errors) and
//! falls back to the whole message, then to the exit code. It is deliberately
//! a pure function over strings so it can be tested without spawning anything.

use serde::{Deserialize, Serialize};

/// What went wrong. Stored per attempt, shown as a tag on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// 429 / "rate limit" — the same model may work again in a minute.
    RateLimit,
    /// Free-tier or plan limit exhausted — this provider is done for now.
    Quota,
    /// Bad or missing credential.
    Auth,
    /// A provider returned an error status (5xx, "overloaded", "unavailable").
    ProviderDown,
    /// Content filtering refused the request.
    ContentSafety,
    /// The job hit the wall-clock budget.
    Timeout,
    /// A tool the agent needed was missing or crashed (ripgrep, dotnet, git).
    Tooling,
    /// The model answered, but not with what the contract needed (bad JSON).
    ModelOutput,
    /// Nothing matched: a human has to look.
    Unknown,
}

impl FailureKind {
    /// Stable string used in the database and the HTTP API.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::RateLimit => "rate_limit",
            FailureKind::Quota => "quota",
            FailureKind::Auth => "auth",
            FailureKind::ProviderDown => "provider_down",
            FailureKind::ContentSafety => "content_safety",
            FailureKind::Timeout => "timeout",
            FailureKind::Tooling => "tooling",
            FailureKind::ModelOutput => "model_output",
            FailureKind::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> FailureKind {
        match s {
            "rate_limit" => FailureKind::RateLimit,
            "quota" => FailureKind::Quota,
            "auth" => FailureKind::Auth,
            "provider_down" => FailureKind::ProviderDown,
            "content_safety" => FailureKind::ContentSafety,
            "timeout" => FailureKind::Timeout,
            "tooling" => FailureKind::Tooling,
            "model_output" => FailureKind::ModelOutput,
            _ => FailureKind::Unknown,
        }
    }

    /// Can a *different* model plausibly succeed where this one failed?
    ///
    /// This is the whole point of the taxonomy: rotation is only safe when the
    /// failure is about the provider or the credential, never about the work
    /// already in flight. A timeout or a half-finished run must not be replayed
    /// on another model — the agent may have committed code before it died.
    pub fn is_infrastructure(self) -> bool {
        matches!(
            self,
            FailureKind::RateLimit
                | FailureKind::Quota
                | FailureKind::Auth
                | FailureKind::ProviderDown
                | FailureKind::ContentSafety
        )
    }

    /// Worth trying once more on the same model before rotating/failing.
    pub fn is_transient(self) -> bool {
        matches!(self, FailureKind::RateLimit | FailureKind::ProviderDown)
    }
}

/// A classified failure: the kind, plus the one line that justified it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Classified {
    pub kind: FailureKind,
    /// The matching line, trimmed — this is what the board shows first, so a
    /// 14 KB output never buries the reason again.
    pub detail: String,
}

/// How much of the tail to trust for error detection. CLIs write their final
/// error last; scanning the whole 256 KB mostly finds false positives in the
/// agent's own prose ("I hit a rate limit earlier but retried").
const TAIL: usize = 2000;

/// Patterns are checked in this order: the first match wins, so the most
/// specific signature must come before the general one. A quota message often
/// also contains "rate limit", and auth messages often mention providers.
fn patterns() -> &'static [(FailureKind, &'static [&'static str])] {
    &[
        (FailureKind::ContentSafety, &["content safety", "content filter", "safety screening"]),
        (
            FailureKind::Quota,
            &[
                "usage limit",
                "quota exceeded",
                "insufficient balance",
                "insufficient credit",
                "credits exhausted",
                "out of tokens",
                "free usage limit",
                // "credits" / "free tier" alone match agent prose and domain
                // code (a model field named Credits, docs about the free
                // tier). They must only count inside a real quota sentence.
                "no credits remaining",
                "tokens remaining",
            ],
        ),
        (
            FailureKind::Auth,
            &[
                "unauthorized",
                "invalid api key",
                "authentication",
                "incorrect api key",
                "401",
                "403",
            ],
        ),
        (
            FailureKind::RateLimit,
            &["429", "too many requests", "rate limit", "ratelimit"],
        ),
        (
            FailureKind::ProviderDown,
            &[
                "500",
                "502",
                "503",
                "504",
                "overloaded",
                "service unavailable",
                "connection refused",
                "socket connection was closed",
                "network error",
                "econnreset",
            ],
        ),
        (
            FailureKind::Tooling,
            &[
                "no such file",
                "command not found",
                "msbuild : error",
                "exit code 127",
                "record exceeded",
                "permission denied",
            ],
        ),
        (FailureKind::Timeout, &["timed out", "timeout"]),
        (
            FailureKind::ModelOutput,
            &[
                "not valid json",
                "could not produce a valid plan",
                "invalid json",
            ],
        ),
    ]
}

/// Classify a failure message.
///
/// `exit_code` is advisory: a non-zero exit with no recognisable text still
/// says "the process died", but not why, so it stays `Unknown` rather than
/// inventing a cause.
pub fn classify(message: &str, exit_code: Option<i32>) -> Classified {
    let tail = tail_of(message);
    for (kind, needles) in patterns() {
        if let Some(line) = first_matching_line(&tail, needles) {
            return Classified {
                kind: *kind,
                detail: line,
            };
        }
    }
    // The tail did not explain it; the agent may have echoed the error earlier.
    for (kind, needles) in patterns() {
        if let Some(line) = first_matching_line(message, needles) {
            return Classified {
                kind: *kind,
                detail: line,
            };
        }
    }
    let detail = match exit_code {
        Some(code) if code != 0 => format!("exited {code} with no recognisable error"),
        _ => "no recognisable error".to_string(),
    };
    Classified {
        kind: FailureKind::Unknown,
        detail,
    }
}

/// The last `TAIL` bytes of a message, cut on a char boundary. Error lines are
/// what matter, and they are at the end.
fn tail_of(s: &str) -> &str {
    if s.len() <= TAIL {
        return s;
    }
    let mut start = s.len() - TAIL;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// The first non-empty line containing one of `needles`, lowercased and
/// trimmed to something a person can read on a card. The *last* occurrence is
/// preferred when the tail contains the same error twice (CLI echo + summary).
fn first_matching_line(haystack: &str, needles: &[&str]) -> Option<String> {
    let mut found: Option<String> = None;
    for line in haystack.lines() {
        let lower = line.to_lowercase();
        if !needles.iter().any(|n| lower.contains(n)) {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        found = Some(clip(trimmed, 240));
    }
    found
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_429_buried_in_agent_chatter_is_found_in_the_tail() {
        // The real shape from BUR-47: pages of narration, then the error.
        let mut msg = String::from("worker tamis failed: 'opencode' exited 1:\n");
        for i in 0..400 {
            msg.push_str(&format!("line {i}: reading files and thinking\n"));
        }
        msg.push_str("Error: Too Many Requests: {\"status\":429,\"title\":\"Too Many Requests\"}");

        let c = classify(&msg, Some(1));
        assert_eq!(c.kind, FailureKind::RateLimit);
        assert!(c.detail.contains("429"), "detail was {:?}", c.detail);
        assert!(c.kind.is_infrastructure());
        assert!(c.kind.is_transient());
    }

    #[test]
    fn the_post_completion_socket_close_is_provider_down_not_a_verdict() {
        // BUR-58: work committed, tests green, then the probe's socket died.
        let msg = "worker tamis failed: 'opencode' exited 1: …\n\
                   Error: Cannot connect to API: The socket connection was closed unexpectedly.";
        let c = classify(msg, Some(1));
        assert_eq!(c.kind, FailureKind::ProviderDown);
        assert!(c.kind.is_infrastructure());
    }

    #[test]
    fn content_safety_wins_over_the_generic_unavailable() {
        let msg = "Error: content safety screening is temporarily unavailable";
        assert_eq!(classify(msg, Some(1)).kind, FailureKind::ContentSafety);
    }

    #[test]
    fn quota_beats_rate_limit_because_it_is_more_specific() {
        let msg = "Error: Free usage limit reached. rate limit: 0 remaining";
        assert_eq!(classify(msg, Some(1)).kind, FailureKind::Quota);
        assert!(classify(msg, Some(1)).kind.is_infrastructure());
    }

    #[test]
    fn domain_words_are_not_mistaken_for_a_quota_wall() {
        // Real false positive from BUR-45: a worker writing C# was killed
        // mid-edit, and the tail of its output contained a model field called
        // Credits. Reporting that as a quota wall hides the real cause.
        let code = r#"var current = new Subject { Code = "JELEN", Name = "Jelenlegi", Credits = 5 };"#;
        assert_ne!(classify(code, Some(1)).kind, FailureKind::Quota);
        // Docs prose must not trip it either.
        let prose = "The free tier is documented in docs/limits.md.";
        assert_ne!(classify(prose, Some(1)).kind, FailureKind::Quota);
    }

    #[test]
    fn a_timeout_is_never_rotated_to_another_model() {
        let c = classify("worker forge failed: adapter 'opencode' timed out after 7200s", None);
        assert_eq!(c.kind, FailureKind::Timeout);
        assert!(
            !c.kind.is_infrastructure(),
            "a 2h job that died may have committed code; replaying it elsewhere risks duplicate work"
        );
    }

    #[test]
    fn bad_manager_json_is_model_output_not_infrastructure() {
        let msg = "manager rector failed: could not produce a valid plan. Raw output: {nope";
        let c = classify(msg, Some(0));
        assert_eq!(c.kind, FailureKind::ModelOutput);
        assert!(!c.kind.is_infrastructure());
    }

    #[test]
    fn tooling_failures_are_recognised() {
        assert_eq!(
            classify("Error: Ripgrep JSON record exceeded 65536 bytes", Some(1)).kind,
            FailureKind::Tooling
        );
        assert_eq!(
            classify("MSBUILD : error MSB1011: Specify which project", Some(1)).kind,
            FailureKind::Tooling
        );
    }

    #[test]
    fn an_unrecognisable_exit_stays_unknown_instead_of_guessing() {
        let c = classify("worker w failed: 'x' exited 3: something odd happened", Some(3));
        assert_eq!(c.kind, FailureKind::Unknown);
        assert!(c.detail.contains("3"), "detail should mention the code");
    }

    #[test]
    fn long_detail_is_clipped_for_a_card() {
        let msg = format!("Error: 429 {}", "x".repeat(500));
        let c = classify(&msg, None);
        assert!(c.detail.chars().count() <= 241, "detail must stay card-sized");
    }

    #[test]
    fn tail_scanning_respects_utf8_boundaries() {
        // Greedy byte slicing on multibyte text must not panic.
        let msg = format!("{}Error: 429 too many requests", "ő".repeat(4000));
        assert_eq!(classify(&msg, None).kind, FailureKind::RateLimit);
    }

    #[test]
    fn round_trips_through_its_stored_string() {
        for k in [
            FailureKind::RateLimit,
            FailureKind::Quota,
            FailureKind::Auth,
            FailureKind::ProviderDown,
            FailureKind::ContentSafety,
            FailureKind::Timeout,
            FailureKind::Tooling,
            FailureKind::ModelOutput,
            FailureKind::Unknown,
        ] {
            assert_eq!(FailureKind::parse(k.as_str()), k);
        }
    }
}
