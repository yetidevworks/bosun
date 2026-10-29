//! `bosun serve`: the tmux actor without the TUI, behind a Unix socket.
//!
//! A GUI (the planned Mac app) connects, says `hello`, gets one
//! `snapshot` of everything it needs to draw a sidebar, then a stream
//! of deltas, and sends commands that are acknowledged by id. The
//! protocol is described in `docs/protocol.md`; the wire types live in
//! [`protocol`] and the framing in [`frame`].
//!
//! The daemon reuses the TUI's own machinery rather than porting it:
//! the same [`tmux_actor`] does all tmux work, and a headless
//! [`AppState`] runs the same reducer the TUI uses for the session
//! list, so sidebar reconcile, section history and container handling
//! behave identically. Settings and sidebar changes are written to the
//! same `config.toml` through the same functions.
//!
//! Single instance: a `flock` on a lock file next to the socket. The
//! daemon exits [`ServeOptions::idle_timeout`] after its last client
//! disconnects (or after starting, if nobody connects).

pub mod frame;
pub mod protocol;
mod turns;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::actors::tmux_actor;
use crate::app::AppState;
use crate::config::{Config, WorktreeLocation};
use crate::error::{BosunError, Result};
use crate::events::{AppMsg, Command, SettingChange};
use crate::sidebar::{SectionRole, SidebarModel};
use crate::store::Store;
use crate::tmux::TokioTmuxClient;
use crate::ui::Theme;

use protocol::{
    Ack, ClientMsg, CmdEnvelope, CmdOp, ConfigWire, KeybindingsWire, PendingLaunchWire, RecentWire,
    SearchAction, SearchWire, ServerInfo, ServerMsg, SessionWire, SettingWire, SidebarWire,
    Snapshot, SpecWire, StatusInfo, ThemeEntry, ThemeSource, ThemeWire, PROTOCOL_VERSION,
};

/// Default for [`ServeOptions::idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Exit this long after the last client disconnects.
    pub idle_timeout: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// The daemon's socket: `bosun.sock` in Bosun's data directory, next
/// to `bosun.db`. The one place this path is decided.
pub fn socket_path() -> Result<PathBuf> {
    Ok(crate::store::data_dir()?.join("bosun.sock"))
}

/// The single-instance lock file, next to the socket.
pub fn lock_path() -> Result<PathBuf> {
    Ok(crate::store::data_dir()?.join("bosun-serve.lock"))
}

/// Errors `bosun serve` reports to the user before it starts serving.
#[derive(Debug)]
pub enum StartError {
    AlreadyRunning { pid: Option<u32> },
    Other(BosunError),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "bosun serve is already running (pid {pid})")
            }
            StartError::AlreadyRunning { pid: None } => write!(f, "bosun serve is already running"),
            StartError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<BosunError> for StartError {
    fn from(e: BosunError) -> Self {
        StartError::Other(e)
    }
}

impl From<std::io::Error> for StartError {
    fn from(e: std::io::Error) -> Self {
        StartError::Other(BosunError::Io(e))
    }
}

/// Run the daemon until it goes idle or gets SIGINT/SIGTERM.
pub async fn run(opts: ServeOptions) -> std::result::Result<(), StartError> {
    let sock_path = socket_path()?;
    let dir = sock_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| BosunError::Store("socket path has no parent".into()))?;
    std::fs::create_dir_all(&dir)?;
    let _lock = InstanceLock::acquire(&lock_path()?)?;

    // sun_path is 104 bytes on macOS, 108 on Linux.
    if sock_path.as_os_str().len() >= 104 {
        return Err(BosunError::Io(std::io::Error::other(format!(
            "socket path is too long for a Unix socket: {}",
            sock_path.display()
        )))
        .into());
    }
    // We hold the lock, so any socket file left over is stale.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    set_owner_only(&sock_path);

    let config = Config::load();
    let store = Arc::new(match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("store open failed, using in-memory: {}", e);
            Store::in_memory().expect("in-memory store cannot fail")
        }
    });
    let client = Arc::new(match &config.tmux_socket {
        Some(s) => TokioTmuxClient::with_socket(s.clone()),
        None => TokioTmuxClient::new(),
    });

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AppMsg>();
    // Every create defers the agent launch inside the actor; the
    // daemon then launches right away or leaves it to the client
    // (`defer_launch`). See `CmdOp::Create`.
    let mut actor_config = config.clone();
    actor_config.embed_enabled = true;
    let actor = tmux_actor::spawn(
        client.clone(),
        config.tmux_socket.clone(),
        actor_config,
        store.clone(),
        cmd_rx,
        evt_tx,
    );
    // No preview to keep live: park the actor's fast capture tick.
    let _ = cmd_tx.send(Command::EmbedActive(true));
    // Hook-reported states reach clients within 250 ms instead of on
    // the 1 s refresh (see `Command::FastStatusPoll`).
    let _ = cmd_tx.send(Command::FastStatusPoll(true));

    let mut daemon = Daemon::new(config, store, client, cmd_tx.clone());
    let result = daemon.serve(listener, evt_rx, opts).await;

    let _ = cmd_tx.send(Command::Shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(3), actor).await;
    let _ = std::fs::remove_file(&sock_path);
    result.map_err(StartError::Other)
}

/// `flock`-based single-instance lock. Released when dropped (the fd
/// closes) or when the process dies, so a crash never leaves it stuck.
struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    fn acquire(path: &Path) -> std::result::Result<Self, StartError> {
        use std::io::{Read, Seek, Write};
        use std::os::unix::io::AsRawFd;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: `flock` on a valid fd we own; no memory involved.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let mut s = String::new();
            let _ = file.read_to_string(&mut s);
            return Err(StartError::AlreadyRunning {
                pid: s.trim().parse().ok(),
            });
        }
        file.set_len(0)?;
        file.rewind()?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { _file: file })
    }
}

fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("chmod 600 {}: {}", path.display(), e);
    }
}

/// Is this message from the actor a failure? The actor reports both
/// outcomes through `AppMsg::Warn`; these prefixes are its success and
/// background messages, everything else is an error.
fn is_error_message(m: &str) -> bool {
    const INFO: &[&str] = &[
        "created ",
        "restarted ",
        "modified ",
        "list: ",
        "live refresh off",
    ];
    !INFO.iter().any(|p| m.starts_with(p))
}

/// Everything the protocol publishes, in wire form. The daemon keeps
/// the last state it sent and diffs against it to produce deltas.
#[derive(Debug, Clone, PartialEq)]
struct Published {
    sessions: Vec<SessionWire>,
    statuses: BTreeMap<String, StatusInfo>,
    sidebar: SidebarWire,
    recents: Vec<RecentWire>,
    config: ConfigWire,
    theme: ThemeWire,
    theme_light: Option<ThemeWire>,
    theme_dark: Option<ThemeWire>,
    themes: Vec<ThemeEntry>,
    keybindings: KeybindingsWire,
}

/// The Mac app's Light/Dark pair (`theme_light` / `theme_dark` in
/// `config.toml`): the names as configured, and their palettes. A side
/// is unset ("same as `theme`") when its name is, or when no theme by
/// that name exists. The pair never changes `theme`, which the TUI uses.
#[derive(Debug, Clone, Default)]
struct ThemePair {
    light: Option<String>,
    dark: Option<String>,
    light_palette: Option<ThemeWire>,
    dark_palette: Option<ThemeWire>,
}

