//! Modal form for creating a new bosun-managed tmux session.
//!
//! Fields: name (auto-prefixed with bosun-), working directory, agent
//! (dropdown), extra args. Tab/Shift-Tab move between fields, Enter
//! submits, Esc cancels. The modal emits a `Command::CreateSession`
//! on submit and lets the tmux actor handle the actual `tmux
//! new-session` invocation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::{
    ClaudeOptions, ClaudeSessionMode, CodexOptions, Command, KimiOptions, OpencodeOptions,
    QwenOptions, SessionSpec, SpecOptions, WorktreeSpec,
};
use crate::store::Recent;
use crate::ui::Theme;

use super::recents::RecentsModal;
use super::{center_rect, Modal, ModalData, ModalResult};

const MODAL_WIDTH: u16 = 64;

/// Maximum filesystem entries to read for completion. Keeps
/// `read_dir` bounded in large directories.
const PATH_SUGGESTION_CAP: usize = 50;

/// Maximum visible rows in the path dropdown overlay.
const DROPDOWN_MAX_VISIBLE: usize = 8;

// --- Agent dropdown --------------------------------------------------

pub use crate::config::AGENTS;

/// Index of `agent` in the selector, falling back to the first entry
/// for anything unrecognised. Config validation already rejects bad
/// names, so the fallback only covers direct callers.
fn agent_index(agent: &str) -> usize {
    AGENTS.iter().position(|a| *a == agent).unwrap_or(0)
}

// --- Modal state -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Path,
    Worktree,
    Branch,
    Agent,
    Args,
    // Claude-only
    ClaudeSession,
    ClaudeSkipPerm,
    // Codex-only
    CodexSession,
    CodexYolo,
    // Kimi-only
    KimiSession,
    KimiYolo,
    // OpenCode-only
    OpencodeSession,
    OpencodeAuto,
    // Qwen-only
    QwenSession,
    QwenYolo,
}

impl Field {
    /// Ordered list of fields the user can tab between for the
    /// currently-selected agent. `lock_path` drops `Field::Path`
    /// from the list — used in add-tab mode where the path is
    /// inherited from the container and isn't editable. `worktree`
    /// reveals `Field::Branch` after the worktree checkbox. Add-tab
    /// mode (`lock_path`) never shows the worktree fields: a tab
    /// inherits its container's path, so worktree is mutually
    /// exclusive with add-tab.
    fn visible_for(agent: &str, lock_path: bool, worktree: bool) -> Vec<Field> {
        let mut v = vec![Field::Name];
        if !lock_path {
            v.push(Field::Path);
            v.push(Field::Worktree);
            if worktree {
                v.push(Field::Branch);
            }
        }
        v.push(Field::Agent);
        v.push(Field::Args);
        match agent {
            "claude" => {
                v.push(Field::ClaudeSession);
                v.push(Field::ClaudeSkipPerm);
            }
            "codex" => {
                v.push(Field::CodexSession);
                v.push(Field::CodexYolo);
            }
            "kimi" => {
                v.push(Field::KimiSession);
                v.push(Field::KimiYolo);
            }
            "opencode" => {
                v.push(Field::OpencodeSession);
                v.push(Field::OpencodeAuto);
            }
            "qwen" => {
                v.push(Field::QwenSession);
                v.push(Field::QwenYolo);
            }
            _ => {}
        }
        v
    }
}

pub struct NewSessionModal {
    name: String,
    path: String,
    /// When true, the actor creates the session inside a fresh git
    /// worktree (see `WorktreeSpec`) instead of using `path`
    /// directly. Reveals the branch field.
    worktree: bool,
    /// Branch name for the worktree. Empty until the user edits it,
    /// in which case `branch_edited` sticks and this overrides the
    /// name-derived slug.
    branch: String,
    /// True once the user manually edits `branch`. After that the
    /// branch no longer tracks the session name slug.
    branch_edited: bool,
    agent_idx: usize,
    args: String,
    claude: ClaudeOptions,
    codex: CodexOptions,
    kimi: KimiOptions,
    opencode: OpencodeOptions,
    qwen: QwenOptions,
    field: Field,
    error: Option<String>,
    /// Recents cached at modal construction time, used when the user
    /// hits Ctrl+R to open the RecentsModal. Fresh on every new
    /// modal open.
    recents: Vec<Recent>,
    /// Index into `path_suggestions()` when the user has arrowed
    /// down into the filesystem dropdown. `None` means the user is
    /// typing freely (no dropdown entry highlighted).
    path_suggestion_idx: Option<usize>,
    /// First visible row in the scrollable path dropdown.
    path_suggestion_scroll: usize,
    /// Whether the path dropdown overlay is showing. Dismissed by
    /// Escape; re-activated by typing, backspace, or arrow-down.
    path_dropdown_active: bool,
    /// Internal session name this modal is editing in modify mode.
    /// `None` is the standard "create new session" flow that emits
    /// `Command::CreateSession`. `Some(internal)` switches the
    /// submit path to `Command::ModifySession` against that
    /// session — pre-filled from its stored metadata. The internal
    /// name is never user-editable, so we stash it once at
    /// construction and read it back on submit.
    modify_for: Option<String>,
    /// Container ID this modal is adding a tab to. `None` is the
    /// standard "create a fresh sidebar container" flow.
    /// `Some(container_id)` locks the path field to the container's
    /// path (read-only) and stamps the id onto the emitted
    /// `SessionSpec.container_id` so the new tmux session joins
    /// that container as another tab. Tab mode is mutually
    /// exclusive with modify mode.
    add_tab_to: Option<String>,
    /// Where a created worktree lands, snapshotted from
    /// `Config::worktree_location`. Drives the read-only preview line
    /// under the branch field so the shown scheme matches what the
    /// actor actually does downstream (`resolve_worktree_path`). Only
    /// meaningful when the worktree UI is shown (never in add-tab or
    /// modify mode).
    worktree_location: crate::config::WorktreeLocation,
}

/// One row in the filesystem dropdown. `name` is the last path
/// segment; `is_dir` drives trailing-slash decoration and Enter's
/// "dive in vs commit" behavior.
#[derive(Debug, Clone)]
struct PathEntry {
    name: String,
    is_dir: bool,
}

impl NewSessionModal {
    pub fn new(recents: Vec<Recent>, worktree_location: crate::config::WorktreeLocation) -> Self {
        Self::with_default_agent(recents, worktree_location, crate::config::DEFAULT_AGENT)
    }

