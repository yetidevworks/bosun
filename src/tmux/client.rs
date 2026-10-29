//! Shell-out layer for tmux. Every byte of tmux I/O lives here or in
//! `attach.rs`. Exposing a trait lets us plug a mock for unit tests.

use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{BosunError, Result};
use crate::tmux::command_channel::{CommandChannel, Outcome};
use crate::tmux::parse::{
    parse_list_sessions, parse_status_rows, LIST_SESSIONS_FORMAT, STATUS_POLL_FORMAT,
};
use crate::tmux::session::TmuxSession;

/// Spec for creating a new tmux session. All strings are expected to
/// already be shell-safe (no unescaped quotes, no interior control
/// characters); the actor is responsible for building this from the
/// form modal's output.
#[derive(Debug, Clone, Default)]
pub struct CreateSpec {
    /// Full tmux session name, including any prefix like `bosun-` and
    /// a uniqueness suffix. This is the name tmux actually uses.
    pub name: String,
    /// Pretty name for the UI. If `Some`, bosun sets the per-session
    /// tmux user option `@bosun_display` to this value so the UI can
    /// show "rasterfox" even though the internal name is
    /// `bosun-rasterfox-a1b2c3d4`.
    pub display_name: Option<String>,
    /// Working directory for the new session. Must exist.
    pub path: String,
    /// Shell command to run as the initial process. Empty means use
    /// the user's default shell.
    pub command: String,
    /// Full session spec (agent, args, options) to persist as
    /// per-session `@bosun_*` tmux user options. Used by restart to
    /// recover the original spec. `None` skips persistence (useful
    /// for tests and for callers that don't care about restart).
    pub metadata: Option<SessionMetadata>,
    /// Extra environment for the session's shell, from the `[env]`
    /// table in `config.toml`. Each pair becomes a `-e KEY=VALUE` on
    /// `new-session`. `BOSUN=1` is always added on top, so callers
    /// don't pass it here.
    pub env: Vec<(String, String)>,
}

/// The subset of `SessionSpec` that bosun persists as tmux user
/// options on each managed session so that `RestartSession` can
/// rebuild the spec without an external store.
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub display_name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub claude_session_mode: String,
    pub claude_skip_permissions: bool,
    pub claude_remote_control: bool,
    pub codex_session_mode: String,
    pub codex_yolo: bool,
    pub kimi_session_mode: String,
    pub kimi_yolo: bool,
    pub opencode_session_mode: String,
    pub opencode_auto: bool,
    pub qwen_session_mode: String,
    pub qwen_yolo: bool,
    /// Sidebar container this session belongs to (tabs feature).
    /// `None` when this session is its own row.
    pub container_id: Option<String>,
    /// Git worktree path backing this session (worktree feature).
    /// `None` when the session isn't in a worktree.
    pub worktree_path: Option<String>,
    /// Branch checked out in the worktree. `None` for non-worktree sessions.
    pub branch: Option<String>,
}

/// Abstraction over the tmux CLI. Real impl shells out; mocks record calls.
#[async_trait]
pub trait TmuxClient: Send + Sync {
    /// Run `tmux list-sessions` and return parsed sessions. An empty
    /// server (exit code 1, "no server running") returns `Ok(vec![])`.
    async fn list_sessions(&self) -> Result<Vec<TmuxSession>>;

    /// Every session's status inputs (`list-sessions` with the slim
    /// [`STATUS_POLL_FORMAT`]), for the 250 ms status poll. The rows
    /// only carry what a hook-reported status depends on; see
    /// [`crate::tmux::parse::parse_status_rows`].
    async fn list_status_rows(&self) -> Result<Vec<TmuxSession>> {
        self.list_sessions().await
    }

    /// Capture the current visible pane (what the user actually sees
    /// right now — no scrollback history), preserving ANSI escape
    /// sequences so we can render them with `ansi-to-tui` and pass
    /// them to detectors. Dead sessions return `Ok(vec![])`.
    async fn capture_pane(&self, session: &str) -> Result<Vec<u8>>;

    /// Create a detached tmux session. The session appears in
    /// subsequent `list_sessions` calls. Returns the name of the
    /// newly-created session on success.
    async fn create_session(&self, spec: &CreateSpec) -> Result<String>;

    /// Kill a tmux session by its internal name. Missing sessions
    /// are treated as success (idempotent).
    async fn kill_session(&self, session: &str) -> Result<()>;

    /// Update the `@bosun_display` per-session user option so the UI
    /// picks up a new pretty label on the next refresh. Does not
    /// change the internal tmux session name.
    async fn set_display_name(&self, session: &str, display: &str) -> Result<()>;

    /// Point the `@bosun_container_id` user option at the sidebar row
    /// `container_id`, after a tab moved between rows. Reconcile puts
    /// a session the sidebar model doesn't hold into the row this
    /// names.
    async fn set_container_id(&self, session: &str, container_id: &str) -> Result<()>;

    /// Set `session`'s `@bosun_turn_at` to `at` (Unix ms), or unset it.
    async fn set_turn_start(&self, _session: &str, _at: Option<u64>) -> Result<()> {
        Ok(())
    }

    /// Read bosun's persisted `@bosun_*` metadata off a session, or
    /// `Ok(None)` if the session has no agent set (pre-dates the
    /// feature or wasn't created by bosun). Used by restart to
    /// rebuild the original spec.
    async fn get_session_metadata(&self, session: &str) -> Result<Option<SessionMetadata>>;

    /// Overwrite the `@bosun_*` metadata user options on a live
    /// session. Used by the modify-session modal to update the
    /// stored spec without recreating the session. The next
    /// `RestartSession` will read these back via
    /// `get_session_metadata` and spawn the agent with the new
    /// flags.
    async fn set_session_metadata(&self, session: &str, metadata: &SessionMetadata) -> Result<()>;

    /// Restart the agent inside a live session without killing the
    /// session itself. Sends Ctrl-C twice (covers agents that swallow
    /// the first interrupt to confirm), then types the new launch
    /// command and Enter. The pane stays alive (the shell keeps
    /// running underneath), the session's internal name doesn't
    /// change, and bosun's sidebar position is preserved with zero
    /// model churn.
    ///
    /// `prep_line` controls whether the C-u/Enter/C-u line-cleanup runs
    /// before typing. The issue-#2 deferral splits a restart into a bare
    /// stop (no command, `prep_line = false`) and a later launch
    /// (`prep_line = true`), so the cleanup — whose `Enter` re-runs the
    /// shell prompt's precmd hooks — happens once, at launch, not twice.
    ///
    /// `kill_first` controls whether Phase 1 sends the interrupting
    /// `C-c` to stop a running agent. Set it to `true` only when there
    /// really is a live agent to kill (the bare *stop* call). The
    /// deferred *launch* call (`LaunchAgent`) always types into a known
    /// bare shell — either freshly created or already stopped by the
    /// restart's stop-half — so it passes `false`: a fresh pane may
    /// still be sourcing a heavy `~/.zshrc`, and `pane_current_command`
    /// reads `zsh` throughout that (it *is* zsh running the rc file), so
    /// the shell-ready poll can't tell "at prompt" from "mid-init". A
    /// `C-c` fired in that window SIGINTs `.zshrc` partway through,
    /// leaving PATH half-built (any tool dir appended late in the rc —
    /// e.g. `~/.kimi-code/bin` on the last line — never gets added), so
    /// the agent binary then isn't found. Skipping the kill lets the rc
    /// finish; the atomic `cmd\r` in Phase 3 buffers and runs once the
    /// prompt is genuinely ready.
    async fn restart_in_place(
        &self,
        session: &str,
        command: &str,
        prep_line: bool,
        kill_first: bool,
    ) -> Result<()>;

    /// Resolve the git work-tree root for `path`. Errors if `path` is
    /// not inside a git repo.
    async fn repo_root(&self, path: &str) -> Result<String>;
    /// `git -C <repo> worktree add -b <branch> <worktree_path> HEAD`.
    async fn worktree_add(&self, repo: &str, branch: &str, worktree_path: &str) -> Result<()>;
    /// `git -C <repo> worktree remove [--force] <worktree_path>`.
    async fn worktree_remove(&self, repo: &str, worktree_path: &str, force: bool) -> Result<()>;
    /// True if `worktree_path` has uncommitted changes.
    async fn is_dirty(&self, worktree_path: &str) -> Result<bool>;
    /// `git -C <repo> merge <branch>` into the repo's current branch.
    async fn branch_merge(&self, repo: &str, branch: &str) -> Result<()>;
    /// `git -C <repo> merge --abort` — restore the pre-merge state after a
    /// failed/conflicted `branch_merge` so the repo isn't left half-merged.
    async fn merge_abort(&self, repo: &str) -> Result<()>;
    /// `git -C <repo> branch -d <branch>`.
    async fn branch_delete(&self, repo: &str, branch: &str) -> Result<()>;
    /// Given a path INSIDE a linked worktree, resolve the MAIN repo
    /// root (the directory the worktree branches from). Used on kill to
    /// find the repo for merge/remove/delete. See implementation note.
    async fn main_repo_root(&self, worktree_path: &str) -> Result<String>;
    /// Idempotently add `pattern` to `<repo>/.git/info/exclude` so a
    /// worktree placed inside the repo's working tree stays out of
    /// `git status`. Local-only (never committed); a no-op if already present.
    async fn ensure_excluded(&self, repo: &str, pattern: &str) -> Result<()>;

