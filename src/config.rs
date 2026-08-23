//! Runtime configuration. Values are read once at startup and passed
//! around by value, so the rest of the code never touches `std::env`
//! or the config file on disk.
//!
//! Sources, in order of precedence (lowest to highest):
//!   1. Built-in defaults.
//!   2. `$XDG_CONFIG_HOME/bosun/config.toml` (`~/Library/Application
//!      Support/dev.yetidevworks.bosun/config.toml` on macOS).
//!   3. Environment variables (`BOSUN_PREFIX`, `BOSUN_TMUX_SOCKET`,
//!      `BOSUN_THEME`).
//!
//! Env vars always win. A missing or malformed config file is
//! non-fatal — we log a warning and fall through to defaults.

use std::env;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::sidebar::{Container, Section, SidebarModel};

/// Legacy Vec<SidebarEntry> shape from v0.2.8. Read-only, used for
/// one-time migration to the new explicit-membership `SidebarModel`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacySidebarEntry {
    Section { id: String, name: String },
    Session { internal: String },
}

fn migrate_legacy_sidebar(old: Vec<LegacySidebarEntry>) -> SidebarModel {
    // Pre-0.2.9 membership was implicit (session belongs to the
    // nearest section above). The new model is explicit — creating a
    // section claims no one. Safest upgrade: put every session in
    // ungrouped and preserve section headers as empty. The user can
    // then populate them with Shift-Right.
    let mut model = SidebarModel::default();
    for e in old {
        match e {
            LegacySidebarEntry::Section { id, name } => {
                model.sections.push(Section {
                    id,
                    name,
                    members: Vec::new(),
                    collapsed: false,
                    banner_font: None,
                });
            }
            LegacySidebarEntry::Session { internal } => {
                model
                    .ungrouped
                    .push(Container::single(internal.clone(), internal));
            }
        }
    }
    model
}

/// The default prefix that bosun considers "managed". Only tmux
/// sessions whose name starts with this prefix appear in bosun's UI
/// and get the bosun status bar applied. Set `BOSUN_PREFIX=` (empty)
/// to see every session on the server.
pub const DEFAULT_SESSION_PREFIX: &str = "bosun-";

/// Default tmux `-L <socket>` that bosun uses. Putting bosun on its
/// own socket means bosun's tmux server is a **child of the bosun
/// process**, which inherits whatever shell context bosun was
/// launched from — critically, including the macOS Keychain lineage
/// that lets Claude Code see its cached credentials. With the default
/// socket, bosun's sessions would live on some ancient server started
/// by some other context and Claude wouldn't see the user's auth.
///
/// Set `BOSUN_TMUX_SOCKET=default` to opt back into the shared
/// default socket (at the cost of the auth issue and of seeing every
/// other tmux session on the machine).
pub const DEFAULT_TMUX_SOCKET: &str = "bosun";

/// Default theme name — must match a built-in in `ui::theme`.
pub const DEFAULT_THEME: &str = "opencode";

/// Every agent the new-session form can launch, in the order its
/// selector cycles through them. Canonical list: the UI reads it for
/// the selector and `default_agent` validates against it, so adding an
/// agent here is enough for both.
pub const AGENTS: &[&str] = &["claude", "codex", "kimi", "opencode", "qwen", "terminal"];

/// Whether a session row is dropped from the sidebar once its tmux
/// session goes away. Off by default: the row keeps its place so `R`
/// can restart it, and losing a grouped layout is worse than leaving a
/// row labelled "exited".
pub const DEFAULT_REMOVE_DEAD_SESSIONS: bool = false;

/// Agent preselected when creating a session unless overridden in
/// `config.toml` with `default_agent`.
pub const DEFAULT_AGENT: &str = "claude";

fn default_agent(value: Option<String>) -> String {
    let Some(value) = value else {
        return DEFAULT_AGENT.to_string();
    };
    let value = value.trim().to_ascii_lowercase();
    if AGENTS.contains(&value.as_str()) {
        value
    } else {
        tracing::warn!(
            configured_agent = %value,
            default_agent = DEFAULT_AGENT,
            "ignoring invalid default agent in config"
        );
        DEFAULT_AGENT.to_string()
    }
}

