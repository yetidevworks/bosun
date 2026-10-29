import AppKit
import BosunCore

/// One node in the sidebar outline. Reused across reloads (keyed by `key`) so expansion state holds.
@MainActor
final class SidebarItem: NSObject {
    enum Kind: Equatable {
        case section(String)
        case row(String)
        case tab(String)
        /// A heading that isn't a section: "On screen", over the row a filter would otherwise hide.
        case heading(String)
    }

    let kind: Kind
    var children: [SidebarItem] = []
    weak var parent: SidebarItem?

    init(kind: Kind) { self.kind = kind }

    var key: String {
        switch kind {
        case .section(let id): "s:\(id)"
        case .row(let id): "r:\(id)"
        case .tab(let s): "t:\(s)"
        case .heading(let id): "h:\(id)"
        }
    }
}

private let rowDragType = NSPasteboard.PasteboardType("dev.yetidevworks.bosun.sidebar-item")

@MainActor
final class SidebarController: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate, NSTextFieldDelegate {
    unowned let app: AppController
    /// The window this sidebar is in: its selection follows that window's row.
    unowned var owner: BosunWindow!
    let view = SidebarContainerView()
    let outline = SidebarOutlineView()
    private let scroll = NSScrollView()
    private var roots: [SidebarItem] = []
    private var cache: [String: SidebarItem] = [:]
    private var updatingSelection = false
    private var reloading = false
    private var hoverTimer: Timer?
    private var hoverRow: String?
    private var renaming: (item: SidebarItem, field: NSTextField, original: String)?
    /// Rows whose badge changed state recently (selftest hook): session → state.
    var onBadgeApplied: ((String, AgentState) -> Void)?
    /// The row under the pointer, filled lightly (rows and tabs only).
    private(set) weak var hoveredItem: SidebarItem?

    // The filter (Option+Cmd+F), this window's own and never saved.
    /// What's typed, applied live while the field has the keyboard and kept after Return.
    private(set) var filterQuery = ""
    /// The field or the chip is showing.
    private(set) var filterShown = false
    /// The filter as applied to the sidebar now; nil without one.
    private(set) var filtered: FilteredSidebar?
    /// The row on screen when `filtered` was worked out (it decides the "On screen" extra).
    private var filteredRow: String?

    /// The space above the first of Snoozed and Docked, which keeps them at the bottom of the sidebar: whatever
    /// the other rows leave, and never less than `minBottomGap`.
    private var bottomGap: CGFloat = SidebarController.minBottomGap
    private static let minBottomGap: CGFloat = 28
    /// The room above Snoozed or Docked when it isn't the first of them (or a filter is applied): its faint line.
    private static let roleLead: CGFloat = 14

    init(app: AppController) {
        self.app = app
        super.init()
        outline.controller = self
        outline.headerView = nil
        outline.refusesFirstResponder = true
        outline.focusRingType = .none
        outline.allowsEmptySelection = true
        outline.allowsMultipleSelection = false
        outline.indentationPerLevel = 0
        outline.indentationMarkerFollowsCell = false
        outline.autoresizesOutlineColumn = false
        outline.style = .plain
        outline.selectionHighlightStyle = .regular
        outline.intercellSpacing = NSSize(width: 0, height: 2)
        outline.backgroundColor = .clear
        outline.floatsGroupRows = false
        outline.rowSizeStyle = .custom
        let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("main"))
        col.resizingMask = .autoresizingMask
        outline.addTableColumn(col)
        outline.outlineTableColumn = col
        outline.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        outline.dataSource = self
        outline.delegate = self
        outline.registerForDraggedTypes([rowDragType] + DropReader.types)
        outline.setDraggingSourceOperationMask(.move, forLocal: true)
        // Not .gap: the gap animation pushes the row under the pointer away, so a drop could never land on
        // the middle of a one-tab row to merge into it.
        outline.draggingDestinationFeedbackStyle = .regular