impl ThemePair {
    fn new(light: Option<String>, dark: Option<String>) -> Self {
        let palette = |name: Option<&str>| {
            let name = name?;
            if !theme_entries().iter().any(|t| t.name == name) {
                return None;
            }
            let theme = Theme::load(name, crate::config::user_themes_dir().as_deref());
            Some(ThemeWire::from(&theme))
        };
        Self {
            light_palette: palette(light.as_deref()),
            dark_palette: palette(dark.as_deref()),
            light,
            dark,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InflightKind {
    Create { defer_launch: bool },
    Restart { defer_launch: bool },
    GetSpec,
    Other,
}

/// A command forwarded to the actor, waiting for its barrier.
struct Inflight {
    barrier: u64,
    client: u64,
    id: u64,
    kind: InflightKind,
    messages: Vec<String>,
    session: Option<String>,
    spec: Option<SpecWire>,
    pending_launch: Option<PendingLaunchWire>,
    /// A create's target section: the new row moves there once the
    /// session appears.
    section: Option<String>,
    /// The daemon's own create resuming a tab of an undocked row: the
    /// agent launches with its resume invocation.
    resume: bool,
    /// A create restarting an exited tab: the new session takes that
    /// tab's place when its refresh arrives.
    replaces: Option<String>,
    /// A client's create asked for the resume invocation (`spec.resume`):
    /// a deferred launch is told so, since the session's metadata doesn't
    /// keep it.
    launch_resume: bool,
}

struct ClientConn {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    ready: bool,
}

enum ClientEvent {
    Frame(Vec<u8>),
    Closed,
}

struct Daemon {
    app: AppState,
    config: Config,
    store: Arc<Store>,
    client: Arc<TokioTmuxClient>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    theme: Theme,
    theme_pair: ThemePair,
    /// `[keybindings]` from `config.toml`, resolved the way the TUI
    /// resolves them (defaults when the table is invalid).
    keybindings: KeybindingsWire,
    embed_enabled: bool,
    agent_hooks: bool,
    seq: u64,
    published: Option<Published>,
    clients: HashMap<u64, ClientConn>,
    next_client: u64,
    inflight: VecDeque<Inflight>,
    next_barrier: u64,
    /// `config.toml` mtime and contents at the last check, to notice
    /// edits made by someone else (the TUI, a text editor).
    config_mtime: Option<SystemTime>,
    file_config: Config,
    turns: turns::TurnClock,
    /// The `@bosun_turn_at` last sent for each session, so a write
    /// isn't repeated while tmux catches up.
    turn_writes: HashMap<String, Option<u64>>,
    /// Tabs of undocked rows waiting to be resumed, as (dead tab, row
    /// id), one at a time: each takes its dead tab's place through the
    /// TUI's restart swap, which holds one at a time.
    resumes: VecDeque<(String, String)>,
    resuming: bool,
    /// For each row snoozed until it needs you, the tabs that already
    /// needed you (see `wake_snoozed`).
    snooze_baseline: HashMap<String, std::collections::HashSet<String>>,
    /// Standing notices from the actor (`AppMsg::Notice`), carried in
    /// every snapshot: the one about an out-of-date tmux server comes
    /// before the app has connected.
    standing: Vec<String>,
}

impl Daemon {
    fn new(
        config: Config,
        store: Arc<Store>,
        client: Arc<TokioTmuxClient>,
        cmd_tx: mpsc::UnboundedSender<Command>,
    ) -> Self {
        let recents = store.list_recents(200).unwrap_or_default();
        let app = AppState {
            divider_x: config.divider_x,
            sidebar: config.sidebar.clone(),
            session_history: config.session_history.clone(),
            banner_font: config.banner_font.clone(),
            session_prefix: config.session_prefix.clone(),
            editor: config.editor.clone(),
            recents,
            single_window_mode: config.single_window_mode,
            sidebar_hidden: config.sidebar_hidden,
            show_group_in_title: config.show_group_in_title,
            hide_single_tab_strip: config.hide_single_tab_strip,
            worktree_location: config.worktree_location,
            default_agent: config.default_agent.clone(),
            remove_dead_sessions: config.remove_dead_sessions,
            ..Default::default()
        };
        let theme = Theme::load(&config.theme, crate::config::user_themes_dir().as_deref());
        let theme_pair = ThemePair::new(config.theme_light.clone(), config.theme_dark.clone());
        Self {
            app,
            theme_pair,
            keybindings: KeybindingsWire::from(&config.keybindings),
            embed_enabled: config.embed_enabled,
            agent_hooks: config.agent_hooks,
            file_config: config.clone(),
            turns: turns::TurnClock::default(),
            turn_writes: HashMap::new(),
            resumes: VecDeque::new(),
            resuming: false,
            snooze_baseline: HashMap::new(),
            standing: Vec::new(),
            config,
            store,
            client,
            cmd_tx,
            theme,
            seq: 0,
            published: None,
            clients: HashMap::new(),
            next_client: 1,
            inflight: VecDeque::new(),
            next_barrier: 1,
            config_mtime: config_mtime(),
        }
    }

    async fn serve(
        &mut self,
        listener: UnixListener,
        mut evt_rx: mpsc::UnboundedReceiver<AppMsg>,
        opts: ServeOptions,
    ) -> Result<()> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let (client_tx, mut client_rx) = mpsc::unbounded_channel::<(u64, ClientEvent)>();
        let mut idle_deadline = Some(Instant::now() + opts.idle_timeout);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First state, so the first snapshot has something in it.
        self.publish();

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("accept: {}", e);
                            continue;
                        }
                    };
                    if !same_user(&stream) {
                        tracing::warn!("refusing a connection from another user");
                        continue;
                    }
                    let id = self.next_client;
                    self.next_client += 1;
                    let tx = spawn_connection(id, stream, client_tx.clone());
                    self.clients.insert(id, ClientConn { tx, ready: false });
                    idle_deadline = None;
                }
                Some((id, ev)) = client_rx.recv() => {
                    match ev {
                        ClientEvent::Frame(bytes) => self.handle_frame(id, &bytes).await,
                        ClientEvent::Closed => {
                            self.clients.remove(&id);
                        }
                    }
                    if self.clients.is_empty() && idle_deadline.is_none() {
                        idle_deadline = Some(Instant::now() + opts.idle_timeout);
                    }
                }
                maybe = evt_rx.recv() => {
                    let Some(msg) = maybe else {
                        tracing::warn!("tmux actor exited");
                        break;
                    };
                    self.handle_app_msg(msg);
                    self.wake_snoozed();
                    self.publish();
                }
                _ = tick.tick() => {
                    self.check_config_file();
                    self.wake_snoozed();
                    self.publish();
                }
                _ = async {
                    match idle_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    tracing::info!("no clients for {:?}; exiting", opts.idle_timeout);
                    break;
                }
                _ = sigterm.recv() => break,
                _ = sigint.recv() => break,
            }
        }
        Ok(())
    }

    // ── state → wire ───────────────────────────────────────────────

    fn current(&self) -> Published {
        let sessions: Vec<SessionWire> = self.app.sessions.iter().map(SessionWire::from).collect();
        let statuses = self
            .app
            .sessions
            .iter()
            .map(|v| {
                let mut info = StatusInfo::from(v);
                info.started_at = self.turns.started(v.name());
                (v.name().to_string(), info)
            })
            .collect();
        let live: std::collections::HashSet<&str> =
            self.app.sessions.iter().map(|v| v.name()).collect();
        let sidebar = SidebarWire::from_model(
            &self.app.sidebar,
            |name| match self.app.session_by_name(name) {
                Some(v) => v.display().to_string(),
                None => self.app.dead_display_for(name),
            },
            |name| live.contains(name),
            |name| self.app.recent_for_internal(name).map(|r| r.id),
        );
        Published {
            sessions,
            statuses,
            sidebar,
            recents: self.app.recents.iter().map(RecentWire::from).collect(),
            config: self.config_view(),
            theme: ThemeWire::from(&self.theme),
            theme_light: self.theme_pair.light_palette.clone(),
            theme_dark: self.theme_pair.dark_palette.clone(),
            themes: theme_entries(),
            keybindings: self.keybindings.clone(),
        }
    }

    /// Docking kills a row's live sessions; leaving Docked resumes its
    /// exited ones.
    fn parking_effects(
        &mut self,
        changes: Vec<(String, Option<SectionRole>, Option<SectionRole>)>,
    ) {
        for (row, was, now) in changes {
            let Some(c) = self.app.sidebar.find_container(&row) else {
                continue;
            };
            let tabs = c.members.clone();
            let docked = Some(SectionRole::Docked);
            if now == docked && was != docked {
                let live: Vec<String> = tabs
                    .into_iter()
                    .filter(|t| self.app.session_by_name(t).is_some())
                    .collect();
                // The name lives on the session, so keep it for the
                // docked row: a rename since it was created would
                // otherwise be lost.
                let mut named = false;
                for t in &live {
                    if let Some(display) =
                        self.app.session_by_name(t).map(|v| v.display().to_string())
                    {
                        named |= self.app.sidebar.set_ended_name(t, &display);
                    }
                }
                if named {
                    self.save_sidebar_and_history();
                }
                if !live.is_empty() {
                    self.app.recently_killed.extend(live.iter().cloned());
                    let _ = self.cmd_tx.send(Command::KillContainer { tabs: live });
                }
            } else if was == docked && now != docked {
                for t in tabs {
                    if self.app.session_by_name(&t).is_none() {
                        self.resumes.push_back((t, row.clone()));
                    }
                }
            }
        }
        self.start_next_resume();
    }

    /// Resume the next exited tab of an undocked row: created from its
    /// recents entry (by display name, as the TUI's restart does) into
    /// the dead tab's place, launching into the agent's resume command.
    fn start_next_resume(&mut self) {
        if self.resuming {
            return;
        }
        while let Some((dead, row)) = self.resumes.pop_front() {
            let still_there = self
                .app
                .sidebar
                .find_container(&row)
                .is_some_and(|c| c.contains_internal(&dead));
            if !still_there || self.app.session_by_name(&dead).is_some() {
                continue;
            }
            // The recents entry it was created from, under the name it
            // had when it was docked or was given since.
            let name = self.app.dead_display_for(&dead);
            let Some(recent) = self.app.recent_for_internal(&dead) else {
                tracing::warn!("undock: no recents entry for {name}, so {dead} stays exited");
                continue;
            };
            let mut spec = recent.to_spec();
            spec.name = name;
            spec.container_id = Some(row);
            spec.resume = true;
            self.forward(
                0,
                0,
                InflightKind::Create {
                    defer_launch: false,
                },
                vec![Command::CreateSession(spec)],
            );
            if let Some(f) = self.inflight.back_mut() {
                f.resume = true;
                f.replaces = Some(dead);
            }
            self.resuming = true;
            return;
        }
    }

    /// Wake snoozed rows whose time has come, or that wait for an agent
    /// that now needs you: a hook's done after the row was snoozed, or a
    /// tab waiting, done or in error that wasn't when it was snoozed (a
    /// detector's done can linger from before).
    fn wake_snoozed(&mut self) {
        use protocol::StatusWire as W;
        let mut needing = std::collections::HashSet::new();
        let mut done_at = HashMap::new();
        for v in &self.app.sessions {
            let info = StatusInfo::from(v);
            if matches!(info.state, W::Waiting | W::Error | W::Done) {
                needing.insert(v.name().to_string());
            }
            if let Some(d) = info.done_at {
                done_at.insert(v.name().to_string(), d);
            }
        }
        // What already needed you when each row was snoozed (first seen
        // here, so a restarted daemon starts again from now); a tab that
        // stops needing you leaves it, so needing you again counts.
        let mut baseline = std::mem::take(&mut self.snooze_baseline);
        let rows: Vec<(String, Vec<String>)> = self
            .app
            .sidebar
            .snoozed()
            .filter(|c| c.parked.as_ref().is_some_and(|p| p.until_needed))
            .map(|c| (c.id.clone(), c.members.clone()))
            .collect();
        baseline.retain(|row, _| rows.iter().any(|(r, _)| r == row));
        for (row, tabs) in &rows {
            let seen = baseline.entry(row.clone()).or_insert_with(|| {
                tabs.iter()
                    .filter(|t| needing.contains(*t))
                    .cloned()
                    .collect()
            });
            seen.retain(|t| needing.contains(t));
        }
        let due = self
            .app
            .sidebar
            .due_to_wake(now_ms(), |row, tab, parked_at| {
                done_at.get(tab).is_some_and(|d| *d > parked_at)
                    || (needing.contains(tab)
                        && !baseline.get(row).is_some_and(|b| b.contains(tab)))
            });
        for row in &due {
            baseline.remove(row);
        }
        self.snooze_baseline = baseline;
        if due.is_empty() {
            return;
        }
        for row in &due {
            let _ = self.app.sidebar.unpark(row);
        }
        self.save_sidebar_and_history();
    }

    fn track_turns(&mut self) {
        let now = now_ms();
        for v in &self.app.sessions {
            let info = StatusInfo::from(v);
            self.turns
                .observe(v.name(), info.state, info.source, now, v.session.turn_at);
        }
        let live: std::collections::HashSet<&str> =
            self.app.sessions.iter().map(|v| v.name()).collect();
        self.turns.retain(|s| live.contains(s));
        self.turn_writes.retain(|s, _| live.contains(s.as_str()));
        // Keep each turn's start on its session for the next daemon.
        let mut writes = Vec::new();
        for v in &self.app.sessions {
            let want = self.turns.stored(v.name(), now);
            if want == v.session.turn_at {
                self.turn_writes.remove(v.name());
            } else if self.turn_writes.get(v.name()) != Some(&want) {
                self.turn_writes.insert(v.name().to_string(), want);
                writes.push((v.name().to_string(), want));
            }
        }
        if !writes.is_empty() {
            let _ = self.cmd_tx.send(Command::SetTurnStarts(writes));
        }
    }

    fn config_view(&self) -> ConfigWire {
        let pin = |k: &str| crate::config::env_pin(k).map(str::to_string);
        let row = |key: &str, value: serde_json::Value, choices: Option<Vec<String>>| {
            let pinned_by = pin(key);
            SettingWire {
                key: key.to_string(),
                value,
                choices,
                read_only: pinned_by.is_some(),
                pinned_by,
            }
        };
        let themes: Vec<String> = theme_entries().into_iter().map(|t| t.name).collect();
        let optional = |name: &Option<String>| match name {
            Some(n) => serde_json::Value::from(n.clone()),
            None => serde_json::Value::Null,
        };
        let mut settings = vec![
            row(
                "theme",
                self.theme.name.clone().into(),
                Some(themes.clone()),
            ),
            row(
                "theme_light",
                optional(&self.theme_pair.light),
                Some(themes.clone()),
            ),
            row("theme_dark", optional(&self.theme_pair.dark), Some(themes)),
            row(
                "banner_font",
                self.app.banner_font.clone().into(),
                Some(crate::ui::banner::names().map(str::to_string).collect()),
            ),
            row(
                "default_agent",
                self.app.default_agent.clone().into(),
                Some(
                    crate::config::AGENTS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                ),
            ),
            row("editor", optional(&self.app.editor), None),
            row(
                "worktree_location",
                worktree_str(self.app.worktree_location).into(),
                Some(vec!["subdir".into(), "sibling".into()]),
            ),
            row("single_window", self.app.single_window_mode.into(), None),
            row(
                "show_group_in_title",
                self.app.show_group_in_title.into(),
                None,
            ),
            row(
                "hide_single_tab_strip",
                self.app.hide_single_tab_strip.into(),
                None,
            ),
            row(
                "remove_dead_sessions",
                self.app.remove_dead_sessions.into(),
                None,
            ),
            row("embed_enabled", self.embed_enabled.into(), None),
            row("agent_hooks", self.agent_hooks.into(), None),
        ];
        let install = |key: &str, value: serde_json::Value, var: &str| SettingWire {
            key: key.to_string(),
            value,
            choices: None,
            pinned_by: std::env::var(var).ok().map(|_| var.to_string()),
            read_only: true,
        };
        settings.push(install(
            "session_prefix",
            self.config.session_prefix.clone().into(),
            "BOSUN_PREFIX",
        ));
        settings.push(install(
            "tmux_socket",
            self.config
                .tmux_socket
                .clone()
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
            "BOSUN_TMUX_SOCKET",
        ));
        ConfigWire { settings }
    }

    fn snapshot(&self) -> ServerMsg {
        let p = self.published.clone().unwrap_or_else(|| self.current());
        ServerMsg::Snapshot(Box::new(Snapshot {
            seq: self.seq,
            server: ServerInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: PROTOCOL_VERSION,
                tmux_socket: self.config.tmux_socket.clone(),
            },
            sessions: p.sessions,
            statuses: p.statuses,
            sidebar: p.sidebar,
            recents: p.recents,
            config: p.config,
            theme: p.theme,
            theme_light: p.theme_light,
            theme_dark: p.theme_dark,
            themes: p.themes,
            keybindings: p.keybindings,
            notices: self.standing.clone(),
        }))
    }

    /// Diff the current state against what was last sent and broadcast
    /// one delta per changed part.
    fn publish(&mut self) {
        self.track_turns();
        let cur = self.current();
        let Some(prev) = self.published.take() else {
            self.seq = 1;
            self.published = Some(cur);
            return;
        };
        let prev_rows: HashMap<&str, &SessionWire> =
            prev.sessions.iter().map(|s| (s.name.as_str(), s)).collect();
        let upsert: Vec<SessionWire> = cur
            .sessions
            .iter()
            .filter(|s| prev_rows.get(s.name.as_str()) != Some(s))
            .cloned()
            .collect();
        let cur_names: std::collections::HashSet<&str> =
            cur.sessions.iter().map(|s| s.name.as_str()).collect();
        let removed: Vec<String> = prev
            .sessions
            .iter()
            .filter(|s| !cur_names.contains(s.name.as_str()))
            .map(|s| s.name.clone())
            .collect();
        if !upsert.is_empty() || !removed.is_empty() {
            let msg = ServerMsg::Sessions {
                seq: self.next_seq(),
                upsert,
                removed,
            };
            self.broadcast(&msg);
        }
        for (name, info) in &cur.statuses {
            if prev.statuses.get(name) != Some(info) {
                let msg = ServerMsg::Status {
                    seq: self.next_seq(),
                    session: name.clone(),
                    state: info.state,
                    source: info.source,
                    detail: info.detail.clone(),
                    done_at: info.done_at,
                    started_at: info.started_at,
                };
                self.broadcast(&msg);
            }
        }
        if cur.sidebar != prev.sidebar {
            let msg = ServerMsg::Sidebar {
                seq: self.next_seq(),
                sidebar: cur.sidebar.clone(),
            };
            self.broadcast(&msg);
        }
        if cur.recents != prev.recents {
            let msg = ServerMsg::Recents {
                seq: self.next_seq(),
                recents: cur.recents.clone(),
            };
            self.broadcast(&msg);
        }
        if cur.config != prev.config {
            let msg = ServerMsg::Config {
                seq: self.next_seq(),
                config: cur.config.clone(),
            };
            self.broadcast(&msg);
        }
        if cur.theme != prev.theme {
            let msg = ServerMsg::Theme {
                seq: self.next_seq(),
                theme: Box::new(cur.theme.clone()),
            };
            self.broadcast(&msg);
        }
        if cur.theme_light != prev.theme_light || cur.theme_dark != prev.theme_dark {
            let msg = ServerMsg::ThemePair {
                seq: self.next_seq(),
                theme_light: cur.theme_light.clone().map(Box::new),
                theme_dark: cur.theme_dark.clone().map(Box::new),
            };
            self.broadcast(&msg);
        }
        if cur.themes != prev.themes {
            let msg = ServerMsg::Themes {
                seq: self.next_seq(),
                themes: cur.themes.clone(),
            };
            self.broadcast(&msg);
        }
        if cur.keybindings != prev.keybindings {
            let msg = ServerMsg::Keybindings {
                seq: self.next_seq(),
                keybindings: cur.keybindings.clone(),
            };
            self.broadcast(&msg);
        }
        self.published = Some(cur);
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn broadcast(&self, msg: &ServerMsg) {
        let frame = frame::encode(msg);
        for c in self.clients.values().filter(|c| c.ready) {
            let _ = c.tx.send(frame.clone());
        }
    }

    fn send_to(&self, client: u64, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&client) {
            let _ = c.tx.send(frame::encode(msg));
        }
    }

    /// Send an ack, after the deltas of whatever the command changed:
    /// the protocol promises deltas first.
    fn ack(&mut self, client: u64, ack: Ack) {
        self.publish();
        self.send_to(client, &ServerMsg::Ack(ack));
    }

    fn ack_result(&mut self, client: u64, id: u64, r: std::result::Result<(), String>) {
        let ack = match r {
            Ok(()) => Ack {
                id,
                ok: true,
                ..Default::default()
            },
            Err(e) => Ack {
                id,
                ok: false,
                error: Some(e),
                ..Default::default()
            },
        };
        self.ack(client, ack);
    }

    // ── client messages ────────────────────────────────────────────

    async fn handle_frame(&mut self, client: u64, bytes: &[u8]) {
        let msg: ClientMsg = match serde_json::from_slice(bytes) {
            Ok(m) => m,
            Err(e) => {
                self.send_to(
                    client,
                    &ServerMsg::Error {
                        code: "bad_message".into(),
                        message: e.to_string(),
                    },
                );
                return;
            }
        };
        let ready = self.clients.get(&client).is_some_and(|c| c.ready);
        match msg {
            ClientMsg::Hello { protocol, .. } => {
                if protocol != PROTOCOL_VERSION {
                    self.send_to(
                        client,
                        &ServerMsg::Error {
                            code: "protocol_mismatch".into(),
                            message: format!(
                                "bosun serve {} speaks protocol {} but this client speaks {}; \
                                 update bosun or the app so both match",
                                env!("CARGO_PKG_VERSION"),
                                PROTOCOL_VERSION,
                                protocol
                            ),
                        },
                    );
                    self.clients.remove(&client);
                    return;
                }
                self.publish();
                let snap = self.snapshot();
                if let Some(c) = self.clients.get_mut(&client) {
                    c.ready = true;
                }
                self.send_to(client, &snap);
            }
            _ if !ready => {
                self.send_to(
                    client,
                    &ServerMsg::Error {
                        code: "no_hello".into(),
                        message: "send hello first".into(),
                    },
                );
                self.clients.remove(&client);
            }
            ClientMsg::Resync => {
                self.publish();
                let snap = self.snapshot();
                self.send_to(client, &snap);
            }
            ClientMsg::Cmd(CmdEnvelope { id, op }) => {
                self.handle_cmd(client, id, op).await;
                self.publish();
            }
        }
    }

    /// Forward a command to the actor, followed by a barrier so we know
    /// when it's done.
    fn forward(&mut self, client: u64, id: u64, kind: InflightKind, cmds: Vec<Command>) {
        let barrier = self.next_barrier;
        self.next_barrier += 1;
        for c in cmds {
            let _ = self.cmd_tx.send(c);
        }
        let _ = self.cmd_tx.send(Command::Barrier(barrier));
        self.inflight.push_back(Inflight {
            barrier,
            client,
            id,
            kind,
            messages: Vec::new(),
            session: None,
            spec: None,
            pending_launch: None,
            section: None,
            resume: false,
            replaces: None,
            launch_resume: false,
        });
    }

    async fn handle_cmd(&mut self, client: u64, id: u64, op: CmdOp) {
        use protocol::check_text;
        match op {
            CmdOp::Create {
                mut spec,
                defer_launch,
                section,
                replaces,
            } => {
                // Restarting an exited tab: the new session joins that
                // tab's row and takes its place there.
                let replaces = replaces.filter(|t| self.app.session_by_name(t).is_none());
                let row = replaces
                    .as_deref()
                    .and_then(|t| self.app.sidebar.container_of(t))
                    .map(|c| c.id.clone());
                let replaces = replaces.filter(|_| row.is_some());
                if spec.container_id.is_none() {
                    spec.container_id = row;
                }
                let section = section.filter(|_| spec.container_id.is_none());
                if let Some(sid) = &section {
                    let e = match self.app.sidebar.sections.iter().find(|s| &s.id == sid) {
                        None => Some(format!("no section with id {sid}")),
                        Some(s) if s.role.is_some() => {
                            Some(format!("new sessions can't go into {}", s.name))
                        }
                        Some(_) => None,
                    };
                    if let Some(e) = e {
                        return self.ack_result(client, id, Err(e));
                    }
                }
                match spec.to_spec() {
                    Ok(spec) => {
                        let launch_resume = spec.resume;
                        self.forward(
                            client,
                            id,
                            InflightKind::Create { defer_launch },
                            vec![Command::CreateSession(spec)],
                        );
                        if let Some(f) = self.inflight.back_mut() {
                            f.section = section;
                            f.replaces = replaces;
                            f.launch_resume = launch_resume;
                        }
                    }
                    Err(e) => self.ack_result(client, id, Err(e)),
                }
            }
            CmdOp::Kill { session } => {
                self.app.recently_killed.insert(session.clone());
                self.forward(
                    client,
                    id,
                    InflightKind::Other,
                    vec![Command::KillSession(session)],
                );
            }
            CmdOp::KillContainer { sessions } => {
                for s in &sessions {
                    self.app.recently_killed.insert(s.clone());
                }
                self.forward(
                    client,
                    id,
                    InflightKind::Other,
                    vec![Command::KillContainer { tabs: sessions }],
                );
            }
            CmdOp::KillRemoveWorktree {
                session,
                worktree_path,
                branch,
                merge,
            } => {
                self.app.recently_killed.insert(session.clone());
                self.forward(
                    client,
                    id,
                    InflightKind::Other,
                    vec![Command::KillSessionRemoveWorktree {
                        internal: session,
                        worktree_path,
                        branch,
                        merge,
                    }],
                );
            }
            CmdOp::Rename { session, display } => match check_text("display", &display, false) {
                // A tab whose session has ended (a docked row's): there's
                // no session to rename, so the name is kept on the
                // sidebar and the tab comes back under it.
                Ok(()) if self.app.session_by_name(&session).is_none() => {
                    let result = if self.app.sidebar.set_ended_name(&session, display.trim()) {
                        self.save_sidebar_and_history();
                        Ok(())
                    } else if self.app.dead_display_for(&session) == display.trim() {
                        Ok(())
                    } else {
                        Err(format!("no session or tab {session}"))
                    };
                    self.ack_result(client, id, result);
                }
                Ok(()) => self.forward(
                    client,
                    id,
                    InflightKind::Other,
                    vec![Command::RenameSession {
                        internal: session,
                        new_display: display,
                    }],
                ),
                Err(e) => self.ack_result(client, id, Err(e)),
            },
            CmdOp::Restart {
                session,
                resume,
                defer_launch,
            } => self.forward(
                client,
                id,
                InflightKind::Restart { defer_launch },
                vec![Command::RestartSession {
                    internal: session,
                    continue_session: resume,
                }],
            ),
            CmdOp::LaunchAgent { session, resume } => self.forward(
                client,
                id,
                InflightKind::Other,
                vec![Command::LaunchAgent {
                    internal: session,
                    resume,
                }],
            ),
            CmdOp::GetSpec { session } => self.forward(
                client,
                id,
                InflightKind::GetSpec,
                vec![Command::OpenModifySession { internal: session }],
            ),
            CmdOp::Modify { session, spec } => match spec.to_spec() {
                Ok(spec) => self.forward(
                    client,
                    id,
                    InflightKind::Other,
                    vec![Command::ModifySession {
                        internal: session,
                        spec,
                    }],
                ),
                Err(e) => self.ack_result(client, id, Err(e)),
            },
            CmdOp::DeleteRecent { recent } => {
                let r = self.store.delete_recent(recent).map_err(|e| e.to_string());
                self.reload_recents();
                self.ack_result(client, id, r);
            }
            CmdOp::SidebarSave { sidebar } => {
                let r = sidebar
                    .to_model()
                    .map(|model| self.save_client_sidebar(model));
                self.ack_result(client, id, r);
            }
            CmdOp::MoveTab {
                session,
                container_id,
                index,
                section,
            } => {
                let sidebar = &mut self.app.sidebar;
                let r = match (container_id.as_deref(), index) {
                    (None, Some(i)) => sidebar.move_tab_to_new_row(&session, section.as_deref(), i),
                    (container, index) => sidebar.move_tab(&session, container, index),
                };
                self.after_move(client, id, r.map(|row| row.map(|r| vec![r])));
            }
            CmdOp::MergeRows { from, into, index } => {
                let r = self
                    .app
                    .sidebar
                    .merge_rows(&from, &into, index)
                    .map(|moved| {
                        if moved.is_empty() {
                            None
                        } else {
                            Some(vec![into.clone()])
                        }
                    });
                self.after_move(client, id, r);
            }
            CmdOp::InsertSection { name } => {
                let r = check_text("name", &name, false).map(|()| {
                    let mut out = Vec::new();
                    self.app.insert_section(name, &mut out);
                    self.run_local(out);
                });
                self.ack_result(client, id, r);
            }
            CmdOp::ParkRow {
                container_id,
                role,
                wake_at,
                until_needed,
            } => {
                let r = self
                    .app
                    .sidebar
                    .park(&container_id, role, wake_at, until_needed, now_ms());
                match r {
                    Ok(was) => {
                        self.save_sidebar_and_history();
                        self.parking_effects(vec![(container_id, was, Some(role))]);
                        self.ack_result(client, id, Ok(()));
                    }
                    Err(e) => self.ack_result(client, id, Err(e.to_string())),
                }
            }
            CmdOp::UnparkRow { container_id } => match self.app.sidebar.unpark(&container_id) {
                Ok(was) => {
                    if was.is_some() {
                        self.save_sidebar_and_history();
                        self.parking_effects(vec![(container_id, was, None)]);
                    }
                    self.ack_result(client, id, Ok(()));
                }
                Err(e) => self.ack_result(client, id, Err(e.to_string())),
            },
            CmdOp::SetSectionDefaults { section, defaults } => {
                let r = defaults
                    .as_ref()
                    .map_or(Ok(()), |d| d.check())
                    .and_then(|()| {
                        let s = self
                            .app
                            .sidebar
                            .sections
                            .iter_mut()
                            .find(|s| s.id == section)
                            .ok_or_else(|| format!("no section with id {section}"))?;
                        s.defaults = defaults;
                        Ok(())
                    });
                if r.is_ok() {
                    self.save_sidebar_and_history();
                }
                self.ack_result(client, id, r);
            }
            CmdOp::RenameSection { section, name } => {
                let r = check_text("name", &name, false).and_then(|()| {
                    if !self.app.sidebar.sections.iter().any(|s| s.id == section) {
                        return Err(format!("no section with id {section}"));
                    }
                    let mut out = Vec::new();
                    self.app.rename_section(&section, name, &mut out);
                    self.run_local(out);
                    Ok(())
                });
                self.ack_result(client, id, r);
            }
            CmdOp::SetTheme { name, persist } => {
                let r = self.set_theme(&name, persist);
                self.ack_result(client, id, r);
            }
            CmdOp::GetTheme { name } => {
                let ack = if theme_entries().iter().any(|t| t.name == name) {
                    let theme = Theme::load(&name, crate::config::user_themes_dir().as_deref());
                    Ack {
                        id,
                        ok: true,
                        theme: Some(Box::new(ThemeWire::from(&theme))),
                        ..Default::default()
                    }
                } else {
                    Ack {
                        id,
                        ok: false,
                        error: Some(format!("unknown theme: {name}")),
                        ..Default::default()
                    }
                };
                self.send_to(client, &ServerMsg::Ack(ack));
            }
            CmdOp::ApplySetting { key, value } => {
                let r = self.apply_setting(&key, &value);
                self.ack_result(client, id, r);
            }
            CmdOp::ClientFlags { tty, ignore_size } => {
                let r = self
                    .client
                    .set_client_ignore_size(&tty, ignore_size)
                    .await
                    .map_err(|e| e.to_string());
                self.ack_result(client, id, r);
            }
            CmdOp::MakeLatest { tty } => {
                let r = self
                    .client
                    .make_client_latest(&tty)
                    .await
                    .map_err(|e| e.to_string());
                self.ack_result(client, id, r);
            }
            CmdOp::Search {
                session,
                action,
                text,
                match_case,
                colors,
            } => {
                use crate::tmux::client::{SearchHighlight, SearchStep};
                let step = match action {
                    SearchAction::Find => SearchStep::Find(text),
                    SearchAction::Older => SearchStep::Older(text),
                    SearchAction::Newer => SearchStep::Newer(text),
                    SearchAction::End => SearchStep::End,
                };
                // Colours go to tmux as a style: only `#rrggbb`.
                let hex = |c: &str| {
                    c.len() == 7
                        && c.starts_with('#')
                        && c[1..].chars().all(|d| d.is_ascii_hexdigit())
                };
                let highlight = colors
                    .filter(|c| {
                        [&c.current_bg, &c.current_fg, &c.match_bg, &c.match_fg]
                            .iter()
                            .all(|c| hex(c))
                    })
                    .map(|c| SearchHighlight {
                        current: format!("bg={},fg={}", c.current_bg, c.current_fg),
                        other: format!("bg={},fg={}", c.match_bg, c.match_fg),
                    });
                let ack = match self
                    .client
                    .copy_mode_search(&session, &step, match_case, highlight.as_ref())
                    .await
                {
                    Ok(r) => Ack {
                        id,
                        ok: true,
                        search: Some(SearchWire {
                            found: r.found,
                            count: r.count,
                            partial: r.partial,
                        }),
                        ..Default::default()
                    },
                    Err(e) => Ack {
                        id,
                        ok: false,
                        error: Some(format!("search: {e}")),
                        ..Default::default()
                    },
                };
                self.send_to(client, &ServerMsg::Ack(ack));
            }
        }
    }

    fn set_theme(&mut self, name: &str, persist: bool) -> std::result::Result<(), String> {
        if !theme_entries().iter().any(|t| t.name == name) {
            return Err(format!("no theme named {name}"));
        }
        self.theme = Theme::load(name, crate::config::user_themes_dir().as_deref());
        if persist {
            crate::config::write_theme(name).map_err(|e| format!("theme: failed to save: {e}"))?;
            self.file_config.theme = name.to_string();
            self.config_mtime = config_mtime();
        }
        Ok(())
    }

    /// `apply_setting` for `theme_light` / `theme_dark`: a theme name,
    /// or `null` (or an empty string) to unset it. `theme` is left alone.
    fn set_theme_pair_side(
        &mut self,
        key: &str,
        value: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        let name = match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if s.trim().is_empty() => None,
            serde_json::Value::String(s) => {
                if !theme_entries().iter().any(|t| t.name == *s) {
                    return Err(format!("no theme named {s}"));
                }
                Some(s.clone())
            }
            _ => return Err(format!("{key} needs a theme name or null")),
        };
        let mut light = self.theme_pair.light.clone();
        let mut dark = self.theme_pair.dark.clone();
        let written = if key == "theme_light" {
            light = name.clone();
            crate::config::write_theme_light(name.as_deref())
        } else {
            dark = name.clone();
            crate::config::write_theme_dark(name.as_deref())
        };
        self.theme_pair = ThemePair::new(light, dark);
        // Our own write isn't an outside edit; don't reload it.
        self.config_mtime = config_mtime();
        self.file_config = Config::load();
        written.map_err(|e| format!("settings: failed to save: {e}"))
    }

    /// `apply_setting` for `editor`: a command, or null (or "") to unset it.
    fn set_editor(&mut self, value: &serde_json::Value) -> std::result::Result<(), String> {
        let editor = match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if s.trim().is_empty() => None,
            serde_json::Value::String(s) => {
                protocol::check_text("editor", s, false)?;
                Some(s.trim().to_string())
            }
            _ => return Err("editor needs a command or null".to_string()),
        };
        let written = crate::config::write_editor(editor.as_deref());
        self.app.editor = editor;
        // Our own write isn't an outside edit; don't reload it.
        self.config_mtime = config_mtime();
        self.file_config = Config::load();
        written.map_err(|e| format!("settings: failed to save: {e}"))
    }

    /// The settings panel's `apply_setting`, for the daemon's state.
    fn apply_setting(
        &mut self,
        key: &str,
        value: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        if let Some(var) = crate::config::env_pin(key) {
            return Err(format!("{key} is pinned by {var}"));
        }
        if key == "theme_light" || key == "theme_dark" {
            return self.set_theme_pair_side(key, value);
        }
        if key == "editor" {
            return self.set_editor(value);
        }
        let as_bool = || {
            value
                .as_bool()
                .ok_or_else(|| format!("{key} needs a boolean"))
        };
        let as_str = || {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key} needs a string"))
        };
        let change = match key {
            "theme" => SettingChange::Theme(as_str()?),
            "banner_font" => {
                let v = as_str()?;
                if !crate::ui::banner::names().any(|n| n == v) {
                    return Err(format!("no banner font named {v}"));
                }
                SettingChange::BannerFont(v)
            }
            "default_agent" => {
                let v = as_str()?;
                if !crate::config::AGENTS.contains(&v.as_str()) {
                    return Err(format!("unknown agent {v}"));
                }
                SettingChange::DefaultAgent(v)
            }
            "worktree_location" => SettingChange::WorktreeLocation(match as_str()?.as_str() {
                "subdir" => WorktreeLocation::Subdir,
                "sibling" => WorktreeLocation::Sibling,
                other => return Err(format!("unknown worktree location {other}")),
            }),
            "single_window" => SettingChange::SingleWindow(as_bool()?),
            "show_group_in_title" => SettingChange::ShowGroupInTitle(as_bool()?),
            "hide_single_tab_strip" => SettingChange::HideSingleTabStrip(as_bool()?),
            "remove_dead_sessions" => SettingChange::RemoveDeadSessions(as_bool()?),
            "embed_enabled" => SettingChange::EmbedEnabled(as_bool()?),
            "agent_hooks" => SettingChange::AgentHooks(as_bool()?),
            "session_prefix" | "tmux_socket" => {
                return Err(format!("{key} is set by editing config.toml"))
            }
            other => return Err(format!("unknown setting {other}")),
        };
        let written = match change {
            SettingChange::Theme(name) => return self.set_theme(&name, true),
            SettingChange::BannerFont(name) => {
                self.app.banner_font = name.clone();
                crate::config::write_banner_font(&name)
            }
            SettingChange::DefaultAgent(agent) => {
                self.app.default_agent = agent.clone();
                crate::config::write_default_agent(&agent)
            }
            SettingChange::WorktreeLocation(loc) => {
                self.app.worktree_location = loc;
                crate::config::write_worktree_location(loc)
            }
            SettingChange::SingleWindow(on) => {
                self.app.single_window_mode = on;
                crate::config::write_single_window(on)
            }
            SettingChange::ShowGroupInTitle(on) => {
                self.app.show_group_in_title = on;
                crate::config::write_show_group_in_title(on)
            }
            SettingChange::HideSingleTabStrip(on) => {
                self.app.hide_single_tab_strip = on;
                crate::config::write_hide_single_tab_strip(on)
            }
            SettingChange::RemoveDeadSessions(on) => {
                self.app.remove_dead_sessions = on;
                crate::config::write_remove_dead_sessions(on)
            }
            SettingChange::EmbedEnabled(on) => {
                self.embed_enabled = on;
                crate::config::write_embed_enabled(on)
            }
            SettingChange::AgentHooks(on) => {
                // The actor owns launching and status, so it rebuilds
                // its hook files and starts or stops honoring hook state.
                self.agent_hooks = on;
                let _ = self.cmd_tx.send(Command::SetAgentHooks(on));
                crate::config::write_agent_hooks(on)
            }
        };
        // Our own write isn't an outside edit; don't reload it.
        self.config_mtime = config_mtime();
        self.file_config = Config::load();
        written.map_err(|e| format!("settings: failed to save: {e}"))
    }

    /// Finish a `move_tab` or `merge_rows` that already changed the
    /// model: save it, then point the `@bosun_container_id` of every
    /// live tab of the rows that changed at its row, so a session the
    /// model doesn't hold (a lost config.toml) is reconciled back into
    /// the same row. `Ok(None)` is a no-op, acked straight away. The
    /// sidebar delta goes out before the ack (see `ack`).
    fn after_move(
        &mut self,
        client: u64,
        id: u64,
        r: std::result::Result<Option<Vec<String>>, crate::sidebar::MoveError>,
    ) {
        let rows = match r {
            Err(e) => return self.ack_result(client, id, Err(e.to_string())),
            Ok(None) => return self.ack_result(client, id, Ok(())),
            Ok(Some(rows)) => rows,
        };
        self.save_sidebar_and_history();
        // The actor skips exited tabs and options already right.
        let stamps: Vec<(String, String)> = rows
            .iter()
            .filter_map(|row| self.app.sidebar.find_container(row))
            .flat_map(|c| c.members.iter().map(|tab| (tab.clone(), c.id.clone())))
            .collect();
        self.forward(
            client,
            id,
            InflightKind::Other,
            vec![Command::SetContainerIds(stamps)],
        );
    }

    /// Adopt a sidebar a client sent. Section history follows, so a
    /// row moved out of a section isn't put back on the next refresh.
    fn save_client_sidebar(&mut self, mut model: SidebarModel) {
        // A row's wire `name` is its label, not the stored name; keep
        // what `config.toml` already has for rows that existed.
        let stored: HashMap<String, String> = self
            .app
            .sidebar
            .ungrouped
            .iter()
            .chain(
                self.app
                    .sidebar
                    .sections
                    .iter()
                    .flat_map(|s| s.members.iter()),
            )
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        for c in model
            .ungrouped
            .iter_mut()
            .chain(model.sections.iter_mut().flat_map(|s| s.members.iter_mut()))
        {
            match stored.get(&c.id) {
                Some(name) => c.name = name.clone(),
                None if c.name.is_empty() => c.name = c.active.clone(),
                None => {}
            }
        }
        // Roles, parking and section defaults aren't in a client's
        // layout; a row dragged into or out of Docked is docked or
        // undocked.
        let changes = self.app.sidebar.adopt(&mut model, now_ms(), true);
        self.app.sidebar = model;
        self.app.selected = 0;
        self.save_sidebar_and_history();
        self.parking_effects(changes);
    }

    /// Write the sidebar after a change, with section history following
    /// it so a row moved out of a section isn't put back on the next
    /// refresh.
    fn save_sidebar_and_history(&mut self) {
        let mut history_changed = false;
        for v in &self.app.sessions {
            let display = v.display().to_string();
            let section = self
                .app
                .sidebar
                .sections
                .iter()
                .find(|s| s.members.iter().any(|c| c.contains_internal(v.name())));
            // A parked row keeps the section it had: Snoozed and Docked
            // are never where a restart should put it.
            if section.is_some_and(|s| s.role.is_some()) {
                continue;
            }
            match section {
                Some(s) => {
                    let prev = self.app.session_history.insert(display, s.name.clone());
                    history_changed |= prev.as_deref() != Some(s.name.as_str());
                }
                None => history_changed |= self.app.session_history.remove(&display).is_some(),
            }
        }
        let mut out = vec![Command::SaveSidebar(self.app.sidebar.clone())];
        if history_changed {
            out.push(Command::SaveSessionHistory(
                self.app.session_history.clone(),
            ));
        }
        self.run_local(out);
    }

    /// Carry out the commands the reducer emits that the TUI's run
    /// loop would handle itself. Selection and preview commands mean
    /// nothing without a screen and are dropped.
    fn run_local(&mut self, out: Vec<Command>) {
        for c in out {
            match c {
                Command::SaveSidebar(model) => {
                    if let Err(e) = crate::config::write_sidebar(&model) {
                        tracing::warn!("sidebar: failed to save: {}", e);
                    }
                    self.file_config.sidebar = model;
                    self.config_mtime = config_mtime();
                }
                Command::SaveSessionHistory(h) => {
                    if let Err(e) = crate::config::write_session_history(&h) {
                        tracing::warn!("history: failed to save: {}", e);
                    }
                    self.file_config.session_history = h;
                    self.config_mtime = config_mtime();
                }
                _ => {}
            }
        }
    }

    fn reload_recents(&mut self) {
        self.app.recents = self.store.list_recents(200).unwrap_or_default();
    }

    // ── actor messages ─────────────────────────────────────────────

    fn handle_app_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::CommandDone(barrier) => self.finish(barrier),
            AppMsg::Warn(text) => {
                if let Some(f) = self.inflight.front_mut() {
                    f.messages.push(text.clone());
                }
                let msg = ServerMsg::Notice {
                    seq: self.next_seq(),
                    message: text,
                };
                self.broadcast(&msg);
            }
            AppMsg::Notice { message, standing } => {
                if standing {
                    self.standing.push(message.clone());
                }
                let msg = ServerMsg::Notice {
                    seq: self.next_seq(),
                    message,
                };
                self.broadcast(&msg);
            }
            AppMsg::DeferRelaunch { internal, resume } => {
                let defer = matches!(
                    self.inflight.front().map(|f| f.kind),
                    Some(InflightKind::Restart { defer_launch: true })
                );
                if defer {
                    if let Some(f) = self.inflight.front_mut() {
                        f.pending_launch = Some(PendingLaunchWire {
                            session: internal,
                            resume: Some(resume),
                        });
                    }
                } else {
                    let _ = self.cmd_tx.send(Command::LaunchAgent {
                        internal,
                        resume: Some(resume),
                    });
                }
            }
            AppMsg::ModifySpecReady { spec, .. } => {
                if let Some(f) = self.inflight.front_mut() {
                    if f.kind == InflightKind::GetSpec {
                        f.spec = Some(SpecWire::from(&spec));
                    }
                }
            }
            msg @ AppMsg::SessionsRefreshed { .. } => {
                let mut place = None;
                if let AppMsg::SessionsRefreshed {
                    select_after: Some(name),
                    ..
                } = &msg
                {
                    if let Some(f) = self.inflight.front_mut() {
                        if matches!(f.kind, InflightKind::Create { .. }) {
                            f.session = Some(name.clone());
                            place = f.section.take().map(|s| (name.clone(), s));
                            // Tied to this create's own refresh, so
                            // another create can't take the swap.
                            if let Some(dead) = f.replaces.take() {
                                self.app.pending_restart_swap = Some(dead);
                            }
                        }
                    }
                }
                let out = self.app.apply(msg);
                self.run_local(out);
                // After the refresh has given the new session its row.
                if let Some((session, section)) = place {
                    match self.app.sidebar.move_row_to_section_end(&session, &section) {
                        Ok(Some(_)) => self.save_sidebar_and_history(),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("placing {session}: {e}"),
                    }
                }
                self.reload_recents();
            }
            msg @ AppMsg::StatusRefreshed { .. } => {
                let _ = self.app.apply(msg);
            }
            AppMsg::ClientSession { tty, session } => {
                let seq = self.next_seq();
                let msg = match session {
                    Some(session) => ServerMsg::ClientSession { seq, tty, session },
                    None => ServerMsg::ClientDetached { seq, tty },
                };
                self.broadcast(&msg);
            }
            AppMsg::Fatal(e) => tracing::error!("tmux actor: {}", e),
            _ => {}
        }
    }

    /// A barrier came back: acknowledge the command it closes.
    fn finish(&mut self, barrier: u64) {
        let Some(pos) = self.inflight.iter().position(|f| f.barrier == barrier) else {
            return;
        };
        let Some(f) = self.inflight.remove(pos) else {
            return;
        };
        let error = f.messages.iter().find(|m| is_error_message(m)).cloned();
        let mut ack = Ack {
            id: f.id,
            ok: error.is_none(),
            error,
            session: f.session.clone(),
            spec: f.spec,
            theme: None,
            pending_launch: f.pending_launch,
            messages: f.messages,
            search: None,
        };
        if let InflightKind::Create { defer_launch } = f.kind {
            if let (true, Some(session)) = (ack.ok, f.session) {
                if defer_launch {
                    ack.pending_launch = Some(PendingLaunchWire {
                        session,
                        resume: f.launch_resume.then_some(true),
                    });
                } else {
                    let _ = self.cmd_tx.send(Command::LaunchAgent {
                        internal: session,
                        resume: f.resume.then_some(true),
                    });
                }
            }
        }
        if f.kind == InflightKind::GetSpec && ack.ok && ack.spec.is_none() {
            ack.ok = false;
            ack.error = Some("no spec stored on this session".into());
        }
        self.ack(f.client, ack);
        if matches!(
            f.kind,
            InflightKind::Create { .. } | InflightKind::Restart { .. }
        ) {
            self.reload_recents();
        }
        if f.resume {
            self.resuming = false;
            self.start_next_resume();
        }
    }

    // ── config.toml edits made elsewhere ───────────────────────────

    /// Pick up `config.toml` changes made by someone else (the TUI's
    /// theme picker or settings panel, a sidebar edit, a text editor):
    /// one `stat` a second, a reload only when the mtime moved, and
    /// only fields whose file value changed are applied, so a live
    /// theme preview here isn't reverted by an unrelated edit.
    fn check_config_file(&mut self) {
        let mtime = config_mtime();
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let new = Config::load();
        let old = std::mem::replace(&mut self.file_config, new.clone());
        if new.theme != old.theme {
            self.theme = Theme::load(&new.theme, crate::config::user_themes_dir().as_deref());
        }
        if new.theme_light != old.theme_light || new.theme_dark != old.theme_dark {
            self.theme_pair = ThemePair::new(new.theme_light.clone(), new.theme_dark.clone());
        }
        if new.banner_font != old.banner_font {
            self.app.banner_font = new.banner_font.clone();
        }
        if new.default_agent != old.default_agent {
            self.app.default_agent = new.default_agent.clone();
        }
        if new.editor != old.editor {
            self.app.editor = new.editor.clone();
        }
        if new.worktree_location != old.worktree_location {
            self.app.worktree_location = new.worktree_location;
        }
        if new.show_group_in_title != old.show_group_in_title {
            self.app.show_group_in_title = new.show_group_in_title;
        }
        if new.hide_single_tab_strip != old.hide_single_tab_strip {
            self.app.hide_single_tab_strip = new.hide_single_tab_strip;
        }
        if new.remove_dead_sessions != old.remove_dead_sessions {
            self.app.remove_dead_sessions = new.remove_dead_sessions;
        }
        if new.embed_enabled != old.embed_enabled {
            self.embed_enabled = new.embed_enabled;
        }
        if new.agent_hooks != old.agent_hooks {
            self.agent_hooks = new.agent_hooks;
            let _ = self.cmd_tx.send(Command::SetAgentHooks(new.agent_hooks));
        }
        if new.sidebar != old.sidebar && new.sidebar != self.app.sidebar {
            // A row the TUI moved into or out of Snoozed or Docked is
            // parked or unparked, as for a client's `sidebar_save`.
            let mut model = new.sidebar.clone();
            let changes = self.app.sidebar.adopt(&mut model, now_ms(), false);
            let fixed = model != new.sidebar;
            self.app.sidebar = model;
            self.app.selected = 0;
            if fixed {
                self.save_sidebar_and_history();
            }
            self.parking_effects(changes);
        }
        if new.session_history != old.session_history {
            self.app.session_history = new.session_history.clone();
        }
        // `Config::load` already fell back to the defaults if the table
        // is invalid, as the TUI does.
        self.keybindings = KeybindingsWire::from(&new.keybindings);
    }
}

