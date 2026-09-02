//! The sole owner of the tmux client + per-session smoothing state.
//!
//! ## Architecture (post tmux -C rewrite)
//!
//! Prior to the v0.2.0 rewrite, refreshes were driven by a 1Hz
//! `poller` task that fired `Tick` events into the main loop, which
//! in turn generated `Command::ListNow` for this actor. That had two
//! problems: (1) wasted work for idle sessions, and (2) during a long
//! `perform_attach` the tick backlog could fill bounded channels and
//! cascade into a mutual-wait deadlock between main and this actor.
//!
//! Both problems went away with the move to tmux control mode. Now
//! this actor owns a long-lived [`ControlClient`] subprocess
//! (`tmux -C attach-session -t __bosun_monitor`) and uses
//! `tokio::select!` to wait on **either** a command from main **or**
//! an asynchronous notification from tmux. Session-list refreshes
//! run on relevant notifications (session added/closed/renamed,
//! window added/closed) instead of on a timer. Zero work on an idle
//! server, zero tick backlog during long attaches.
//!
//! `Command::FocusPreview` still lets the app prioritize capturing a
//! specific session's pane immediately — useful on selection change
//! so the preview updates without waiting for a notification.
//!
//! Attach stays handled inline by the app task (needs the controlling
//! tty). This actor only handles read-only operations, command
//! execution, and the status bar side effects.
//!
//! ## Fallback
//!
//! If the control client fails to spawn at startup (e.g. tmux not
//! installed or a permissions issue), this actor emits a `Warn`
//! message and continues in **commands-only** mode — refreshes still
//! run when main sends `Command::ListNow` or any lifecycle command,
//! but there are no push updates. It's degraded, not dead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::events::{AppMsg, ClaudeSessionMode, Command, SessionSpec, SpecOptions};
use crate::store::Store;
use crate::tmux::attach::{
    clear_ctrl_q_bound, clear_quick_jump_bound, clear_session_cycle_bound, ensure_ctrl_q_bound,
    ensure_quick_jump_bound, ensure_session_cycle_bound,
};
use crate::tmux::control::Notification;
use crate::tmux::control_client::ControlClient;
use crate::tmux::detector::{DetectContext, DetectorRegistry, Status};
use crate::tmux::session::SessionView;
use crate::tmux::status_bar::{self, BarSession};
use crate::tmux::{CreateSpec, SessionMetadata, TmuxClient};
use crate::util::collision::resolve_name_collision;
use crate::util::hysteresis::Smoother;

/// RAII cleanup for globals installed by the status bar (prefix-1..9
/// bindings), the C-q detach binding, the S-Left / S-Right session
/// cycle bindings, and the M-O quick-jump popup binding. Per-session
/// status-* options are left in place when the actor exits — they die
/// with their sessions, and leaving them means a restarting bosun can
/// reuse them without a reinit flash.
struct GlobalsGuard {
    socket: Option<String>,
    installed: bool,
    cq_installed: bool,
    cycle_installed: bool,
    quick_jump_installed: bool,
    /// When the key-binding self-heal last ran. See [`SELF_HEAL_EVERY`].
    last_self_heal: std::time::Instant,
}

/// How often `do_refresh` re-asserts the C-q / S-Left / S-Right / M-O
/// bindings.
///
/// These used to be re-asserted on *every* refresh, which meant three
/// blocking `tmux bind-key` execs a second for the whole life of the
/// process — forever, on an idle server, just in case something
/// clobbered the root key table. Nothing does that on a sub-minute
/// cadence (it takes a `source-file` or another tool's hook), so a
/// half-minute self-heal is just as safe and costs ~0.1 exec/s
/// instead of 3.
const SELF_HEAL_EVERY: Duration = Duration::from_secs(30);

/// How long a session has to have been quiet before [`refresh_all`]
/// stops re-capturing it and reuses the snapshot it already has.
///
/// tmux reports `session_activity` with one-second resolution, so a
/// capture taken during second T can miss output that lands later in
/// the same second without the timestamp moving. Requiring the
/// recorded activity to be at least this old closes that race:
/// anything that produced output recently is always re-captured, and
/// only genuinely quiet sessions — the common case once a handful of
/// agents are parked at a prompt — skip the exec.
const CAPTURE_REUSE_AFTER: Duration = Duration::from_secs(2);

/// Per-session state that outlives a single refresh pass.
#[derive(Default)]
struct RefreshState {
    /// Status hysteresis, keyed by internal session name.
    smoothers: HashMap<String, Smoother>,
    /// The last `capture-pane` for each session, so a quiet session
    /// doesn't cost a `tmux` exec on every 1Hz tick. See
    /// [`CAPTURE_REUSE_AFTER`].
    captures: HashMap<String, CachedCapture>,
}

impl RefreshState {
    /// Drop per-session state for sessions that are no longer listed.
    fn retain(&mut self, views: &[SessionView]) {
        self.smoothers
            .retain(|name, _| views.iter().any(|v| v.name() == name));
        self.captures
            .retain(|name, _| views.iter().any(|v| v.name() == name));
    }
}

/// One session's cached `capture-pane`, plus everything we derive
/// from it. Held behind `Arc`s so reusing it is a refcount bump, not
/// a copy of the pane.
struct CachedCapture {
    /// tmux's `session_activity` at the moment of the capture.
    activity: Option<SystemTime>,
    /// The pane width the capture was taken at. A resize reflows the
    /// pane without necessarily counting as activity, so the width
    /// changing invalidates the snapshot on its own.
    width: u16,
    ansi: Arc<[u8]>,
    plain: Arc<str>,
    hash: u64,
}

impl CachedCapture {
    fn new(activity: Option<SystemTime>, width: u16, ansi: Vec<u8>) -> Self {
        let plain: Arc<str> = Arc::from(crate::tmux::detector::strip_ansi(&ansi));
        // Fingerprint the visible text so the app can tell when a row
        // has changed since the user last looked at it (the unread
        // dot). Cheap, and rides the capture we already did — no extra
        // tmux exec.
        let hash = content_hash(&plain);
        Self {
            activity,
            width,
            ansi: Arc::from(ansi.into_boxed_slice()),
            plain,
            hash,
        }
    }

    fn snapshot(&self) -> (Arc<[u8]>, Arc<str>, u64) {
        (Arc::clone(&self.ansi), Arc::clone(&self.plain), self.hash)
    }

    /// Whether this snapshot can stand in for a fresh capture: the
    /// session's activity timestamp hasn't moved since we took it,
    /// and it's old enough that tmux's one-second resolution can't be
    /// hiding newer output behind the same value.
    fn still_valid(&self, activity: Option<SystemTime>, width: u16, now: SystemTime) -> bool {
        if self.width != width {
            return false;
        }
        match (self.activity, activity) {
            (Some(prev), Some(cur)) if prev == cur => now
                .duration_since(cur)
                .map(|age| age >= CAPTURE_REUSE_AFTER)
                .unwrap_or(false),
            _ => false,
        }
    }
}

impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        if self.installed {
            status_bar::uninstall_globals(self.socket.as_deref());
        }
        if self.cq_installed {
            clear_ctrl_q_bound(self.socket.as_deref());
        }
        if self.cycle_installed {
            clear_session_cycle_bound(self.socket.as_deref());
        }
        if self.quick_jump_installed {
            clear_quick_jump_bound(self.socket.as_deref());
        }
    }
}

