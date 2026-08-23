//! Generic yes/no confirmation modal. Takes a message and a Command
//! that fires if the user confirms (Enter or 'y'). Esc or 'n' cancels.
//!
//! Up to two extra actions (`with_alt`) bind more keys to their own
//! Commands — used by the restart modal to offer `r · resume` alongside
//! the plain restart, and by the kill-cleanup flow to offer `m` / `x`
//! alongside the default keep, mirroring the existing `y`/`n` keys.

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
/// Wider when one alt action is present so the three-action footer
/// (`enter / y · … r · … esc / n · …`) fits on one line.
const MODAL_WIDTH_ALT: u16 = 64;
/// Wider still when two alt actions are present so the four-action
/// footer fits on one line. Budget: the kill-cleanup footer
/// (` enter / y · confirm   m · merge & remove   x · remove, keep
/// branch   esc / n · cancel`) is 86 display columns; the usable text
/// column is `width - H_PAD`, so 86 + 4 = 90.
const MODAL_WIDTH_ALT2: u16 = 90;
/// Horizontal padding consumed by the accent bar + insets, subtracted
/// from the modal width to get the usable text column for the message.
const H_PAD: u16 = 4;

/// Greedy word-wrap into lines no wider than `width` columns. Used so a
/// long confirmation message flows onto extra lines instead of being
/// clipped. A single word longer than `width` is left on its own
/// (over-long) line rather than hard-split — fine for our short
/// messages. Always returns at least one line.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// An extra action bound to a single key, rendered in the footer
/// between the confirm and cancel hints. Up to two may be present.
struct AltAction {
    key: char,
    label: String,
    /// `Option` so it can be `.take()`n on close — `Command` isn't Clone.
    command: Option<Command>,
}

pub struct ConfirmModal {
    title: String,
    message: String,
    /// Wrapped in Option so we can `.take()` it on close — `Command`
    /// isn't Clone and we need to move it out of `&mut self`.
    on_yes: Option<Command>,
    /// If true, the accent color shifts to red to signal a destructive
    /// action (kill, delete).
    destructive: bool,
    /// Extra key-bound actions (e.g. `r · resume`), up to two.
    alts: Vec<AltAction>,
}

impl ConfirmModal {
    pub fn new(title: impl Into<String>, message: impl Into<String>, on_yes: Command) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            on_yes: Some(on_yes),
            destructive: false,
            alts: Vec::new(),
        }
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Bind an extra action to `key` (matched case-insensitively),
    /// labelled `label`, firing `command` on press. The footer shows
    /// it as `{key} · {label}`. At most two alts are supported.
    pub fn with_alt(mut self, key: char, label: impl Into<String>, command: Command) -> Self {
        self.alts.push(AltAction {
            key: key.to_ascii_lowercase(),
            label: label.into(),
            command: Some(command),
        });
        debug_assert!(self.alts.len() <= 2);
        self
    }

    /// Number of extra key-bound actions currently registered. Test-only
    /// accessor — the modal's behavior is otherwise observed through `handle`.
    #[cfg(test)]
    pub fn alt_count(&self) -> usize {
        self.alts.len()
    }

    fn width(&self) -> u16 {
        match self.alts.len() {
            0 => MODAL_WIDTH,
            1 => MODAL_WIDTH_ALT,
            _ => MODAL_WIDTH_ALT2,
        }
    }

    /// The message wrapped to the usable text width.
    fn message_lines(&self) -> Vec<String> {
        wrap_text(&self.message, self.width().saturating_sub(H_PAD) as usize)
    }

    /// The single-line footer hint string, rendered between the modal
    /// body and its bottom edge: the primary confirm hint, then one
    /// `{key} · {label}` per alt, then the cancel hint. `width()` is
    /// sized so this stays within the usable text column (`width - H_PAD`).
    fn footer(&self) -> String {
        if self.alts.is_empty() {
            " enter / y · confirm      esc / n · cancel".to_string()
        } else {
            let mut footer = String::from(" enter / y · confirm");
            for alt in &self.alts {
                footer.push_str(&format!("   {} · {}", alt.key, alt.label));
            }
            footer.push_str("   esc / n · cancel");
            footer
        }
    }

    /// Total modal height. The body is title + blank + N message lines +
    /// blank + footer (4 + N), plus the top/bottom border rows and two
    /// rows of trailing padding — so a one-line message keeps the
    /// original 9-row look and longer messages grow downward.
    fn height(&self) -> u16 {
        8 + self.message_lines().len() as u16
    }
}

