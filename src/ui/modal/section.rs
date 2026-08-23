//! Create or rename a sidebar section header. Single text field, one
//! line of help. Submit with Enter, cancel with Esc.
//!
//! Unlike most modals, the result doesn't flow through the tmux actor
//! — sections live purely in `AppState`. The modal emits a local
//! `Command::InsertSection` or `Command::RenameSection`, which the
//! app loop intercepts alongside `SaveDivider` / `SetTheme`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::Command;
use crate::ui::Theme;

use super::{center_rect, Modal, ModalResult};

const MODAL_WIDTH: u16 = 54;
const MODAL_HEIGHT: u16 = 10;

pub struct SectionModal {
    /// `Some((id, current_name))` → rename mode; `None` → create.
    editing: Option<(String, String)>,
    name: String,
    error: Option<String>,
}

impl SectionModal {
    pub fn new_section() -> Self {
        Self {
            editing: None,
            name: String::new(),
            error: None,
        }
    }

    pub fn rename_section(id: impl Into<String>, current: impl Into<String>) -> Self {
        let current = current.into();
        Self {
            editing: Some((id.into(), current.clone())),
            name: current,
            error: None,
        }
    }

    fn is_rename(&self) -> bool {
        self.editing.is_some()
    }
}

impl Modal for SectionModal {
    fn id(&self) -> &'static str {
        "section"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Enter => {
                let trimmed = self.name.trim();
                if trimmed.is_empty() {
                    self.error = Some("name is required".to_string());
                    return ModalResult::Consumed;
                }
                if !trimmed.chars().any(|c| c.is_alphanumeric()) {
                    self.error = Some("name must contain at least one letter or digit".to_string());
                    return ModalResult::Consumed;
                }
                let cmd = match &self.editing {
                    Some((id, _)) => Command::RenameSection {
                        id: id.clone(),
                        new_name: trimmed.to_string(),
                    },
                    None => Command::InsertSection {
                        name: trimmed.to_string(),
                    },
                };
                ModalResult::Close(Some(cmd))
            }
            KeyCode::Backspace => {
                self.error = None;
                self.name.pop();
                ModalResult::Consumed
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.error = None;
                self.name.push(c);
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rect = center_rect(area, MODAL_WIDTH, MODAL_HEIGHT);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            crate::ui::paint::tint(buf, shadow, style);
        }

        let body_style = Style::default().bg(body_bg);
        crate::ui::paint::fill_opaque(buf, rect, body_style);

        let accent_style = Style::default().bg(theme.accent);
        crate::ui::paint::fill_opaque(buf, crate::ui::paint::left_edge(rect), accent_style);

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let title_style = Style::default()
            .fg(theme.text)
            .bg(body_bg)
            .add_modifier(Modifier::BOLD);

        let title = if self.is_rename() {
            "Rename section"
        } else {
            "New section"
        };

        let field_width = (inner.width as usize).saturating_sub(2);
        let mut value_padded = format!(" {}▎", self.name);
        while value_padded.chars().count() < field_width {
            value_padded.push(' ');
        }

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled(title, title_style),
                Span::styled(
                    "      esc · cancel     enter · save",
                    Style::default().fg(theme.text_muted).bg(body_bg),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " section name",
                Style::default()
                    .fg(theme.accent)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                value_padded,
                Style::default().fg(theme.text).bg(theme.selection_bg),
            )),
        ];

        if let Some(e) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" ! {}", e),
                Style::default().fg(theme.status_error).bg(body_bg),
            )));
        }

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
            .render(inner, frame.buffer_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn new_mode_submits_insert_section() {
        let mut m = SectionModal::new_section();
        for c in "Work".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::InsertSection { name })) => {
                assert_eq!(name, "Work");
            }
            _ => panic!("expected Close with InsertSection"),
        }
    }

    #[test]
    fn rename_mode_prefills_and_submits_rename() {
        let mut m = SectionModal::rename_section("sec-123", "Old");
        assert_eq!(m.name, "Old");
        m.handle(key(KeyCode::Backspace));
        m.handle(key(KeyCode::Backspace));
        m.handle(key(KeyCode::Backspace));
        for c in "New".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::RenameSection { id, new_name })) => {
                assert_eq!(id, "sec-123");
                assert_eq!(new_name, "New");
            }
            _ => panic!("expected Close with RenameSection"),
        }
    }

    #[test]
    fn empty_name_errors() {
        let mut m = SectionModal::new_section();
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
        assert!(m.error.is_some());
    }

    #[test]
    fn esc_cancels() {
        let mut m = SectionModal::new_section();
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }
}
