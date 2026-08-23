pub mod banner;
pub mod embed_terminal;
pub mod key_encode;
pub mod layout;
pub mod modal;
pub mod mouse_encode;
pub mod paint;
pub mod preview;
pub mod section_preview;
pub mod session_list;
pub mod statusbar;
pub mod tab_strip;
pub mod theme;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::embed_terminal::EmbedTerminal;
pub use theme::Theme;

pub fn draw(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    embed: Option<&EmbedTerminal>,
    embed_focused: bool,
) {
    draw_inner(frame, state, theme, embed, embed_focused);

    // 256-color fallback. Everything above paints in 24-bit `Rgb`
    // (theme chrome + whatever the embedded pane emitted). On a
    // terminal without truecolor (Apple Terminal.app) those sequences
    // render as garbage, so we rewrite the finished frame to the
    // nearest xterm-256 palette in one pass. No-op — and never
    // called — on truecolor terminals.
    if !theme::terminal_truecolor() {
        theme::degrade_buffer_to_256(frame.buffer_mut());
    }
}

fn draw_inner(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    embed: Option<&EmbedTerminal>,
    embed_focused: bool,
) {
    let area = frame.area();
    // Collapse the sidebar when the user has hidden it (Ctrl+B) while
    // focused on the embed — the embed then owns the whole body, same
    // layout as the narrow-terminal path.
    let collapsed = state.single_window_mode && embed_focused && state.sidebar_hidden;
    let l = layout::compute(area, state.divider_x, collapsed);

    // Full-body focused mode: either a narrow terminal (mobile / mosh)
    // with no room for a split, or the user collapsed the sidebar with
    // Ctrl+B. Hand the entire body to the embed and skip the sidebar.
    // The user detaches with Ctrl-Q (or shows the sidebar again with
    // Ctrl+B), and the sidebar comes back. No focus border is drawn in
    // this layout, so the embed gets full width.
    if state.single_window_mode && embed_focused && l.preview.is_none() {
        let body = Rect::new(l.list.x, l.list.y, l.list.width, l.list.height);
        preview::render(frame, body, state, theme, embed, false);
        statusbar::render(frame, l.statusbar, state, theme);
        state.modals.render(frame, area, theme);
        return;
    }

    // In single-window mode the sidebar can pick up the focus
    // border (when the embed isn't focused). Inset the content rect
    // by one cell on every side so the border's perimeter doesn't
    // overdraw session rows — the top edge used to slice through
    // the first row's name and status glyph.
    let list_content = if state.single_window_mode {
        inset_one(l.list)
    } else {
        l.list
    };
    session_list::render(frame, list_content, state, theme);
    // Preview is hidden on narrow terminals (mobile / mosh).
    if let Some(preview_area) = l.preview {
        preview::render(frame, preview_area, state, theme, embed, true);
    }
    // Divider glyph sits between list and preview in wide mode.
    // Accent color while the user is dragging, muted otherwise so
    // it reads as a passive separator until you reach for it.
    if let Some(divider_area) = l.divider {
        render_divider(frame, divider_area, state, theme);
    }
    statusbar::render(frame, l.statusbar, state, theme);
    // Single-window mode: outline whichever pane currently has
    // keyboard focus so the user can tell whether keystrokes go to
    // bosun (list nav) or to the embedded tmux session. The unfocused
    // pane gets no border so the indicator stays unambiguous. Drawn
    // after content + before modals so it overlays the perimeter
    // cells but stays underneath any open modal.
    if state.single_window_mode {
        let active = if embed_focused {
            // The focus border surrounds the embed area. When a tab
            // strip is drawn (1 row at the top of the preview rect
            // for any container), the border starts one row lower so
            // the strip stays visible above the border — without
            // this shrink, the top edge of the focus border drew
            // straight through the tab labels.
            l.preview.map(|p| {
                if has_tabstrip(state) && p.height >= 2 {
                    Rect::new(p.x, p.y + 1, p.width, p.height - 1)
                } else {
                    p
                }
            })
        } else {
            Some(l.list)
        };
        if let Some(rect) = active {
            draw_focus_border(frame.buffer_mut(), rect, theme.accent, theme.bg);
        }
    }
    // Modals paint last so they float above everything else. The
    // stack handles dimming the background and rendering one or more
    // modals top-down.
    state.modals.render(frame, area, theme);
}

/// Inset `area` by one cell on every side. Zero-sized fallback if
/// the rect is too small to inset safely; callers always wrap a
/// larger pane so this is a guard rather than a meaningful
/// degradation.
fn inset_one(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

/// True when the cursor sits on a container (sidebar entry) — the
/// preview pane carves a 1-row tab strip off the top in that case.
/// Used by the focus-border code so the border doesn't overdraw
/// the strip, and by `App::tab_strip_height` so the embed-area
/// math stays in sync.
fn has_tabstrip(state: &AppState) -> bool {
    state
        .sidebar
        .visible()
        .get(state.selected)
        .map(|e| e.container().is_some())
        .unwrap_or(false)
}

fn draw_focus_border(buf: &mut Buffer, area: Rect, fg: Color, bg: Color) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let style = Style::default().fg(fg).bg(bg);
    let left = area.left();
    let right = area.right() - 1;
    let top = area.top();
    let bottom = area.bottom() - 1;

    for x in left..=right {
        let cell = &mut buf[(x, top)];
        cell.set_char('─');
        cell.set_style(style);
        let cell = &mut buf[(x, bottom)];
        cell.set_char('─');
        cell.set_style(style);
    }
    for y in top..=bottom {
        let cell = &mut buf[(left, y)];
        cell.set_char('│');
        cell.set_style(style);
        let cell = &mut buf[(right, y)];
        cell.set_char('│');
        cell.set_style(style);
    }
    let cell = &mut buf[(left, top)];
    cell.set_char('╭');
    cell.set_style(style);
    let cell = &mut buf[(right, top)];
    cell.set_char('╮');
    cell.set_style(style);
    let cell = &mut buf[(left, bottom)];
    cell.set_char('╰');
    cell.set_style(style);
    let cell = &mut buf[(right, bottom)];
    cell.set_char('╯');
    cell.set_style(style);
}

fn render_divider(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let fg = if state.dragging_divider {
        theme.accent
    } else {
        theme.text_muted
    };
    let style = Style::default().fg(fg).bg(theme.bg);
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        let cell = &mut buf[(area.left(), y)];
        cell.set_char('│');
        cell.set_style(style);
    }
}
