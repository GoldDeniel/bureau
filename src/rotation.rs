//! Model rotation policy: given a failure and where an agent sits in its
//! fallback ladder, what should happen next?
//!
//! The rule that matters most is the one that refuses to act. A job that ran
//! for two hours and then died may have committed code; replaying that prompt
//! on a different model is not a retry, it is a second implementation. So only
//! *provider* failures rotate. Everything else that might have touched the repo
//! stops and asks a human.
//!
//! Kept pure so the policy can be tested without spawning agents or a clock.

use crate::failure::FailureKind;

/// What the dispatcher should do with a failed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Same model, immediately — the failure was a blip.
    RetrySame,
    /// Advance to the next usable rung of the ladder.
    Rotate,
    /// No ladder left. Hand it to a human with the tag attached.
    Fail,
    /// The attempt itself is ambiguous and may have side effects: stop, do not
    /// retry, do not rotate.
    Park,
}

/// Decide the next move.
///
/// `attempt` is 1-based: this is the number of the run that just failed.
/// `has_next` says whether a usable (not cooling down) rung exists after the
/// current one.
pub fn decide(kind: FailureKind, attempt: u32, has_next: bool) -> Action {
    match kind {
        // A blip: one immediate retry on the same rung, then treat it like the
        // infra failure it probably is.
        FailureKind::RateLimit | FailureKind::ProviderDown => {
            if attempt < 2 {
                Action::RetrySame
            } else if has_next {
                Action::Rotate
            } else {
                Action::Fail
            }
        }
        // Not a blip — the rung is out of credit or broken for the day.
        FailureKind::Quota | FailureKind::Auth | FailureKind::ContentSafety => {
            if has_next { Action::Rotate } else { Action::Fail }
        }
        // Two hours in, or the model answered badly. Either way the ticket is
        // in an unknown state, so nothing is retried automatically.
        FailureKind::Timeout | FailureKind::ModelOutput => Action::Park,
        // Tooling is local to the machine: another model fixes nothing.
        FailureKind::Tooling | FailureKind::Unknown => Action::Fail,
        // Tester's verdict is a manual decision, not a model failure.
        FailureKind::TesterVerdict => Action::Park,
    }
}

/// How long a rung stays out of rotation after failing this way. Zero means
/// "don't cool down" (the rung is still fine, or the failure was not its fault).
pub fn cooldown_secs(kind: FailureKind) -> i64 {
    match kind {
        // A free tier is usually done for the day; a paid one for a while.
        FailureKind::Quota => 6 * 60 * 60,
        FailureKind::Auth => 24 * 60 * 60,
        FailureKind::RateLimit => 60,
        FailureKind::ProviderDown => 5 * 60,
        FailureKind::ContentSafety => 30 * 60,
        _ => 0,
    }
}

/// The provider name for a model spec. Bureau stores `provider/model` (as the
/// CLIs do) but single-segment specs are common for local models, so an absent
/// provider is reported as `local` rather than guessed.
pub fn provider_of(model: &str) -> &str {
    match model.split_once('/') {
        Some((p, _)) if !p.is_empty() => p,
        _ => "local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::FailureKind as K;

    #[test]
    fn a_429_retries_once_then_rotates() {
        assert_eq!(decide(K::RateLimit, 1, true), Action::RetrySame);
        assert_eq!(decide(K::RateLimit, 2, true), Action::Rotate);
        // ...and fails cleanly when there is nowhere left to go.
        assert_eq!(decide(K::RateLimit, 2, false), Action::Fail);
    }

    #[test]
    fn a_dead_provider_retries_once_then_rotates() {
        assert_eq!(decide(K::ProviderDown, 1, true), Action::RetrySame);
        assert_eq!(decide(K::ProviderDown, 2, true), Action::Rotate);
    }

    #[test]
    fn exhausted_quota_rotates_immediately_without_burning_a_retry() {
        // Retrying a "free usage limit reached" just wastes a job slot.
        assert_eq!(decide(K::Quota, 1, true), Action::Rotate);
        assert_eq!(decide(K::Auth, 1, true), Action::Rotate);
        assert_eq!(decide(K::ContentSafety, 1, true), Action::Rotate);
    }

    #[test]
    fn a_two_hour_timeout_is_parked_not_replayed_elsewhere() {
        // This is the BUR-58 case: the agent had already committed before the
        // probe died. Rotating would re-run the work against a second model.
        assert_eq!(decide(K::Timeout, 1, true), Action::Park);
        assert_eq!(decide(K::Timeout, 3, true), Action::Park);
        assert_eq!(decide(K::ModelOutput, 1, true), Action::Park);
    }

    #[test]
    fn tooling_failures_do_not_rotate_because_a_new_model_has_the_same_shell() {
        assert_eq!(decide(K::Tooling, 1, true), Action::Fail);
        assert_eq!(decide(K::Unknown, 1, true), Action::Fail);
    }

    #[test]
    fn cooldowns_reflect_how_long_the_cause_actually_lasts() {
        assert!(cooldown_secs(K::Auth) >= cooldown_secs(K::Quota));
        assert!(cooldown_secs(K::Quota) > cooldown_secs(K::RateLimit));
        assert_eq!(cooldown_secs(K::Timeout), 0, "a timeout is not the model's fault");
        assert_eq!(cooldown_secs(K::ModelOutput), 0);
    }

    /// The end-to-end shape observed against a real dispatcher run: a 429 on
    /// the first rung, one retry, then rotation to the next rung.
    #[test]
    fn a_full_ladder_walk_matches_the_observed_dispatcher_trace() {
        let kinds = [K::RateLimit, K::RateLimit, K::RateLimit];
        let mut rung = 0usize;
        let mut attempt = 0u32;
        let mut walks = Vec::new();
        for k in kinds {
            attempt += 1;
            let has_next = rung + 1 < 2;
            match decide(k, attempt, has_next) {
                Action::RetrySame => walks.push((rung, "retry")),
                Action::Rotate => {
                    walks.push((rung, "rotate"));
                    rung += 1;
                    attempt = 0;
                }
                Action::Fail => walks.push((rung, "fail")),
                Action::Park => walks.push((rung, "park")),
            }
        }
        // Each rung gets its own single retry, including the last one — the
        // ladder resets the attempt counter on rotation. Only a failure *after*
        // that retry ends the walk, which the fourth entry proves.
        assert_eq!(walks, vec![(0, "retry"), (0, "rotate"), (1, "retry")]);
        assert_eq!(
            decide(K::RateLimit, 2, false),
            Action::Fail,
            "the final rung fails once its own retry is spent"
        );
    }

    #[test]
    fn provider_comes_from_the_model_spec_or_falls_back_to_local() {
        assert_eq!(provider_of("nvidia/minimaxai/minimax-m3.1-flash"), "nvidia");
        assert_eq!(provider_of("opencode/deepseek-v4.1-flash-free"), "opencode");
        assert_eq!(provider_of("qwen3.8:27b-ud-gguf"), "local");
        assert_eq!(provider_of(""), "local");
        assert_eq!(provider_of("/noprovider"), "local");
    }
}