    /// Whether tmux commands are currently carried by the control-mode
    /// channel, so one costs a pipe write rather than a `tmux` exec.
    /// Pollers that would be too expensive over exec check this first.
    fn control_channel_up(&self) -> bool {
        false
    }

    /// Every attached terminal client's session and terminal type
    /// (`#{client_session}`, `#{client_termname}`), for deciding which
    /// sessions show tmux's status line (see
    /// [`crate::tmux::status_bar::wants_status_line`]). Control-mode
    /// clients are left out.
    async fn list_client_terms(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    /// Each session's `status` option as tmux resolves it: whether the
    /// session shows a status line.
    async fn session_status_lines(&self) -> Result<Vec<(String, bool)>> {
        Ok(Vec::new())
    }

    /// Turn a session's status line on or off (its `status` option).
    async fn set_session_status_line(&self, _session: &str, _on: bool) -> Result<()> {
        Ok(())
    }
}

/// Production implementation. tmux commands go over a long-lived
/// control-mode connection when it's up and fall back to one
/// `tokio::process::Command` exec each otherwise; see
/// [`crate::tmux::command_channel`]. Supports an optional `-L <socket>`
/// for test isolation. Clones share one command channel.
#[derive(Debug, Clone)]
pub struct TokioTmuxClient {
    socket: Option<String>,
    channel: Arc<CommandChannel>,
}

/// Output of one tmux command, whichever way it was carried. For the
/// control channel, `stderr` holds the lines of an `%error` block —
/// the same text exec prints on stderr.
#[derive(Debug, Clone, Default)]
pub struct TmuxOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Whether a command can be re-run over exec when the control
/// connection dies after the command was written but before its reply
/// arrived (tmux may already have run it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Reads and idempotent writes (`set-option`, `kill-session` of a
    /// possibly-missing session): run again over exec.
    Safe,
    /// `new-session`, `send-keys`: report the failure instead of
    /// risking a duplicate session or doubled keystrokes.
    Unsafe,
}

/// One step of [`TokioTmuxClient::copy_mode_search`]. Each carries the
/// search text, so a step can start the search again if the pane has
/// left copy mode since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStep {
    /// Search for the text as typed so far.
    Find(String),
    /// The next match towards older output.
    Older(String),
    /// The next match towards newer output.
    Newer(String),
    /// Leave copy mode: the pane is back at its live output.
    End,
}

/// How [`TokioTmuxClient::copy_mode_search`] marks matches: tmux
/// styles (`bg=#rrggbb,fg=#rrggbb`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHighlight {
    /// The match the pane is on.
    pub current: String,
    /// Every other match.
    pub other: String,
}

