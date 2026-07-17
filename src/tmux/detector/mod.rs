//! Pluggable status detection.
//!
//! The whole point of this module is to avoid the agent-deck trap of
//! hardcoding Claude-specific detection into the hot path. Detectors
//! are pure functions of `DetectContext`; they never touch tmux or
//! the filesystem. The actor layer owns per-session state (streak for
//! hysteresis, last status) and smooths raw detector output before it
//! reaches the UI.

pub mod claude;
pub mod codex;
pub mod generic;
pub mod kimi;

use std::time::{Duration, SystemTime};

/// Semantic state of a tmux session from the user's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// The agent is actively producing output right now.
    Running,
    /// The agent is paused waiting for user input (prompt, confirmation).
    Waiting,
    /// The agent is done / quiet. Shell prompt, sleeping, etc.
    Idle,
    /// Agent crashed or session errored out.
    #[allow(dead_code)]
    Error,
    /// Detector could not determine the state.
    #[default]
    Unknown,
}

impl Status {
    pub fn glyph(self) -> &'static str {
        match self {
            Status::Running => "●",
            Status::Waiting => "◐",
            Status::Idle => "○",
            Status::Error => "✕",
            Status::Unknown => "·",
        }
    }
}

/// Everything a detector needs to make a decision. Immutable per call.
pub struct DetectContext<'a> {
    /// Raw capture-pane output including ANSI / OSC escapes.
    pub ansi: &'a [u8],
    /// ANSI-stripped plain text version — cheap to match substrings in.
    pub plain: &'a str,
    /// Age of tmux's `session_activity` timestamp.
    pub activity_age: Duration,
    /// The smoothed status from the previous poll (None on first tick).
    pub previous: Option<Status>,
    /// For future use — label on the session so user-regex detectors
    /// can match on session name / agent type.
    #[allow(dead_code)]
    pub session_name: &'a str,
}

impl<'a> DetectContext<'a> {
    pub fn from_parts(
        ansi: &'a [u8],
        plain: &'a str,
        last_activity: Option<SystemTime>,
        now: SystemTime,
        previous: Option<Status>,
        session_name: &'a str,
    ) -> Self {
        let activity_age = match last_activity {
            Some(ts) => now.duration_since(ts).unwrap_or(Duration::ZERO),
            None => Duration::from_secs(u64::MAX / 2),
        };
        Self {
            ansi,
            plain,
            activity_age,
            previous,
            session_name,
        }
    }
}

pub trait StatusDetector: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, ctx: &DetectContext<'_>) -> Status;
    /// Higher priority detectors run first. Claude=100, Codex=90,
    /// Kimi=85, user-regex=80, generic=10. A detector returning
    /// `Unknown` means "I don't know, try the next one".
    fn priority(&self) -> u8;
}

/// Dispatches detectors in priority order. The first non-`Unknown` answer
/// wins; if everyone returns `Unknown`, the result is `Unknown` and the
/// actor will keep the previous status.
pub struct DetectorRegistry {
    detectors: Vec<Box<dyn StatusDetector>>,
}

impl DetectorRegistry {
    pub fn new() -> Self {
        Self {
            detectors: Vec::new(),
        }
    }

    pub fn register(mut self, d: Box<dyn StatusDetector>) -> Self {
        self.detectors.push(d);
        // Sort descending by priority so higher ones run first.
        self.detectors
            .sort_by_key(|d| std::cmp::Reverse(d.priority()));
        self
    }

    /// Build the default registry: claude + codex + kimi + generic.
    pub fn default_stack() -> Self {
        Self::new()
            .register(Box::new(claude::ClaudeDetector))
            .register(Box::new(codex::CodexDetector))
            .register(Box::new(kimi::KimiDetector))
            .register(Box::new(generic::GenericDetector))
    }

    pub fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        for d in &self.detectors {
            let s = d.detect(ctx);
            if s != Status::Unknown {
                return s;
            }
        }
        Status::Unknown
    }
}

impl Default for DetectorRegistry {
    fn default() -> Self {
        Self::default_stack()
    }
}

/// Strip ANSI / OSC escape sequences from `bytes`, returning UTF-8 text.
/// Best-effort: invalid UTF-8 is replaced. Used by detectors and by the
/// actor before populating `DetectContext::plain`.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC — consume an escape sequence.
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: ESC [ ... final byte in 0x40..=0x7e
                    for ch in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: ESC ] ... (BEL | ESC \)
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' {
                            // ESC \ terminator
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some(&c2) if ('\x40'..='\x5f').contains(&c2) => {
                    // 2-byte C1 escape: ESC @..ESC _
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi() {
        let s = strip_ansi(b"\x1b[31mred\x1b[0m plain");
        assert_eq!(s, "red plain");
    }

    #[test]
    fn strip_ansi_removes_osc_bel_terminated() {
        let s = strip_ansi(b"before\x1b]0;title\x07after");
        assert_eq!(s, "beforeafter");
    }

    #[test]
    fn strip_ansi_removes_osc_st_terminated() {
        let s = strip_ansi(b"before\x1b]0;title\x1b\\after");
        assert_eq!(s, "beforeafter");
    }

    #[test]
    fn strip_ansi_preserves_unicode() {
        let s = strip_ansi("日本語 \x1b[1mbold\x1b[0m".as_bytes());
        assert_eq!(s, "日本語 bold");
    }

    #[test]
    fn registry_picks_first_non_unknown_by_priority() {
        struct Fake {
            name: &'static str,
            prio: u8,
            answer: Status,
        }
        impl StatusDetector for Fake {
            fn name(&self) -> &'static str {
                self.name
            }
            fn priority(&self) -> u8 {
                self.prio
            }
            fn detect(&self, _: &DetectContext<'_>) -> Status {
                self.answer
            }
        }
        let r = DetectorRegistry::new()
            .register(Box::new(Fake {
                name: "low",
                prio: 10,
                answer: Status::Idle,
            }))
            .register(Box::new(Fake {
                name: "high_unknown",
                prio: 100,
                answer: Status::Unknown,
            }))
            .register(Box::new(Fake {
                name: "mid",
                prio: 50,
                answer: Status::Running,
            }));
        let now = SystemTime::now();
        let ctx = DetectContext::from_parts(b"", "", Some(now), now, None, "x");
        // high returns Unknown, mid returns Running → Running wins
        assert_eq!(r.detect(&ctx), Status::Running);
    }
}
