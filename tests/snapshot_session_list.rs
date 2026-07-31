//! Snapshot test: render the session list panel against a `TestBackend`
//! and compare the visible text grid against an `insta` snapshot. This is
//! the regression net for UI layout — any unintended change in row content,
//! selection rendering, or layout math trips the snapshot.

use std::time::SystemTime;

use bosun::app::AppState;
use bosun::sidebar::{Container, Section, SidebarModel};
use bosun::tmux::detector::Status;
use bosun::tmux::session::SessionView;
use bosun::tmux::TmuxSession;
use bosun::ui::Theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Build an AppState where every session sits in the `ungrouped`
/// bucket (no sections). For snapshot tests that only care about
/// flat-list rendering.
fn state_with(sessions: Vec<SessionView>) -> AppState {
    let ungrouped = sessions
        .iter()
        .map(|s| Container::single(s.name().to_string(), s.name().to_string()))
        .collect();
    AppState {
        sessions,
        sidebar: SidebarModel {
            ungrouped,
            sections: Vec::new(),
        },
        ..Default::default()
    }
}

fn ses(name: &str, attached: bool) -> SessionView {
    ses_with_status(name, attached, Status::Idle)
}

fn ses_with_status(name: &str, attached: bool, status: Status) -> SessionView {
    // `fmt_age` in `section_preview` is `now - last_activity`, so using
    // `UNIX_EPOCH` would render as "N weeks" where N grows every
    // calendar week and breaks the snapshot. Anchor `last_activity` to
    // `now - 1.5 weeks` so it deterministically rounds to "1w" no
    // matter when the test runs.
    let week_and_a_half = std::time::Duration::from_secs(604_800 + 302_400);
    let stable_activity = SystemTime::now()
        .checked_sub(week_and_a_half)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    SessionView::new(
        TmuxSession {
            name: name.into(),
            display_name: None,
            windows: 1,
            attached,
            created: Some(stable_activity),
            last_activity: Some(stable_activity),
            current_path: Some("/tmp".into()),
            agent: Some("claude".into()),
            container_id: None,
            // `/tmp/work` is chosen deliberately: the session_list meta
            // line runs `shorten_path` which replaces a `$HOME` prefix
            // with `~`. Any path under a real home dir would become
            // `~/...` on the dev machine and stay literal on CI, so the
            // snapshot would drift across machines. `/tmp` can never be
            // anyone's HOME, so both environments render identically.
            spec_path: Some("/tmp/work".into()),
            worktree_path: None,
            branch: None,
            pane_width: 80,
            pane_title: None,
            pane_command: None,
        },
        status,
        None,
    )
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default_opencode();
    terminal
        .draw(|f| bosun::ui::draw(f, state, &theme, None, false))
        .unwrap();
    // TestBackend exposes a Buffer; dump the visible characters.
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn empty_session_list() {
    let state = AppState::default();
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("empty_session_list", frame);
}

#[test]
fn three_sessions_with_middle_selected() {
    let mut state = state_with(vec![
        ses("alpha", false),
        ses("beta", true),
        ses("gamma", false),
    ]);
    state.selected = 1;
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("three_sessions_middle_selected", frame);
}

#[test]
fn warning_shows_in_statusbar() {
    let mut state = state_with(vec![ses("alpha", false)]);
    state.warning = Some("list: tmux not running".to_string());
    let frame = render(&state, 80, 6);
    insta::assert_snapshot!("warning_in_statusbar", frame);
}

#[test]
fn mixed_statuses_render_glyphs() {
    let mut state = state_with(vec![
        ses_with_status("build", false, Status::Running),
        ses_with_status("review", true, Status::Waiting),
        ses_with_status("shell", false, Status::Idle),
        ses_with_status("crashed", false, Status::Error),
    ]);
    state.selected = 1;
    let frame = render(&state, 80, 10);
    insta::assert_snapshot!("mixed_statuses", frame);
}

#[test]
fn kill_in_progress_marks_the_row() {
    // Issue #7: a dispatched-but-not-landed kill swaps the row's status
    // glyph for the working marker and trails a "killing…" label, while
    // the untouched row keeps its normal idle glyph.
    use bosun::app::{OpKind, PendingOp};
    let mut state = state_with(vec![ses("alpha", false), ses("beta", false)]);
    state.pending_ops.insert(
        "alpha".into(),
        PendingOp {
            kind: OpKind::Killing,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(20),
        },
    );
    let frame = render(&state, 80, 8);
    insta::assert_snapshot!("kill_in_progress_row", frame);
}

#[test]
fn create_in_progress_shows_in_statusbar() {
    // Issue #7: a create has no row yet, so its feedback lands in the
    // status bar's left segment instead of on a row.
    use bosun::app::PendingCreate;
    let mut state = state_with(vec![ses("alpha", false)]);
    state.pending_create = Some(PendingCreate {
        display: "rocket-fox".into(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(20),
    });
    let frame = render(&state, 80, 6);
    insta::assert_snapshot!("create_in_progress_statusbar", frame);
}

#[test]
fn sections_group_sessions() {
    let mut state = AppState {
        sessions: vec![ses("alpha", false), ses("beta", false), ses("gamma", true)],
        selected: 1, // on the section header
        ..Default::default()
    };
    state.sidebar = SidebarModel {
        ungrouped: vec![Container::single("alpha".into(), "alpha".into())],
        sections: vec![Section {
            id: "g1".into(),
            name: "Premium Products".into(),
            members: vec![
                Container::single("beta".into(), "beta".into()),
                Container::single("gamma".into(), "gamma".into()),
            ],
            collapsed: false,
            banner_font: None,
        }],
    };
    let frame = render(&state, 80, 12);
    insta::assert_snapshot!("sections_group_sessions", frame);
}
