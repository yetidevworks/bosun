//! Session list panel. Walks the explicit-membership `SidebarModel`,
//! emitting one row per visible entry (ungrouped session, section
//! header, or section member).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, OpKind};
use crate::sidebar::{Container, Section, VisibleEntry};
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default().style(Style::default().bg(theme.bg));

    let visible = state.sidebar.visible();

    if visible.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no tmux sessions",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(Span::styled(
                "  (press n to create · g for a section)",
                Style::default().fg(theme.text_muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Track the section-index (0-based) as we walk the visible list so
    // section headers can show their numeric jump key.
    let mut section_idx: usize = 0;
    let mut out: Vec<Line<'_>> = Vec::with_capacity(visible.len() * 2);
    // In narrow mode the preview pane (and its tab strip) is hidden,
    // so multi-tab container membership is otherwise invisible. Emit
    // an extra "tabs:" line per container in that case.
    let narrow = state.term_size.0 < crate::ui::layout::PREVIEW_MIN_WIDTH;
    // For each entry, record the first/last line index it produced.
    // Used after the build to compute a scroll offset that keeps the
    // selected entry fully visible — without this, on small screens
    // (mobile / mosh / phone) the list never scrolls and entries past
    // the viewport bottom are unreachable.
    let mut entry_first_line: Vec<usize> = Vec::with_capacity(visible.len());
    let mut entry_last_line: Vec<usize> = Vec::with_capacity(visible.len());
    for (i, entry) in visible.iter().enumerate() {
        let selected = i == state.selected;
        let start = out.len();
        match entry {
            VisibleEntry::Ungrouped(c) => {
                let tabs = c.members.len() as u16;
                let bg_busy = background_activity(state, c);
                let unread = container_unread(state, c);
                match state.session_by_name(&c.active) {
                    Some(v) => {
                        let op = state.pending_ops.get(v.name()).map(|o| o.kind);
                        let status = state.display_status(v);
                        out.push(render_primary_line(
                            v, status, selected, false, tabs, bg_busy, unread, op, area.width,
                            theme,
                        ));
                        out.push(render_meta_line(v, selected, false, area.width, theme));
                        if narrow && tabs > 1 {
                            out.push(render_tabs_line(
                                c, state, selected, false, area.width, theme,
                            ));
                        }
                    }
                    None => {
                        let label = state.dead_display_for(&c.active);
                        out.push(render_missing_line(
                            &label, selected, false, area.width, theme,
                        ));
                    }
                }
            }
            VisibleEntry::SectionHeader(s) => {
                let jump_key = if section_idx < 9 {
                    Some((section_idx as u8 + b'1') as char)
                } else {
                    None
                };
                out.push(render_section_line(
                    s, selected, jump_key, area.width, theme,
                ));
                section_idx += 1;
            }
            VisibleEntry::Member { container, .. } => {
                let tabs = container.members.len() as u16;
                let bg_busy = background_activity(state, container);
                let unread = container_unread(state, container);
                match state.session_by_name(&container.active) {
                    Some(v) => {
                        let op = state.pending_ops.get(v.name()).map(|o| o.kind);
                        let status = state.display_status(v);
                        out.push(render_primary_line(
                            v, status, selected, true, tabs, bg_busy, unread, op, area.width, theme,
                        ));
                        out.push(render_meta_line(v, selected, true, area.width, theme));
                        if narrow && tabs > 1 {
                            out.push(render_tabs_line(
                                container, state, selected, true, area.width, theme,
                            ));
                        }
                    }
                    None => {
                        let label = state.dead_display_for(&container.active);
                        out.push(render_missing_line(
                            &label, selected, true, area.width, theme,
                        ));
                    }
                }
            }
        }
        entry_first_line.push(start);
        entry_last_line.push(out.len().saturating_sub(1));
    }

    let total_lines = out.len();
    let viewport = area.height as usize;
    // Compute a scroll offset that keeps the selected entry fully
    // visible. No persistent scroll state — derive every frame from
    // the current selection so j/k, jump keys, and section toggles
    // all stay consistent without sync logic. Without this, on small
    // screens (mobile / mosh) entries past the viewport bottom were
    // unreachable.
    let scroll: u16 = if total_lines <= viewport || viewport == 0 {
        0
    } else {
        let sel = state.selected.min(entry_first_line.len().saturating_sub(1));
        let first = entry_first_line[sel];
        let last = entry_last_line[sel];
        let max_scroll = total_lines.saturating_sub(viewport);
        // Center-ish: aim for the selection at ~1/3 from the top so
        // there's context above and below as the user navigates.
        let target = viewport / 3;
        let mut s = first.saturating_sub(target);
        // Always keep the entry's last line on-screen — matters for
        // 2-line session entries near the bottom of the list.
        if last >= s + viewport {
            s = (last + 1).saturating_sub(viewport);
        }
        s.min(max_scroll) as u16
    };

    let p = Paragraph::new(out).block(block).scroll((scroll, 0));
    frame.render_widget(p, area);
}

