//! Sidebar ordering + grouping (explicit-membership model).
//!
//! The sidebar is two ordered lists: an `ungrouped` bucket and a
//! `sections` list. Each entry is a `Container` — a Bosun-side
//! grouping that owns 1..N tmux sessions ("tabs"). Phase 1 always
//! has exactly one tab per container; the shape is plumbed through
//! ahead of phase 2's tab strip UI so the rest of the codebase can
//! migrate in one move.
//!
//! The rendered sidebar flattens this model into a single list:
//! every ungrouped container, followed by each section's header
//! and its members. `AppState::selected` indexes into that
//! flattened list.
//!
//! Persisted in `config.toml` as `[sidebar]`. Containers
//! deserialize from either the new table shape **or** a bare
//! string (legacy single-tab sessions); on save they always emit
//! the table shape, so first save after upgrade migrates the file
//! in place.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A named section that groups a set of containers. `members`
/// holds containers in the user's chosen order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_containers")]
    pub members: Vec<Container>,
    /// When true, the section's members are hidden from the rendered
    /// sidebar (only the header is visible). Toggled by Tab on the
    /// header. Persisted in `config.toml` so the open/closed state
    /// survives restarts. Default false (expanded) — old configs
    /// without this field stay expanded on first read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    /// Per-section override for the TDF banner font shown in the
    /// preview pane. `None` falls back to `Config::banner_font`
    /// (the global default). Toggled by `f` on the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_font: Option<String>,
    /// What a new session made in this section starts with in Bosun
    /// for Mac's new-session sheet. `None`: the usual defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<SectionDefaults>,
    /// The built-in Snoozed or Docked section. `None`: one of the
    /// user's own sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SectionRole>,
}

/// The built-in sections a row is parked in, kept after the user's own
/// sections, Snoozed before Docked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionRole {
    /// Out of the way for a while; its sessions keep running, and it
    /// goes back by itself at `wake_at` or when it needs you.
    Snoozed,
    /// Finished with: its sessions are killed on the way in and
    /// resumed on the way out.
    Docked,
}

impl SectionRole {
    pub fn title(self) -> &'static str {
        match self {
            SectionRole::Snoozed => "Snoozed",
            SectionRole::Docked => "Docked",
        }
    }
}

/// Where a parked row came from, so it goes back to the same place,
/// and when a snoozed one wakes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Parked {
    /// When it was parked, Unix ms.
    pub at: u64,
    /// The section it came from; `None` for the ungrouped rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_section: Option<String>,
    /// Its index there.
    #[serde(default)]
    pub from_index: usize,
    /// Snoozed: when it wakes, Unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<u64>,
    /// Snoozed: wake as soon as one of its agents is waiting, done or
    /// in error.
    #[serde(default, skip_serializing_if = "is_false")]
    pub until_needed: bool,
}

/// A section's starting values for a new session: its folder, agent
/// and the agent's options, and extra arguments. Empty strings and
/// `None` leave that field to the sheet's usual default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SectionDefaults {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    #[serde(default)]
    pub options: crate::serve::protocol::OptionsWire,
}

impl SectionDefaults {
    /// The same checks a session spec gets.
    pub fn check(&self) -> Result<(), String> {
        crate::serve::protocol::check_text("defaults.path", &self.path, true)?;
        crate::serve::protocol::check_text("defaults.args", &self.args, true)?;
        if let Some(a) = &self.agent {
            if !crate::config::AGENTS.contains(&a.as_str()) {
                return Err(format!(
                    "defaults.agent must be one of {}",
                    crate::config::AGENTS.join(", ")
                ));
            }
        }
        Ok(())
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Section {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: next_id("sec"),
            name: name.into(),
            members: Vec::new(),
            collapsed: false,
            banner_font: None,
            defaults: None,
            role: None,
        }
    }
}

/// One sidebar entry. Owns an ordered list of tmux session names
/// (its "tabs") and tracks which one is currently active. Phase 1
/// always has exactly one tab; phase 2's tab strip starts using
/// `members.len() > 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    /// Display label for the sidebar row, independent of any
    /// individual tab's name. Phase 1 seeds this with the single
    /// tab's internal name as a placeholder; phase 2's render path
    /// starts reading it directly.
    pub name: String,
    pub members: Vec<String>,
    /// Internal tmux name of the currently-active tab. Always one
    /// of `members`; invariant maintained by all mutators.
    pub active: String,
    /// Set while the row is in the Snoozed or Docked section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<Parked>,
}

impl Container {
    /// Construct a single-tab container wrapping `internal` as the
    /// only tab. `name` is the sidebar label.
    pub fn single(internal: String, name: String) -> Self {
        Self {
            id: next_id("con"),
            name,
            active: internal.clone(),
            members: vec![internal],
            parked: None,
        }
    }

    /// Construct a single-tab container with a caller-supplied
    /// id — used by `reconcile` when a freshly-seen tmux session
    /// already advertises a `@bosun_container_id` for which we
    /// have no existing container (server-side first sighting).
    pub fn with_id(id: String, internal: String, name: String) -> Self {
        Self {
            id,
            name,
            active: internal.clone(),
            members: vec![internal],
            parked: None,
        }
    }

    /// Append `internal` as a new tab and switch the active tab
    /// to it. No-op if the name is already a tab.
    pub fn add_tab(&mut self, internal: String) {
        if self.members.iter().any(|m| m == &internal) {
            self.active = internal;
            return;
        }
        self.active = internal.clone();
        self.members.push(internal);
    }

    /// True iff `internal` is one of this container's tabs.
    pub fn contains_internal(&self, internal: &str) -> bool {
        self.members.iter().any(|m| m == internal)
    }
}

/// On-disk shape for a container entry: either the new table form
/// (proper `Container` table) or a legacy bare string (1.x configs
/// pre-containers). Normalized into `Container` on load; never
/// serialized — saves always emit the table form so an upgraded
/// config migrates on its first write.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StoredContainer {
    Legacy(String),
    Container(Container),
}

impl From<StoredContainer> for Container {
    fn from(s: StoredContainer) -> Self {
        match s {
            StoredContainer::Legacy(internal) => Container::single(internal.clone(), internal),
            StoredContainer::Container(c) => c,
        }
    }
}

fn deserialize_containers<'de, D>(de: D) -> Result<Vec<Container>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stored: Vec<StoredContainer> = Vec::deserialize(de)?;
    Ok(stored.into_iter().map(Container::from).collect())
}

/// Monotonic 32-bit suffix generator for new IDs. Nanos alone can
/// collide when many entries are created in a tight loop (legacy
/// config upgrade is the obvious case), so we xor in a per-process
/// sequence to keep collisions astronomically unlikely without
/// reaching for a UUID dependency.
fn next_id(prefix: &str) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0);
    format!("{}-{:08x}", prefix, nanos.wrapping_add(seq))
}

/// Full sidebar state. `ungrouped` holds top-level containers;
/// `sections` is an ordered list of sections.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarModel {
    #[serde(default, deserialize_with = "deserialize_containers")]
    pub ungrouped: Vec<Container>,
    #[serde(default)]
    pub sections: Vec<Section>,
    /// Display names of tabs whose tmux session has ended, by session
    /// name: written when a row is docked (the name lived on the
    /// session) and when a docked tab is renamed. Without one, an
    /// ended tab's name comes from recents, which keep the name its
    /// session was created with.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ended_names: BTreeMap<String, String>,
}

/// Why [`SidebarModel::move_tab`] or [`SidebarModel::merge_rows`]
/// refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveError {
    /// No row has this session as a tab.
    UnknownSession(String),
    /// No row has this container id.
    UnknownContainer(String),
    /// No section has this id.
    UnknownSection(String),
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::UnknownSession(s) => write!(f, "no sidebar row has the session {s}"),
            MoveError::UnknownContainer(c) => write!(f, "no sidebar row with id {c}"),
            MoveError::UnknownSection(s) => write!(f, "no section with id {s}"),
        }
    }
}

/// One row in the rendered sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleKind {
    Ungrouped,
    Header,
    Member,
}

/// A location inside the model — used to mutate after resolving a
/// selection index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// `ungrouped[idx]`
    Ungrouped(usize),
    /// `sections[si]` (the header)
    Header(usize),
    /// `sections[si].members[mi]`
    Member(usize, usize),
}