/// Where a new git worktree is placed relative to its repo root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeLocation {
    /// `<repo>/.worktrees/<branch>` — self-contained inside the repo.
    #[default]
    Subdir,
    /// `<repo>-<branch>` — a sibling directory next to the repo.
    Sibling,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Only sessions whose name starts with this prefix are shown in
    /// bosun's UI and get the bosun status bar applied. Empty string
    /// means "show everything".
    pub session_prefix: String,
    /// Tmux `-L` socket name. `None` means use tmux's default socket.
    /// `Some("bosun")` (the default) means `tmux -L bosun ...`.
    pub tmux_socket: Option<String>,
    /// Name of the tmux session bosun is currently running inside,
    /// if any. `None` if bosun was launched outside tmux. We exclude
    /// this session from bosun's own list so the preview doesn't
    /// capture bosun itself (which would create a visual feedback
    /// loop: bosun renders a preview of itself, which shows bosun
    /// rendering a preview of itself, etc).
    pub self_session: Option<String>,
    /// Theme name. Resolved against user themes first, then
    /// built-ins (`opencode`, `tokyonight`), then a hard-fallback.
    pub theme: String,
    /// Persisted divider position (absolute terminal column). `None`
    /// means use the default 38% split.
    pub divider_x: Option<u16>,
    /// User-defined sidebar ordering with explicit section
    /// membership. Empty on first launch. Persisted as the
    /// `[sidebar]` table in `config.toml` so ordering and group
    /// structure survive bosun restarts.
    pub sidebar: SidebarModel,
    /// Map from session display name → last-known section name.
    /// When a session is killed/restarted or a recent is opened,
    /// the new session is placed back into that section if one with
    /// the same name still exists. Persisted as `[session_history]`.
    pub session_history: std::collections::HashMap<String, String>,
    /// Global TDF banner font used for the section/empty preview
    /// banner. Per-section overrides live on `Section.banner_font`.
    /// Persisted in `config.toml` as `banner_font = "metalix"`.
    pub banner_font: String,
    /// External editor command launched by the `e` key on a highlighted
    /// session — bosun runs `<editor> <session_path>` detached. `None`
    /// means "no editor configured"; pressing `e` warns in the status
    /// bar. Set via `bosun editor <cmd>` or directly in `config.toml`
    /// as `editor = "zed"` (or `code`, `subl`, `nvim`, ...).
    pub editor: Option<String>,
    /// Fast preview tick (milliseconds): how often the tmux actor
    /// re-captures the *focused* session's pane to push a fresh
    /// preview to the UI. Independent of the 1Hz full-refresh tick
    /// that updates the session list + status detectors + statusbar.
    /// Default 200ms (5 fps) keeps the preview perceptually live for
    /// the cost of one `capture-pane` per tick. 0 disables the fast
    /// tick (falls back to v0.x behavior — preview only updates on
    /// full refresh). Override with `preview_tick_ms = 250` in
    /// `config.toml` or `BOSUN_PREVIEW_TICK_MS=300` in the env.
    pub preview_tick_ms: u64,
    /// Embedded-terminal preview (2.0+): when true, the focused
    /// session's preview pane is a real `vt100`-parsed embedded
    /// terminal streaming from `tmux attach -r`, rather than a
    /// periodic `capture-pane` snapshot. Default true. Disable with
    /// `embed = false` in `config.toml` or `BOSUN_EMBED=0` /
    /// `BOSUN_EMBED=off` in the env to fall back to the v0.4 polled
    /// preview path. Non-focused sessions and section/empty-state
    /// previews are unaffected — they always use the polled path
    /// (they don't need a PTY).
    pub embed_enabled: bool,
    /// Single-window mode (2.0+): when true, `Enter` / `Right` on a
    /// session opens it *inside* the preview pane (focused embed)
    /// instead of tearing down ratatui and running a full-screen
    /// `tmux attach`. The sidebar stays visible the whole time.
    /// `Ctrl-Q` exits back to bosun navigation, same as it does
    /// from a real tmux attach. Default false (matches v0.4
    /// behavior). Toggled live with `s` and persisted to
    /// `config.toml` as `single_window = true`. Env override:
    /// `BOSUN_SINGLE_WINDOW=1|true|yes|on` enables, anything else
    /// disables.
    pub single_window_mode: bool,
    /// Sticky "hide the sidebar while focused on a session" preference
    /// (2.0.5+). Toggled live with `Ctrl+B` while the embed is
    /// focused; persisted to `config.toml` as `sidebar_hidden = true`.
    /// Only takes effect while focused — detaching always brings the
    /// sidebar back so the session list is reachable. Default false.
    pub sidebar_hidden: bool,
    /// When true, sessions that belong to a section render as
    /// `group/session` in the tab strip pills and the OSC terminal
    /// window title; ungrouped sessions stay bare. Display only — no
    /// persistence. Default false. Set `show_group_in_title = true` in
    /// `config.toml` or `BOSUN_SHOW_GROUP_IN_TITLE=1|true|yes|on`.
    pub show_group_in_title: bool,
    /// Where `git worktree add` places new worktrees. See `WorktreeLocation`.
    pub worktree_location: WorktreeLocation,
    /// Agent preselected by the new-session form. Invalid or missing
    /// values fall back to `claude`.
    pub default_agent: String,
    /// Drop a session's sidebar row when its tmux session ends, rather
    /// than keeping it as an `exited` row that `R` can restart.
    pub remove_dead_sessions: bool,
    /// Per-agent binary overrides from the `[agents]` table in
    /// `config.toml`, e.g. `opencode = "~/bin/opencode-wrapper"`.
    /// The value replaces the agent's binary in the launch command
    /// verbatim, so a wrapper script can fix up the environment
    /// before exec'ing the real binary. Missing entries fall back to
    /// the agent's own name resolved on the login-shell PATH.
    pub agent_binaries: std::collections::HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
            tmux_socket: Some(DEFAULT_TMUX_SOCKET.to_string()),
            self_session: None,
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
            editor: None,
            preview_tick_ms: DEFAULT_PREVIEW_TICK_MS,
            embed_enabled: DEFAULT_EMBED_ENABLED,
            // v2.0.2+: focused single-window mode is the only mode.
            // The field is retained as `true` for callers that still
            // gate on it, but it is no longer user-toggleable or
            // persisted.
            single_window_mode: true,
            sidebar_hidden: false,
            show_group_in_title: DEFAULT_SHOW_GROUP_IN_TITLE,
            worktree_location: WorktreeLocation::default(),
            default_agent: DEFAULT_AGENT.to_string(),
            remove_dead_sessions: DEFAULT_REMOVE_DEAD_SESSIONS,
            agent_binaries: std::collections::HashMap::new(),
        }
    }
}