        scroll.documentView = outline
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.automaticallyAdjustsContentInsets = false
        scroll.contentInsets = NSEdgeInsets(top: 0, left: 0, bottom: 8, right: 0)
        view.scroll = scroll
        view.addSubview(scroll)
        view.footer.onNew = { [weak self] in self?.app.newSession() }
        view.footer.onNewSection = { [weak self] in self?.app.newSection() }
        view.header.onReconnect = { [weak self] in self?.app.daemon.reconnectNow() }
        view.header.onFilter = { [weak self] in
            guard let self else { return }
            self.app.setCurrent(self.owner)
            self.toggleFilter()
        }
        view.filterBar.field.delegate = self
        view.filterBar.onActivate = { [weak self] in
            guard let self else { return }
            self.app.setCurrent(self.owner)
            self.beginFilterEditing()
        }
        view.filterBar.onClear = { [weak self] in self?.clearFilter() }
        view.emptyFilter.onClear = { [weak self] in self?.clearFilter() }
        scroll.contentView.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.frameDidChangeNotification, object: scroll.contentView, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.updateBottomGap() }
        }
    }

    // MARK: Model

    func reload() {
        let sb = app.daemon.state.sidebar
        filtered = computeFilter()
        filteredRow = owner.workspace.currentRow
        var used = Set<String>()
        func item(_ kind: SidebarItem.Kind) -> SidebarItem {
            let probe = SidebarItem(kind: kind)
            let it = cache[probe.key] ?? probe
            cache[it.key] = it
            used.insert(it.key)
            return it
        }
        func rowItem(_ c: Container, parent: SidebarItem?) -> SidebarItem {
            let r = item(.row(c.id))
            r.parent = parent
            r.children = c.tabs.count > 1 ? c.tabs.map { t in
                let ti = item(.tab(t.session))
                ti.parent = r
                ti.children = []
                return ti
            } : []
            return r
        }
        var newRoots: [SidebarItem] = []
        if let f = filtered {
            // Matching rows only, every section with one shown expanded, and the row on screen at the end
            // under its own heading when it doesn't match.
            newRoots = f.ungrouped.compactMap { sb.container(id: $0) }.map { rowItem($0, parent: nil) }
            for fs in f.sections {
                guard sb.sections.contains(where: { $0.id == fs.id }) else { continue }
                let si = item(.section(fs.id))
                si.parent = nil
                si.children = fs.rows.compactMap { sb.container(id: $0) }.map { rowItem($0, parent: si) }
                newRoots.append(si)
            }
            if let id = f.onScreen, let c = sb.container(id: id) {
                let h = item(.heading("on-screen"))
                h.parent = nil
                h.children = [rowItem(c, parent: h)]
                newRoots.append(h)
            }
        } else {
            newRoots = sb.ungrouped.map { rowItem($0, parent: nil) }
            for s in sb.sections {
                let si = item(.section(s.id))
                si.parent = nil
                si.children = s.members.map { rowItem($0, parent: si) }
                newRoots.append(si)
            }
        }
        cache = cache.filter { used.contains($0.key) }
        roots = newRoots
        reloading = true
        outline.reloadData()
        for r in roots { expandAsModel(r, sidebar: sb) }
        reloading = false
        updateBottomGap()
        syncSelection()
        updateFilterChrome()
    }

    /// A section shows collapsed when it's saved that way, or has no rows (it opens again as its saved state says
    /// once it has one).
    private func shownCollapsed(_ s: Section) -> Bool { s.collapsed || s.members.isEmpty }

    /// Snoozed or Docked, for a section header.
    private func role(of item: SidebarItem) -> SectionRole? {
        guard case .section(let id) = item.kind else { return nil }
        return app.daemon.state.sidebar.sections.first { $0.id == id }?.role
    }

    /// The empty room above a section header's own line: the gap that keeps Snoozed and Docked at the bottom,
    /// above the first of them, and room for the faint line above each. None on every other row.
    func lead(of item: SidebarItem) -> CGFloat {
        guard role(of: item) != nil else { return 0 }
        return filtered == nil && item === roots.first(where: { role(of: $0) != nil }) ? bottomGap : Self.roleLead
    }

    /// Whether `p` (in the outline) is in the empty room above a header, which clicks and drops pass over.
    func inLead(row: Int, at p: NSPoint) -> Bool {
        guard row >= 0, let item = outline.item(atRow: row) as? SidebarItem else { return false }
        let lead = lead(of: item)
        return lead > 0 && p.y < outline.rect(ofRow: row).minY + lead
    }

    /// Size the gap above Snoozed and Docked so they sit at the bottom of the sidebar, below everything else,
    /// and scroll with the rest once the rows outgrow it.
    private func updateBottomGap() {
        guard filtered == nil, let first = roots.first(where: { role(of: $0) != nil }) else { return }
        let clip = scroll.contentView.bounds.height - scroll.contentInsets.top - scroll.contentInsets.bottom
        var used: CGFloat = 0
        for r in 0..<outline.numberOfRows {
            guard let it = outline.item(atRow: r) as? SidebarItem else { continue }
            used += baseHeight(it) + outline.intercellSpacing.height + (it === first ? 0 : lead(of: it))
        }
        let gap = max(Self.minBottomGap, floor(clip - used))
        guard gap != bottomGap else { return }
        bottomGap = gap
        let row = outline.row(forItem: first)
        guard row >= 0 else { return }
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = 0
            outline.noteHeightOfRows(withIndexesChanged: IndexSet(integer: row))
        }
        if let cell = outline.view(atColumn: 0, row: row, makeIfNecessary: false) { configure(cell, first) }
        outline.rowView(atRow: row, makeIfNecessary: false)?.needsDisplay = true
    }

    private func expandAsModel(_ item: SidebarItem, sidebar sb: Sidebar) {
        switch item.kind {
        case .section(let id):
            // A filter shows every section with a match expanded, without touching its saved state.
            let collapsed = filtered == nil && (sb.sections.first { $0.id == id }.map(shownCollapsed) ?? false)
            if collapsed { outline.collapseItem(item) } else { outline.expandItem(item) }
            if !collapsed { for c in item.children { expandAsModel(c, sidebar: sb) } }
        case .heading:
            outline.expandItem(item)
            for c in item.children { expandAsModel(c, sidebar: sb) }
        case .row:
            if !item.children.isEmpty { outline.expandItem(item) }
        case .tab:
            break
        }
    }

    /// Redraw status and names without reloading structure (status deltas land here).
    func refreshRows(_ sessions: Set<String>? = nil) {
        // A state change can move a row in or out of a filter ("wait"): then the outline is rebuilt.
        if filtered != nil, computeFilter() != filtered { reload(); return }
        outline.enumerateAvailableRowViews { rowView, row in
            guard let item = outline.item(atRow: row) as? SidebarItem else { return }
            if let sessions, !sessions.isEmpty {
                let mine = self.sessions(of: item)
                if mine.isDisjoint(with: sessions) && !self.isSection(item) { return }
            }
            (rowView as? SidebarRowView)?.theme = app.theme
            rowView.needsDisplay = true
            if let cell = outline.view(atColumn: 0, row: row, makeIfNecessary: false) {
                configure(cell, item)
            }
        }
    }

    private func isSection(_ item: SidebarItem) -> Bool {
        switch item.kind {
        case .section, .heading: return true
        case .row, .tab: return false
        }
    }

    private func sessions(of item: SidebarItem) -> Set<String> {
        switch item.kind {
        case .tab(let s): return [s]
        case .row(let id): return Set(app.daemon.state.sidebar.container(id: id)?.tabs.map(\.session) ?? [])
        case .section, .heading: return []
        }
    }

    /// The session a click on this item makes the active pane: a tab's own, or the row's remembered pane.
    func session(for item: SidebarItem) -> String? {
        switch item.kind {
        case .tab(let s): return s
        case .row(let id): return owner.workspace.rememberedPane(row: id)
        case .section, .heading: return nil
        }
    }

    /// The row (container id) an item belongs to.
    func rowID(for item: SidebarItem) -> String? {
        switch item.kind {
        case .row(let id): return id
        case .tab(let s): return app.daemon.state.sidebar.container(holding: s)?.id
        case .section, .heading: return nil
        }
    }

    /// Whether this item is what's selected: the shown row, or the active pane's tab under it.
    func isShownSelection(_ item: SidebarItem) -> Bool {
        switch item.kind {
        case .row(let id): return id == owner.workspace.currentRow
        case .tab(let s): return rowID(for: item) == owner.workspace.currentRow && s == owner.workspace.layout.active
        case .section, .heading: return false
        }
    }

    /// Select the shown row (selection follows the main area, never the other way). Its tab rows show
    /// which pane is active (drawn by `SidebarRowView`).
    func syncSelection() {
        // With a filter, the row on screen decides the "On screen" extra: a new one rebuilds the outline.
        if filtered != nil, owner.workspace.currentRow != filteredRow { reload(); return }
        updatingSelection = true
        defer { updatingSelection = false }
        guard let id = owner.workspace.currentRow, let item = cache["r:\(id)"] else {
            outline.deselectAll(nil)
            return
        }
        let row = outline.row(forItem: item)
        if row >= 0 {
            if outline.selectedRow != row { outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false) }
            outline.scrollRowToVisible(row)
        } else {
            outline.deselectAll(nil)
        }
        outline.enumerateAvailableRowViews { rv, _ in rv.needsDisplay = true }
    }

    func itemFor(session: String) -> SidebarItem? {
        let sb = app.daemon.state.sidebar
        guard let c = sb.container(holding: session) else { return nil }
        if c.tabs.count > 1 { return cache["t:\(session)"] }
        return cache["r:\(c.id)"]
    }

    func applyTheme() {
        view.theme = app.theme
        refreshRows()
        outline.enumerateAvailableRowViews { rv, _ in rv.needsDisplay = true }
    }

    func setStatus(_ s: HeaderStatus) {
        view.header.set(s)
    }

    // MARK: Clicks

    /// Set when a click turned into a drag, so it isn't also treated as a click.
    var dragStarted = false

    func rowClicked(_ row: Int) {
        guard let item = outline.item(atRow: row) as? SidebarItem else { return }
        if case .section(let id) = item.kind {
            // A click anywhere on a section header toggles it (with a filter, sections stay open; one with no
            // rows stays closed).
            if filtered == nil, !item.children.isEmpty { app.setSectionCollapsed(id, outline.isItemExpanded(item)) }
            return
        }
        if case .heading = item.kind { return }
        // A click in a window that isn't key acts in that window.
        app.setCurrent(owner)
        switch item.kind {
        case .row(let id): app.sidebarActivated(row: id)
        case .tab(let s): app.sidebarActivated(session: s)
        case .section, .heading: break
        }
    }

    /// A double-click on a row's or tab's name renames it. (Not a click on the selected one, Finder style: that
    /// fired too easily on a click meant only to bring a row back to the front.)
    func doubleClickedName(_ item: SidebarItem) {
        guard !isSection(item), renaming == nil else { return }
        beginRename(item)
    }

    // MARK: Hover prewarm

    /// Hovering a row (or one of its tabs) for 50 ms warms every pane of that row.
    func hovered(row: Int) {
        setHover(row >= 0 ? outline.item(atRow: row) as? SidebarItem : nil)
        let id = row >= 0 ? (outline.item(atRow: row) as? SidebarItem).flatMap { rowID(for: $0) } : nil
        guard id != hoverRow else { return }
        hoverRow = id
        hoverTimer?.invalidate()
        guard let id else { return }
        hoverTimer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.owner.workspace.prewarm(row: id) }
        }
    }

    /// The light fill under the pointer: rows and tabs, never headers.
    func setHover(_ item: SidebarItem?) {
        let new: SidebarItem? = item.flatMap { if case .row = $0.kind { return $0 } else if case .tab = $0.kind { return $0 } else { return nil } }
        guard new !== hoveredItem else { return }
        let old = hoveredItem
        hoveredItem = new
        for it in [old, new].compactMap({ $0 }) {
            let r = outline.row(forItem: it)
            if r >= 0 { outline.rowView(atRow: r, makeIfNecessary: false)?.needsDisplay = true }
        }
    }

    // MARK: Filter

    /// The filter's field has the keyboard (its ring shows).
    var filterEditing: Bool { owner.focus.exception == .filter }

    /// The rows Cmd+1…9 number and previous/next row walks: what this sidebar shows.
    var visibleRowIDs: [String] { app.daemon.state.sidebar.visibleRows(filter: filtered).map(\.id) }

    private func computeFilter() -> FilteredSidebar? {
        guard SidebarFilter.normalized(filterQuery) != nil else { return nil }
        let st = app.daemon.state
        return SidebarFilter.apply(filterQuery, sidebar: st.sidebar, current: owner.workspace.currentRow, label: { [app] in app.rowLabel($0) }) { [app] s in
            SidebarFilter.TabInfo(name: app.displayName(s), path: st.sessions[s]?.path, agent: st.sessions[s]?.agent,
                                  state: app.displayState(s), alive: app.isAlive(s))
        }
    }

    /// Option+Cmd+F and the magnifier: open the field (or give it the keyboard again, the query selected); with
    /// the field focused and empty, close it.
    func toggleFilter() {
        if filterEditing {
            if view.filterBar.field.stringValue.isEmpty { clearFilter() } else { view.filterBar.field.currentEditor()?.selectAll(nil) }
            return
        }
        beginFilterEditing()
    }

    /// The field borrows the keyboard (design 7b's temporary, visible exception), with the query selected.
    func beginFilterEditing() {
        guard let window = outline.window, !filterEditing else { return }
        // One exception at a time: a rename in progress ends (keeping its name), the palette closes.
        if owner.focus.exception == .rename { window.makeFirstResponder(nil) }
        if owner.focus.exception == .palette, owner.palette != nil { app.closePalette() }
        if owner.focus.exception == .find { owner.closeFind(leaveCopyMode: true) }
        guard owner.focus.exception == nil else { NSSound.beep(); return }
        filterShown = true
        let field = view.filterBar.field
        field.stringValue = filterQuery
        view.filterBar.setEditing(true)
        owner.focus.begin(.filter)
        updateFilterChrome()
        view.layoutSubtreeIfNeeded()
        window.makeFirstResponder(field)
        field.currentEditor()?.selectAll(nil)
    }

    /// Return, a click anywhere else, Cmd+K: keep what's typed as a chip and hand the keyboard back to the
    /// active terminal. An empty field just closes.
    func commitFilter() {
        guard filterEditing else { return }
        filterQuery = view.filterBar.field.stringValue
        if SidebarFilter.normalized(filterQuery) == nil { clearFilter(); return }
        endFilterEditing()
        updateFilterChrome()
    }

    /// Esc, the chip's clear button, the empty state's Clear: no filter, field closed, keyboard back.
    func clearFilter() {
        let had = filtered != nil
        filterQuery = ""
        view.filterBar.field.stringValue = ""
        filterShown = false
        endFilterEditing()
        if had { reload() } else { updateFilterChrome() }
    }

    private func endFilterEditing() {
        if owner.focus.end(.filter) {
            owner.workspace.focusActiveTerminal()
            // No terminal to take it (an exited row): nothing holds the keyboard rather than the field.
            if let w = outline.window, let ed = w.firstResponder as? NSTextView, ed.delegate === view.filterBar.field {
                w.makeFirstResponder(nil)
            }
        }
        view.filterBar.setEditing(false)
    }

    /// The field, the magnifier's dot, the footer hint and the empty state follow the filter.
    private func updateFilterChrome() {
        let theme = app.theme
        view.filterBar.isHidden = !filterShown
        view.filterBar.count.stringValue = filtered?.countLabel ?? ""
        view.header.filterApplied = filtered != nil
        view.footer.hint.isHidden = filtered == nil || filterEditing
        if let f = filtered, f.isEmpty {
            view.emptyFilter.isHidden = false
            view.emptyFilter.set(query: f.query, theme: theme)
        } else {
            view.emptyFilter.isHidden = true
        }
        view.needsLayout = true
        view.filterBar.needsLayout = true
        view.footer.needsLayout = true
    }

    // MARK: Rename

    func beginRename(_ item: SidebarItem) {
        let row = outline.row(forItem: item)
        guard row >= 0, let cell = outline.view(atColumn: 0, row: row, makeIfNecessary: true) as? SidebarCellView,
              let window = outline.window else { return }
        endRename(commit: false)
        // Starting a rename ends the filter's claim on the keyboard, never the query it applied.
        if filterEditing { commitFilter() }
        let field = cell.nameField
        let original = field.stringValue
        renaming = (item, field, original)
        owner.focus.begin(.rename)
        field.isEditable = true
        field.isSelectable = true
        field.delegate = self
        field.drawsBackground = true
        field.backgroundColor = app.theme.bg
        field.textColor = app.theme.text
        field.focusRingType = .none
        window.makeFirstResponder(field)
        field.currentEditor()?.selectAll(nil)
    }

    func beginRename(session: String) {
        if let item = itemFor(session: session) { beginRename(item) }
    }

    var isRenaming: Bool { renaming != nil }

    private func endRename(commit: Bool) {
        guard let r = renaming else { return }
        renaming = nil
        let newName = r.field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        r.field.isEditable = false
        r.field.isSelectable = false
        r.field.drawsBackground = false
        r.field.delegate = nil
        if commit, !newName.isEmpty, newName != r.original {
            app.rename(item: r.item, to: newName)
        } else {
            r.field.stringValue = r.original
        }
        if owner.focus.end(.rename) { owner.workspace.focusActiveTerminal() }
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy sel: Selector) -> Bool {
        if control === view.filterBar.field {
            switch sel {
            case #selector(NSResponder.insertNewline(_:)): commitFilter(); return true
            case #selector(NSResponder.cancelOperation(_:)): clearFilter(); return true
            // Up/Down do nothing here: Cmd+K is for jumping, and Cmd+1…9 follow the filter after Return.
            case #selector(NSResponder.moveUp(_:)), #selector(NSResponder.moveDown(_:)): return true
            default: return false
            }
        }
        if sel == #selector(NSResponder.insertNewline(_:)) { endRename(commit: true); return true }
        if sel == #selector(NSResponder.cancelOperation(_:)) { endRename(commit: false); return true }
        return false
    }

    func controlTextDidChange(_ obj: Notification) {
        // Every key narrows the list live.
        guard (obj.object as? NSTextField) === view.filterBar.field, filterEditing else { return }
        filterQuery = view.filterBar.field.stringValue
        reload()
    }

    func controlTextDidEndEditing(_ obj: Notification) {
        if (obj.object as? NSTextField) === view.filterBar.field {
            // The field lost the keyboard some other way: keep what's typed, as Return would.
            if filterEditing { commitFilter() }
            return
        }
        // Clicking anywhere else ends the edit and keeps the new name.
        endRename(commit: true)
    }

    // MARK: Context menu

    func menu(forRow row: Int) -> NSMenu? {
        guard row >= 0, let item = outline.item(atRow: row) as? SidebarItem else { return app.sidebarBackgroundMenu() }
        switch item.kind {
        case .section(let id): return app.sectionMenu(for: id)
        case .heading: return app.sidebarBackgroundMenu()
        case .row(let id):
            guard let c = app.daemon.state.sidebar.container(id: id) else { return nil }
            return app.sessionMenu(for: c.active, rowItem: item)
        case .tab(let s): return app.sessionMenu(for: s, rowItem: item)
        }
    }

    // MARK: Data source

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        (item as? SidebarItem)?.children.count ?? roots.count
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        (item as? SidebarItem)?.children[index] ?? roots[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        guard let it = item as? SidebarItem else { return false }
        if case .section = it.kind { return true }
        return !it.children.isEmpty
    }



    // MARK: Delegate

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat {
        guard let it = item as? SidebarItem else { return 26 }
        return baseHeight(it) + lead(of: it)
    }

    private func baseHeight(_ it: SidebarItem) -> CGFloat {
        switch it.kind {
        case .section: return Fonts.height(30)
        case .heading: return Fonts.height(28)
        case .row: return Fonts.height(28)
        case .tab: return Fonts.height(24)
        }
    }

    func outlineView(_ outlineView: NSOutlineView, rowViewForItem item: Any) -> NSTableRowView? {
        let rv = SidebarRowView()
        rv.theme = app.theme
        rv.item = item as? SidebarItem
        rv.controller = self
        return rv
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let it = item as? SidebarItem else { return nil }
        let cell: NSView
        if isSection(it) {
            cell = outlineView.makeView(withIdentifier: SectionCellView.id, owner: nil) ?? SectionCellView()
        } else {
            cell = outlineView.makeView(withIdentifier: SidebarCellView.id, owner: nil) ?? SidebarCellView()
        }
        configure(cell, it)
        return cell
    }

    func configure(_ cell: NSView, _ item: SidebarItem) {
        let theme = app.theme
        let st = app.daemon.state
        switch item.kind {
        case .section(let id):
            guard let c = cell as? SectionCellView, let s = st.sidebar.sections.first(where: { $0.id == id }) else { return }
            let waiting = s.members.flatMap(\.tabs).contains { app.displayState($0.session) == .waiting }
            c.chevronRoom = filtered == nil ? 20 : 0
            c.lead = lead(of: item)
            c.configure(name: s.name, count: s.members.count, collapsed: shownCollapsed(s) && filtered == nil, waiting: waiting, theme: theme,
                        onAdd: s.role != nil ? nil : { [weak self] in self?.app.newSession(section: id) })
        case .heading:
            guard let c = cell as? SectionCellView else { return }
            c.lead = 0
            c.configure(name: "On screen", count: nil, collapsed: false, waiting: false, theme: theme)
        case .row(let id):
            guard let c = cell as? SidebarCellView, let con = st.sidebar.container(id: id) else { return }
            let multi = con.tabs.count > 1
            let session = con.active
            let alive = con.tabs.contains { app.isAlive($0.session) }
            let name = app.rowLabel(con)
            let state: AgentState = multi ? aggregate(con) : app.displayState(session)
            // A row another window shows says so: selecting it brings that window forward.
            let elsewhere = app.isShown(row: id, elsewhereThan: owner)
            // The row on screen that a filter would hide, listed at the end, dimmed.
            let extra: Bool = { if case .heading = item.parent?.kind { return true }; return false }()
            var t = multi ? "\(con.tabs.count) tabs" : trailing(for: session)
            switch st.sidebar.role(ofRow: id) {
            case .docked: t = "docked"
            case .snoozed: t = app.snoozeLeft(con).map { "\($0) left" } ?? "until needed"
            case nil: break
            }
            if extra { t = "on screen" }
            if elsewhere { t = t.isEmpty ? "⧉" : "⧉ " + t }
            c.configure(name: renaming?.item !== item ? name : nil, matches: filtered?.nameMatches[id],
                        state: state, exited: !alive, trailing: t,
                        trailingColor: state == .waiting && !multi && !extra ? theme.waiting : theme.textMuted, indent: 0, theme: theme,
                        tooltip: elsewhere ? [tooltip(session), elsewhereTip].compactMap { $0 }.joined(separator: "\n") : tooltip(session),
                        trailingTip: elsewhere ? elsewhereTip : nil, dimmed: extra)
            c.badge.onApplied = { [weak self] s in self?.onBadgeApplied?(session, s) }
        case .tab(let s):
            guard let c = cell as? SidebarCellView else { return }
            let alive = app.isAlive(s)
            let state = app.displayState(s)
            let extra: Bool = { if case .heading = item.parent?.parent?.kind { return true }; return false }()
            c.configure(name: renaming?.item !== item ? app.displayName(s) : nil, matches: filtered?.tabMatches[s],
                        state: state, exited: !alive, trailing: trailing(for: s), trailingColor: state == .waiting ? theme.waiting : theme.textMuted,
                        indent: 16, theme: theme, tooltip: tooltip(s), trailingTip: nil, dimmed: extra)
            c.badge.onApplied = { [weak self] st in self?.onBadgeApplied?(s, st) }
        }
    }

    private let elsewhereTip = "Shown in another window — click to bring it forward"

    private func aggregate(_ c: Container) -> AgentState {
        let states = c.tabs.map { app.displayState($0.session) }
        for s in [AgentState.waiting, .error, .running, .done, .seen] where states.contains(s) { return s }
        return states.first ?? .unknown
    }

    private func trailing(for session: String) -> String {
        let st = app.daemon.state
        guard let info = st.sessions[session] else { return "exited" }
        let state = app.displayState(session)
        if state == .waiting { return "waiting" }
        if let e = app.turnElapsed(session) { return e }
        if let b = info.branch, !b.isEmpty { return "⎇ \(b)" }
        if let a = info.agent, a != "terminal" { return a }
        return ""
    }

    private func tooltip(_ session: String) -> String? {
        guard let info = app.daemon.state.sessions[session] else { return nil }
        var parts = [info.display]
        if let p = info.path { parts.append((p as NSString).abbreviatingWithTildeInPath) }
        if let d = app.statusDetail(session) { parts.append(d) }
        return parts.joined(separator: "\n")
    }

    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        guard let it = item as? SidebarItem else { return false }
        return !isSection(it)
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        // Selection mirrors the active tile; a click goes through `rowClicked` instead.
        guard !updatingSelection else { return }
        outline.enumerateAvailableRowViews { rv, _ in rv.needsDisplay = true }
    }

    func outlineViewItemDidExpand(_ notification: Notification) {
        if !reloading { updateBottomGap() }
        guard !reloading, let it = notification.userInfo?["NSObject"] as? SidebarItem, case .section(let id) = it.kind else { return }
        app.setSectionCollapsed(id, false)
        for c in it.children where !c.children.isEmpty { outline.expandItem(c) }
    }

    func outlineViewItemDidCollapse(_ notification: Notification) {
        if !reloading { updateBottomGap() }
        guard !reloading, let it = notification.userInfo?["NSObject"] as? SidebarItem, case .section(let id) = it.kind else { return }
        app.setSectionCollapsed(id, true)
    }

    /// A section with no rows stays closed.
    func outlineView(_ outlineView: NSOutlineView, shouldExpandItem item: Any) -> Bool {
        guard let it = item as? SidebarItem, case .section = it.kind else { return true }
        return !it.children.isEmpty
    }

    func outlineView(_ outlineView: NSOutlineView, shouldCollapseItem item: Any) -> Bool {
        guard let it = item as? SidebarItem else { return false }
        // With a filter, sections stay open (their saved state is left alone).
        if case .section = it.kind { return filtered == nil || reloading }
        return reloading
    }

    // MARK: Drag and drop

    func outlineView(_ outlineView: NSOutlineView, pasteboardWriterForItem item: Any) -> (any NSPasteboardWriting)? {
        guard let it = item as? SidebarItem, renaming == nil else { return nil }
        if case .heading = it.kind { return nil }
        let pb = NSPasteboardItem()
        pb.setString(it.key, forType: rowDragType)
        return pb
    }

    func outlineView(_ outlineView: NSOutlineView, draggingSession session: NSDraggingSession, willBeginAt screenPoint: NSPoint, forItems draggedItems: [Any]) {
        dragStarted = true
        if debugDrag { print("drag: began at \(screenPoint) items \(draggedItems.count)"); fflush(stdout) }
        setHover(nil)
        draggingKind = (draggedItems.first as? SidebarItem)?.kind
    }

    /// What's being dragged out of this sidebar, for a drag that ends outside every window.
    private var draggingKind: SidebarItem.Kind?

    /// A drag let go outside every Bosun window tears its row (or tab) off into a new window.
    func outlineView(_ outlineView: NSOutlineView, draggingSession session: NSDraggingSession, endedAt screenPoint: NSPoint, operation: NSDragOperation) {
        defer { draggingKind = nil }
        if debugDrag { print("drag: ended at \(screenPoint) operation \(operation.rawValue)"); fflush(stdout) }
        guard operation == [], let kind = draggingKind, !app.isOverBosunWindow(screenPoint) else { return }
        // On the next turn: the drag's own cleanup runs first.
        DispatchQueue.main.async { [app] in MainActor.assumeIsolated { app.sidebarDraggedOut(kind, at: screenPoint) } }
    }

    private func draggedItem(_ info: NSDraggingInfo) -> SidebarItem? {
        guard let key = info.draggingPasteboard.string(forType: rowDragType) else { return nil }
        return cache[key]
    }

    func outlineView(_ outlineView: NSOutlineView, validateDrop info: NSDraggingInfo, proposedItem item: Any?, proposedChildIndex index: Int) -> NSDragOperation {
        let op = validate(outlineView, info, item, index)
        if debugDrag {
            let p = outlineView.convert(info.draggingLocation, from: nil)
            print("drag: \(String(format: "%.2f", Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 1000))) at \(Int(p.x)),\(Int(p.y)) proposed \((item as? SidebarItem)?.key ?? "root")#\(index) dragged \(draggedItem(info)?.key ?? "nil") filtered \(filtered != nil) -> \(op.rawValue)")
            fflush(stdout)
        }
        return op
    }

    private func validate(_ outlineView: NSOutlineView, _ info: NSDraggingInfo, _ item: Any?, _ index: Int) -> NSDragOperation {
        var target = item as? SidebarItem
        var index = index
        guard let dragged = draggedItem(info) else {
            // Files: only onto a session row. Anywhere else is refused, so the cursor says so before the drop.
            guard DropReader.canRead(info.draggingPasteboard) else { return [] }
            let p = outlineView.convert(info.draggingLocation, from: nil)
            let row = outlineView.row(at: p)
            guard row >= 0, let under = outlineView.item(atRow: row) as? SidebarItem, let s = session(for: under), app.isAlive(s) else { return [] }
            outlineView.setDropItem(under, dropChildIndex: NSOutlineViewDropOnItemIndex)
            return .copy
        }
        let ungroupedCount = app.daemon.state.sidebar.ungrouped.count
        let pointerY = outlineView.convert(info.draggingLocation, from: nil).y
        // In the empty room above Snoozed or Docked: a drop between that header and what's above it, never into it.
        let pointerRow = outlineView.row(at: outlineView.convert(info.draggingLocation, from: nil))
        if inLead(row: pointerRow, at: outlineView.convert(info.draggingLocation, from: nil)),
           let header = outlineView.item(atRow: pointerRow) as? SidebarItem, let i = roots.firstIndex(where: { $0 === header }) {
            (target, index) = (nil, i)
            outlineView.setDropItem(nil, dropChildIndex: i)
        }
        // With a filter, the outline shows only some rows, so positions in it aren't positions in the
        // sidebar: only merges (onto a row) and a tab out into its own row are taken.
        let limited = filtered != nil
        /// Whether the pointer is in the lower half of `item`'s own row.
        func below(_ item: SidebarItem) -> Bool {
            let row = outlineView.row(forItem: item)
            return row >= 0 && pointerY > outlineView.rect(ofRow: row).midY
        }
        /// The row a drop goes into, and where among its tabs: over the middle of a row's own line, after its
        /// tabs (nil); over one of its tabs, before or after that tab by the pointer. Nearer a row's top or bottom
        /// edge, or at the bottom of its last tab, it's nil, which means "put it beside the row" as before.
        func intoTarget() -> (row: SidebarItem, index: Int?)? {
            let p = outlineView.convert(info.draggingLocation, from: nil)
            let r = outlineView.row(at: p)
            guard r >= 0, let under = outlineView.item(atRow: r) as? SidebarItem else { return nil }
            let rect = outlineView.rect(ofRow: r)
            let middle = abs(p.y - rect.midY) < rect.height * 0.3
            switch under.kind {
            case .row: return middle ? (under, nil) : nil
            case .tab:
                guard let row = under.parent else { return nil }
                // A filtered row may not show every tab, so only "after its tabs" is taken.
                if limited { return middle ? (row, nil) : nil }
                guard let i = row.children.firstIndex(where: { $0 === under }) else { return nil }
                let lower = p.y > rect.midY
                if lower, i == row.children.count - 1, p.y > rect.maxY - rect.height * 0.25 { return nil }
                return (row, i + (lower ? 1 : 0))
            case .section, .heading: return nil
            }
        }
        /// Where a row (or a tab coming out of its row) goes among the rows: beside the row under the pointer,
        /// above or below by the pointer, or at the outline's own spot between rows or in a section. False when
        /// there's nowhere to put it.
        func placeRow() -> Bool {
            // Over a multi-tab row or one of its tabs (the outline proposes dropping inside it): beside that row
            // in its own list instead.
            var over = target
            if let t = over, case .tab = t.kind { over = t.parent }
            if let t = over, case .row = t.kind {
                let list = t.parent?.children ?? roots
                guard let i = list.firstIndex(where: { $0 === t }) else { return false }
                outlineView.setDropItem(t.parent, dropChildIndex: i + (below(t) ? 1 : 0))
                return true
            }
            if over == nil {
                if index == NSOutlineViewDropOnItemIndex { return false }
                if index <= ungroupedCount { return true }
                // Between sections: land at the end of the section above.
                let sectionIdx = index - ungroupedCount - 1
                if sectionIdx >= 0, sectionIdx < roots.count - ungroupedCount {
                    let sec = roots[ungroupedCount + sectionIdx]
                    outlineView.setDropItem(sec, dropChildIndex: sec.children.count)
                    return true
                }
                return false
            }
            if let t = over, case .section = t.kind {
                if index == NSOutlineViewDropOnItemIndex { outlineView.setDropItem(t, dropChildIndex: t.children.count) }
                return true
            }
            return false
        }
        switch dragged.kind {
        case .row:
            if let into = intoTarget(), into.row !== dragged {
                outlineView.setDropItem(into.row, dropChildIndex: into.index ?? NSOutlineViewDropOnItemIndex)
                return .move
            }
            if limited { return [] }
            return placeRow() ? .move : []
        case .section:
            if limited { return [] }
            // Sections only go between top-level items, but the outline proposes drops inside whatever is
            // under the pointer: take the top-level item there and go before or after it.
            var top = target
            while let p = top?.parent { top = p }
            if let top {
                guard let i = roots.firstIndex(where: { $0 === top }) else { return [] }
                outlineView.setDropItem(nil, dropChildIndex: max(ungroupedCount, i + (below(top) ? 1 : 0)))
                return .move
            }
            guard index != NSOutlineViewDropOnItemIndex, index >= 0 else { return [] }
            if index < ungroupedCount { outlineView.setDropItem(nil, dropChildIndex: ungroupedCount) }
            return .move
        case .tab:
            guard let row = dragged.parent else { return [] }
            if let into = intoTarget() {
                if into.row !== row {
                    outlineView.setDropItem(into.row, dropChildIndex: into.index ?? NSOutlineViewDropOnItemIndex)
                    return .move
                }
                // Over one of its own row's tabs: a reorder, before or after that tab.
                if let i = into.index {
                    outlineView.setDropItem(row, dropChildIndex: i)
                    return .move
                }
            }
            if let target, target === row {
                if limited { return [] }
                return index == NSOutlineViewDropOnItemIndex ? [] : .move
            }
            // Out of its row it becomes a row of its own, where it's let go.
            if !limited, placeRow() { return .move }
            // With a filter, it goes right after the old row, so show the gap there.
            let list = row.parent?.children ?? roots
            guard let i = list.firstIndex(where: { $0 === row }) else { return [] }
            outlineView.setDropItem(row.parent, dropChildIndex: i + 1)
            return .move
        case .heading:
            return []
        }
    }

    func outlineView(_ outlineView: NSOutlineView, acceptDrop info: NSDraggingInfo, item: Any?, childIndex index: Int) -> Bool {
        let target = item as? SidebarItem
        guard let dragged = draggedItem(info) else {
            guard let target, let s = session(for: target) else { return false }
            app.dropOnSidebar(session: s, pasteboard: info.draggingPasteboard)
            return true
        }
        // Merges and moves into or out of a row go to the daemon, which owns the tmux side of them too. (A tab
        // dropped among its own row's tabs is a reorder, below.)
        if let target, case .row(let into) = target.kind, index == NSOutlineViewDropOnItemIndex || target !== dragged.parent {
            let at = index == NSOutlineViewDropOnItemIndex ? nil : index
            switch dragged.kind {
            case .row(let from): DispatchQueue.main.async { [app] in MainActor.assumeIsolated { app.mergeRow(from, into: into, at: at) } }
            case .tab(let s): DispatchQueue.main.async { [app] in MainActor.assumeIsolated { app.moveTab(s, into: into, at: at) } }
            case .section, .heading: return false
            }
            return true
        }
        var sb = app.daemon.state.sidebar
        let ungroupedCount = sb.ungrouped.count
        if case .tab(let s) = dragged.kind, target !== dragged.parent {
            // A row of its own where it was let go: among the ungrouped rows or in a section. With a filter, the
            // outline's positions aren't the sidebar's, so it goes right after its old row.
            var at: Int?
            var section: String?
            if filtered == nil, index != NSOutlineViewDropOnItemIndex {
                if let target, case .section(let sid) = target.kind {
                    (at, section) = (index, sid)
                } else if target == nil {
                    at = min(index, ungroupedCount)
                }
            }
            DispatchQueue.main.async { [app] in MainActor.assumeIsolated { app.moveTab(s, into: nil, at: at, section: section) } }
            return true
        }
        var changed = false
        switch dragged.kind {
        case .row(let id):
            if let target, case .section(let sid) = target.kind {
                changed = sb.moveRow(id: id, toSection: sid, index: index)
            } else if target == nil {
                changed = sb.moveRow(id: id, toSection: nil, index: min(index, ungroupedCount))
            }
        case .section(let id):
            changed = sb.moveSection(id: id, toIndex: max(0, index - ungroupedCount))
        case .tab(let s):
            changed = sb.moveTab(session: s, toIndex: index)
        case .heading:
            break
        }
        // Saved on the next turn: saving reloads the outline, which mustn't happen inside its own drop handling.
        if changed { DispatchQueue.main.async { [app] in MainActor.assumeIsolated { app.daemon.saveSidebar(sb) } } }
        return changed
    }
}