pub fn spawn(
    client: Arc<dyn TmuxClient>,
    socket: Option<String>,
    config: Config,
    store: Arc<Store>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<AppMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let registry = DetectorRegistry::default_stack();
        let mut refresh_state = RefreshState::default();
        let mut focused: Option<String> = None;
        // Set by `Command::EmbedActive`: a live embed is already
        // streaming the focused session's pane, so the fast preview
        // tick has nothing left to contribute. See the fast branch
        // in the select! below.
        let mut embed_active = false;
        let mut last_bar_state: Vec<BarSession> = Vec::new();
        let mut globals = GlobalsGuard {
            socket: socket.clone(),
            installed: false,
            cq_installed: false,
            cycle_installed: false,
            quick_jump_installed: false,
            last_self_heal: std::time::Instant::now(),
        };

        // Install the C-q detach binding up-front so it's live even
        // before the first tmux notification arrives. `do_refresh`
        // re-asserts it on every tick — cheap, and guards against
        // anything that clobbers the root key table mid-session.
        ensure_ctrl_q_bound(socket.as_deref());
        globals.cq_installed = true;

        // Install the S-Left / S-Right MRU session cycle bindings. Same
        // self-heal pattern as C-q: do_refresh re-asserts every tick.
        ensure_session_cycle_bound(socket.as_deref());
        globals.cycle_installed = true;

        // Install the M-O quick-jump popup binding. Same self-heal.
        ensure_quick_jump_bound(socket.as_deref());
        globals.quick_jump_installed = true;

        // Start the control-mode monitor subprocess. The guard is
        // held for the lifetime of the actor — dropping it on exit
        // kills the subprocess. `notifs` is the receive side of a
        // channel the reader task pushes parsed notifications onto.
        //
        // Fallback: if spawn fails, we log a warning and run in
        // commands-only mode (notifs = None, the select! branch
        // falls through to std::future::pending).
        let (_control_guard, mut notifs) = match ControlClient::spawn(socket.as_deref()).await {
            Ok((guard, rx)) => (Some(guard), Some(rx)),
            Err(e) => {
                tracing::warn!("tmux control mode unavailable: {}", e);
                let _ = evt_tx.send(AppMsg::Warn(format!("live refresh off: {}", e)));
                (None, None)
            }
        };

        // Internal 1Hz refresh timer. Control-mode notifications
        // drive session/window lifecycle updates, but tmux doesn't
        // notify on plain pane content changes — so without a timer,
        // the preview for the focused session would never update
        // while the underlying pane is writing output (the exact
        // "preview: capturing…" stuck state we hit on first v0.2.0
        // build). `Skip` missed-tick behavior means a slow host or
        // a long refresh doesn't produce a burst of catch-up ticks
        // afterwards — at most one tick per wake-up.
        //
        // Unlike the old standalone `poller` task, this timer lives
        // *inside* `tmux_actor` and triggers `do_refresh` directly.
        // No tick flows through `main`'s event loop, no
        // `cmd_tx`/`evt_tx` cross-channel handoff, so the back-
        // pressure deadlock that killed v0.1.x can't manifest here.
        let mut preview_tick = time::interval(Duration::from_millis(1000));
        preview_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate first tick — we're about to do a
        // refresh explicitly just below.
        preview_tick.tick().await;

        // Fast preview tick (`Config::preview_tick_ms`, default 200ms).
        // This is the v0.x "1 fps preview" fix from Step 0 of the 2.0
        // plan: re-capture just the focused session's pane on a tight
        // cadence so the preview is perceptually live without paying
        // for a full `refresh_all` (list-sessions + per-session
        // detector + statusbar diff). The full 1Hz `preview_tick`
        // above still runs and still updates the focused session's
        // preview as a side effect — the fast tick is purely additive.
        //
        // When `preview_tick_ms == 0` or there's no focused session,
        // the fast branch in the select! below is a no-op.
        let mut preview_fast_tick = if config.preview_tick_ms > 0 {
            let mut t = time::interval(Duration::from_millis(config.preview_tick_ms));
            t.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Skip the immediate first tick — the initial refresh
            // below covers it.
            t.tick().await;
            Some(t)
        } else {
            None
        };

        // Initial refresh so the UI populates without waiting for a
        // notification. Otherwise a user starting bosun against an
        // already-quiet tmux server would see an empty list until
        // something changed.
        let _ = do_refresh(
            &*client,
            &config,
            &registry,
            &mut refresh_state,
            focused.as_deref(),
            socket.as_deref(),
            &mut last_bar_state,
            &mut globals,
            &evt_tx,
            None,
        )
        .await;

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                Command::ListNow => {
                    let views = refresh_all(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                    )
                    .await;
                    match views {
                        Ok(views) => {
                            refresh_state.retain(&views);

                            // Only sync the status bar when the set of
                            // (internal, display) tuples has actually
                            // changed. Skips the ~N*7 set-option
                            // calls on ticks where nothing's moved.
                            let state: Vec<BarSession> = views
                                .iter()
                                .map(|v| BarSession {
                                    internal: v.name().to_string(),
                                    display: v.display().to_string(),
                                    attached: v.session.attached,
                                })
                                .collect();
                            if !bar_state_equal(&state, &last_bar_state) {
                                sync_status_bar(socket.as_deref(), &state, &mut globals);
                                last_bar_state = state;
                            }

                            if evt_tx
                                .send(AppMsg::SessionsRefreshed {
                                    sessions: views,
                                    select_after: None,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            if evt_tx.send(AppMsg::Warn(format!("list: {}", e))).is_err() {
                                break;
                            }
                        }
                    }
                }
                Command::FocusPreview { name } => {
                    // Set focus, then capture the newly-focused
                    // session immediately so the preview catches up
                    // to the selection without waiting up to 1s for
                    // the next preview_tick. Without this the user
                    // sees a stuck "preview: capturing…" when
                    // switching between sessions quickly.
                    //
                    // This used to run a full `do_refresh` — three
                    // `bind-key` self-heals, `list-sessions`, and a
                    // `capture-pane` per managed session — on every
                    // selection change. Moving the cursor through
                    // the list only needs the *selected* session's
                    // preview and status to be fresh; everything
                    // else stays on the 1Hz tick. Same single-
                    // session path the fast tick uses.
                    focused = Some(name);
                    refresh_focused(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        &evt_tx,
                    )
                    .await;
                }
                Command::EmbedActive(active) => {
                    embed_active = active;
                }
                Command::KillSession(internal) => {
                    match client.kill_session(&internal).await {
                        Ok(()) => {
                            // If we killed the focused session, drop
                            // the focus so the preview doesn't keep
                            // trying to capture a dead pane.
                            if focused.as_deref() == Some(internal.as_str()) {
                                focused = None;
                            }
                            // Force a refresh so the session disappears
                            // from the UI without a 1s wait.
                            let _ = do_refresh(
                                &*client,
                                &config,
                                &registry,
                                &mut refresh_state,
                                focused.as_deref(),
                                socket.as_deref(),
                                &mut last_bar_state,
                                &mut globals,
                                &evt_tx,
                                None,
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("kill: {}", e)));
                        }
                    }
                }
                Command::KillSessionRemoveWorktree {
                    internal,
                    worktree_path,
                    branch,
                    merge,
                } => {
                    handle_kill_remove_worktree(
                        &*client,
                        &internal,
                        &worktree_path,
                        &branch,
                        merge,
                        &evt_tx,
                    )
                    .await;
                    // Mirror the KillSession post-kill refresh: drop the
                    // focus if we just tore down the focused session, then
                    // force a refresh so the row disappears without a 1s wait.
                    if focused.as_deref() == Some(internal.as_str()) {
                        focused = None;
                    }
                    let _ = do_refresh(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        socket.as_deref(),
                        &mut last_bar_state,
                        &mut globals,
                        &evt_tx,
                        None,
                    )
                    .await;
                }
                Command::KillContainer { tabs } => {
                    // Multi-kill: iterate each tab serially so a
                    // failure on one doesn't abort the rest. The
                    // sidebar reconcile after the final refresh
                    // drops the now-tab-less container.
                    let mut failed = Vec::new();
                    for tab in &tabs {
                        if let Err(e) = client.kill_session(tab).await {
                            failed.push(format!("{}: {}", tab, e));
                        } else if focused.as_deref() == Some(tab.as_str()) {
                            focused = None;
                        }
                    }
                    if !failed.is_empty() {
                        let _ = evt_tx.send(AppMsg::Warn(format!(
                            "kill container: {}",
                            failed.join(", ")
                        )));
                    }
                    let _ = do_refresh(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        socket.as_deref(),
                        &mut last_bar_state,
                        &mut globals,
                        &evt_tx,
                        None,
                    )
                    .await;
                }
                Command::DeleteRecent(id) => {
                    if let Err(e) = store.delete_recent(id) {
                        tracing::warn!("delete_recent({}): {}", id, e);
                    }
                }
                Command::RestartSession {
                    internal,
                    continue_session,
                } => {
                    // In-place restart, two phases split across the
                    // actor and the app so the agent never relaunches
                    // before its OSC background responder is live
                    // (issue #2). Here we only do the *stop* half:
                    // `restart_in_place` with an empty command sends
                    // C-c until the pane drops back to a shell, then
                    // leaves it clean. The session, its internal name,
                    // and the pane all stay the same — no sidebar
                    // churn, no ghost row, no slot change. We then emit
                    // `DeferRelaunch`; the app waits for `sync_embed`
                    // to (re)attach the embed and fires
                    // `Command::LaunchAgent`, which types the command
                    // into the now-OSC-answering pane. This matches the
                    // fresh-create deferral so a cold-start `R` (whose
                    // embed may not be attached yet) no longer relaunches
                    // Codex against a dead pane and caches a dark diff.
                    match client.get_session_metadata(&internal).await {
                        Ok(Some(meta)) => {
                            let spec = metadata_to_spec(meta);
                            // Stop only — no line prep, so the relaunch
                            // call below is the sole place the prompt's
                            // precmd hooks fire (issue #2; was running a
                            // `git status` precmd twice per restart).
                            // kill_first = true: this is the stop-half,
                            // interrupting a live agent back to a shell.
                            if let Err(e) =
                                client.restart_in_place(&internal, "", false, true).await
                            {
                                let _ = evt_tx.send(AppMsg::Warn(format!("restart: {}", e)));
                                continue;
                            }
                            // Recents row is touched so this session
                            // bubbles to the top of the recents store,
                            // matching the pre-existing kill+create
                            // semantics.
                            if let Err(e) = store.upsert_recent(&spec) {
                                tracing::warn!("store upsert on restart: {}", e);
                            }
                            let _ =
                                evt_tx.send(AppMsg::Warn(format!("restarted {}", spec.name)));
                            // Hand the relaunch back to the app so it
                            // gates on the embed (OSC responder) being
                            // attached before the agent starts.
                            let _ = evt_tx.send(AppMsg::DeferRelaunch {
                                internal: internal.clone(),
                                resume: continue_session,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(AppMsg::Warn(
                                "cannot restart: session predates metadata support".to_string(),
                            ));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("restart read: {}", e)));
                        }
                    }
                }
                Command::LaunchAgent { internal, resume } => {
                    // Deferred agent launch for a session that's sitting
                    // at a bare shell — either freshly created or just
                    // stopped by an in-place restart (issue #2). The app
                    // fires this once the OSC-answering embed has
                    // attached, so the agent's startup background probe
                    // gets a real answer. We rebuild the command from the
                    // persisted metadata; `resume` overrides the launch
                    // mode for this one launch (`None` = use the stored
                    // mode for a fresh create, `Some(b)` = the restart's
                    // one-shot choice). `restart_in_place` waits for the
                    // shell, types the command, and bursts a redraw. A
                    // `terminal` session has an empty command and is left
                    // as a plain shell.
                    match client.get_session_metadata(&internal).await {
                        Ok(Some(meta)) => {
                            let spec = metadata_to_spec(meta);
                            let command = build_launch_command(
                                &spec.agent,
                                &spec.options,
                                &spec.args,
                                &spec.name,
                                resume.unwrap_or(spec.resume),
                                &config.agent_binaries,
                            );
                            if command.is_empty() {
                                continue;
                            }
                            // prep_line = true: this call does the single
                            // C-u/Enter/C-u cleanup right before typing.
                            // kill_first = false: the pane is a known bare
                            // shell (fresh-created or already stopped), so
                            // skip the interrupting C-c that would abort a
                            // still-sourcing ~/.zshrc and break PATH.
                            if let Err(e) =
                                client.restart_in_place(&internal, &command, true, false).await
                            {
                                let _ = evt_tx.send(AppMsg::Warn(format!("launch: {}", e)));
                                continue;
                            }
                            for delay_ms in [200u64, 600, 1200] {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                let _ = do_refresh(
                                    &*client,
                                    &config,
                                    &registry,
                                    &mut refresh_state,
                                    focused.as_deref(),
                                    socket.as_deref(),
                                    &mut last_bar_state,
                                    &mut globals,
                                    &evt_tx,
                                    None,
                                )
                                .await;
                            }
                        }
                        Ok(None) => {
                            // No metadata — nothing to launch. Leave the
                            // shell as-is rather than guessing a command.
                            tracing::debug!("launch: no metadata for {}", internal);
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("launch read: {}", e)));
                        }
                    }
                }
                Command::RenameSession {
                    internal,
                    new_display,
                } => match client.set_display_name(&internal, &new_display).await {
                    Ok(()) => {
                        let _ = do_refresh(
                            &*client,
                            &config,
                            &registry,
                            &mut refresh_state,
                            focused.as_deref(),
                            socket.as_deref(),
                            &mut last_bar_state,
                            &mut globals,
                            &evt_tx,
                            None,
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = evt_tx.send(AppMsg::Warn(format!("rename: {}", e)));
                    }
                },
                Command::CreateSession(spec) => {
                    // Collision-check against the CURRENT live sessions
                    // so "Bosun" auto-becomes "Bosun 2" when a session
                    // with the same display name already exists.
                    let spec = match resolve_collision(&*client, &config, spec).await {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("create: {}", e)));
                            continue;
                        }
                    };

                    // Defer the agent launch to a post-embed
                    // `LaunchAgent` whenever embeds are on, so the OSC
                    // background-color responder is live before the
                    // agent probes (issue #2). With embeds off there's
                    // no responder anyway, so launch inline.
                    let defer_launch = config.embed_enabled;
                    match create_session(&*client, &config, spec.clone(), defer_launch).await {
                        Ok(internal_name) => {
                            focused = Some(internal_name.clone());
                            // Save the recent (on the resolved spec —
                            // so if "Bosun" became "Bosun 2", the
                            // recents store remembers "Bosun 2").
                            if let Err(e) = store.upsert_recent(&spec) {
                                tracing::warn!("store upsert_recent: {}", e);
                            }
                            let _ = evt_tx.send(AppMsg::Warn(format!("created {}", internal_name)));
                            let _ = do_refresh(
                                &*client,
                                &config,
                                &registry,
                                &mut refresh_state,
                                focused.as_deref(),
                                socket.as_deref(),
                                &mut last_bar_state,
                                &mut globals,
                                &evt_tx,
                                Some(internal_name),
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = evt_tx.send(AppMsg::Warn(format!("create: {}", e)));
                        }
                    }
                }
                Command::OpenModifySession { internal } => {
                    // JIT read of the live `@bosun_*` metadata so
                    // the modify modal pre-fills against the
                    // current state of the session (not whatever
                    // was last cached in the recents store).
                    // Surfacing this as a warning is fine because
                    // the only way to land here is `m` on a session
                    // that bosun didn't create — recoverable.
                    match client.get_session_metadata(&internal).await {
                        Ok(Some(meta)) => {
                            let spec = metadata_to_spec(meta);
                            let _ = evt_tx.send(AppMsg::ModifySpecReady {
                                internal,
                                spec,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(AppMsg::Warn(
                                "modify: session predates metadata support".to_string(),
                            ));
                        }
                        Err(e) => {
                            let _ = evt_tx
                                .send(AppMsg::Warn(format!("modify read: {}", e)));
                        }
                    }
                }
                Command::ModifySession { internal, spec } => {
                    // Write the new spec back as `@bosun_*` user
                    // options on the live session. The agent
                    // process keeps running with its old flags;
                    // the next `R` (restart) picks the new spec up
                    // via the same `get_session_metadata` path
                    // RestartSession already uses.
                    let meta = spec_to_metadata(&spec);
                    let mut any_err = false;
                    if let Err(e) =
                        client.set_display_name(&internal, &meta.display_name).await
                    {
                        any_err = true;
                        let _ = evt_tx
                            .send(AppMsg::Warn(format!("modify display: {}", e)));
                    }
                    if let Err(e) =
                        client.set_session_metadata(&internal, &meta).await
                    {
                        any_err = true;
                        let _ = evt_tx
                            .send(AppMsg::Warn(format!("modify metadata: {}", e)));
                    }
                    if let Err(e) = store.upsert_recent(&spec) {
                        tracing::warn!("modify upsert_recent: {}", e);
                    }
                    if !any_err {
                        let _ = evt_tx.send(AppMsg::Warn(format!(
                            "modified {} — press R to apply",
                            spec.name
                        )));
                    }
                    let _ = do_refresh(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        socket.as_deref(),
                        &mut last_bar_state,
                        &mut globals,
                        &evt_tx,
                        None,
                    )
                    .await;
                }
                Command::Attach { .. } => {
                    tracing::warn!("tmux_actor received Attach — ignored; app task handles attach");
                }
                Command::SetTheme { .. }
                | Command::ApplySetting(_)
                | Command::SaveDivider(_)
                | Command::SaveSidebar(_)
                | Command::SaveSessionHistory(_)
                | Command::SaveBannerFont(_)
                | Command::InsertSection { .. }
                | Command::RenameSection { .. }
                | Command::OpenEditor { .. } => {
                    // Pure UI state — the app loop intercepts these
                    // before forwarding. If one makes it here the
                    // intercept path is broken.
                    tracing::warn!("tmux_actor received UI-only command — should be intercepted by app");
                }
                        Command::Shutdown => break,
                    }
                }
                maybe_notif = async {
                    // If the control client failed at spawn or has
                    // since closed, disable this branch by awaiting
                    // a future that never resolves. select! will
                    // then only poll the cmd branch.
                    match notifs.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(notif) = maybe_notif else {
                        // Reader task exited — monitor is gone. Fall
                        // back to commands-only mode for the rest of
                        // this actor's lifetime.
                        tracing::warn!("tmux control notification stream closed");
                        notifs = None;
                        continue;
                    };

                    // Lifecycle notifications trigger a full refresh.
                    // Pane `%output` and layout changes are ignored
                    // for now — status detection still runs against
                    // pane captures on refresh, and preview updates
                    // come via FocusPreview commands. (A future
                    // improvement can wire %output into the
                    // detectors for push-based status + preview.)
                    let should_refresh = matches!(
                        notif,
                        Notification::SessionsChanged
                            | Notification::SessionChanged { .. }
                            | Notification::SessionRenamed { .. }
                            | Notification::SessionClosed { .. }
                            | Notification::SessionWindowChanged { .. }
                            | Notification::WindowAdd { .. }
                            | Notification::WindowClose { .. }
                            | Notification::WindowRenamed { .. }
                    );

                    if matches!(notif, Notification::Exit) {
                        tracing::warn!(
                            "tmux control subprocess exited — commands-only mode"
                        );
                        notifs = None;
                        continue;
                    }

                    if should_refresh {
                        let _ = do_refresh(
                            &*client,
                            &config,
                            &registry,
                            &mut refresh_state,
                            focused.as_deref(),
                            socket.as_deref(),
                            &mut last_bar_state,
                            &mut globals,
                            &evt_tx,
                            None,
                        )
                        .await;
                    }
                }
                _ = preview_tick.tick() => {
                    // Periodic refresh for preview + status
                    // detection. See the comment on `preview_tick`
                    // above for why this lives inside the actor
                    // rather than in a separate task.
                    let _ = do_refresh(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        socket.as_deref(),
                        &mut last_bar_state,
                        &mut globals,
                        &evt_tx,
                        None,
                    )
                    .await;
                }
                _ = async {
                    // If the fast tick is disabled (`preview_tick_ms = 0`),
                    // park forever — select! just never picks this branch.
                    match preview_fast_tick.as_mut() {
                        Some(t) => { t.tick().await; }
                        None => { std::future::pending::<()>().await; }
                    }
                } => {
                    // Fast tick: live status + preview for the
                    // *focused* session only.
                    //
                    // We used to `capture-pane` every managed session
                    // on this tick to keep the whole sidebar's
                    // Running/Waiting glyphs at `preview_tick_ms`
                    // latency. That meant `1 + N` tmux execs every tick
                    // (one `list-sessions` + one `capture-pane` per
                    // session) — with a dozen sessions and a 200ms
                    // tick that's ~60 short-lived `tmux` processes a
                    // second, *per bosun instance*. macOS Gatekeeper
                    // re-scans each exec of an ad-hoc-signed binary
                    // (Homebrew's tmux included) and never caches the
                    // verdict, so a high exec rate pins `syspolicyd` at
                    // hundreds of percent CPU. See the project notes.
                    //
                    // Background sessions don't need sub-second glyphs:
                    // the 1Hz `preview_tick` already captures and
                    // detects every managed session via `refresh_all`,
                    // so they stay live at 1s. Only the session the
                    // user is actually watching needs the tight
                    // cadence, so the fast tick now captures just that
                    // one — dropping the per-tick cost from `1 + N` to
                    // `1 + 1`. capture-pane failures are silently
                    // dropped; the 1Hz tick reconciles membership.
                    //
                    // Nothing focused → nothing needs the tight
                    // cadence, so skip the tick outright and don't
                    // even pay the `list-sessions` exec.
                    //
                    // Same for `embed_active`: when an embedded
                    // terminal is relaying the focused session, its
                    // vt100 grid is what `ui::preview` actually
                    // draws — the bytes this tick captures are
                    // thrown away, and the two `AppMsg`s it emits
                    // (`StatusRefreshed` + `PreviewRefreshed`) each
                    // cost a full-screen repaint in the app's run
                    // loop. That was ~10 tmux execs and ~10 redundant
                    // frames a second underneath a live embed
                    // (issue #16). The 1Hz tick still refreshes the
                    // focused session's status glyph like every
                    // other session's.
                    if focused.is_none() || embed_active {
                        continue;
                    }
                    refresh_focused(
                        &*client,
                        &config,
                        &registry,
                        &mut refresh_state,
                        focused.as_deref(),
                        &evt_tx,
                    )
                    .await;
                }
            }
        }

        // `globals` drops here → uninstall_globals runs.
        drop(globals);
    })
}

