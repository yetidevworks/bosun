//! Browser-style tab strip rendered above the embedded terminal
//! whenever the selected sidebar row is a container. Single-tab
//! containers show the strip too so the `+` add-tab button is
//! always reachable without having to first create a second tab.
//!
//! The layout is computed by [`compute`] (a pure function over the
//! preview rect, the container, and per-tab `SessionView`s); the
//! result is used by both [`render`] and the mouse-click path in
//! `app.rs` so the click hit-test always matches what was drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::sidebar::Container;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;
use crate::ui::Theme;

/// Padding spaces around each tab's `<glyph> <label>` core.
const TAB_PAD: u16 = 1;
/// On-screen text for the add-tab button. Three columns wide so
/// it's easy to click and visually separates from the last tab.
const PLUS_LABEL: &str = " + ";

/// One clickable region in the tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Tmux internal name of the tab, or the literal `"+"` for
    /// the add-tab button.
    pub key: String,
    pub rect: Rect,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub tabs: Vec<Slot>,
    pub plus: Option<Slot>,
    /// Index (into the caller's `tab_labels` slice) of the leftmost
    /// visible tab. Useful for renderers that need to map slot
    /// positions back to original tab data, and for click handlers
    /// that need to resolve a slot to its internal tmux name.
    pub first_visible: usize,
    /// Index (exclusive) one past the rightmost visible tab.
    pub last_visible: usize,
}

impl Layout {
    /// Resolve a click on `(col, row)` to the slot under the
    /// pointer, if any. Plus button takes precedence (the layout
    /// never makes a tab overlap it).
    pub fn hit(&self, col: u16, row: u16) -> Option<&Slot> {
        if let Some(p) = &self.plus {
            if contains(p.rect, col, row) {
                return Some(p);
            }
        }
        self.tabs.iter().find(|t| contains(t.rect, col, row))
    }
}

fn contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

/// Compute the on-screen rectangles for each tab pill and the
/// add-tab `+` button. Tabs are laid out left-to-right starting at
/// `area.x`; the `+` button sits immediately right of the last
/// visible tab. Its 3 columns are still reserved out of the available
/// width so overflow can't push it off-screen.
///
/// When more tabs exist than fit, the window slides so the active
/// tab (`active_idx`) stays visible — dropped tabs come from the
/// LEFT, with the active tab landing as the rightmost-visible
/// pill. The returned `Layout.first_visible` and
/// `Layout.last_visible` index back into the caller's `tab_labels`
/// slice; the caller uses them to render the slot keys correctly.
/// `active_idx = None` falls back to "fit from the front" — the
/// original phase 2 behavior.
pub fn compute(area: Rect, tab_labels: &[&str], active_idx: Option<usize>) -> Layout {
    let mut out = Layout::default();
    if area.width == 0 || area.height == 0 || tab_labels.is_empty() {
        out.plus = plus_slot(area);
        return out;
    }
    let plus_w = PLUS_LABEL.chars().count() as u16;
    if area.width <= plus_w {
        return out;
    }
    // Reserve the `+` button's columns so overflow can't push it off
    // the right edge, but lay tabs first and place the button
    // immediately after the last visible one (below) rather than
    // pinning it to the far right.
    let available = area.width.saturating_sub(plus_w);

    let widths: Vec<u16> = tab_labels
        .iter()
        .map(|l| TAB_PAD * 2 + 2 + l.chars().count() as u16)
        .collect();

    // First pass: fit from the front. Common case (few tabs) ends here.
    let mut first = 0usize;
    let mut last_excl = 0usize;
    let mut acc: u16 = 0;
    for (i, w) in widths.iter().enumerate() {
        if acc.saturating_add(*w) > available {
            break;
        }
        acc = acc.saturating_add(*w);
        last_excl = i + 1;
    }

    // If active falls outside the visible window, slide so active
    // is the rightmost-visible tab. Earlier tabs scroll off the
    // left edge; the user can press `[` to bring them back.
    if let Some(a) = active_idx {
        if a >= last_excl {
            last_excl = a + 1;
            let mut acc2: u16 = 0;
            first = a + 1;
            for i in (0..last_excl).rev() {
                if acc2.saturating_add(widths[i]) > available {
                    break;
                }
                acc2 = acc2.saturating_add(widths[i]);
                first = i;
            }
        }
    }

    let mut x = area.x;
    for w in widths.iter().take(last_excl).skip(first) {
        out.tabs.push(Slot {
            key: String::new(),
            rect: Rect::new(x, area.y, *w, 1),
        });
        x = x.saturating_add(*w);
    }
    out.first_visible = first;
    out.last_visible = last_excl;
    // `+` button sits right after the last visible tab. The reserved
    // `plus_w` columns guarantee `x + plus_w <= area.right()`, so it
    // always fits on-screen.
    out.plus = Some(Slot {
        key: "+".to_string(),
        rect: Rect::new(x, area.y, plus_w, 1),
    });
    out
}

