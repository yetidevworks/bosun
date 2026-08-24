//! Settings panel — the options that live in `config.toml`, editable
//! without leaving the TUI.
//!
//! Every row is a toggle or a cycle: there is no free-text field, so
//! the whole panel is driven by ←/→ (or space) on the highlighted row.
//! A change applies and persists immediately, the same way the theme
//! picker commits — there is no "save" button to forget to press, and
//! `esc` just closes.
//!
//! Rows deliberately left out: `session_prefix`, `tmux_socket` and the
//! `[agents]` binary overrides are install-level settings you set once
//! by editing the file, and the layout state (`sidebar`, `divider_x`,
//! `session_history`) is bookkeeping rather than preference.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::config::WorktreeLocation;
use crate::events::{Command, SettingChange};
use crate::ui::Theme;

use super::{center_rect, Modal, ModalResult};

const MODAL_WIDTH: u16 = 64;

/// One editable option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Theme,
    BannerFont,
    DefaultAgent,
    WorktreeLocation,
    SingleWindow,
    ShowGroupInTitle,
    RemoveDeadSessions,
    EmbedEnabled,
}

impl Row {
    const ALL: &'static [Row] = &[
        Row::Theme,
        Row::BannerFont,
        Row::DefaultAgent,
        Row::WorktreeLocation,
        Row::SingleWindow,
        Row::ShowGroupInTitle,
        Row::RemoveDeadSessions,
        Row::EmbedEnabled,
    ];

    fn label(self) -> &'static str {
        match self {
            Row::Theme => "theme",
            Row::BannerFont => "banner font",
            Row::DefaultAgent => "default agent",
            Row::WorktreeLocation => "worktree location",
            Row::SingleWindow => "single-window mode",
            Row::ShowGroupInTitle => "group in tab title",
            Row::RemoveDeadSessions => "remove exited sessions",
            Row::EmbedEnabled => "live preview",
        }
    }

    /// One line explaining what the option actually does, shown under
    /// the highlighted row. The panel is the only place most of these
    /// are documented outside the README.
    /// Key used to look up an environment pin for this row, if the
    /// setting has one. See `config::env_pin`.
    fn env_key(self) -> Option<&'static str> {
        match self {
            Row::Theme => Some("theme"),
            Row::DefaultAgent => Some("default_agent"),
            Row::RemoveDeadSessions => Some("remove_dead_sessions"),
            Row::ShowGroupInTitle => Some("show_group_in_title"),
            Row::EmbedEnabled => Some("embed_enabled"),
            Row::BannerFont | Row::WorktreeLocation | Row::SingleWindow => None,
        }
    }

    fn help(self) -> &'static str {
        match self {
            Row::Theme => "colors for the whole UI",
            Row::BannerFont => "figlet font for section banners",
            Row::DefaultAgent => "agent preselected when creating a session",
            Row::WorktreeLocation => "where git worktree add puts new worktrees",
            Row::SingleWindow => "attach inside bosun instead of handing over the terminal",
            Row::ShowGroupInTitle => "prefix grouped sessions as group/session",
            Row::RemoveDeadSessions => "drop a session's row when its tmux session ends",
            Row::EmbedEnabled => "live terminal in the preview pane, not polled snapshots",
        }
    }
}

/// Current values, handed in when the panel opens.
#[derive(Debug, Clone)]
pub struct SettingsValues {
    /// Settings currently pinned by an environment variable, keyed by
    /// `Row::env_key`, with the variable's name as the value. Resolved
    /// by the caller so the modal stays free of ambient state — see
    /// `config::env_pin`.
    pub pins: std::collections::HashMap<&'static str, &'static str>,
    pub theme: String,
    pub themes: Vec<String>,
    pub banner_font: String,
    pub banner_fonts: Vec<String>,
    pub default_agent: String,
    pub worktree_location: WorktreeLocation,
    pub single_window: bool,
    pub show_group_in_title: bool,
    pub remove_dead_sessions: bool,
    pub embed_enabled: bool,
}

