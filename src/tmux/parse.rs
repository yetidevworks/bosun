//! Pure parsers for tmux CLI output. No I/O, no allocations we can avoid.
//! Every parser gets unit-tested against fixtures in `#[cfg(test)]` so the
//! shell-out layer (`client.rs`) can stay thin.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{BosunError, Result};
use crate::tmux::session::TmuxSession;

/// The format string we pass to `tmux list-sessions -F`. Fields are
/// separated by the printable sequence `|||`. We can't use a control
/// character (like `\x1f`) because tmux 3.4+ escapes all control
/// characters in format output as octal sequences (`\037` for 0x1f)
/// as a security measure against terminal-injection via session
/// names — that broke the Ubuntu CI job, which ships tmux 3.4. `|||`
/// is printable ASCII, so tmux passes it through unmodified, and
/// three pipes is vanishingly unlikely to appear in a real session
/// name, display name, or filesystem path.
///
/// The trailing `@bosun_*` fields read user options we set at create time:
/// - `@bosun_display`       — pretty UI name (e.g. "rasterfox" for internal `bosun-rasterfox-a1b2c3d4`)
/// - `@bosun_agent`         — agent kind (claude / codex / terminal)
/// - `@bosun_path`          — spec path the user typed into the new-session modal
/// - `@bosun_container_id`  — sidebar container this session belongs to (tabs feature)
/// - `@bosun_worktree_path` — git worktree backing this session (worktree feature)
/// - `@bosun_branch`        — branch checked out in that worktree (worktree feature)
///
/// All six are empty strings for non-bosun sessions and get parsed as
/// `None` so the UI renders them only when available.
///
/// The final two fields are tmux's own view of the active pane, and are
/// what the status detectors prefer over scraping the pane body:
/// - `pane_title`           — the OSC title the app set. Claude Code writes
///   `<glyph> <task>`, braille while working and `✳` when not, and only
///   rewrites it on a state change — so unlike the pane body it doesn't
///   churn frame-by-frame.
/// - `pane_current_command` — the running process. Claude Code renames
///   itself to its version (`2_1_220`), so this separates a live agent
///   from a bare `zsh`.
pub const LIST_SESSIONS_FORMAT: &str = "#{session_name}|||#{session_windows}|||#{session_attached}|||#{session_created}|||#{session_activity}|||#{session_path}|||#{@bosun_display}|||#{@bosun_agent}|||#{@bosun_path}|||#{@bosun_container_id}|||#{@bosun_worktree_path}|||#{@bosun_branch}|||#{pane_width}|||#{pane_title}|||#{pane_current_command}";

const FIELD_SEP: &str = "|||";

/// Parse the full `tmux list-sessions -F <LIST_SESSIONS_FORMAT>` output.
/// One session per line; empty input is valid (no sessions).
pub fn parse_list_sessions(input: &str) -> Result<Vec<TmuxSession>> {
    let mut out = Vec::new();
    for (idx, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(parse_session_line(line).map_err(|e| {
            // Include the raw line (with control chars debug-escaped so
            // the ASCII-31 `\x1f` separators are visible) in the error —
            // when this fires on CI the message tells us exactly what
            // tmux output instead of leaving us guessing.
            BosunError::Parse(format!("line {}: {} — raw: {:?}", idx + 1, e, line))
        })?);
    }
    Ok(out)
}