fn plus_slot(area: Rect) -> Option<Slot> {
    let plus_w = PLUS_LABEL.chars().count() as u16;
    if area.width < plus_w || area.height == 0 {
        return None;
    }
    // No tabs yet → the button leads the strip at the left edge
    // (there's no "last tab" to sit after).
    Some(Slot {
        key: "+".to_string(),
        rect: Rect::new(area.x, area.y, plus_w, 1),
    })
}

/// Render the tab strip into `area` and return the layout for
/// click handling. The caller supplies `tab_views` indexed in
/// container-member order; `None` for any tab whose tmux session
/// no longer exists (dead-row case — rare; the tab still renders
/// using its internal name and a `Status::Unknown` glyph).
///
/// `tab_statuses` carries the *display* status per tab (see
/// `AppState::display_status`) rather than the raw detected one, so a
/// sibling tab that finished a turn shows `◆` here for the same reason
/// its sidebar row does. Shorter than `container.members` (or absent
/// entries) falls back to the view's own status.
#[allow(clippy::too_many_arguments)]
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    container: &Container,
    tab_views: &[Option<&SessionView>],
    tab_statuses: &[Status],
    theme: &Theme,
    group: Option<&str>,
) -> Layout {
    // Resolve display label + status for each tab, then compute
    // the geometric layout in one shot. The labels are owned
    // strings so the slice can be reused for the (pure) compute
    // call below.
    let resolved: Vec<(String, Status)> = container
        .members
        .iter()
        .enumerate()
        .map(|(i, internal)| {
            let view = tab_views.get(i).and_then(|v| *v);
            let base = match view {
                Some(v) => v.display().to_string(),
                None => internal.clone(),
            };
            let label = match group {
                Some(g) => format!("{g}/{base}"),
                None => base,
            };
            let status = tab_statuses
                .get(i)
                .copied()
                .or_else(|| view.map(|v| v.status))
                .unwrap_or(Status::Unknown);
            (label, status)
        })
        .collect();
    let label_refs: Vec<&str> = resolved.iter().map(|(l, _)| l.as_str()).collect();
    let active_idx = container
        .members
        .iter()
        .position(|m| m == &container.active);
    let mut layout = compute(area, &label_refs, active_idx);

    // Paint the strip background so we never bleed onto whatever
    // was under it previously (the embed redraws every frame too,
    // but the strip lives outside the embed rect).
    let bg_style = Style::default().bg(theme.panel).fg(theme.text_muted);
    crate::ui::paint::fill_opaque(
        buf,
        ratatui::layout::Rect::new(area.x, area.y, area.width, 1),
        bg_style,
    );

    // Draw each tab pill in slot order. The visible window maps
    // `layout.tabs[i]` → `container.members[first_visible + i]`.
    for (i, slot) in layout.tabs.iter_mut().enumerate() {
        let member_idx = layout.first_visible + i;
        let internal = match container.members.get(member_idx) {
            Some(m) => m,
            None => continue,
        };
        slot.key = internal.clone();
        let (label, status) = &resolved[member_idx];
        let active = internal == &container.active;
        let style = if active {
            // Accent bg with a luminance-matched ink fg — same pill
            // recipe as the `bosun` chip in the status bar, so the
            // active tab reads as a mode/selection indicator that's
            // visually tied to the focus border. `theme.on` keeps the
            // label legible whether the theme's accent is light
            // (tokyonight blue) or dark (opencode purple).
            Style::default().bg(theme.accent).fg(theme.on(theme.accent))
        } else {
            Style::default().bg(theme.panel).fg(theme.text_muted)
        };
        let glyph = status.glyph();
        let pill = format!(" {} {} ", glyph, label);
        let mut col = slot.rect.x;
        for ch in pill.chars() {
            if col >= slot.rect.x.saturating_add(slot.rect.width) {
                break;
            }
            let cell = &mut buf[(col, slot.rect.y)];
            cell.set_char(ch);
            cell.set_style(style);
            col = col.saturating_add(1);
        }
    }

    // Draw the `+` button right after the last tab, accent glyph on a
    // slightly raised `panel_alt` fill so it reads as a little tab-
    // shaped control instead of blending into the strip background.
    if let Some(plus) = &layout.plus {
        let style = Style::default().bg(theme.panel_alt).fg(theme.accent);
        let mut col = plus.rect.x;
        for ch in PLUS_LABEL.chars() {
            if col >= plus.rect.x.saturating_add(plus.rect.width) {
                break;
            }
            let cell = &mut buf[(col, plus.rect.y)];
            cell.set_char(ch);
            cell.set_style(style);
            col = col.saturating_add(1);
        }
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_lays_tabs_left_to_right_with_plus_after_last_tab() {
        let area = Rect::new(0, 5, 40, 1);
        let layout = compute(area, &["one", "two"], Some(0));
        assert_eq!(layout.tabs.len(), 2);
        // " ● one " = 1+1+1+3+1 = 7 cols; same for "two".
        assert_eq!(layout.tabs[0].rect, Rect::new(0, 5, 7, 1));
        assert_eq!(layout.tabs[1].rect, Rect::new(7, 5, 7, 1));
        // " + " (3 cols) sits immediately right of the last tab.
        assert_eq!(layout.plus.as_ref().unwrap().rect, Rect::new(14, 5, 3, 1));
        assert_eq!(layout.first_visible, 0);
        assert_eq!(layout.last_visible, 2);
    }

    #[test]
    fn compute_truncates_overflow_keeping_plus() {
        // Five tabs at 7 cols each = 35, plus button = 3 → need
        // 38 cols. With only 25 cols, only 3 tabs fit.
        let area = Rect::new(0, 0, 25, 1);
        let layout = compute(area, &["one", "two", "thr", "fou", "fiv"], Some(0));
        assert_eq!(layout.tabs.len(), 3);
        assert!(layout.plus.is_some());
    }

    #[test]
    fn compute_scrolls_window_to_keep_active_visible() {
        // 5 tabs, only 3 fit. Active is index 4 (rightmost). The
        // window should slide so active is the rightmost-visible
        // pill and tabs 2..5 are shown.
        let area = Rect::new(0, 0, 25, 1);
        let layout = compute(area, &["one", "two", "thr", "fou", "fiv"], Some(4));
        assert_eq!(layout.first_visible, 2);
        assert_eq!(layout.last_visible, 5);
        assert_eq!(layout.tabs.len(), 3);
    }

    #[test]
    fn render_prefixes_group_when_some() {
        use crate::ui::Theme;
        let theme = Theme::default_opencode();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let con = Container::single("bosun-alpha-bbbb".into(), "alpha".into());
        // No live SessionView -> label falls back to the internal name,
        // which is fine for asserting the prefix is applied.
        let views: Vec<Option<&SessionView>> = vec![None];
        render(&mut buf, area, &con, &views, &[], &theme, Some("proj"));
        let row: String = (0..40)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(row.contains("proj/"), "expected group prefix, got: {row:?}");
    }

    #[test]
    fn render_no_prefix_when_none() {
        use crate::ui::Theme;
        let theme = Theme::default_opencode();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let con = Container::single("bosun-alpha-bbbb".into(), "alpha".into());
        let views: Vec<Option<&SessionView>> = vec![None];
        render(&mut buf, area, &con, &views, &[], &theme, None);
        let row: String = (0..40)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(!row.contains('/'), "expected no group prefix, got: {row:?}");
    }

    #[test]
    fn hit_resolves_plus_then_tabs() {
        let area = Rect::new(0, 0, 40, 1);
        let layout = compute(area, &["one", "two"], Some(0));
        // Click inside the first tab.
        let hit = layout.hit(3, 0).unwrap();
        assert_eq!(hit.rect, Rect::new(0, 0, 7, 1));
        // Click on + (now right after the two tabs: cols 14..17).
        let hit = layout.hit(15, 0).unwrap();
        assert_eq!(hit.key, "+");
        // Click in dead space to the right of the + button.
        assert!(layout.hit(20, 0).is_none());
    }
}
