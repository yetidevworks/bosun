//! Live pane preview rendered from `tmux capture-pane -e` output via
//! `ansi-to-tui`. The preview buffer for the selected session lives in
//! `AppState`; this module is pure rendering.
//!
//! Background handling: we deliberately do NOT paint a background color
//! on the preview area. TUIs like Claude Code emit cells with "default"
//! background (Color::Reset), which ratatui treats as "leave whatever
//! was there". If we painted our own panel color underneath, the reset
//! cells would show our color and clash with the cells that *did* carry
//! an explicit background (like Claude Code's status line). By leaving
//! the area's buffer at Color::Reset, all reset cells fall through to
//! the terminal's own default and stay visually consistent with what
//! the user sees when they actually attach to the session.

use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::app::AppState;
use crate::sidebar::{Location, VisibleKind};
use crate::ui::embed_terminal::EmbedTerminal;
use crate::ui::{banner, section_preview, tab_strip, Theme};

pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    embed: Option<&EmbedTerminal>,
    // Whether the embed reserves focus-border cells. False for the
    // full-body layouts (narrow terminal, or sidebar collapsed via
    // Ctrl+B) where no border is drawn and the embed fills edge to
    // edge. Mirrors `App::embed_has_border`.
    with_border: bool,
) {
    // Reset every cell in the preview area to Color::Reset before drawing
    // so any leftover styling from a previous frame (e.g. the placeholder
    // text) doesn't bleed through into a later TUI capture.
    reset_area(frame.buffer_mut(), area);

    // Empty state (no managed sessions): paint the Bosun splash —
    // banner + version — instead of leaving the pane silent. Lets the
    // user cycle the global font even before they have any sessions.
    if state.sessions.is_empty() && state.sidebar.is_empty() {
        let font = banner::canonical(&state.banner_font);
        section_preview::render_empty(frame.buffer_mut(), area, font, theme);
        return;
    }

    // If the cursor is on a container, carve a 1-row tab strip off
    // the top of the preview rect. Single-tab containers get the
    // strip too so the `+` add-tab button is always reachable
    // without having to first create a second tab. The strip lives
    // above the focus border, so embed-area math (focus-border
    // inset) operates on the *remaining* rect.
    let mut working_area = area;
    if let Some(entry) = state.sidebar.visible().get(state.selected) {
        if let Some(container) = entry.container() {
            if area.height > 0 && area.width > 0 {
                let strip_area = Rect::new(area.x, area.y, area.width, 1);
                let tab_views: Vec<Option<&crate::tmux::session::SessionView>> = container
                    .members
                    .iter()
                    .map(|m| state.session_by_name(m))
                    .collect();
                let tab_statuses: Vec<crate::tmux::detector::Status> = tab_views
                    .iter()
                    .map(|v| match v {
                        Some(view) => state.display_status(view),
                        None => crate::tmux::detector::Status::Unknown,
                    })
                    .collect();
                // Prefix the pills with the section name only when the
                // user opted in and the container actually lives in a
                // section (ungrouped rows stay bare).
                let group = match entry {
                    crate::sidebar::VisibleEntry::Member { section, .. }
                        if state.show_group_in_title =>
                    {
                        Some(section.name.as_str())
                    }
                    _ => None,
                };
                tab_strip::render(
                    frame.buffer_mut(),
                    strip_area,
                    container,
                    &tab_views,
                    &tab_statuses,
                    theme,
                    group,
                );
                working_area = Rect::new(
                    area.x,
                    area.y + 1,
                    area.width,
                    area.height.saturating_sub(1),
                );
            }
        }
    }
    let area = working_area;

    // Section header selected: render banner + per-section table.
    // The cursor location tells us which section, regardless of how
    // many sessions live elsewhere.
    if let Some(VisibleKind::Header) = state.selected_kind() {
        if let Some(Location::Header(si)) = state.selected_location() {
            if let Some(sec) = state.sidebar.sections.get(si) {
                let font = sec
                    .banner_font
                    .as_deref()
                    .map(banner::canonical)
                    .unwrap_or_else(|| banner::canonical(&state.banner_font));
                let members: Vec<&crate::tmux::session::SessionView> = sec
                    .members
                    .iter()
                    .filter_map(|c| state.session_by_name(&c.active))
                    .collect();
                section_preview::render_section(
                    frame.buffer_mut(),
                    area,
                    sec,
                    &members,
                    font,
                    theme,
                );
                return;
            }
        }
    }

    // 2.0 fast path: if there's a live embed and its session matches
    // the cursor's selection, render the vt100 grid via tui-term.
    // The embed gets a real-time stream from the PTY, no scroll math
    // required — vt100 already maintains the right scrollback row
    // alignment inside its screen grid. Falls through to the polled
    // snapshot path for non-focused sessions, when the embed is
    // disabled, or when the spawn failed.
    //
    // Shrink by 1 cell on each side whenever single-window mode is
    // on — focused or not. When focused the focus border occupies
    // those reserved cells; when unfocused they stay blank, acting
    // as a transparent placeholder. Reserving the space in both
    // states keeps the inner app's wrap width constant across focus
    // toggles, so attaching / detaching no longer shifts every line
    // by a column (which used to reflow paragraphs and look like
    // the content was jumping). The matching PTY shrink lives in
    // `App::preview_dims` so the inner app's terminal dimensions
    // match the area we actually render into.
    if let Some(embed) = embed {
        if let Some(name) = state.selected_session_name() {
            if embed.session() == name {
                // Reserve the focus-border cells only when the caller
                // says a border is drawn (the wide sidebar + preview
                // layout). On a narrow terminal or with the sidebar
                // collapsed via Ctrl+B the embed owns the whole body
                // and no border is painted, so insetting would just
                // leave dead padding on the sides — give it the full
                // width instead. Keep in sync with `App::preview_dims`
                // / `App::embed_rect` / `App::embed_has_border`.
                let render_area = if with_border && state.single_window_mode {
                    shrink_for_focus_border(area)
                } else {
                    area
                };
                embed.render(frame.buffer_mut(), render_area);
                // Drive the outer terminal's real cursor to the inner
                // app's cursor cell instead of painting a fake one
                // (see `EmbedTerminal::render`). Skipped while a modal
                // is up so a blinking cursor doesn't show through the
                // dimmed preview behind the dialog.
                if state.modals.is_empty() {
                    if let Some(pos) = embed.cursor_position(render_area) {
                        frame.set_cursor_position(pos);
                    }
                }
                return;
            }
        }
    }

    let text: Text<'_> = if state.sessions.is_empty() {
        // Sidebar has sections but no live sessions yet — stay quiet
        // rather than saying "capturing…" which is misleading.
        placeholder("", theme)
    } else {
        match state.selected_preview() {
            Some(bytes) if !bytes.is_empty() => bytes
                .into_text()
                .unwrap_or_else(|_| placeholder("preview: (ansi parse failed)", theme)),
            _ => placeholder("preview: capturing…", theme),
        }
    };

    // Scroll so the bottom of the captured pane aligns with the bottom
    // of the preview area. The tmux pane may be taller than our preview
    // viewport, and the user always wants to see the most-recent output.
    let text_lines = text.lines.len() as u16;
    let scroll_y = text_lines.saturating_sub(area.height);

    // No wrap, no background. Lines wider than the area are clipped at
    // the right edge. Wrapping throws off the scroll math because wrapped
    // lines count as multiple visual rows and we can't tell how many
    // without measuring the rendered output.
    Paragraph::new(text)
        .scroll((scroll_y, 0))
        .render(area, frame.buffer_mut());
}

/// Inset `area` by one cell on every side. Returns a zero-sized
/// rect if the area is too small to inset safely — callers should
/// already have wider minimum-rect handling, so this is a last-line
/// guard rather than a meaningful fallback.
fn shrink_for_focus_border(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

fn reset_area(buf: &mut Buffer, area: Rect) {
    // Opaque, so leftover attributes (underline, reverse…) from the
    // previous frame's content don't shine through the fresh one —
    // `Cell::set_style` alone would leave them in place. See
    // `crate::ui::paint`.
    let reset_style = Style::default().fg(Color::Reset).bg(Color::Reset);
    crate::ui::paint::fill_opaque(buf, area, reset_style);
}

fn placeholder(msg: &str, theme: &Theme) -> Text<'static> {
    Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", msg),
            Style::default().fg(theme.text_muted),
        )),
    ])
}