impl Modal for ConfirmModal {
    fn id(&self) -> &'static str {
        "confirm"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        if let KeyCode::Char(c) = key.code {
            let c = c.to_ascii_lowercase();
            for alt in self.alts.iter_mut() {
                if c == alt.key {
                    return ModalResult::Close(alt.command.take());
                }
            }
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => ModalResult::Close(None),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                ModalResult::Close(self.on_yes.take())
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rect = center_rect(area, self.width(), self.height());
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

        let accent_color = if self.destructive {
            theme.status_error
        } else {
            theme.accent
        };
        let accent_style = Style::default().bg(accent_color);
        crate::ui::paint::fill_opaque(buf, crate::ui::paint::left_edge(rect), accent_style);

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let title_style = Style::default()
            .fg(if self.destructive {
                theme.status_error
            } else {
                theme.text
            })
            .bg(body_bg)
            .add_modifier(Modifier::BOLD);

        let footer = self.footer();

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(self.title.clone(), title_style)));
        lines.push(Line::from(""));
        for msg_line in self.message_lines() {
            lines.push(Line::from(Span::styled(
                msg_line,
                Style::default().fg(theme.text).bg(body_bg),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.text_muted).bg(body_bg),
        )));

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
    fn enter_closes_with_command() {
        let mut m = ConfirmModal::new("Kill?", "Are you sure?", Command::KillSession("foo".into()));
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::KillSession(name))) => assert_eq!(name, "foo"),
            _ => panic!("expected Close with KillSession"),
        }
    }

    #[test]
    fn y_also_confirms() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("bar".into()));
        match m.handle(key(KeyCode::Char('y'))) {
            ModalResult::Close(Some(Command::KillSession(name))) => assert_eq!(name, "bar"),
            _ => panic!("expected Close on y"),
        }
    }

    #[test]
    fn esc_cancels_without_command() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn n_also_cancels() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Char('n'))),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn wrap_splits_on_word_boundaries() {
        let lines = wrap_text("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
        assert!(lines.len() > 1);
    }

    #[test]
    fn wrap_short_message_is_one_line() {
        assert_eq!(wrap_text("are you sure?", 54), vec!["are you sure?"]);
    }

    #[test]
    fn wrap_empty_yields_one_empty_line() {
        assert_eq!(wrap_text("", 54), vec![String::new()]);
    }

    #[test]
    fn long_message_grows_modal_height() {
        let short = ConfirmModal::new("t", "short", Command::KillSession("x".into()));
        let long = ConfirmModal::new(
            "t",
            "this is a considerably longer confirmation message that must wrap \
             across several lines inside the modal body without being clipped",
            Command::KillSession("x".into()),
        );
        assert!(long.height() > short.height());
    }

    #[test]
    fn other_keys_consumed() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into()));
        assert!(matches!(
            m.handle(key(KeyCode::Char('z'))),
            ModalResult::Consumed
        ));
    }

    #[test]
    fn alt_key_fires_its_command() {
        let mut m = ConfirmModal::new(
            "Restart?",
            "",
            Command::RestartSession {
                internal: "foo".into(),
                continue_session: false,
            },
        )
        .with_alt(
            'r',
            "resume",
            Command::RestartSession {
                internal: "foo".into(),
                continue_session: true,
            },
        );
        match m.handle(key(KeyCode::Char('r'))) {
            ModalResult::Close(Some(Command::RestartSession {
                continue_session, ..
            })) => assert!(continue_session),
            _ => panic!("expected Close with continuing RestartSession"),
        }
    }

    #[test]
    fn alt_key_is_case_insensitive() {
        let mut m = ConfirmModal::new("", "", Command::KillSession("x".into())).with_alt(
            'r',
            "resume",
            Command::KillSession("alt".into()),
        );
        match m.handle(key(KeyCode::Char('R'))) {
            ModalResult::Close(Some(Command::KillSession(name))) => assert_eq!(name, "alt"),
            _ => panic!("expected Close with alt command on uppercase key"),
        }
    }

    #[test]
    fn enter_still_fires_primary_with_alt_present() {
        let mut m = ConfirmModal::new(
            "",
            "",
            Command::RestartSession {
                internal: "foo".into(),
                continue_session: false,
            },
        )
        .with_alt('r', "resume", Command::KillSession("alt".into()));
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::RestartSession {
                continue_session, ..
            })) => assert!(!continue_session),
            _ => panic!("expected primary RestartSession on Enter"),
        }
    }

    #[test]
    fn two_alts_bind_three_keys() {
        let m = ConfirmModal::new("Kill?", "msg", Command::KillSession("x".into()))
            .with_alt('m', "merge & remove", Command::KillSession("m".into()))
            .with_alt('x', "remove", Command::KillSession("r".into()));
        assert_eq!(m.alt_count(), 2);
    }

    #[test]
    fn pressing_second_alt_fires_its_command() {
        let mut m = ConfirmModal::new("Kill?", "msg", Command::KillSession("x".into()))
            .with_alt('m', "merge", Command::KillSession("MERGE".into()))
            .with_alt('x', "remove", Command::KillSession("REMOVE".into()));
        match m.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)) {
            ModalResult::Close(Some(Command::KillSession(n))) => assert_eq!(n, "REMOVE"),
            _ => panic!("expected the x-alt command"),
        }
    }

    #[test]
    fn two_alt_footer_fits_within_width() {
        // Locks down MODAL_WIDTH_ALT2 against Task 7's kill-cleanup labels:
        // the rendered footer must fit the usable text column (width - H_PAD)
        // since the footer Paragraph does not wrap and would otherwise clip.
        let m = ConfirmModal::new("Kill?", "msg", Command::KillSession("keep".into()))
            .with_alt('m', "merge & remove", Command::KillSession("merge".into()))
            .with_alt(
                'x',
                "remove, keep branch",
                Command::KillSession("remove".into()),
            );
        assert_eq!(m.width(), MODAL_WIDTH_ALT2);
        assert_eq!(MODAL_WIDTH_ALT2, 90);
        let usable = m.width().saturating_sub(H_PAD) as usize;
        let footer_cols = m.footer().chars().count();
        assert!(
            footer_cols <= usable,
            "footer is {footer_cols} cols but only {usable} are usable at width {}",
            m.width()
        );
    }
}