    pub fn with_default_agent(
        recents: Vec<Recent>,
        worktree_location: crate::config::WorktreeLocation,
        default_agent: &str,
    ) -> Self {
        // Default the path to the most-recently-used session's path so
        // the modal "remembers" where you last worked across restarts.
        // Falls back to cwd (and then to ~) when there are no recents.
        let path = recents
            .first()
            .map(|r| r.path.clone())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| "~".to_string());
        let mut modal = Self {
            name: String::new(),
            path,
            worktree: false,
            branch: String::new(),
            branch_edited: false,
            agent_idx: agent_index(default_agent),
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            field: Field::Name,
            error: None,
            recents,
            path_suggestion_idx: None,
            path_suggestion_scroll: 0,
            path_dropdown_active: true,
            modify_for: None,
            add_tab_to: None,
            worktree_location,
        };
        modal.apply_remembered_options();
        modal
    }

    /// Construct the modal in modify mode: pre-fill every field
    /// from `spec` and remember `internal` so submit emits
    /// `Command::ModifySession` against the right tmux session
    /// instead of a fresh `CreateSession`. Recents are still
    /// passed in for Ctrl+R access — modifying lets the user pull
    /// flags from a past session just like creating does.
    pub fn for_modify(internal: String, spec: SessionSpec, recents: Vec<Recent>) -> Self {
        let mut modal = Self {
            name: String::new(),
            path: String::new(),
            worktree: false,
            branch: String::new(),
            branch_edited: false,
            agent_idx: 0,
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            field: Field::Name,
            error: None,
            recents,
            path_suggestion_idx: None,
            path_suggestion_scroll: 0,
            path_dropdown_active: false,
            modify_for: Some(internal),
            add_tab_to: None,
            // Modify mode never re-creates the worktree, so the value
            // is irrelevant here — the worktree UI is not shown.
            worktree_location: crate::config::WorktreeLocation::Subdir,
        };
        modal.fill_from_spec(spec);
        modal
    }

    /// Construct the modal in add-tab mode: pre-fill the path from
    /// the container (rendered read-only — all tabs share one
    /// path) and remember `container_id` so submit stamps it onto
    /// the new session's `@bosun_container_id` and reconcile routes
    /// the new tmux session as a tab on the container instead of a
    /// fresh sidebar row. The `name` field starts empty so the user
    /// types a fresh tab label — the container's existing internal
    /// tmux name is not a useful seed.
    pub fn for_add_tab(
        container_id: String,
        container_path: String,
        recents: Vec<Recent>,
        default_agent: &str,
    ) -> Self {
        let mut modal = Self {
            name: String::new(),
            path: container_path,
            worktree: false,
            branch: String::new(),
            branch_edited: false,
            agent_idx: agent_index(default_agent),
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            field: Field::Name,
            error: None,
            recents,
            path_suggestion_idx: None,
            path_suggestion_scroll: 0,
            path_dropdown_active: false,
            modify_for: None,
            add_tab_to: Some(container_id),
            // Add-tab mode hides the worktree UI (path is locked to
            // the container), so the value is never read.
            worktree_location: crate::config::WorktreeLocation::Subdir,
        };
        modal.apply_remembered_options();
        modal
    }

    /// True when this modal is adding a tab to an existing
    /// container (path is locked, no path dropdown, submit emits
    /// `CreateSession` with the container_id stamped on the spec).
    pub fn is_add_tab(&self) -> bool {
        self.add_tab_to.is_some()
    }

    /// Filesystem entries that match the current `self.path`.
    /// Reads the directory portion of the typed path and filters by
    /// the trailing segment. Capped at `PATH_SUGGESTION_CAP` for UI.
    fn path_suggestions(&self) -> Vec<PathEntry> {
        read_dir_filtered(&self.path, PATH_SUGGESTION_CAP)
    }

    /// Uncapped list of filesystem matches. Used by Tab's longest-
    /// common-prefix completion so we don't miss matches beyond the
    /// display window.
    fn path_suggestions_all(&self) -> Vec<PathEntry> {
        read_dir_filtered(&self.path, usize::MAX)
    }

    /// Commit a filesystem entry into `self.path`. Directories get a
    /// trailing slash so the dropdown refreshes with their contents
    /// on the next render.
    fn commit_path_entry(&mut self, entry: &PathEntry) {
        let (dir, _prefix) = split_path(&self.path);
        let mut new_path = format!("{}{}", dir, entry.name);
        if entry.is_dir {
            new_path.push('/');
        }
        self.path = new_path;
        self.reset_path_dropdown();
    }

    fn reset_path_dropdown(&mut self) {
        self.path_suggestion_idx = None;
        self.path_suggestion_scroll = 0;
        self.path_dropdown_active = true;
    }

    /// Keep the selected suggestion within the visible dropdown window.
    fn clamp_dropdown_scroll(&mut self, count: usize) {
        if let Some(idx) = self.path_suggestion_idx {
            let max_vis = DROPDOWN_MAX_VISIBLE.min(count);
            if idx < self.path_suggestion_scroll {
                self.path_suggestion_scroll = idx;
            } else if max_vis > 0 && idx >= self.path_suggestion_scroll + max_vis {
                self.path_suggestion_scroll = idx + 1 - max_vis;
            }
        } else {
            self.path_suggestion_scroll = 0;
        }
    }

    /// Shell-style Tab completion. Returns true if the path was
    /// extended (caller should stay on the Path field); false means
    /// "nothing to do, advance to next field".
    fn tab_complete_path(&mut self) -> bool {
        if !self.path_dropdown_active {
            return false;
        }
        let suggestions = self.path_suggestions_all();
        if suggestions.is_empty() {
            return false;
        }
        let (dir, prefix) = split_path(&self.path);

        // One match: commit it outright (with trailing slash for
        // dirs so the user can dive further).
        if suggestions.len() == 1 {
            self.commit_path_entry(&suggestions[0]);
            return true;
        }

        // Many matches: extend to the longest common prefix.
        // If the prefix is already at the LCP, stay on the field so
        // the user can arrow through the visible dropdown.
        let names: Vec<&str> = suggestions.iter().map(|e| e.name.as_str()).collect();
        let lcp = longest_common_prefix(&names);
        if lcp.chars().count() > prefix.chars().count() {
            self.path = format!("{}{}", dir, lcp);
            self.reset_path_dropdown();
        }
        true
    }

    /// Overwrite all form fields from a selected recent. Called by
    /// `on_child_closed` when the RecentsModal returns a
    /// `FillSessionSpec`.
    fn fill_from_spec(&mut self, spec: SessionSpec) {
        self.name = spec.name;
        self.path = spec.path;
        self.args = spec.args;
        self.claude = spec.options.claude;
        self.codex = spec.options.codex;
        self.kimi = spec.options.kimi;
        self.opencode = spec.options.opencode;
        self.qwen = spec.options.qwen;
        if let Some(idx) = AGENTS.iter().position(|a| *a == spec.agent) {
            self.agent_idx = idx;
        }
        self.error = None;
        self.field = Field::Name;
    }

    fn agent(&self) -> &'static str {
        AGENTS[self.agent_idx]
    }

    /// The branch name that will be used for the worktree: the
    /// manually-edited `branch` once the user has touched it,
    /// otherwise a slug derived live from the session name.
    fn branch_effective(&self) -> String {
        if self.branch_edited {
            self.branch.clone()
        } else {
            slug(&self.name)
        }
    }

    /// Schematic, read-only preview of where the worktree lands,
    /// honoring `worktree_location`. The real repo root is resolved
    /// downstream (`resolve_worktree_path`), not here, so we use the
    /// last segment of `self.path` as the repo-name stand-in and keep
    /// the string short enough to fit the modal width. The SCHEME
    /// shown must match the actor's, so a user on `sibling` sees the
    /// sibling form instead of a misleading `.worktrees/` path.
    fn worktree_preview(&self) -> String {
        use crate::config::WorktreeLocation::*;
        let branch = self.branch_effective();
        match self.worktree_location {
            Subdir => format!(".worktrees/{}", branch),
            Sibling => {
                let repo = self
                    .path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("repo");
                format!("{}-{}", repo, branch)
            }
        }
    }

    fn next_field(&mut self) {
        let visible = Field::visible_for(self.agent(), self.is_add_tab(), self.worktree);
        let idx = visible.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = visible[(idx + 1) % visible.len()];
        if self.field == Field::Path {
            self.path_dropdown_active = true;
        }
    }

    fn prev_field(&mut self) {
        let visible = Field::visible_for(self.agent(), self.is_add_tab(), self.worktree);
        let idx = visible.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = visible[(idx + visible.len() - 1) % visible.len()];
        if self.field == Field::Path {
            self.path_dropdown_active = true;
        }
    }

    /// When the agent changes, snap the focused field to something
    /// that actually exists in the new agent's option set. This only
    /// matters if the user is mid-navigation on an agent-specific
    /// field when the agent changes, which currently can't happen
    /// (agent can only change while on Field::Agent) — but the clamp
    /// is cheap and keeps the invariant obvious.
    fn clamp_field_for_agent(&mut self) {
        let visible = Field::visible_for(self.agent(), self.is_add_tab(), self.worktree);
        if !visible.contains(&self.field) {
            self.field = Field::Agent;
        }
    }

    /// Pre-fill agent-specific options from the most recently used
    /// session of the same agent type. Recents are ordered by
    /// `last_used_at DESC`, so the first match is the freshest.
    fn apply_remembered_options(&mut self) {
        match self.agent() {
            "claude" => {
                if let Some(r) = self.recents.iter().find(|r| r.agent == "claude") {
                    self.claude = r.claude.clone();
                }
            }
            "codex" => {
                if let Some(r) = self.recents.iter().find(|r| r.agent == "codex") {
                    self.codex = r.codex.clone();
                }
            }
            "kimi" => {
                if let Some(r) = self.recents.iter().find(|r| r.agent == "kimi") {
                    self.kimi = r.kimi.clone();
                }
            }
            "opencode" => {
                if let Some(r) = self.recents.iter().find(|r| r.agent == "opencode") {
                    self.opencode = r.opencode.clone();
                }
            }
            "qwen" => {
                if let Some(r) = self.recents.iter().find(|r| r.agent == "qwen") {
                    self.qwen = r.qwen.clone();
                }
            }
            _ => {}
        }
    }

    /// Combined handler when the agent dropdown changes: clamp the
    /// focused field and restore the last-used options for the new
    /// agent.
    fn on_agent_changed(&mut self) {
        self.clamp_field_for_agent();
        self.apply_remembered_options();
    }

    /// Compute the modal height based on the current agent and state.
    /// Path suggestions are rendered as a floating overlay and do not
    /// affect the modal height.
    fn modal_height(&self) -> u16 {
        // Base: title, blank, name label+input, blank, path label+input,
        //       blank, agent label+line, blank, args label+input = 13
        let mut h: u16 = 13;

        // Worktree checkbox (blank + checkbox line) — shown in every
        // mode except add-tab, which locks the path to its container.
        // The branch label + input + preview lines appear only when
        // the worktree toggle is on.
        if !self.is_add_tab() {
            h += 2; // blank + checkbox
            if self.worktree {
                h += 3; // branch label + branch input + preview line
            }
        }

        // Agent-specific options.
        match self.agent() {
            "claude" => h += 4,   // blank + header + radio + checkbox
            "codex" => h += 4,    // blank + header + radio + checkbox
            "kimi" => h += 4,     // blank + header + radio + checkbox
            "opencode" => h += 4, // blank + header + radio + checkbox
            "qwen" => h += 4,     // blank + header + radio + checkbox
            _ => {}
        }

        // Validation error.
        if self.error.is_some() {
            h += 2; // blank + error line
        }

        // Padding: 1 top + 1 bottom from inner rect inset.
        h + 2
    }

    fn build_spec(&self) -> Result<SessionSpec, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name is required".into());
        }
        // Don't allow the user to type `bosun-foo` — we prepend the
        // prefix in the actor based on Config. Strip it here if they
        // typed it, so the stored form is always the bare name.
        let name = name.strip_prefix("bosun-").unwrap_or(name);
        // The internal tmux session name is slugified from this, so
        // we need at least one alphanumeric character to work with.
        if !name.chars().any(|c| c.is_alphanumeric()) {
            return Err("name must contain at least one letter or digit".into());
        }

        let path = self.path.trim();
        if path.is_empty() {
            return Err("path is required".into());
        }

        // When creating in a worktree, validate the branch. A slash
        // breaks the sibling worktree-path scheme downstream
        // (`<repo>-feat/foo` makes a nested `repo-feat/` dir), so it's
        // rejected. The auto-slug never contains `/`, so this only
        // guards a manual edit.
        let worktree = if self.worktree {
            let branch = self.branch_effective();
            if !branch.chars().any(|c| c.is_alphanumeric()) {
                return Err("branch must contain at least one letter or digit".into());
            }
            if branch.contains('/') {
                return Err("branch cannot contain '/'".into());
            }
            // A space is a valid keystroke on the Branch field but
            // `git worktree add -b "my branch"` fails downstream with a
            // raw toast, so reject it here where we can explain why.
            if branch.chars().any(char::is_whitespace) {
                return Err("branch cannot contain spaces".into());
            }
            Some(WorktreeSpec { branch })
        } else {
            None
        };

        Ok(SessionSpec {
            name: name.to_string(),
            path: path.to_string(),
            agent: self.agent().to_string(),
            args: self.args.trim().to_string(),
            options: SpecOptions {
                claude: self.claude.clone(),
                codex: self.codex.clone(),
                kimi: self.kimi.clone(),
                opencode: self.opencode.clone(),
                qwen: self.qwen.clone(),
            },
            container_id: self.add_tab_to.clone(),
            resume: false,
            worktree,
        })
    }
}