/// Kill a worktree-backed session and clean up its git worktree.
///
/// Extracted from the command loop so it's testable against the real
/// git client without spinning up the whole actor. Every failure is
/// surfaced as a `Warn` toast and leaves the worktree intact — we
/// never force-remove a dirty tree and never auto-merge past a
/// conflict.
async fn handle_kill_remove_worktree(
    client: &dyn TmuxClient,
    internal: &str,
    worktree_path: &str,
    branch: &str,
    merge: bool,
    evt_tx: &mpsc::UnboundedSender<AppMsg>,
) {
    // 1. Kill the tmux session (idempotent — fine if already gone).
    let _ = client.kill_session(internal).await;
    // 2. Resolve the main repo root from the worktree path.
    let repo = match client.main_repo_root(worktree_path).await {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx.send(AppMsg::Warn(format!("worktree cleanup: {}", e)));
            return;
        }
    };
    // 3. Dirty guard — never force-remove a dirty tree.
    match client.is_dirty(worktree_path).await {
        Ok(true) => {
            let _ = evt_tx.send(AppMsg::Warn(
                "worktree has uncommitted changes; not removed".into(),
            ));
            return;
        }
        Ok(false) => {}
        Err(e) => {
            let _ = evt_tx.send(AppMsg::Warn(format!("worktree status: {}", e)));
            return;
        }
    }
    // 4. Optional merge into the repo's current branch.
    if merge {
        if let Err(e) = client.branch_merge(&repo, branch).await {
            // A conflicted `git merge` exits non-zero but leaves the main
            // repo mid-merge (conflict markers + MERGE_HEAD). Abort so the
            // repo is restored to exactly its pre-merge state — otherwise
            // the user would have to run `git merge --abort` by hand.
            if let Err(abort_err) = client.merge_abort(&repo).await {
                let _ = evt_tx.send(AppMsg::Warn(format!(
                    "merge {}: {} (also failed to abort: {})",
                    branch, e, abort_err
                )));
            } else {
                let _ = evt_tx.send(AppMsg::Warn(format!(
                    "merge {} conflicted; aborted, worktree kept",
                    branch
                )));
            }
            return; // leave the worktree intact on a failed/conflicted merge
        }
    }
    // 5. Remove the worktree (force=false; the dirty guard above already passed).
    // Note: cleanup is not atomic. If `merge` already succeeded above and this
    // removal fails, the merge stays committed on the repo's current branch
    // while the worktree (and branch) survive — surfaced as a Warn, no rollback
    // (rolling back a git merge is riskier than leaving the stray worktree).
    if let Err(e) = client.worktree_remove(&repo, worktree_path, false).await {
        // Tell the user if the merge already landed, so they know the branch
        // was integrated even though the worktree couldn't be removed.
        let prefix = if merge {
            "merged, but worktree remove failed"
        } else {
            "worktree remove"
        };
        let _ = evt_tx.send(AppMsg::Warn(format!("{}: {}", prefix, e)));
        return;
    }
    // 6. Delete the branch only on the merge path. Surface a failure as a Warn
    // for consistency with the rest of the handler (in practice `branch -d`
    // after a successful merge, with the worktree now removed, always succeeds).
    if merge {
        if let Err(e) = client.branch_delete(&repo, branch).await {
            let _ = evt_tx.send(AppMsg::Warn(format!("branch delete {}: {}", branch, e)));
        }
    }
}