// MARK: - Views

final class SidebarOutlineView: NSOutlineView {
    weak var controller: SidebarController?
    private var tracking: NSTrackingArea?
    /// Focus test hook: a drag that AppKit's own tracking starts is handed here instead of to the
    /// window server, so the test plays the drop side in-process and the drag never follows the real mouse.
    var dragCapture: ((NSDraggingSession, [NSDraggingItem], NSEvent) -> Void)?

    /// Outside the app's windows a let-go tears the item off, so the drag image shouldn't slide back there.
    /// (No super: NSTableView doesn't implement this optional NSDraggingSource method, and the call would
    /// throw on the drag's first move, which AppKit swallows by ending the drag.)
    override func draggingSession(_ session: NSDraggingSession, movedTo screenPoint: NSPoint) {
        if let app = controller?.app { session.animatesToStartingPositionsOnCancelOrFail = app.isOverBosunWindow(screenPoint) }
    }

    override func beginDraggingSession(with items: [NSDraggingItem], event: NSEvent, source: any NSDraggingSource) -> NSDraggingSession {
        guard let capture = dragCapture else { return super.beginDraggingSession(with: items, event: event, source: source) }
        let session = NSDraggingSession()
        source.draggingSession?(session, willBeginAt: window?.convertPoint(toScreen: event.locationInWindow) ?? .zero)
        capture(session, items, event)
        return session
    }