impl Default for NewSessionModal {
    fn default() -> Self {
        Self::new(Vec::new(), crate::config::WorktreeLocation::default())
    }
}

impl Modal for NewSessionModal {
    fn id(&self) -> &'static str {
        "new_session"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        // Let Ctrl-C close the modal as a convenience.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }

        // Ctrl-R opens the recents picker.
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Push(Box::new(RecentsModal::new(self.recents.clone())));
        }

        // Ctrl-W rubs out the word before the cursor, as a shell does.
        // On the Path field a "word" is a path segment, so it walks up
        // one directory — the thing it's most useful for here.
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.error = None;
            match self.field {
                Field::Name => self.name = delete_prev_word(&self.name, WordBreak::Whitespace),
                Field::Path => {
                    self.path = delete_prev_word(&self.path, WordBreak::PathSegment);
                    self.reset_path_dropdown();
                }
                Field::Args => self.args = delete_prev_word(&self.args, WordBreak::Whitespace),
                Field::Branch => {
                    self.branch = delete_prev_word(&self.branch, WordBreak::Whitespace);
                    // Emptying the field un-latches the manual edit so
                    // the name slug takes over again, matching Backspace.
                    self.branch_edited = !self.branch.is_empty();
                }
                _ => {}
            }
            return ModalResult::Consumed;
        }

        match key.code {
            KeyCode::Esc => {
                // If the path dropdown is visible, Escape dismisses it
                // instead of closing the modal, so Tab can advance.
                if self.field == Field::Path && self.path_dropdown_active {
                    self.path_dropdown_active = false;
                    self.path_suggestion_idx = None;
                    return ModalResult::Consumed;
                }
                ModalResult::Close(None)
            }
            KeyCode::Tab => {
                // Tab always advances, on every field including Path.
                // It used to complete the path first and only fall
                // through to the next field when there was nothing left
                // to complete — but there almost always was, so Tab
                // effectively stopped working as "next field" and the
                // user had to press Esc to escape the field. The footer
                // promises "tab next"; path completion lives on Right
                // instead (see below).
                self.next_field();
                ModalResult::Consumed
            }
            KeyCode::BackTab => {
                self.prev_field();
                ModalResult::Consumed
            }
            KeyCode::Enter => {
                // Enter on Path with a highlighted dropdown entry:
                // commit it. Directories → stay on Path so the user
                // keeps browsing into subfolders. Files → advance to
                // the next field (so Enter feels like "pick this").
                if self.field == Field::Path {
                    if let Some(idx) = self.path_suggestion_idx {
                        let entries = self.path_suggestions();
                        if let Some(entry) = entries.get(idx).cloned() {
                            let was_dir = entry.is_dir;
                            self.commit_path_entry(&entry);
                            if !was_dir {
                                self.next_field();
                            }
                            return ModalResult::Consumed;
                        }
                    }
                }
                match self.build_spec() {
                    Ok(spec) => {
                        let cmd = match &self.modify_for {
                            Some(internal) => Command::ModifySession {
                                internal: internal.clone(),
                                spec,
                            },
                            None => Command::CreateSession(spec),
                        };
                        ModalResult::Close(Some(cmd))
                    }
                    Err(e) => {
                        self.error = Some(e);
                        ModalResult::Consumed
                    }
                }
            }
            KeyCode::Left => {
                match self.field {
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + AGENTS.len() - 1) % AGENTS.len();
                        self.on_agent_changed();
                    }
                    Field::ClaudeSession => {
                        self.claude.session_mode = self.claude.session_mode.prev();
                    }
                    Field::CodexSession => {
                        self.codex.session_mode = self.codex.session_mode.prev();
                    }
                    Field::KimiSession => {
                        self.kimi.session_mode = self.kimi.session_mode.prev();
                    }
                    Field::OpencodeSession => {
                        self.opencode.session_mode =
                            opencode_mode_toggled(self.opencode.session_mode);
                    }
                    Field::QwenSession => {
                        self.qwen.session_mode = self.qwen.session_mode.prev();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            // Right accepts a path completion, the way a shell's
            // autosuggestion does: the highlighted dropdown entry if the
            // user arrowed into one, otherwise the longest common prefix
            // of what matches. The Path field has no in-field cursor, so
            // Right is free for this.
            KeyCode::Right if self.field == Field::Path => {
                if let Some(idx) = self.path_suggestion_idx {
                    let entries = self.path_suggestions();
                    if let Some(entry) = entries.get(idx).cloned() {
                        self.commit_path_entry(&entry);
                        return ModalResult::Consumed;
                    }
                }
                self.tab_complete_path();
                ModalResult::Consumed
            }
            KeyCode::Right => {
                match self.field {
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
                        self.on_agent_changed();
                    }
                    Field::ClaudeSession => {
                        self.claude.session_mode = self.claude.session_mode.next();
                    }
                    Field::CodexSession => {
                        self.codex.session_mode = self.codex.session_mode.next();
                    }
                    Field::KimiSession => {
                        self.kimi.session_mode = self.kimi.session_mode.next();
                    }
                    Field::OpencodeSession => {
                        self.opencode.session_mode =
                            opencode_mode_toggled(self.opencode.session_mode);
                    }
                    Field::QwenSession => {
                        self.qwen.session_mode = self.qwen.session_mode.next();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Down if self.field == Field::Path => {
                let suggestions = self.path_suggestions();
                let count = suggestions.len();
                if !suggestions.is_empty() {
                    self.path_dropdown_active = true;
                    self.path_suggestion_idx = Some(match self.path_suggestion_idx {
                        None => 0,
                        Some(i) if i + 1 < count => i + 1,
                        Some(i) => i,
                    });
                    self.clamp_dropdown_scroll(count);
                }
                ModalResult::Consumed
            }
            KeyCode::Up if self.field == Field::Path => {
                self.path_suggestion_idx = match self.path_suggestion_idx {
                    None | Some(0) => None,
                    Some(i) => Some(i - 1),
                };
                let count = self.path_suggestions().len();
                self.clamp_dropdown_scroll(count);
                ModalResult::Consumed
            }
            KeyCode::Backspace => {
                self.error = None;
                match self.field {
                    Field::Name => {
                        self.name.pop();
                    }
                    Field::Path => {
                        self.path.pop();
                        self.reset_path_dropdown();
                    }
                    Field::Args => {
                        self.args.pop();
                    }
                    Field::Branch => {
                        self.branch.pop();
                        // Clearing the field to empty un-latches the
                        // manual edit so `branch_effective` re-engages
                        // the name slug — matches the expectation that
                        // emptying the branch restores the auto-slug.
                        self.branch_edited = !self.branch.is_empty();
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            KeyCode::Char(' ') => {
                // Space: toggle boolean option fields, cycle agent on
                // the Agent field, or type a literal space in text
                // fields.
                self.error = None;
                match self.field {
                    Field::Name => self.name.push(' '),
                    Field::Path => {
                        self.path.push(' ');
                        self.reset_path_dropdown();
                    }
                    Field::Args => self.args.push(' '),
                    Field::Branch => {
                        self.branch.push(' ');
                        self.branch_edited = true;
                    }
                    Field::Worktree => {
                        self.worktree = !self.worktree;
                    }
                    Field::Agent => {
                        self.agent_idx = (self.agent_idx + 1) % AGENTS.len();
                        self.on_agent_changed();
                    }
                    Field::ClaudeSkipPerm => {
                        self.claude.skip_permissions = !self.claude.skip_permissions;
                    }
                    Field::CodexYolo => {
                        self.codex.yolo = !self.codex.yolo;
                    }
                    Field::KimiYolo => {
                        self.kimi.yolo = !self.kimi.yolo;
                    }
                    Field::OpencodeAuto => {
                        self.opencode.auto = !self.opencode.auto;
                    }
                    Field::QwenYolo => {
                        self.qwen.yolo = !self.qwen.yolo;
                    }
                    Field::ClaudeSession => {
                        // Space on a radio cycles forward, matching Right.
                        self.claude.session_mode = self.claude.session_mode.next();
                    }
                    Field::CodexSession => {
                        // Space on a radio cycles forward, matching Right.
                        self.codex.session_mode = self.codex.session_mode.next();
                    }
                    Field::KimiSession => {
                        // Space on a radio cycles forward, matching Right.
                        self.kimi.session_mode = self.kimi.session_mode.next();
                    }
                    Field::OpencodeSession => {
                        // Two-state radio: Space toggles, matching Right.
                        self.opencode.session_mode =
                            opencode_mode_toggled(self.opencode.session_mode);
                    }
                    Field::QwenSession => {
                        // Space on a radio cycles forward, matching Right.
                        self.qwen.session_mode = self.qwen.session_mode.next();
                    }
                }
                ModalResult::Consumed
            }
            // Modified keys never insert text: without this, any Ctrl
            // combo the arms above don't claim (Ctrl-W was the one
            // reported, but Ctrl-A, Ctrl-E and friends did it too)
            // typed its bare letter into the field. Matches the guard
            // the rename and section modals already use.
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.error = None;
                match self.field {
                    Field::Name => self.name.push(c),
                    Field::Path => {
                        self.path.push(c);
                        self.reset_path_dropdown();
                    }
                    Field::Args => self.args.push(c),
                    Field::Branch => {
                        self.branch.push(c);
                        self.branch_edited = true;
                    }
                    _ => {}
                }
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn on_child_closed(&mut self, data: ModalData) {
        let ModalData::FillSessionSpec(spec) = data;
        self.fill_from_spec(spec);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rect = center_rect(area, MODAL_WIDTH, self.modal_height());
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        // Drop shadow: 1 row below + 1 col right in near-black.
        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            crate::ui::paint::tint(buf, shadow, style);
        }

        // Modal body: solid panel fill.
        let body_style = Style::default().bg(body_bg);
        crate::ui::paint::fill_opaque(buf, rect, body_style);

        // Left accent bar — 1 col wide, full height.
        let accent_style = Style::default().bg(theme.accent);
        crate::ui::paint::fill_opaque(buf, crate::ui::paint::left_edge(rect), accent_style);

        // Content inset from the accent bar + padding.
        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let title_text = if self.is_add_tab() {
            "Add tab"
        } else if self.modify_for.is_some() {
            "Modify session"
        } else {
            "New session"
        };
        let path_label = if self.is_add_tab() {
            "path (locked to container)"
        } else {
            "path"
        };
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled(
                    title_text,
                    Style::default()
                        .fg(theme.text)
                        .bg(body_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if self.field == Field::Path {
                        "    tab next · → complete · ^r recents · esc cancel"
                    } else {
                        "    tab next · ^r recents · esc cancel · enter create"
                    },
                    Style::default().fg(theme.text_muted).bg(body_bg),
                ),
            ]),
            Line::from(""),
            label_line("name", self.field == Field::Name, theme),
            input_line(&self.name, self.field == Field::Name, inner.width, theme),
            Line::from(""),
            label_line(path_label, self.field == Field::Path, theme),
            input_line(&self.path, self.field == Field::Path, inner.width, theme),
        ];

        // Worktree checkbox goes AFTER the path block so the path
        // dropdown overlay's magic `inner.y + 7` offset stays anchored
        // to the path input. Add-tab mode locks the path to the
        // container, so the worktree option is hidden there.
        if !self.is_add_tab() {
            lines.push(Line::from(""));
            lines.push(checkbox_line(
                "Create in git worktree",
                self.worktree,
                self.field == Field::Worktree,
                theme,
            ));
            if self.worktree {
                let branch = self.branch_effective();
                lines.push(label_line("branch", self.field == Field::Branch, theme));
                lines.push(input_line(
                    &branch,
                    self.field == Field::Branch,
                    inner.width,
                    theme,
                ));
                // Read-only preview of where the worktree lands. Honors
                // `worktree_location` so the shown scheme matches the
                // actor's `resolve_worktree_path`. The real repo root is
                // resolved downstream, so this stays schematic.
                lines.push(Line::from(Span::styled(
                    format!("   worktree: {}", self.worktree_preview()),
                    Style::default().fg(theme.text_muted).bg(body_bg),
                )));
            }
        }

        lines.extend([
            Line::from(""),
            label_line("agent", self.field == Field::Agent, theme),
            agent_line(self.agent_idx, self.field == Field::Agent, theme),
            Line::from(""),
            label_line("args (optional)", self.field == Field::Args, theme),
            input_line(&self.args, self.field == Field::Args, inner.width, theme),
        ]);

        // Agent-specific options section.
        match self.agent() {
            "claude" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Claude options —", theme));
                lines.push(session_radio_line(
                    self.claude.session_mode,
                    self.field == Field::ClaudeSession,
                    theme,
                ));
                lines.push(checkbox_line(
                    "Skip permissions (--dangerously-skip-permissions)",
                    self.claude.skip_permissions,
                    self.field == Field::ClaudeSkipPerm,
                    theme,
                ));
            }
            "codex" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Codex options —", theme));
                lines.push(session_radio_line(
                    self.codex.session_mode,
                    self.field == Field::CodexSession,
                    theme,
                ));
                lines.push(checkbox_line(
                    "YOLO mode (--yolo · bypass approvals & sandbox)",
                    self.codex.yolo,
                    self.field == Field::CodexYolo,
                    theme,
                ));
            }
            "kimi" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Kimi options —", theme));
                lines.push(session_radio_line(
                    self.kimi.session_mode,
                    self.field == Field::KimiSession,
                    theme,
                ));
                lines.push(checkbox_line(
                    "YOLO mode (--yolo · auto-approve all actions)",
                    self.kimi.yolo,
                    self.field == Field::KimiYolo,
                    theme,
                ));
            }
            "opencode" => {
                lines.push(Line::from(""));
                lines.push(section_header("— OpenCode options —", theme));
                lines.push(session_radio_line_modes(
                    self.opencode.session_mode,
                    &[ClaudeSessionMode::New, ClaudeSessionMode::Continue],
                    self.field == Field::OpencodeSession,
                    theme,
                ));
                lines.push(checkbox_line(
                    "Auto mode (--auto · auto-approve permissions)",
                    self.opencode.auto,
                    self.field == Field::OpencodeAuto,
                    theme,
                ));
            }
            "qwen" => {
                lines.push(Line::from(""));
                lines.push(section_header("— Qwen options —", theme));
                lines.push(session_radio_line(
                    self.qwen.session_mode,
                    self.field == Field::QwenSession,
                    theme,
                ));
                lines.push(checkbox_line(
                    "YOLO mode (--yolo · auto-approve all actions)",
                    self.qwen.yolo,
                    self.field == Field::QwenYolo,
                    theme,
                ));
            }
            _ => {}
        }

        if let Some(e) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  ! {}", e),
                Style::default().fg(theme.status_error).bg(body_bg),
            )));
        }

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
            .render(inner, frame.buffer_mut());

        // --- Path dropdown overlay ---
        // Rendered after the main content so it paints on top of the
        // agent/args/options fields below the path input. Only shown
        // when the dropdown hasn't been dismissed with Escape.
        if self.field == Field::Path && self.path_dropdown_active {
            let suggestions = self.path_suggestions();
            if !suggestions.is_empty() {
                // Path input is line 6 in the content; dropdown starts
                // immediately below it.
                let dropdown_y = inner.y + 7;
                let dropdown_x = inner.x;
                let avail = area.bottom().saturating_sub(dropdown_y) as usize;
                let visible = suggestions.len().min(DROPDOWN_MAX_VISIBLE).min(avail);

                if visible > 0 {
                    let scroll = self.path_suggestion_scroll;
                    let has_above = scroll > 0;
                    let has_below = scroll + visible < suggestions.len();
                    let buf = frame.buffer_mut();

                    for vi in 0..visible {
                        let si = scroll + vi;
                        let y = dropdown_y + vi as u16;
                        if y >= area.bottom() {
                            break;
                        }

                        let entry = &suggestions[si];
                        let highlighted = self.path_suggestion_idx == Some(si);
                        let bg = if highlighted {
                            theme.selection_bg
                        } else {
                            theme.bg
                        };
                        let fg = if highlighted {
                            theme.text
                        } else {
                            theme.text_muted
                        };
                        let marker = if highlighted { "▸" } else { " " };
                        let suffix = if entry.is_dir { "/" } else { "" };

                        let text = format!(" {} {}{}", marker, entry.name, suffix);
                        let field_w = inner.width.saturating_sub(3) as usize;

                        // Left margin: keep the modal body bg for the
                        // 3-char indent, then fill the entry area.
                        let margin_style = Style::default().bg(body_bg);
                        for x in dropdown_x..dropdown_x.saturating_add(3).min(area.right()) {
                            let cell = &mut buf[(x, y)];
                            cell.set_char(' ');
                            cell.set_style(margin_style);
                        }

                        let entry_style = Style::default().fg(fg).bg(bg);
                        let entry_x = dropdown_x + 3;
                        // Fill the entry background, then write text.
                        for x in
                            entry_x..(entry_x + inner.width.saturating_sub(3)).min(area.right())
                        {
                            let cell = &mut buf[(x, y)];
                            cell.set_char(' ');
                            cell.set_style(entry_style);
                        }
                        buf.set_string(entry_x, y, &text, entry_style);

                        // Scroll indicators at the right edge.
                        let ind_x = entry_x + field_w as u16 - 2;
                        if ind_x < area.right() {
                            if vi == 0 && has_above {
                                buf.set_string(
                                    ind_x,
                                    y,
                                    "▴",
                                    Style::default().fg(theme.text_muted).bg(bg),
                                );
                            }
                            if vi == visible - 1 && has_below {
                                buf.set_string(
                                    ind_x,
                                    y,
                                    "▾",
                                    Style::default().fg(theme.text_muted).bg(bg),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn label_line(label: &str, focused: bool, theme: &Theme) -> Line<'static> {
    let marker = if focused { "▸" } else { " " };
    let label_style = if focused {
        Style::default()
            .fg(theme.accent)
            .bg(theme.panel_alt)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_muted).bg(theme.panel_alt)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", marker), label_style),
        Span::styled(label.to_string(), label_style),
    ])
}

/// Slugify a display name into a git-branch-safe token: lowercase,
/// each run of non-alphanumeric characters collapsed to a single `-`,
/// with leading/trailing `-` trimmed. Never produces a `/`, so the
/// auto-derived branch always passes `build_spec` validation.
///
/// Distinct on purpose from `tmux_actor::slugify` (which slugs the
/// internal tmux *session* name and keeps `_`). This one feeds the git
/// *branch* name — don't unify them.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(c.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

// --- Filesystem helpers ---------------------------------------------

/// Split a path into its directory portion (with trailing `/`) and
/// the trailing segment the user is typing. Preserves a leading `~`
/// so the stored path keeps its original form.
fn split_path(path: &str) -> (String, String) {
    if path.is_empty() {
        return (String::new(), String::new());
    }
    if path.ends_with('/') {
        return (path.to_string(), String::new());
    }
    match path.rfind('/') {
        Some(idx) => (path[..=idx].to_string(), path[idx + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// What counts as a word boundary for `delete_prev_word`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordBreak {
    /// Shell-style: words are separated by whitespace.
    Whitespace,
    /// Paths: segments are separated by `/`, so rubbing out a word
    /// lands on the parent directory.
    PathSegment,
}

/// Delete the word at the end of `s`, the way a shell's Ctrl-W does:
/// any trailing separators go first, then everything back to (but not
/// including) the separator before them. Returns the empty string when
/// there is no earlier separator to stop at.
fn delete_prev_word(s: &str, brk: WordBreak) -> String {
    let is_sep = |c: char| match brk {
        WordBreak::Whitespace => c.is_whitespace(),
        WordBreak::PathSegment => c == '/',
    };
    // Trailing separators belong to the segment being removed, so
    // Ctrl-W on `/a/b/` acts on `b`, not on the empty piece after it.
    let trimmed = s.trim_end_matches(is_sep);
    match trimmed.rfind(is_sep) {
        // Keep the separator itself: `/a/b/c` -> `/a/b/` stays a
        // directory, and `foo bar` -> `foo ` keeps the space a shell
        // would leave behind.
        Some(idx) => {
            let sep_len = trimmed[idx..].chars().next().map_or(1, char::len_utf8);
            trimmed[..idx + sep_len].to_string()
        }
        None => String::new(),
    }
}

/// Read the directory implied by `path` and return entries whose
/// names start with the trailing segment of `path`. Dirs come first,
/// then files, alphabetically within each group. Hidden entries
/// (starting with `.`) are excluded unless the user's typed prefix
/// also starts with `.`. Capped at `limit` entries.
fn read_dir_filtered(path: &str, limit: usize) -> Vec<PathEntry> {
    let (dir, prefix) = split_path(path);
    // Empty dir = CWD. Otherwise expand ~ for the filesystem lookup.
    let lookup = if dir.is_empty() {
        ".".to_string()
    } else {
        crate::util::path::expand_tilde(&dir)
    };
    let Ok(read) = std::fs::read_dir(&lookup) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<PathEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = entry.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
        out.push(PathEntry { name, is_dir });
    }
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    out.truncate(limit);
    out
}

/// Longest common prefix of a set of strings (character-wise, so
/// multi-byte Unicode is handled correctly).
fn longest_common_prefix(strs: &[&str]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut prefix: Vec<char> = strs[0].chars().collect();
    for s in &strs[1..] {
        let common_len = prefix
            .iter()
            .zip(s.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        prefix.truncate(common_len);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

fn input_line(value: &str, focused: bool, width: u16, theme: &Theme) -> Line<'static> {
    let bg = if focused {
        theme.selection_bg
    } else {
        theme.bg
    };
    let fg = if value.is_empty() {
        theme.text_muted
    } else {
        theme.text
    };
    let cursor = if focused { "│" } else { "" };
    let content = format!(" {}{} ", value, cursor);
    // Pad content to field width so the bg extends cleanly.
    let field_width = width.saturating_sub(3) as usize;
    let padded = if content.chars().count() < field_width {
        let mut s = content;
        while s.chars().count() < field_width {
            s.push(' ');
        }
        s
    } else {
        content
    };
    Line::from(vec![
        Span::styled("   ", Style::default().bg(theme.panel_alt)),
        Span::styled(padded, Style::default().fg(fg).bg(bg)),
    ])
}

fn section_header(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("   ", Style::default().bg(theme.panel_alt)),
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme.text_muted)
                .bg(theme.panel_alt)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn checkbox_line(label: &str, checked: bool, focused: bool, theme: &Theme) -> Line<'static> {
    let body_bg = theme.panel_alt;
    let marker = if focused { "▸" } else { " " };
    let box_glyph = if checked { "[x]" } else { "[ ]" };
    let label_style = if focused {
        Style::default()
            .fg(theme.accent)
            .bg(body_bg)
            .add_modifier(Modifier::BOLD)
    } else if checked {
        Style::default().fg(theme.text).bg(body_bg)
    } else {
        Style::default().fg(theme.text_muted).bg(body_bg)
    };
    let box_style = if checked {
        Style::default()
            .fg(theme.accent)
            .bg(body_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_muted).bg(body_bg)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", marker), label_style),
        Span::styled(box_glyph.to_string(), box_style),
        Span::styled(" ", Style::default().bg(body_bg)),
        Span::styled(label.to_string(), label_style),
    ])
}

/// The New/Continue toggle for OpenCode's two-state session radio —
/// its CLI has no picker, so `Resume` is never offered and cycling in
/// either direction just flips between the two states.
fn opencode_mode_toggled(mode: ClaudeSessionMode) -> ClaudeSessionMode {
    match mode {
        ClaudeSessionMode::New => ClaudeSessionMode::Continue,
        _ => ClaudeSessionMode::New,
    }
}

fn session_radio_line(mode: ClaudeSessionMode, focused: bool, theme: &Theme) -> Line<'static> {
    session_radio_line_modes(
        mode,
        &[
            ClaudeSessionMode::New,
            ClaudeSessionMode::Continue,
            ClaudeSessionMode::Resume,
        ],
        focused,
        theme,
    )
}

/// Render the session radio with an explicit option set — agents
/// without a CLI session picker (OpenCode) offer only New/Continue.
fn session_radio_line_modes(
    mode: ClaudeSessionMode,
    modes: &[ClaudeSessionMode],
    focused: bool,
    theme: &Theme,
) -> Line<'static> {
    let body_bg = theme.panel_alt;
    let marker = if focused { "▸" } else { " " };
    let marker_style = if focused {
        Style::default()
            .fg(theme.accent)
            .bg(body_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_muted).bg(body_bg)
    };
    let label_style = if focused {
        Style::default()
            .fg(theme.accent)
            .bg(body_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_muted).bg(body_bg)
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled("Session  ", label_style),
    ];
    for &option in modes {
        let selected = option == mode;
        let (dot, val_style) = if selected {
            let style = if focused {
                Style::default()
                    .fg(theme.accent)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD)
            };
            ("(•)", style)
        } else {
            ("( )", Style::default().fg(theme.text_muted).bg(body_bg))
        };
        spans.push(Span::styled(format!(" {} ", dot), val_style));
        spans.push(Span::styled(option.label().to_string(), val_style));
        spans.push(Span::styled(" ", Style::default().bg(body_bg)));
    }
    Line::from(spans)
}

fn agent_line(selected: usize, focused: bool, theme: &Theme) -> Line<'static> {
    let body_bg = theme.panel_alt;
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("   ", Style::default().bg(body_bg)));
    for (i, agent) in AGENTS.iter().enumerate() {
        let style = if i == selected && focused {
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if i == selected {
            Style::default()
                .fg(theme.accent)
                .bg(body_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_muted).bg(body_bg)
        };
        spans.push(Span::styled(format!(" {} ", agent), style));
        spans.push(Span::styled(" ", Style::default().bg(body_bg)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorktreeLocation;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Factory for tests that exercise Tab field-cycling. `NewSessionModal::new`
    /// pulls the current working directory into `self.path`, and Tab on the
    /// Path field does filesystem completion — so if the cwd happens to
    /// contain entries matching the typed prefix, Tab commits a completion
    /// and stays on Path instead of advancing. That's environment-dependent
    /// and was breaking CI (Linux runner's `/home/runner/work/bosun/bosun`
    /// has exactly one child named `bosun` → single-match commit) while
    /// passing locally (dev box has multiple `bosun*` neighbors → LCP extends
    /// past ambiguity). Pin the path to a guaranteed-nonexistent directory so
    /// `read_dir` returns empty and Tab always advances the field.
    fn modal_for_field_tests() -> NewSessionModal {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.path = "/_bosun_unit_test_nonexistent_/".into();
        m
    }

    /// Issue #12: the modal is drawn straight over a live terminal
    /// pane, and `Cell::set_style` only *adds* modifiers — it never
    /// clears what is already on the cell. Painting the body with a
    /// plain background style therefore left underlines (and bold,
    /// and reverse video) from the pane below running through the
    /// dialog. Render over a deliberately attribute-heavy buffer and
    /// assert the modal's own surface comes out clean.
    #[test]
    fn modal_body_covers_attributes_from_the_pane_below() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;
        use ratatui::Terminal;

        let m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        let theme = crate::ui::Theme::default_opencode();
        let (w, h) = (120u16, 40u16);

        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let area = ratatui::layout::Rect::new(0, 0, w, h);
        terminal
            .draw(|f| {
                // Stand in for the embedded pane: every cell underlined,
                // bold and reversed.
                let busy = Style::default()
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD | Modifier::REVERSED);
                let buf = f.buffer_mut();
                for y in 0..h {
                    for x in 0..w {
                        let cell = &mut buf[(x, y)];
                        cell.set_char('_');
                        cell.set_style(busy);
                    }
                }
                m.render(f, area, &theme);
            })
            .unwrap();

        let rect = center_rect(area, MODAL_WIDTH, m.modal_height());
        let buf = terminal.backend().buffer();
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                let cell = &buf[(x, y)];
                assert!(
                    !cell.modifier.contains(Modifier::UNDERLINED),
                    "underline from the pane below bled into the modal at ({x},{y})"
                );
                assert!(
                    !cell.modifier.contains(Modifier::REVERSED),
                    "reverse video from the pane below bled into the modal at ({x},{y})"
                );
                assert_ne!(
                    cell.symbol(),
                    "_",
                    "pane text showing through the modal body at ({x},{y})"
                );
            }
        }
    }

    /// A directory with two `zebra_*` children, so completion has a
    /// longest common prefix to extend to and the dropdown is non-empty.
    ///
    /// Each call gets its own directory. It used to key the name off
    /// `line!()`, which expands where the macro is *written* rather
    /// than at the call site — so every caller shared one directory,
    /// and since the tests run in parallel and each removes the
    /// directory when it finishes, one test could delete the tree
    /// another was still using.
    fn dir_with_completions() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "bosun-complete-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(base.join("zebra_one")).expect("mkdir");
        std::fs::create_dir_all(base.join("zebra_two")).expect("mkdir");
        base
    }

    #[test]
    fn tab_advances_from_path_even_when_completions_exist() {
        let base = dir_with_completions();
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Path;
        m.path = format!("{}/z", base.display());
        let typed = m.path.clone();

        m.handle(key(KeyCode::Tab));

        // Tab is "next field" everywhere — it must not silently turn
        // into a completion just because the Path field had matches.
        assert_eq!(m.field, Field::Worktree, "Tab should advance off Path");
        assert_eq!(m.path, typed, "Tab should not rewrite the path");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn right_completes_path_and_stays_on_the_field() {
        let base = dir_with_completions();
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Path;
        m.path = format!("{}/z", base.display());

        m.handle(key(KeyCode::Right));

        assert_eq!(
            m.path,
            format!("{}/zebra_", base.display()),
            "Right should extend to the longest common prefix"
        );
        assert_eq!(
            m.field,
            Field::Path,
            "completing should not leave the field"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn right_commits_the_highlighted_dropdown_entry() {
        let base = dir_with_completions();
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Path;
        m.path = format!("{}/z", base.display());

        m.handle(key(KeyCode::Down)); // arrow into the dropdown
        m.handle(key(KeyCode::Right)); // accept what's highlighted

        assert_eq!(
            m.path,
            format!("{}/zebra_one/", base.display()),
            "Right should commit the highlighted entry, with a trailing slash for a dir"
        );
        assert_eq!(m.field, Field::Path);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn delete_prev_word_walks_up_a_path() {
        let up = |p: &str| delete_prev_word(p, WordBreak::PathSegment);
        assert_eq!(up("/Users/olaa/work/"), "/Users/olaa/");
        assert_eq!(up("/Users/olaa/work"), "/Users/olaa/");
        assert_eq!(up("/Users/olaa/"), "/Users/");
        assert_eq!(up("~/work/deep"), "~/work/");
        assert_eq!(up("~/"), "");
        assert_eq!(up("/"), "");
        assert_eq!(up("relative"), "");
        assert_eq!(up(""), "");
        // A space inside a path is part of the segment, not a break.
        assert_eq!(up("/tmp/my folder/x"), "/tmp/my folder/");
    }

    #[test]
    fn delete_prev_word_rubs_out_words_elsewhere() {
        let w = |t: &str| delete_prev_word(t, WordBreak::Whitespace);
        assert_eq!(w("hello world"), "hello ");
        assert_eq!(w("hello world   "), "hello ");
        assert_eq!(w("solo"), "");
        assert_eq!(w(""), "");
    }

    /// Issue #11: Ctrl-W used to type a `w` into the field.
    #[test]
    fn ctrl_w_edits_instead_of_typing_a_letter() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());

        m.field = Field::Path;
        m.path = "/Users/olaa/work/".into();
        m.handle(ctrl(KeyCode::Char('w')));
        assert_eq!(m.path, "/Users/olaa/", "Ctrl-W should walk up a directory");

        m.field = Field::Name;
        m.name = "my session".into();
        m.handle(ctrl(KeyCode::Char('w')));
        assert_eq!(m.name, "my ");

        m.field = Field::Args;
        m.args = "--foo --bar".into();
        m.handle(ctrl(KeyCode::Char('w')));
        assert_eq!(m.args, "--foo ");
    }

    /// Emptying the branch by rubbing out its last word re-engages the
    /// auto-slug, the same way Backspace to empty does.
    #[test]
    fn ctrl_w_emptying_the_branch_restores_the_slug() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Branch;
        m.branch = "custom".into();
        m.branch_edited = true;
        m.handle(ctrl(KeyCode::Char('w')));
        assert_eq!(m.branch, "");
        assert!(
            !m.branch_edited,
            "empty branch should un-latch the manual edit"
        );
    }

    /// The underlying cause of #11: a modified key fell through to the
    /// plain character arm and inserted its letter.
    #[test]
    fn other_control_combos_do_not_type_their_letter() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Name;
        for c in ['a', 'e', 'u', 'k'] {
            m.handle(ctrl(KeyCode::Char(c)));
        }
        assert_eq!(m.name, "", "Ctrl combos must not insert text");
        // Plain typing still works.
        m.handle(key(KeyCode::Char('x')));
        assert_eq!(m.name, "x");
    }

    #[test]
    fn new_session_modal_preselects_the_configured_agent() {
        let m = NewSessionModal::with_default_agent(
            Vec::new(),
            WorktreeLocation::default(),
            "opencode",
        );
        assert_eq!(m.agent(), "opencode");
    }

    /// The add-tab form is the other way to create a session, so it has
    /// to honour `default_agent` too — otherwise `n` and Ctrl+T would
    /// disagree about which agent is preselected.
    #[test]
    fn add_tab_modal_preselects_the_configured_agent() {
        let m = NewSessionModal::for_add_tab(
            "bosun-container".to_string(),
            "/tmp".to_string(),
            Vec::new(),
            "qwen",
        );
        assert_eq!(m.agent(), "qwen");
    }

    #[test]
    fn an_unknown_default_agent_falls_back_to_the_first() {
        let m = NewSessionModal::with_default_agent(
            Vec::new(),
            WorktreeLocation::default(),
            "nonesuch",
        );
        assert_eq!(m.agent(), AGENTS[0]);
        let t = NewSessionModal::for_add_tab(
            "c".to_string(),
            "/tmp".to_string(),
            Vec::new(),
            "nonesuch",
        );
        assert_eq!(t.agent(), AGENTS[0]);
    }

    #[test]
    fn tab_cycles_fields_for_claude() {
        let mut m = modal_for_field_tests();
        assert_eq!(m.agent(), "claude");
        assert_eq!(m.field, Field::Name);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Path);
        // Worktree checkbox is always in the tab order (off by default);
        // the Branch field only appears once it's toggled on.
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Worktree);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Agent);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Args);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::ClaudeSession);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::ClaudeSkipPerm);
        // Wraps back to Name.
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn tab_cycles_fields_for_kimi() {
        let mut m = modal_for_field_tests();
        let kimi_idx = AGENTS.iter().position(|a| *a == "kimi").unwrap();
        m.agent_idx = kimi_idx;
        assert_eq!(m.agent(), "kimi");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Worktree
        m.handle(key(KeyCode::Tab)); // Worktree -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        m.handle(key(KeyCode::Tab)); // Args -> KimiSession
        assert_eq!(m.field, Field::KimiSession);
        m.handle(key(KeyCode::Tab)); // KimiSession -> KimiYolo
        assert_eq!(m.field, Field::KimiYolo);
        // Wraps back to Name.
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn modal_height_reserves_room_for_kimi_options() {
        // Regression: kimi must reserve the same 4 option rows as claude
        // (blank + header + session radio + yolo checkbox), otherwise the
        // options render below the clipped modal height and vanish.
        let mut m = modal_for_field_tests();
        m.agent_idx = AGENTS.iter().position(|a| *a == "claude").unwrap();
        let claude_h = m.modal_height();
        m.agent_idx = AGENTS.iter().position(|a| *a == "kimi").unwrap();
        let kimi_h = m.modal_height();
        m.agent_idx = AGENTS.iter().position(|a| *a == "terminal").unwrap();
        let terminal_h = m.modal_height();
        assert_eq!(kimi_h, claude_h);
        assert!(kimi_h > terminal_h);
    }

    #[test]
    fn tab_cycles_fields_for_codex() {
        let mut m = modal_for_field_tests();
        // Switch to codex (second in the list).
        m.agent_idx = 1;
        assert_eq!(m.agent(), "codex");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Worktree
        m.handle(key(KeyCode::Tab)); // Worktree -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        m.handle(key(KeyCode::Tab)); // Args -> CodexSession
        assert_eq!(m.field, Field::CodexSession);
        m.handle(key(KeyCode::Tab)); // CodexSession -> CodexYolo
        assert_eq!(m.field, Field::CodexYolo);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn tab_cycles_fields_for_opencode() {
        let mut m = modal_for_field_tests();
        let idx = AGENTS.iter().position(|a| *a == "opencode").unwrap();
        m.agent_idx = idx;
        assert_eq!(m.agent(), "opencode");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Worktree
        m.handle(key(KeyCode::Tab)); // Worktree -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        m.handle(key(KeyCode::Tab)); // Args -> OpencodeSession
        assert_eq!(m.field, Field::OpencodeSession);
        m.handle(key(KeyCode::Tab)); // OpencodeSession -> OpencodeAuto
        assert_eq!(m.field, Field::OpencodeAuto);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn tab_cycles_fields_for_qwen() {
        let mut m = modal_for_field_tests();
        let idx = AGENTS.iter().position(|a| *a == "qwen").unwrap();
        m.agent_idx = idx;
        assert_eq!(m.agent(), "qwen");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Worktree
        m.handle(key(KeyCode::Tab)); // Worktree -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        m.handle(key(KeyCode::Tab)); // Args -> QwenSession
        assert_eq!(m.field, Field::QwenSession);
        m.handle(key(KeyCode::Tab)); // QwenSession -> QwenYolo
        assert_eq!(m.field, Field::QwenYolo);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn opencode_session_radio_toggles_two_states() {
        let mut m = modal_for_field_tests();
        let idx = AGENTS.iter().position(|a| *a == "opencode").unwrap();
        m.agent_idx = idx;
        m.field = Field::OpencodeSession;
        assert_eq!(m.opencode.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.opencode.session_mode, ClaudeSessionMode::Continue);
        // Right again wraps back to New — Resume is never offered.
        m.handle(key(KeyCode::Right));
        assert_eq!(m.opencode.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Left));
        assert_eq!(m.opencode.session_mode, ClaudeSessionMode::Continue);
    }

    #[test]
    fn tab_cycles_fields_for_terminal() {
        let mut m = modal_for_field_tests();
        m.agent_idx = AGENTS.len() - 1;
        assert_eq!(m.agent(), "terminal");
        m.handle(key(KeyCode::Tab)); // Name -> Path
        m.handle(key(KeyCode::Tab)); // Path -> Worktree
        m.handle(key(KeyCode::Tab)); // Worktree -> Agent
        m.handle(key(KeyCode::Tab)); // Agent -> Args
        assert_eq!(m.field, Field::Args);
        m.handle(key(KeyCode::Tab));
        assert_eq!(m.field, Field::Name);
    }

    #[test]
    fn worktree_checkbox_reveals_branch_field() {
        let mut m = modal_for_field_tests();
        // Off by default: Branch not in tab order.
        assert!(!Field::visible_for(m.agent(), false, false).contains(&Field::Branch));
        m.worktree = true;
        assert!(Field::visible_for(m.agent(), false, true).contains(&Field::Branch));
    }

    #[test]
    fn branch_slug_tracks_name_until_edited() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.worktree = true;
        // Field starts on Name; type the name.
        for c in "My Feature".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.branch_effective(), "my-feature"); // slug of the name
                                                        // Manually edit the branch — this sets branch_edited.
        m.field = Field::Branch;
        for c in "custom".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert!(m.branch_edited);
        let edited = m.branch_effective();
        // Now change the name again; the manual branch edit must STICK.
        m.field = Field::Name;
        m.handle(key(KeyCode::Char('X')));
        assert_eq!(
            m.branch_effective(),
            edited,
            "manual branch edit must survive a name change"
        );
    }

    #[test]
    fn build_spec_carries_worktree() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "feat".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        m.worktree = true;
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert!(spec.worktree.is_some());
                assert_eq!(spec.worktree.unwrap().branch, "feat");
            }
            _ => panic!("expected CreateSession with worktree"),
        }
    }

    #[test]
    fn build_spec_rejects_slash_in_branch() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "feat".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        m.worktree = true;
        m.field = Field::Branch;
        for c in "foo/bar".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        // Slash in branch must be rejected with an error, not submitted.
        assert!(matches!(r, ModalResult::Consumed));
        assert_eq!(m.error.as_deref(), Some("branch cannot contain '/'"));
    }

    #[test]
    fn build_spec_rejects_space_in_branch() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "feat".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        m.worktree = true;
        m.field = Field::Branch;
        for c in "my branch".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        // A space breaks `git worktree add -b` downstream, so reject it.
        assert!(matches!(r, ModalResult::Consumed));
        assert_eq!(m.error.as_deref(), Some("branch cannot contain spaces"));
    }

    #[test]
    fn branch_backspaced_to_empty_re_engages_name_slug() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.worktree = true;
        // Type a name so the slug has something to fall back to.
        for c in "My Feature".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        // Manually edit the branch — latches branch_edited.
        m.field = Field::Branch;
        for c in "custom".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert!(m.branch_edited);
        // Backspace the branch all the way to empty.
        for _ in 0.."custom".len() {
            m.handle(key(KeyCode::Backspace));
        }
        assert!(m.branch.is_empty());
        assert!(!m.branch_edited, "clearing the branch must un-latch it");
        // The effective branch falls back to the name slug again.
        assert_eq!(m.branch_effective(), "my-feature");
    }

    #[test]
    fn worktree_preview_honors_location() {
        let mut sub = NewSessionModal::new(Vec::new(), WorktreeLocation::Subdir);
        sub.path = "/srv/proj".into();
        sub.worktree = true;
        for c in "feat".chars() {
            sub.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(sub.worktree_preview(), ".worktrees/feat");

        let mut sib = NewSessionModal::new(Vec::new(), WorktreeLocation::Sibling);
        sib.path = "/srv/proj".into();
        sib.worktree = true;
        for c in "feat".chars() {
            sib.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(sib.worktree_preview(), "proj-feat");
        // The two schemes must differ so the preview isn't misleading.
        assert_ne!(sub.worktree_preview(), sib.worktree_preview());
    }

    #[test]
    fn add_tab_mode_hides_worktree_fields() {
        // lock_path = true (add-tab mode) must expose NEITHER the
        // worktree checkbox nor the branch field: a tab inherits its
        // container's path, so worktree is mutually exclusive with it.
        let visible = Field::visible_for("claude", true, true);
        assert!(!visible.contains(&Field::Worktree));
        assert!(!visible.contains(&Field::Branch));
    }

    #[test]
    fn space_toggles_worktree_when_focused() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Worktree;
        assert!(!m.worktree);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.worktree);
        m.handle(key(KeyCode::Char(' ')));
        assert!(!m.worktree);
    }

    #[test]
    fn space_toggles_skip_permissions_when_focused() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::ClaudeSkipPerm;
        assert!(!m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.claude.skip_permissions);
        m.handle(key(KeyCode::Char(' ')));
        assert!(!m.claude.skip_permissions);
    }

    #[test]
    fn left_right_cycles_claude_session_mode() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::ClaudeSession;
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Continue);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Resume);
        m.handle(key(KeyCode::Right));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::New);
        m.handle(key(KeyCode::Left));
        assert_eq!(m.claude.session_mode, ClaudeSessionMode::Resume);
    }

    #[test]
    fn space_toggles_codex_yolo_when_focused() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.agent_idx = 1;
        m.field = Field::CodexYolo;
        assert!(!m.codex.yolo);
        m.handle(key(KeyCode::Char(' ')));
        assert!(m.codex.yolo);
    }

    #[test]
    fn submit_spec_carries_claude_options() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "test".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        m.claude.skip_permissions = true;
        m.claude.session_mode = ClaudeSessionMode::Continue;
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert!(spec.options.claude.skip_permissions);
                assert_eq!(
                    spec.options.claude.session_mode,
                    ClaudeSessionMode::Continue
                );
            }
            _ => panic!("expected CreateSession"),
        }
    }

    #[test]
    fn typing_fills_focused_field() {
        // Pinned path so the Tab below unambiguously advances Name -> Path
        // instead of triggering filesystem completion — see
        // `modal_for_field_tests` for context.
        let mut m = modal_for_field_tests();
        for c in "api".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.name, "api");
        m.handle(key(KeyCode::Tab));
        m.handle(key(KeyCode::Backspace));
        // Backspace on path removes from default path, not name.
        assert_eq!(m.name, "api");
    }

    #[test]
    fn left_right_on_agent_field_cycles_selection() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        m.field = Field::Agent;
        assert_eq!(m.agent(), "claude");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "codex");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "kimi");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "opencode");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "qwen");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "terminal");
        m.handle(key(KeyCode::Right));
        assert_eq!(m.agent(), "claude");
        m.handle(key(KeyCode::Left));
        assert_eq!(m.agent(), "terminal");
    }

    #[test]
    fn enter_with_empty_name_shows_error() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.is_some());
    }

    #[test]
    fn enter_with_valid_data_closes_with_command() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "work".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert_eq!(spec.name, "work");
                assert_eq!(spec.agent, "claude");
            }
            _ => panic!("expected Close with CreateSession"),
        }
    }

    #[test]
    fn bosun_prefix_is_stripped_from_name_on_submit() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "bosun-work".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                assert_eq!(spec.name, "work");
            }
            _ => panic!("expected Close with CreateSession"),
        }
    }

    #[test]
    fn name_with_spaces_is_accepted() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "My Rocket Fox".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::Close(Some(Command::CreateSession(spec))) => {
                // Display name preserved verbatim, caps + spaces included.
                assert_eq!(spec.name, "My Rocket Fox");
            }
            _ => panic!("expected CreateSession with 'My Rocket Fox'"),
        }
    }

    #[test]
    fn name_with_only_symbols_is_rejected() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        for c in "!!!".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        let r = m.handle(key(KeyCode::Enter));
        assert!(matches!(r, ModalResult::Consumed));
        assert!(m.error.as_deref().unwrap().contains("letter"));
    }

    #[test]
    fn esc_closes_without_command() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        let r = m.handle(key(KeyCode::Esc));
        assert!(matches!(r, ModalResult::Close(None)));
    }

    #[test]
    fn ctrl_r_pushes_recents_modal() {
        let recent = Recent {
            id: 1,
            name: "work".into(),
            path: "/srv".into(),
            agent: "claude".into(),
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            last_used_at: 0,
            use_count: 1,
        };
        let mut m = NewSessionModal::new(vec![recent], WorktreeLocation::default());
        let k = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let r = m.handle(k);
        assert!(matches!(r, ModalResult::Push(_)));
    }

    #[test]
    fn split_path_handles_absolute_and_relative() {
        assert_eq!(
            split_path("/tmp/user/proj"),
            ("/tmp/user/".to_string(), "proj".to_string())
        );
        assert_eq!(
            split_path("/tmp/user/"),
            ("/tmp/user/".to_string(), "".to_string())
        );
        assert_eq!(split_path("proj"), ("".to_string(), "proj".to_string()));
        assert_eq!(split_path(""), ("".to_string(), "".to_string()));
    }

    #[test]
    fn longest_common_prefix_handles_unicode() {
        assert_eq!(longest_common_prefix(&["abcd", "abce"]), "abc");
        assert_eq!(longest_common_prefix(&["abc", "xyz"]), "");
        assert_eq!(longest_common_prefix(&["same", "same"]), "same");
        assert_eq!(longest_common_prefix(&[]), "");
        // Multi-byte characters handled char-wise.
        assert_eq!(longest_common_prefix(&["日本語", "日本人"]), "日本");
    }

    #[test]
    fn on_child_closed_fills_all_fields_from_spec() {
        let mut m = NewSessionModal::new(Vec::new(), WorktreeLocation::default());
        let spec = SessionSpec {
            name: "api".into(),
            path: "/srv/api".into(),
            agent: "codex".into(),
            args: "--verbose".into(),
            options: SpecOptions {
                claude: ClaudeOptions::default(),
                codex: CodexOptions {
                    yolo: true,
                    ..Default::default()
                },
                kimi: KimiOptions::default(),
                opencode: OpencodeOptions::default(),
                qwen: QwenOptions::default(),
            },
            container_id: None,
            resume: false,
            worktree: None,
        };
        m.on_child_closed(ModalData::FillSessionSpec(spec));
        assert_eq!(m.name, "api");
        assert_eq!(m.path, "/srv/api");
        assert_eq!(m.args, "--verbose");
        assert_eq!(m.agent(), "codex");
        assert!(m.codex.yolo);
        assert_eq!(m.field, Field::Name);
    }
}