/// Assemble the internal tmux session name from the user's typed
/// display name. Internal format: `<prefix><slug>-<hex-suffix>`,
/// e.g. `bosun-my-rocket-fox-a1b2c3d4`. The display name can contain
/// caps, spaces, punctuation — anything — but the tmux-visible name
/// is a lowercase dashed slug + unique hex suffix so it's safe to
/// pass to `-t` and always unique even for duplicate display names.
fn build_internal_name(prefix: &str, display: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:08x}", nanos as u32);
    let slug = slugify(display);
    // If the slug somehow ends up empty (e.g. display was all symbols,
    // which the modal should reject but be defensive), fall back to
    // "session".
    let slug = if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    };
    format!("{}{}-{}", prefix, slug, suffix)
}

/// Lowercase slug: alphanumeric and underscores are kept (underscore
/// is valid in tmux session names); everything else collapses to
/// single dashes; leading/trailing dashes are trimmed.
///
/// Distinct on purpose from `ui::modal::new_session::slug` (which slugs
/// the git *branch* name and drops `_`). This one feeds the internal
/// tmux *session* name — don't unify them.
pub(crate) fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Reverse of `build_internal_name`: extract the slug portion from an
/// internal session name shaped like `<prefix><slug>-<8-hex>`. Returns
/// `None` if the input doesn't match the expected shape — caller can
/// then fall back to showing the raw internal name.
///
/// Used by the sidebar to render a friendlier label on "missing" rows
/// (sessions that died with a tmux server restart) and to match those
/// rows back to a `Recent` so `R` can recreate them.
pub(crate) fn slug_from_internal<'a>(internal: &'a str, prefix: &str) -> Option<&'a str> {
    let after_prefix = if prefix.is_empty() {
        internal
    } else {
        internal.strip_prefix(prefix)?
    };
    // Last `-` separates slug from the 8-hex suffix.
    let dash = after_prefix.rfind('-')?;
    let (slug, rest) = after_prefix.split_at(dash);
    let suffix = rest.strip_prefix('-')?;
    if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(slug)
    } else {
        None
    }
}

/// Map the agent + options + extra args into a shell command to type
/// into the session's pane.
///
/// We type the command directly into the user's login shell — no
/// `bash -c 'exec ...'` wrapping. Bosun runs its own tmux server on
/// a dedicated `-L bosun` socket, which is a child of the bosun
/// process, so pane shells inherit the right environment (including
/// Keychain lineage for Claude Code). The agent runs as a child of
/// the shell; Ctrl-Z suspends the agent directly, fg resumes it, and
/// when the agent exits the shell stays alive so the session doesn't
/// die.
///
/// `terminal` just types whatever extra args the user provided (or
/// nothing — you get a plain shell).
///
/// `name` is the bosun display name; for claude it's slugified and
/// passed as `--name` so the Claude Code session list shows the same
/// name as the bosun sidebar (on `--continue` restarts it re-asserts
/// the name on the resumed session).
/// Resolve the binary that launches `agent`: the user's `[agents]`
/// override from `config.toml` when set (e.g. a wrapper script that
/// fixes up the environment before exec'ing the real binary),
/// otherwise the agent name itself — every built-in agent's binary is
/// named after it.
fn agent_binary<'a>(bins: &'a HashMap<String, String>, agent: &'a str) -> &'a str {
    bins.get(agent)
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(agent)
}