    override var acceptsFirstResponder: Bool { false }
    /// A click on a row in a background window acts straight away (the keyboard stays in the terminal anyway).
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        let row = self.row(at: p)
        // The empty room above Snoozed and Docked is background: no toggle, no drag.
        if controller?.inLead(row: row, at: p) == true { return }
        let clicked = row >= 0 ? item(atRow: row) as? SidebarItem : nil
        var onName = false
        if row >= 0, let cell = view(atColumn: 0, row: row, makeIfNecessary: false) as? SidebarCellView {
            let q = cell.convert(event.locationInWindow, from: nil)
            onName = cell.nameField.frame.insetBy(dx: -2, dy: -3).contains(q)
                && q.x <= cell.nameField.frame.minX + cell.nameField.intrinsicContentSize.width + 4
        }
        controller?.dragStarted = false
        let onDisclosure = row >= 0 && frameOfOutlineCell(atRow: row).contains(p)
        // super runs AppKit's tracking loop (selection, drag start, disclosure) and returns on mouse up.
        super.mouseDown(with: event)
        guard row >= 0, controller?.dragStarted == false, !onDisclosure else { return }
        controller?.rowClicked(row)
        if event.clickCount == 2, onName, let clicked { controller?.doubleClickedName(clicked) }
    }

    /// The section chevron (only sections have one) says what a click does.
    override func makeView(withIdentifier identifier: NSUserInterfaceItemIdentifier, owner: Any?) -> NSView? {
        let v = super.makeView(withIdentifier: identifier, owner: owner)
        if identifier == NSOutlineView.disclosureButtonIdentifier, let b = v as? NSButton {
            b.toolTip = "Collapse or expand this section (a click on its name does too)"
        }
        return v
    }

    override func menu(for event: NSEvent) -> NSMenu? {
        let p = convert(event.locationInWindow, from: nil)
        let row = self.row(at: p)
        return controller?.menu(forRow: controller?.inLead(row: row, at: p) == true ? -1 : row)
    }

    override func updateTrackingAreas() {
        if let tracking { removeTrackingArea(tracking) }
        let t = NSTrackingArea(rect: bounds, options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect], owner: self, userInfo: nil)
        addTrackingArea(t)
        tracking = t
        super.updateTrackingAreas()
    }

    override func mouseMoved(with event: NSEvent) {
        controller?.hovered(row: row(at: convert(event.locationInWindow, from: nil)))
    }

    override func mouseExited(with event: NSEvent) { controller?.hovered(row: -1) }

    /// No disclosure triangles on rows (their tabs are always shown); sections keep theirs.
    override func frameOfOutlineCell(atRow row: Int) -> NSRect {
        // With a filter, sections stay open, so they have no chevron either.
        guard let item = item(atRow: row) as? SidebarItem, case .section = item.kind, controller?.filtered == nil else { return .zero }
        let f = super.frameOfOutlineCell(atRow: row)
        // Centred on the header's own line, below any room above it.
        let lead = controller?.lead(of: item) ?? 0
        return NSRect(x: bounds.width - f.width - 10, y: f.minY + lead / 2, width: f.width, height: f.height)
    }

    override func frameOfCell(atColumn column: Int, row: Int) -> NSRect {
        let f = super.frameOfCell(atColumn: column, row: row)
        return NSRect(x: 8, y: f.minY, width: bounds.width - 16, height: f.height)
    }

    override func drawBackground(inClipRect clipRect: NSRect) {}
}

