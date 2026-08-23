//! Modal dialog infrastructure.
//!
//! Modals can:
//!   * Stay open and consume input (`ModalResult::Consumed`)
//!   * Let the current key fall through to the main list (`PassThrough`)
//!   * Close, optionally emitting a Command to the tmux actor (`Close`)
//!   * Close AND hand typed data back to their parent modal on the
//!     stack via `CloseWithData` + `Modal::on_child_closed`
//!   * Push a child modal on top of themselves (`Push`) — used by
//!     the new-session modal to open the recents picker on Ctrl+R
//!
//! Design rules:
//!   * Modals own their own state (form fields, selection, etc).
//!   * Modals render pure — take `&self` + a Frame + Rect.
//!   * Parent/child data passing is explicit via `ModalData` variants
//!     so there's no `dyn Any` downcasting.

pub mod confirm;
pub mod help;
pub mod new_session;
pub mod quickjump;
pub mod recents;
pub mod rename;
pub mod section;
pub mod settings;
pub mod theme;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::events::{Command, SessionSpec};
use crate::ui::Theme;

/// Typed payloads a closing child modal can return to its parent
/// via `ModalResult::CloseWithData`. Parents implement
/// `Modal::on_child_closed` and pattern-match to absorb the data.
pub enum ModalData {
    /// A `Recent` was picked — unpack into fields for the
    /// new-session form.
    FillSessionSpec(SessionSpec),
}

/// Result of dispatching a key event to a modal.
pub enum ModalResult {
    /// Modal handled the key; keep it open and don't propagate.
    Consumed,
    /// Modal didn't care about this key; let the caller route it
    /// elsewhere (main list, etc). Rare — most modals want to eat
    /// every key while they're open so the user can't navigate the
    /// background while typing.
    #[allow(dead_code)]
    PassThrough,
    /// Close the modal. If `Some(cmd)`, the command is emitted to
    /// the tmux actor after the modal is popped.
    Close(Option<Command>),
    /// Close the modal and hand the data to the next modal on the
    /// stack (its parent) via `Modal::on_child_closed`. If there's
    /// no parent, the data is silently dropped.
    CloseWithData(ModalData),
    /// Push a new modal on top of this one. Used when one modal
    /// opens another, e.g. the new-session modal opens the recents
    /// picker on Ctrl+R.
    Push(Box<dyn Modal>),
    /// Emit a command to the tmux actor but keep the modal open.
    /// Used by the RecentsModal's `d`-to-delete handler: the delete
    /// fires, the modal refreshes its local view, and the user stays
    /// in the picker to continue browsing.
    EmitCommand(Command),
}

pub trait Modal: Send {
    /// Stable identifier for the modal kind. Useful for tests and
    /// for de-duplicating repeat opens ("don't push another
    /// new_session modal if one is already on top").
    fn id(&self) -> &'static str;
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);
    fn handle(&mut self, key: KeyEvent) -> ModalResult;
    /// Called when a child modal closes with data. Default: ignore.
    /// Parents that care override this and pattern-match on the
    /// `ModalData` variants they understand.
    fn on_child_closed(&mut self, _data: ModalData) {}
}

#[derive(Default)]
pub struct ModalStack {
    stack: Vec<Box<dyn Modal>>,
}

impl std::fmt::Debug for ModalStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModalStack")
            .field("depth", &self.stack.len())
            .field(
                "top",
                &self.stack.last().map(|m| m.id()).unwrap_or("<empty>"),
            )
            .finish()
    }
}

impl ModalStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    pub fn pop(&mut self) -> Option<Box<dyn Modal>> {
        self.stack.pop()
    }

    pub fn top_id(&self) -> Option<&'static str> {
        self.stack.last().map(|m| m.id())
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Dim the background, then render the top modal (plus any
        // stacked ones once we support that). Bottom-up so the top
        // modal paints last.
        if self.stack.is_empty() {
            return;
        }
        dim_background(frame, area, theme);
        for modal in &self.stack {
            modal.render(frame, area, theme);
        }
    }

    /// Dispatch a key to the top modal and apply the result to the
    /// stack in-place. Returns an optional Command that the caller
    /// should forward to the tmux actor (from `Close(Some(cmd))` or
    /// `EmitCommand(cmd)`), plus a `PassThrough` signal so the caller
    /// knows whether to route the key to the main list instead.
    pub fn dispatch(&mut self, key: KeyEvent) -> StackDispatch {
        if self.stack.is_empty() {
            return StackDispatch::PassThrough;
        }
        let top = self.stack.last_mut().unwrap();
        match top.handle(key) {
            ModalResult::Consumed => StackDispatch::Consumed,
            ModalResult::PassThrough => StackDispatch::PassThrough,
            ModalResult::Close(cmd) => {
                self.stack.pop();
                StackDispatch::Closed(cmd)
            }
            ModalResult::CloseWithData(data) => {
                self.stack.pop();
                if let Some(parent) = self.stack.last_mut() {
                    parent.on_child_closed(data);
                }
                StackDispatch::Consumed
            }
            ModalResult::Push(child) => {
                self.stack.push(child);
                StackDispatch::Consumed
            }
            ModalResult::EmitCommand(cmd) => StackDispatch::Emit(cmd),
        }
    }
}

/// What the app loop should do after dispatching a key into the
/// modal stack.
pub enum StackDispatch {
    /// Stack handled it; don't touch the main list.
    Consumed,
    /// No modal was open, or the top modal explicitly passed.
    PassThrough,
    /// A modal just closed; forward its optional command to the
    /// tmux actor.
    Closed(Option<Command>),
    /// A modal fired a command but stayed open (e.g. delete-recent).
    Emit(Command),
}

fn dim_background(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            // Drop foreground brightness and wash over the existing bg
            // with a muted gray. This preserves the underlying glyph
            // layout so the modal looks like it's floating above a
            // real UI rather than painted onto a blank rectangle.
            cell.set_fg(theme.dim_fg);
            cell.set_bg(theme.bg);
        }
    }
}

/// Center a `width`x`height` rect inside `outer`, clamping to outer
/// bounds. Used by modals to self-center.
pub fn center_rect(outer: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(outer.width);
    let h = height.min(outer.height);
    let x = outer.x + outer.width.saturating_sub(w) / 2;
    let y = outer.y + outer.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}