fn build_agent_command(
    agent: &str,
    options: &SpecOptions,
    args: &str,
    name: &str,
    bins: &HashMap<String, String>,
) -> String {
    let args = args.trim();
    let bin = agent_binary(bins, agent);
    match agent {
        "claude" => {
            let mut parts: Vec<String> = vec![bin.into()];
            match options.claude.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue => parts.push("--continue".into()),
                ClaudeSessionMode::Resume => parts.push("--resume".into()),
            }
            if options.claude.skip_permissions {
                parts.push("--dangerously-skip-permissions".into());
            }
            let slug = slugify(name);
            if !slug.is_empty() && !args.contains("--name") {
                parts.push(format!("--name {}", slug));
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "codex" => {
            let mut parts: Vec<String> = vec![bin.into()];
            match options.codex.session_mode {
                ClaudeSessionMode::New => {}
                // `codex resume --last` continues the most recent
                // session; bare `codex resume` opens the picker.
                ClaudeSessionMode::Continue => {
                    parts.push("resume".into());
                    parts.push("--last".into());
                }
                ClaudeSessionMode::Resume => parts.push("resume".into()),
            }
            if options.codex.yolo {
                parts.push("--yolo".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "kimi" => {
            // Moonshot's Kimi Code agent — the `kimi` binary (not the
            // legacy `kimi-cli`). `--continue` resumes the working dir's
            // last session; `--session` with no id opens the interactive
            // session picker.
            let mut parts: Vec<String> = vec![bin.into()];
            match options.kimi.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue => parts.push("--continue".into()),
                ClaudeSessionMode::Resume => parts.push("--session".into()),
            }
            if options.kimi.yolo {
                parts.push("--yolo".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "opencode" => {
            // OpenCode has no CLI session picker (`--session` needs an
            // explicit id), so both Continue and a stray Resume map to
            // `--continue` — the in-TUI session list covers the rest.
            let mut parts: Vec<String> = vec![bin.into()];
            match options.opencode.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue | ClaudeSessionMode::Resume => {
                    parts.push("--continue".into())
                }
            }
            if options.opencode.auto {
                parts.push("--auto".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        "qwen" => {
            // Qwen Code: `--continue` resumes the working dir's most
            // recent session; `--resume` with no id opens the picker.
            let mut parts: Vec<String> = vec![bin.into()];
            match options.qwen.session_mode {
                ClaudeSessionMode::New => {}
                ClaudeSessionMode::Continue => parts.push("--continue".into()),
                ClaudeSessionMode::Resume => parts.push("--resume".into()),
            }
            if options.qwen.yolo {
                parts.push("--yolo".into());
            }
            if !args.is_empty() {
                parts.push(args.to_string());
            }
            parts.join(" ")
        }
        _ => args.to_string(),
    }
}

/// Build the launch command, optionally forcing a one-shot resume.
/// When `resume` is true and the agent supports it, swap in the resume
/// invocation — claude `--continue`, codex `resume --last` — instead of
/// whatever the persisted `options` would produce. Callers that resume
/// (the restart prompt's `r` action) never persist the override, so the
/// next plain launch goes back to the saved mode. For agents with no
/// resume concept (or `resume == false`) this is identical to
/// `build_agent_command`.
fn build_launch_command(
    agent: &str,
    options: &SpecOptions,
    args: &str,
    name: &str,
    resume: bool,
    bins: &HashMap<String, String>,
) -> String {
    if !resume {
        return build_agent_command(agent, options, args, name, bins);
    }
    let mut options = options.clone();
    match agent {
        "claude" => options.claude.session_mode = ClaudeSessionMode::Continue,
        "codex" => options.codex.session_mode = ClaudeSessionMode::Continue,
        "kimi" => options.kimi.session_mode = ClaudeSessionMode::Continue,
        "opencode" => options.opencode.session_mode = ClaudeSessionMode::Continue,
        "qwen" => options.qwen.session_mode = ClaudeSessionMode::Continue,
        _ => {}
    }
    build_agent_command(agent, &options, args, name, bins)
}

async fn create_session(
    client: &dyn TmuxClient,
    config: &Config,
    spec: SessionSpec,
    defer_launch: bool,
) -> crate::error::Result<String> {
    let internal = build_internal_name(&config.session_prefix, &spec.name);
    // If a worktree was requested, create it and repoint the path.
    // `spec` is taken by value (see the fn signature), so rebinding as
    // mut is valid and later shared reads/clones of `spec` still compile.
    let mut spec = spec;
    // Expand a leading `~` before the path reaches anything that won't
    // do it for us. tmux doesn't expand tildes in `new-session -c`, and
    // when the directory doesn't exist it silently falls back to $HOME
    // — so `~/work` used to produce a session sitting in `~` instead
    // (issue #10). `git -C` (the worktree path below) has the same
    // blind spot. Doing it here rather than in the new-session modal
    // covers every source of a spec: the modal, a recents entry stored
    // in the old unexpanded form, and a restart rebuilt from session
    // metadata.
    spec.path = crate::util::path::expand_tilde(&spec.path);
    // When we create a worktree below, remember (repo, worktree_path, branch)
    // so we can roll it back if a later step (the tmux `create_session`)
    // fails — otherwise the worktree + its new branch would be orphaned with
    // no session attached to them.
    let mut created_worktree: Option<(String, String, String)> = None;
    if let Some(wt) = spec.worktree.clone() {
        let repo = client.repo_root(&spec.path).await?; // errors if not a git repo
                                                        // The `Subdir` scheme drops the worktree inside the repo's own
                                                        // working tree (`<repo>/.worktrees/`). Git doesn't auto-ignore a
                                                        // nested linked worktree, so without this it shows as untracked in
                                                        // `git status` and `git add -A` would try to embed it as a gitlink.
                                                        // Exclude it locally (best-effort — never block create on this).
        if config.worktree_location == crate::config::WorktreeLocation::Subdir {
            if let Err(e) = client.ensure_excluded(&repo, "/.worktrees/").await {
                tracing::warn!("failed to exclude .worktrees/ in {}: {}", repo, e);
            }
        }
        let worktree_path = resolve_worktree_path(&repo, &wt.branch, config.worktree_location);
        client
            .worktree_add(&repo, &wt.branch, &worktree_path)
            .await?; // aborts create on failure
        created_worktree = Some((repo, worktree_path.clone(), wt.branch));
        spec.path = worktree_path; // spec_to_metadata reads spec.path + spec.worktree below
    }
    // `defer_launch` creates the pane as a bare shell and leaves the
    // agent command for a later `Command::LaunchAgent` — fired by the
    // app once the OSC-answering embed has attached (issue #2). The
    // metadata still carries the full spec, so LaunchAgent can rebuild
    // the exact command. When not deferring (embeds off), the command
    // runs as part of create, matching the pre-issue-#2 behavior.
    let command = if defer_launch {
        String::new()
    } else {
        build_launch_command(
            &spec.agent,
            &spec.options,
            &spec.args,
            &spec.name,
            spec.resume,
            &config.agent_binaries,
        )
    };
    let metadata = Some(spec_to_metadata(&spec));
    let create = CreateSpec {
        name: internal.clone(),
        display_name: Some(spec.name.clone()),
        path: spec.path.clone(),
        command,
        metadata,
    };
    match client.create_session(&create).await {
        Ok(_) => Ok(internal),
        Err(e) => {
            // Roll back a just-created worktree so a failed tmux create
            // doesn't leave an orphaned worktree + branch behind. The
            // worktree is pristine (no commits of its own), so force-remove
            // is safe. `git worktree remove` does NOT delete the branch that
            // `worktree_add -b` created, so delete it explicitly afterwards —
            // otherwise a retry with the same name hits "branch already
            // exists". Both steps are best-effort (log on failure).
            if let Some((repo, worktree_path, branch)) = created_worktree {
                if let Err(cleanup_err) = client.worktree_remove(&repo, &worktree_path, true).await
                {
                    tracing::warn!(
                        "failed to roll back worktree {} after create error: {}",
                        worktree_path,
                        cleanup_err
                    );
                } else if let Err(branch_err) = client.branch_delete(&repo, &branch).await {
                    // Only attempt the branch delete once the worktree is
                    // gone — `branch -d` is refused while the branch is
                    // checked out in a worktree.
                    tracing::warn!(
                        "failed to roll back branch {} after create error: {}",
                        branch,
                        branch_err
                    );
                }
            }
            Err(e)
        }
    }
}

/// `ClaudeSessionMode` ↔ persisted-string mapping shared by every
/// agent whose options reuse the tri-state enum.
fn mode_to_str(mode: ClaudeSessionMode) -> String {
    mode.label().to_string()
}

fn mode_from_str(s: &str) -> ClaudeSessionMode {
    match s {
        "Continue" => ClaudeSessionMode::Continue,
        "Resume" => ClaudeSessionMode::Resume,
        _ => ClaudeSessionMode::New,
    }
}

/// Project a `SessionSpec` into the persisted tmux-options shape.
fn spec_to_metadata(spec: &SessionSpec) -> SessionMetadata {
    SessionMetadata {
        display_name: spec.name.clone(),
        path: spec.path.clone(),
        agent: spec.agent.clone(),
        args: spec.args.clone(),
        claude_session_mode: mode_to_str(spec.options.claude.session_mode),
        claude_skip_permissions: spec.options.claude.skip_permissions,
        codex_session_mode: mode_to_str(spec.options.codex.session_mode),
        codex_yolo: spec.options.codex.yolo,
        kimi_session_mode: mode_to_str(spec.options.kimi.session_mode),
        kimi_yolo: spec.options.kimi.yolo,
        opencode_session_mode: mode_to_str(spec.options.opencode.session_mode),
        opencode_auto: spec.options.opencode.auto,
        qwen_session_mode: mode_to_str(spec.options.qwen.session_mode),
        qwen_yolo: spec.options.qwen.yolo,
        container_id: spec.container_id.clone(),
        // By the time this runs inside `create_session`, `spec.path` has
        // already been repointed to the resolved worktree path (see the
        // worktree branch there), so persist both from the spec.
        worktree_path: spec.worktree.is_some().then(|| spec.path.clone()),
        branch: spec.worktree.as_ref().map(|w| w.branch.clone()),
    }
}

/// Compute where a new git worktree for `branch` should live, given the
/// repo root and the configured placement scheme. Pure — the actual
/// `git worktree add` happens in `create_session`.
fn resolve_worktree_path(
    repo_root: &str,
    branch: &str,
    loc: crate::config::WorktreeLocation,
) -> String {
    use crate::config::WorktreeLocation::*;
    let repo = repo_root.trim_end_matches('/');
    match loc {
        Subdir => format!("{}/.worktrees/{}", repo, branch),
        Sibling => format!("{}-{}", repo, branch),
    }
}

/// Inverse of `spec_to_metadata` — rebuild a SessionSpec from the
/// metadata we read off a live tmux session during restart.
fn metadata_to_spec(meta: SessionMetadata) -> SessionSpec {
    use crate::events::{ClaudeOptions, CodexOptions, KimiOptions, OpencodeOptions, QwenOptions};
    SessionSpec {
        name: meta.display_name,
        path: meta.path,
        agent: meta.agent,
        args: meta.args,
        options: SpecOptions {
            claude: ClaudeOptions {
                session_mode: mode_from_str(&meta.claude_session_mode),
                skip_permissions: meta.claude_skip_permissions,
            },
            codex: CodexOptions {
                session_mode: mode_from_str(&meta.codex_session_mode),
                yolo: meta.codex_yolo,
            },
            kimi: KimiOptions {
                session_mode: mode_from_str(&meta.kimi_session_mode),
                yolo: meta.kimi_yolo,
            },
            opencode: OpencodeOptions {
                session_mode: mode_from_str(&meta.opencode_session_mode),
                auto: meta.opencode_auto,
            },
            qwen: QwenOptions {
                session_mode: mode_from_str(&meta.qwen_session_mode),
                yolo: meta.qwen_yolo,
            },
        },
        container_id: meta.container_id,
        resume: false,
        // Restart/modify never re-create the worktree — it already
        // exists on disk from the original create.
        worktree: None,
    }
}

/// Query the live session list, extract display names, and rename
/// `spec.name` via `resolve_name_collision` if needed. Pure-ish
/// wrapper; the one side-effect is the tmux list-sessions roundtrip.
async fn resolve_collision(
    client: &dyn TmuxClient,
    config: &Config,
    mut spec: SessionSpec,
) -> crate::error::Result<SessionSpec> {
    let sessions = client.list_sessions().await?;
    let existing: Vec<String> = sessions
        .into_iter()
        .filter(|s| config.manages(&s.name))
        .map(|s| s.display_name.unwrap_or(s.name))
        .collect();
    spec.name = resolve_name_collision(&spec.name, &existing);
    Ok(spec)
}

/// Capture + detect just the focused session and push its status and
/// preview bytes to the app. One `list-sessions` (for the activity
/// timestamp and pane metadata the detectors need) plus one
/// `capture-pane`, regardless of how many sessions bosun manages.
/// Shared by the fast preview tick and `Command::FocusPreview`.
/// Failures are logged and dropped; the 1Hz tick reconciles.
async fn refresh_focused(
    client: &dyn TmuxClient,
    config: &Config,
    registry: &DetectorRegistry,
    refresh_state: &mut RefreshState,
    focused: Option<&str>,
    evt_tx: &mpsc::UnboundedSender<AppMsg>,
) {
    let Some(focused) = focused else {
        return;
    };
    let raw = match client.list_sessions().await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!("focused list-sessions: {}", e);
            return;
        }
    };
    let now = SystemTime::now();
    let Some(s) = raw
        .into_iter()
        .filter(|s| config.manages(&s.name))
        .find(|s| s.name == focused)
    else {
        return;
    };
    let bytes = match client.capture_pane(&s.name).await {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("focused capture {}: {}", s.name, e);
            return;
        }
    };
    // Park the capture in the shared cache too, so the next 1Hz
    // `refresh_all` can reuse it if the session stays quiet.
    let entry = CachedCapture::new(s.last_activity, s.pane_width, bytes);
    let (ansi, plain, _) = entry.snapshot();
    refresh_state.captures.insert(s.name.clone(), entry);

    let prev = refresh_state.smoothers.get(&s.name).map(|sm| sm.current());
    let ctx = DetectContext::from_parts(
        &ansi,
        &plain,
        s.last_activity,
        now,
        prev,
        &s.name,
        s.pane_title.as_deref(),
        s.pane_command.as_deref(),
    );
    let detected = registry.detect(&ctx);
    let smoothed = refresh_state
        .smoothers
        .entry(s.name.clone())
        .or_default()
        .observe(detected);
    let publish = if smoothed == Status::Unknown {
        Status::Idle
    } else {
        smoothed
    };
    let _ = evt_tx.send(AppMsg::StatusRefreshed {
        name: s.name.clone(),
        status: publish,
    });
    let _ = evt_tx.send(AppMsg::PreviewRefreshed {
        name: s.name,
        bytes: ansi,
    });
}

#[allow(clippy::too_many_arguments)]
async fn do_refresh(
    client: &dyn TmuxClient,
    config: &Config,
    registry: &DetectorRegistry,
    refresh_state: &mut RefreshState,
    focused: Option<&str>,
    socket: Option<&str>,
    last_bar_state: &mut Vec<BarSession>,
    globals: &mut GlobalsGuard,
    evt_tx: &mpsc::UnboundedSender<AppMsg>,
    select_after: Option<String>,
) -> crate::error::Result<()> {
    // Periodically re-assert the Ctrl-Q detach binding. `bind-key` is
    // idempotent, and re-running it means the binding self-heals if
    // anything clobbers the root key table during a long-running
    // session (source-file, another tool's hook, etc). Rate-limited
    // to `SELF_HEAL_EVERY` — these are blocking execs, and at one
    // refresh a second they were three tmux processes a second for
    // the entire life of the process.
    if globals.last_self_heal.elapsed() >= SELF_HEAL_EVERY {
        globals.last_self_heal = std::time::Instant::now();
        ensure_ctrl_q_bound(socket);
        // Same self-heal for the S-Left / S-Right cycle bindings.
        ensure_session_cycle_bound(socket);
        // And for the M-O quick-jump popup binding.
        ensure_quick_jump_bound(socket);
    }

    let views = refresh_all(client, config, registry, refresh_state, focused).await?;
    refresh_state.retain(&views);

    let state: Vec<BarSession> = views
        .iter()
        .map(|v| BarSession {
            internal: v.name().to_string(),
            display: v.display().to_string(),
            attached: v.session.attached,
        })
        .collect();
    if !bar_state_equal(&state, last_bar_state) {
        sync_status_bar(socket, &state, globals);
        *last_bar_state = state;
    }

    let _ = evt_tx.send(AppMsg::SessionsRefreshed {
        sessions: views,
        select_after,
    });
    Ok(())
}

/// Whether the status bar needs rewriting. Compares membership, order
/// and display names only. `attached` is deliberately ignored: nothing
/// `sync_status_bar` writes depends on it, and bosun's own embed
/// attach/detach flips it on every sidebar move — comparing it made
/// each selection change cost a full rewrite (`show-options` + seven
/// `set-option`s per session + rebinding the nine jump keys).
fn bar_state_equal(a: &[BarSession], b: &[BarSession]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.internal == y.internal && x.display == y.display)
}

fn sync_status_bar(socket: Option<&str>, sessions: &[BarSession], globals: &mut GlobalsGuard) {
    // Install the global prefix-1..9 bindings on first non-empty state.
    if !globals.installed && !sessions.is_empty() {
        if let Err(e) = status_bar::install_globals(socket, sessions) {
            tracing::warn!("status bar: install_globals failed: {}", e);
            return;
        }
        globals.installed = true;
    } else if globals.installed {
        // Already installed — rebind in case the list changed.
        if let Err(e) = status_bar::install_globals(socket, sessions) {
            tracing::warn!("status bar: rebind jump keys failed: {}", e);
        }
    }

    // Apply per-session status-* options. Bosun only touches sessions
    // it manages; everything else keeps whatever bar it had.
    for entry in sessions {
        if let Err(e) = status_bar::configure_session(socket, &entry.internal, sessions) {
            tracing::warn!(
                "status bar: configure_session {} failed: {}",
                entry.internal,
                e
            );
        }
    }
}

/// One full refresh pass: list, filter by the configured prefix,
/// capture (with preview for focused), detect, smooth. Returns a
/// ready-to-ship Vec<SessionView>.
async fn refresh_all(
    client: &dyn TmuxClient,
    config: &Config,
    registry: &DetectorRegistry,
    state: &mut RefreshState,
    focused: Option<&str>,
) -> crate::error::Result<Vec<SessionView>> {
    let raw = client.list_sessions().await?;
    // Drop anything that doesn't match the managed-session prefix.
    // Empty prefix → everything matches.
    let sessions: Vec<_> = raw
        .into_iter()
        .filter(|s| config.manages(&s.name))
        .collect();

    let now = SystemTime::now();
    let mut out = Vec::with_capacity(sessions.len());

    for s in sessions {
        // Capture the visible pane only (no scrollback) for both status
        // detection and preview rendering. Scrollback would pick up old
        // shell command history — not what the user expects to see.
        //
        // A session that hasn't been active since the last time we
        // looked can't have changed, so reuse the previous snapshot
        // instead of paying another `tmux` exec. See
        // `CAPTURE_REUSE_AFTER` for why "quiet" needs a couple of
        // seconds of slack.
        let cached = state
            .captures
            .get(&s.name)
            .filter(|c| c.still_valid(s.last_activity, s.pane_width, now));
        let snap = match cached {
            Some(c) => c.snapshot(),
            None => {
                let ansi = match client.capture_pane(&s.name).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("capture-pane {} failed: {}", s.name, e);
                        Vec::new()
                    }
                };
                let entry = CachedCapture::new(s.last_activity, s.pane_width, ansi);
                let snap = entry.snapshot();
                state.captures.insert(s.name.clone(), entry);
                snap
            }
        };
        let (ansi, plain, content_hash) = snap;

        let prev = state.smoothers.get(&s.name).map(|sm| sm.current());
        let ctx = DetectContext::from_parts(
            &ansi,
            &plain,
            s.last_activity,
            now,
            prev,
            &s.name,
            s.pane_title.as_deref(),
            s.pane_command.as_deref(),
        );
        let detected = registry.detect(&ctx);
        let smoothed = state
            .smoothers
            .entry(s.name.clone())
            .or_default()
            .observe(detected);

        // Only hold onto the preview buffer for the focused session — the
        // others get None so we don't keep megabytes of pane history alive.
        let preview = if Some(s.name.as_str()) == focused {
            Some(ansi)
        } else {
            None
        };
        let mut view = SessionView::new(
            s,
            if smoothed == Status::Unknown {
                // Never surface Unknown to the UI — fall back to Idle so the
                // glyph is stable instead of blinking.
                Status::Idle
            } else {
                smoothed
            },
            preview,
        );
        view.content_hash = content_hash;
        out.push(view);
    }

    Ok(out)
}

/// Layout-independent fingerprint of a pane's visible plain text, used
/// for unread detection (see `AppState::session_unread`).
///
/// The point is to hash the *text*, not how it happens to be laid out
/// for the currently attached client. A resize — most visibly,
/// re-attaching from a different-size device like a phone — reflows
/// every pane, and naively hashing the raw capture would then read
/// every session as unread even though no agent produced new output.
/// Two normalizations keep the hash about content:
///
/// - `capture_pane` already passes `-J`, which rejoins lines tmux
///   wrapped to the pane width, so a width change doesn't re-split a
///   long line into a different number of pieces.
/// - [`normalize_line`] strips the parts of an agent TUI that animate
///   on their own — spinner glyphs and elapsed-time counters — and
///   collapses whitespace runs, so idle chrome doesn't read as output.
/// - blank lines are dropped entirely, so vertical blank-row
///   differences don't perturb it — this also covers an idle pane whose
///   only "change" is the cursor parking on a blank row.
///
/// Returns `0` for empty/whitespace-only text (a failed or blank
/// capture) so the app treats it as "no information" rather than a
/// change.
fn content_hash(plain: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut any = false;
    let mut buf = String::new();
    for line in plain.lines() {
        buf.clear();
        normalize_line(line, &mut buf);
        if buf.is_empty() {
            continue;
        }
        any = true;
        buf.hash(&mut h);
        0u8.hash(&mut h); // unambiguous separator between lines
    }
    if !any {
        return 0;
    }
    h.finish()
}

/// Characters agent TUIs animate purely as decoration: Claude's
/// rotating star set (U+2722..U+273F — `✢ ✳ ✶ ✻ ✽` …), the braille
/// spinner frames, and the bullets used in their place. None of them
/// carry meaning, and Claude cycles them several times a second — on
/// the backgrounded-task list, one such glyph was the *only* difference
/// between consecutive captures, which made the unread dot strobe on
/// and off as the hash wandered back onto its own baseline.
fn is_spinner_glyph(c: char) -> bool {
    matches!(
        c,
        '\u{2722}'..='\u{273f}' | '\u{2217}' | '\u{2219}' | '\u{00b7}'
    ) || crate::tmux::detector::is_braille(c)
}

/// Normalize one captured line for hashing, appending to `out`.
///
/// Drops spinner glyphs, masks elapsed-time and token counters (`3m`,
/// `1.2k`, `22d` — these tick on their own and a timestamp ageing from
/// `3m` to `4m` is emphatically not new output), and collapses
/// whitespace runs to a single space so tmux's column padding and the
/// shifting alignment around a removed glyph don't perturb the result.
/// The output is trimmed, so a line that was pure decoration comes back
/// empty and is skipped by the caller.
fn normalize_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut pending_space = false;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || is_spinner_glyph(c) {
            // Collapse to at most one space, and only once we know real
            // content follows — that keeps `✢ Ensure…` and `· Ensure…`
            // and a bare `  Ensure…` all normalizing identically.
            pending_space = !out.is_empty();
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            // Scan the numeric run, allowing interior decimal points.
            let start = i;
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit()
                    || (chars[j] == '.' && chars.get(j + 1).is_some_and(char::is_ascii_digit)))
            {
                j += 1;
            }
            // A duration/count suffix only counts when it stands alone —
            // `3m` and `1.2k` are counters, `3days` and `2_1_220` are not.
            let is_counter = chars
                .get(j)
                .is_some_and(|u| matches!(u, 's' | 'm' | 'h' | 'd' | 'k'))
                && !chars.get(j + 1).is_some_and(|n| n.is_alphanumeric());
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            if is_counter {
                out.push('#');
                i = j + 1;
            } else {
                out.extend(&chars[start..j]);
                i = j;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        i += 1;
    }
}

#[cfg(test)]
mod content_hash_tests {
    use super::content_hash;

    #[test]
    fn empty_capture_is_zero() {
        assert_eq!(content_hash(""), 0);
        assert_eq!(content_hash("   \n  \n\n"), 0);
    }

    #[test]
    fn trailing_whitespace_does_not_change_hash() {
        // tmux pads each line to the pane width; the padding must not
        // count as content, or a width change would read as unread.
        let a = content_hash("hello\nworld");
        let b = content_hash("hello   \nworld\t");
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn blank_line_padding_does_not_change_hash() {
        // A shorter terminal shows fewer/more blank rows; ignore them.
        let a = content_hash("line one\nline two");
        let b = content_hash("\nline one\n\n\nline two\n\n");
        assert_eq!(a, b);
    }

    #[test]
    fn different_text_changes_hash() {
        assert_ne!(content_hash("answer yes?"), content_hash("answer no?"));
    }

    #[test]
    fn line_boundaries_are_significant() {
        // "ab" on one line is not the same content as "a"/"b" on two —
        // the separator keeps these distinct so we don't collide real
        // text differences.
        assert_ne!(content_hash("ab"), content_hash("a\nb"));
    }

    #[test]
    fn rotating_spinner_glyph_does_not_change_hash() {
        // The reported bug, verbatim. Claude's backgrounded task list
        // cycles this glyph roughly twice a second; sampled at 1Hz the
        // hash used to walk over six values — one of which was the
        // user's own baseline — so the unread dot strobed on and off.
        // Every frame must fingerprint identically.
        let frame = |g: &str| {
            content_hash(&format!(
                "{g} Ensure forum-pro CSS works with default…  i'm pretty close …\n\
                 ❯ describe a task for a new session"
            ))
        };
        let base = frame("✢");
        for g in ["✽", "✳", "✻", "✶", "·", "∙", "∗", "⠋", "⠙"] {
            assert_eq!(base, frame(g), "spinner frame {g} perturbed the hash");
        }
        assert_ne!(base, 0);
    }

    #[test]
    fn elapsed_time_counters_do_not_change_hash() {
        // Relative timestamps in the task list tick over on their own.
        // A row ageing from 3m to 4m is not new output.
        let a = content_hash("Redesign Trilby landing page   Pre-upgrade bot traffic   3m");
        let b = content_hash("Redesign Trilby landing page   Pre-upgrade bot traffic   4m");
        assert_eq!(a, b);
        // Same for the working line's elapsed + token counters.
        assert_eq!(
            content_hash("✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)"),
            content_hash("✽ Thinking… (47s · ↑ 3.8k tokens · esc to interrupt)")
        );
    }

    #[test]
    fn real_text_changes_still_register_through_normalization() {
        // The normalizer must not be so aggressive that genuine output
        // stops registering — that would break unread entirely.
        assert_ne!(
            content_hash("✳ Investigate GitHub issue 13   4d"),
            content_hash("✳ Investigate GitHub issue 14   4d")
        );
        // A digit that isn't a counter is still content.
        assert_ne!(content_hash("issue #4194"), content_hash("issue #4195"));
        // And a whole new line is a change even if every existing line
        // normalized identically.
        assert_ne!(
            content_hash("✻ Working on it"),
            content_hash("✻ Working on it\nDone — 3 files changed")
        );
    }

    #[test]
    fn counter_suffix_must_stand_alone() {
        // `3days` and Claude's `2_1_220` process string are not counters
        // and must survive verbatim, or unrelated text would collide.
        assert_ne!(content_hash("waited 3days"), content_hash("waited 9days"));
        assert_ne!(content_hash("v2_1_220"), content_hash("v2_1_221"));
    }

    #[test]
    fn glyph_removal_does_not_disturb_alignment() {
        // Dropping a glyph shifts everything after it. Whitespace runs
        // collapse so a row with a spinner, a row with a bullet, and a
        // row with neither all fingerprint the same.
        let a = content_hash("✻ Review GitHub triage advisories");
        let b = content_hash("·  Review GitHub triage advisories");
        let c = content_hash("   Review   GitHub triage advisories");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }
}

#[cfg(test)]
mod build_cmd_tests {
    use super::*;
    use crate::events::{ClaudeOptions, CodexOptions, KimiOptions, OpencodeOptions, QwenOptions};

    fn opts() -> SpecOptions {
        SpecOptions::default()
    }

    fn bins() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn claude_with_no_options_is_bare() {
        assert_eq!(
            build_agent_command("claude", &opts(), "", "", &bins()),
            "claude"
        );
    }

    #[test]
    fn claude_continue_adds_flag() {
        let mut o = opts();
        o.claude.session_mode = ClaudeSessionMode::Continue;
        assert_eq!(
            build_agent_command("claude", &o, "", "", &bins()),
            "claude --continue"
        );
    }

    #[test]
    fn claude_resume_skip_permissions_combines() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                session_mode: ClaudeSessionMode::Resume,
                skip_permissions: true,
            },
            codex: CodexOptions::default(),
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("claude", &o, "", "", &bins()),
            "claude --resume --dangerously-skip-permissions"
        );
    }

    #[test]
    fn claude_with_extra_args_appends() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                skip_permissions: true,
                ..Default::default()
            },
            codex: CodexOptions::default(),
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("claude", &o, "--model=opus", "", &bins()),
            "claude --dangerously-skip-permissions --model=opus"
        );
    }

    #[test]
    fn claude_name_appends_slugified_display_name() {
        assert_eq!(
            build_agent_command("claude", &opts(), "", "My Rocket Fox", &bins()),
            "claude --name my-rocket-fox"
        );
    }

    #[test]
    fn claude_name_combines_with_flags_and_args() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                session_mode: ClaudeSessionMode::Continue,
                skip_permissions: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("claude", &o, "--model=opus", "Bosun Fix", &bins()),
            "claude --continue --dangerously-skip-permissions --name bosun-fix --model=opus"
        );
    }

    #[test]
    fn claude_name_skipped_when_user_args_set_their_own() {
        // A user-supplied --name in the extra args wins; don't emit a
        // duplicate that commander would reject.
        assert_eq!(
            build_agent_command("claude", &opts(), "--name custom", "My Session", &bins()),
            "claude --name custom"
        );
    }

    #[test]
    fn claude_name_skipped_when_slug_is_empty() {
        assert_eq!(
            build_agent_command("claude", &opts(), "", "!!!", &bins()),
            "claude"
        );
    }

    #[test]
    fn launch_resume_keeps_claude_name() {
        // The `r` restart re-asserts the bosun name on the resumed session.
        assert_eq!(
            build_launch_command("claude", &opts(), "", "My Rocket Fox", true, &bins()),
            "claude --continue --name my-rocket-fox"
        );
    }

    #[test]
    fn name_ignored_for_other_agents() {
        assert_eq!(
            build_agent_command("codex", &opts(), "", "My Fox", &bins()),
            "codex"
        );
        assert_eq!(
            build_agent_command("kimi", &opts(), "", "My Fox", &bins()),
            "kimi"
        );
        assert_eq!(
            build_agent_command("terminal", &opts(), "", "My Fox", &bins()),
            ""
        );
    }

    #[test]
    fn codex_yolo() {
        let o = SpecOptions {
            codex: CodexOptions {
                yolo: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("codex", &o, "", "", &bins()),
            "codex --yolo"
        );
    }

    #[test]
    fn codex_continue_uses_resume_last() {
        let o = SpecOptions {
            codex: CodexOptions {
                session_mode: ClaudeSessionMode::Continue,
                yolo: false,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("codex", &o, "", "", &bins()),
            "codex resume --last"
        );
    }

    #[test]
    fn codex_resume_opens_picker() {
        let o = SpecOptions {
            codex: CodexOptions {
                session_mode: ClaudeSessionMode::Resume,
                yolo: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("codex", &o, "", "", &bins()),
            "codex resume --yolo"
        );
    }

    #[test]
    fn opencode_defaults_bare_and_continue_flags() {
        assert_eq!(
            build_agent_command("opencode", &opts(), "", "", &bins()),
            "opencode"
        );
        let o = SpecOptions {
            opencode: OpencodeOptions {
                session_mode: ClaudeSessionMode::Continue,
                auto: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("opencode", &o, "-m anthropic/claude", "", &bins()),
            "opencode --continue --auto -m anthropic/claude"
        );
    }

    #[test]
    fn opencode_resume_maps_to_continue() {
        // No CLI picker in opencode — a stray Resume mode degrades to
        // `--continue` instead of emitting an invalid flag.
        let o = SpecOptions {
            opencode: OpencodeOptions {
                session_mode: ClaudeSessionMode::Resume,
                auto: false,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("opencode", &o, "", "", &bins()),
            "opencode --continue"
        );
    }

    #[test]
    fn qwen_modes_and_yolo() {
        assert_eq!(
            build_agent_command("qwen", &opts(), "", "", &bins()),
            "qwen"
        );
        let mut o = opts();
        o.qwen = QwenOptions {
            session_mode: ClaudeSessionMode::Continue,
            yolo: true,
        };
        assert_eq!(
            build_agent_command("qwen", &o, "", "", &bins()),
            "qwen --continue --yolo"
        );
        o.qwen.session_mode = ClaudeSessionMode::Resume;
        assert_eq!(
            build_agent_command("qwen", &o, "-m qwen3-coder-plus", "", &bins()),
            "qwen --resume --yolo -m qwen3-coder-plus"
        );
    }

    #[test]
    fn binary_override_replaces_agent_binary() {
        let mut b = bins();
        b.insert("opencode".into(), "/Users/me/bin/opencode-wrapper".into());
        assert_eq!(
            build_agent_command("opencode", &opts(), "", "", &b),
            "/Users/me/bin/opencode-wrapper"
        );
        // Other agents are untouched by an unrelated override.
        assert_eq!(build_agent_command("claude", &opts(), "", "", &b), "claude");
    }

    #[test]
    fn binary_override_applies_on_resume_launch() {
        let mut b = bins();
        b.insert("codex".into(), "codex-nightly".into());
        assert_eq!(
            build_launch_command("codex", &opts(), "", "", true, &b),
            "codex-nightly resume --last"
        );
    }

    #[test]
    fn blank_binary_override_falls_back_to_agent_name() {
        let mut b = bins();
        b.insert("qwen".into(), "   ".into());
        assert_eq!(build_agent_command("qwen", &opts(), "", "", &b), "qwen");
    }

    #[test]
    fn kimi_uses_kimi_binary_and_defaults_bare() {
        assert_eq!(
            build_agent_command("kimi", &opts(), "", "", &bins()),
            "kimi"
        );
    }

    #[test]
    fn kimi_continue_and_yolo_combine() {
        let o = SpecOptions {
            kimi: KimiOptions {
                session_mode: ClaudeSessionMode::Continue,
                yolo: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("kimi", &o, "-m k2", "", &bins()),
            "kimi --continue --yolo -m k2"
        );
    }

    #[test]
    fn kimi_resume_uses_session_flag() {
        let o = SpecOptions {
            kimi: KimiOptions {
                session_mode: ClaudeSessionMode::Resume,
                yolo: false,
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("kimi", &o, "", "", &bins()),
            "kimi --session"
        );
    }

    #[test]
    fn kimi_launch_resume_override_forces_continue() {
        // The `r` restart action forces `--continue` for kimi without
        // touching the persisted session mode, mirroring claude.
        let o = SpecOptions {
            kimi: KimiOptions {
                session_mode: ClaudeSessionMode::New,
                yolo: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_launch_command("kimi", &o, "", "", true, &bins()),
            "kimi --continue --yolo"
        );
    }

    #[test]
    fn terminal_ignores_options_runs_args() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                skip_permissions: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_agent_command("terminal", &o, "vim .zshrc", "", &bins()),
            "vim .zshrc"
        );
        assert_eq!(
            build_agent_command("terminal", &opts(), "", "", &bins()),
            ""
        );
    }

    #[test]
    fn launch_without_resume_matches_plain_build() {
        assert_eq!(
            build_launch_command("claude", &opts(), "", "", false, &bins()),
            "claude"
        );
    }

    #[test]
    fn launch_resume_forces_claude_continue() {
        // Persisted mode is the default (New); the one-shot resume
        // override swaps in `--continue` without touching the options.
        assert_eq!(
            build_launch_command("claude", &opts(), "", "", true, &bins()),
            "claude --continue"
        );
    }

    #[test]
    fn launch_resume_keeps_other_claude_flags() {
        let o = SpecOptions {
            claude: ClaudeOptions {
                session_mode: ClaudeSessionMode::New,
                skip_permissions: true,
            },
            ..Default::default()
        };
        assert_eq!(
            build_launch_command("claude", &o, "--model=opus", "", true, &bins()),
            "claude --continue --dangerously-skip-permissions --model=opus"
        );
    }

    #[test]
    fn launch_resume_uses_codex_resume_last() {
        assert_eq!(
            build_launch_command("codex", &opts(), "", "", true, &bins()),
            "codex resume --last"
        );
    }

    #[test]
    fn launch_resume_codex_keeps_yolo_and_args() {
        let o = SpecOptions {
            codex: CodexOptions {
                yolo: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_launch_command("codex", &o, "--model gpt-5", "", true, &bins()),
            "codex resume --last --yolo --model gpt-5"
        );
    }

    #[test]
    fn launch_resume_noop_for_terminal() {
        assert_eq!(
            build_launch_command("terminal", &opts(), "vim .zshrc", "", true, &bins()),
            "vim .zshrc"
        );
    }

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("My Rocket Fox"), "my-rocket-fox");
        assert_eq!(slugify("Foo.Bar_baz"), "foo-bar_baz");
        assert_eq!(slugify("  leading space"), "leading-space");
        assert_eq!(slugify("multi   spaces"), "multi-spaces");
        assert_eq!(slugify("trailing!!!"), "trailing");
    }

    #[test]
    fn slug_from_internal_strips_prefix_and_hex_suffix() {
        assert_eq!(
            slug_from_internal("bosun-raycast-1e18ae00", "bosun-"),
            Some("raycast")
        );
        assert_eq!(
            slug_from_internal("bosun-my-rocket-fox-a1b2c3d4", "bosun-"),
            Some("my-rocket-fox")
        );
        // Empty prefix (BOSUN_PREFIX="") is allowed.
        assert_eq!(slug_from_internal("raycast-1e18ae00", ""), Some("raycast"));
    }

    #[test]
    fn slug_from_internal_rejects_non_hex_suffix() {
        // Last 8 chars after `-` aren't hex → not bosun-shaped, decline.
        assert_eq!(slug_from_internal("bosun-foo-zzzzzzzz", "bosun-"), None);
        // Suffix is hex but wrong length.
        assert_eq!(slug_from_internal("bosun-foo-abc", "bosun-"), None);
        // No prefix match.
        assert_eq!(slug_from_internal("other-foo-12345678", "bosun-"), None);
    }
}

#[cfg(test)]
mod kill_remove_worktree_tests {
    use super::*;
    use crate::tmux::client::TokioTmuxClient;

    /// Spawn `git -C <dir> <args>`, asserting the command succeeds.
    /// Local copy — `client.rs`'s git_tests helper is private to that
    /// module.
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create an initialised repo with one empty commit at `dir/repo`.
    fn init_repo(dir: &std::path::Path) -> std::path::PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "t@t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        repo
    }

    #[tokio::test]
    async fn kill_remove_worktree_merge_path_removes_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // Commit a file on `feat` inside the worktree.
        std::fs::write(wt.join("f.txt"), "data").unwrap();
        run_git(&wt, &["add", "f.txt"]);
        run_git(&wt, &["commit", "-q", "-m", "add f"]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // `internal` names a tmux session that doesn't exist —
        // kill_session is idempotent, so no tmux server is needed.
        handle_kill_remove_worktree(
            &client,
            "nonexistent-sess",
            wt.to_str().unwrap(),
            "feat",
            true,
            &tx,
        )
        .await;

        // Worktree dir gone.
        assert!(!wt.exists(), "worktree dir should be removed");
        // Branch `feat` gone (merge path deletes it).
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "feat"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "branch feat should be deleted"
        );
        // The feat commit's file is present on the repo's checked-out
        // branch (i.e. it was merged in).
        assert!(repo.join("f.txt").exists(), "feat commit should be merged");
        // No warnings on the happy path.
        assert!(rx.try_recv().is_err(), "no warning expected on success");
    }

    #[tokio::test]
    async fn kill_remove_worktree_conflicted_merge_aborts_and_leaves_repo_clean() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());

        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        // Branch `feat` from the initial (empty) commit BEFORE either side
        // touches x.txt, so the two sides diverge from a common base that
        // lacks the file → a real content conflict on merge.
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // Main side commits x.txt one way...
        std::fs::write(repo.join("x.txt"), "main-side\n").unwrap();
        run_git(&repo, &["add", "x.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "main x"]);
        // ...and the feat worktree commits the same path differently.
        std::fs::write(wt.join("x.txt"), "feat-side\n").unwrap();
        run_git(&wt, &["add", "x.txt"]);
        run_git(&wt, &["commit", "-q", "-m", "feat x"]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle_kill_remove_worktree(
            &client,
            "nonexistent-sess",
            wt.to_str().unwrap(),
            "feat",
            true,
            &tx,
        )
        .await;

        // A Warn about the merge was surfaced.
        match rx.try_recv() {
            Ok(AppMsg::Warn(msg)) => {
                assert!(
                    msg.contains("merge"),
                    "warning should mention the merge: {msg}"
                );
            }
            other => panic!("expected a merge Warn, got {other:?}"),
        }
        // The worktree is left intact — a failed merge must not remove it.
        assert!(wt.exists(), "worktree must survive a conflicted merge");
        // The main repo must NOT be stuck mid-merge: no MERGE_HEAD and a
        // clean working tree (the abort restored the pre-merge state).
        assert!(
            !repo.join(".git").join("MERGE_HEAD").exists(),
            "MERGE_HEAD must be gone — merge should have been aborted"
        );
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "main repo working tree must be clean after abort"
        );
    }

    #[tokio::test]
    async fn kill_remove_worktree_dirty_tree_is_left_intact() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // Dirty the worktree with an untracked file → is_dirty true.
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle_kill_remove_worktree(
            &client,
            "nonexistent-sess",
            wt.to_str().unwrap(),
            "feat",
            false,
            &tx,
        )
        .await;

        // Removal refused → worktree dir still exists.
        assert!(wt.exists(), "dirty worktree must be left intact");
        // A Warn was surfaced.
        match rx.try_recv() {
            Ok(AppMsg::Warn(msg)) => {
                assert!(
                    msg.contains("uncommitted"),
                    "warning should mention uncommitted changes: {msg}"
                );
            }
            other => panic!("expected a Warn, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod worktree_tests {
    use super::*;
    use crate::config::WorktreeLocation;

    fn minimal_spec() -> SessionSpec {
        SessionSpec {
            name: "api".into(),
            path: "/srv/api".into(),
            agent: "claude".into(),
            args: String::new(),
            options: SpecOptions::default(),
            container_id: None,
            resume: false,
            worktree: None,
        }
    }

    #[test]
    fn spec_to_metadata_carries_worktree() {
        let mut spec = minimal_spec();
        spec.worktree = Some(crate::events::WorktreeSpec {
            branch: "feat".into(),
        });
        // path is set by the actor to the resolved worktree path before
        // persist; here it's the spec's path. Both halves of the derivation
        // must round-trip: branch from the WorktreeSpec, worktree_path from
        // spec.path (only when a worktree was requested).
        let meta = spec_to_metadata(&spec);
        assert_eq!(meta.branch.as_deref(), Some("feat"));
        assert_eq!(meta.worktree_path.as_deref(), Some("/srv/api"));
    }

    #[test]
    fn resolve_worktree_path_subdir_and_sibling() {
        assert_eq!(
            resolve_worktree_path("/srv/proj", "feat", WorktreeLocation::Subdir),
            "/srv/proj/.worktrees/feat"
        );
        assert_eq!(
            resolve_worktree_path("/srv/proj", "feat", WorktreeLocation::Sibling),
            "/srv/proj-feat"
        );
        // A trailing slash on the repo root is normalized away.
        assert_eq!(
            resolve_worktree_path("/srv/proj/", "feat", WorktreeLocation::Subdir),
            "/srv/proj/.worktrees/feat"
        );
    }
}

#[cfg(test)]
mod capture_cache_tests {
    use super::*;

    /// A capture only stands in for a fresh one when the session has
    /// been demonstrably quiet: same activity stamp, same pane width,
    /// and the stamp old enough that tmux's one-second resolution
    /// can't be hiding newer output behind it.
    #[test]
    fn cached_capture_reuse_needs_a_quiet_session() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let cap = CachedCapture::new(Some(at), 80, b"hello".to_vec());

        // Quiet for longer than the grace period — reuse it.
        assert!(cap.still_valid(Some(at), 80, at + Duration::from_secs(5)));
        // Same second as the capture: more output could still land
        // without moving the stamp, so re-capture.
        assert!(!cap.still_valid(Some(at), 80, at + Duration::from_millis(500)));
        // Activity moved — the pane wrote something.
        assert!(!cap.still_valid(
            Some(at + Duration::from_secs(1)),
            80,
            at + Duration::from_secs(5)
        ));
        // Same activity but the pane was resized, so it reflowed.
        assert!(!cap.still_valid(Some(at), 120, at + Duration::from_secs(5)));
        // No activity stamp at all (tmux didn't report one) — never reuse.
        assert!(!cap.still_valid(None, 80, at + Duration::from_secs(5)));
    }

    /// The snapshot handed to callers is the derived text and hash,
    /// not just the raw bytes — that's what lets a reused capture
    /// skip `strip_ansi` and the content hash as well as the exec.
    #[test]
    fn cached_capture_snapshot_carries_derived_text() {
        let cap = CachedCapture::new(None, 80, b"\x1b[31mred\x1b[0m text".to_vec());
        let (ansi, plain, hash) = cap.snapshot();
        assert_eq!(&*ansi, b"\x1b[31mred\x1b[0m text");
        assert_eq!(&*plain, "red text");
        assert_eq!(hash, content_hash("red text"));
    }
}