/// Default fast preview tick in milliseconds. 200ms = 5 fps on the
/// focused session's preview pane. See `Config::preview_tick_ms`.
pub const DEFAULT_PREVIEW_TICK_MS: u64 = 200;

/// Default for `Config::embed_enabled`. v2.0+ ships with the
/// embedded-terminal preview on; set to false here to invert the
/// default if early adopters report regressions.
pub const DEFAULT_EMBED_ENABLED: bool = true;

/// Default for `Config::show_group_in_title`. Off by default so the
/// tab pills and OSC title look unchanged for existing users until they
/// opt in via `show_group_in_title = true` or `BOSUN_SHOW_GROUP_IN_TITLE=1`.
pub const DEFAULT_SHOW_GROUP_IN_TITLE: bool = false;

/// Shape of `config.toml` on disk. All fields are optional and
/// defaulted independently so a half-written file still loads.
/// `Serialize` is used by `write_theme` to round-trip the file on
/// disk: read → update one field → write.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    divider_x: Option<u16>,
    /// Explicit-membership sidebar (v0.2.9+). Tables + arrays.
    /// `read_config_file` preprocesses the raw TOML so a legacy v0.2.8
    /// `sidebar = [...]` array is migrated in-place to the new shape
    /// before this field is deserialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar: Option<SidebarModel>,
    /// Last-known section name per display name. Used to restore
    /// group membership across kill/restart/recents flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_history: Option<std::collections::HashMap<String, String>>,
    /// Legacy pre-0.2.8 flat list of internal names. Read-only for
    /// migration; never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_order: Option<Vec<String>>,
    /// Global TDF banner font name (e.g. `"metalix"`). When absent,
    /// `Config::load` falls back to `banner::default_name()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    banner_font: Option<String>,
    /// External editor command for the `e`-key launch. `None` means
    /// unset; `e` will warn until the user configures one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    editor: Option<String>,
    /// Fast preview tick in milliseconds. See `Config::preview_tick_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview_tick_ms: Option<u64>,
    /// Embedded-terminal preview opt-out. See `Config::embed_enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embed: Option<bool>,
    /// Single-window mode persistence. See `Config::single_window_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    single_window: Option<bool>,
    /// Sticky hide-sidebar-while-focused preference. See
    /// `Config::sidebar_hidden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar_hidden: Option<bool>,
    /// Group-in-title opt-in. See `Config::show_group_in_title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    show_group_in_title: Option<bool>,
    /// Worktree placement scheme. See `Config::worktree_location`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_location: Option<WorktreeLocation>,
    /// Agent preselected by the new-session form. Must be one of
    /// `claude`, `codex`, `kimi`, `opencode`, `qwen`, or `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_agent: Option<String>,
    /// See `Config::remove_dead_sessions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remove_dead_sessions: Option<bool>,
    /// Per-agent binary overrides. See `Config::agent_binaries`.
    /// Persisted as the `[agents]` table:
    /// ```toml
    /// [agents]
    /// opencode = "/Users/me/bin/opencode-wrapper"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agents: Option<std::collections::HashMap<String, String>>,
}

