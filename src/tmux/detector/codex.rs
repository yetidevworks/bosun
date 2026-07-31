//! Codex CLI detector.
//!
//! Same bottom-region strategy as the Claude detector: most signals
//! that distinguish "thinking" from "waiting" live near the prompt,
//! so scoping the substring scans to the trailing visible lines
//! keeps the older "Working…" lines that scrolled past from pinning
//! the glyph to Running. Whole-screen scans are kept only for anchor
//! detection (deciding "is this even a Codex pane").
//!
//! Stack, cheapest first:
//!   1. Pane-identity anchor (`Codex` / `OpenAI Codex` / `codex-cli`,
//!      or `codex` in the bottom region to avoid false positives
//!      from random shell history mentioning the word).
//!   2. Confirmation prompts in the bottom region → Waiting.
//!   3. Activity verbs in the bottom region OR braille spinner in
//!      the OSC title → Running.
//!   4. Recent `session_activity` as a final tie-breaker.

use super::{DetectContext, Status, StatusDetector};

const BOTTOM_REGION_LINES: usize = 12;

pub struct CodexDetector;

impl StatusDetector for CodexDetector {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn priority(&self) -> u8 {
        90
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_codex(ctx.plain, &bottom) {
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

fn looks_like_codex(plain: &str, bottom: &str) -> bool {
    // Strong anchors can appear anywhere in the capture (splash,
    // banner, version line) — those don't fade.
    if plain.contains("OpenAI Codex") || plain.contains("codex-cli") || plain.contains("Codex") {
        return true;
    }
    // The bare "codex" word is too generic to allow a whole-capture
    // match (someone's shell history could contain it). Require it
    // to appear in the live bottom region.
    bottom.contains("codex")
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
    // detector. Codex CLI also sets terminal titles while working.
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
    fn non_codex_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn codex_prompt_yields_waiting() {
        let ctx = ctx_plain("OpenAI Codex\n\nDo you want to proceed? (y/n)\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn codex_thinking_yields_running() {
        let ctx = ctx_plain("codex session\n· Thinking…\n");
        assert_eq!(CodexDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn codex_idle_when_settled() {
        let ctx = {
            let now = SystemTime::now();
            let ago = now - std::time::Duration::from_secs(60);
            DetectContext::from_parts(
                b"codex session done",
                "codex session done",
                Some(ago),
                now,
                None,
                "test",
                None,
                None,
            )
        };
        assert_eq!(CodexDetector.detect(&ctx), Status::Idle);
    }
}