final class SidebarRowView: NSTableRowView {
    var theme: AppTheme?
    weak var item: SidebarItem?
    weak var controller: SidebarController?

    /// The selected row: the selection fill and the same accent bar the active pane's tab gets, so "which
    /// row am I on" reads on every theme (light themes' selection fills are close to their panels).
    override func drawSelection(in dirtyRect: NSRect) {
        guard let theme else { return }
        let r = bounds.insetBy(dx: 6, dy: 0)
        theme.selection.setFill()
        NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6).fill()
        SidebarRowView.accentBar(in: r, theme)
    }

    static func accentBar(in r: NSRect, _ theme: AppTheme) {
        theme.accent.setFill()
        NSBezierPath(roundedRect: NSRect(x: r.minX + 3, y: r.minY + 5, width: 3, height: r.height - 10), xRadius: 1.5, yRadius: 1.5).fill()
    }

    override func drawBackground(in dirtyRect: NSRect) {
        guard let theme, let item, let controller, !isSelected else { return }
        // Snoozed and Docked each have a faint line above them.
        let lead = controller.lead(of: item)
        if lead > 0 {
            theme.textMuted.withAlphaComponent(0.22).setFill()
            NSRect(x: 14, y: isFlipped ? lead - 7 : bounds.height - lead + 6, width: max(0, bounds.width - 28), height: 1).fill()
            return
        }
        let r = bounds.insetBy(dx: 6.5, dy: 0.5)
        // The shown row's tabs each have a pane on screen; the active pane's tab is marked (a fill and
        // an accent bar), so the row says which pane has the keyboard.
        if case .tab(let s) = item.kind, let ws = controller.owner?.workspace,
           controller.rowID(for: item) == ws.currentRow, s == ws.layout.active, ws.layout.count > 1 {
            theme.panelAlt.setFill()
            NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6).fill()
            SidebarRowView.accentBar(in: r, theme)
            return
        }
        // The row under the pointer answers it.
        if controller.hoveredItem === item {
            theme.panelAlt.withAlphaComponent(0.5).setFill()
            NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6).fill()
        }
    }

    /// A row being dropped onto (a merge): an accent outline in the theme's colours.
    override func drawDraggingDestinationFeedback(in dirtyRect: NSRect) {
        guard let theme else { return super.drawDraggingDestinationFeedback(in: dirtyRect) }
        var r = bounds
        if let item, let lead = controller?.lead(of: item), lead > 0 {
            r.size.height -= lead
            if isFlipped { r.origin.y += lead }
        }
        let path = NSBezierPath(roundedRect: r.insetBy(dx: 7, dy: 1), xRadius: 6, yRadius: 6)
        theme.accent.withAlphaComponent(0.18).setFill()
        path.fill()
        theme.accent.setStroke()
        path.lineWidth = 2
        path.stroke()
    }

    override var isEmphasized: Bool { get { false } set {} }
    override var interiorBackgroundStyle: NSView.BackgroundStyle { .normal }
}