impl Config {
    /// Full load path: defaults → config.toml → env vars. See the
    /// module-level doc comment for the precedence order.
    pub fn load() -> Self {
        let file = read_config_file().unwrap_or_default();

        let session_prefix = env::var("BOSUN_PREFIX")
            .ok()
            .or(file.session_prefix)
            .unwrap_or_else(|| DEFAULT_SESSION_PREFIX.to_string());

        let tmux_socket = match env::var("BOSUN_TMUX_SOCKET") {
            Ok(s) if s.is_empty() || s == "default" => None,
            Ok(s) => Some(s),
            Err(_) => match file.tmux_socket.as_deref() {
                Some("") | Some("default") => None,
                Some(s) => Some(s.to_string()),
                None => Some(DEFAULT_TMUX_SOCKET.to_string()),
            },
        };

        let theme = env::var("BOSUN_THEME")
            .ok()
            .or(file.theme)
            .unwrap_or_else(|| DEFAULT_THEME.to_string());

        // Only detect self-session if we're on the same socket as
        // the caller's tmux. If bosun uses a dedicated socket, the
        // parent tmux (if any) is on a different server and bosun
        // isn't "inside" any session on its own socket.
        let self_session = if tmux_socket.is_none() {
            detect_self_session()
        } else {
            None
        };

        let divider_x = file.divider_x;
        // Prefer the new `sidebar` field (explicit-membership model).
        // If absent, migrate the pre-0.2.8 flat `session_order` list.
        // Legacy v0.2.8 `sidebar = [...]` arrays are migrated by
        // `read_config_file` before we get here.
        let sidebar = match file.sidebar {
            Some(s) => s,
            None => {
                let ungrouped = file
                    .session_order
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| Container::single(n.clone(), n))
                    .collect();
                SidebarModel {
                    ungrouped,
                    sections: Vec::new(),
                }
            }
        };

        let session_history = file.session_history.unwrap_or_default();
        let banner_font = file
            .banner_font
            .unwrap_or_else(|| crate::ui::banner::default_name().to_string());
        // Editor is intentionally not env-overridable. It's a persistent
        // user preference, not a per-session knob, and putting it on
        // $EDITOR would conflict with the conventional terminal-editor
        // meaning ($EDITOR is usually `vim` / `nvim` — not what a user
        // wants the `e` key to spawn against a project path).
        let editor = file.editor.filter(|e| !e.trim().is_empty());