/// Lines a single visible entry occupies in the rendered list.
/// Sessions render as two lines (primary + meta); section headers
/// and dead-row sessions are single-line. Kept as a small shared
/// helper so [`entry_at_row`] stays in lockstep with [`render`].
fn entry_line_count(state: &AppState, entry: &VisibleEntry<'_>) -> u16 {
    match entry {
        VisibleEntry::Ungrouped(c) => {
            if state.session_by_name(&c.active).is_some() {
                2
            } else {
                1
            }
        }
        VisibleEntry::SectionHeader(_) => 1,
        VisibleEntry::Member { container, .. } => {
            if state.session_by_name(&container.active).is_some() {
                2
            } else {
                1
            }
        }
    }
}

/// Same scroll heuristic as [`render`] but computed from line
/// counts only — no rendered `Line`s needed. Centers the selected
/// entry at roughly the top third of the viewport while keeping
/// its last line on-screen.
fn compute_scroll(counts: &[u16], selected: usize, viewport: u16) -> u16 {
    let total_lines: u16 = counts.iter().copied().fold(0u16, u16::saturating_add);
    if total_lines <= viewport || viewport == 0 {
        return 0;
    }
    let sel = selected.min(counts.len().saturating_sub(1));
    let first: u16 = counts
        .iter()
        .take(sel)
        .copied()
        .fold(0u16, u16::saturating_add);
    let last = first.saturating_add(counts.get(sel).copied().unwrap_or(0).saturating_sub(1));
    let max_scroll = total_lines.saturating_sub(viewport);
    let target = viewport / 3;
    let mut s = first.saturating_sub(target);
    if last >= s.saturating_add(viewport) {
        s = last.saturating_add(1).saturating_sub(viewport);
    }
    s.min(max_scroll)
}

/// Map an absolute terminal row (mouse Y coord) to a visible-entry
/// index in the sidebar. Returns `None` when the row is outside the
/// list rect or past the last entry's last line. Used by the click
/// handler so a mouse-down on a session row jumps the selection
/// straight to it (same effect as pressing j/k until the cursor
/// lands).
pub fn entry_at_row(state: &AppState, list_area: Rect, abs_row: u16) -> Option<usize> {
    if abs_row < list_area.y || abs_row >= list_area.y.saturating_add(list_area.height) {
        return None;
    }
    let visible = state.sidebar.visible();
    if visible.is_empty() {
        return None;
    }
    let counts: Vec<u16> = visible.iter().map(|e| entry_line_count(state, e)).collect();
    let scroll = compute_scroll(&counts, state.selected, list_area.height);
    let local_row = abs_row - list_area.y;
    let target_line = local_row.saturating_add(scroll);
    let mut acc: u16 = 0;
    for (i, c) in counts.iter().enumerate() {
        if target_line >= acc && target_line < acc + c {
            return Some(i);
        }
        acc = acc.saturating_add(*c);
    }
    None
}

pub fn status_color(status: Status, theme: &Theme) -> Color {
    match status {
        Status::Running => theme.status_running,
        Status::Waiting => theme.status_waiting,
        Status::Done => theme.done_color(),
        Status::Idle | Status::Unknown => theme.status_idle,
        Status::Error => theme.status_error,
    }
}

fn row_bg(selected: bool, theme: &Theme) -> Color {
    if selected {
        theme.selection_bg
    } else {
        theme.panel
    }
}

