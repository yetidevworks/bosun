//! Preview pane for section headers (and the empty-no-sessions
//! splash). Renders a TDF banner via `ui::banner` plus a per-section
//! summary table — replacing the stale "preview: capturing…" message
//! that section headers used to show.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::sidebar::Section;
use crate::tmux::session::SessionView;
use crate::ui::banner;
use crate::ui::Theme;

/// Render the preview pane for a section header. Paints the TDF
/// banner of the section name at the top, the active font name
/// underneath, then a horizontal rule and a per-session table of
/// the section's members.
pub fn render_section(
    buf: &mut Buffer,
    area: Rect,
    section: &Section,
    members: &[&SessionView],
    font_name: &str,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let banner_height = banner::paint(buf, area, &section.name, font_name, theme);
    paint_caption_and_table(
        buf,
        area,
        banner_height,
        font_name,
        members,
        section.members.len(),
        theme,
    );
}

/// Render the empty/no-sessions splash: a banner of "Bosun" plus the
/// crate version and a hint about creating sessions.
pub fn render_empty(buf: &mut Buffer, area: Rect, font_name: &str, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let banner_height = banner::paint(buf, area, "Bosun", font_name, theme);
    let lpad = "   ";
    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            format!(
                "{lpad}v{} · TDF banner: {}",
                env!("CARGO_PKG_VERSION"),
                font_name
            ),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{lpad}press n to create a session"),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            format!("{lpad}press f to cycle banner fonts"),
            Style::default().fg(theme.text_muted),
        )),
    ];
    let body = Rect::new(
        area.x,
        area.y.saturating_add(banner_height),
        area.width,
        area.height.saturating_sub(banner_height),
    );
    if body.height == 0 {
        return;
    }
    Paragraph::new(lines).render(body, buf);
}

fn paint_caption_and_table(
    buf: &mut Buffer,
    area: Rect,
    banner_height: u16,
    font_name: &str,
    members: &[&SessionView],
    total_members: usize,
    theme: &Theme,
) {
    let body_y = area.y.saturating_add(banner_height);
    let body_h = area.height.saturating_sub(banner_height);
    if body_h == 0 {
        return;
    }
    let body = Rect::new(area.x, body_y, area.width, body_h);

    // Left/right indent for everything below the banner. Matches the
    // banner's PAD_LEFT (3) so the caption and table align under the
    // glyph block. Right margin matches PAD_RIGHT (2).
    let lpad = "   "; // 3 spaces
    let rpad: usize = 2;

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body_h as usize);
    lines.push(Line::from(Span::styled(
        format!(
            "{lpad}font: {} · f to cycle · {} session{}",
            font_name,
            total_members,
            if total_members == 1 { "" } else { "s" }
        ),
        Style::default().fg(theme.text_muted),
    )));
    let rule_len = (area.width as usize)
        .saturating_sub(lpad.len())
        .saturating_sub(rpad);
    lines.push(Line::from(Span::styled(
        format!("{lpad}{}", "─".repeat(rule_len)),
        Style::default().fg(theme.text_muted),
    )));

    if members.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{lpad}(no sessions in this group yet)"),
            Style::default().fg(theme.text_muted),
        )));
    } else {
        for v in members {
            for line in session_rows(v, area.width, theme, lpad) {
                lines.push(line);
            }
        }
    }

    Paragraph::new(lines).render(body, buf);
}

fn session_rows(view: &SessionView, width: u16, theme: &Theme, lpad: &str) -> [Line<'static>; 2] {
    let glyph = view.status.glyph().to_string();
    let glyph_color = crate::ui::session_list::status_color(view.status, theme);
    let name = view.display().to_string();
    let attached = if view.session.attached {
        "  •attached"
    } else {
        ""
    };
    let agent = view
        .session
        .agent
        .clone()
        .unwrap_or_else(|| "—".to_string());
    let age = view
        .session
        .created
        .map(|t| fmt_age(SystemTime::now(), t))
        .unwrap_or_default();

    let primary = Line::from(vec![
        Span::styled(lpad.to_string(), Style::default()),
        Span::styled(glyph, Style::default().fg(glyph_color)),
        Span::styled("  ", Style::default()),
        Span::styled(
            name,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("   {agent}"), Style::default().fg(theme.text_muted)),
        Span::styled(
            if age.is_empty() {
                String::new()
            } else {
                format!("   {age}")
            },
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(attached, Style::default().fg(theme.status_running)),
    ]);

    let path = view
        .session
        .best_path()
        .map(shorten_path)
        .unwrap_or_default();
    // Meta indent: lpad + glyph(1) + 2 spaces + 2 leading spaces under
    // the name = lpad.len() + 5. Keep the path aligned under the name.
    let meta_indent = " ".repeat(lpad.chars().count() + 5);
    let max_path = (width as usize).saturating_sub(meta_indent.chars().count() + 1);
    let path = truncate_to(&path, max_path);
    let meta = Line::from(vec![
        Span::styled(meta_indent, Style::default()),
        Span::styled(path, Style::default().fg(theme.text_muted)),
    ]);

    [primary, meta]
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

/// Coarse "how long ago" string. Mirrors the resolution typically
/// shown by tmux UIs: seconds → minutes → hours → days → weeks.
fn fmt_age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}w", secs / 604_800)
    }
}

#[allow(dead_code)]
fn _unused_color_ref(_c: Color) {}