        // Env var wins over config file, both win over the default.
        // Parse failure of either falls through silently to the next
        // source — bad values shouldn't brick startup.
        let preview_tick_ms = env::var("BOSUN_PREVIEW_TICK_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .or(file.preview_tick_ms)
            .unwrap_or(DEFAULT_PREVIEW_TICK_MS);

        // Embed opt-out: accept `0`, `false`, `off`, `no` (case-
        // insensitive) as disable; anything else as enable. Mirrors
        // the conventional shell-flag idiom. Env beats file beats
        // default.
        let embed_enabled = match env::var("BOSUN_EMBED") {
            Ok(s) => !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => file.embed.unwrap_or(DEFAULT_EMBED_ENABLED),
        };

        // v2.0.2+: focused single-window is the only mode. The
        // setting is no longer user-toggleable; we keep the field
        // wired as `true` for code paths that gate on it.
        let single_window_mode = true;

        // Sticky preference only — no env override. It's flipped at
        // runtime via Ctrl+B and read back on next launch.
        let sidebar_hidden = file.sidebar_hidden.unwrap_or(false);

        // Group-in-title opt-in. Same enable/disable idiom as
        // BOSUN_EMBED, except an empty value (`BOSUN_SHOW_GROUP_IN_TITLE=`)
        // disables here rather than enabling: this flag is off by
        // default, so a bare/empty set value should not silently flip it
        // on. Env beats file beats default.
        let show_group_in_title = match env::var("BOSUN_SHOW_GROUP_IN_TITLE") {
            Ok(s) => !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "off" | "no"
            ),
            Err(_) => file
                .show_group_in_title
                .unwrap_or(DEFAULT_SHOW_GROUP_IN_TITLE),
        };

        let worktree_location = file.worktree_location.unwrap_or_default();
        let default_agent = default_agent(file.default_agent);
        let remove_dead_sessions = file
            .remove_dead_sessions
            .unwrap_or(DEFAULT_REMOVE_DEAD_SESSIONS);

        let agent_binaries = file.agents.unwrap_or_default();

        Self {
            session_prefix,
            tmux_socket,
            self_session,
            theme,
            divider_x,
            sidebar,
            session_history,
            banner_font,
            editor,
            preview_tick_ms,
            embed_enabled,
            single_window_mode,
            sidebar_hidden,
            show_group_in_title,
            worktree_location,
            default_agent,
            remove_dead_sessions,
            agent_binaries,
        }
    }

    /// Back-compat shim for callers that only want env-driven config.
    /// Retained so tests and a few internal paths don't need to
    /// touch the filesystem.
    pub fn from_env() -> Self {
        Self::load()
    }

    /// Does `name` pass the managed-session filter?
    pub fn manages(&self, name: &str) -> bool {
        // Never manage the session bosun is running in — that causes
        // the recursive preview feedback loop.
        if self.self_session.as_deref() == Some(name) {
            return false;
        }
        self.session_prefix.is_empty() || name.starts_with(&self.session_prefix)
    }
}

/// Location of bosun's config directory. Same `ProjectDirs` entry
/// the SQLite store uses, so `config.toml` lives alongside
/// `bosun.db` on macOS.
pub fn config_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "yetidevworks", "bosun").map(|d| d.config_dir().to_path_buf())
}

/// Where user-defined themes live — one `.toml` per theme, file name
/// without extension is the theme name.
pub fn user_themes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("themes"))
}

fn read_config_file() -> Option<ConfigFile> {
    let path = config_dir()?.join("config.toml");
    let s = std::fs::read_to_string(&path).ok()?;

    // Parse as a generic Value first so we can detect + migrate the
    // legacy v0.2.8 `sidebar = [...]` array shape. The v0.2.9 shape
    // is a `[sidebar]` table, so a sidebar Value that's an Array is
    // unambiguously the old form.
    let mut value: toml::Value = match toml::from_str(&s) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse {:?}: {}", path, e);
            return None;
        }
    };

    if let Some(table) = value.as_table_mut() {
        if let Some(sidebar) = table.get("sidebar") {
            if sidebar.is_array() {
                let cloned = sidebar.clone();
                match cloned.try_into::<Vec<LegacySidebarEntry>>() {
                    Ok(legacy) => {
                        let migrated = migrate_legacy_sidebar(legacy);
                        match toml::Value::try_from(&migrated) {
                            Ok(v) => {
                                table.insert("sidebar".to_string(), v);
                            }
                            Err(e) => {
                                tracing::warn!("failed to serialize migrated sidebar: {}", e);
                                table.remove("sidebar");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse legacy sidebar: {}", e);
                        table.remove("sidebar");
                    }
                }
            }
        }
    }

    match value.try_into::<ConfigFile>() {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("failed to deserialize {:?}: {}", path, e);
            None
        }
    }
}