fn parse_session_line(line: &str) -> std::result::Result<TmuxSession, String> {
    let mut parts = line.split(FIELD_SEP);

    let name = parts
        .next()
        .ok_or_else(|| "missing session name".to_string())?
        .to_string();
    let windows_raw = parts
        .next()
        .ok_or_else(|| "missing session_windows".to_string())?;
    let attached_raw = parts
        .next()
        .ok_or_else(|| "missing session_attached".to_string())?;
    let created_raw = parts
        .next()
        .ok_or_else(|| "missing session_created".to_string())?;
    let activity_raw = parts
        .next()
        .ok_or_else(|| "missing session_activity".to_string())?;
    let path = parts.next().map(|s| s.to_string());
    let display_raw = parts.next().map(|s| s.to_string());
    // `@bosun_agent`, `@bosun_path`, and `@bosun_container_id` are
    // optional trailing fields — older tmux list-sessions outputs
    // (from before we added them to LIST_SESSIONS_FORMAT) may not
    // include them, and fixtures in tests sometimes omit them for
    // brevity.
    let agent_raw = parts.next().map(|s| s.to_string());
    let spec_path_raw = parts.next().map(|s| s.to_string());
    let container_id_raw = parts.next().map(|s| s.to_string());
    // Worktree path and branch (worktree feature). Optional trailing
    // fields for the same back-compat reasons as the other `@bosun_*`
    // options above — older outputs and terse fixtures omit them.
    let worktree_path_raw = parts.next().map(|s| s.to_string());
    let branch_raw = parts.next().map(|s| s.to_string());
    // Current pane width. Used to tell a genuine content change from a
    // mere reflow (resize / embed / a second bosun instance attaching),
    // so a layout change doesn't read as unread. Optional trailing
    // field: older outputs and terse test fixtures omit it → 0.
    let pane_width_raw = parts.next();
    // Pane title + running command. Both are first-party state signals
    // the agent TUI publishes about itself, far steadier than scraping
    // the pane body. Optional trailing fields for the same back-compat
    // reasons as the fields above.
    let pane_title_raw = parts.next().map(|s| s.to_string());
    let pane_command_raw = parts.next().map(|s| s.to_string());

    if parts.next().is_some() {
        return Err("unexpected extra field".into());
    }

    let windows: u32 = windows_raw
        .parse()
        .map_err(|e| format!("session_windows '{}': {}", windows_raw, e))?;

    let attached = match attached_raw {
        "0" => false,
        "1" => true,
        // tmux gives the attached-client count; >0 means attached.
        other => other.parse::<u32>().map(|n| n > 0).unwrap_or(false),
    };

    let created = parse_epoch(created_raw);
    let last_activity = parse_epoch(activity_raw);
    let pane_width: u16 = pane_width_raw.and_then(|s| s.parse().ok()).unwrap_or(0);

    Ok(TmuxSession {
        name,
        windows,
        attached,
        created,
        last_activity,
        current_path: path.filter(|p| !p.is_empty()),
        display_name: display_raw.filter(|s| !s.is_empty()),
        agent: agent_raw.filter(|s| !s.is_empty()),
        spec_path: spec_path_raw.filter(|s| !s.is_empty()),
        container_id: container_id_raw.filter(|s| !s.is_empty()),
        worktree_path: worktree_path_raw.filter(|s| !s.is_empty()),
        branch: branch_raw.filter(|s| !s.is_empty()),
        pane_width,
        pane_title: pane_title_raw.filter(|s| !s.is_empty()),
        pane_command: pane_command_raw.filter(|s| !s.is_empty()),
    })
}