final class SidebarCellView: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("row")
    let badge = StatusBadge(frame: NSRect(x: 0, y: 0, width: 10, height: 10))
    let nameField = NSTextField(labelWithString: "")
    private let trailing = NSTextField(labelWithString: "")
    /// The text on the right (for the test modes).
    var trailingText: String { trailing.stringValue }
    var trailingTip: String? { trailing.toolTip }
    private var indent: CGFloat = 0

    init() {
        super.init(frame: .zero)
        identifier = SidebarCellView.id
        textField = nameField
        nameField.font = Fonts.ui(13)
        nameField.lineBreakMode = .byTruncatingTail
        nameField.isSelectable = false
        nameField.isEditable = false
        nameField.drawsBackground = false
        nameField.isBezeled = false
        nameField.cell?.isScrollable = true
        trailing.font = Fonts.ui(11)
        trailing.alignment = .right
        trailing.lineBreakMode = .byTruncatingHead
        addSubview(badge)
        addSubview(nameField)
        addSubview(trailing)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        let h = bounds.height
        badge.frame = NSRect(x: 8 + indent, y: (h - 10) / 2, width: 10, height: 10)
        trailing.sizeToFit()
        let tw = min(trailing.frame.width, bounds.width * 0.4)
        trailing.frame = NSRect(x: bounds.width - tw - 8, y: (h - trailing.frame.height) / 2, width: tw, height: trailing.frame.height)
        let nx = 26 + indent
        let nameH = nameField.intrinsicContentSize.height
        nameField.frame = NSRect(x: nx, y: (h - nameH) / 2, width: max(10, trailing.frame.minX - nx - 6), height: nameH)
    }

    /// `name` nil leaves the field alone (it's being renamed). `matches` are the characters a filter
    /// matched, underlined in the accent colour.
    func configure(name: String?, matches: [Int]?, state: AgentState, exited: Bool, trailing t: String, trailingColor: NSColor,
                   indent: CGFloat, theme: AppTheme, tooltip: String?, trailingTip: String?, dimmed: Bool) {
        self.indent = indent
        badge.set(state, exited: exited, theme: theme)
        if !nameField.isEditable {
            let color = exited ? theme.textMuted : theme.text
            let font = Fonts.ui(indent > 0 ? 12.5 : 13, state == .waiting || state == .done ? .medium : .regular)
            nameField.textColor = color
            nameField.font = font
            if let name {
                if let matches, !matches.isEmpty {
                    nameField.attributedStringValue = SidebarCellView.underlined(name, matches, font: font, color: color, accent: theme.accent)
                } else {
                    nameField.stringValue = name
                }
            }
        }
        trailing.font = Fonts.ui(11)
        trailing.stringValue = t
        trailing.textColor = trailingColor
        trailing.toolTip = trailingTip
        toolTip = tooltip
        alphaValue = dimmed ? 0.55 : 1
        needsLayout = true
    }

    /// `name` with the characters at `offsets` underlined in the accent colour and a touch heavier.
    static func underlined(_ name: String, _ offsets: [Int], font: NSFont, color: NSColor, accent: NSColor) -> NSAttributedString {
        let out = NSMutableAttributedString(string: name, attributes: [.font: font, .foregroundColor: color])
        let marked = Fonts.ui(font.pointSize, .semibold, scaled: false)
        let chars = Array(name.indices)
        for o in offsets where o < chars.count {
            let r = NSRange(chars[o]..<name.index(after: chars[o]), in: name)
            out.addAttributes([.underlineStyle: NSUnderlineStyle.single.rawValue, .underlineColor: accent, .font: marked], range: r)
        }
        return out
    }
}