/// The regular expression that finds `text` as typed. tmux ignores case
/// when an expression has no capital letter: without `match_case` the
/// text is lowercased, and with it, text without a capital gets one that
/// matches nothing (`[A]{0}`).
pub fn search_pattern(text: &str, match_case: bool) -> String {
    let text = if match_case {
        text.to_string()
    } else {
        text.to_lowercase()
    };
    let mut pattern = String::with_capacity(text.len() + 8);
    if match_case
        && text.chars().any(|c| c.is_ascii_lowercase())
        && !text.chars().any(|c| c.is_ascii_uppercase())
    {
        pattern.push_str("[A]{0}");
    }
    for c in text.chars() {
        if "\\.[]()*+?{}^$|".contains(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

/// A search step left the cursor on the match it was already on: on the
/// same line, and no further than the match is wide, where the next match
/// on that line would be at least that far. `before` and `after` end in
/// `#{scroll_position}|#{copy_cursor_y}|#{copy_cursor_x}`. tmux 3.4, which
/// doesn't count matches, is checked by the line alone, as before.
fn search_stalled(before: &str, after: &str, text: &str, counted: bool) -> bool {
    fn cursor(state: &str) -> Option<(&str, &str, i64)> {
        let mut f = state.rsplitn(4, '|');
        let x = f.next()?.parse().ok()?;
        let y = f.next()?;
        let scroll = f.next()?;
        Some((scroll, y, x))
    }
    let (Some((bs, by, bx)), Some((as_, ay, ax))) = (cursor(before), cursor(after)) else {
        return false;
    };
    if (bs, by) != (as_, ay) {
        return false;
    }
    !counted || (ax - bx).unsigned_abs() <= text.chars().count() as u64
}

/// The pane's program takes the mouse on the alternate screen.
const WHEEL_TO_PROGRAM: &str = "#{&&:#{alternate_on},#{mouse_any_flag}}";

/// The command a `list-keys -T <table> <key>` line binds, to keep for
/// the history, with its `\;` separators as a plain `;` (it goes inside
/// `if-shell`). `None` when the key is unbound or already Bosun's.
fn wheel_command(line: &str, key: &str) -> Option<String> {
    let line = line.lines().next()?;
    let (_, command) = line.split_once(&format!(" {key} "))?;
    let command = command.trim();
    if command.is_empty() || command.contains(WHEEL_TO_PROGRAM) {
        return None;
    }
    Some(command.replace(" \\; ", " ; "))
}

/// Where a search stands after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchResult {
    /// The pane is on a match.
    pub found: bool,
    /// Matches in the whole history. `None` on a tmux too old to count
    /// them (3.4 has no `search_count`).
    pub count: Option<u64>,
    /// tmux stopped counting before the end of a long history: there
    /// are at least `count`.
    pub partial: bool,
}

impl SearchResult {
    /// `#{search_present}|#{search_count}|#{search_count_partial}`.
    fn parse(s: &str) -> Self {
        let mut f = s.split('|');
        let found = f.next() == Some("1");
        let count = f.next().and_then(|c| c.parse().ok()).filter(|_| found);
        let partial = found && f.next() == Some("1");
        Self {
            found,
            count,
            partial,
        }
    }
}

impl TokioTmuxClient {
    pub fn new() -> Self {
        Self::build(None)
    }

    #[allow(dead_code)]
    pub fn with_socket(socket: impl Into<String>) -> Self {
        Self::build(Some(socket.into()))
    }

    fn build(socket: Option<String>) -> Self {
        let channel = Arc::new(CommandChannel::new(
            socket.clone(),
            crate::tmux::command_channel::enabled_from_env(),
        ));
        Self { socket, channel }
    }

    /// Same socket, but every command execs (the pre-control-mode
    /// behavior). For comparisons and as an escape hatch.
    pub fn exec_only(mut self) -> Self {
        self.channel = Arc::new(CommandChannel::new(self.socket.clone(), false));
        self
    }

    /// The shared command channel, for stats and tests.
    pub fn command_channel(&self) -> &Arc<CommandChannel> {
        &self.channel
    }

    /// Build a tmux command with the configured socket prefix.
    pub(crate) fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        if let Some(sock) = &self.socket {
            c.arg("-L").arg(sock);
        }
        c.stdin(Stdio::null());
        c.kill_on_drop(true);
        c
    }

    /// Run one tmux command: over the control channel when it's up,
    /// otherwise (or after a lost connection, for [`Retry::Safe`]
    /// commands) by exec. Errors only when the command couldn't be run
    /// at all; a tmux-level failure comes back as `success == false`.
    /// `list-sessions -F <format>`, parsed by `parse`.
    async fn list_with(
        &self,
        format: &str,
        parse: fn(&str) -> Result<Vec<TmuxSession>>,
    ) -> Result<Vec<TmuxSession>> {
        let output = self
            .run(&["list-sessions", "-F", format], Retry::Safe)
            .await?;

        if output.success {
            let s = String::from_utf8_lossy(&output.stdout);
            return parse(&s);
        }

        // tmux exits non-zero when there are no sessions. The phrasing varies
        // by how we got there:
        //   * Attached but zero sessions: "no server running on /tmp/tmux-501/default"
        //   * Custom -L socket that was never created:
        //     "error connecting to /private/tmp/tmux-501/<name> (No such file or directory)"
        //   * Some versions: "no sessions"
        //   * Racing the shutdown that killing the *last* session
        //     triggers: "server exited unexpectedly". The server is on
        //     its way out precisely because nothing is left to list, so
        //     that means empty too, not an error worth showing the
        //     user. (Seen on Linux tmux; macOS usually finishes exiting
        //     before we ask and answers "no server running".)
        // All of them mean "empty" for our purposes.
        let stderr = &output.stderr;
        if is_empty_server_stderr(stderr) {
            return Ok(Vec::new());
        }

        Err(BosunError::Tmux(format!(
            "list-sessions failed: {}",
            stderr.trim()
        )))
    }

    pub async fn run(&self, args: &[&str], retry: Retry) -> Result<TmuxOutput> {
        match self.channel.execute(args).await {
            Outcome::Replied(block) => {
                let mut out = TmuxOutput {
                    success: block.ok,
                    ..Default::default()
                };
                if block.ok {
                    out.stdout = block.stdout();
                } else {
                    out.stderr = String::from_utf8_lossy(&block.stdout()).into_owned();
                }
                return Ok(out);
            }
            Outcome::NotSent => {}
            Outcome::Lost => {
                if retry == Retry::Unsafe {
                    return Err(BosunError::Tmux(format!(
                        "tmux connection lost while running {}",
                        args.first().copied().unwrap_or("command")
                    )));
                }
            }
        }
        self.channel.note_exec();
        let mut cmd = self.cmd();
        cmd.args(args);
        let output = cmd.output().await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BosunError::TmuxNotInstalled,
            _ => BosunError::Io(e),
        })?;
        Ok(TmuxOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// `refresh-client -t <tty> -f ignore-size` (or `!ignore-size`).
    /// Lets a hidden GUI view stop taking part in window-size
    /// negotiation without detaching. Only a flag, never a size.
    pub async fn set_client_ignore_size(&self, tty: &str, ignore: bool) -> Result<()> {
        check_client_tty(tty)?;
        let flag = if ignore {
            "ignore-size"
        } else {
            "!ignore-size"
        };
        let out = self
            .run(&["refresh-client", "-t", tty, "-f", flag], Retry::Safe)
            .await?;
        if out.success {
            Ok(())
        } else {
            Err(BosunError::Tmux(format!(
                "refresh-client -t {tty}: {}",
                out.stderr.trim()
            )))
        }
    }

    /// Make the client on `tty` tmux's "latest" client for its session,
    /// so under `window-size latest` the session takes that client's
    /// size. `refresh-client` can't do this; `switch-client` to the
    /// session the client is already on does, without changing its
    /// last-session. Side effects, same as a keypress in that client:
    /// the client redraws and the session's activity time is bumped.
    pub async fn make_client_latest(&self, tty: &str) -> Result<()> {
        check_client_tty(tty)?;
        // Find the client's session from `list-clients`. Not
        // `display-message -c <tty> '#{client_session}'`: over a
        // control connection the `client_*` formats resolve against the
        // control client itself (`__bosun_monitor`), and switching the
        // GUI's client there would be exactly wrong.
        let out = self
            .run(
                &["list-clients", "-F", "#{client_tty}|||#{client_session}"],
                Retry::Safe,
            )
            .await?;
        let listing = String::from_utf8_lossy(&out.stdout).into_owned();
        let session = listing
            .lines()
            .filter_map(|l| l.split_once("|||"))
            .find(|(t, _)| *t == tty)
            .map(|(_, s)| s.to_string())
            .filter(|s| !s.is_empty());
        let Some(session) = session else {
            return Err(BosunError::Tmux(format!("no tmux client on {tty}")));
        };
        let target = format!("={session}:");
        let out = self
            .run(&["switch-client", "-c", tty, "-t", &target], Retry::Safe)
            .await?;
        if out.success {
            Ok(())
        } else {
            Err(BosunError::Tmux(format!(
                "switch-client -c {tty}: {}",
                out.stderr.trim()
            )))
        }
    }

    /// One step of a search through a session's history, in tmux's copy
    /// mode, so it covers the whole scrollback and tmux highlights every
    /// match. Each new search starts from the bottom, towards older
    /// output. `highlight` sets how matches look, on a `Find`.
    pub async fn copy_mode_search(
        &self,
        session: &str,
        step: &SearchStep,
        match_case: bool,
        highlight: Option<&SearchHighlight>,
    ) -> Result<SearchResult> {
        let target = format!("={session}:");
        let mode = self
            .display(&target, "#{pane_in_mode}|#{pane_mode}")
            .await?;
        let in_copy_mode = mode == "1|copy-mode";
        let text = match step {
            SearchStep::Find(t) | SearchStep::Older(t) | SearchStep::Newer(t) => t.as_str(),
            SearchStep::End => "",
        };
        if text.is_empty() {
            if in_copy_mode {
                self.copy_mode_command(&target, &["cancel"]).await?;
            }
            return Ok(SearchResult::default());
        }
        // Not in copy mode any more (left with q, or from another
        // client): start the search again rather than step through one
        // that's gone.
        let find = !in_copy_mode || matches!(step, SearchStep::Find(_));
        if !in_copy_mode {
            self.wheel_reaches_mouse_programs().await;
            let out = self.run(&["copy-mode", "-t", &target], Retry::Safe).await?;
            if !out.success {
                return Err(BosunError::Tmux(format!(
                    "copy-mode {session}: {}",
                    out.stderr.trim()
                )));
            }
        }
        if let (true, Some(h)) = (find, highlight) {
            for (opt, style) in [
                ("copy-mode-current-match-style", &h.current),
                ("copy-mode-match-style", &h.other),
            ] {
                // Best-effort: tmux's own colours still mark the matches.
                if let Err(e) = self
                    .run(
                        &["set-option", "-w", "-t", &target, opt, style],
                        Retry::Safe,
                    )
                    .await
                {
                    tracing::warn!("set {opt} on {session}: {e}");
                }
            }
        }
        // The search's state, then where the cursor is (the last three
        // fields).
        const STATE: &str = "#{search_present}|#{search_count}|#{search_count_partial}|#{scroll_position}|#{copy_cursor_y}|#{copy_cursor_x}";
        if find {
            // A regular expression rather than tmux's plain-text search,
            // whose case rule can't be changed. Each keystroke searches
            // again from the bottom.
            let pattern = search_pattern(text, match_case);
            self.copy_mode_command(&target, &["history-bottom"]).await?;
            self.copy_mode_command(&target, &["end-of-line"]).await?;
            self.copy_mode_command(&target, &["search-backward", &pattern])
                .await?;
            return Ok(SearchResult::parse(&self.display(&target, STATE).await?));
        }
        let command = match step {
            SearchStep::Newer(_) => "search-reverse",
            _ => "search-again",
        };
        let before = self.display(&target, STATE).await?;
        self.copy_mode_command(&target, &[command]).await?;
        let mut state = self.display(&target, STATE).await?;
        // When the direction changes, tmux (3.4 and 3.8 alike) only moves
        // to the other end of the match it's already on: step once more.
        let result = SearchResult::parse(&state);
        if result.found && search_stalled(&before, &state, text, result.count.is_some()) {
            self.copy_mode_command(&target, &[command]).await?;
            state = self.display(&target, STATE).await?;
        }
        Ok(SearchResult::parse(&state))
    }

    /// The mouse wheel in copy mode: a program that takes the mouse on the
    /// alternate screen (Claude Code, an editor) scrolls itself, and tmux
    /// keeps no history for it, so there the wheel leaves copy mode and
    /// goes to the program, as it does outside copy mode. Anywhere else it
    /// scrolls through the history as before. tmux's own binding (which
    /// differs between versions, or the user's) is kept for that. Only on
    /// bosun's own server, and best-effort.
    async fn wheel_reaches_mouse_programs(&self) {
        if self.socket.is_none() {
            return;
        }
        for table in ["copy-mode", "copy-mode-vi"] {
            for key in ["WheelUpPane", "WheelDownPane"] {
                let Ok(out) = self
                    .run(&["list-keys", "-T", table, key], Retry::Safe)
                    .await
                else {
                    continue;
                };
                let line = String::from_utf8_lossy(&out.stdout);
                let Some(command) = wheel_command(&line, key) else {
                    continue;
                };
                let args = [
                    "bind-key",
                    "-T",
                    table,
                    key,
                    "if-shell",
                    "-F",
                    "-t",
                    "=",
                    WHEEL_TO_PROGRAM,
                    "send-keys -X cancel ; send-keys -M",
                    &command,
                ];
                match self.run(&args, Retry::Safe).await {
                    Ok(out) if !out.success => {
                        tracing::warn!("bind {table} {key}: {}", out.stderr.trim())
                    }
                    Err(e) => tracing::warn!("bind {table} {key}: {e}"),
                    _ => {}
                }
            }
        }
    }

    /// `display-message -p -t <target> <format>`, trimmed.
    async fn display(&self, target: &str, format: &str) -> Result<String> {
        let out = self
            .run(
                &["display-message", "-p", "-t", target, format],
                Retry::Safe,
            )
            .await?;
        if !out.success {
            return Err(BosunError::Tmux(format!(
                "display-message {target}: {}",
                out.stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `send-keys -t <target> -X <command…>`: a copy-mode command.
    async fn copy_mode_command(&self, target: &str, command: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", target, "-X"];
        args.extend_from_slice(command);
        let out = self.run(&args, Retry::Unsafe).await?;
        if out.success {
            Ok(())
        } else {
            Err(BosunError::Tmux(format!(
                "{}: {}",
                command[0],
                out.stderr.trim()
            )))
        }
    }

    /// Pull the socket flag for use by `attach.rs` when it needs to spawn
    /// its own non-`tokio` process (attach must be synchronous on the
    /// controlling tty).
    #[allow(dead_code)]
    pub fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    /// Read the basename of the pane's foreground process, e.g. `zsh`
    /// while sitting at a prompt or `node` / `claude` / `codex` /
    /// `python3` while an agent is running. Returns an empty string if
    /// the session has gone away. Used by `restart_in_place` to poll
    /// for "agent has died" and "agent has started" without relying on
    /// fixed-duration sleeps.
    async fn pane_current_command(&self, session: &str) -> String {
        let args = [
            "display-message",
            "-p",
            "-t",
            session,
            "#{pane_current_command}",
        ];
        match self.run(&args, Retry::Safe).await {
            Ok(out) if out.success => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            _ => String::new(),
        }
    }

    /// `send-keys -t <session> <keys…>`, logging (not returning) a
    /// failure — the restart path's fire-and-forget keystrokes.
    async fn send_keys_logged(&self, session: &str, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", session];
        args.extend_from_slice(keys);
        if let Err(e) = self.run(&args, Retry::Unsafe).await {
            tracing::warn!("send-keys to {}: {}", session, e);
        }
    }
}

/// A client target for `refresh-client -t` / `switch-client -c` must
/// be a tty path (what libghostty's `ghostty_surface_tty_name` and
/// tmux's `#{client_tty}` report).
fn check_client_tty(tty: &str) -> Result<()> {
    if tty.starts_with("/dev/") && tty.len() > 5 {
        Ok(())
    } else {
        Err(BosunError::Tmux(format!(
            "client tty must be a /dev path, got {tty:?}"
        )))
    }
}

/// Heuristic: is this pane's foreground process a shell prompt, i.e.
/// safe to type a launch command into? Matches the common login
/// shells. False negatives just mean `restart_in_place` waits a bit
/// longer for shell detection before falling through on timeout.
fn is_shell(cmd: &str) -> bool {
    matches!(
        cmd,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "pwsh"
    )
}

impl Default for TokioTmuxClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TmuxClient for TokioTmuxClient {
    fn control_channel_up(&self) -> bool {
        self.channel.is_up()
    }

    async fn list_sessions(&self) -> Result<Vec<TmuxSession>> {
        self.list_with(LIST_SESSIONS_FORMAT, parse_list_sessions)
            .await
    }

    async fn list_status_rows(&self) -> Result<Vec<TmuxSession>> {
        self.list_with(STATUS_POLL_FORMAT, parse_status_rows).await
    }

    async fn capture_pane(&self, session: &str) -> Result<Vec<u8>> {
        // -p : stdout
        // -e : include escape sequences
        // -J : join wrapped lines (so we don't split in the middle of an
        //      ANSI sequence)
        // No -S/-E flags: we want just the currently visible pane — no
        // scrollback history. Scrollback would pick up whatever the user
        // typed earlier (e.g. literal `printf '\033[32m...'` source),
        // which looks like escape code garbage in the preview.
        let output = self
            .run(
                &["capture-pane", "-p", "-e", "-J", "-t", session],
                Retry::Safe,
            )
            .await?;

        if output.success {
            return Ok(output.stdout);
        }

        // Session may have just been killed — treat as empty capture.
        let stderr = &output.stderr;
        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return Ok(Vec::new());
        }
        Err(BosunError::Tmux(format!(
            "capture-pane {} failed: {}",
            session,
            stderr.trim()
        )))
    }

    async fn create_session(&self, spec: &CreateSpec) -> Result<String> {
        // Create the session with NO initial command. This starts the
        // user's default login shell, which sources their rc files
        // (zshrc / bashrc) and sets up the environment the way manual
        // `tmux new` + typing the command would. Running the command
        // directly via `new-session -d -s name command` would skip
        // shell init entirely, and agents like Claude rely on that
        // init for things like PATH and (historically) env vars.
        //
        // We deliberately do NOT pass bosun's own environment through
        // with `-e` — it inflates the command to dozens of args and
        // didn't resolve the Claude auth issue in testing. Claude reads
        // its credentials from a file or the macOS Keychain, not from
        // env. What we do set is the user's `[env]` table plus the
        // `BOSUN=1` marker, so a shell or wrapper can tell it's running
        // inside bosun without parsing `$TMUX` for a socket name that
        // varies per install (issue #17). The marker goes last so an
        // `[env]` entry can't override it. `new-session -e` needs
        // tmux 3.2+. Restart-in-place reuses this shell, so the
        // variables carry over without being set again.
        let env_pairs: Vec<String> = spec
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let mut args: Vec<&str> = vec!["new-session", "-d", "-s", &spec.name];
        if !spec.path.is_empty() {
            args.push("-c");
            args.push(&spec.path);
        }
        for pair in &env_pairs {
            args.push("-e");
            args.push(pair);
        }
        args.push("-e");
        args.push("BOSUN=1");
        let output = self.run(&args, Retry::Unsafe).await?;
        if !output.success {
            return Err(BosunError::Tmux(format!(
                "new-session -s {} failed: {}",
                spec.name,
                output.stderr.trim()
            )));
        }

        // Step 1b: opt bosun's dedicated tmux server into the csi-u
        // extended-keys format. Kimi Code warns on startup unless the
        // server uses csi-u (the default is `xterm`), and modern agents
        // generally encode modified keys better with it. These are
        // *global* (`-g`) server options, but they only touch the
        // `-L bosun` socket (`self.run` carries it), so the user's
        // default tmux is untouched, and they're harmless for
        // claude/codex. Best-effort — a failure just means the warning
        // may reappear. Idempotent, so re-running per create is fine.
        for (opt, val) in [("extended-keys", "on"), ("extended-keys-format", "csi-u")] {
            if let Err(e) = self.run(&["set-option", "-g", opt, val], Retry::Safe).await {
                tracing::warn!("set -g {} {}: {}", opt, val, e);
            }
        }
        // Passthrough, so a program in a pane can reach the real
        // terminal: kitty graphics (inline images in Bosun for Mac,
        // Ghostty, kitty) need it. Only on bosun's own server: with
        // `tmux_socket = "default"` the server is the user's, and
        // their passthrough setting is theirs to choose.
        if self.socket.is_some() {
            if let Err(e) = self
                .run(
                    &["set-option", "-g", "allow-passthrough", "on"],
                    Retry::Safe,
                )
                .await
            {
                tracing::warn!("set -g allow-passthrough on: {}", e);
            }
        }

        // Step 2: set the pretty display name on the freshly-created
        // session via a per-session user option. Best-effort — if
        // this fails, the UI falls back to the internal name.
        if let Some(display) = &spec.display_name {
            let args = ["set-option", "-t", &spec.name, "@bosun_display", display];
            if let Err(e) = self.run(&args, Retry::Safe).await {
                tracing::warn!("set @bosun_display on {}: {}", spec.name, e);
            }
        }

        // Step 2b: persist the full session metadata as @bosun_*
        // user options so RestartSession can recover the spec later.
        // Best-effort; failures just mean restart won't work for
        // this session.
        if let Some(meta) = &spec.metadata {
            for (key, value) in metadata_options(meta) {
                let args = ["set-option", "-t", &spec.name, key, &value];
                if let Err(e) = self.run(&args, Retry::Safe).await {
                    tracing::warn!("set {} on {}: {}", key, spec.name, e);
                }
            }
        }

        // Step 3: type the agent command via send-keys so it runs
        // inside the user's shell with their full environment set up.
        //
        // We match agent-deck's idiom here:
        //   * `send-keys -l -- <cmd>` for the literal characters, so
        //     tmux doesn't interpret things like `C-c` or `Space` in
        //     the command as key-name shortcuts.
        //   * A brief sleep (100ms) so tmux's bracketed-paste handler
        //     finishes processing the literal chunk before Enter lands.
        //   * A separate `send-keys Enter` to submit. Sending Enter in
        //     the same call as `-l` would make it a literal "Enter"
        //     string instead of a newline.
        if !spec.command.is_empty() {
            let args = ["send-keys", "-l", "-t", &spec.name, "--", &spec.command];
            if let Err(e) = self.run(&args, Retry::Unsafe).await {
                tracing::warn!("send-keys -l to {}: {}", spec.name, e);
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            if let Err(e) = self
                .run(&["send-keys", "-t", &spec.name, "Enter"], Retry::Unsafe)
                .await
            {
                tracing::warn!("send-keys Enter to {}: {}", spec.name, e);
            }
        }

        Ok(spec.name.clone())
    }

    async fn kill_session(&self, session: &str) -> Result<()> {
        let output = self
            .run(&["kill-session", "-t", session], Retry::Safe)
            .await?;
        if output.success {
            return Ok(());
        }
        // If the session is already gone, treat as idempotent success.
        let stderr = &output.stderr;
        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return Ok(());
        }
        Err(BosunError::Tmux(format!(
            "kill-session {} failed: {}",
            session,
            stderr.trim()
        )))
    }

    async fn set_display_name(&self, session: &str, display: &str) -> Result<()> {
        let output = self
            .run(
                &["set-option", "-t", session, "@bosun_display", display],
                Retry::Safe,
            )
            .await?;
        if output.success {
            return Ok(());
        }
        Err(BosunError::Tmux(format!(
            "set @bosun_display on {}: {}",
            session,
            output.stderr.trim()
        )))
    }

    async fn set_container_id(&self, session: &str, container_id: &str) -> Result<()> {
        let output = self
            .run(
                &[
                    "set-option",
                    "-t",
                    session,
                    "@bosun_container_id",
                    container_id,
                ],
                Retry::Safe,
            )
            .await?;
        if output.success {
            return Ok(());
        }
        Err(BosunError::Tmux(format!(
            "set @bosun_container_id on {}: {}",
            session,
            output.stderr.trim()
        )))
    }

    async fn set_turn_start(&self, session: &str, at: Option<u64>) -> Result<()> {
        let at = at.map(|t| t.to_string());
        let args: Vec<&str> = match &at {
            Some(t) => vec!["set-option", "-t", session, "@bosun_turn_at", t],
            None => vec!["set-option", "-u", "-t", session, "@bosun_turn_at"],
        };
        let output = self.run(&args, Retry::Safe).await?;
        if output.success {
            return Ok(());
        }
        Err(BosunError::Tmux(format!(
            "set @bosun_turn_at on {}: {}",
            session,
            output.stderr.trim()
        )))
    }

    async fn get_session_metadata(&self, session: &str) -> Result<Option<SessionMetadata>> {
        // Single display-message call returns all metadata fields
        // separated by `|||`. We can't use a control character (the old `\x1f`
        // unit separator) because tmux 3.4+ escapes control chars in
        // format output as octal sequences (`\037`), which the parser
        // would never see as a real separator — that was breaking the
        // Ubuntu CI lifecycle integration test. `|||` is printable so
        // tmux passes it through untouched. See the matching fix in
        // `tmux::parse::LIST_SESSIONS_FORMAT`.
        const SEP: &str = "|||";
        let fmt = format!(
            "#{{@bosun_display}}{SEP}#{{@bosun_path}}{SEP}#{{@bosun_agent}}{SEP}#{{@bosun_args}}{SEP}#{{@bosun_claude_session_mode}}{SEP}#{{@bosun_claude_skip_permissions}}{SEP}#{{@bosun_codex_yolo}}{SEP}#{{@bosun_container_id}}{SEP}#{{@bosun_worktree_path}}{SEP}#{{@bosun_branch}}{SEP}#{{@bosun_kimi_session_mode}}{SEP}#{{@bosun_kimi_yolo}}{SEP}#{{@bosun_codex_session_mode}}{SEP}#{{@bosun_opencode_session_mode}}{SEP}#{{@bosun_opencode_auto}}{SEP}#{{@bosun_qwen_session_mode}}{SEP}#{{@bosun_qwen_yolo}}{SEP}#{{@bosun_claude_remote_control}}",
            SEP = SEP
        );
        let output = self
            .run(&["display-message", "-p", "-t", session, &fmt], Retry::Safe)
            .await?;
        if !output.success {
            return Err(BosunError::Tmux(format!(
                "display-message on {}: {}",
                session,
                output.stderr.trim()
            )));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        let line = raw.trim_end_matches('\n');
        Ok(parse_metadata_line(line, SEP))
    }

    async fn set_session_metadata(&self, session: &str, metadata: &SessionMetadata) -> Result<()> {
        // Re-uses the same key/value mapping the create path writes
        // on session birth, so a modify produces options
        // byte-identical to what the create path would have
        // produced for the same spec. Errors on the first failed
        // option write so the caller can surface a single message
        // — partial-update state is rare enough (it would mean
        // tmux died mid-call) that we'd rather fail loudly.
        for (key, value) in metadata_options(metadata) {
            let output = self
                .run(&["set-option", "-t", session, key, &value], Retry::Safe)
                .await?;
            if !output.success {
                return Err(BosunError::Tmux(format!(
                    "set {} on {}: {}",
                    key,
                    session,
                    output.stderr.trim()
                )));
            }
        }
        Ok(())
    }

    async fn restart_in_place(
        &self,
        session: &str,
        command: &str,
        prep_line: bool,
        kill_first: bool,
    ) -> Result<()> {
        // Strategy: poll `#{pane_current_command}` instead of guessing
        // timings with fixed sleeps. The two questions we need answered
        // are "has the old agent actually exited?" and "has the new
        // agent actually started?" — both are directly observable via
        // tmux's display-message format, so we wait for the actual
        // state transition rather than hoping a sleep was long enough.
        //
        //   1. Send C-c, poll until pane_current_command is a shell.
        //      Re-send C-c periodically while the agent is still up
        //      (claude / codex sometimes swallow the first one to ask
        //      for confirmation, etc.). Bounded by a hard timeout so
        //      we never wedge the actor.
        //   2. Once we observe a shell, prep the line: Enter (forces
        //      any async prompt framework to finish painting), C-u
        //      (wipe residue from the shutdown banner).
        //   3. send-keys -l <command> + Enter to launch the new agent.
        //   4. Poll again until pane_current_command leaves the shell
        //      — i.e. the agent process is actually the foreground
        //      process — and re-send if a slow shell dropped it.
        //
        // Nothing else is typed. An earlier version sent a trailing C-l
        // to force a repaint for capture-pane, from before launches
        // waited for an attached client (issue #2). It landed before
        // the agent had switched its terminal to raw mode, so the tty
        // echoed a literal `^L` into the agent's prompt; the agent
        // paints its own screen on start, so the C-l bought nothing.
        use std::time::Duration;
        use tokio::time::Instant;

        // ── Phase 1: kill the running agent, wait for shell ──────────
        // Only send the interrupting C-c when there's actually a live
        // agent to stop (`kill_first`). The deferred launch path types
        // into a known bare shell that may still be sourcing a heavy
        // `~/.zshrc`; a C-c there SIGINTs the rc mid-init and leaves PATH
        // half-built, so late-appended tool dirs (e.g. `~/.kimi-code/bin`)
        // go missing and the agent binary isn't found. See the trait doc.
        if kill_first {
            self.send_keys_logged(session, &["C-c"]).await;
        }
        let kill_deadline = Instant::now() + Duration::from_millis(3500);
        let mut next_cc = Instant::now() + Duration::from_millis(250);
        let mut at_shell = false;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let cur = self.pane_current_command(session).await;
            if cur.is_empty() {
                // Session went away — nothing to restart.
                return Ok(());
            }
            if is_shell(&cur) {
                at_shell = true;
                break;
            }
            if Instant::now() >= kill_deadline {
                tracing::warn!(
                    "restart_in_place: gave up waiting for shell on {} (still running {})",
                    session,
                    cur
                );
                break;
            }
            if kill_first && Instant::now() >= next_cc {
                self.send_keys_logged(session, &["C-c"]).await;
                next_cc = Instant::now() + Duration::from_millis(400);
            }
        }

        // Tiny settle so the shell's line editor is fully primed before
        // we type. Even after pane_current_command flips to "zsh",
        // async prompt frameworks (powerlevel10k, spaceship) may still
        // be painting; ~100ms is enough in practice.
        tokio::time::sleep(Duration::from_millis(120)).await;

        // ── Phase 2: prep the shell line for input ───────────────────
        // Clear any residue on the input line (C-u) and settle, then
        // type. We deliberately do NOT press Enter here: an empty Enter
        // at the shell re-runs the prompt's precmd hooks (e.g. a
        // `git status` baked into the prompt), which the user sees as a
        // spurious newline + `git status` before every relaunch. C-u
        // alone is a no-op on an already-empty line, so it's safe; the
        // settle gives an async prompt framework (powerlevel10k,
        // spaceship) time to finish painting before we send the command.
        //
        // Skipped entirely when `prep_line` is false — the issue-#2
        // deferral's bare *stop* call only kills the agent and must not
        // touch the line at all (the matching launch call preps).
        if prep_line {
            self.send_keys_logged(session, &["C-u"]).await;
            tokio::time::sleep(Duration::from_millis(160)).await;
        }

        // Empty command means "leave the shell as-is" — either a bare
        // stop (prep_line = false) or a terminal agent with no args.
        // Nothing else to type.
        if command.is_empty() {
            return Ok(());
        }

        // ── Phase 3 + 4: submit the launch command atomically, confirm
        // the agent started, and re-send if it didn't ───────────────
        //
        // A freshly-created pane may still be sourcing a heavy ~/.zshrc
        // (antidote, compinit, prompt frameworks, ZLE plugins) when we
        // type — worst right after a reboot, when shell caches are cold
        // so the first shell of the session is slowest to come up. The
        // original failure mode: we typed the command and then sent Enter
        // as a *separate* keystroke; if the command text was swallowed by
        // the still-initialising shell but the Enter still landed, it hit
        // an *empty* line — and with an oh-my-zsh `magic-enter`-style
        // binding on Return, an empty-line Enter runs a default command
        // (`ls` / `git status`) instead of the agent, so the pane filled
        // with a directory listing and nothing launched. A live restart
        // never hit this (its shell is long since initialised), which is
        // why it only showed on fresh creates / dead-session recreates.
        //
        // Fix: send the command AND its Enter as one atomic literal — a
        // trailing carriage return in the same `send-keys -l` chunk. The
        // two can't desync, so a magic-enter Return can never see an empty
        // line: the whole `"cmd\r"` is either buffered together (and runs
        // when the shell becomes ready) or dropped together (no stray
        // command). We then verify the agent process actually replaced the
        // shell and re-send if a slow/lossy shell dropped the first one.
        // Bounded so a genuinely broken command (bad PATH, etc.) can't loop
        // forever.
        const LAUNCH_ATTEMPTS: usize = 3;
        // Command followed by a literal carriage return, sent as one
        // `-l` chunk so the submit can't be separated from the text.
        let atomic = format!("{command}\r");
        let mut agent_up = false;
        for attempt in 0..LAUNCH_ATTEMPTS {
            if attempt > 0 {
                tracing::warn!(
                    "restart_in_place: agent didn't start on {}; re-sending launch (attempt {}/{})",
                    session,
                    attempt + 1,
                    LAUNCH_ATTEMPTS
                );
                // Clear any half-entered residue before re-sending.
                self.send_keys_logged(session, &["C-u"]).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }

            let args = ["send-keys", "-l", "-t", session, "--", &atomic];
            if let Err(e) = self.run(&args, Retry::Unsafe).await {
                tracing::warn!("restart_in_place send-keys -l to {}: {}", session, e);
            }

            // Confirm the agent actually started (its process replaces the
            // shell in pane_current_command). A real launch execs within a
            // second or two even on a cold machine, so a generous window
            // reliably separates "still initialising / buffered" from
            // "genuinely dropped".
            let start_deadline = Instant::now() + Duration::from_millis(8000);
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let cur = self.pane_current_command(session).await;
                if cur.is_empty() {
                    // Session vanished.
                    return Ok(());
                }
                if !is_shell(&cur) {
                    agent_up = true;
                    break;
                }
                if Instant::now() >= start_deadline {
                    break;
                }
            }
            if agent_up {
                break;
            }
        }
        if !agent_up {
            tracing::warn!(
                "restart_in_place: agent never appeared on {} after {} attempt(s)",
                session,
                LAUNCH_ATTEMPTS
            );
        }
        let _ = at_shell;

        Ok(())
    }

    async fn repo_root(&self, path: &str) -> Result<String> {
        let out = run_git(&["-C", path, "rev-parse", "--show-toplevel"]).await?;
        Ok(out.trim().to_string())
    }

    async fn worktree_add(&self, repo: &str, branch: &str, worktree_path: &str) -> Result<()> {
        run_git(&[
            "-C",
            repo,
            "worktree",
            "add",
            "-b",
            branch,
            worktree_path,
            "HEAD",
        ])
        .await?;
        Ok(())
    }

    async fn worktree_remove(&self, repo: &str, worktree_path: &str, force: bool) -> Result<()> {
        // `--` terminates option parsing so a path that happens to start
        // with `-` is treated as an operand, not a flag.
        let mut args = vec!["-C", repo, "worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push("--");
        args.push(worktree_path);
        run_git(&args).await?;
        Ok(())
    }

    async fn is_dirty(&self, worktree_path: &str) -> Result<bool> {
        let out = run_git(&["-C", worktree_path, "status", "--porcelain"]).await?;
        Ok(!out.trim().is_empty())
    }

    async fn branch_merge(&self, repo: &str, branch: &str) -> Result<()> {
        // `--` so a branch name starting with `-` can't be read as a flag.
        run_git(&["-C", repo, "merge", "--", branch]).await?;
        Ok(())
    }

    async fn merge_abort(&self, repo: &str) -> Result<()> {
        run_git(&["-C", repo, "merge", "--abort"]).await?;
        Ok(())
    }

    async fn branch_delete(&self, repo: &str, branch: &str) -> Result<()> {
        // `--` so a branch name starting with `-` can't be read as a flag.
        run_git(&["-C", repo, "branch", "-d", "--", branch]).await?;
        Ok(())
    }

    async fn main_repo_root(&self, worktree_path: &str) -> Result<String> {
        // From INSIDE a linked worktree, `rev-parse --show-toplevel`
        // returns the *worktree* path, not the main repo — so we can't
        // use it directly. `--git-common-dir` points at the main repo's
        // git dir (shared across all worktrees); `--path-format=absolute`
        // is required because older git returns it relative otherwise.
        let common = run_git(&[
            "-C",
            worktree_path,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .await?;
        let common = common.trim();
        // The git dir's parent is the main repo's work tree. Take the
        // parent via Path::parent() rather than string-stripping a
        // literal `/.git`, which would break on non-standard git-dir
        // names or bare repos.
        let parent = std::path::Path::new(common).parent().ok_or_else(|| {
            BosunError::Git(format!("git-common-dir {common} has no parent directory"))
        })?;
        let parent = parent.to_str().ok_or_else(|| {
            BosunError::Git(format!(
                "git-common-dir parent {parent:?} is not valid UTF-8"
            ))
        })?;
        // Confirm by resolving the toplevel from the parent directory.
        let root = run_git(&["-C", parent, "rev-parse", "--show-toplevel"]).await?;
        Ok(root.trim().to_string())
    }

    async fn ensure_excluded(&self, repo: &str, pattern: &str) -> Result<()> {
        // Resolve the repo's git dir rather than assuming `<repo>/.git` is a
        // directory — it's a file for linked worktrees and submodules. The
        // common dir is shared across worktrees, which is exactly where a
        // repo-wide `info/exclude` belongs.
        let git_dir = run_git(&[
            "-C",
            repo,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .await?;
        let git_dir = git_dir.trim();
        let info = std::path::Path::new(git_dir).join("info");
        let exclude = info.join("exclude");

        // Already excluded? Match the pattern on its own line so we don't
        // re-append (and don't match it as a substring of another rule).
        if let Ok(existing) = std::fs::read_to_string(&exclude) {
            if existing.lines().any(|l| l.trim() == pattern) {
                return Ok(());
            }
        }

        std::fs::create_dir_all(&info)
            .map_err(|e| BosunError::Git(format!("create {}: {e}", info.display())))?;
        // Read-modify-append, ensuring the new rule lands on its own line
        // even if the file didn't end in a newline.
        let mut contents = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(pattern);
        contents.push('\n');
        std::fs::write(&exclude, contents)
            .map_err(|e| BosunError::Git(format!("write {}: {e}", exclude.display())))?;
        Ok(())
    }

    async fn list_client_terms(&self) -> Result<Vec<(String, String)>> {
        const SEP: &str = "|||";
        let fmt =
            format!("#{{client_control_mode}}{SEP}#{{client_session}}{SEP}#{{client_termname}}");
        let out = self.run(&["list-clients", "-F", &fmt], Retry::Safe).await?;
        if !out.success {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut parts = l.splitn(3, SEP);
                let control = parts.next()?;
                let session = parts.next()?;
                let term = parts.next()?;
                (control != "1" && !session.is_empty())
                    .then(|| (session.to_string(), term.to_string()))
            })
            .collect())
    }

    async fn session_status_lines(&self) -> Result<Vec<(String, bool)>> {
        const SEP: &str = "|||";
        let fmt = format!("#{{session_name}}{SEP}#{{status}}");
        let out = self
            .run(&["list-sessions", "-F", &fmt], Retry::Safe)
            .await?;
        if !out.success {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.rsplit_once(SEP))
            .map(|(name, status)| (name.to_string(), status != "off"))
            .collect())
    }

    async fn set_session_status_line(&self, session: &str, on: bool) -> Result<()> {
        let value = if on { "on" } else { "off" };
        let out = self
            .run(&["set-option", "-t", session, "status", value], Retry::Safe)
            .await?;
        if out.success {
            Ok(())
        } else {
            Err(BosunError::Tmux(format!(
                "set status on {session}: {}",
                out.stderr.trim()
            )))
        }
    }
}

/// Map a `SessionMetadata` into the `(key, value)` pairs that should
/// be written via `set-option -t <session>`.
fn metadata_options(m: &SessionMetadata) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("@bosun_path", m.path.clone()),
        ("@bosun_agent", m.agent.clone()),
        ("@bosun_args", m.args.clone()),
        ("@bosun_claude_session_mode", m.claude_session_mode.clone()),
        (
            "@bosun_claude_skip_permissions",
            if m.claude_skip_permissions { "1" } else { "0" }.to_string(),
        ),
        (
            "@bosun_claude_remote_control",
            if m.claude_remote_control { "1" } else { "0" }.to_string(),
        ),
        (
            "@bosun_codex_yolo",
            if m.codex_yolo { "1" } else { "0" }.to_string(),
        ),
        ("@bosun_kimi_session_mode", m.kimi_session_mode.clone()),
        (
            "@bosun_kimi_yolo",
            if m.kimi_yolo { "1" } else { "0" }.to_string(),
        ),
        ("@bosun_codex_session_mode", m.codex_session_mode.clone()),
        (
            "@bosun_opencode_session_mode",
            m.opencode_session_mode.clone(),
        ),
        (
            "@bosun_opencode_auto",
            if m.opencode_auto { "1" } else { "0" }.to_string(),
        ),
        ("@bosun_qwen_session_mode", m.qwen_session_mode.clone()),
        (
            "@bosun_qwen_yolo",
            if m.qwen_yolo { "1" } else { "0" }.to_string(),
        ),
    ];
    // Only emit `@bosun_container_id` when a container assignment
    // is requested — leaves pre-feature sessions clean and avoids
    // writing a `None` sentinel value that we'd then have to
    // distinguish from "no option" on reads.
    if let Some(id) = &m.container_id {
        out.push(("@bosun_container_id", id.clone()));
    }
    // Same "only when Some" treatment for the worktree options — keeps
    // non-worktree sessions clean and lets reads distinguish "no option"
    // from an empty value.
    if let Some(p) = &m.worktree_path {
        out.push(("@bosun_worktree_path", p.clone()));
    }
    if let Some(b) = &m.branch {
        out.push(("@bosun_branch", b.clone()));
    }
    out
}

/// Parse a `display-message` metadata line (fields separated by `sep`)
/// into a `SessionMetadata`, or `None` when the line doesn't come from a
/// metadata-aware bosun session.
///
/// Field order mirrors the read format string in `get_session_metadata`:
/// `display | path | agent | args | claude_session_mode |
/// claude_skip_permissions | codex_yolo | container_id | worktree_path |
/// branch | kimi_session_mode | kimi_yolo | codex_session_mode |
/// opencode_session_mode | opencode_auto | qwen_session_mode | qwen_yolo |
/// claude_remote_control`.
fn parse_metadata_line(line: &str, sep: &str) -> Option<SessionMetadata> {
    let parts: Vec<&str> = line.split(sep).collect();
    // Accept every historical field count: 7 (pre-container_id), 8
    // (container_id added), 9/10 (worktree_path + branch), 11/12
    // (kimi_session_mode + kimi_yolo), 13..=17 (codex_session_mode,
    // opencode + qwen fields), and 18 (claude_remote_control) — keeps
    // sessions created by an older bosun usable after upgrade.
    // Widening this matters: after appending fields to the read
    // format, metadata-aware sessions emit the full count, so a
    // narrower guard would reject every session and silently disable
    // restart/modify.
    if !matches!(parts.len(), 7..=18) {
        return None;
    }
    // Agent is the required anchor — if it's empty, this session
    // wasn't created by a metadata-aware bosun.
    if parts[2].is_empty() {
        return None;
    }
    let container_id = parts
        .get(7)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    Some(SessionMetadata {
        display_name: parts[0].to_string(),
        path: parts[1].to_string(),
        agent: parts[2].to_string(),
        args: parts[3].to_string(),
        claude_session_mode: if parts[4].is_empty() {
            "New".to_string()
        } else {
            parts[4].to_string()
        },
        claude_skip_permissions: parts[5] == "1",
        codex_yolo: parts[6] == "1",
        container_id,
        worktree_path: parts
            .get(8)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        branch: parts
            .get(9)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        kimi_session_mode: match parts.get(10) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => "New".to_string(),
        },
        kimi_yolo: parts.get(11) == Some(&"1"),
        codex_session_mode: match parts.get(12) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => "New".to_string(),
        },
        opencode_session_mode: match parts.get(13) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => "New".to_string(),
        },
        opencode_auto: parts.get(14) == Some(&"1"),
        qwen_session_mode: match parts.get(15) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => "New".to_string(),
        },
        qwen_yolo: parts.get(16) == Some(&"1"),
        claude_remote_control: parts.get(17) == Some(&"1"),
    })
}

