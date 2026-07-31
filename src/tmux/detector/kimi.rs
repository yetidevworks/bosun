//! Kimi Code detector (Moonshot `kimi`).
//!
//! Same bottom-region strategy as the Claude and Codex detectors:
//! the signals that separate "thinking" from "waiting" live near the
//! prompt, so the substring scans are scoped to the trailing visible
//! lines. That keeps older "Thinking…" lines that scrolled past from
//! pinning the glyph to Running. Whole-screen scans are kept only for
//! the pane-identity anchor.
//!
//! Stack, cheapest first:
//!   1. Pane-identity anchor (`Kimi` banner anywhere, or a bare
//!      `kimi` in the bottom region to avoid false positives from
//!      shell history that merely mentions the word).
//!   2. Confirmation prompts in the bottom region → Waiting.
//!   3. Activity verbs in the bottom region OR a braille spinner in
//!      the OSC title → Running.
//!   4. Recent `session_activity` as a final tie-breaker.

use super::{DetectContext, Status, StatusDetector};

const BOTTOM_REGION_LINES: usize = 12;

pub struct KimiDetector;

impl StatusDetector for KimiDetector {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn priority(&self) -> u8 {
        // Below Codex (90) and above the generic fallback (10). Kimi
        // and Codex anchors don't overlap, so relative order between
        // them doesn't matter — both return Unknown on the other's
        // panes.
        85
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_kimi(ctx.plain, &bottom) {
            return Status::Unknown;
        }

        if has_prompt_marker(&bottom) {
            return Status::Waiting;
        }

        if has_activity_marker(&bottom) || has_spinner_title(ctx.ansi) {
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

fn looks_like_kimi(plain: &str, bottom: &str) -> bool {
    // Strong anchors can appear anywhere in the capture (splash,
    // banner, version line) — those don't fade as the conversation
    // scrolls.
    if plain.contains("Kimi") || plain.contains("kimi-code") || plain.contains("Moonshot") {
        return true;
    }
    // The bare "kimi" word is too generic to allow a whole-capture
    // match (someone's shell history could contain it). Require it to
    // appear in the live bottom region.
    bottom.contains("kimi")
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
        "allow",
        "deny",
        "Deny",
    ];
    PROMPTS.iter().any(|p| region.contains(p))
}

fn has_activity_marker(region: &str) -> bool {
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

fn has_spinner_title(ansi: &[u8]) -> bool {
    // Reuse the same braille-spinner-in-OSC-title trick as the Claude
    // and Codex detectors. Many CLIs set a terminal title while busy.
    let s = String::from_utf8_lossy(ansi);
    let mut in_title = false;
    let mut title = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some(']') {
                for _ in 0..2 {
                    chars.next();
                }
                in_title = true;
                title.clear();
            }
        } else if in_title {
            if c == '\x07' || c == '\x1b' {
                if title
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
                {
                    return true;
                }
                in_title = false;
            } else {
                title.push(c);
            }
        }
    }
    false
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
    fn non_kimi_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(KimiDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn kimi_prompt_yields_waiting() {
        let ctx = ctx_plain("Kimi\n\nDo you want to proceed? (y/n)\n");
        assert_eq!(KimiDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn kimi_thinking_yields_running() {
        let ctx = ctx_plain("kimi session\n· Thinking…\n");
        assert_eq!(KimiDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn kimi_idle_when_settled() {
        let ctx = {
            let now = SystemTime::now();
            let ago = now - std::time::Duration::from_secs(60);
            DetectContext::from_parts(
                b"Kimi session done",
                "Kimi session done",
                Some(ago),
                now,
                None,
                "test",
                None,
                None,
            )
        };
        assert_eq!(KimiDetector.detect(&ctx), Status::Idle);
    }
}