/// A single visible entry produced by flattening the model. Carries
/// references into the model for rendering. Container rows hand
/// back the whole `&Container` so renderers can read both the
/// active tab (for status / preview lookup) and the tab count (for
/// the `(N)` badge in phase 2).
#[derive(Debug, Clone, Copy)]
pub enum VisibleEntry<'a> {
    Ungrouped(&'a Container),
    SectionHeader(&'a Section),
    Member {
        section: &'a Section,
        container: &'a Container,
    },
}

impl<'a> VisibleEntry<'a> {
    pub fn kind(&self) -> VisibleKind {
        match self {
            Self::Ungrouped(_) => VisibleKind::Ungrouped,
            Self::SectionHeader(_) => VisibleKind::Header,
            Self::Member { .. } => VisibleKind::Member,
        }
    }

    /// For container rows, the active tab's internal tmux name.
    /// `None` for section headers. This is what most callers want
    /// when they ask "what session is under the cursor."
    pub fn session_name(&self) -> Option<&'a str> {
        match self {
            Self::Ungrouped(c) => Some(c.active.as_str()),
            Self::Member { container, .. } => Some(container.active.as_str()),
            Self::SectionHeader(_) => None,
        }
    }

    /// The full container under the cursor, if any. Section
    /// headers return `None`.
    pub fn container(&self) -> Option<&'a Container> {
        match self {
            Self::Ungrouped(c) => Some(c),
            Self::Member { container, .. } => Some(container),
            Self::SectionHeader(_) => None,
        }
    }

    /// Stable identity for selection-preservation across refreshes.
    /// Container rows use `container.id` (stable across tab
    /// switches and renames); section rows use `section.id`.
    pub fn identity(&self) -> &'a str {
        match self {
            Self::Ungrouped(c) => &c.id,
            Self::SectionHeader(s) => &s.id,
            Self::Member { container, .. } => &container.id,
        }
    }
}

impl SidebarModel {
    /// How many of `s.members` actually contribute visible rows. Zero
    /// when the section is collapsed; full count when expanded.
    fn visible_member_count(s: &Section) -> usize {
        if s.collapsed {
            0
        } else {
            s.members.len()
        }
    }

    /// Total number of visible rows in the flattened sidebar.
    pub fn len(&self) -> usize {
        self.ungrouped.len()
            + self
                .sections
                .iter()
                .map(|s| 1 + Self::visible_member_count(s))
                .sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Name of the section that owns the container holding the tmux
    /// session `internal`, if any. Walks sections in order and returns
    /// the first whose container `members` contains `internal`.
    /// `None` for ungrouped sessions and unknown names.
    pub fn section_of(&self, internal: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|s| {
                s.members
                    .iter()
                    .any(|c| c.members.iter().any(|m| m == internal))
            })
            .map(|s| s.name.as_str())
    }

