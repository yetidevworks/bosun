//! Qwen Code detector (`qwen`).
//!
//! Same bottom-region strategy as the Claude/Codex/Kimi detectors:
//! the signals that separate "thinking" from "waiting" live near the
//! prompt, so the substring scans are scoped to the trailing visible
//! lines. Whole-screen scans are kept only for the pane-identity
//! anchor.

use super::{DetectContext, Status, StatusDetector};

const BOTTOM_REGION_LINES: usize = 12;

pub struct QwenDetector;

impl StatusDetector for QwenDetector {
    fn name(&self) -> &'static str {
        "qwen"
    }

    fn priority(&self) -> u8 {
        // Below OpenCode (84) and above the generic fallback (10).
        // The agent anchors don't overlap, so relative order among
        // the agent detectors doesn't matter — each returns Unknown
        // on the others' panes.
        83
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_qwen(ctx.plain, &bottom) {
            return Status::Unknown;
        }

        if has_prompt_marker(&bottom) {
            return Status::Waiting;
        }

        if has_activity_marker(&bottom) || super::kimi::has_spinner_title(ctx.ansi) {
            return Status::Running;
        }

        if ctx.activity_age < std::time::Duration::from_secs(3) {
            return Status::Running;
        }

        Status::Idle
    }
}

fn bottom_region(plain: &str) -> String {
    let mut lines: Vec<&str> = plain
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(BOTTOM_REGION_LINES)
        .collect();
    lines.reverse();
    lines.join("\n")
}

fn looks_like_qwen(plain: &str, bottom: &str) -> bool {
    // Strong anchors can appear anywhere in the capture (banner,
    // model line like `qwen3-coder-plus`) — those don't fade as the
    // conversation scrolls.
    if plain.contains("Qwen") || plain.contains("qwen3-coder") || plain.contains("QWEN.md") {
        return true;
    }
    // The bare word is too generic for a whole-capture match (shell
    // history could mention it), so require it in the live bottom
    // region, where qwen's status bar shows the model name.
    bottom.contains("qwen")
}

fn has_prompt_marker(region: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
        "Yes, allow",
        "allow always",
        "Apply this change",
        "approve",
        "Approve",
        "Allow",
        "deny",
        "Deny",
    ];
    PROMPTS.iter().any(|p| region.contains(p))
}

fn has_activity_marker(region: &str) -> bool {
    // Qwen Code (a gemini-cli fork) shows "(esc to cancel" next to
    // its spinner while the model is working; its working verbs are
    // randomized, so that hint is the reliable busy signal.
    if region.contains("esc to cancel") {
        return true;
    }
    const MARKERS: &[&str] = &[
        "Thinking",
        "Working",
        "Running",
        "Executing",
        "Generating",
        "Applying",
        "Searching",
        "Reading",
        "Writing",
        "Reasoning",
    ];
    MARKERS
        .iter()
        .any(|v| region.contains(&format!("{v}…")) || region.contains(&format!("{v}...")))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::tmux::detector::DetectContext;

    fn ctx_plain(s: &str) -> DetectContext<'_> {
        let now = SystemTime::now();
        DetectContext::from_parts(s.as_bytes(), s, Some(now), now, None, "test", None, None)
    }

    #[test]
    fn non_qwen_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(QwenDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn qwen_prompt_yields_waiting() {
        let ctx = ctx_plain("Qwen Code\n\nApply this change? Yes, allow once\n");
        assert_eq!(QwenDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn qwen_busy_yields_running() {
        let ctx = ctx_plain("qwen3-coder-plus\n⠸ Levitating… (esc to cancel, 12s)\n");
        assert_eq!(QwenDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn qwen_idle_when_settled() {
        let ctx = {
            let now = SystemTime::now();
            let ago = now - std::time::Duration::from_secs(60);
            DetectContext::from_parts(
                b"Qwen session done",
                "Qwen session done",
                Some(ago),
                now,
                None,
                "test",
                None,
                None,
            )
        };
        assert_eq!(QwenDetector.detect(&ctx), Status::Idle);
    }
}