fn parse_epoch(s: &str) -> Option<SystemTime> {
    if s.is_empty() {
        return None;
    }
    let secs: u64 = s.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_input() {
        assert!(parse_list_sessions("").unwrap().is_empty());
    }

    #[test]
    fn parses_single_session() {
        let line = "main|||3|||1|||1712000000|||1712003600|||/tmp/code";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.name, "main");
        assert_eq!(s.windows, 3);
        assert!(s.attached);
        assert_eq!(s.current_path.as_deref(), Some("/tmp/code"));
        assert!(s.created.is_some());
        assert!(s.last_activity.is_some());
    }

    #[test]
    fn parses_pane_width() {
        // Full format including the two worktree fields and the trailing
        // pane_width field (pane_width stays last after the insert).
        let line =
            "main|||1|||1|||1712000000|||1712003600|||/tmp|||Main|||claude|||/tmp|||cid|||/wt|||br|||128";
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.pane_width, 128);
    }

    #[test]
    fn missing_pane_width_defaults_to_zero() {
        // Terse fixtures / older tmux output omit the field.
        let line = "main|||1|||1|||1712000000|||1712003600|||/tmp";
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.pane_width, 0);
    }

    #[test]
    fn parses_pane_title_and_command() {
        // A real line off `tmux -L bosun list-sessions`, spinner glyph
        // and all — the title is what the Claude detector reads to
        // decide Running vs Idle.
        let line = concat!(
            "main|||1|||1|||1712000000|||1712003600|||/tmp|||Main|||claude|||/tmp|||cid|||/wt|||br",
            "|||95|||⠐ Ensure forum-pro CSS works|||2_1_220"
        );
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.pane_width, 95);
        assert_eq!(
            s.pane_title.as_deref(),
            Some("⠐ Ensure forum-pro CSS works")
        );
        assert_eq!(s.pane_command.as_deref(), Some("2_1_220"));
    }

    #[test]
    fn missing_pane_title_and_command_default_to_none() {
        // Back-compat: terse fixtures stop at pane_width.
        let line =
            "main|||1|||1|||1712000000|||1712003600|||/tmp|||Main|||claude|||/tmp|||cid|||/wt|||br|||95";
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.pane_title, None);
        assert_eq!(s.pane_command, None);
    }

    #[test]
    fn empty_pane_title_is_none_not_blank() {
        // A pane that never set a title yields an empty field; that's
        // "unknown", not a title of "".
        let line = concat!(
            "main|||1|||1|||1712000000|||1712003600|||/tmp|||Main|||claude|||/tmp|||cid|||/wt|||br",
            "|||95||||||zsh"
        );
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.pane_title, None);
        assert_eq!(s.pane_command.as_deref(), Some("zsh"));
    }

    #[test]
    fn parses_multiple_sessions() {
        let input = concat!(
            "alpha|||1|||0|||1700000000|||1700000100|||/tmp\n",
            "beta|||2|||1|||1700001000|||1700002000|||/home/rhuk\n",
            "gamma|||5|||0|||1700003000|||1700004000|||\n",
        );
        let sessions = parse_list_sessions(input).unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].name, "alpha");
        assert!(!sessions[0].attached);
        assert_eq!(sessions[1].name, "beta");
        assert!(sessions[1].attached);
        assert_eq!(sessions[2].name, "gamma");
        assert!(sessions[2].current_path.is_none());
    }

    #[test]
    fn names_with_special_chars_survive_separator() {
        // Colons, single pipes, and spaces must all pass through —
        // the separator is specifically `|||` (three pipes) so a
        // lone `|` in a name doesn't trip the split.
        let line = "work: proj | v2|||1|||1|||1700000000|||1700000100|||/srv";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].name, "work: proj | v2");
    }

    #[test]
    fn unicode_name_preserved() {
        let line = "日本語セッション|||1|||0|||1700000000|||1700000100|||/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].name, "日本語セッション");
    }

    #[test]
    fn attached_client_count_treated_as_bool() {
        let line = "multi|||1|||3|||1700000000|||1700000100|||/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].attached);
    }

    #[test]
    fn empty_lines_skipped() {
        let input = "\nalpha|||1|||0|||1700000000|||1700000100|||/tmp\n\n";
        let sessions = parse_list_sessions(input).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "alpha");
    }

    #[test]
    fn malformed_line_errors_with_line_number() {
        let input = "alpha|||1|||0\nbroken_but_no_seps";
        let err = parse_list_sessions(input).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("line 1") || msg.contains("line 2"),
            "msg was: {}",
            msg
        );
    }

    #[test]
    fn missing_activity_ok_but_none() {
        let line = "alpha|||1|||0|||1700000000||||||/tmp";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].created.is_some());
        assert!(sessions[0].last_activity.is_none());
    }

    #[test]
    fn bosun_user_options_parse_when_present() {
        let line = "bosun-foo|||1|||0|||1700000000|||1700000100|||/srv|||foo|||claude|||~/proj";
        let sessions = parse_list_sessions(line).unwrap();
        assert_eq!(sessions[0].display_name.as_deref(), Some("foo"));
        assert_eq!(sessions[0].agent.as_deref(), Some("claude"));
        assert_eq!(sessions[0].spec_path.as_deref(), Some("~/proj"));
    }

    #[test]
    fn parses_worktree_fields() {
        // Full format: ...|@bosun_container_id|@bosun_worktree_path|@bosun_branch|pane_width
        let line = "bosun-foo|||1|||0|||1700000000|||1700000100|||/srv|||foo|||claude|||~/proj|||cid|||/srv/.worktrees/feat|||feat|||120";
        let s = &parse_list_sessions(line).unwrap()[0];
        assert_eq!(s.worktree_path.as_deref(), Some("/srv/.worktrees/feat"));
        assert_eq!(s.branch.as_deref(), Some("feat"));
        assert_eq!(s.pane_width, 120);
    }

    #[test]
    fn missing_worktree_fields_are_none() {
        let line = "plain|||1|||0|||1700000000|||1700000100|||/srv";
        let s = &parse_list_sessions(line).unwrap()[0];
        assert!(s.worktree_path.is_none());
        assert!(s.branch.is_none());
    }

    #[test]
    fn missing_bosun_options_are_none_not_error() {
        // Non-bosun session: `@bosun_*` all return empty strings,
        // which we parse as None. Also covers the back-compat case
        // where `@bosun_agent`/`@bosun_path` were added later.
        let line = "plain|||1|||0|||1700000000|||1700000100|||/srv|||||||||";
        let sessions = parse_list_sessions(line).unwrap();
        assert!(sessions[0].display_name.is_none());
        assert!(sessions[0].agent.is_none());
        assert!(sessions[0].spec_path.is_none());
    }
}