fn render_section_line(
    section: &Section,
    selected: bool,
    jump_key: Option<char>,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let marker = if selected { "▌" } else { " " };
    let marker_style = if selected {
        Style::default().fg(theme.accent).bg(bg)
    } else {
        Style::default().fg(bg).bg(bg)
    };
    let title_style = Style::default()
        .fg(theme.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.text_muted).bg(bg);

    let key_prefix = match jump_key {
        Some(k) => format!("{} ", k),
        None => "  ".to_string(),
    };
    let count_label = format!("  ({})", section.members.len());
    let count_style = Style::default().fg(theme.text_muted).bg(bg);

    // Disclosure glyph follows the standard tree convention: ▸ when
    // the section is closed (members hidden), ▾ when open. An empty
    // section always shows ▸ since there's nothing to collapse.
    let glyph = if section.collapsed || section.members.is_empty() {
        "▸"
    } else {
        "▾"
    };
    let label = format!("{glyph} {}", section.name.to_uppercase());
    let used = 3 + key_prefix.chars().count() + label.chars().count() + count_label.chars().count();
    let pad = (width as usize).saturating_sub(used);

    Line::from(vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(key_prefix, key_style),
        Span::styled(label, title_style),
        Span::styled(count_label, count_style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

/// True iff at least one **background** tab (anything other than
/// `container.active`) is currently Running or Waiting — i.e. a
/// tab the user can't see is doing work right now. Used to render
/// a small accent dot on the sidebar row so multi-tab containers
/// surface background activity without the user having to cycle
/// through tabs.
fn background_activity(state: &AppState, container: &crate::sidebar::Container) -> bool {
    use crate::tmux::detector::Status;
    container
        .members
        .iter()
        .filter(|m| *m != &container.active)
        .filter_map(|m| state.session_by_name(m))
        .any(|v| matches!(v.status, Status::Running | Status::Waiting))
}

/// True iff any tab in `container` has unviewed changes — its pane
/// differs from what the user last saw. Aggregated across all tabs
/// (not just the active one) so a multi-tab container lights up even
/// when the changed tab isn't the one on top. The currently-viewed
/// session is kept baselined by `AppState::sync_focus`, so this never
/// lights the row under the cursor.
fn container_unread(state: &AppState, container: &crate::sidebar::Container) -> bool {
    container.members.iter().any(|m| state.session_unread(m))
}

#[allow(clippy::too_many_arguments)]
fn render_primary_line(
    view: &SessionView,
    status: Status,
    selected: bool,
    indented: bool,
    tabs: u16,
    bg_busy: bool,
    unread: bool,
    op: Option<OpKind>,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);

    // Left gutter: the selection bar wins when this row is selected;
    // otherwise an unread session shows a notification dot there.
    // These never coincide — selecting a row clears its unread flag —
    // so reusing the one column keeps every row's columns aligned.
    // The dot uses the error/red slot, not the (yellow) waiting slot,
    // so it never reads as the in-progress Waiting glyph.
    let (marker, marker_style) = if selected {
        ("▌", Style::default().fg(theme.accent).bg(bg))
    } else if unread {
        ("●", Style::default().fg(theme.status_error).bg(bg))
    } else {
        (" ", Style::default().fg(bg).bg(bg))
    };
    // Bold the name for the selected row and for unread rows — the
    // dot alone is easy to miss, and bold reads as "new" even on
    // terminals where the accent color is muted.
    let name_style = if selected || unread {
        Style::default()
            .fg(theme.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(bg)
    };
    // An in-progress op (issue #7) takes over the status column: a
    // working marker in the waiting color replaces the detector glyph,
    // and a present-progressive label ("killing…") trails the row.
    let status_style = match op {
        Some(_) => Style::default().fg(theme.status_waiting).bg(bg),
        None => Style::default().fg(status_color(status, theme)).bg(bg),
    };

    let glyph = match op {
        Some(_) => "⟳".to_string(),
        None => status.glyph().to_string(),
    };
    let op_label = match op {
        Some(k) => format!("  {}…", k.label()),
        None => String::new(),
    };
    let name = view.display().to_string();
    // `(N)` tab-count badge for multi-tab containers. Hidden when
    // tabs <= 1 so single-tab rows render identically to the
    // pre-tabs sidebar.
    let tab_label = if tabs > 1 {
        format!("  ({})", tabs)
    } else {
        String::new()
    };
    // Single accent dot when a non-active tab is busy. Same row,
    // right after the (N) badge, so the user can tell at a glance
    // whether a background tab is doing work.
    let activity_label = if tabs > 1 && bg_busy { " ●" } else { "" };
    let windows_label = format!("  {}w", view.session.windows);
    let attached_label = if view.session.attached {
        "  •attached"
    } else {
        ""
    };
    let indent = if indented { "  " } else { "" };

    let used = 3
        + indent.chars().count()
        + glyph.chars().count()
        + 2
        + name.chars().count()
        + tab_label.chars().count()
        + activity_label.chars().count()
        + windows_label.chars().count()
        + attached_label.chars().count()
        + op_label.chars().count();
    let pad = (width as usize).saturating_sub(used);

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(indent, Style::default().bg(bg)),
        Span::styled(glyph, status_style),
        Span::styled("  ", Style::default().bg(bg)),
        Span::styled(name, name_style),
    ];
    if !tab_label.is_empty() {
        spans.push(Span::styled(
            tab_label,
            Style::default().fg(theme.text_muted).bg(bg),
        ));
    }
    if !activity_label.is_empty() {
        spans.push(Span::styled(
            activity_label,
            Style::default().fg(theme.accent).bg(bg),
        ));
    }
    spans.push(Span::styled(
        windows_label,
        Style::default().fg(theme.text_muted).bg(bg),
    ));
    if view.session.attached {
        spans.push(Span::styled(
            attached_label,
            Style::default().fg(theme.status_running).bg(bg),
        ));
    }
    if !op_label.is_empty() {
        spans.push(Span::styled(
            op_label,
            Style::default().fg(theme.status_waiting).bg(bg),
        ));
    }
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));

    Line::from(spans)
}