/// Shell out to `git` with the given args, returning raw (untrimmed)
/// stdout as a `String` on success — callers trim as needed. Mirrors
/// the tmux shell-out idiom:
/// build the command, collect output, and map a non-zero exit to a
/// `BosunError::Git` carrying the trimmed stderr. A missing `git`
/// binary maps to the same error variant so the caller gets a single,
/// user-facing message rather than a raw io panic.
async fn run_git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| BosunError::Git(format!("failed to spawn git: {e}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(BosunError::Git(format!(
        "git {} failed ({}): {}",
        args.join(" "),
        output.status,
        stderr.trim()
    )))
}

/// Build a synchronous `std::process::Command` for tmux with the given args.
/// Used by `attach.rs` and other places that need blocking semantics.
#[allow(dead_code)]
pub(crate) fn sync_tmux<I, S>(socket: Option<&str>, args: I) -> std::process::Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = std::process::Command::new("tmux");
    if let Some(sock) = socket {
        c.arg("-L").arg(sock);
    }
    for a in args {
        c.arg(a);
    }
    c
}

/// True when tmux's stderr means "there is nothing to list" rather than
/// a real failure. tmux exits non-zero for an empty or absent server and
/// phrases it several ways depending on version, platform, and whether
/// we caught the server mid-shutdown right after its last session was
/// killed.
fn is_empty_server_stderr(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("no sessions")
        || stderr.contains("server exited unexpectedly")
        || (stderr.contains("error connecting") && stderr.contains("No such file or directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_server_stderr_covers_every_phrasing() {
        for e in [
            "no server running on /tmp/tmux-501/default",
            "no sessions",
            // Emitted when list-sessions races the server shutdown that
            // killing the last session triggers.
            "server exited unexpectedly",
            "error connecting to /private/tmp/tmux-501/x (No such file or directory)",
        ] {
            assert!(is_empty_server_stderr(e), "should read as empty: {e}");
        }
        for e in ["can't find session: foo", "usage: list-sessions"] {
            assert!(!is_empty_server_stderr(e), "should be a real error: {e}");
        }
    }

    #[test]
    fn search_result_reads_tmux_formats() {
        let r = |found, count, partial| SearchResult {
            found,
            count,
            partial,
        };
        assert_eq!(SearchResult::parse("1|29|0"), r(true, Some(29), false));
        assert_eq!(SearchResult::parse("1|1000|1"), r(true, Some(1000), true));
        // tmux 3.4: no count.
        assert_eq!(SearchResult::parse("1||"), r(true, None, false));
        assert_eq!(SearchResult::parse("0||"), r(false, None, false));
        assert_eq!(SearchResult::parse(""), r(false, None, false));
    }

    #[test]
    fn search_stalled_tells_the_same_match_from_the_next() {
        // tmux 3.8, "row 29": changing direction from the start of the
        // match only reaches its end, and back.
        assert!(search_stalled(
            "1|11|0|0|24|0",
            "1|11|0|0|24|6",
            "row 29",
            true
        ));
        assert!(search_stalled(
            "1|11|0|0|25|6",
            "1|11|0|0|25|0",
            "row 29",
            true
        ));
        // A real step: another line, or scrolled, or a second match
        // further along the same line.
        assert!(!search_stalled(
            "1|11|0|0|25|0",
            "1|11|0|0|24|0",
            "row 29",
            true
        ));
        assert!(!search_stalled(
            "1|11|0|0|0|0",
            "1|11|0|3|0|0",
            "row 29",
            true
        ));
        assert!(!search_stalled(
            "1|2|0|0|5|0",
            "1|2|0|0|5|12",
            "row 29",
            true
        ));
        // tmux 3.4: the line alone.
        assert!(search_stalled("1|||0|5|0", "1|||0|5|12", "row 29", false));
        assert!(!search_stalled("1|||0|5|0", "1|||0|4|0", "row 29", false));
        assert!(!search_stalled("", "1|11|0|0|24|6", "row 29", true));
    }

    #[test]
    fn wheel_command_keeps_the_history_binding() {
        // tmux 3.4 (and 3.8 without a config).
        assert_eq!(
            wheel_command(
                "bind-key -T copy-mode WheelUpPane       select-pane \\; send-keys -X -N 5 scroll-up\n",
                "WheelUpPane"
            )
            .as_deref(),
            Some("select-pane ; send-keys -X -N 5 scroll-up")
        );
        assert_eq!(
            wheel_command(
                "bind-key  -T copy-mode-vi WheelDownPane     send-keys -X scroll-down",
                "WheelDownPane"
            )
            .as_deref(),
            Some("send-keys -X scroll-down")
        );
        // Unbound, or Bosun's already.
        assert_eq!(wheel_command("", "WheelUpPane"), None);
        assert_eq!(
            wheel_command(
                "bind-key -T copy-mode WheelUpPane if-shell -F -t = \"#{&&:#{alternate_on},#{mouse_any_flag}}\" \"send-keys -X cancel ; send-keys -M\" \"send-keys -X scroll-up\"",
                "WheelUpPane"
            ),
            None
        );
    }

    #[test]
    fn search_pattern_sets_the_case_rule() {
        // Case ignored: lowercase, which tmux searches without case.
        assert_eq!(search_pattern("TUI", false), "tui");
        assert_eq!(search_pattern("tui", false), "tui");
        // Case matched: a capital that matches nothing, unless the text
        // has its own, or no letters at all.
        assert_eq!(search_pattern("tui", true), "[A]{0}tui");
        assert_eq!(search_pattern("TUI", true), "TUI");
        assert_eq!(search_pattern("12:30", true), "12:30");
        // The text is literal.
        assert_eq!(search_pattern("row.7 (a|b)*", false), r"row\.7 \(a\|b\)\*");
        assert_eq!(
            search_pattern(r"C:\x [1]{2}^$+?", false),
            r"c:\\x \[1\]\{2\}\^\$\+\?"
        );
    }

    const SEP: &str = "|||";

    #[test]
    fn parse_metadata_full_10_field_line_round_trips() {
        // display|path|agent|args|mode|skip|yolo|container|worktree|branch
        let line = "My Session|||/tmp/my|||claude|||--model=opus|||Resume|||1|||0|||cont1|||/srv/.worktrees/feat|||feat";
        let m = parse_metadata_line(line, SEP).expect("metadata parses");
        assert_eq!(m.display_name, "My Session");
        assert_eq!(m.path, "/tmp/my");
        assert_eq!(m.agent, "claude");
        assert_eq!(m.args, "--model=opus");
        assert_eq!(m.claude_session_mode, "Resume");
        assert!(m.claude_skip_permissions);
        assert!(!m.codex_yolo);
        assert_eq!(m.container_id.as_deref(), Some("cont1"));
        assert_eq!(m.worktree_path.as_deref(), Some("/srv/.worktrees/feat"));
        assert_eq!(m.branch.as_deref(), Some("feat"));
    }

    #[test]
    fn parse_metadata_full_12_field_line_round_trips_kimi() {
        // display|path|agent|args|mode|skip|yolo|container|worktree|branch|kimi_mode|kimi_yolo
        let line = [
            "Moon", "/tmp/m", "kimi", "-m k2.5", "New", "0", "0", "", "", "", "Continue", "1",
        ]
        .join(SEP);
        let m = parse_metadata_line(&line, SEP).expect("kimi metadata parses");
        assert_eq!(m.agent, "kimi");
        assert_eq!(m.kimi_session_mode, "Continue");
        assert!(m.kimi_yolo);
    }

    #[test]
    fn parse_metadata_full_17_field_line_round_trips_new_agents() {
        // display|path|agent|args|mode|skip|yolo|container|worktree|branch|
        // kimi_mode|kimi_yolo|codex_mode|opencode_mode|opencode_auto|
        // qwen_mode|qwen_yolo
        let line = [
            "Multi", "/tmp/x", "opencode", "", "New", "0", "0", "", "", "", "New", "0", "Resume",
            "Continue", "1", "Resume", "1",
        ]
        .join(SEP);
        let m = parse_metadata_line(&line, SEP).expect("17-field metadata parses");
        assert_eq!(m.agent, "opencode");
        assert_eq!(m.codex_session_mode, "Resume");
        assert_eq!(m.opencode_session_mode, "Continue");
        assert!(m.opencode_auto);
        assert_eq!(m.qwen_session_mode, "Resume");
        assert!(m.qwen_yolo);
        // A session persisted before claude_remote_control existed
        // has no 18th field — it defaults to off.
        assert!(!m.claude_remote_control);
    }

    #[test]
    fn parse_metadata_full_18_field_line_round_trips_remote_control() {
        // The 17 fields above, then claude_remote_control.
        let line = [
            "Remote", "/tmp/r", "claude", "", "New", "0", "0", "", "", "", "New", "0", "New",
            "New", "0", "New", "0", "1",
        ]
        .join(SEP);
        let m = parse_metadata_line(&line, SEP).expect("18-field metadata parses");
        assert_eq!(m.agent, "claude");
        assert!(m.claude_remote_control);
        assert!(!m.claude_skip_permissions);
    }

    #[test]
    fn parse_metadata_12_field_line_defaults_new_agent_fields() {
        // A session persisted by a pre-opencode/qwen bosun emits 12
        // fields — the trailing agent fields default to New / false.
        let line = [
            "Moon", "/tmp/m", "kimi", "", "New", "0", "0", "", "", "", "Continue", "1",
        ]
        .join(SEP);
        let m = parse_metadata_line(&line, SEP).expect("12-field metadata parses");
        assert_eq!(m.codex_session_mode, "New");
        assert_eq!(m.opencode_session_mode, "New");
        assert!(!m.opencode_auto);
        assert_eq!(m.qwen_session_mode, "New");
        assert!(!m.qwen_yolo);
    }

    #[test]
    fn parse_metadata_pre_kimi_line_defaults_kimi_fields() {
        // A session persisted before the kimi columns existed reads
        // through the new 12-field format as a 10-field line → the two
        // trailing kimi fields are absent and default to New / false.
        let line = "My Session|||/tmp/my|||claude|||--model=opus|||Resume|||1|||0|||cont1|||/srv/.worktrees/feat|||feat";
        let m = parse_metadata_line(line, SEP).expect("metadata parses");
        assert_eq!(m.kimi_session_mode, "New");
        assert!(!m.kimi_yolo);
    }

    #[test]
    fn parse_metadata_legacy_8_field_line_still_parses() {
        // Pre-worktree session: container_id present, no worktree fields.
        let line = "Old|||/tmp/old|||codex|||args|||New|||0|||1|||cont2";
        let m = parse_metadata_line(line, SEP).expect("legacy metadata parses");
        assert_eq!(m.agent, "codex");
        assert_eq!(m.container_id.as_deref(), Some("cont2"));
        assert!(m.worktree_path.is_none());
        assert!(m.branch.is_none());
    }

    #[test]
    fn parse_metadata_empty_worktree_fields_are_none() {
        // Metadata-aware session with container_id and the two new
        // options unset → trailing empty fields parse to None. Built
        // by joining so the field count is unambiguous (10 fields).
        let line = ["S", "/p", "claude", "a", "New", "0", "0", "", "", ""].join(SEP);
        let m = parse_metadata_line(&line, SEP).expect("parses");
        assert!(m.worktree_path.is_none());
        assert!(m.branch.is_none());
    }

    #[test]
    fn parse_metadata_none_when_agent_empty() {
        let line = "S|||/p||||||a|||New|||0|||0|||c|||/wt|||b";
        assert!(parse_metadata_line(line, SEP).is_none());
    }

    #[test]
    fn parse_metadata_none_on_wrong_field_count() {
        let line = "too|||few";
        assert!(parse_metadata_line(line, SEP).is_none());
    }
}

/// Git shell-out tests. These spawn a real `git` in a tempdir — no
/// tmux, no network — so they run unconditionally (not gated behind
/// `tmux-it`). They exercise the actual git behaviour these methods
/// depend on: worktree/branch bookkeeping and the linked-worktree
/// `--git-common-dir` resolution that `main_repo_root` relies on.
#[cfg(test)]
mod git_tests {
    use super::*;

    /// Spawn `git -C <dir> <args>`, asserting the command succeeds.
    ///
    /// Signing is forced off for these throwaway repos. A developer
    /// with `commit.gpgsign = true` globally otherwise drags gpg-agent
    /// into every fixture commit, and the suite fails non-
    /// deterministically ("gpg: signing failed") on whichever test
    /// happens to run when the agent is unhappy. Nothing here is about
    /// signatures.
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgsign=false")
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
    async fn worktree_add_creates_dir_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "t@t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);

        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();
        assert!(wt.join(".git").exists());
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "feat"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("feat"));
    }

    #[tokio::test]
    async fn repo_root_resolves_toplevel() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let sub = repo.join("sub");
        std::fs::create_dir(&sub).unwrap();

        let client = TokioTmuxClient::new();
        // From a subdirectory, repo_root should still resolve to the repo top.
        let root = client.repo_root(sub.to_str().unwrap()).await.unwrap();
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );
    }

    #[tokio::test]
    async fn repo_root_errors_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        // dir.path() itself is not a git repo.
        let client = TokioTmuxClient::new();
        assert!(client
            .repo_root(dir.path().to_str().unwrap())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn worktree_remove_deletes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();
        assert!(wt.exists());

        client
            .worktree_remove(repo.to_str().unwrap(), wt.to_str().unwrap(), false)
            .await
            .unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn worktree_remove_force_removes_dirty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();
        // Dirty the worktree: a plain remove would refuse.
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

        // Non-force remove should fail on a dirty worktree.
        assert!(client
            .worktree_remove(repo.to_str().unwrap(), wt.to_str().unwrap(), false)
            .await
            .is_err());
        // Force remove should succeed.
        client
            .worktree_remove(repo.to_str().unwrap(), wt.to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn is_dirty_reflects_working_tree_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // Freshly added worktree is clean.
        assert!(!client.is_dirty(wt.to_str().unwrap()).await.unwrap());
        // Add an untracked file → dirty.
        std::fs::write(wt.join("new.txt"), "hi").unwrap();
        assert!(client.is_dirty(wt.to_str().unwrap()).await.unwrap());
    }

    #[tokio::test]
    async fn branch_merge_brings_in_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // Commit something on the worktree's `feat` branch.
        std::fs::write(wt.join("f.txt"), "data").unwrap();
        run_git(&wt, &["add", "f.txt"]);
        run_git(&wt, &["commit", "-q", "-m", "add f"]);

        // Merge feat into the main repo's checked-out branch.
        client
            .branch_merge(repo.to_str().unwrap(), "feat")
            .await
            .unwrap();
        assert!(repo.join("f.txt").exists());
    }

    #[tokio::test]
    async fn merge_abort_restores_pre_merge_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        // feat branches from the empty init commit, before either side
        // touches x.txt → the merge below conflicts on that path.
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        std::fs::write(repo.join("x.txt"), "main-side\n").unwrap();
        run_git(&repo, &["add", "x.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "main x"]);
        std::fs::write(wt.join("x.txt"), "feat-side\n").unwrap();
        run_git(&wt, &["add", "x.txt"]);
        run_git(&wt, &["commit", "-q", "-m", "feat x"]);

        // The merge conflicts and leaves MERGE_HEAD behind.
        assert!(client
            .branch_merge(repo.to_str().unwrap(), "feat")
            .await
            .is_err());
        assert!(repo.join(".git").join("MERGE_HEAD").exists());

        // Abort clears MERGE_HEAD and restores a clean working tree.
        client.merge_abort(repo.to_str().unwrap()).await.unwrap();
        assert!(!repo.join(".git").join("MERGE_HEAD").exists());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).trim().is_empty());
    }

    #[tokio::test]
    async fn branch_delete_removes_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();
        // Merge feat (no new commits) so `branch -d` sees it as merged,
        // then remove the worktree so the branch is free to delete.
        client
            .branch_merge(repo.to_str().unwrap(), "feat")
            .await
            .unwrap();
        client
            .worktree_remove(repo.to_str().unwrap(), wt.to_str().unwrap(), false)
            .await
            .unwrap();

        client
            .branch_delete(repo.to_str().unwrap(), "feat")
            .await
            .unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "feat"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    }

    #[tokio::test]
    async fn ensure_excluded_appends_once_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let client = TokioTmuxClient::new();

        // First call adds the rule; a nested worktree dir is then untracked-
        // ignored so it won't show in `git status`.
        client
            .ensure_excluded(repo.to_str().unwrap(), "/.worktrees/")
            .await
            .unwrap();
        let exclude = repo.join(".git").join("info").join("exclude");
        let after_first = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(
            after_first
                .lines()
                .filter(|l| l.trim() == "/.worktrees/")
                .count(),
            1,
            "rule should be present exactly once"
        );

        // Second call is a no-op — no duplicate line.
        client
            .ensure_excluded(repo.to_str().unwrap(), "/.worktrees/")
            .await
            .unwrap();
        let after_second = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(
            after_second
                .lines()
                .filter(|l| l.trim() == "/.worktrees/")
                .count(),
            1,
            "second call must not duplicate the rule"
        );

        // Prove the effect: a `.worktrees/` dir stays out of git status.
        std::fs::create_dir(repo.join(".worktrees")).unwrap();
        std::fs::write(repo.join(".worktrees").join("x"), "y").unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&status.stdout).contains(".worktrees"),
            "excluded worktree dir must not appear in git status"
        );
    }

    #[tokio::test]
    async fn main_repo_root_resolves_from_inside_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let wt = dir.path().join("wt");
        let client = TokioTmuxClient::new();
        client
            .worktree_add(repo.to_str().unwrap(), "feat", wt.to_str().unwrap())
            .await
            .unwrap();

        // From INSIDE the linked worktree, main_repo_root must resolve to
        // the MAIN repo root, not the worktree path itself.
        let main = client.main_repo_root(wt.to_str().unwrap()).await.unwrap();
        assert_eq!(
            std::fs::canonicalize(&main).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );
        // Sanity: it must NOT be the worktree path.
        assert_ne!(
            std::fs::canonicalize(&main).unwrap(),
            std::fs::canonicalize(&wt).unwrap()
        );
    }
}
