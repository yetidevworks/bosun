use std::sync::Arc;

use crossterm::event::{KeyEvent, MouseEvent};

use crate::tmux::session::SessionView;

/// Data the new-session modal gathers and hands off to the tmux actor.
/// The `name` is the unprefixed user-entered name; the actor prepends
/// `Config::session_prefix` (e.g. `bosun-`) before calling tmux.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub options: SpecOptions,
    /// When `Some`, the actor stamps this container ID onto the
    /// new tmux session's `@bosun_container_id` user option so the
    /// freshly-created session appears as a tab inside the named
    /// sidebar container. `None` (the default) creates a session
    /// that gets its own fresh single-tab container on reconcile,
    /// matching the pre-tabs behavior.
    pub container_id: Option<String>,
    /// One-shot resume override for this launch only. When true and the
    /// agent supports it, the actor swaps in the resume invocation
    /// (claude/kimi `--continue`, codex `resume --last`) instead of whatever
    /// `options` would otherwise produce. This is the `r` action on the
    /// restart prompt for a dead session being recreated from recents.
    /// It is deliberately NOT persisted — `spec_to_metadata` and the
    /// recents store both ignore it — so it never sticks to the
    /// recreated session's saved spec; the next plain restart goes back
    /// to the stored mode.
    pub resume: bool,
    /// When `Some`, the actor creates the session inside a fresh git
    /// worktree instead of opening the user-picked path directly. See
    /// `WorktreeSpec`. `None` (the default) is a normal path-based
    /// session.
    pub worktree: Option<WorktreeSpec>,
}

/// Request to create the session inside a fresh git worktree. The
/// worktree location comes from `Config::worktree_location`; only the
/// branch is user-chosen. `None` on `SessionSpec` means a normal
/// path-based session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSpec {
    pub branch: String,
}