pub struct SettingsModal {
    values: SettingsValues,
    selected: usize,
}

impl SettingsModal {
    pub fn new(values: SettingsValues) -> Self {
        Self {
            values,
            selected: 0,
        }
    }

    fn row(&self) -> Row {
        Row::ALL[self.selected.min(Row::ALL.len() - 1)]
    }

    /// The environment variable pinning `row`, if any.
    fn pinned_by(&self, row: Row) -> Option<&'static str> {
        row.env_key().and_then(|k| self.values.pins.get(k).copied())
    }

    /// Rendered value for a row.
    fn value_of(&self, row: Row) -> String {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        match row {
            Row::Theme => self.values.theme.clone(),
            Row::BannerFont => self.values.banner_font.clone(),
            Row::DefaultAgent => self.values.default_agent.clone(),
            Row::WorktreeLocation => match self.values.worktree_location {
                WorktreeLocation::Sibling => "sibling".to_string(),
                WorktreeLocation::Subdir => "subdir".to_string(),
            },
            Row::SingleWindow => on_off(self.values.single_window),
            Row::ShowGroupInTitle => on_off(self.values.show_group_in_title),
            Row::RemoveDeadSessions => on_off(self.values.remove_dead_sessions),
            Row::EmbedEnabled => on_off(self.values.embed_enabled),
        }
    }

    /// Step the highlighted row's value by `delta` (+1 / -1), update
    /// the local copy so the panel redraws with the new value, and
    /// return the change to apply. Booleans ignore the direction.
    fn cycle(&mut self, delta: i32) -> Option<SettingChange> {
        let row = self.row();
        // Changing a pinned row would write to config.toml and then be
        // overridden by the environment on the next launch — so don't
        // pretend it worked.
        if self.pinned_by(row).is_some() {
            return None;
        }
        let change = match row {
            Row::Theme => {
                let next = step(&self.values.themes, &self.values.theme, delta)?;
                self.values.theme = next.clone();
                SettingChange::Theme(next)
            }
            Row::BannerFont => {
                let next = step(&self.values.banner_fonts, &self.values.banner_font, delta)?;
                self.values.banner_font = next.clone();
                SettingChange::BannerFont(next)
            }
            Row::DefaultAgent => {
                let agents: Vec<String> = crate::config::AGENTS
                    .iter()
                    .map(|a| a.to_string())
                    .collect();
                let next = step(&agents, &self.values.default_agent, delta)?;
                self.values.default_agent = next.clone();
                SettingChange::DefaultAgent(next)
            }
            Row::WorktreeLocation => {
                let next = match self.values.worktree_location {
                    WorktreeLocation::Sibling => WorktreeLocation::Subdir,
                    WorktreeLocation::Subdir => WorktreeLocation::Sibling,
                };
                self.values.worktree_location = next;
                SettingChange::WorktreeLocation(next)
            }
            Row::SingleWindow => {
                self.values.single_window = !self.values.single_window;
                SettingChange::SingleWindow(self.values.single_window)
            }
            Row::ShowGroupInTitle => {
                self.values.show_group_in_title = !self.values.show_group_in_title;
                SettingChange::ShowGroupInTitle(self.values.show_group_in_title)
            }
            Row::RemoveDeadSessions => {
                self.values.remove_dead_sessions = !self.values.remove_dead_sessions;
                SettingChange::RemoveDeadSessions(self.values.remove_dead_sessions)
            }
            Row::EmbedEnabled => {
                self.values.embed_enabled = !self.values.embed_enabled;
                SettingChange::EmbedEnabled(self.values.embed_enabled)
            }
        };
        Some(change)
    }
}

/// Next entry after `current` in `options`, wrapping. `delta` is +1 or
/// -1. Returns `None` when there is nothing to cycle through.
fn step(options: &[String], current: &str, delta: i32) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let idx = options.iter().position(|o| o == current).unwrap_or(0);
    let len = options.len() as i32;
    let next = ((idx as i32 + delta) % len + len) % len;
    options.get(next as usize).cloned()
}