/// Persist a new theme choice to `config.toml`. Read-modify-write:
/// existing file fields are preserved, only `theme` is updated. If
/// the file doesn't exist it's created. Returns `Err` if the config
/// dir can't be resolved or writing fails — callers (the theme
/// picker) surface this as a warning in the status bar but still
/// apply the change to the live UI.
pub fn write_theme(name: &str) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.theme = Some(name.to_string());

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    // Atomic write: temp file + rename. Avoids a half-written
    // config.toml if bosun is killed mid-write.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the sticky hide-sidebar-while-focused preference to
/// `config.toml`. Same read-modify-write approach as `write_theme`.
/// Called from the `Ctrl+B` toggle; failure is surfaced as a status
/// bar warning but the live toggle still applies.
pub fn write_sidebar_hidden(hidden: bool) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.sidebar_hidden = Some(hidden);

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the global banner font to `config.toml`. Same
/// read-modify-write approach as `write_theme`.
pub fn write_banner_font(name: &str) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.banner_font = Some(name.to_string());

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the editor command to `config.toml`. Pass `None` to clear
/// the field. Same read-modify-write approach as `write_theme` so
/// other fields survive intact.
pub fn write_editor(editor: Option<&str>) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = read_config_file().unwrap_or_default();
    file.editor = editor
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the single-window-mode flag to `config.toml`. Same
/// read-modify-write approach as `write_theme`. Writes `None`
/// (skipped via `skip_serializing_if`) when the value is the
/// default-false so the file stays clean of redundant entries.
pub fn write_single_window(on: bool) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.single_window = if on { Some(true) } else { None };

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the divider position to `config.toml`. Same
/// read-modify-write approach as `write_theme`.
pub fn write_divider_x(x: Option<u16>) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    file.divider_x = x;

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the session-history map (display_name → section_name) to
/// `config.toml`. An empty map clears the field.
pub fn write_session_history(
    history: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let mut file = read_config_file().unwrap_or_default();
    file.session_history = if history.is_empty() {
        None
    } else {
        Some(history.clone())
    };

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persist the user-defined sidebar (explicit-membership model) to
/// `config.toml`. Same read-modify-write approach as `write_theme`.
/// An empty model clears the field from the file. Also drops the
/// legacy `session_order` so the config converges on the new shape.
pub fn write_sidebar(model: &SidebarModel) -> std::io::Result<()> {
    let dir =
        config_dir().ok_or_else(|| std::io::Error::other("cannot resolve bosun config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    // Read via the migrating reader so a pre-existing legacy sidebar
    // is converted before we overwrite it. Otherwise the first save
    // after a legacy read-through would wipe the user's groups.
    let mut file = read_config_file().unwrap_or_default();
    file.sidebar = if model.ungrouped.is_empty() && model.sections.is_empty() {
        None
    } else {
        Some(model.clone())
    };
    file.session_order = None;

    let body = toml::to_string(&file)
        .map_err(|e| std::io::Error::other(format!("toml serialize: {e}")))?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// If `$TMUX` is set, ask tmux for the current session name. Used to
/// exclude bosun's own session from its list.
fn detect_self_session() -> Option<String> {
    if env::var("TMUX").is_err() {
        return None;
    }
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(prefix: &str) -> Config {
        Config {
            session_prefix: prefix.to_string(),
            tmux_socket: Some(DEFAULT_TMUX_SOCKET.to_string()),
            self_session: None,
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
            editor: None,
            preview_tick_ms: DEFAULT_PREVIEW_TICK_MS,
            embed_enabled: DEFAULT_EMBED_ENABLED,
            single_window_mode: false,
            sidebar_hidden: false,
            show_group_in_title: DEFAULT_SHOW_GROUP_IN_TITLE,
            worktree_location: WorktreeLocation::default(),
            default_agent: DEFAULT_AGENT.to_string(),
            remove_dead_sessions: DEFAULT_REMOVE_DEAD_SESSIONS,
            agent_binaries: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn default_prefix_matches_bosun_sessions() {
        let c = cfg(DEFAULT_SESSION_PREFIX);
        assert!(c.manages("bosun-work"));
        assert!(c.manages("bosun-"));
        assert!(!c.manages("agentdeck-work"));
        assert!(!c.manages("main"));
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let c = cfg("");
        assert!(c.manages("anything"));
        assert!(c.manages(""));
    }

    #[test]
    fn custom_prefix_matches_its_namespace() {
        let c = cfg("work-");
        assert!(c.manages("work-api"));
        assert!(!c.manages("bosun-api"));
    }

    #[test]
    fn self_session_is_excluded_even_when_prefix_matches() {
        let c = Config {
            session_prefix: DEFAULT_SESSION_PREFIX.to_string(),
            tmux_socket: None,
            self_session: Some("bosun-mine-abc".to_string()),
            theme: DEFAULT_THEME.to_string(),
            divider_x: None,
            sidebar: SidebarModel::default(),
            session_history: std::collections::HashMap::new(),
            banner_font: crate::ui::banner::default_name().to_string(),
            editor: None,
            preview_tick_ms: DEFAULT_PREVIEW_TICK_MS,
            embed_enabled: DEFAULT_EMBED_ENABLED,
            single_window_mode: false,
            sidebar_hidden: false,
            show_group_in_title: DEFAULT_SHOW_GROUP_IN_TITLE,
            worktree_location: WorktreeLocation::default(),
            default_agent: DEFAULT_AGENT.to_string(),
            remove_dead_sessions: DEFAULT_REMOVE_DEAD_SESSIONS,
            agent_binaries: std::collections::HashMap::new(),
        };
        assert!(!c.manages("bosun-mine-abc"));
        assert!(c.manages("bosun-other-xyz"));
    }

    #[test]
    fn default_agent_accepts_opencode_and_falls_back_to_claude() {
        assert_eq!(default_agent(Some("opencode".to_string())), "opencode");
        assert_eq!(default_agent(None), DEFAULT_AGENT);
        assert_eq!(default_agent(Some("unknown".to_string())), DEFAULT_AGENT);
    }

    /// Validation reads `AGENTS`, so a newly added agent is
    /// automatically a valid `default_agent` — no second list to keep
    /// in sync.
    #[test]
    fn every_agent_is_a_valid_default() {
        for agent in AGENTS {
            assert_eq!(default_agent(Some((*agent).to_string())), *agent);
        }
    }

    #[test]
    fn default_agent_tolerates_case_and_whitespace() {
        assert_eq!(default_agent(Some("  OpenCode \n".to_string())), "opencode");
    }

    #[test]
    fn config_file_fields_parse() {
        let src = r#"
            session_prefix = "work-"
            tmux_socket = "scratch"
            theme = "tokyonight"
        "#;
        let parsed: ConfigFile = toml::from_str(src).unwrap();
        assert_eq!(parsed.session_prefix.as_deref(), Some("work-"));
        assert_eq!(parsed.tmux_socket.as_deref(), Some("scratch"));
        assert_eq!(parsed.theme.as_deref(), Some("tokyonight"));
    }

    #[test]
    fn empty_config_file_is_all_defaults() {
        let parsed: ConfigFile = toml::from_str("").unwrap();
        assert!(parsed.session_prefix.is_none());
        assert!(parsed.tmux_socket.is_none());
        assert!(parsed.theme.is_none());
    }

    #[test]
    fn editor_field_parses() {
        let parsed: ConfigFile = toml::from_str(r#"editor = "zed""#).unwrap();
        assert_eq!(parsed.editor.as_deref(), Some("zed"));
    }

    #[test]
    fn show_group_in_title_defaults_off() {
        assert!(!Config::default().show_group_in_title);
    }

    #[test]
    fn worktree_location_parses_and_defaults() {
        let parsed: ConfigFile = toml::from_str(r#"worktree_location = "sibling""#).unwrap();
        assert_eq!(parsed.worktree_location, Some(WorktreeLocation::Sibling));
        let empty: ConfigFile = toml::from_str("").unwrap();
        assert_eq!(empty.worktree_location, None);
        // Default when unset.
        assert_eq!(WorktreeLocation::default(), WorktreeLocation::Subdir);
    }
}