    /// Flatten the model into an ordered list of visible entries.
    /// Members of collapsed sections are skipped — only the header row
    /// is emitted for those.
    pub fn visible(&self) -> Vec<VisibleEntry<'_>> {
        let mut out = Vec::with_capacity(self.len());
        for c in &self.ungrouped {
            out.push(VisibleEntry::Ungrouped(c));
        }
        for s in &self.sections {
            out.push(VisibleEntry::SectionHeader(s));
            if !s.collapsed {
                for c in &s.members {
                    out.push(VisibleEntry::Member {
                        section: s,
                        container: c,
                    });
                }
            }
        }
        out
    }

    /// Resolve a flattened index to a mutable location in the model.
    /// Returns `None` if `idx` is out of range.
    pub fn locate(&self, idx: usize) -> Option<Location> {
        if idx < self.ungrouped.len() {
            return Some(Location::Ungrouped(idx));
        }
        let mut cursor = self.ungrouped.len();
        for (si, sec) in self.sections.iter().enumerate() {
            if idx == cursor {
                return Some(Location::Header(si));
            }
            let next = cursor + 1 + Self::visible_member_count(sec);
            if idx < next {
                return Some(Location::Member(si, idx - cursor - 1));
            }
            cursor = next;
        }
        None
    }

    /// Convert a location back into a flattened index. Saturates to
    /// `len()` if the location is out of bounds. A collapsed section's
    /// members aren't visible — `Member(si, _)` returns the header's
    /// index in that case.
    pub fn flat_index(&self, loc: Location) -> usize {
        match loc {
            Location::Ungrouped(i) => i.min(self.ungrouped.len()),
            Location::Header(si) => {
                let mut idx = self.ungrouped.len();
                let bound = si.min(self.sections.len());
                for s in &self.sections[..bound] {
                    idx += 1 + Self::visible_member_count(s);
                }
                idx
            }
            Location::Member(si, mi) => {
                if si >= self.sections.len() {
                    return self.len();
                }
                let mut idx = self.ungrouped.len();
                for s in &self.sections[..si] {
                    idx += 1 + Self::visible_member_count(s);
                }
                let header = idx;
                if self.sections[si].collapsed {
                    return header;
                }
                header + 1 + mi.min(self.sections[si].members.len())
            }
        }
    }

    /// Find an entry by identity. Accepts:
    /// - a section id (`sec-…`) → header row,
    /// - a container id (`con-…`) → container row,
    /// - or an internal tmux name → the container that holds it
    ///   as one of its tabs.
    ///
    /// The three id namespaces don't overlap in practice (each has
    /// its own prefix; internal names use the bosun- prefix from
    /// `Config::session_prefix`). Section ids beat container ids
    /// beat internal-name lookup if a collision ever did happen.
    pub fn find_identity(&self, ident: &str) -> Option<usize> {
        for (si, s) in self.sections.iter().enumerate() {
            if s.id == ident {
                return Some(self.flat_index(Location::Header(si)));
            }
        }
        for (i, c) in self.ungrouped.iter().enumerate() {
            if c.id == ident {
                return Some(self.flat_index(Location::Ungrouped(i)));
            }
        }
        for (si, s) in self.sections.iter().enumerate() {
            for (mi, c) in s.members.iter().enumerate() {
                if c.id == ident {
                    return Some(self.flat_index(Location::Member(si, mi)));
                }
            }
        }
        // Internal-name fallback: find the container that owns it.
        for (i, c) in self.ungrouped.iter().enumerate() {
            if c.contains_internal(ident) {
                return Some(self.flat_index(Location::Ungrouped(i)));
            }
        }
        for (si, s) in self.sections.iter().enumerate() {
            for (mi, c) in s.members.iter().enumerate() {
                if c.contains_internal(ident) {
                    return Some(self.flat_index(Location::Member(si, mi)));
                }
            }
        }
        None
    }

    /// Reconcile against the current live set of tmux session names.
    /// - Dedupes tabs that somehow appear in multiple containers
    ///   (keeps the first occurrence in visible order).
    /// - Appends any live session not already present in any
    ///   container to `ungrouped` as a fresh single-tab container.
    ///
    /// Sections are preserved even if they end up empty.
    ///
    /// **Dead sessions are NOT auto-removed.** Containers and their
    /// tabs are dropped only when the user explicitly deletes them
    /// via `remove_session` — otherwise a tmux server restart (or
    /// reboot) would wipe the entire sidebar, losing the user's
    /// grouping and ordering work. Dead containers render as
    /// "missing" rows; the user can recreate them from the recents
    /// store or `d` to remove.
    /// Drop every member for which `should_remove` returns true, then
    /// drop any container left with no tabs. Returns whether anything
    /// changed. Used only when `remove_dead_sessions` is enabled —
    /// `reconcile` deliberately keeps rows whose session has gone so
    /// `R` can restart them.
    pub fn prune_members(&mut self, should_remove: &dyn Fn(&str) -> bool) -> bool {
        let mut changed = false;
        let mut drop_from = |members: &mut Vec<String>, active: &mut String| {
            let before = members.len();
            members.retain(|m| !should_remove(m));
            if members.len() != before {
                changed = true;
            }
            if !members.iter().any(|m| m == active) {
                if let Some(first) = members.first() {
                    *active = first.clone();
                }
            }
        };
        for c in &mut self.ungrouped {
            drop_from(&mut c.members, &mut c.active);
        }
        for sec in &mut self.sections {
            for c in &mut sec.members {
                drop_from(&mut c.members, &mut c.active);
            }
        }
        let before = self.ungrouped.len();
        self.ungrouped.retain(|c| !c.members.is_empty());
        if self.ungrouped.len() != before {
            changed = true;
        }
        for sec in &mut self.sections {
            let before = sec.members.len();
            sec.members.retain(|c| !c.members.is_empty());
            if sec.members.len() != before {
                changed = true;
            }
        }
        changed
    }

    pub fn reconcile(&mut self, live: &[(String, Option<String>)]) -> bool {
        let mut changed = false;
        let mut seen = std::collections::HashSet::new();
        for c in &mut self.ungrouped {
            let before = c.members.len();
            c.members.retain(|m| seen.insert(m.clone()));
            if c.members.len() != before {
                changed = true;
            }
            if !c.members.iter().any(|m| m == &c.active) {
                if let Some(first) = c.members.first() {
                    c.active = first.clone();
                    changed = true;
                }
            }
        }
        // Drop ungrouped containers whose last tab was deduped
        // away — a container with zero tabs has no sidebar
        // identity left to render.
        let before = self.ungrouped.len();
        self.ungrouped.retain(|c| !c.members.is_empty());
        if self.ungrouped.len() != before {
            changed = true;
        }
        for s in &mut self.sections {
            for c in &mut s.members {
                let before = c.members.len();
                c.members.retain(|m| seen.insert(m.clone()));
                if c.members.len() != before {
                    changed = true;
                }
                if !c.members.iter().any(|m| m == &c.active) {
                    if let Some(first) = c.members.first() {
                        c.active = first.clone();
                        changed = true;
                    }
                }
            }
            let before = s.members.len();
            s.members.retain(|c| !c.members.is_empty());
            if s.members.len() != before {
                changed = true;
            }
        }
        // For every new live session: if its `@bosun_container_id`
        // matches an existing container, join it as a tab; if it
        // matches no container but carries an id, create a new
        // container with that id (so the next refresh recognizes
        // siblings); else fall back to a fresh anonymous
        // single-tab container.
        for (n, cid) in live {
            if seen.contains(n) {
                continue;
            }
            let joined = match cid {
                Some(id) => self.add_tab_to_container(id, n.clone()),
                None => false,
            };
            if !joined {
                let container = match cid {
                    Some(id) => Container::with_id(id.clone(), n.clone(), n.clone()),
                    None => Container::single(n.clone(), n.clone()),
                };
                self.ungrouped.push(container);
            }
            seen.insert(n.clone());
            changed = true;
        }
        changed
    }

    /// Append `internal` as a new tab on the container identified
    /// by `container_id`. Returns true if a container was found.
    /// Switches the container's active tab to the new one — adding
    /// a tab is normally followed by the user wanting to look at
    /// it.
    pub fn add_tab_to_container(&mut self, container_id: &str, internal: String) -> bool {
        for c in &mut self.ungrouped {
            if c.id == container_id {
                c.add_tab(internal);
                return true;
            }
        }
        for s in &mut self.sections {
            for c in &mut s.members {
                if c.id == container_id {
                    c.add_tab(internal);
                    return true;
                }
            }
        }
        false
    }

    /// Switch the active tab on the container identified by
    /// `container_id`. Returns true if both container and tab were
    /// found. No-op if `internal` isn't a tab on that container.
    pub fn set_active_tab(&mut self, container_id: &str, internal: &str) -> bool {
        for c in &mut self.ungrouped {
            if c.id == container_id {
                if c.contains_internal(internal) {
                    c.active = internal.to_string();
                    return true;
                }
                return false;
            }
        }
        for s in &mut self.sections {
            for c in &mut s.members {
                if c.id == container_id {
                    if c.contains_internal(internal) {
                        c.active = internal.to_string();
                        return true;
                    }
                    return false;
                }
            }
        }
        false
    }

    /// Look up a container by id.
    pub fn find_container(&self, container_id: &str) -> Option<&Container> {
        for c in &self.ungrouped {
            if c.id == container_id {
                return Some(c);
            }
        }
        for s in &self.sections {
            for c in &s.members {
                if c.id == container_id {
                    return Some(c);
                }
            }
        }
        None
    }

    /// The container holding a tab.
    pub fn container_of(&self, internal: &str) -> Option<&Container> {
        self.ungrouped
            .iter()
            .chain(self.sections.iter().flat_map(|s| s.members.iter()))
            .find(|c| c.contains_internal(internal))
    }

    /// Replace one internal name with another, in place. Used by
    /// the restart flow so a kill+recreate doesn't leave a dead
    /// row above the freshly-created session — the row's slot
    /// (and section) is inherited by the new internal name.
    /// No-op if `old` isn't present or `new` is already present
    /// (the swap would create a duplicate). Returns true if the
    /// swap happened.
    pub fn replace_session(&mut self, old: &str, new: &str) -> bool {
        if old == new {
            return false;
        }
        self.ended_names.remove(old);
        if self.contains(new) {
            return false;
        }
        for c in &mut self.ungrouped {
            if let Some(slot) = c.members.iter_mut().find(|m| *m == old) {
                *slot = new.to_string();
                if c.active == old {
                    c.active = new.to_string();
                }
                return true;
            }
        }
        for s in &mut self.sections {
            for c in &mut s.members {
                if let Some(slot) = c.members.iter_mut().find(|m| *m == old) {
                    *slot = new.to_string();
                    if c.active == old {
                        c.active = new.to_string();
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Drop the names of ended tabs that are no longer in any row.
    pub fn prune_ended_names(&mut self) {
        let names = std::mem::take(&mut self.ended_names);
        self.ended_names = names
            .into_iter()
            .filter(|(tab, _)| self.contains(tab))
            .collect();
    }

    /// Name an ended tab (see [`SidebarModel::ended_names`]). False when
    /// no row has it or the name is unchanged.
    pub fn set_ended_name(&mut self, tab: &str, name: &str) -> bool {
        if !self.contains(tab) || self.ended_names.get(tab).is_some_and(|n| n == name) {
            return false;
        }
        self.ended_names.insert(tab.to_string(), name.to_string());
        true
    }

    fn contains(&self, internal: &str) -> bool {
        self.ungrouped.iter().any(|c| c.contains_internal(internal))
            || self
                .sections
                .iter()
                .any(|s| s.members.iter().any(|c| c.contains_internal(internal)))
    }

    /// Explicit removal of a tmux session from every container.
    /// Containers that lose their last tab are dropped from the
    /// sidebar entirely. Called only when the user kills a session
    /// via `d`, never from reconciliation — dead-but-grouped
    /// sessions survive a tmux restart / reboot.
    pub fn remove_session(&mut self, internal: &str) {
        for c in &mut self.ungrouped {
            c.members.retain(|m| m != internal);
            if c.active == internal {
                c.active = c.members.first().cloned().unwrap_or_default();
            }
        }
        self.ungrouped.retain(|c| !c.members.is_empty());
        for s in &mut self.sections {
            for c in &mut s.members {
                c.members.retain(|m| m != internal);
                if c.active == internal {
                    c.active = c.members.first().cloned().unwrap_or_default();
                }
            }
            s.members.retain(|c| !c.members.is_empty());
        }
    }

    /// Where the container `container_id` sits: its bucket (`None` for
    /// ungrouped, `Some(si)` for `sections[si]`) and its index there.
    fn container_slot(&self, container_id: &str) -> Option<(Option<usize>, usize)> {
        if let Some(i) = self.ungrouped.iter().position(|c| c.id == container_id) {
            return Some((None, i));
        }
        self.sections.iter().enumerate().find_map(|(si, s)| {
            s.members
                .iter()
                .position(|c| c.id == container_id)
                .map(|mi| (Some(si), mi))
        })
    }

    /// The slot of the container holding the tab `internal`.
    fn slot_of_tab(&self, internal: &str) -> Option<(Option<usize>, usize)> {
        if let Some(i) = self
            .ungrouped
            .iter()
            .position(|c| c.contains_internal(internal))
        {
            return Some((None, i));
        }
        self.sections.iter().enumerate().find_map(|(si, s)| {
            s.members
                .iter()
                .position(|c| c.contains_internal(internal))
                .map(|mi| (Some(si), mi))
        })
    }

    fn bucket_mut(&mut self, bucket: Option<usize>) -> &mut Vec<Container> {
        match bucket {
            None => &mut self.ungrouped,
            Some(si) => &mut self.sections[si].members,
        }
    }

    fn container_at_mut(&mut self, (bucket, i): (Option<usize>, usize)) -> &mut Container {
        &mut self.bucket_mut(bucket)[i]
    }

    /// Take the tab `internal` out of the container at `slot`. When it
    /// was the active tab, the tab that takes its place becomes active
    /// (the one after it, or the one before when it was last), the way
    /// closing a tab works.
    fn take_tab(&mut self, slot: (Option<usize>, usize), internal: &str) {
        let c = self.container_at_mut(slot);
        let Some(pos) = c.members.iter().position(|m| m == internal) else {
            return;
        };
        c.members.remove(pos);
        if c.active == internal {
            let next = pos.min(c.members.len().saturating_sub(1));
            c.active = c.members.get(next).cloned().unwrap_or_default();
        }
    }

    /// Drop the container `container_id` if it has no tabs left.
    fn drop_if_empty(&mut self, container_id: &str) {
        if let Some((bucket, i)) = self.container_slot(container_id) {
            let b = self.bucket_mut(bucket);
            if b[i].members.is_empty() {
                b.remove(i);
            }
        }
    }

    /// Move the tab `internal` out of its row.
    ///
    /// - `target: Some(id)` makes it a tab of that row, at `index`
    ///   among its tabs (clamped; appended when `None`). The target's
    ///   active tab stays as it was.
    /// - `target: None` gives it a new row of its own, placed right
    ///   after the row it came from (in the same section).
    ///
    /// A row left with no tabs is removed. Returns the id of the row
    /// the tab is now in, or `None` when nothing changed: the target is
    /// the row it's already in, or it's asked for a row of its own and
    /// already is one.
    pub fn move_tab(
        &mut self,
        internal: &str,
        target: Option<&str>,
        index: Option<usize>,
    ) -> Result<Option<String>, MoveError> {
        let from = self
            .slot_of_tab(internal)
            .ok_or_else(|| MoveError::UnknownSession(internal.to_string()))?;
        let from_id = self.container_at_mut(from).id.clone();
        match target {
            Some(target_id) => {
                let to = self
                    .container_slot(target_id)
                    .ok_or_else(|| MoveError::UnknownContainer(target_id.to_string()))?;
                if to == from {
                    return Ok(None);
                }
                self.take_tab(from, internal);
                // Taking a tab never removes a container, so `to` is
                // still valid here.
                let t = self.container_at_mut(to);
                let at = index.unwrap_or(t.members.len()).min(t.members.len());
                t.members.insert(at, internal.to_string());
                self.drop_if_empty(&from_id);
                Ok(Some(target_id.to_string()))
            }
            None => {
                if self.container_at_mut(from).members.len() == 1 {
                    return Ok(None);
                }
                self.take_tab(from, internal);
                let row = Container::single(internal.to_string(), internal.to_string());
                let id = row.id.clone();
                let (bucket, i) = from;
                self.bucket_mut(bucket).insert(i + 1, row);
                Ok(Some(id))
            }
        }
    }

    /// Move the tab `internal` out of its row into a new row of its
    /// own at `index` (clamped) in section `section`, or among the
    /// ungrouped rows with `None`. Returns the new row's id, or `None`
    /// when nothing changed: the tab is already a row of its own.
    pub fn move_tab_to_new_row(
        &mut self,
        internal: &str,
        section: Option<&str>,
        index: usize,
    ) -> Result<Option<String>, MoveError> {
        let from = self
            .slot_of_tab(internal)
            .ok_or_else(|| MoveError::UnknownSession(internal.to_string()))?;
        let bucket = match section {
            None => None,
            Some(sid) => Some(
                self.sections
                    .iter()
                    .position(|s| s.id == sid)
                    .ok_or_else(|| MoveError::UnknownSection(sid.to_string()))?,
            ),
        };
        if self.container_at_mut(from).members.len() == 1 {
            return Ok(None);
        }
        // The old row keeps a tab, so it stays and `index` still
        // counts the rows the client saw.
        self.take_tab(from, internal);
        let row = Container::single(internal.to_string(), internal.to_string());
        let id = row.id.clone();
        let list = self.bucket_mut(bucket);
        let at = index.min(list.len());
        list.insert(at, row);
        Ok(Some(id))
    }

    /// Move the row holding the tab `internal` to the end of section
    /// `section`, as a new session created into a section is placed.
    /// Returns the row's id, or `None` when it's already in that
    /// section.
    pub fn move_row_to_section_end(
        &mut self,
        internal: &str,
        section: &str,
    ) -> Result<Option<String>, MoveError> {
        let (bucket, i) = self
            .slot_of_tab(internal)
            .ok_or_else(|| MoveError::UnknownSession(internal.to_string()))?;
        let si = self
            .sections
            .iter()
            .position(|s| s.id == section)
            .ok_or_else(|| MoveError::UnknownSection(section.to_string()))?;
        if bucket == Some(si) {
            return Ok(None);
        }
        let row = self.bucket_mut(bucket).remove(i);
        let id = row.id.clone();
        self.sections[si].members.push(row);
        Ok(Some(id))
    }

    /// Move every tab of row `from` into row `into`, in order, at
    /// `index` among its tabs (clamped; after its existing tabs when
    /// `None`), and remove `from`. `into`'s active tab stays as it
    /// was. Returns the tabs that moved (empty when `from` and `into`
    /// are the same row).
    pub fn merge_rows(
        &mut self,
        from: &str,
        into: &str,
        index: Option<usize>,
    ) -> Result<Vec<String>, MoveError> {
        let src = self
            .container_slot(from)
            .ok_or_else(|| MoveError::UnknownContainer(from.to_string()))?;
        let dst = self
            .container_slot(into)
            .ok_or_else(|| MoveError::UnknownContainer(into.to_string()))?;
        if src == dst {
            return Ok(Vec::new());
        }
        let moved = std::mem::take(&mut self.container_at_mut(src).members);
        let t = self.container_at_mut(dst);
        let mut at = index.unwrap_or(t.members.len()).min(t.members.len());
        for m in &moved {
            if !t.contains_internal(m) {
                t.members.insert(at, m.clone());
                at += 1;
            }
        }
        self.drop_if_empty(from);
        Ok(moved)
    }

    /// The index of the `role` section, made (collapsed, after the
    /// user's own sections, Snoozed before Docked) when there isn't one.
    fn role_section(&mut self, role: SectionRole) -> usize {
        if let Some(i) = self.sections.iter().position(|s| s.role == Some(role)) {
            return i;
        }
        let mut sec = Section::new(role.title());
        sec.role = Some(role);
        sec.collapsed = true;
        let at = self
            .sections
            .iter()
            .position(|s| s.role.is_some_and(|r| r > role))
            .unwrap_or(self.sections.len());
        self.sections.insert(at, sec);
        at
    }

    /// The role of the section a row is in, if it's parked there.
    pub fn role_of(&self, container_id: &str) -> Option<SectionRole> {
        match self.container_slot(container_id)? {
            (Some(si), _) => self.sections[si].role,
            (None, _) => None,
        }
    }

    /// Move row `container_id` into the `role` section (at its end),
    /// remembering where it came from. A row already parked keeps its
    /// origin, so snoozing a docked row and waking it still takes it
    /// home; `wake_at` and `until_needed` are replaced. Returns the
    /// role it was in before, if any.
    pub fn park(
        &mut self,
        container_id: &str,
        role: SectionRole,
        wake_at: Option<u64>,
        until_needed: bool,
        now: u64,
    ) -> Result<Option<SectionRole>, MoveError> {
        let (bucket, i) = self
            .container_slot(container_id)
            .ok_or_else(|| MoveError::UnknownContainer(container_id.to_string()))?;
        let was = bucket.and_then(|si| self.sections[si].role);
        let mut row = self.bucket_mut(bucket).remove(i);
        let mut parked = match (was, row.parked.take()) {
            (Some(_), Some(p)) => p,
            _ => Parked {
                at: now,
                from_section: bucket.map(|si| self.sections[si].id.clone()),
                from_index: i,
                ..Parked::default()
            },
        };
        parked.wake_at = wake_at;
        parked.until_needed = until_needed;
        row.parked = Some(parked);
        let si = self.role_section(role);
        let list = &mut self.sections[si].members;
        // Re-parking in the same section keeps its place.
        let at = if was == Some(role) {
            i.min(list.len())
        } else {
            list.len()
        };
        list.insert(at, row);
        self.drop_empty_role_sections();
        Ok(was)
    }

    /// Move a parked row back where it came from (its section at its old
    /// index, clamped; the ungrouped rows when that section is gone).
    /// Returns the role it was parked in, or `None` when it wasn't.
    pub fn unpark(&mut self, container_id: &str) -> Result<Option<SectionRole>, MoveError> {
        let (bucket, i) = self
            .container_slot(container_id)
            .ok_or_else(|| MoveError::UnknownContainer(container_id.to_string()))?;
        let Some(role) = bucket.and_then(|si| self.sections[si].role) else {
            return Ok(None);
        };
        let mut row = self.bucket_mut(bucket).remove(i);
        let parked = row.parked.take().unwrap_or_default();
        let home = parked.from_section.as_deref().and_then(|id| {
            self.sections
                .iter()
                .position(|s| s.id == id && s.role.is_none())
        });
        let list = self.bucket_mut(home);
        let at = parked.from_index.min(list.len());
        list.insert(at, row);
        self.drop_empty_role_sections();
        Ok(Some(role))
    }

    fn drop_empty_role_sections(&mut self) {
        self.sections
            .retain(|s| s.role.is_none() || !s.members.is_empty());
    }

    /// Snoozed rows due to wake at `now` (Unix ms): `wake_at` has come,
    /// or `until_needed` and `needs_you` says one of its tabs needs you
    /// (it's given the row, the tab and the time the row was parked).
    pub fn due_to_wake(
        &self,
        now: u64,
        needs_you: impl Fn(&str, &str, u64) -> bool,
    ) -> Vec<String> {
        self.snoozed()
            .filter(|c| {
                let Some(p) = &c.parked else { return false };
                p.wake_at.is_some_and(|t| t <= now)
                    || (p.until_needed && c.members.iter().any(|m| needs_you(&c.id, m, p.at)))
            })
            .map(|c| c.id.clone())
            .collect()
    }

    /// The rows in the Snoozed section.
    pub fn snoozed(&self) -> impl Iterator<Item = &Container> {
        self.sections
            .iter()
            .filter(|s| s.role == Some(SectionRole::Snoozed))
            .flat_map(|s| s.members.iter())
    }

    /// Bring a new layout in line with this one, the model it
    /// replaces: a row moved into a role section is parked there
    /// (remembering where it was in `self`), one moved out is unparked,
    /// and one that stayed keeps its parking. A client's layout carries
    /// no roles or defaults, so with `from_client` sections keep this
    /// model's; a `config.toml` edited elsewhere (the TUI) has its own.
    /// Returns each row whose role changed, with its old and new role.
    pub fn adopt(
        &self,
        new: &mut SidebarModel,
        now: u64,
        from_client: bool,
    ) -> Vec<(String, Option<SectionRole>, Option<SectionRole>)> {
        if from_client {
            for s in &mut new.sections {
                if let Some(old) = self.sections.iter().find(|o| o.id == s.id) {
                    s.role = old.role;
                    s.defaults = old.defaults.clone();
                }
            }
        }
        let mut changed = Vec::new();
        let roles: Vec<Option<SectionRole>> = new.sections.iter().map(|s| s.role).collect();
        let rows = new.ungrouped.iter_mut().map(|c| (None, c)).chain(
            new.sections
                .iter_mut()
                .enumerate()
                .flat_map(|(si, s)| s.members.iter_mut().map(move |c| (Some(si), c))),
        );
        for (si, c) in rows {
            let role = si.and_then(|si| roles[si]);
            let old_slot = self.container_slot(&c.id);
            let old_role = old_slot.and_then(|(b, _)| b.and_then(|si| self.sections[si].role));
            let old_parked = old_slot.and_then(|slot| self.container_at(slot).parked.clone());
            c.parked = match role {
                None => None,
                Some(_) if old_role.is_some() => old_parked,
                Some(_) => Some(Parked {
                    at: now,
                    from_section: old_slot
                        .and_then(|(b, _)| b.map(|si| self.sections[si].id.clone())),
                    from_index: old_slot.map_or(0, |(_, i)| i),
                    until_needed: role == Some(SectionRole::Snoozed),
                    ..Parked::default()
                }),
            };
            if role != old_role && old_slot.is_some() {
                changed.push((c.id.clone(), old_role, role));
            }
        }
        // Ended tabs' names aren't in a client's layout (or in an older
        // copy of the file): keep ours, for the tabs that are still
        // there.
        for (tab, name) in &self.ended_names {
            new.ended_names.insert(tab.clone(), name.clone());
        }
        new.prune_ended_names();
        new.drop_empty_role_sections();
        // Role sections stay after the user's own, Snoozed first.
        new.sections
            .sort_by_key(|s| s.role.map_or(0, |r| 1 + r as u8));
        changed
    }

    fn container_at(&self, (bucket, i): (Option<usize>, usize)) -> &Container {
        match bucket {
            None => &self.ungrouped[i],
            Some(si) => &self.sections[si].members[i],
        }
    }

    /// Append a new empty section at the end of the sections list.
    /// Returns the new section's id.
    pub fn insert_section_at_end(&mut self, name: String) -> String {
        let s = Section::new(name);
        let id = s.id.clone();
        self.sections.push(s);
        id
    }

    /// Rename a section by id. Returns true if found.
    pub fn rename_section(&mut self, id: &str, new_name: String) -> bool {
        for s in &mut self.sections {
            if s.id == id {
                s.name = new_name;
                return true;
            }
        }
        false
    }

    /// Delete a section by its sections-index. Members are appended
    /// to `ungrouped` in their current order.
    pub fn delete_section_at(&mut self, si: usize) {
        if si >= self.sections.len() {
            return;
        }
        let mut sec = self.sections.remove(si);
        for c in &mut sec.members {
            c.parked = None;
        }
        self.ungrouped.append(&mut sec.members);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn con(internal: &str) -> Container {
        Container::single(internal.to_string(), internal.to_string())
    }

    fn sec(id: &str, name: &str, members: &[&str]) -> Section {
        Section {
            id: id.into(),
            name: name.into(),
            members: members.iter().map(|s| con(s)).collect(),
            collapsed: false,
            banner_font: None,
            defaults: None,
            role: None,
        }
    }

    fn model(ungrouped: &[&str], sections: Vec<Section>) -> SidebarModel {
        SidebarModel {
            ungrouped: ungrouped.iter().map(|s| con(s)).collect(),
            sections,
            ended_names: Default::default(),
        }
    }

    fn ungrouped_names(m: &SidebarModel) -> Vec<String> {
        m.ungrouped.iter().map(|c| c.active.clone()).collect()
    }

    fn section_names(s: &Section) -> Vec<String> {
        s.members.iter().map(|c| c.active.clone()).collect()
    }

    /// Build the `live` list shape reconcile expects from a flat
    /// slice of internal names — no container_id, matching pre-
    /// tabs behavior. Tests that exercise container_id grouping
    /// build the tuple list inline.
    fn live(names: &[&str]) -> Vec<(String, Option<String>)> {
        names.iter().map(|n| (n.to_string(), None)).collect()
    }

    #[test]
    fn flat_index_matches_visible_iteration() {
        let m = model(
            &["a", "b"],
            vec![sec("g1", "Work", &["c"]), sec("g2", "Play", &["d", "e"])],
        );
        let visible = m.visible();
        assert_eq!(visible.len(), m.len());
        for i in 0..m.len() {
            let loc = m.locate(i).expect("locate");
            assert_eq!(m.flat_index(loc), i, "round-trip failed at {}", i);
        }
    }

    #[test]
    fn locate_covers_all_zones() {
        let m = model(&["a", "b"], vec![sec("g1", "W", &["c"])]);
        assert!(matches!(m.locate(0), Some(Location::Ungrouped(0))));
        assert!(matches!(m.locate(1), Some(Location::Ungrouped(1))));
        assert!(matches!(m.locate(2), Some(Location::Header(0))));
        assert!(matches!(m.locate(3), Some(Location::Member(0, 0))));
        assert!(m.locate(4).is_none());
    }

    #[test]
    fn prune_members_drops_only_what_it_is_told_to() {
        let mut m = model(&["a", "gone"], vec![sec("g1", "W", &["b", "dead"])]);
        let remove = |n: &str| n == "gone" || n == "dead";
        assert!(m.prune_members(&remove));
        assert_eq!(ungrouped_names(&m), vec!["a".to_string()]);
        assert_eq!(section_names(&m.sections[0]), vec!["b".to_string()]);
    }

    #[test]
    fn prune_members_reports_no_change_when_nothing_matches() {
        let mut m = model(&["a"], vec![sec("g1", "W", &["b"])]);
        assert!(!m.prune_members(&|_| false));
        assert_eq!(ungrouped_names(&m), vec!["a".to_string()]);
    }

    /// A container empties out when its last tab is pruned; an empty
    /// container has nothing left to render, so it goes too.
    #[test]
    fn prune_members_drops_containers_left_empty() {
        let mut m = model(&["solo"], vec![sec("g1", "W", &["only"])]);
        assert!(m.prune_members(&|_| true));
        assert!(ungrouped_names(&m).is_empty());
        assert!(
            m.sections[0].members.is_empty(),
            "section keeps no empty containers"
        );
    }

    #[test]
    fn reconcile_keeps_dead_sessions_appends_new() {
        let mut m = model(&["a"], vec![sec("g1", "W", &["b", "gone"])]);
        m.reconcile(&live(&["a", "b", "newbie"]));
        // "gone" stays in the section even though it's not live;
        // "newbie" lands in ungrouped as a fresh single-tab container.
        assert_eq!(
            ungrouped_names(&m),
            vec!["a".to_string(), "newbie".to_string()]
        );
        assert_eq!(
            section_names(&m.sections[0]),
            vec!["b".to_string(), "gone".to_string()]
        );
    }

    #[test]
    fn reconcile_empty_live_preserves_everything() {
        let mut m = model(
            &["alpha", "beta"],
            vec![sec("g1", "Work", &["gamma", "delta"])],
        );
        m.reconcile(&[]);
        assert_eq!(
            ungrouped_names(&m),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(
            section_names(&m.sections[0]),
            vec!["gamma".to_string(), "delta".to_string()]
        );
    }

    #[test]
    fn reconcile_dedupes_across_buckets() {
        // Two separate single-tab containers both holding "b" —
        // would happen only via hand-edited config, but reconcile
        // must collapse cleanly.
        let mut m = model(&["a", "b"], vec![sec("g1", "W", &["b", "c"])]);
        m.reconcile(&live(&["a", "b", "c"]));
        assert_eq!(ungrouped_names(&m), vec!["a".to_string(), "b".to_string()]);
        // "b" stayed in ungrouped (visible-order earliest); the
        // section's "b" container loses its only tab and is
        // dropped, leaving just "c".
        assert_eq!(section_names(&m.sections[0]), vec!["c".to_string()]);
    }

    #[test]
    fn reconcile_groups_new_session_by_container_id() {
        // Two existing single-tab containers in ungrouped; a third
        // live session arrives advertising the second container's id.
        // It should join that container as a tab, not get its own row.
        let mut m = model(&["a", "b"], vec![]);
        let target_id = m.ungrouped[1].id.clone();
        m.reconcile(&[
            ("a".into(), None),
            ("b".into(), None),
            ("b2".into(), Some(target_id.clone())),
        ]);
        assert_eq!(m.ungrouped.len(), 2);
        assert_eq!(m.ungrouped[1].id, target_id);
        assert_eq!(
            m.ungrouped[1].members,
            vec!["b".to_string(), "b2".to_string()]
        );
        // add_tab switches the active tab — opening the modal to
        // create a new tab implies the user wants to see it.
        assert_eq!(m.ungrouped[1].active, "b2");
    }

    #[test]
    fn reconcile_unknown_container_id_creates_keyed_container() {
        // First sighting of a session whose container_id refers to
        // no existing container — create a fresh container *with*
        // that id so the next refresh recognizes siblings.
        let mut m = model(&[], vec![]);
        m.reconcile(&[("new".into(), Some("con-external".into()))]);
        assert_eq!(m.ungrouped.len(), 1);
        assert_eq!(m.ungrouped[0].id, "con-external");
        assert_eq!(m.ungrouped[0].active, "new");
    }

    #[test]
    fn remove_session_drops_from_both_buckets() {
        let mut m = model(
            &["alpha", "beta"],
            vec![sec("g1", "W", &["gamma", "delta"])],
        );
        m.remove_session("alpha");
        m.remove_session("gamma");
        assert_eq!(ungrouped_names(&m), vec!["beta".to_string()]);
        assert_eq!(section_names(&m.sections[0]), vec!["delta".to_string()]);
    }

    #[test]
    fn delete_section_moves_members_to_ungrouped() {
        let mut m = model(&["a"], vec![sec("g1", "W", &["b", "c"])]);
        m.delete_section_at(0);
        assert_eq!(
            ungrouped_names(&m),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(m.sections.is_empty());
    }

    #[test]
    fn find_identity_matches_section_container_and_internal() {
        let m = model(&["a"], vec![sec("g1", "W", &["b"])]);
        // Section id.
        assert_eq!(m.find_identity("g1"), Some(1));
        // Container id of the ungrouped row.
        let con_a = &m.ungrouped[0];
        assert_eq!(m.find_identity(&con_a.id), Some(0));
        // Internal tmux name fallback.
        assert_eq!(m.find_identity("a"), Some(0));
        assert_eq!(m.find_identity("b"), Some(2));
        assert!(m.find_identity("nope").is_none());
    }

    #[test]
    fn ended_names_follow_their_tabs() {
        let mut m = model(&["a", "b"], vec![]);
        assert!(m.set_ended_name("a", "Alpha"));
        assert!(!m.set_ended_name("a", "Alpha"), "unchanged");
        assert!(!m.set_ended_name("nope", "x"), "not in any row");
        // A client's layout has no names: adopting it keeps ours, for
        // the tabs it still has.
        let mut new = model(&["a"], vec![]);
        new.ungrouped[0].id = m.ungrouped[0].id.clone();
        m.ended_names.insert("b".into(), "Beta".into());
        m.adopt(&mut new, 1, true);
        assert_eq!(
            new.ended_names,
            BTreeMap::from([("a".to_string(), "Alpha".to_string())])
        );
        // Resumed into a new session: the name now lives on that one.
        assert!(new.replace_session("a", "a2"));
        assert!(new.ended_names.is_empty());
        // Saved with the sidebar.
        m.prune_ended_names();
        let back: SidebarModel = toml::from_str(&toml::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.ended_names.get("a").map(String::as_str), Some("Alpha"));
    }

    #[test]
    fn replace_session_updates_active_too() {
        let mut m = model(&["old"], vec![]);
        assert!(m.replace_session("old", "new"));
        assert_eq!(m.ungrouped[0].members, vec!["new".to_string()]);
        assert_eq!(m.ungrouped[0].active, "new");
    }

    #[test]
    fn roundtrip_toml_new_shape() {
        let m = model(
            &["bosun-alpha"],
            vec![
                sec("g1", "Premium", &["bosun-beta", "bosun-gamma"]),
                sec("g2", "YetiDev", &[]),
            ],
        );
        let toml = toml::to_string(&m).expect("serialize");
        let parsed: SidebarModel = toml::from_str(&toml).expect("parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn legacy_string_shape_deserializes_into_containers() {
        // The exact on-disk shape an 0.x bosun would have written:
        // ungrouped is a flat string array, section.members likewise.
        let legacy = r#"
ungrouped = ["bosun-alpha", "bosun-beta"]

[[sections]]
id = "g1"
name = "Work"
members = ["bosun-gamma"]
"#;
        let parsed: SidebarModel = toml::from_str(legacy).expect("legacy parse");
        assert_eq!(parsed.ungrouped.len(), 2);
        assert_eq!(parsed.ungrouped[0].members, vec!["bosun-alpha".to_string()]);
        assert_eq!(parsed.ungrouped[0].active, "bosun-alpha");
        assert!(parsed.ungrouped[0].id.starts_with("con-"));
        assert_eq!(parsed.sections[0].members.len(), 1);
        assert_eq!(
            parsed.sections[0].members[0].members,
            vec!["bosun-gamma".to_string()]
        );

        // Re-serializing must emit the new table form so the next
        // load doesn't depend on the legacy compat path.
        let re_emitted = toml::to_string(&parsed).expect("serialize");
        assert!(
            re_emitted.contains("[[ungrouped]]"),
            "expected table form, got:\n{}",
            re_emitted
        );
    }

    /// A row with several tabs, `tabs[active]` active.
    fn row(tabs: &[&str], active: usize) -> Container {
        let mut c = con(tabs[0]);
        for t in &tabs[1..] {
            c.members.push(t.to_string());
        }
        c.active = tabs[active].to_string();
        c
    }

    fn tabs(c: &Container) -> Vec<&str> {
        c.members.iter().map(String::as_str).collect()
    }

    #[test]
    fn move_tab_into_another_row_keeps_the_targets_active_tab() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["a1", "a2"], 1), row(&["b1"], 0)],
            sections: vec![],
            ended_names: Default::default(),
        };
        let b = m.ungrouped[1].id.clone();
        assert_eq!(m.move_tab("a2", Some(&b), None), Ok(Some(b.clone())));
        assert_eq!(tabs(&m.ungrouped[0]), vec!["a1"]);
        assert_eq!(m.ungrouped[0].active, "a1", "source picks a new active tab");
        assert_eq!(tabs(&m.ungrouped[1]), vec!["b1", "a2"]);
        assert_eq!(m.ungrouped[1].active, "b1", "target's active tab stays");
    }

    #[test]
    fn move_tab_honours_and_clamps_the_index() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["a", "x", "y"], 0), row(&["b1", "b2"], 1)],
            sections: vec![],
            ended_names: Default::default(),
        };
        let b = m.ungrouped[1].id.clone();
        m.move_tab("x", Some(&b), Some(0)).unwrap();
        assert_eq!(tabs(&m.ungrouped[1]), vec!["x", "b1", "b2"]);
        m.move_tab("y", Some(&b), Some(99)).unwrap();
        assert_eq!(tabs(&m.ungrouped[1]), vec!["x", "b1", "b2", "y"]);
        assert_eq!(m.ungrouped[1].active, "b2");
    }

    #[test]
    fn move_tab_drops_a_row_left_empty_across_sections() {
        let mut m = SidebarModel {
            ungrouped: vec![con("solo")],
            sections: vec![sec("g1", "Work", &["w1", "w2"])],
            ended_names: Default::default(),
        };
        let w2 = m.sections[0].members[1].id.clone();
        assert_eq!(m.move_tab("solo", Some(&w2), None), Ok(Some(w2.clone())));
        assert!(m.ungrouped.is_empty(), "the emptied row is gone");
        assert_eq!(m.sections[0].members.len(), 2);
        assert_eq!(tabs(&m.sections[0].members[1]), vec!["w2", "solo"]);
    }

    #[test]
    fn move_tab_to_its_own_row_lands_right_after_the_old_one() {
        let mut m = SidebarModel {
            ungrouped: vec![con("top")],
            sections: vec![Section {
                members: vec![con("w0"), row(&["w1", "t2", "t3"], 1), con("w9")],
                ..sec("g1", "Work", &[])
            }],
            ended_names: Default::default(),
        };
        let new_id = m.move_tab("t2", None, None).unwrap().expect("moved");
        let members = &m.sections[0].members;
        assert_eq!(members.len(), 4);
        assert_eq!(tabs(&members[1]), vec!["w1", "t3"]);
        assert_eq!(members[1].active, "t3", "the next tab takes over");
        assert_eq!(members[2].id, new_id);
        assert_eq!(tabs(&members[2]), vec!["t2"]);
        assert_eq!(members[2].active, "t2");
        assert_eq!(tabs(&members[3]), vec!["w9"]);
        assert_eq!(m.ungrouped.len(), 1);
    }

    #[test]
    fn move_tab_to_new_row_places_it_where_asked() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["a1", "a2"], 1), con("b")],
            sections: vec![Section {
                members: vec![con("w1"), con("w2")],
                ..sec("g1", "Work", &[])
            }],
            ended_names: Default::default(),
        };
        let id = m.move_tab_to_new_row("a2", None, 0).unwrap().unwrap();
        assert_eq!(m.ungrouped[0].id, id);
        assert_eq!(tabs(&m.ungrouped[0]), vec!["a2"]);
        assert_eq!(tabs(&m.ungrouped[1]), vec!["a1"]);
        assert_eq!(
            m.ungrouped[1].active, "a1",
            "the old row's next tab takes over"
        );

        let mut m = SidebarModel {
            ungrouped: vec![row(&["a1", "a2"], 0)],
            sections: vec![Section {
                members: vec![con("w1"), con("w2")],
                ..sec("g1", "Work", &[])
            }],
            ended_names: Default::default(),
        };
        let sid = m.sections[0].id.clone();
        m.move_tab_to_new_row("a2", Some(&sid), 1).unwrap();
        let names: Vec<_> = m.sections[0].members.iter().map(|c| tabs(c)[0]).collect();
        assert_eq!(names, vec!["w1", "a2", "w2"]);
        assert_eq!(
            m.move_tab_to_new_row("a1", Some(&sid), 9),
            Ok(None),
            "a single tab is already its own row"
        );
        assert_eq!(m.ungrouped.len(), 1);
    }

    #[test]
    fn move_tab_to_new_row_errors() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["a1", "a2"], 0)],
            sections: vec![],
            ended_names: Default::default(),
        };
        let before = m.clone();
        assert_eq!(
            m.move_tab_to_new_row("nope", None, 0),
            Err(MoveError::UnknownSession("nope".into()))
        );
        assert_eq!(
            m.move_tab_to_new_row("a2", Some("sec-x"), 0),
            Err(MoveError::UnknownSection("sec-x".into()))
        );
        assert_eq!(m, before);
    }

    #[test]
    fn move_tab_no_ops_and_errors() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["a1", "a2"], 0), con("b")],
            sections: vec![],
            ended_names: Default::default(),
        };
        let before = m.clone();
        let a = m.ungrouped[0].id.clone();
        assert_eq!(m.move_tab("a2", Some(&a), Some(0)), Ok(None), "same row");
        assert_eq!(m.move_tab("b", None, None), Ok(None), "already its own row");
        assert_eq!(m, before);
        assert_eq!(
            m.move_tab("nope", Some(&a), None),
            Err(MoveError::UnknownSession("nope".into()))
        );
        assert_eq!(
            m.move_tab("a1", Some("con-missing"), None),
            Err(MoveError::UnknownContainer("con-missing".into()))
        );
        assert_eq!(m, before, "errors change nothing");
    }

    #[test]
    fn merge_rows_moves_every_tab_in_order_and_drops_the_source() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["k1", "k1b"], 1)],
            sections: vec![Section {
                members: vec![row(&["k2", "k2b"], 0)],
                ..sec("g1", "Work", &[])
            }],
            ended_names: Default::default(),
        };
        let from = m.ungrouped[0].id.clone();
        let into = m.sections[0].members[0].id.clone();
        assert_eq!(
            m.merge_rows(&from, &into, None),
            Ok(vec!["k1".to_string(), "k1b".to_string()])
        );
        assert!(m.ungrouped.is_empty());
        let target = &m.sections[0].members[0];
        assert_eq!(tabs(target), vec!["k2", "k2b", "k1", "k1b"]);
        assert_eq!(target.active, "k2");
        assert!(m.find_container(&from).is_none());
    }

    #[test]
    fn merge_rows_honours_and_clamps_the_index() {
        let mut m = SidebarModel {
            ungrouped: vec![row(&["k1", "k1b"], 0), row(&["k2", "k2b"], 1)],
            sections: vec![],
            ended_names: Default::default(),
        };
        let from = m.ungrouped[0].id.clone();
        let into = m.ungrouped[1].id.clone();
        m.merge_rows(&from, &into, Some(1)).unwrap();
        assert_eq!(tabs(&m.ungrouped[0]), vec!["k2", "k1", "k1b", "k2b"]);
        assert_eq!(m.ungrouped[0].active, "k2b");

        let mut m = SidebarModel {
            ungrouped: vec![con("a"), con("b")],
            sections: vec![],
            ended_names: Default::default(),
        };
        let a = m.ungrouped[0].id.clone();
        let b = m.ungrouped[1].id.clone();
        m.merge_rows(&b, &a, Some(9)).unwrap();
        assert_eq!(tabs(&m.ungrouped[0]), vec!["a", "b"]);
    }

    #[test]
    fn merge_rows_into_itself_or_unknown_rows() {
        let mut m = SidebarModel {
            ungrouped: vec![con("a"), con("b")],
            sections: vec![],
            ended_names: Default::default(),
        };
        let a = m.ungrouped[0].id.clone();
        let before = m.clone();
        assert_eq!(m.merge_rows(&a, &a, None), Ok(vec![]));
        assert_eq!(
            m.merge_rows("con-x", &a, None),
            Err(MoveError::UnknownContainer("con-x".into()))
        );
        assert_eq!(
            m.merge_rows(&a, "con-y", None),
            Err(MoveError::UnknownContainer("con-y".into()))
        );
        assert_eq!(m, before);
    }

    /// After a merge, a refresh that still sees the moved session's
    /// old `@bosun_container_id` keeps it where the model put it: the
    /// model wins for sessions it already holds.
    #[test]
    fn reconcile_keeps_a_merged_tab_where_the_model_put_it() {
        let mut m = SidebarModel {
            ungrouped: vec![con("a"), con("b")],
            sections: vec![],
            ended_names: Default::default(),
        };
        let a = m.ungrouped[0].id.clone();
        let b = m.ungrouped[1].id.clone();
        m.merge_rows(&b, &a, None).unwrap();
        m.reconcile(&[("a".into(), None), ("b".into(), Some(b.clone()))]);
        assert_eq!(m.ungrouped.len(), 1);
        assert_eq!(tabs(&m.ungrouped[0]), vec!["a", "b"]);
    }

    /// A session the model doesn't hold (a lost or fresh config.toml)
    /// joins the row its `@bosun_container_id` names, which is what
    /// `move_tab` stamps on it.
    #[test]
    fn reconcile_rebuilds_a_merged_row_from_the_container_option() {
        let mut m = SidebarModel::default();
        m.reconcile(&[
            ("a".into(), Some("con-merged".into())),
            ("b".into(), Some("con-merged".into())),
        ]);
        assert_eq!(m.ungrouped.len(), 1);
        assert_eq!(m.ungrouped[0].id, "con-merged");
        assert_eq!(tabs(&m.ungrouped[0]), vec!["a", "b"]);
    }

    #[test]
    fn section_of_finds_grouped_member_and_ignores_others() {
        let mut model = SidebarModel::default();
        // Ungrouped container: must NOT resolve to a section.
        model
            .ungrouped
            .push(Container::single("bosun-loose-aaaa".into(), "loose".into()));
        // Section with one container holding two tabs.
        let mut sec = Section::new("proj");
        let mut con = Container::single("bosun-alpha-bbbb".into(), "alpha".into());
        con.add_tab("bosun-beta-cccc".into());
        sec.members.push(con);
        model.sections.push(sec);

        assert_eq!(model.section_of("bosun-alpha-bbbb"), Some("proj"));
        assert_eq!(model.section_of("bosun-beta-cccc"), Some("proj"));
        assert_eq!(model.section_of("bosun-loose-aaaa"), None);
        assert_eq!(model.section_of("bosun-missing-zzzz"), None);
    }
    #[test]
    fn move_row_to_section_end_moves_the_whole_row() {
        let mut m = SidebarModel::default();
        m.ungrouped.push(Container::single("a".into(), "a".into()));
        let mut row = Container::single("b".into(), "b".into());
        row.add_tab("c".into());
        m.ungrouped.push(row);
        let sid = m.insert_section_at_end("work".into());
        m.sections[0]
            .members
            .push(Container::single("d".into(), "d".into()));

        let moved = m.move_row_to_section_end("c", &sid).unwrap();
        assert!(moved.is_some());
        assert_eq!(m.ungrouped.len(), 1);
        let names: Vec<_> = m.sections[0]
            .members
            .iter()
            .map(|c| c.members.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                vec!["d".to_string()],
                vec!["b".to_string(), "c".to_string()]
            ]
        );
        // Already there: nothing to do.
        assert_eq!(m.move_row_to_section_end("b", &sid).unwrap(), None);
        assert_eq!(
            m.move_row_to_section_end("a", "sec-nope"),
            Err(MoveError::UnknownSection("sec-nope".into()))
        );
        assert_eq!(
            m.move_row_to_section_end("zz", &sid),
            Err(MoveError::UnknownSession("zz".into()))
        );
    }
    fn parking_model() -> SidebarModel {
        let mut m = SidebarModel::default();
        m.ungrouped.push(con("a"));
        m.ungrouped.push(con("b"));
        let sid = m.insert_section_at_end("work".into());
        m.sections[0].members.push(con("c"));
        m.sections[0].members.push(con("d"));
        assert_eq!(m.sections[0].id, sid);
        m
    }

    fn row_id(m: &SidebarModel, tab: &str) -> String {
        let slot = m.slot_of_tab(tab).unwrap();
        m.container_at(slot).id.clone()
    }

    #[test]
    fn park_and_unpark_go_back_to_the_same_place() {
        let mut m = parking_model();
        let c = row_id(&m, "c");
        assert_eq!(
            m.park(&c, SectionRole::Docked, None, false, 7).unwrap(),
            None
        );
        assert_eq!(m.role_of(&c), Some(SectionRole::Docked));
        assert_eq!(m.sections.len(), 2);
        assert_eq!(m.sections[1].name, "Docked");
        assert!(m.sections[1].collapsed);
        let p = m.find_container(&c).unwrap().parked.clone().unwrap();
        assert_eq!(
            (p.at, p.from_section.as_deref(), p.from_index),
            (7, Some(m.sections[0].id.as_str()), 0)
        );
        // Snoozed sits before Docked; snoozing the docked row keeps its origin.
        let a = row_id(&m, "a");
        m.park(&a, SectionRole::Snoozed, Some(100), false, 8)
            .unwrap();
        assert_eq!(
            m.sections
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["work", "Snoozed", "Docked"]
        );
        assert_eq!(
            m.park(&c, SectionRole::Snoozed, None, true, 9).unwrap(),
            Some(SectionRole::Docked)
        );
        assert_eq!(m.sections.len(), 2, "the empty Docked section goes");
        let p = m.find_container(&c).unwrap().parked.clone().unwrap();
        assert_eq!(
            (p.at, p.from_index, p.until_needed, p.wake_at),
            (7, 0, true, None)
        );

        assert_eq!(m.unpark(&c).unwrap(), Some(SectionRole::Snoozed));
        assert_eq!(m.sections[0].members[0].members, vec!["c".to_string()]);
        assert_eq!(m.find_container(&c).unwrap().parked, None);
        assert_eq!(m.unpark(&a).unwrap(), Some(SectionRole::Snoozed));
        assert_eq!(m.ungrouped[0].members, vec!["a".to_string()]);
        assert_eq!(m.sections.len(), 1, "no role sections left");
        // Not parked: nothing to do.
        assert_eq!(m.unpark(&a).unwrap(), None);
    }

    #[test]
    fn unpark_into_a_removed_section_goes_to_the_ungrouped_rows() {
        let mut m = parking_model();
        let d = row_id(&m, "d");
        m.park(&d, SectionRole::Docked, None, false, 1).unwrap();
        m.delete_section_at(0);
        m.unpark(&d).unwrap();
        assert!(m.ungrouped.iter().any(|r| r.id == d));
    }

    #[test]
    fn due_to_wake_by_time_or_need() {
        let mut m = parking_model();
        let (a, b, c) = (row_id(&m, "a"), row_id(&m, "b"), row_id(&m, "c"));
        m.park(&a, SectionRole::Snoozed, Some(1_000), false, 0)
            .unwrap();
        m.park(&b, SectionRole::Snoozed, None, true, 0).unwrap();
        m.park(&c, SectionRole::Docked, Some(1), false, 0).unwrap();
        assert!(m.due_to_wake(999, |_, _, _| false).is_empty());
        assert_eq!(m.due_to_wake(1_000, |_, _, _| false), vec![a.clone()]);
        assert_eq!(m.due_to_wake(5, |_, t, _| t == "b"), vec![b]);
    }

    #[test]
    fn adopt_parks_rows_dragged_in_and_unparks_rows_dragged_out() {
        let mut old = parking_model();
        let (a, c) = (row_id(&old, "a"), row_id(&old, "c"));
        old.park(&c, SectionRole::Docked, None, false, 3).unwrap();
        old.sections[0].defaults = Some(SectionDefaults {
            path: "/w".into(),
            ..Default::default()
        });
        // What a client sends back: no roles, no parking, no defaults;
        // it dragged a into Docked and c out to the top level.
        let mut new = old.clone();
        for s in &mut new.sections {
            s.role = None;
            s.defaults = None;
            for r in &mut s.members {
                r.parked = None;
            }
        }
        let row_a = new.ungrouped.remove(0);
        let row_c = new.sections[1].members.remove(0);
        new.sections[1].members.push(row_a);
        new.ungrouped.insert(0, row_c);
        let changed = old.adopt(&mut new, 50, true);
        assert_eq!(new.sections[1].role, Some(SectionRole::Docked));
        assert_eq!(new.sections[0].defaults.as_ref().unwrap().path, "/w");
        let pa = new.find_container(&a).unwrap().parked.clone().unwrap();
        assert_eq!((pa.at, pa.from_section, pa.from_index), (50, None, 0));
        assert_eq!(new.find_container(&c).unwrap().parked, None);
        assert!(changed.contains(&(a.clone(), None, Some(SectionRole::Docked))));
        assert!(changed.contains(&(c.clone(), Some(SectionRole::Docked), None)));
        assert_eq!(changed.len(), 2);
    }
}
