//! Wire types for the `bosun serve` socket protocol, version 1.
//!
//! Every message is one length-prefixed JSON frame (see
//! [`super::frame`]). Each message is a JSON object with a `"t"` field
//! naming its kind. The full description, with examples, lives in
//! `docs/protocol.md`; keep the two in step.
//!
//! These types are deliberately separate from Bosun's domain types
//! (`SessionView`, `SidebarModel`, `Theme`, …): the wire format is a
//! contract with other programs, so a refactor inside Bosun must not
//! change it by accident. The `From`/`TryFrom` conversions at the end
//! are the only bridge.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::events::{
    ClaudeOptions, ClaudeSessionMode, CodexOptions, KimiOptions, OpencodeOptions, QwenOptions,
    SessionSpec, SpecOptions, WorktreeSpec,
};
use crate::sidebar::{Container, Parked, Section, SectionDefaults, SectionRole, SidebarModel};
use crate::store::Recent;
use crate::tmux::detector::Status;
use crate::tmux::session::SessionView;

/// Protocol major version. Bump only for changes an older client
/// can't ignore; adding optional fields or new message kinds doesn't
/// need a bump (clients must ignore what they don't know).
pub const PROTOCOL_VERSION: u32 = 1;

// ── client → daemon ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on every connection.
    Hello {
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    /// Ask for a fresh `snapshot` (after a `seq` gap, say).
    Resync,
    /// A command. The daemon answers with an `ack` carrying `id`.
    Cmd(CmdEnvelope),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub op: CmdOp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CmdOp {
    /// Create a session. The agent launches straight away unless
    /// `defer_launch` is set, in which case the session starts as a
    /// bare shell and the client sends `launch_agent` once its
    /// terminal is attached (so the agent's startup color probe gets
    /// an answer — see issue #2).
    Create {
        spec: SpecWire,
        #[serde(default)]
        defer_launch: bool,
        /// Put the new row at the end of this section (by id). Ignored
        /// when `spec.container_id` adds the session to a row as a tab.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        /// An exited tab this session restarts: it takes that tab's
        /// place in its row instead of getting a row of its own.
        /// Ignored unless it names a tab whose session has ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
    },
    Kill {
        session: String,
    },
    /// Kill every tab of a container.
    KillContainer {
        sessions: Vec<String>,
    },
    /// Kill a worktree-backed session and remove its worktree; with
    /// `merge`, merge the branch first and delete it afterwards.
    KillRemoveWorktree {
        session: String,
        worktree_path: String,
        branch: String,
        #[serde(default)]
        merge: bool,
    },
    Rename {
        session: String,
        display: String,
    },
    /// Stop the agent and relaunch it in place. `resume` relaunches
    /// into the agent's resume invocation for this launch only.
    /// `defer_launch` works as for `create`.
    Restart {
        session: String,
        #[serde(default)]
        resume: bool,
        #[serde(default)]
        defer_launch: bool,
    },
    /// Type the agent's launch command into a session sitting at a
    /// bare shell. `resume: null` uses the session's stored mode.
    LaunchAgent {
        session: String,
        #[serde(default)]
        resume: Option<bool>,
    },
    /// Read a session's stored spec (to pre-fill a modify form). The
    /// ack carries it in `spec`.
    GetSpec {
        session: String,
    },
    /// Read any theme's palette (for a client's live preview while a
    /// picker is open). Changes nothing; the ack carries it in `theme`.
    GetTheme {
        name: String,
    },
    /// Store a new spec on a live session. Takes effect on the next
    /// restart.
    Modify {
        session: String,
        spec: SpecWire,
    },
    /// Remove a recents row (its `id` from the `recents` list).
    DeleteRecent {
        recent: i64,
    },
    /// Replace the sidebar model (after a reorder, a move between
    /// sections, a collapse toggle…). Tabs only need `session`.
    SidebarSave {
        sidebar: SidebarWire,
    },
    /// Move one tab into another row (`container_id`, at `index` among
    /// its tabs, appended when absent), or with `container_id: null`
    /// out into a new row of its own: at `index` among the rows of
    /// `section` (the ungrouped rows when absent), or right after the
    /// old one when `index` is absent.
    MoveTab {
        session: String,
        container_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
    /// Move every tab of row `from` into row `into`, in order, at
    /// `index` among its tabs (appended when absent), as one sidebar
    /// change.
    MergeRows {
        from: String,
        into: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    InsertSection {
        name: String,
    },
    /// Park a row in the Snoozed or Docked section. Docking kills its
    /// sessions; a snoozed row wakes at `wake_at` (Unix ms), or with
    /// `until_needed` when one of its agents needs you.
    ParkRow {
        container_id: String,
        role: SectionRole,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_at: Option<u64>,
        #[serde(default)]
        until_needed: bool,
    },
    /// Move a parked row back where it came from; an undocked row's
    /// sessions are resumed.
    UnparkRow {
        container_id: String,
    },
    /// Set (or with `null`, clear) a section's starting values for a
    /// new session.
    SetSectionDefaults {
        section: String,
        defaults: Option<SectionDefaults>,
    },
    RenameSection {
        section: String,
        name: String,
    },
    /// Switch theme. `persist: false` is a live preview that isn't
    /// written to `config.toml`.
    SetTheme {
        name: String,
        #[serde(default = "default_true")]
        persist: bool,
    },
    /// Change one settings-panel option. `key` is a `config.settings`
    /// key; pinned and read-only settings are refused.
    ApplySetting {
        key: String,
        value: serde_json::Value,
    },
    /// Toggle tmux's `ignore-size` flag on the client attached through
    /// `tty` (`refresh-client -t <tty> -f [!]ignore-size`).
    ClientFlags {
        tty: String,
        ignore_size: bool,
    },
    /// Make the client on `tty` tmux's latest client, so the session
    /// takes its size under `window-size latest`.
    MakeLatest {
        tty: String,
    },
}

fn default_true() -> bool {
    true
}

// ── daemon → client ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Snapshot(Box<Snapshot>),
    /// Rows added or changed (`upsert`, full rows) and rows gone
    /// (`removed`, session names).
    Sessions {
        seq: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        upsert: Vec<SessionWire>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
    },
    Status {
        seq: u64,
        session: String,
        state: StatusWire,
        source: StatusSourceWire,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// See [`StatusInfo::done_at`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        done_at: Option<u64>,
        /// See [`StatusInfo::started_at`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<u64>,
    },
    Sidebar {
        seq: u64,
        sidebar: SidebarWire,
    },
    Recents {
        seq: u64,
        recents: Vec<RecentWire>,
    },
    Config {
        seq: u64,
        config: ConfigWire,
    },
    Theme {
        seq: u64,
        theme: Box<ThemeWire>,
    },
    Themes {
        seq: u64,
        themes: Vec<ThemeEntry>,
    },
    /// The palettes of the Light/Dark pair (`theme_light` and
    /// `theme_dark` in `config.toml`), both at once. Absent is unset.
    ThemePair {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_light: Option<Box<ThemeWire>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_dark: Option<Box<ThemeWire>>,
    },
    Keybindings {
        seq: u64,
        keybindings: KeybindingsWire,
    },
    /// A human-readable message from Bosun (the TUI shows these in its
    /// status bar): "created …", errors from background work, etc.
    Notice {
        seq: u64,
        message: String,
    },
    /// The tmux client on terminal `tty` is now on `session`: it
    /// attached, or something moved it (`switch-client`, a key binding
    /// typed inside it). Sent for every terminal client on Bosun's tmux
    /// server, including when the session didn't change.
    ClientSession {
        seq: u64,
        tty: String,
        session: String,
    },
    /// The tmux client on terminal `tty` detached.
    ClientDetached {
        seq: u64,
        tty: String,
    },
    Ack(Ack),
    /// A protocol-level problem. `protocol_mismatch` and `no_hello`
    /// close the connection.
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub server: ServerInfo,
    pub sessions: Vec<SessionWire>,
    pub statuses: BTreeMap<String, StatusInfo>,
    pub sidebar: SidebarWire,
    pub recents: Vec<RecentWire>,
    pub config: ConfigWire,
    pub theme: ThemeWire,
    /// Palette for the macOS Light appearance, when `theme_light` is
    /// set. Absent means "use `theme`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_light: Option<ThemeWire>,
    /// Palette for the macOS Dark appearance, when `theme_dark` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_dark: Option<ThemeWire>,
    pub themes: Vec<ThemeEntry>,
    #[serde(default)]
    pub keybindings: KeybindingsWire,
    /// Warnings that hold for as long as the daemon runs, such as a tmux
    /// server older than `tmux -V`. Each was also sent as a `notice`,
    /// but a client that connected later missed it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
}

/// The navigation shortcuts from `[keybindings]` in `config.toml`, as
/// the strings a user would write there (`Shift+Left`, `Ctrl+v`), so a
/// GUI honours the same remaps as the TUI. `None` (JSON `null`) is an
/// action the user disabled. Every field is always sent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeybindingsWire {
    pub previous_tab: Option<String>,
    pub next_tab: Option<String>,
    pub previous_session: Option<String>,
    pub next_session: Option<String>,
    pub send_next_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Ack {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `create`: the internal name of the new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// `get_spec`: the stored spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<SpecWire>,
    /// `get_theme`: the theme's palette.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Box<ThemeWire>>,
    /// `create`/`restart` with `defer_launch`: send `launch_agent`
    /// with these values once the terminal is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_launch: Option<PendingLaunchWire>,
    /// Every Bosun message produced while the command ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLaunchWire {
    pub session: String,
    #[serde(default)]
    pub resume: Option<bool>,
}

/// One live tmux session: what a sidebar row shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWire {
    /// Internal tmux name; the key for every command.
    pub name: String,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The path to show: the spec path, else the shell's cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub windows: u32,
    /// Whether any tmux client is attached.
    pub attached: bool,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusWire {
    Running,
    Waiting,
    Done,
    Idle,
    Error,
    Unknown,
}

/// Where a status came from: the agent's own hook, or Bosun's guess
/// from the pane title and contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSourceWire {
    Hook,
    Detector,
}

/// One session's status in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub state: StatusWire,
    pub source: StatusSourceWire,
    /// What a hook-reported state is about, e.g. `permission: Bash`
    /// while waiting. Only sent with `source: hook`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the agent's hook reported that it finished its turn, Unix
    /// milliseconds. Only sent with `source: hook` when the hook's own
    /// state is `done`; `state` is `idle` then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<u64>,
    /// When the current turn started, Unix milliseconds: sent while the
    /// state is `running` or `waiting`, for a working timer. A turn
    /// carries on through permission waits until the agent is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarWire {
    #[serde(default)]
    pub ungrouped: Vec<ContainerWire>,
    #[serde(default)]
    pub sections: Vec<SectionWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionWire {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_font: Option<String>,
    /// Starting values for a new session in this section. Set with
    /// `set_section_defaults`; a `sidebar_save` leaves them as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<SectionDefaults>,
    /// `snoozed` or `docked` for the built-in sections parked rows go
    /// to; absent for the user's own sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SectionRole>,
    #[serde(default)]
    pub members: Vec<ContainerWire>,
}

/// A sidebar row: one or more sessions shown as tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerWire {
    pub id: String,
    /// The row's label, as the TUI shows it: the active tab's display
    /// name (or, for an exited tab, the name recovered from recents).
    /// Ignored on `sidebar_save`.
    #[serde(default)]
    pub name: String,
    /// Session name of the active tab.
    pub active: String,
    pub tabs: Vec<TabWire>,
    /// Set while the row is in the Snoozed or Docked section: where it
    /// came from and when a snoozed row wakes. Ignored on
    /// `sidebar_save`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<Parked>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabWire {
    pub session: String,
    /// Display name; for an exited session, the best name Bosun can
    /// recover from recents. Ignored on `sidebar_save`.
    #[serde(default)]
    pub display: String,
    /// False for a row whose tmux session has ended (the TUI shows it
    /// as "exited" so it can be restarted). Ignored on `sidebar_save`.
    #[serde(default)]
    pub alive: bool,
    /// For an exited session: the id of the recents entry it was
    /// created from, to restart it from. Ignored on `sidebar_save`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentWire {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub options: OptionsWire,
    /// Unix milliseconds.
    pub last_used_at: i64,
    pub use_count: i64,
}

