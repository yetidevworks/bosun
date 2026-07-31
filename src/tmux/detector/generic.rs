//! Generic activity-based detector. Last-resort priority.
//!
//! Heuristic: we don't know what tool is running, so we use the
//! session's `session_activity` age to guess.
//!   * < 2s since last activity → Running
//!   * 2s – 30s                 → Waiting (might be a prompt)
//!   * > 30s                    → Idle
//!
//! This is intentionally simple. Agent-specific detectors (claude,
//! codex) register at higher priority and will override it when
//! they're confident.

use std::time::Duration;

use super::{DetectContext, Status, StatusDetector};

pub struct GenericDetector;

impl StatusDetector for GenericDetector {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn priority(&self) -> u8 {
        10
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        if ctx.activity_age < Duration::from_secs(2) {
            Status::Running
        } else if ctx.activity_age < Duration::from_secs(30) {
            Status::Waiting
        } else {
            Status::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn ctx_with_age(secs: u64) -> DetectContext<'static> {
        let now = SystemTime::now();
        let ago = now - Duration::from_secs(secs);
        DetectContext::from_parts(b"", "", Some(ago), now, None, "s", None, None)
    }

    #[test]
    fn recent_activity_is_running() {
        assert_eq!(GenericDetector.detect(&ctx_with_age(0)), Status::Running);
        assert_eq!(GenericDetector.detect(&ctx_with_age(1)), Status::Running);
    }

    #[test]
    fn mid_range_is_waiting() {
        assert_eq!(GenericDetector.detect(&ctx_with_age(5)), Status::Waiting);
        assert_eq!(GenericDetector.detect(&ctx_with_age(20)), Status::Waiting);
    }

    #[test]
    fn old_activity_is_idle() {
        assert_eq!(GenericDetector.detect(&ctx_with_age(60)), Status::Idle);
        assert_eq!(GenericDetector.detect(&ctx_with_age(3600)), Status::Idle);
    }
}
