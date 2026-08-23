//! Help / key-binding reference modal.
//!
//! Opened with `?` or `h` from the main list. Renders a scrollable
//! cheat sheet of every key binding in bosun — main list, modals,
//! and the attach-session detach key — so users can discover the
//! subtle bits (Shift+arrows, `Ctrl+R`, `1`–`9` direct-jump, etc)
//! that don't fit in the status bar.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::Theme;

use super::{center_rect, Modal, ModalResult};

const MODAL_WIDTH: u16 = 78;
const MODAL_MIN_HEIGHT: u16 = 14;
/// Width of the key-name column (including its leading padding).
/// Wide enough to fit the longest binding label (`Shift+↑ / Shift+↓`).
const KEY_COL_WIDTH: usize = 22;

/// One logical row in the help body. Section headers visually break
/// up the table; bindings are key/action pairs; legend rows are the
/// same but draw their glyph in the colour it actually appears in on
/// the session list (a legend printed in plain text would be missing
/// half the information it exists to convey); blanks add spacing.
enum Row {
    Section(&'static str),
    Binding(&'static str, &'static str),
    Legend(&'static str, Tint, &'static str),
    Blank,
}

/// Which theme colour a legend glyph is drawn in. Mirrors the choices
/// made in `ui::session_list` so the legend can't drift from the rows
/// it describes.
enum Tint {
    /// Same colour the status column uses for this state.
    Status(crate::tmux::detector::Status),
    /// The unread notification dot.
    Unread,
    /// Selection bar and background-activity dot.
    Accent,
    /// Muted chrome — tab counts, the attached tag.
    Muted,
}

pub struct HelpModal {
    rows: Vec<Row>,
    /// First row index currently visible. Up/Down scroll this; the
    /// renderer clamps it against the viewport height computed each
    /// frame (the modal doesn't know the terminal height until then).
    scroll: usize,
    /// Last rendered viewport height in rows. Stashed by `render` so
    /// PgUp/PgDn and the bottom-clamp in `handle` can use it. `Cell`
    /// because `render` takes `&self`.
    viewport: std::cell::Cell<usize>,
}

impl Default for HelpModal {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpModal {
    pub fn new() -> Self {
        Self {
            rows: build_rows(),
            scroll: 0,
            viewport: std::cell::Cell::new(0),
        }
    }

    fn max_scroll(&self) -> usize {
        let vp = self.viewport.get().max(1);
        self.rows.len().saturating_sub(vp)
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        let next = (self.scroll as isize + delta).clamp(0, max);
        self.scroll = next as usize;
    }
}

impl Modal for HelpModal {
    fn id(&self) -> &'static str {
        "help"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        match key.code {
            KeyCode::Esc
            | KeyCode::Enter
            | KeyCode::Char('q')
            | KeyCode::Char('?')
            | KeyCode::Char('h')
            | KeyCode::Char('H') => ModalResult::Close(None),
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(-1);
                ModalResult::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1);
                ModalResult::Consumed
            }
            KeyCode::PageUp => {
                let step = self.viewport.get().max(1) as isize;
                self.scroll_by(-step);
                ModalResult::Consumed
            }
            KeyCode::PageDown => {
                let step = self.viewport.get().max(1) as isize;
                self.scroll_by(step);
                ModalResult::Consumed
            }
            KeyCode::Home => {
                self.scroll = 0;
                ModalResult::Consumed
            }
            KeyCode::End => {
                self.scroll = self.max_scroll();
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Take as much vertical space as the terminal will give us,
        // capped at what we actually need. Two-row chrome (top/bottom)
        // is included in `height`; the body gets `height - 2` rows.
        let needed = self.rows.len() as u16 + 6; // header + hint + spacing
        let max_h = area.height.saturating_sub(2).max(MODAL_MIN_HEIGHT);
        let height = needed.clamp(MODAL_MIN_HEIGHT, max_h);

        let rect = center_rect(area, MODAL_WIDTH, height);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        // Shadow.
        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            crate::ui::paint::tint(buf, shadow, style);
        }