final class SectionCellView: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("section")
    private let label = NSTextField(labelWithString: "")
    private let count = NSTextField(labelWithString: "")
    private let dot = CALayer()
    /// New Session in Section, beside the chevron. Hidden on the "On screen" heading.
    let addButton = NSButton()
    var onAdd: (() -> Void)?
    /// Room the outline's chevron takes at the right edge (none while a filter keeps sections open).
    var chevronRoom: CGFloat = 20
    /// Empty room above the header's own line (Snoozed and Docked): the content sits below it.
    var lead: CGFloat = 0 { didSet { if lead != oldValue { needsLayout = true } } }

    init() {
        super.init(frame: .zero)
        identifier = SectionCellView.id
        wantsLayer = true
        label.lineBreakMode = .byTruncatingTail
        addSubview(label)
        addSubview(count)
        layer?.addSublayer(dot)
        dot.cornerRadius = 3
        addButton.image = NSImage(systemSymbolName: "plus", accessibilityDescription: "New Session in Section")?
            .withSymbolConfiguration(.init(pointSize: 10, weight: .semibold))
        addButton.isBordered = false
        addButton.imagePosition = .imageOnly
        addButton.toolTip = "New Session in Section"
        addButton.target = self
        addButton.action = #selector(addClicked)
        addSubview(addButton)
    }

    @objc private func addClicked() { onAdd?() }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        label.sizeToFit()
        count.sizeToFit()
        let h = bounds.height - lead
        let y = h - label.frame.height - 5
        label.frame = NSRect(x: 8, y: y, width: max(0, min(label.frame.width, bounds.width - 70)), height: label.frame.height)
        count.frame = NSRect(x: label.frame.maxX + 6, y: y, width: count.frame.width, height: count.frame.height)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dot.frame = NSRect(x: count.frame.maxX + 6, y: y + (label.frame.height - 6) / 2, width: 6, height: 6)
        CATransaction.commit()
        // Centred on the row, like the outline's chevron beside it.
        addButton.frame = NSRect(x: bounds.width - chevronRoom - 18, y: (h - 16) / 2, width: 16, height: 16)
    }

    /// The row count beside the name.
    var countText: String { count.stringValue }

    func configure(name: String, count n: Int?, collapsed: Bool, waiting: Bool, theme: AppTheme, onAdd: (() -> Void)? = nil) {
        self.onAdd = onAdd
        addButton.isHidden = onAdd == nil
        label.font = Fonts.ui(11, .semibold)
        count.font = Fonts.digits(11)
        addButton.contentTintColor = theme.textMuted
        label.stringValue = name.uppercased()
        label.textColor = theme.textMuted
        count.stringValue = n.map { "\($0)" } ?? ""
        count.textColor = theme.dimFg
        dot.backgroundColor = theme.waiting.cgColor
        dot.isHidden = !(collapsed && waiting)
        needsLayout = true
    }
}

