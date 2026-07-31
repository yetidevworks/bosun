//! Claude Code detector.
//!
//! Strategy: prefer what Claude says about *itself* over what we can
//! infer from its pixels. Claude publishes two first-party signals that
//! ride `tmux list-sessions` for free:
//!
//!   * `#{pane_title}` — set to `<glyph> <task title>`, where the glyph
//!     is a braille spinner frame while a turn is running and `✳` when
//!     it isn't. tmux stores only the last title the app set, so this
//!     does NOT animate frame-by-frame the way the pane body does.
//!   * `#{pane_current_command}` — Claude names its process after its
//!     own version (`2_1_220`), so a session whose agent exited reads
//!     as `zsh`/`bash` instead.
//!
//! Pane scraping stays as the fallback (and as the *only* source for
//! confirmation prompts, which the title can't express). Confining
//! those heuristics to the bottom region of the capture is the single
//! biggest reliability win — it filters out stale "Thinking…" strings
//! sitting in scrollback above the prompt and stops random shell output
//! from triggering false positives.
//!
//! Stack, cheapest first:
//!   1. Claude anchor: the version-shaped `pane_current_command`, the
//!      prompt-box corners (`╭` … `╰`) in the bottom region, or the
//!      classic substring anchors for transient full-screen states.
//!   2. Confirmation prompts ("Do you want to", numbered `❯` option,
//!      `(y/n)`) in the bottom region → Waiting.
//!   3. Active spinner: a braille frame leading the pane title, a
//!      braille glyph in the raw OSC title, or a "Thinking / Pondering
//!      / …" verb with ellipsis in the bottom region → Running.
//!   4. Idle fallback when none of the above fire. The app promotes
//!      Idle to `Done` for a session that ran and hasn't been looked at
//!      since — see `AppState::display_status`.
//!
//! If the pane doesn't look like Claude at all, return Unknown so
//! the registry falls through to the next detector.
//!
//! Tested against real Claude Code `capture-pane -e` fixtures under
//! `tests/fixtures/detector/`.

use super::{is_braille, title_is_working, DetectContext, Status, StatusDetector};

/// How many trailing non-empty lines to consider "the prompt region."
/// Claude's prompt box is 3 lines; the surrounding hint / spinner /
/// confirmation rows live within ~10 lines of it. Wider than that and
/// we start matching stale output again.
const BOTTOM_REGION_LINES: usize = 12;

pub struct ClaudeDetector;

impl StatusDetector for ClaudeDetector {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn priority(&self) -> u8 {
        100
    }

    fn detect(&self, ctx: &DetectContext<'_>) -> Status {
        let bottom = bottom_region(ctx.plain);

        if !looks_like_claude(ctx.plain, &bottom, ctx.pane_command) {
            return Status::Unknown;
        }

        // Prompt markers — user needs to answer. Scoped to the bottom
        // region so a "(y/n)" in code output doesn't pin the glyph to
        // Waiting forever. The pane is the only source for this; the
        // title says nothing about a pending confirmation.
        if has_prompt_marker(&bottom) {
            return Status::Waiting;
        }

        // Busy. The pane title's leading spinner frame is checked first
        // and is authoritative — Claude sets it on state change, so it
        // can't go stale mid-turn the way a scrolled-past "Thinking…"
        // line can. The pane heuristics stay as fallback for older
        // Claude builds (and other tools) that don't set a title:
        // the raw-OSC scan covers a title tmux hasn't recorded yet, and
        // the verb scan is bottom-region only.
        if ctx.pane_title.is_some_and(title_is_working)
            || has_thinking_marker(&bottom)
            || has_spinner_title(ctx.ansi)
        {
            return Status::Running;
        }

        // Claude is up but neither answering a prompt nor actively
        // working. That covers an empty composer (a fresh instance you
        // haven't tasked yet), a composer with unsubmitted text, the
        // splash screen, the backgrounded-conversation task list, and
        // shell output after exit. None of these need your attention,
        // so they all read as Idle — only an explicit
        // confirmation/question (handled above) counts as Waiting.
        // Previously a visible prompt box pinned the glyph to Waiting,
        // which made every idle Claude look like it was asking for
        // something.
        //
        // A turn that just *finished* also lands here: the pane alone
        // can't tell "done, unread" from "sitting at an empty composer"
        // — that's a question about what the user has looked at, which
        // only the app knows. `AppState::display_status` promotes Idle
        // to `Done` for a session that ran and hasn't been viewed since.
        Status::Idle
    }
}