fn config_mtime() -> Option<SystemTime> {
    let path = crate::config::config_dir()?.join("config.toml");
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn worktree_str(loc: WorktreeLocation) -> &'static str {
    match loc {
        WorktreeLocation::Subdir => "subdir",
        WorktreeLocation::Sibling => "sibling",
    }
}

/// Bundled themes plus the user's, in picker order. A user theme with
/// a bundled name replaces it (and is reported as `user`).
fn theme_entries() -> Vec<ThemeEntry> {
    let user_dir = crate::config::user_themes_dir();
    let is_user = |name: &str| {
        user_dir
            .as_ref()
            .is_some_and(|d| d.join(format!("{name}.toml")).is_file())
    };
    Theme::available(user_dir.as_deref())
        .into_iter()
        .map(|name| ThemeEntry {
            source: if is_user(&name) {
                ThemeSource::User
            } else {
                ThemeSource::Bundled
            },
            name,
        })
        .collect()
}

fn same_user(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        // SAFETY: geteuid has no preconditions.
        Ok(cred) => cred.uid() == unsafe { libc::geteuid() },
        Err(_) => false,
    }
}

/// Start the reader and writer tasks for one connection. Returns the
/// sender for outgoing frames; dropping it ends the writer, which
/// closes the socket after flushing what was queued.
fn spawn_connection(
    id: u64,
    stream: UnixStream,
    events: mpsc::UnboundedSender<(u64, ClientEvent)>,
) -> mpsc::UnboundedSender<Vec<u8>> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if frame::write_frame(&mut writer, &f).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    tokio::spawn(async move {
        // EOF or a read error both end the connection.
        while let Ok(Some(bytes)) = frame::read_frame(&mut reader).await {
            if events.send((id, ClientEvent::Frame(bytes))).is_err() {
                return;
            }
        }
        let _ = events.send((id, ClientEvent::Closed));
    });
    tx
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_messages_classify_as_the_actor_means_them() {
        // Every Warn format in tmux_actor.rs, filled in.
        for ok in [
            "created bosun-x-1",
            "restarted api",
            "modified api — press Ctrl+R to apply",
            "list: tmux command failed: boom",
            "live refresh off: no tmux",
        ] {
            assert!(!is_error_message(ok), "{ok}");
        }
        for err in [
            "create: tmux command failed: duplicate session",
            "kill: tmux command failed: x",
            "kill container: a: x",
            "rename: x",
            "move tab: x",
            "restart: x",
            "restart read: x",
            "cannot restart: session predates metadata support",
            "launch: x",
            "launch read: x",
            "modify display: x",
            "modify metadata: x",
            "modify read: x",
            "modify: session predates metadata support",
            "worktree cleanup: x",
            "worktree has uncommitted changes; not removed",
            "worktree status: x",
            "merge feat conflicted; aborted, worktree kept",
            "merge feat: x (also failed to abort: y)",
            "merged, but worktree remove failed: x",
            "worktree remove: x",
            "branch delete feat: x",
        ] {
            assert!(is_error_message(err), "{err}");
        }
    }

    #[test]
    fn socket_and_lock_live_in_the_data_dir() {
        let dir = crate::store::data_dir().unwrap();
        assert_eq!(socket_path().unwrap(), dir.join("bosun.sock"));
        assert_eq!(lock_path().unwrap(), dir.join("bosun-serve.lock"));
    }

    #[test]
    fn second_lock_reports_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = InstanceLock::acquire(&path).expect("first lock");
        match InstanceLock::acquire(&path) {
            Err(StartError::AlreadyRunning { pid }) => {
                assert_eq!(pid, Some(std::process::id()))
            }
            other => panic!("expected AlreadyRunning, got {:?}", other.err()),
        }
        drop(first);
        // A child another test forks in this instant holds a copy of the
        // fd (and so the flock) until its exec closes it: allow a moment.
        let mut again = InstanceLock::acquire(&path);
        for _ in 0..50 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            again = InstanceLock::acquire(&path);
        }
        again.expect("lock is free again");
    }
}