impl Modal for SettingsModal {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => ModalResult::Close(None),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                ModalResult::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < Row::ALL.len() {
                    self.selected += 1;
                }
                ModalResult::Consumed
            }
            // Changes apply as you make them — see the module note.
            KeyCode::Left | KeyCode::Char('h') => match self.cycle(-1) {
                Some(c) => ModalResult::EmitCommand(Command::ApplySetting(c)),
                None => ModalResult::Consumed,
            },
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => match self.cycle(1) {
                Some(c) => ModalResult::EmitCommand(Command::ApplySetting(c)),
                None => ModalResult::Consumed,
            },
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Rows + title, spacer, help line and its spacer.
        let height = (Row::ALL.len() as u16 + 7).clamp(12, 30);
        let rect = center_rect(area, MODAL_WIDTH, height);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            crate::ui::paint::tint(buf, shadow, Style::default().bg(theme.shadow));
        }
        crate::ui::paint::fill_opaque(buf, rect, Style::default().bg(body_bg));
        crate::ui::paint::fill_opaque(
            buf,
            crate::ui::paint::left_edge(rect),
            Style::default().bg(theme.accent),
        );

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(Row::ALL.len() + 5);
        lines.push(Line::from(vec![
            Span::styled(
                "Settings",
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "    ←→ change · esc close",
                Style::default().fg(theme.text_muted).bg(body_bg),
            ),
        ]));
        lines.push(Line::from(""));

        let label_width = Row::ALL
            .iter()
            .map(|r| r.label().chars().count())
            .max()
            .unwrap_or(0);

        // Pin markers sit in their own column so they line up rather
        // than trailing values of differing lengths.
        let value_width = Row::ALL
            .iter()
            .map(|r| self.value_of(*r).chars().count())
            .max()
            .unwrap_or(0);

        for (i, row) in Row::ALL.iter().enumerate() {
            let selected = i == self.selected;
            let marker = if selected { "▸ " } else { "  " };
            let label = format!("{:<width$}", row.label(), width = label_width);
            let value = self.value_of(*row);
            let label_style = if selected {
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_muted).bg(body_bg)
            };
            let value_style = if selected {
                Style::default().fg(theme.accent).bg(body_bg)
            } else {
                Style::default().fg(theme.text).bg(body_bg)
            };
            let pinned = self.pinned_by(*row).is_some();
            let value = if pinned {
                format!("{:<width$}", value, width = value_width)
            } else {
                value
            };
            let mut spans = vec![
                Span::styled(marker, label_style),
                Span::styled(label, label_style),
                Span::styled("   ", Style::default().bg(body_bg)),
                Span::styled(value, value_style),
            ];
            if pinned {
                // Just a marker: the variable names are long enough
                // (`BOSUN_REMOVE_DEAD_SESSIONS`) to overflow the panel,
                // so the footer names the exact one for the selected row.
                spans.push(Span::styled(
                    "   env",
                    Style::default().fg(theme.dim_fg).bg(body_bg),
                ));
            }
            lines.push(Line::from(spans));
        }

        lines.push(Line::from(""));
        let footer = match self.pinned_by(self.row()) {
            Some(var) => format!("  set by {var} — unset it to change this here"),
            None => format!("  {}", self.row().help()),
        };
        lines.push(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.dim_fg).bg(body_bg),
        )));

        Paragraph::new(lines).render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn values() -> SettingsValues {
        SettingsValues {
            pins: std::collections::HashMap::new(),
            theme: "opencode".into(),
            themes: vec!["opencode".into(), "gruvbox".into()],
            banner_font: "newsx".into(),
            banner_fonts: vec!["newsx".into(), "tdf".into()],
            default_agent: "claude".into(),
            worktree_location: WorktreeLocation::default(),
            single_window: true,
            show_group_in_title: false,
            remove_dead_sessions: false,
            embed_enabled: true,
        }
    }

    fn change(m: &mut SettingsModal, code: KeyCode) -> Option<SettingChange> {
        match m.handle(key(code)) {
            ModalResult::EmitCommand(Command::ApplySetting(c)) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn toggling_a_boolean_emits_the_new_value() {
        let mut m = SettingsModal::new(values());
        m.selected = Row::ALL
            .iter()
            .position(|r| *r == Row::RemoveDeadSessions)
            .unwrap();
        assert_eq!(
            change(&mut m, KeyCode::Right),
            Some(SettingChange::RemoveDeadSessions(true))
        );
        // The panel shows the new value straight away.
        assert_eq!(m.value_of(Row::RemoveDeadSessions), "on");
        assert_eq!(
            change(&mut m, KeyCode::Right),
            Some(SettingChange::RemoveDeadSessions(false))
        );
    }

    #[test]
    fn cycling_a_list_wraps_in_both_directions() {
        let mut m = SettingsModal::new(values());
        m.selected = Row::ALL.iter().position(|r| *r == Row::Theme).unwrap();
        assert_eq!(
            change(&mut m, KeyCode::Right),
            Some(SettingChange::Theme("gruvbox".into()))
        );
        assert_eq!(
            change(&mut m, KeyCode::Right),
            Some(SettingChange::Theme("opencode".into())),
            "wraps past the end"
        );
        assert_eq!(
            change(&mut m, KeyCode::Left),
            Some(SettingChange::Theme("gruvbox".into())),
            "wraps backwards past the start"
        );
    }

    #[test]
    fn every_agent_can_be_chosen_as_the_default() {
        let mut m = SettingsModal::new(values());
        m.selected = Row::ALL
            .iter()
            .position(|r| *r == Row::DefaultAgent)
            .unwrap();
        let mut seen = vec![m.value_of(Row::DefaultAgent)];
        for _ in 1..crate::config::AGENTS.len() {
            change(&mut m, KeyCode::Right);
            seen.push(m.value_of(Row::DefaultAgent));
        }
        for agent in crate::config::AGENTS {
            assert!(seen.contains(&agent.to_string()), "{agent} unreachable");
        }
    }

    /// Issue #15: a setting pinned by an environment variable would be
    /// written to config.toml and then overridden again on the next
    /// launch, so the panel refuses to change it rather than pretending.
    #[test]
    fn a_pinned_row_cannot_be_changed() {
        let mut v = values();
        v.pins.insert("default_agent", "BOSUN_DEFAULT_AGENT");
        let mut m = SettingsModal::new(v);
        m.selected = Row::ALL
            .iter()
            .position(|r| *r == Row::DefaultAgent)
            .unwrap();

        assert_eq!(change(&mut m, KeyCode::Right), None, "no change is emitted");
        assert_eq!(
            m.value_of(Row::DefaultAgent),
            "claude",
            "and the shown value stays put"
        );

        // An unpinned row on the same panel still works.
        m.selected = Row::ALL
            .iter()
            .position(|r| *r == Row::RemoveDeadSessions)
            .unwrap();
        assert_eq!(
            change(&mut m, KeyCode::Right),
            Some(SettingChange::RemoveDeadSessions(true))
        );
    }

    #[test]
    fn navigation_stays_inside_the_list() {
        let mut m = SettingsModal::new(values());
        m.handle(key(KeyCode::Up));
        assert_eq!(m.selected, 0, "up on the first row stays put");
        for _ in 0..Row::ALL.len() * 2 {
            m.handle(key(KeyCode::Down));
        }
        assert_eq!(m.selected, Row::ALL.len() - 1, "down stops on the last row");
    }

    #[test]
    fn esc_and_enter_close_without_further_changes() {
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            let mut m = SettingsModal::new(values());
            assert!(
                matches!(m.handle(key(code)), ModalResult::Close(None)),
                "{code:?} should close cleanly — changes already applied as they were made"
            );
        }
    }
}