fn render_meta_line(
    view: &SessionView,
    selected: bool,
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let meta_style = Style::default().fg(theme.text_muted).bg(bg);

    // Base indent aligns under the session name ("  ▌  ○  " = 3+1+2=6 cols
    // of leading padding after the marker); indented rows add 2 more.
    let base_indent: &str = "       ";
    let extra = if indented { "  " } else { "" };

    let agent = view.session.agent.as_deref();
    let path = view.session.best_path();

    let body = match (agent, path) {
        (Some(a), Some(p)) => format!("{a} · {}", shorten_path(p)),
        (Some(a), None) => a.to_string(),
        (None, Some(p)) => shorten_path(p),
        (None, None) => String::new(),
    };

    let lead = base_indent.chars().count() + extra.chars().count();
    let max_body = (width as usize).saturating_sub(lead).saturating_sub(1);
    let body = truncate_to(&body, max_body);

    let used = lead + body.chars().count();
    let pad = (width as usize).saturating_sub(used);

    Line::from(vec![
        Span::styled(base_indent, Style::default().bg(bg)),
        Span::styled(extra, Style::default().bg(bg)),
        Span::styled(body, meta_style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

/// Narrow-mode tab listing under a multi-tab container row. The
/// preview pane (and its tab strip) is hidden when the terminal is
/// below `PREVIEW_MIN_WIDTH`, so without this line a user on mobile
/// can't tell what tabs a container actually holds. Active tab gets
/// the accent background so it stands out at a glance.
fn render_tabs_line(
    container: &Container,
    state: &AppState,
    selected: bool,
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let muted = Style::default().fg(theme.text_muted).bg(bg);
    let active_style = Style::default().fg(theme.on(theme.accent)).bg(theme.accent);

    let base_indent: &str = "       ";
    let extra = if indented { "  " } else { "" };
    let lead = base_indent.chars().count() + extra.chars().count();
    let max_body = (width as usize).saturating_sub(lead).saturating_sub(1);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(container.members.len() * 2 + 2);
    spans.push(Span::styled(base_indent, Style::default().bg(bg)));
    spans.push(Span::styled(extra, Style::default().bg(bg)));

    let mut used = 0usize;
    let names: Vec<(String, bool)> = container
        .members
        .iter()
        .map(|m| {
            let name = state
                .session_by_name(m)
                .map(|v| v.display().to_string())
                .unwrap_or_else(|| m.clone());
            (name, m == &container.active)
        })
        .collect();

    for (i, (name, active)) in names.iter().enumerate() {
        if i > 0 {
            let sep = " · ";
            if used + sep.chars().count() >= max_body {
                break;
            }
            spans.push(Span::styled(sep.to_string(), muted));
            used += sep.chars().count();
        }
        let remaining = max_body.saturating_sub(used);
        let label = truncate_to(name, remaining);
        let label_len = label.chars().count();
        if *active {
            spans.push(Span::styled(format!(" {label} "), active_style));
            used += label_len + 2;
        } else {
            spans.push(Span::styled(label, muted));
            used += label_len;
        }
    }

    let pad = (width as usize).saturating_sub(lead + used);
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    Line::from(spans)
}

fn render_missing_line(
    label: &str,
    selected: bool,
    indented: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let bg = row_bg(selected, theme);
    let extra = if indented { "  " } else { "" };
    // Label is the resolved display name (from Recents lookup) or
    // a slug fallback — see `AppState::dead_display_for`. `R` on this
    // row recreates the session from its persisted spec.
    let body = format!("  ? {}", label);
    let used = extra.chars().count() + body.chars().count();
    let pad = (width as usize).saturating_sub(used);
    Line::from(vec![
        Span::styled(extra, Style::default().bg(bg)),
        Span::styled(body, Style::default().fg(theme.text_muted).bg(bg)),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
    ])
}

fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

fn truncate_to(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = s.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}
