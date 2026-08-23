//! Low-level buffer painting helpers.
//!
//! These exist because of one sharp edge in ratatui's `Cell::set_style`:
//! it *adds* the modifiers a style asks for and removes the ones it
//! explicitly subtracts, but it never clears the modifiers already on
//! the cell. Painting a surface with a plain `Style::default().bg(..)`
//! therefore leaves attributes from whatever was underneath — and what
//! is underneath in bosun is a live terminal pane, which is full of
//! them. A modal drawn over a Claude Code session showing underlined
//! text came out with those underlines running through its own body
//! (issue #12); the same applies to bold, italic, and reverse video.
//!
//! Anything that paints over unrelated content should go through here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Paint `rect` as an opaque surface: every cell becomes a blank
/// carrying exactly `style`, with any character attributes inherited
/// from the content underneath cleared.
pub fn fill_opaque(buf: &mut Buffer, rect: Rect, style: Style) {
    let rect = rect.intersection(buf.area);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let cell = &mut buf[(x, y)];
            // `reset` drops the symbol, colors *and* modifiers; the
            // style then applies to a genuinely clean cell.
            cell.reset();
            cell.set_char(' ');
            cell.set_style(style);
        }
    }
}

/// Recolor `rect` while leaving what's there intact — used for the
/// modal drop shadow, which darkens the content it falls across rather
/// than covering it.
///
/// Attributes are deliberately kept. 2.1.8 cleared them here as well as
/// on the opaque surfaces, which left a strip of un-styled cells under
/// the shadow: with underlined text behind a dialog you could see the
/// underlines stop at the shadow and resume past it, which reads as a
/// glitch rather than a shadow.
pub fn tint(buf: &mut Buffer, rect: Rect, style: Style) {
    let rect = rect.intersection(buf.area);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            buf[(x, y)].set_style(style);
        }
    }
}

/// A 1-column-wide slice down the left edge of `rect`, the shape every
/// modal's accent bar uses.
pub fn left_edge(rect: Rect) -> Rect {
    Rect::new(rect.x, rect.y, 1.min(rect.width), rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn underlined_buffer(w: u16, h: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        let busy = Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD | Modifier::REVERSED);
        for y in 0..h {
            for x in 0..w {
                let cell = &mut buf[(x, y)];
                cell.set_char('x');
                cell.set_style(busy);
            }
        }
        buf
    }

    #[test]
    fn fill_opaque_clears_inherited_attributes() {
        let mut buf = underlined_buffer(6, 3);
        let panel = Style::default().bg(Color::Blue);
        fill_opaque(&mut buf, Rect::new(1, 1, 3, 1), panel);

        for x in 1..4 {
            let cell = &buf[(x, 1)];
            assert_eq!(cell.symbol(), " ", "surface should be blank");
            assert_eq!(cell.bg, Color::Blue);
            assert!(
                cell.modifier.is_empty(),
                "underline/bold/reverse must not survive an opaque fill, got {:?}",
                cell.modifier
            );
        }
        // Cells outside the rect are untouched.
        assert!(buf[(0, 1)].modifier.contains(Modifier::UNDERLINED));
        assert!(buf[(4, 1)].modifier.contains(Modifier::UNDERLINED));
    }

    /// The shadow darkens content rather than covering it, so both the
    /// characters and their attributes stay — clearing the attributes
    /// made underlined text visibly break where the shadow crossed it.
    #[test]
    fn tint_recolors_but_keeps_content_and_attributes() {
        let mut buf = underlined_buffer(4, 1);
        tint(
            &mut buf,
            Rect::new(0, 0, 2, 1),
            Style::default().bg(Color::Black),
        );

        assert_eq!(buf[(0, 0)].symbol(), "x", "tint should not erase content");
        assert_eq!(buf[(0, 0)].bg, Color::Black);
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::UNDERLINED),
            "an underline should carry on through the shadow"
        );
        assert!(buf[(3, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn painting_is_clipped_to_the_buffer() {
        let mut buf = underlined_buffer(3, 2);
        // Rect extending past both edges must not panic.
        fill_opaque(&mut buf, Rect::new(2, 1, 40, 40), Style::default());
        tint(&mut buf, Rect::new(0, 0, 40, 40), Style::default());
        assert!(
            buf[(2, 1)].modifier.is_empty(),
            "the opaque fill still clears"
        );
    }

    #[test]
    fn left_edge_is_one_column_and_survives_a_zero_width_rect() {
        assert_eq!(left_edge(Rect::new(4, 2, 10, 5)), Rect::new(4, 2, 1, 5));
        assert_eq!(left_edge(Rect::new(4, 2, 0, 5)), Rect::new(4, 2, 0, 5));
    }
}
