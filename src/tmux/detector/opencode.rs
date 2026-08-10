//! OpenCode detector (sst `opencode`).
//!
//! Same bottom-region strategy as the Claude/Codex/Kimi detectors:
//! the signals that separate "thinking" from "waiting" live near the
//! prompt, so the substring scans are scoped to the trailing visible
//! lines. Whole-screen scans are kept only for the pane-identity
//! anchor.

use super::{DetectContext, Status, StatusDetector};

const BOTTOM_REGION_LINES: usize = 12;

pub struct OpencodeDetector;

impl StatusDetector for OpencodeDetector {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn priority(&self) -> u8 {
        // Below Kimi (85) and above the generic fallback (10). The
        // agent anchors don't overlap, so relative order among the
        // agent detectors doesn't matter — each returns Unknown on
        // the others' panes.
        84
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_opencode(ctx.plain, &bottom) {
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

fn looks_like_opencode(plain: &str, bottom: &str) -> bool {
    // Strong anchors can appear anywhere in the capture (the block
    // banner, version line) — those don't fade as the conversation
    // scrolls.
    if plain.contains("OpenCode") || plain.contains("opencode v") {
        return true;
    }
    // The bare word is too generic for a whole-capture match (shell
    // history could mention it — it's also a bosun theme name), so
    // require it in the live bottom region, where opencode's status
    // bar keeps its branding.
    bottom.contains("opencode")
}

fn has_prompt_marker(region: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
        "approve",
        "Approve",
        "Allow",
        "allow once",
        "deny",
        "Deny",
        "Reject",
    ];
    PROMPTS.iter().any(|p| region.contains(p))
}

fn has_activity_marker(region: &str) -> bool {
    // opencode shows "esc interrupt" in its status area while the
    // agent is busy; the verb list covers tool-call status lines.
    if region.contains("esc interrupt") || region.contains("esc to interrupt") {
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
    fn non_opencode_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(OpencodeDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn opencode_prompt_yields_waiting() {
        let ctx = ctx_plain("OpenCode\n\nDo you want to run this command? (y/n)\n");
        assert_eq!(OpencodeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn opencode_busy_yields_running() {
        let ctx = ctx_plain("opencode v1.18\nWorking…  esc interrupt\n");
        assert_eq!(OpencodeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn opencode_idle_when_settled() {
        let ctx = {
            let now = SystemTime::now();
            let ago = now - std::time::Duration::from_secs(60);
            DetectContext::from_parts(
                b"OpenCode session done",
                "OpenCode session done",
                Some(ago),
                now,
                None,
                "test",
                None,
                None,
            )
        };
        assert_eq!(OpencodeDetector.detect(&ctx), Status::Idle);
    }
}