/// Last N non-empty lines of `plain`, joined with `\n` in source
/// order. Cheap to re-scan in subsequent helpers and small enough
/// that substring checks stay free.
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

fn looks_like_claude(plain: &str, bottom: &str, pane_command: Option<&str>) -> bool {
    // Strongest signal, and free: Claude Code renames its process to
    // its own version, so `#{pane_current_command}` reads `2_1_220`.
    // This holds even when the pane is showing something that looks
    // nothing like the usual UI (a full-screen diff, the backgrounded
    // task list, a `/`-menu), where every text anchor below can miss.
    if pane_command.is_some_and(is_claude_version_command) {
        return true;
    }
    // Next strongest: Claude's box-drawn prompt corners visible in
    // the bottom region. Unique enough that no plain shell pane will
    // hit it by accident.
    if bottom.contains('╭') && bottom.contains('╰') {
        return true;
    }
    // Fall back to broader anchors for transient states where the
    // box isn't currently rendered (full-screen modal, splash, etc.).
    plain.contains("? for shortcuts")
        || plain.contains("▐▛███▜▌") // splash art
        || plain.contains("Claude Code")
}

/// Claude Code sets its process title to its version with dots replaced
/// by underscores (`2.1.220` → `2_1_220`), which is what
/// `#{pane_current_command}` reports. Match that shape — leading digit,
/// then only digits and underscores, with at least one underscore — so
/// a genuine binary named e.g. `node` or `zsh` can't be mistaken for it.
fn is_claude_version_command(cmd: &str) -> bool {
    cmd.starts_with(|c: char| c.is_ascii_digit())
        && cmd.contains('_')
        && cmd.chars().all(|c| c.is_ascii_digit() || c == '_')
}

fn has_prompt_marker(region: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "Do you want to",
        "Would you like to",
        "Choose an option",
        "Press any key to continue",
        "(y/n)",
        "(Y/n)",
        "(y/N)",
    ];
    if PROMPTS.iter().any(|p| region.contains(p)) {
        return true;
    }
    // `❯` heads the *selected* row of a confirmation menu — but recent
    // Claude versions ALSO use `❯` as the composer's input glyph
    // (`│ ❯ …`), so a bare `❯` no longer means "waiting on a choice."
    // Claude's menus are numbered (`❯ 1. Yes`), so only count `❯` when
    // the text right after it starts with a digit. That matches the
    // option arrow while ignoring an empty or typed-into composer.
    region.lines().any(|l| match l.find('❯') {
        Some(i) => l[i + '❯'.len_utf8()..]
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        None => false,
    })
}

fn has_thinking_marker(region: &str) -> bool {
    // Claude prints rotating verbs: "Thinking", "Pondering", etc., often
    // preceded by a `✻` / middle-dot bullet. We look for any of the
    // common verbs combined with an ellipsis to avoid false positives
    // from normal text. Scoped to the bottom region so a "Thinking…"
    // line that scrolled past the prompt no longer pegs the glyph to
    // Running.
    const VERBS: &[&str] = &[
        "Thinking",
        "Pondering",
        "Reviewing",
        "Synthesizing",
        "Computing",
        "Formulating",
        "Contemplating",
        "Analyzing",
        "Reasoning",
        "Crafting",
        "Considering",
        "Working",
    ];
    VERBS
        .iter()
        .any(|v| region.contains(&format!("{}…", v)) || region.contains(&format!("{}...", v)))
}