/// Sidebar column: the header (under the traffic lights), the filter's field when it shows, the outline,
/// and a footer with "+" and "New Section".
final class SidebarContainerView: NSView {
    var scroll: NSScrollView? {
        didSet { if let scroll { addSubview(emptyFilter, positioned: .above, relativeTo: scroll) } }
    }
    let header = SidebarHeader()
    let filterBar = SidebarFilterBar()
    let emptyFilter = SidebarEmptyFilterView()
    let footer = SidebarFooter()
    private let edge = CALayer()
    var theme: AppTheme? {
        didSet {
            guard let theme else { return }
            layer?.backgroundColor = theme.panel.cgColor
            edge.backgroundColor = theme.shadow.cgColor
            header.theme = theme
            filterBar.theme = theme
            footer.theme = theme
        }
    }
    static let footerHeight: CGFloat = 28

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        addSubview(header)
        addSubview(filterBar)
        addSubview(footer)
        emptyFilter.isHidden = true
        filterBar.isHidden = true
        layer?.addSublayer(edge)
    }

    required init?(coder: NSCoder) { fatalError() }

    /// Where the outline starts: under the header, and under the filter's field while it shows.
    var outlineTop: CGFloat { SidebarHeader.height + (filterBar.isHidden ? 0 : SidebarFilterBar.space) }

    override func layout() {
        super.layout()
        let fh = SidebarContainerView.footerHeight
        header.frame = NSRect(x: 0, y: 0, width: bounds.width, height: SidebarHeader.height)
        filterBar.frame = NSRect(x: 10, y: SidebarHeader.height + 2, width: max(0, bounds.width - 20), height: SidebarFilterBar.height)
        let top = outlineTop
        let outline = NSRect(x: 0, y: top, width: bounds.width, height: max(0, bounds.height - top - fh))
        scroll?.frame = outline
        // While a filter matches nothing, its message sits at the top of the outline and the row on screen below it.
        emptyFilter.frame = NSRect(x: 0, y: top, width: bounds.width, height: SidebarEmptyFilterView.height)
        if let scroll {
            let inset = emptyFilter.isHidden ? 0 : SidebarEmptyFilterView.height
            if scroll.contentInsets.top != inset { scroll.contentInsets = NSEdgeInsets(top: inset, left: 0, bottom: 8, right: 0) }
        }
        footer.frame = NSRect(x: 0, y: bounds.height - fh, width: bounds.width, height: fh)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        edge.frame = NSRect(x: bounds.width - 1, y: 0, width: 1, height: bounds.height)
        CATransaction.commit()
    }

    /// Drops on the sidebar background are refused (rows handle their own).
    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation { [] }
}

/// "+" and "New Section"; while a filter is applied, a reminder that Cmd+1…9 follow it. (The connection
/// status is in the header.)
final class SidebarFooter: NSView {
    private let add = NSButton()
    private let addSection = NSButton()
    let hint = NSTextField(labelWithString: "⌘1–9 follow the filter")
    private let line = CALayer()
    var onNew: (() -> Void)?
    var onNewSection: (() -> Void)?
    var theme: AppTheme? {
        didSet {
            guard let theme else { return }
            line.backgroundColor = theme.shadow.cgColor
            add.contentTintColor = theme.textMuted
            addSection.contentTintColor = theme.textMuted
            hint.textColor = theme.textMuted
        }
    }

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.addSublayer(line)
        for (b, symbol, tip, sel) in [(add, "plus", "New Session (⇧⌘N)", #selector(newClicked)),
                                      (addSection, "folder.badge.plus", "New Section", #selector(sectionClicked))] {
            b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tip)
            b.isBordered = false
            b.bezelStyle = .regularSquare
            b.toolTip = tip
            b.refusesFirstResponder = true
            b.target = self
            b.action = sel
            addSubview(b)
        }
        hint.font = .systemFont(ofSize: 11)
        hint.lineBreakMode = .byTruncatingTail
        hint.isHidden = true
        addSubview(hint)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        let h = bounds.height
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        line.frame = NSRect(x: 0, y: 0, width: bounds.width, height: 1)
        CATransaction.commit()
        add.frame = NSRect(x: 10, y: (h - 22) / 2, width: 24, height: 22)
        addSection.frame = NSRect(x: 36, y: (h - 22) / 2, width: 26, height: 22)
        hint.sizeToFit()
        let right = bounds.width - 10
        let hw = min(hint.frame.width, max(0, right - 70))
        hint.frame = NSRect(x: right - hw, y: (h - hint.frame.height) / 2, width: hw, height: hint.frame.height)
    }

    @objc private func newClicked() { onNew?() }
    @objc private func sectionClicked() { onNewSection?() }
}