/// Agent-specific flags the user toggled in the new-session modal.
/// The actor's `build_agent_command` reads these and produces the
/// right CLI flags when spawning the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecOptions {
    pub claude: ClaudeOptions,
    pub codex: CodexOptions,
    pub kimi: KimiOptions,
    pub opencode: OpencodeOptions,
    pub qwen: QwenOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeOptions {
    pub session_mode: ClaudeSessionMode,
    pub skip_permissions: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClaudeSessionMode {
    #[default]
    New,
    Continue,
    Resume,
}

impl ClaudeSessionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::Continue => "Continue",
            Self::Resume => "Resume",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::New => Self::Continue,
            Self::Continue => Self::Resume,
            Self::Resume => Self::New,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::New => Self::Resume,
            Self::Continue => Self::New,
            Self::Resume => Self::Continue,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexOptions {
    /// New (`codex`) / Continue (`codex resume --last`) / Resume
    /// (`codex resume` — interactive picker). Same tri-state as
    /// Claude, so `ClaudeSessionMode` is reused.
    pub session_mode: ClaudeSessionMode,
    /// Codex `--yolo` — bypass approvals and sandbox. Dangerous.
    pub yolo: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KimiOptions {
    /// New / Continue (`--continue`) / Resume (`--resume`). Kimi's
    /// session tri-state matches Claude's exactly, so we reuse
    /// `ClaudeSessionMode` rather than duplicate the enum.
    pub session_mode: ClaudeSessionMode,
    /// kimi `--yolo` — auto-approve all actions. Dangerous.
    pub yolo: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpencodeOptions {
    /// New (`opencode`) / Continue (`opencode --continue`). OpenCode
    /// has no CLI session picker (`--session` needs an explicit id;
    /// the picker lives inside its TUI), so the modal only offers
    /// New/Continue — `Resume` is mapped to Continue defensively if
    /// it ever reaches the command builder.
    pub session_mode: ClaudeSessionMode,
    /// opencode `--auto` — auto-approve permissions that are not
    /// explicitly denied. Dangerous.
    pub auto: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QwenOptions {
    /// New / Continue (`--continue`) / Resume (`--resume`, which
    /// opens Qwen's session picker when given no id). Same tri-state
    /// as Claude, so `ClaudeSessionMode` is reused.
    pub session_mode: ClaudeSessionMode,
    /// qwen `--yolo` — auto-approve all actions. Dangerous.
    pub yolo: bool,
}

/// Commands flow from the UI/app task into the tmux actor.
#[derive(Debug)]
pub enum Command {
    /// Refresh the session list immediately (out of schedule).
    ListNow,
    /// Attach to the selected session. The actor takes care of
    /// installing the Ctrl-Q binding before attach and removing it after.
    #[allow(dead_code)]
    Attach { name: String },
    /// The user selected a different session; the actor should capture
    /// its pane with priority so the preview updates quickly. The name
    /// is owned so the command can cross the mpsc boundary.
    FocusPreview { name: String },
    /// Create a new tmux session from the new-session modal's form data.
    CreateSession(SessionSpec),
    /// Kill a session by its internal tmux name. `tmux kill-session -t`.
    KillSession(String),
    /// Kill a worktree-backed session and clean up its git worktree.
    /// The actor resolves the main repo root from `worktree_path` via
    /// `main_repo_root`. `merge` merges the branch into the repo's
    /// current branch and deletes it after removing the worktree;
    /// `false` removes the worktree but keeps the branch. Refuses on a
    /// dirty tree.
    KillSessionRemoveWorktree {
        internal: String,
        worktree_path: String,
        branch: String,
        merge: bool,
    },
    /// Kill every tmux session named in `tabs` in one batch — used
    /// by `Shift+D` to tear down all tabs in a container at once.
    /// The actor iterates `KillSession` for each name; sidebar
    /// reconcile drops the now-empty container on the next
    /// refresh.
    KillContainer { tabs: Vec<String> },
    /// Rename the pretty display name of a session. The internal tmux
    /// name never changes (we only update the `@bosun_display` user
    /// option); the UI picks up the new label on the next refresh.
    RenameSession {
        internal: String,
        new_display: String,
    },
    /// Kill and recreate a session using the metadata we persisted
    /// as `@bosun_*` tmux user options when it was first created.
    /// The new session gets a fresh internal name (new hex suffix)
    /// but keeps the same display name, path, agent and options.
    ///
    /// `continue_session` is a one-shot override for this restart: when
    /// true and the agent supports it, the actor swaps in the resume
    /// invocation (claude `--continue`, codex `resume --last`) instead
    /// of the persisted session mode. The restart modal sets it when
    /// the user picks the `r` (resume) action instead of a plain
    /// restart. The stored `@bosun_*` metadata is left untouched.
    RestartSession {
        internal: String,
        continue_session: bool,
    },
    /// Type the agent launch command into a session that was created as
    /// a bare shell (deferred-launch path, issue #2). Emitted by the
    /// app once the embedded terminal — which answers the OSC 10/11/12
    /// background-color queries Codex/Neovim probe at startup — has
    /// attached to the new session. The actor reads the session's
    /// `@bosun_*` metadata, rebuilds the command, and types it via the
    /// same `restart_in_place` path restart uses. Launching only after
    /// the OSC responder is live is what lets Codex detect a light
    /// background instead of caching a dark one. No-op if the session
    /// already left its shell (the agent is somehow already running).
    ///
    /// `resume` is a one-shot override for the launch mode: `None`
    /// uses the persisted `@bosun_*` session mode (the fresh-create
    /// path), `Some(b)` forces resume on/off for this launch only
    /// (the in-place restart path, where the restart modal's `r`
    /// action passes `Some(true)` to relaunch into the agent's resume
    /// invocation without touching stored metadata).
    LaunchAgent {
        internal: String,
        resume: Option<bool>,
    },
    /// Read the current `@bosun_*` metadata off a live session so
    /// the modify-session modal can pre-fill its fields. The actor
    /// replies with an `AppMsg::ModifySpecReady`.
    OpenModifySession { internal: String },
    /// Persist a new spec to the session's `@bosun_*` user options
    /// (and update the display label if it changed). Does NOT
    /// restart the running agent — the user picks that up next
    /// time they hit `R`. Also upserts the recents row so the
    /// recents picker reflects the latest spec.
    ModifySession { internal: String, spec: SessionSpec },
    /// Delete a single recent entry from the SQLite store by its
    /// primary key. The RecentsModal emits this when the user hits
    /// `d` on a highlighted row.
    DeleteRecent(i64),
    /// Persist the divider position to config.toml. Intercepted by
    /// the app loop — never forwarded to the tmux actor.
    SaveDivider(Option<u16>),
    /// Persist the user-defined sidebar (sections + session order) to
    /// config.toml. Intercepted by the app loop — never forwarded to
    /// the tmux actor.
    SaveSidebar(crate::sidebar::SidebarModel),
    /// Persist the display_name → section_name history to config.toml.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    SaveSessionHistory(std::collections::HashMap<String, String>),
    /// Persist the global TDF banner font to config.toml. Intercepted
    /// by the app loop — never forwarded to the tmux actor.
    SaveBannerFont(String),
    /// Insert a new section header above the cursor with the given name.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    InsertSection { name: String },
    /// Rename a section header by its id.
    /// Intercepted by the app loop — never forwarded to the tmux actor.
    RenameSection { id: String, new_name: String },
    /// Set the active theme. Intercepted by the app loop — this
    /// command is NEVER forwarded to the tmux actor (it's a pure UI
    /// state change). `persist=false` is a transient live preview
    /// (sent by the theme picker on every arrow key); `persist=true`
    /// also writes `theme = "<name>"` to `config.toml`. On cancel,
    /// the picker emits `SetTheme { original, persist: false }` to
    /// revert the UI without touching disk.
    SetTheme { name: String, persist: bool },
    /// Spawn the configured external editor (`bosun editor <cmd>` /
    /// `editor = "..."` in config.toml) against the highlighted
    /// session's path. Intercepted by the app loop — runs
    /// `<editor> <path>` detached so the TUI keeps the foreground.
    /// The reducer pre-resolves both fields so the loop just calls
    /// `Command::new(...).spawn()`.
    OpenEditor { editor: String, path: String },
    /// Graceful shutdown signal.
    #[allow(dead_code)]
    Shutdown,
}

/// Messages flow from actors (input, tmux) back to the app task.
/// The app task is the single writer of `AppState`.
///
/// There's no periodic `Tick` variant any more. As of the tmux -C
/// rewrite, session-list refreshes are push-driven by control-mode
/// notifications from the `tmux_actor`'s monitor subprocess, not by
/// a 1Hz poller. Main no longer generates `ListNow` commands from a
/// timer — it just waits for `SessionsRefreshed` to arrive.
#[derive(Debug)]
pub enum AppMsg {
    /// A key from the terminal.
    Key(KeyEvent),
    /// A mouse event from the terminal. Used by the draggable divider
    /// between the session list and preview. Dropped by non-mouse
    /// consumers.
    Mouse(MouseEvent),
    /// A paste event from the terminal (bracketed paste). Crossterm
    /// decodes `\e[200~ ... \e[201~` sequences from the outer
    /// terminal and hands us the inner text as a `String`. Outer
    /// terminals also use bracketed paste to deliver drag-drop
    /// content (file paths, image markers), so this is the path
    /// for "I dropped a file onto bosun". When the embed is
    /// focused we re-wrap and forward to the embed PTY; otherwise
    /// bosun ignores it (no modal currently accepts pasted text
    /// directly — they all go through `Key(c)` events for
    /// individual characters).
    Paste(String),
    /// Terminal was resized.
    Resize(u16, u16),
    /// Synthetic wake-up from the run loop's selection-settle timer:
    /// the cursor has rested on a session long enough that a
    /// deferred embed spawn should go ahead. Carries no data and
    /// changes no state — `App::sync_embed` does the work.
    EmbedSettle,
    /// Fresh session list from tmux, with smoothed status and optional
    /// preview buffer per entry. `select_after` carries an internal
    /// session name that the app should jump the selection to — set
    /// when this refresh is the result of a create (so the new session
    /// auto-highlights). `None` for notification-driven refreshes where
    /// the app preserves its existing selection.
    SessionsRefreshed {
        sessions: Vec<SessionView>,
        select_after: Option<String>,
    },
    /// Lightweight preview refresh for a single (focused) session. The
    /// tmux actor's fast-preview tick (`Config::preview_tick_ms`,
    /// default 200ms) captures just the focused pane and emits this
    /// message — much cheaper than a full `SessionsRefreshed`. The app
    /// handler updates the preview bytes on the matching SessionView
    /// in place and does no other work (no detector run, no sidebar
    /// reconcile, no statusbar sync). A no-op if the named session is
    /// no longer in the list (it was killed between capture and
    /// delivery).
    PreviewRefreshed { name: String, bytes: Arc<[u8]> },
    /// Response to `Command::OpenModifySession`: the actor has
    /// read the live `@bosun_*` metadata off the named session and
    /// the app should open the modify-session modal pre-filled
    /// from `spec`. `internal` lets the modal remember which
    /// session it's editing so the submit emits
    /// `Command::ModifySession` against the right name.
    ModifySpecReady { internal: String, spec: SessionSpec },
    /// Lightweight status push for a single session. Sibling of
    /// `PreviewRefreshed` — emitted by the tmux actor's fast tick
    /// once it has captured + classified a pane, so the sidebar
    /// glyph updates at the fast-tick cadence instead of waiting
    /// for the 1Hz `SessionsRefreshed` reconcile. The app handler
    /// updates the matching `SessionView.session.status` in place
    /// and does nothing else — no list reconcile, no statusbar
    /// diff, no detector re-run. A no-op if the named session is
    /// no longer in the list.
    StatusRefreshed {
        name: String,
        status: crate::tmux::detector::Status,
    },
    /// A chunk of bytes read from an embedded terminal's PTY (2.0+).
    /// `session` is the internal tmux session name the embed was
    /// spawned for. The app handler discards the chunk if the
    /// currently-active embed isn't for the same session (stale
    /// chunks from a previous embed instance after focus switch);
    /// otherwise it feeds the bytes into the embed's vt100 parser.
    /// Each chunk arrives from a dedicated reader thread inside
    /// `EmbedTerminal` and triggers a normal redraw on the next
    /// iteration of the app event loop.
    EmbedBytes { session: String, bytes: Vec<u8> },
    /// An attach just started — the UI should render a placeholder
    /// while we block in `tmux attach`.
    AttachStarted { name: String },
    /// The attach returned (user detached).
    AttachEnded { name: String },
    /// A non-fatal error to surface in the status bar.
    Warn(String),
    /// A fatal error — bail out of the event loop.
    Fatal(String),
    /// Explicit shutdown request (Ctrl-C, SIGTERM).
    Shutdown,
    /// SIGCONT — we came back from Ctrl-Z suspend, re-enter raw mode.
    Resume,
    /// An in-place restart (`R`) has stopped the old agent and left a
    /// bare shell; the app should defer the relaunch until its embed
    /// has (re)attached so the OSC 10/11/12 background responder is
    /// live before the agent's startup probe (issue #2). Mirrors the
    /// fresh-create deferral, but carries the restart's one-shot
    /// `resume` choice so the `r` action still relaunches into the
    /// agent's resume invocation. The app marks the session pending and
    /// fires `Command::LaunchAgent` for it after the next `sync_embed`.
    DeferRelaunch { internal: String, resume: bool },
    /// Terminal regained focus (e.g. switching back to the iTerm
    /// window). Triggers a full repaint to recover from things like
    /// iTerm's Cmd+R "reset" that clears the screen and exits alt
    /// screen out from under us without notifying ratatui.
    FocusGained,
    /// Terminal lost focus. We only track it so the *next*
    /// `FocusGained` is recognized as a genuine refocus (and not the
    /// echo a terminal emits when focus reporting is re-enabled), so
    /// recovery runs once instead of looping. See `App::has_focus`.
    FocusLost,
}