fn has_spinner_title(ansi: &[u8]) -> bool {
    // OSC 0 / OSC 2 titles look like `ESC ] 0 ; <title> BEL` or
    // `ESC ] 0 ; <title> ESC \`. Scan the raw bytes for a title
    // containing braille spinner glyphs (U+2800..U+28FF).
    let s = String::from_utf8_lossy(ansi);
    let mut in_title = false;
    let mut title = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some(']') {
                // consume "0;" or "2;"
                for _ in 0..2 {
                    chars.next();
                }
                in_title = true;
                title.clear();
            }
        } else if in_title {
            if c == '\x07' || c == '\x1b' {
                if title.chars().any(is_braille) {
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
    use crate::tmux::detector::{strip_ansi, DetectContext};

    fn ctx_plain(s: &str) -> DetectContext<'_> {
        let now = SystemTime::now();
        DetectContext::from_parts(s.as_bytes(), s, Some(now), now, None, "test", None, None)
    }

    /// Same as [`ctx_plain`] but with the first-party tmux signals set,
    /// so the title/process-name paths get exercised.
    fn ctx_with<'a>(s: &'a str, title: Option<&'a str>, cmd: Option<&'a str>) -> DetectContext<'a> {
        let now = SystemTime::now();
        DetectContext::from_parts(s.as_bytes(), s, Some(now), now, None, "test", title, cmd)
    }

    #[test]
    fn non_claude_returns_unknown() {
        let ctx = ctx_plain("$ ls -la\ntotal 42\n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn empty_prompt_box_is_idle() {
        // Steady-state Claude UI: prompt box visible, no spinner, no
        // confirmation. A fresh/idle composer doesn't need the user's
        // attention, so it reads as Idle — not Waiting.
        let pane = "\
Claude Code v2.1.152
some prior output

╭──────────────────────────────╮
│ >                             │
╰──────────────────────────────╯
  ? for shortcuts
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn caret_composer_glyph_is_idle() {
        // Recent Claude uses `❯` as the composer input glyph. A bare
        // `❯` in the prompt box must NOT read as a confirmation menu —
        // this is the "fresh instance shows as waiting" bug.
        let pane = "\
Claude Code v2.1.152

╭──────────────────────────────╮
│ ❯                             │
╰──────────────────────────────╯
  ? for shortcuts
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn caret_typed_into_composer_is_idle() {
        // `❯` followed by free-form text (a typed-but-unsubmitted
        // message) is still the composer, not a numbered menu.
        let pane = "\
Claude Code v2.1.152

╭──────────────────────────────╮
│ ❯ Fix the parser bug          │
╰──────────────────────────────╯
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn numbered_caret_menu_is_waiting() {
        // The real confirmation menu: `❯` heads a numbered option.
        let pane = "\
Claude Code v2.1.152
running a tool

╭──────────────────────────────╮
│ Edit file.rs?                 │
│ ❯ 1. Yes                      │
│   2. No                       │
╰──────────────────────────────╯
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn typed_but_unsubmitted_prompt_is_idle() {
        // Text sitting in the composer (not yet submitted) still means
        // Claude isn't doing anything — Idle, not Waiting.
        let pane = "\
Claude Code v2.1.152

╭──────────────────────────────╮
│ > fix the parser bug          │
╰──────────────────────────────╯
  ? for shortcuts
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn confirm_prompt_yields_waiting() {
        let pane = "\
Claude Code v2.1.152
running tool…

Do you want to proceed? (y/n)
❯ 1. Yes
  2. No
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Waiting);
    }

    #[test]
    fn thinking_in_bottom_region_yields_running() {
        let pane = "\
Claude Code session
some output

✻ Thinking… (3s · esc to interrupt)
╭──────────────────────────────╮
│ > my prompt                   │
╰──────────────────────────────╯
";
        let ctx = ctx_plain(pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn stale_thinking_in_scrollback_does_not_trigger_running() {
        // "Thinking…" appears far above the visible prompt, simulating
        // a stale line that scrolled almost-off (still in the visible
        // capture but well outside the bottom region). The live prompt
        // box is an empty composer, so the pane reads as Idle — the
        // key assertion is that the stale verb does NOT peg it Running.
        let pane = format!(
            "Claude Code v2.1.152\n· Thinking…\n{}\n╭──────╮\n│ >     │\n╰──────╯\n",
            "filler line\n".repeat(20)
        );
        let ctx = ctx_plain(&pane);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn spinner_title_yields_running() {
        // Build the byte stream by hand: OSC title with a braille
        // spinner glyph, followed by a Claude prompt box. Mixing
        // multi-byte UTF-8 into a byte string literal isn't allowed,
        // so we concatenate.
        let mut ansi: Vec<u8> = Vec::new();
        ansi.extend_from_slice(b"Claude Code\n\x1b]0;\xe2\xa0\x8b Working\x07");
        ansi.extend_from_slice("╭──╮\n│ > │\n╰──╯".as_bytes());
        let plain = strip_ansi(&ansi);
        let now = SystemTime::now();
        let ctx =
            DetectContext::from_parts(&ansi, &plain, Some(now), now, None, "test", None, None);
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn pane_title_spinner_yields_running() {
        // The authoritative busy signal: Claude's own OSC title carries
        // a braille frame while a turn is in flight. No "Thinking…" in
        // the pane at all — the title alone must be enough.
        let pane = "\
Claude Code v2.1.220

╭──────────────────────────────╮
│ ❯                             │
╰──────────────────────────────╯
";
        let ctx = ctx_with(pane, Some("⠙ Fix the parser bug"), Some("2_1_220"));
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Running);
    }

    #[test]
    fn pane_title_star_glyph_is_idle() {
        // `✳` is Claude's not-working marker. Same pane as above, so
        // the only thing deciding the answer is the title glyph.
        let pane = "\
Claude Code v2.1.220

╭──────────────────────────────╮
│ ❯                             │
╰──────────────────────────────╯
";
        let ctx = ctx_with(pane, Some("✳ Fix the parser bug"), Some("2_1_220"));
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn version_process_name_anchors_claude() {
        // The backgrounded task list has none of the usual anchors —
        // no prompt box, no "? for shortcuts" — but the process name
        // still identifies it as Claude rather than falling through.
        let pane = "\
Ready for review
· Review TailwindPHP library for Grav adm…  claude --dangerously-skip-permissions
───────────────────────────────────────────
❯ describe a task for a new session
";
        let ctx = ctx_with(
            pane,
            Some("2 awaiting input · claude agents"),
            Some("2_1_220"),
        );
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn task_list_working_row_does_not_read_as_running() {
        // The task list shows OTHER sessions' work, including rotating
        // spinner glyphs on their rows. That must not make *this*
        // session read as busy — its own title has no spinner frame.
        let pane = "\
Working
✻ Ensure forum-pro CSS works with default…  i'm pretty close to getting Forum Pro …   1d
───────────────────────────────────────────
❯ describe a task for a new session
";
        let ctx = ctx_with(
            pane,
            Some("2 awaiting input · claude agents"),
            Some("2_1_220"),
        );
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn shell_process_name_is_not_claude() {
        let ctx = ctx_with("$ ls -la\ntotal 42\n", Some("hades-2.local"), Some("zsh"));
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Unknown);
    }

    #[test]
    fn version_command_shape_is_strict() {
        assert!(is_claude_version_command("2_1_220"));
        assert!(is_claude_version_command("2_1_0"));
        assert!(!is_claude_version_command("zsh"));
        assert!(!is_claude_version_command("node"));
        assert!(!is_claude_version_command("7zip"));
        assert!(!is_claude_version_command("bash5_2"));
    }

    #[test]
    fn bare_prompt_line_is_idle() {
        // Pre-box fallback: still recognized as Claude via the splash /
        // shortcuts anchor, but a bare `>` composer line is idle — the
        // instance is up and waiting for you to type a task, which is
        // not the same as the agent needing a decision.
        let ctx = ctx_plain("Claude Code session\n? for shortcuts\n\n> \n");
        assert_eq!(ClaudeDetector.detect(&ctx), Status::Idle);
    }

    #[test]
    fn settled_output_is_idle() {
        let ctx = ctx_plain("Claude Code session\nDone.\n$ ");
        // "$ " is non-prompt shell line — plain terminal state after exit
        let got = ClaudeDetector.detect(&ctx);
        assert!(
            got == Status::Idle || got == Status::Unknown,
            "expected Idle or Unknown, got {:?}",
            got
        );
    }
}