        // Body fill.
        let body_style = Style::default().bg(body_bg);
        crate::ui::paint::fill_opaque(buf, rect, body_style);

        // Left accent bar.
        let accent_style = Style::default().bg(theme.accent);
        crate::ui::paint::fill_opaque(buf, crate::ui::paint::left_edge(rect), accent_style);

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        // Compute viewport: total inner height minus title row,
        // hint row, and the spacer below the title.
        let viewport = (inner.height as usize).saturating_sub(3);
        self.viewport.set(viewport);
        // Re-clamp scroll against the freshly-known viewport.
        let scroll = self.scroll.min(self.rows.len().saturating_sub(viewport));

        let has_more_above = scroll > 0;
        let has_more_below = scroll + viewport < self.rows.len();

        let title_hint = match (has_more_above, has_more_below) {
            (false, false) => "   esc close · ↑↓ scroll".to_string(),
            (true, false) => "   esc close · ↑↓ scroll  ▲ more above".to_string(),
            (false, true) => "   esc close · ↑↓ scroll  ▼ more below".to_string(),
            (true, true) => "   esc close · ↑↓ scroll  ▲▼ more".to_string(),
        };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport + 2);
        lines.push(Line::from(vec![
            Span::styled(
                "Bosun · Reference",
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title_hint,
                Style::default().fg(theme.text_muted).bg(body_bg),
            ),
        ]));
        lines.push(Line::from(""));

        for row in self.rows.iter().skip(scroll).take(viewport) {
            lines.push(render_row(row, theme, body_bg));
        }

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
            .render(inner, frame.buffer_mut());
    }
}

fn render_row(row: &Row, theme: &Theme, bg: ratatui::style::Color) -> Line<'static> {
    match row {
        Row::Section(name) => Line::from(vec![Span::styled(
            (*name).to_string(),
            Style::default()
                .fg(theme.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        )]),
        Row::Binding(keys, action) => {
            // Pad the keys column so action labels line up. Counting
            // chars (not bytes) keeps the alignment honest when the
            // key string includes arrows or other multi-byte glyphs.
            let mut key_padded = format!("  {}", keys);
            while key_padded.chars().count() < KEY_COL_WIDTH {
                key_padded.push(' ');
            }
            Line::from(vec![
                Span::styled(
                    key_padded,
                    Style::default()
                        .fg(theme.text)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    (*action).to_string(),
                    Style::default().fg(theme.text_muted).bg(bg),
                ),
            ])
        }
        Row::Legend(glyph, tint, meaning) => {
            let color = match tint {
                Tint::Status(s) => crate::ui::session_list::status_color(*s, theme),
                Tint::Unread => theme.status_error,
                Tint::Accent => theme.accent,
                Tint::Muted => theme.text_muted,
            };
            // Glyph in its real colour, then the label that names the
            // state, padded to the same column the bindings use.
            let label = format!("  {}", glyph);
            let mut name_padded = String::new();
            while label.chars().count() + name_padded.chars().count() < KEY_COL_WIDTH {
                name_padded.push(' ');
            }
            Line::from(vec![
                Span::styled(label, Style::default().fg(color).bg(bg)),
                Span::styled(name_padded, Style::default().bg(bg)),
                Span::styled(
                    (*meaning).to_string(),
                    Style::default().fg(theme.text_muted).bg(bg),
                ),
            ])
        }
        Row::Blank => Line::from(""),
    }
}

