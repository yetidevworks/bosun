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
        }
    }

    fn model(ungrouped: &[&str], sections: Vec<Section>) -> SidebarModel {
        SidebarModel {
            ungrouped: ungrouped.iter().map(|s| con(s)).collect(),
            sections,
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
}
