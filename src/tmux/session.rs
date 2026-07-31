use std::sync::Arc;
use std::time::SystemTime;

use crate::tmux::detector::Status;

/// A tmux session as observed by Bosun. This is NOT our authoritative
/// source of truth for persistence — tmux itself is. We rebuild these
/// structs on every poll from `tmux list-sessions -F ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    /// Actual tmux session name — includes prefix + uniq suffix for
    /// bosun-managed sessions, e.g. `bosun-rasterfox-a1b2c3d4`. This
    /// is what we pass to `tmux attach-session -t`, `capture-pane -t`,
    /// etc.
    pub name: String,
    /// Pretty name shown in bosun's UI. Populated from the tmux user
    /// option `@bosun_display` that we set at create time. `None` for
    /// sessions bosun didn't create (or ones set by an older bosun).
    pub display_name: Option<String>,
    pub windows: u32,
    pub attached: bool,
    pub created: Option<SystemTime>,
    pub last_activity: Option<SystemTime>,
    pub current_path: Option<String>,
    /// Agent name stored in `@bosun_agent` at create time
    /// (`claude` / `codex` / `terminal`). `None` for non-bosun
    /// sessions or sessions from an older bosun that didn't persist
    /// the user option.
    pub agent: Option<String>,
    /// Original spec path stored in `@bosun_path` at create time.
    /// This is the path the user typed into the new-session modal,
    /// which is more stable than `current_path` (which tracks the
    /// shell's cwd and can drift). `None` for non-bosun sessions.
    pub spec_path: Option<String>,
    /// Container ID stored in `@bosun_container_id` at create time.
    /// Identifies which sidebar container this tmux session belongs
    /// to ("tab" semantics — multiple sessions sharing one sidebar
    /// row). `None` for non-bosun sessions and for older bosun
    /// sessions from before the container feature shipped; those
    /// reconcile into their own fresh single-tab containers.
    pub container_id: Option<String>,
    /// Absolute path of the git worktree backing this session, stored
    /// in `@bosun_worktree_path` at create time. `None` for sessions
    /// not created in a worktree.
    pub worktree_path: Option<String>,
    /// Git branch checked out in the worktree, stored in `@bosun_branch`.
    /// `None` for non-worktree sessions.
    pub branch: Option<String>,
    /// Width (columns) of the session's active pane on this poll, from
    /// `#{pane_width}`. Used by the unread tracker to distinguish a real
    /// content change from a reflow: a different width means the pane
    /// was resized (terminal resize, the focus-embed sizing it to the
    /// preview area, or a second bosun instance attaching), which
    /// rewraps the text without any new agent output. `0` when unknown
    /// (older tmux output / terse test fixtures).
    pub pane_width: u16,
    /// The active pane's OSC title (`#{pane_title}`). Agent TUIs set
    /// this to describe their own state — Claude Code writes
    /// `<glyph> <task title>`, where the glyph is a braille spinner
    /// while it's working and `✳` when it isn't. tmux records only the
    /// last title the app set, so unlike the pane body this does *not*
    /// animate frame-by-frame: it's a stable, first-party state signal
    /// that costs nothing extra to read (it rides `list-sessions`).
    /// `None` when the pane never set a title.
    pub pane_title: Option<String>,
    /// Command running in the active pane (`#{pane_current_command}`).
    /// Distinguishes a live agent from a bare shell: Claude Code names
    /// its process after its own version (`2_1_220`), while a session
    /// whose agent exited falls back to `zsh` / `bash`. Lets the
    /// detectors tell "agent up and idle" from "agent gone" without
    /// guessing from pane text. `None` when unknown.
    pub pane_command: Option<String>,
}

impl TmuxSession {
    /// Pretty name for the UI. Falls back to the internal session name
    /// if no display name was set.
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Best-available path for the UI: the user's declared spec path
    /// if the session is bosun-managed, otherwise the shell's current
    /// working directory. `None` if neither is known.
    pub fn best_path(&self) -> Option<&str> {
        self.spec_path.as_deref().or(self.current_path.as_deref())
    }
}

/// The session as the UI wants to see it: the raw tmux view plus the
/// smoothed status and (optionally) the latest pane capture for preview.
/// The actor produces these and hands them to the app.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: TmuxSession,
    pub status: Status,
    /// Raw ANSI capture of the session's pane. `Arc<[u8]>` so the UI
    /// can render without cloning the whole buffer on every frame.
    /// `None` for sessions we skipped capturing on this tick.
    pub preview: Option<Arc<[u8]>>,
    /// Hash of the visible pane's plain text on this poll. The app
    /// compares it against the hash the user last *saw* (when the
    /// session was selected) to decide whether the row has unviewed
    /// changes — the "unread" notification dot. `0` means "no usable
    /// capture this tick" (empty / failed) and never counts as a
    /// change. Computed by the actor in `refresh_all`.
    pub content_hash: u64,
}

impl SessionView {
    pub fn new(session: TmuxSession, status: Status, preview: Option<Arc<[u8]>>) -> Self {
        Self {
            session,
            status,
            preview,
            content_hash: 0,
        }
    }

    /// The internal tmux name — use this when talking to tmux.
    pub fn name(&self) -> &str {
        &self.session.name
    }

    /// The pretty display name — use this in the UI.
    pub fn display(&self) -> &str {
        self.session.display()
    }

    /// Width of the session's active pane on this poll (see
    /// [`TmuxSession::pane_width`]). Paired with [`Self::content_hash`]
    /// by the unread tracker so a reflow isn't mistaken for new output.
    pub fn width(&self) -> u16 {
        self.session.pane_width
    }

    /// The active pane's OSC title, if it set one (see
    /// [`TmuxSession::pane_title`]).
    pub fn pane_title(&self) -> Option<&str> {
        self.session.pane_title.as_deref()
    }
}