fn build_rows() -> Vec<Row> {
    use crate::tmux::detector::Status as S;
    use Row::*;
    vec![
        // Legend first: the glyphs are on screen the whole time and
        // are the one part of the UI you can't discover by pressing a
        // key. `Binding` doubles as the layout for it — glyph in the
        // left column, meaning on the right.
        Section("Status glyphs"),
        Legend(
            "●  Running",
            Tint::Status(S::Running),
            "Agent is working on a turn right now",
        ),
        Legend(
            "◐  Waiting",
            Tint::Status(S::Waiting),
            "Agent is blocked on you — a prompt or a confirmation",
        ),
        Legend(
            "◆  Done",
            Tint::Status(S::Done),
            "Finished a turn you haven't looked at yet",
        ),
        Legend(
            "○  Idle",
            Tint::Status(S::Idle),
            "Up, nothing pending — empty composer or a shell",
        ),
        Legend(
            "✕  Error",
            Tint::Status(S::Error),
            "Agent exited or the session errored out",
        ),
        Legend(
            "⟳  Working",
            Tint::Status(S::Waiting),
            "Bosun itself is busy — killing, restarting, creating",
        ),
        Blank,
        Section("Row markers"),
        Legend(
            "● gutter",
            Tint::Unread,
            "Unread — the pane changed since you last looked",
        ),
        Legend("▌ gutter", Tint::Accent, "The selected row"),
        Legend("● after (N)", Tint::Accent, "A background tab is busy"),
        Legend("(N)", Tint::Muted, "Tab count for a multi-tab container"),
        Legend(
            "•attached",
            Tint::Status(S::Running),
            "A terminal is attached to this session",
        ),
        Blank,
        Section("Navigation"),
        Binding("↑ ↓ / k j", "Move selection"),
        Binding("Enter", "Attach to selected session"),
        Binding("Double-click", "Attach to clicked session"),
        Binding("→ ← / ] [", "Cycle next / prev tab within container"),
        Binding("Tab", "Collapse / expand section (on header)"),
        Binding("/", "Quick-switch — type-ahead session picker"),
        Binding("Mouse wheel", "Scroll session list"),
        Binding("Drag divider", "Resize list / preview split"),
        Blank,
        Section("Sessions"),
        Binding("n", "New session"),
        Binding("r", "Rename session (on header: rename section)"),
        Binding(
            "R",
            "Restart — kill + recreate with same spec (r in prompt: resume agent)",
        ),
        Binding(
            "m",
            "Modify session (path, agent, flags) — applies on next R",
        ),
        Binding(
            "d",
            "Kill active tab (on header: delete section; last tab kills container)",
        ),
        Binding("Shift+D", "Kill whole container — every tab at once"),
        Binding("e", "Open session's path in configured editor"),
        Binding("Ctrl+R", "Force immediate refresh"),
        Binding(
            "Ctrl+L",
            "Redraw screen — recover from a terminal reset (e.g. Cmd+R)",
        ),
        Binding(
            "Ctrl+B",
            "Hide / show the sidebar (while focused on a session; remembered)",
        ),
        Blank,
        Section("Tabs"),
        Binding("Ctrl+T", "Add a tab to the selected container"),
        Binding(
            "→ ← / ] [ / Shift+→ ←",
            "Cycle next / prev tab within container",
        ),
        Binding(
            "Click tab / +",
            "Mouse: switch active tab or open the add-tab modal",
        ),
        Blank,
        Section("Organize"),
        Binding(
            "Ctrl+Shift+↑ ↓ / K J",
            "Reorder within section / move section block",
        ),
        Binding("Ctrl+Shift+→", "Move session to next section"),
        Binding("Ctrl+Shift+←", "Move session to previous section"),
        Binding("Shift+↑ ↓", "Cycle next / prev session in sidebar order"),
        Binding("1 – 9", "Move session to section N"),
        Binding("0", "Move session to ungrouped"),
        Binding("g", "New section"),
        Binding("f", "Cycle banner font (header: section override)"),
        Blank,
        Section("Settings"),
        Binding("s / ,", "Settings panel (←→ changes, saved as you go)"),
        Binding("t", "Theme picker (↑↓ live preview, Enter applies)"),
        Binding("? / h", "Show this help"),
        Binding("q / Ctrl+C", "Quit"),
        Blank,
        Section("Inside attached session"),
        Binding("Ctrl+Q", "Detach back to bosun"),
        Binding(
            "Shift+→ / Shift+←",
            "Cycle next / prev tab within the current container",
        ),
        Binding(
            "Shift+↓ / Shift+↑",
            "Cycle next / prev session in sidebar order",
        ),
        Binding("Option+← / Option+→", "Move cursor by word"),
        Binding("Option+Delete", "Delete previous word"),
        Binding(
            "Ctrl+B",
            "Hide / show the sidebar (remembered across sessions)",
        ),
        Binding("Ctrl+L", "Redraw — recover from a terminal reset (Cmd+R)"),
        Blank,
        Section("New-session modal"),
        Binding("Tab / Shift+Tab", "Next / previous field"),
        Binding("Ctrl+R", "Open recents picker — pre-fill from history"),
        Binding("Tab (path field)", "Filesystem completion"),
        Binding("↑ ↓ (path field)", "Navigate filesystem dropdown"),
        Binding("Esc (path field)", "Dismiss dropdown so Tab advances"),
        Binding("Space (checkbox)", "Toggle option"),
        Binding("Enter", "Create session"),
        Binding("Esc", "Cancel"),
        Blank,
        Section("Recents picker"),
        Binding("↑ ↓", "Navigate"),
        Binding("Type", "Filter by name / agent / path"),
        Binding("Enter", "Pre-fill new-session form"),
        Binding("Ctrl+D", "Delete recent entry"),
        Binding("Esc", "Close"),
        Blank,
        Section("Quick-switch (/)"),
        Binding("↑ ↓", "Navigate matches"),
        Binding("Type", "Filter"),
        Binding("Enter", "Attach to match"),
        Binding("Esc", "Cancel"),
        Blank,
        Section("Theme picker"),
        Binding("↑ ↓ / k j", "Live-preview next / previous theme"),
        Binding("Home / End", "Jump to first / last theme"),
        Binding("Enter", "Apply + persist to config.toml"),
        Binding("Esc", "Revert"),
        Blank,
        Section("Help (this dialog)"),
        Binding("↑ ↓ / k j", "Scroll one line"),
        Binding("PgUp / PgDn", "Scroll one page"),
        Binding("Home / End", "Top / bottom"),
        Binding("Esc / Enter / ? / h / q", "Close"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_closes() {
        let mut m = HelpModal::new();
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn question_mark_closes() {
        let mut m = HelpModal::new();
        assert!(matches!(
            m.handle(key(KeyCode::Char('?'))),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn h_closes() {
        let mut m = HelpModal::new();
        assert!(matches!(
            m.handle(key(KeyCode::Char('h'))),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn down_scrolls_when_viewport_known() {
        let mut m = HelpModal::new();
        // Simulate a render: pretend the viewport is 5 rows tall so
        // there's room to scroll.
        m.viewport.set(5);
        let before = m.scroll;
        m.handle(key(KeyCode::Down));
        assert_eq!(m.scroll, before + 1);
    }

    #[test]
    fn scroll_clamps_at_bottom() {
        let mut m = HelpModal::new();
        m.viewport.set(5);
        for _ in 0..1000 {
            m.handle(key(KeyCode::Down));
        }
        assert_eq!(m.scroll, m.max_scroll());
    }

    #[test]
    fn up_does_not_underflow_at_top() {
        let mut m = HelpModal::new();
        m.viewport.set(5);
        m.handle(key(KeyCode::Up));
        assert_eq!(m.scroll, 0);
    }

    #[test]
    fn pgdn_jumps_a_viewport() {
        let mut m = HelpModal::new();
        m.viewport.set(8);
        m.handle(key(KeyCode::PageDown));
        assert_eq!(m.scroll, 8.min(m.max_scroll()));
    }

    #[test]
    fn home_resets_to_top() {
        let mut m = HelpModal::new();
        m.viewport.set(5);
        m.scroll = 10;
        m.handle(key(KeyCode::Home));
        assert_eq!(m.scroll, 0);
    }

    #[test]
    fn end_goes_to_max() {
        let mut m = HelpModal::new();
        m.viewport.set(5);
        m.handle(key(KeyCode::End));
        assert_eq!(m.scroll, m.max_scroll());
    }
}