/// What the new-session form collects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecWire {
    pub name: String,
    #[serde(default)]
    pub path: String,
    pub agent: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub options: OptionsWire,
    /// Join an existing container as a new tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// One-shot resume for this launch.
    #[serde(default)]
    pub resume: bool,
    /// Create the session in a new git worktree on this branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModeWire {
    #[default]
    New,
    Continue,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OptionsWire {
    pub claude: ClaudeWire,
    pub codex: ModeYoloWire,
    pub kimi: ModeYoloWire,
    pub opencode: OpencodeWire,
    pub qwen: ModeYoloWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeWire {
    pub session_mode: SessionModeWire,
    pub skip_permissions: bool,
    pub remote_control: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeYoloWire {
    pub session_mode: SessionModeWire,
    pub yolo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpencodeWire {
    pub session_mode: SessionModeWire,
    pub auto: bool,
}

/// The settings panel, as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigWire {
    pub settings: Vec<SettingWire>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingWire {
    pub key: String,
    pub value: serde_json::Value,
    /// Allowed values for a choice setting; absent for booleans and
    /// read-only rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    /// Environment variable pinning this setting, if one is set. A
    /// pinned setting is read-only: env beats `config.toml`, so a
    /// change would snap back on the next launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_by: Option<String>,
    /// True for pinned settings and for install-level ones
    /// (`session_prefix`, `tmux_socket`) that are only set by editing
    /// `config.toml`.
    pub read_only: bool,
}

/// The active theme: every `Theme` color slot as `#rrggbb`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeWire {
    pub name: String,
    pub bg: String,
    pub panel: String,
    pub panel_alt: String,
    pub selection_bg: String,
    pub text: String,
    pub text_muted: String,
    pub accent: String,
    pub shadow: String,
    pub dim_fg: String,
    pub status_running: String,
    pub status_waiting: String,
    pub status_idle: String,
    pub status_error: String,
    /// Resolved: themes that don't set it fall back to `accent`, the
    /// same as the TUI.
    pub status_done: String,
    /// The theme's `[terminal]` table, if it has one. Optional and left
    /// out when absent, so older clients and daemons are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalWire>,
}

/// A theme's terminal colours as `#rrggbb`: what a graphical client
/// paints its terminals with. `palette` is ANSI 0-7 then 8-15.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalWire {
    /// The Ghostty theme the colours came from, for reference only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghostty: Option<String>,
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection_bg: String,
    pub selection_fg: String,
    pub palette: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeEntry {
    pub name: String,
    pub source: ThemeSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSource {
    Bundled,
    /// A `.toml` in the user's themes directory (it wins over a
    /// bundled theme of the same name).
    User,
}

// ── conversions ────────────────────────────────────────────────────

impl From<crate::agent_hooks::StatusSource> for StatusSourceWire {
    fn from(s: crate::agent_hooks::StatusSource) -> Self {
        match s {
            crate::agent_hooks::StatusSource::Hook => Self::Hook,
            crate::agent_hooks::StatusSource::Detector => Self::Detector,
        }
    }
}

impl From<&crate::tmux::session::SessionView> for StatusInfo {
    fn from(v: &crate::tmux::session::SessionView) -> Self {
        Self {
            state: v.status.into(),
            source: v.status_source.into(),
            detail: v.status_detail.clone(),
            done_at: v.done_at,
            started_at: None,
        }
    }
}

impl From<Status> for StatusWire {
    fn from(s: Status) -> Self {
        match s {
            Status::Running => Self::Running,
            Status::Waiting => Self::Waiting,
            Status::Done => Self::Done,
            Status::Idle => Self::Idle,
            Status::Error => Self::Error,
            Status::Unknown => Self::Unknown,
        }
    }
}

fn unix_secs(t: Option<std::time::SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

impl From<&SessionView> for SessionWire {
    fn from(v: &SessionView) -> Self {
        let s = &v.session;
        Self {
            name: s.name.clone(),
            display: s.display().to_string(),
            agent: s.agent.clone(),
            path: s.best_path().map(str::to_string),
            windows: s.windows,
            attached: s.attached,
            created: unix_secs(s.created),
            container_id: s.container_id.clone(),
            worktree_path: s.worktree_path.clone(),
            branch: s.branch.clone(),
            pane_title: s.pane_title.clone(),
            pane_command: s.pane_command.clone(),
        }
    }
}

impl From<ClaudeSessionMode> for SessionModeWire {
    fn from(m: ClaudeSessionMode) -> Self {
        match m {
            ClaudeSessionMode::New => Self::New,
            ClaudeSessionMode::Continue => Self::Continue,
            ClaudeSessionMode::Resume => Self::Resume,
        }
    }
}

impl From<SessionModeWire> for ClaudeSessionMode {
    fn from(m: SessionModeWire) -> Self {
        match m {
            SessionModeWire::New => Self::New,
            SessionModeWire::Continue => Self::Continue,
            SessionModeWire::Resume => Self::Resume,
        }
    }
}

impl From<&SpecOptions> for OptionsWire {
    fn from(o: &SpecOptions) -> Self {
        Self {
            claude: ClaudeWire {
                session_mode: o.claude.session_mode.into(),
                skip_permissions: o.claude.skip_permissions,
                remote_control: o.claude.remote_control,
            },
            codex: ModeYoloWire {
                session_mode: o.codex.session_mode.into(),
                yolo: o.codex.yolo,
            },
            kimi: ModeYoloWire {
                session_mode: o.kimi.session_mode.into(),
                yolo: o.kimi.yolo,
            },
            opencode: OpencodeWire {
                session_mode: o.opencode.session_mode.into(),
                auto: o.opencode.auto,
            },
            qwen: ModeYoloWire {
                session_mode: o.qwen.session_mode.into(),
                yolo: o.qwen.yolo,
            },
        }
    }
}

impl From<&OptionsWire> for SpecOptions {
    fn from(o: &OptionsWire) -> Self {
        Self {
            claude: ClaudeOptions {
                session_mode: o.claude.session_mode.into(),
                skip_permissions: o.claude.skip_permissions,
                remote_control: o.claude.remote_control,
            },
            codex: CodexOptions {
                session_mode: o.codex.session_mode.into(),
                yolo: o.codex.yolo,
            },
            kimi: KimiOptions {
                session_mode: o.kimi.session_mode.into(),
                yolo: o.kimi.yolo,
            },
            opencode: OpencodeOptions {
                session_mode: o.opencode.session_mode.into(),
                auto: o.opencode.auto,
            },
            qwen: QwenOptions {
                session_mode: o.qwen.session_mode.into(),
                yolo: o.qwen.yolo,
            },
        }
    }
}

impl From<&SessionSpec> for SpecWire {
    fn from(s: &SessionSpec) -> Self {
        Self {
            name: s.name.clone(),
            path: s.path.clone(),
            agent: s.agent.clone(),
            args: s.args.clone(),
            options: (&s.options).into(),
            container_id: s.container_id.clone(),
            resume: s.resume,
            worktree_branch: s.worktree.as_ref().map(|w| w.branch.clone()),
        }
    }
}

/// Reject text that would break a tmux format line or a sidebar row:
/// control characters (a newline in a display name splits the
/// `list-sessions` line it rides on).
pub fn check_text(field: &str, value: &str, allow_empty: bool) -> Result<(), String> {
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(())
}

impl SpecWire {
    /// Validate and convert to the actor's spec.
    pub fn to_spec(&self) -> Result<SessionSpec, String> {
        check_text("spec.name", &self.name, false)?;
        check_text("spec.path", &self.path, true)?;
        check_text("spec.args", &self.args, true)?;
        if !crate::config::AGENTS.contains(&self.agent.as_str()) {
            return Err(format!(
                "spec.agent must be one of {}",
                crate::config::AGENTS.join(", ")
            ));
        }
        if let Some(b) = &self.worktree_branch {
            check_text("spec.worktree_branch", b, false)?;
            if b.starts_with('-') || b.contains(char::is_whitespace) {
                return Err("spec.worktree_branch is not a valid branch name".into());
            }
        }
        if let Some(c) = &self.container_id {
            check_text("spec.container_id", c, false)?;
        }
        Ok(SessionSpec {
            name: self.name.clone(),
            path: self.path.clone(),
            agent: self.agent.clone(),
            args: self.args.clone(),
            options: (&self.options).into(),
            container_id: self.container_id.clone(),
            resume: self.resume,
            worktree: self
                .worktree_branch
                .as_ref()
                .map(|b| WorktreeSpec { branch: b.clone() }),
        })
    }
}

impl From<&Recent> for RecentWire {
    fn from(r: &Recent) -> Self {
        let spec = r.to_spec();
        Self {
            id: r.id,
            name: r.name.clone(),
            path: r.path.clone(),
            agent: r.agent.clone(),
            args: r.args.clone(),
            options: (&spec.options).into(),
            last_used_at: r.last_used_at,
            use_count: r.use_count,
        }
    }
}

impl SidebarWire {
    /// Build the wire sidebar. `display_for` names a tab (live display
    /// name, or the recovered name of an exited session); `alive`
    /// says whether its tmux session is still there; `recent_for` finds
    /// the recents entry an exited session was created from.
    pub fn from_model(
        model: &SidebarModel,
        display_for: impl Fn(&str) -> String,
        alive: impl Fn(&str) -> bool,
        recent_for: impl Fn(&str) -> Option<i64>,
    ) -> Self {
        let container = |c: &Container| ContainerWire {
            id: c.id.clone(),
            // The row's label as the TUI shows it: the active tab's
            // name. The model's own `name` is an internal placeholder
            // (the first tab's tmux name) that nothing displays.
            name: display_for(&c.active),
            active: c.active.clone(),
            tabs: c
                .members
                .iter()
                .map(|m| {
                    let alive = alive(m);
                    TabWire {
                        session: m.clone(),
                        display: display_for(m),
                        alive,
                        recent: if alive { None } else { recent_for(m) },
                    }
                })
                .collect(),
            parked: c.parked.clone(),
        };
        Self {
            ungrouped: model.ungrouped.iter().map(container).collect(),
            sections: model
                .sections
                .iter()
                .map(|s| SectionWire {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    collapsed: s.collapsed,
                    banner_font: s.banner_font.clone(),
                    defaults: s.defaults.clone(),
                    role: s.role,
                    members: s.members.iter().map(container).collect(),
                })
                .collect(),
        }
    }

    /// Validate and convert a client's sidebar to the model. Ids must
    /// be unique, every container needs at least one tab, its active
    /// tab must be one of them, and a session may appear only once.
    pub fn to_model(&self) -> Result<SidebarModel, String> {
        let mut ids = std::collections::HashSet::new();
        let mut sessions = std::collections::HashSet::new();
        let mut container = |c: &ContainerWire| -> Result<Container, String> {
            check_text("container.id", &c.id, false)?;
            check_text("container.name", &c.name, true)?;
            if !ids.insert(c.id.clone()) {
                return Err(format!("duplicate id {}", c.id));
            }
            if c.tabs.is_empty() {
                return Err(format!("container {} has no tabs", c.id));
            }
            let members: Vec<String> = c.tabs.iter().map(|t| t.session.clone()).collect();
            for m in &members {
                check_text("tab.session", m, false)?;
                if !sessions.insert(m.clone()) {
                    return Err(format!("session {m} appears more than once"));
                }
            }
            if !members.contains(&c.active) {
                return Err(format!(
                    "container {} active tab is not one of its tabs",
                    c.id
                ));
            }
            Ok(Container {
                id: c.id.clone(),
                name: c.name.clone(),
                members,
                active: c.active.clone(),
                parked: c.parked.clone(),
            })
        };
        let ungrouped = self
            .ungrouped
            .iter()
            .map(&mut container)
            .collect::<Result<Vec<_>, _>>()?;
        let mut sections = Vec::new();
        let mut section_ids = std::collections::HashSet::new();
        for s in &self.sections {
            check_text("section.id", &s.id, false)?;
            check_text("section.name", &s.name, false)?;
            if !section_ids.insert(s.id.clone()) {
                return Err(format!("duplicate section id {}", s.id));
            }
            sections.push(Section {
                id: s.id.clone(),
                name: s.name.clone(),
                collapsed: s.collapsed,
                banner_font: s.banner_font.clone(),
                defaults: None,
                role: s.role,
                members: s
                    .members
                    .iter()
                    .map(&mut container)
                    .collect::<Result<Vec<_>, _>>()?,
            });
        }
        // Ended tabs' names aren't in a client's layout:
        // `SidebarModel::adopt` carries them over.
        Ok(SidebarModel {
            ungrouped,
            sections,
            ended_names: Default::default(),
        })
    }
}

/// `#rrggbb` for a theme color. Theme files only hold hex colors, so
/// this is almost always the `Rgb` arm; the named and indexed arms use
/// xterm's default palette so nothing is ever left out.
pub fn color_hex(c: ratatui::style::Color) -> String {
    use ratatui::style::Color as C;
    let (r, g, b) = match c {
        C::Rgb(r, g, b) => (r, g, b),
        C::Indexed(i) => xterm_256(i),
        C::Black | C::Reset => (0, 0, 0),
        C::Red => (0xcd, 0, 0),
        C::Green => (0, 0xcd, 0),
        C::Yellow => (0xcd, 0xcd, 0),
        C::Blue => (0, 0, 0xee),
        C::Magenta => (0xcd, 0, 0xcd),
        C::Cyan => (0, 0xcd, 0xcd),
        C::Gray => (0xe5, 0xe5, 0xe5),
        C::DarkGray => (0x7f, 0x7f, 0x7f),
        C::LightRed => (0xff, 0, 0),
        C::LightGreen => (0, 0xff, 0),
        C::LightYellow => (0xff, 0xff, 0),
        C::LightBlue => (0x5c, 0x5c, 0xff),
        C::LightMagenta => (0xff, 0, 0xff),
        C::LightCyan => (0, 0xff, 0xff),
        C::White => (0xff, 0xff, 0xff),
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn xterm_256(i: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (0xcd, 0, 0),
        (0, 0xcd, 0),
        (0xcd, 0xcd, 0),
        (0, 0, 0xee),
        (0xcd, 0, 0xcd),
        (0, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0, 0),
        (0, 0xff, 0),
        (0xff, 0xff, 0),
        (0x5c, 0x5c, 0xff),
        (0xff, 0, 0xff),
        (0, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

impl From<&crate::keybindings::KeyBindings> for KeybindingsWire {
    fn from(k: &crate::keybindings::KeyBindings) -> Self {
        use crate::keybindings::Action;
        Self {
            previous_tab: k.config_value(Action::PreviousTab),
            next_tab: k.config_value(Action::NextTab),
            previous_session: k.config_value(Action::PreviousSession),
            next_session: k.config_value(Action::NextSession),
            send_next_key: k.config_value(Action::SendNextKey),
        }
    }
}

impl From<&crate::ui::Theme> for ThemeWire {
    fn from(t: &crate::ui::Theme) -> Self {
        Self {
            name: t.name.clone(),
            bg: color_hex(t.bg),
            panel: color_hex(t.panel),
            panel_alt: color_hex(t.panel_alt),
            selection_bg: color_hex(t.selection_bg),
            text: color_hex(t.text),
            text_muted: color_hex(t.text_muted),
            accent: color_hex(t.accent),
            shadow: color_hex(t.shadow),
            dim_fg: color_hex(t.dim_fg),
            status_running: color_hex(t.status_running),
            status_waiting: color_hex(t.status_waiting),
            status_idle: color_hex(t.status_idle),
            status_error: color_hex(t.status_error),
            status_done: color_hex(t.done_color()),
            terminal: t.terminal.as_ref().map(TerminalWire::from),
        }
    }
}

impl From<&crate::ui::theme::TerminalTheme> for TerminalWire {
    fn from(t: &crate::ui::theme::TerminalTheme) -> Self {
        Self {
            ghostty: t.ghostty.clone(),
            background: color_hex(t.background),
            foreground: color_hex(t.foreground),
            cursor: color_hex(t.cursor),
            selection_bg: color_hex(t.selection_bg),
            selection_fg: color_hex(t.selection_fg),
            palette: t.palette.iter().map(|c| color_hex(*c)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(msg: ClientMsg) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg, "via {json}");
    }

    fn round_trip_server(msg: ServerMsg) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg, "via {json}");
    }

    fn spec() -> SpecWire {
        SpecWire {
            name: "api".into(),
            path: "~/work/api".into(),
            agent: "claude".into(),
            args: "--model opus".into(),
            options: OptionsWire {
                claude: ClaudeWire {
                    session_mode: SessionModeWire::Continue,
                    skip_permissions: true,
                    remote_control: false,
                },
                ..Default::default()
            },
            container_id: None,
            resume: false,
            worktree_branch: Some("feat-x".into()),
        }
    }

    #[test]
    fn hello_matches_the_documented_wire_form() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"t":"hello","protocol":1,"client":"bosun-mac/0.1"}"#).unwrap();
        assert_eq!(
            m,
            ClientMsg::Hello {
                protocol: 1,
                client: Some("bosun-mac/0.1".into())
            }
        );
        round_trip_client(m);
        round_trip_client(ClientMsg::Resync);
    }

    #[test]
    fn every_command_round_trips() {
        let ops = vec![
            CmdOp::Create {
                spec: spec(),
                defer_launch: true,
                section: Some("sec-1".into()),
                replaces: Some("bosun-a-0".into()),
            },
            CmdOp::Kill {
                session: "bosun-a-1".into(),
            },
            CmdOp::KillContainer {
                sessions: vec!["a".into(), "b".into()],
            },
            CmdOp::KillRemoveWorktree {
                session: "s".into(),
                worktree_path: "/r/.worktrees/x".into(),
                branch: "x".into(),
                merge: true,
            },
            CmdOp::Rename {
                session: "s".into(),
                display: "New".into(),
            },
            CmdOp::Restart {
                session: "s".into(),
                resume: true,
                defer_launch: false,
            },
            CmdOp::LaunchAgent {
                session: "s".into(),
                resume: None,
            },
            CmdOp::GetSpec {
                session: "s".into(),
            },
            CmdOp::GetTheme {
                name: "nord".into(),
            },
            CmdOp::Modify {
                session: "s".into(),
                spec: spec(),
            },
            CmdOp::DeleteRecent { recent: 9 },
            CmdOp::SidebarSave {
                sidebar: SidebarWire::default(),
            },
            CmdOp::MoveTab {
                session: "s".into(),
                container_id: Some("con-1".into()),
                index: Some(0),
                section: None,
            },
            CmdOp::MoveTab {
                session: "s".into(),
                container_id: None,
                index: None,
                section: None,
            },
            CmdOp::MoveTab {
                session: "s".into(),
                container_id: None,
                index: Some(2),
                section: Some("sec-1".into()),
            },
            CmdOp::MergeRows {
                from: "con-2".into(),
                into: "con-1".into(),
                index: None,
            },
            CmdOp::MergeRows {
                from: "con-2".into(),
                into: "con-1".into(),
                index: Some(1),
            },
            CmdOp::InsertSection {
                name: "Work".into(),
            },
            CmdOp::SetSectionDefaults {
                section: "sec-1".into(),
                defaults: Some(SectionDefaults {
                    path: "~/work/api".into(),
                    agent: Some("codex".into()),
                    args: "--model o4".into(),
                    options: OptionsWire::default(),
                }),
            },
            CmdOp::SetSectionDefaults {
                section: "sec-1".into(),
                defaults: None,
            },
            CmdOp::ParkRow {
                container_id: "con-1".into(),
                role: SectionRole::Snoozed,
                wake_at: Some(1_760_000_000_000),
                until_needed: true,
            },
            CmdOp::UnparkRow {
                container_id: "con-1".into(),
            },
            CmdOp::RenameSection {
                section: "sec-1".into(),
                name: "Play".into(),
            },
            CmdOp::SetTheme {
                name: "nord".into(),
                persist: false,
            },
            CmdOp::ApplySetting {
                key: "remove_dead_sessions".into(),
                value: serde_json::Value::Bool(true),
            },
            CmdOp::ClientFlags {
                tty: "/dev/ttys004".into(),
                ignore_size: true,
            },
            CmdOp::MakeLatest {
                tty: "/dev/ttys004".into(),
            },
        ];
        for (i, op) in ops.into_iter().enumerate() {
            round_trip_client(ClientMsg::Cmd(CmdEnvelope { id: i as u64, op }));
        }
    }

    #[test]
    fn cmd_is_flat_on_the_wire() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"cmd","id":7,"op":"restart","session":"bosun-api","resume":true}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                id: 7,
                op: CmdOp::Restart {
                    session: "bosun-api".into(),
                    resume: true,
                    defer_launch: false
                }
            })
        );
        // `persist` defaults to true when omitted.
        let m: ClientMsg =
            serde_json::from_str(r#"{"t":"cmd","id":1,"op":"set_theme","name":"nord"}"#).unwrap();
        assert!(matches!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                op: CmdOp::SetTheme { persist: true, .. },
                ..
            })
        ));
    }

    #[test]
    fn move_tab_and_merge_rows_match_the_documented_wire_form() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"cmd","id":3,"op":"move_tab","session":"bosun-a","container_id":null}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                id: 3,
                op: CmdOp::MoveTab {
                    session: "bosun-a".into(),
                    container_id: None,
                    index: None,
                    section: None,
                }
            })
        );
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"cmd","id":4,"op":"move_tab","session":"bosun-a","container_id":"con-1","index":2}"#,
        )
        .unwrap();
        assert!(matches!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                op: CmdOp::MoveTab { index: Some(2), container_id: Some(ref c), .. },
                ..
            }) if c == "con-1"
        ));
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"cmd","id":5,"op":"merge_rows","from":"con-2","into":"con-1"}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                id: 5,
                op: CmdOp::MergeRows {
                    from: "con-2".into(),
                    into: "con-1".into(),
                    index: None,
                }
            })
        );
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"cmd","id":6,"op":"merge_rows","from":"con-2","into":"con-1","index":0}"#,
        )
        .unwrap();
        assert!(matches!(
            m,
            ClientMsg::Cmd(CmdEnvelope {
                op: CmdOp::MergeRows { index: Some(0), .. },
                ..
            })
        ));
    }

    #[test]
    fn unknown_op_is_an_error_not_a_panic() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"cmd","id":1,"op":"nuke"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"bogus"}"#).is_err());
    }

    #[test]
    fn server_messages_round_trip() {
        let session = SessionWire {
            name: "bosun-api-1a2b3c4d".into(),
            display: "api".into(),
            agent: Some("claude".into()),
            path: Some("/w/api".into()),
            windows: 1,
            attached: true,
            created: Some(1_760_000_000),
            container_id: Some("con-1".into()),
            worktree_path: None,
            branch: None,
            pane_title: Some("✳ Fix bug".into()),
            pane_command: Some("2_1_220".into()),
        };
        let theme = ThemeWire::from(&crate::ui::Theme::default_opencode());
        let mut statuses = BTreeMap::new();
        statuses.insert(
            session.name.clone(),
            StatusInfo {
                state: StatusWire::Waiting,
                source: StatusSourceWire::Hook,
                detail: Some("permission: Bash".into()),
                done_at: None,
                started_at: Some(1_760_000_100_000),
            },
        );
        statuses.insert(
            "bosun-web-9c0d1e2f".into(),
            StatusInfo {
                state: StatusWire::Idle,
                source: StatusSourceWire::Hook,
                detail: None,
                done_at: Some(1_760_000_123_456),
                started_at: None,
            },
        );
        let sidebar = SidebarWire {
            ungrouped: vec![ContainerWire {
                id: "con-1".into(),
                name: "api".into(),
                active: session.name.clone(),
                tabs: vec![TabWire {
                    session: session.name.clone(),
                    display: "api".into(),
                    alive: true,
                    recent: None,
                }],
                parked: None,
            }],
            sections: vec![SectionWire {
                id: "sec-1".into(),
                name: "Work".into(),
                collapsed: true,
                banner_font: None,
                defaults: None,
                role: None,
                members: vec![],
            }],
        };
        let config = ConfigWire {
            settings: vec![SettingWire {
                key: "theme".into(),
                value: "opencode".into(),
                choices: Some(vec!["opencode".into(), "nord".into()]),
                pinned_by: Some("BOSUN_THEME".into()),
                read_only: true,
            }],
        };
        let recents = vec![RecentWire {
            id: 3,
            name: "api".into(),
            path: "/w/api".into(),
            agent: "claude".into(),
            args: String::new(),
            options: OptionsWire::default(),
            last_used_at: 1_760_000_000_000,
            use_count: 4,
        }];
        let themes = vec![ThemeEntry {
            name: "opencode".into(),
            source: ThemeSource::Bundled,
        }];
        round_trip_server(ServerMsg::Snapshot(Box::new(Snapshot {
            seq: 1,
            server: ServerInfo {
                version: "2.2.0".into(),
                protocol: PROTOCOL_VERSION,
                tmux_socket: Some("bosun".into()),
            },
            sessions: vec![session.clone()],
            statuses,
            sidebar: sidebar.clone(),
            recents: recents.clone(),
            config: config.clone(),
            theme: theme.clone(),
            theme_light: None,
            theme_dark: Some(theme.clone()),
            themes: themes.clone(),
            keybindings: KeybindingsWire::default(),
            notices: vec!["tmux server is old".into()],
        })));
        round_trip_server(ServerMsg::Sessions {
            seq: 2,
            upsert: vec![session],
            removed: vec!["bosun-old".into()],
        });
        round_trip_server(ServerMsg::Status {
            seq: 3,
            session: "s".into(),
            state: StatusWire::Running,
            source: StatusSourceWire::Detector,
            detail: None,
            done_at: None,
            started_at: Some(1_760_000_100_000),
        });
        round_trip_server(ServerMsg::Status {
            seq: 3,
            session: "s".into(),
            state: StatusWire::Idle,
            source: StatusSourceWire::Hook,
            detail: None,
            done_at: Some(1_760_000_123_456),
            started_at: None,
        });
        round_trip_server(ServerMsg::Sidebar { seq: 4, sidebar });
        round_trip_server(ServerMsg::Recents { seq: 5, recents });
        round_trip_server(ServerMsg::Config { seq: 6, config });
        round_trip_server(ServerMsg::ThemePair {
            seq: 9,
            theme_light: Some(Box::new(theme.clone())),
            theme_dark: None,
        });
        round_trip_server(ServerMsg::ThemePair {
            seq: 9,
            theme_light: None,
            theme_dark: None,
        });
        round_trip_server(ServerMsg::Theme {
            seq: 7,
            theme: Box::new(theme),
        });
        round_trip_server(ServerMsg::Themes { seq: 8, themes });
        round_trip_server(ServerMsg::Keybindings {
            seq: 10,
            keybindings: KeybindingsWire::from(&crate::keybindings::KeyBindings::default()),
        });
        round_trip_server(ServerMsg::Notice {
            seq: 9,
            message: "created x".into(),
        });
        let moved = ServerMsg::ClientSession {
            seq: 11,
            tty: "/dev/ttys026".into(),
            session: "bosun-api-1a2b3c4d".into(),
        };
        assert_eq!(
            serde_json::to_value(&moved).unwrap(),
            serde_json::json!({"t": "client_session", "seq": 11, "tty": "/dev/ttys026", "session": "bosun-api-1a2b3c4d"})
        );
        round_trip_server(moved);
        round_trip_server(ServerMsg::ClientDetached {
            seq: 12,
            tty: "/dev/ttys026".into(),
        });
        round_trip_server(ServerMsg::Ack(Ack {
            id: 7,
            ok: true,
            session: Some("bosun-x".into()),
            spec: Some(spec()),
            pending_launch: Some(PendingLaunchWire {
                session: "bosun-x".into(),
                resume: Some(true),
            }),
            messages: vec!["created bosun-x".into()],
            ..Default::default()
        }));
        round_trip_server(ServerMsg::Error {
            code: "protocol_mismatch".into(),
            message: "update bosun".into(),
        });
    }

    #[test]
    fn done_at_matches_the_documented_wire_form() {
        use crate::agent_hooks::{HookState, StatusSource};
        let msg = ServerMsg::Status {
            seq: 4,
            session: "bosun-web-9c0d1e2f".into(),
            state: StatusWire::Idle,
            source: StatusSourceWire::Hook,
            detail: None,
            done_at: Some(1_760_000_123_456),
            started_at: None,
        };
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            serde_json::json!({
                "t": "status", "seq": 4, "session": "bosun-web-9c0d1e2f",
                "state": "idle", "source": "hook", "done_at": 1_760_000_123_456u64
            })
        );
        // Absent, not null, when there is no done report.
        let mut view = crate::tmux::session::SessionView::new(
            crate::tmux::TmuxSession {
                name: "bosun-web-9c0d1e2f".into(),
                display_name: None,
                windows: 1,
                attached: false,
                created: None,
                last_activity: None,
                current_path: None,
                agent: Some("claude".into()),
                spec_path: None,
                container_id: None,
                worktree_path: None,
                branch: None,
                pane_width: 80,
                pane_title: None,
                pane_command: None,
                hook: None,
                turn_at: None,
            },
            Status::Idle,
            None,
        );
        let plain = serde_json::to_value(StatusInfo::from(&view)).unwrap();
        assert_eq!(
            plain,
            serde_json::json!({"state": "idle", "source": "detector"})
        );
        // A hook-reported done carries the hook's time.
        view.status_source = StatusSource::Hook;
        view.session.hook = Some(HookState {
            status: Status::Done,
            at_ms: 1_760_000_123_456,
            detail: None,
            background: 0,
        });
        view.done_at = Some(1_760_000_123_456);
        assert_eq!(
            serde_json::to_value(StatusInfo::from(&view)).unwrap(),
            serde_json::json!({"state": "idle", "source": "hook", "done_at": 1_760_000_123_456u64})
        );
        // Older clients' messages (no done_at) still parse.
        let old: ServerMsg = serde_json::from_str(
            r#"{"t":"status","seq":5,"session":"s","state":"idle","source":"hook"}"#,
        )
        .unwrap();
        assert!(matches!(old, ServerMsg::Status { done_at: None, .. }));
    }

    #[test]
    fn keybindings_match_the_documented_wire_form() {
        let keys = crate::keybindings::BindingsConfig {
            next_tab: "ctrl+shift+right".into(),
            send_next_key: "none".into(),
            ..Default::default()
        }
        .resolve()
        .unwrap();
        let msg = ServerMsg::Keybindings {
            seq: 10,
            keybindings: KeybindingsWire::from(&keys),
        };
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            serde_json::json!({
                "t": "keybindings", "seq": 10,
                "keybindings": {
                    "previous_tab": "Shift+Left", "next_tab": "Ctrl+Shift+Right",
                    "previous_session": "Shift+Up", "next_session": "Shift+Down",
                    "send_next_key": null
                }
            })
        );
    }

    #[test]
    fn spec_validation() {
        assert!(spec().to_spec().is_ok());
        let mut bad = spec();
        bad.name = "a\nb".into();
        assert!(bad.to_spec().is_err());
        let mut bad = spec();
        bad.agent = "rm -rf".into();
        assert!(bad.to_spec().is_err());
        let mut bad = spec();
        bad.worktree_branch = Some("--force".into());
        assert!(bad.to_spec().is_err());
        // Domain round trip keeps every field.
        let s = spec().to_spec().unwrap();
        assert_eq!(SpecWire::from(&s), spec());
    }

    #[test]
    fn sidebar_round_trips_through_the_model() {
        let mut model = SidebarModel::default();
        model
            .ungrouped
            .push(Container::single("bosun-a-1".into(), "a".into()));
        let mut sec = Section::new("Work");
        sec.members
            .push(Container::single("bosun-b-2".into(), "b".into()));
        model.sections.push(sec);
        let wire = SidebarWire::from_model(
            &model,
            |n| n.to_uppercase(),
            |n| n == "bosun-a-1",
            |_| Some(7),
        );
        assert_eq!(wire.ungrouped[0].tabs[0].display, "BOSUN-A-1");
        assert!(!wire.sections[0].members[0].tabs[0].alive);
        // Only an exited tab says which recent it restarts from.
        assert_eq!(wire.ungrouped[0].tabs[0].recent, None);
        assert_eq!(wire.sections[0].members[0].tabs[0].recent, Some(7));
        // A row's wire name is its label (the active tab's name), not
        // the stored placeholder; `sidebar_save` keeps the stored one.
        assert_eq!(wire.ungrouped[0].name, "BOSUN-A-1");
        assert_eq!(wire.sections[0].members[0].name, "BOSUN-B-2");
        let mut back = wire.to_model().unwrap();
        back.ungrouped[0].name = "a".into();
        back.sections[0].members[0].name = "b".into();
        assert_eq!(back, model);
    }

    #[test]
    fn sidebar_validation_rejects_bad_models() {
        let tab = |s: &str| TabWire {
            session: s.into(),
            display: String::new(),
            alive: true,
            recent: None,
        };
        let c = |id: &str, tabs: Vec<TabWire>, active: &str| ContainerWire {
            id: id.into(),
            name: "x".into(),
            active: active.into(),
            tabs,
            parked: None,
        };
        let dup_session = SidebarWire {
            ungrouped: vec![c("c1", vec![tab("s")], "s"), c("c2", vec![tab("s")], "s")],
            sections: vec![],
        };
        assert!(dup_session.to_model().is_err());
        let bad_active = SidebarWire {
            ungrouped: vec![c("c1", vec![tab("s")], "t")],
            sections: vec![],
        };
        assert!(bad_active.to_model().is_err());
        let empty = SidebarWire {
            ungrouped: vec![c("c1", vec![], "s")],
            sections: vec![],
        };
        assert!(empty.to_model().is_err());
    }

    #[test]
    fn theme_pair_matches_the_documented_wire_form() {
        let nord = ThemeWire::from(&crate::ui::Theme::builtin("nord").unwrap());
        let msg = ServerMsg::ThemePair {
            seq: 9,
            theme_light: None,
            theme_dark: Some(Box::new(nord)),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["t"], "theme_pair");
        assert_eq!(v["seq"], 9);
        assert!(v.get("theme_light").is_none(), "unset is left out: {v}");
        assert_eq!(v["theme_dark"]["name"], "nord");
        assert_eq!(v["theme_dark"]["bg"], "#2e3440");
        // `null` reads as unset too.
        let back: ServerMsg = serde_json::from_str(
            r#"{"t":"theme_pair","seq":3,"theme_light":null,"theme_dark":null}"#,
        )
        .unwrap();
        assert_eq!(
            back,
            ServerMsg::ThemePair {
                seq: 3,
                theme_light: None,
                theme_dark: None
            }
        );
    }

    #[test]
    fn theme_wire_carries_the_terminal_table_when_there_is_one() {
        let w = ThemeWire::from(&crate::ui::Theme::builtin("nord").unwrap());
        let v = serde_json::to_value(&w).unwrap();
        let t = &v["terminal"];
        assert_eq!(t["ghostty"], "Nord");
        assert_eq!(t["background"], "#2e3440");
        assert_eq!(t["foreground"], "#d8dee9");
        assert_eq!(t["cursor"], "#eceff4");
        assert_eq!(t["selection_bg"], "#eceff4");
        assert_eq!(t["selection_fg"], "#4c566a");
        assert_eq!(t["palette"].as_array().unwrap().len(), 16);
        assert_eq!(t["palette"][1], "#bf616a");
        let back: ThemeWire = serde_json::from_value(v).unwrap();
        assert_eq!(back, w);
        // A theme without a `ghostty` name leaves the key out.
        let opencode =
            serde_json::to_value(ThemeWire::from(&crate::ui::Theme::default_opencode())).unwrap();
        assert!(opencode["terminal"].get("ghostty").is_none(), "{opencode}");
    }

    #[test]
    fn theme_wire_without_a_terminal_table_leaves_the_key_out() {
        let mut theme = crate::ui::Theme::default_opencode();
        theme.terminal = None;
        let v = serde_json::to_value(ThemeWire::from(&theme)).unwrap();
        assert!(v.get("terminal").is_none(), "{v}");
        // An older daemon's theme (no key at all) still decodes.
        let back: ThemeWire = serde_json::from_value(v).unwrap();
        assert_eq!(back.terminal, None);
    }

    #[test]
    fn theme_colors_are_hex() {
        let w = ThemeWire::from(&crate::ui::Theme::default_opencode());
        for c in [&w.bg, &w.accent, &w.status_done, &w.status_running] {
            assert!(c.len() == 7 && c.starts_with('#'), "{c}");
        }
        assert_eq!(
            color_hex(ratatui::style::Color::Rgb(0x7c, 0x5c, 0xff)),
            "#7c5cff"
        );
        assert_eq!(color_hex(ratatui::style::Color::Indexed(196)), "#ff0000");
        assert_eq!(color_hex(ratatui::style::Color::Indexed(232)), "#080808");
    }
}
