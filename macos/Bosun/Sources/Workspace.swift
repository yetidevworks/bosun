import AppKit
import BosunCore
import os
import UniformTypeIdentifiers

/// What every window's workspace shares: each row's layout (a row is shown in one window at a time, but
/// its layout is the same whichever window shows it), and the bookkeeping for panes the app asked for,
/// closed or is launching an agent in.
@MainActor
final class RowStore {
    /// Every row's layout, by container id. A row gets one the first time it's shown (or prewarmed).
    var rows: [String: RowLayout] = [:]
    /// Sessions whose deferred agent launch waits for their terminal to attach.
    var pendingLaunch: [String: Bool?] = [:]
    /// Panes the app asked for (Cmd+D, New Tab…): where the row's next new tab goes, until it arrives.
    var pendingSplits: [String: (split: PendingSplit, at: Date)] = [:]
    /// A tab the app created that should become its row's active pane once it has one.
    var activateWhenPlaced: [String: String] = [:]
    /// Panes closed with Cmd+W whose kill hasn't landed: a delta that still lists them as alive
    /// mustn't bring the pane back.
    var closing: Set<String> = []
}

/// One window's main area (design section 6): the shown sidebar row's panes, one per live tab, laid out
/// from that row's split tree by hand (no Auto Layout), with the warm pool's views positioned inside
/// them, per-pane chrome, dividers, click/hover activation and drops. Every row keeps its own tree and
/// its active pane; showing another row swaps the whole area for that row's panes. A row is on screen
/// in one window at most.
@MainActor
final class Workspace {
    unowned let app: AppController
    /// The window this area is in (its focus model, sidebar and key state).
    unowned var owner: BosunWindow!
    let area = MainAreaView()
    var pool: TerminalPool { app.pool }
    private var store: RowStore { app.rowStore }
    /// Every row's layout, by container id (shared by every window).
    private(set) var rows: [String: RowLayout] {
        get { store.rows }
        set { store.rows = newValue }
    }
    /// The row on screen (a container id), nil before there is any.
    private(set) var currentRow: String?
    private var chrome: [TileID: TileChromeView] = [:]
    /// Chrome views of panes that went, kept for the next row's panes so a switch makes no new views.
    private var spareChrome: [TileChromeView] = []
    private var dividers: [DividerView] = []
    private var lastLayout = SplitNode.Layout()
    /// A pane's header, taller with a larger interface font.
    static var headerHeight: CGFloat { max(26, Fonts.height(26)) }
    static let gap: CGFloat = 1
    static let inset: CGFloat = 5
    private(set) var windowKey = true
    private var dropTile: TileID?
    /// A pane being dragged by its header, and where it would land (see `tileDragMoved`).
    private var tileDrag: (tile: TileID, target: TileID?, zone: TileDropZone?)?
    let dropIndicator = TileDropIndicatorView()
    private var pendingLaunch: [String: Bool?] {
        get { store.pendingLaunch }
        set { store.pendingLaunch = newValue }
    }
    private var pendingSplits: [String: (split: PendingSplit, at: Date)] {
        get { store.pendingSplits }
        set { store.pendingSplits = newValue }
    }
    private var activateWhenPlaced: [String: String] {
        get { store.activateWhenPlaced }
        set { store.activateWhenPlaced = newValue }
    }
    private var closing: Set<String> {
        get { store.closing }
        set { store.closing = newValue }
    }
    /// The row order at the last reconcile, to find the neighbour of a row that went.
    private var lastRowOrder: [String] = []
    /// The old single-tree layout file's active session: its row is shown first after the change.
    private var legacyActive: String?
    /// Views attach only once the app has the daemon's tmux socket (`startAttaching`).
    private var attaching = false
    /// When the current row switch started (the warm and cold measurements count from here).
    private var switchStart: CFTimeInterval?
    /// Hooks for the self-test: (session, warm, seconds to first frame / first painted frame).
    var onShowMeasured: ((String, Bool, Double, Double?) -> Void)?
    /// Self-test hook: a row was shown (row id, every pane warm, seconds of main-thread work for the swap).
    var onRowShown: ((String, Bool, Double) -> Void)?

    init(app: AppController) {
        self.app = app
        area.workspace = self
        area.registerForDraggedTypes(DropReader.types)
    }

    // MARK: What is shown

    /// The shown row's layout (empty when there's no row, or every tab of it has ended).
    var layout: RowLayout { currentRow.flatMap { rows[$0] } ?? RowLayout() }
    /// The shown row as the daemon describes it.
    var container: Container? { currentRow.flatMap { app.daemon.state.sidebar.container(id: $0) } }

    /// The tree on screen: the row's panes; for a row whose tabs have all ended, one tile for its
    /// active tab (the exited placeholder, with Restart); with no rows at all, one empty tile ("").
    var displayRoot: SplitNode {
        if let r = layout.shownRoot { return r }
        if let c = container { return .leaf(c.active) }
        return .leaf("")
    }

    var tiles: [TileID] { displayRoot.leaves }
    var activeTile: TileID { layout.active ?? tiles[0] }
    /// The active pane's session (an exited row's ended tab while it shows the placeholder).
    var activeSession: String? { activeTile.isEmpty ? nil : activeTile }
    var activeTerminal: TerminalView? {
        guard layout.contains(activeTile) else { return nil }
        return pool.view(for: activeTile).flatMap { $0.exited ? nil : $0 }
    }
    /// The panes on screen (with a zoomed pane, just that one).
    var visibleSessions: Set<String> { Set(layout.shown) }
    var tileRects: [TileID: CGRect] { lastLayout.tiles }
    func headerSession(_ tile: TileID) -> String? { chrome[tile]?.headerSession }
    /// What a pane's header shows: its badge, the text beside the name and that text's colour (for the test modes).
    func headerStatus(_ tile: TileID) -> (state: AgentState?, text: String, color: NSColor?)? { chrome[tile]?.headerStatus }
    /// The title of a placeholder on screen ("api has exited", "No sessions yet"), for the test modes.
    var placeholderTitle: String? {
        chrome.values.compactMap(\.placeholderView).first { !$0.isHiddenOrHasHiddenAncestor }?.title.stringValue
    }
    func headerZoomNote(_ tile: TileID) -> String? { chrome[tile]?.headerZoomNote }
    /// The find bar open on a pane (Cmd+F), if any.
    func findBar(for tile: TileID) -> PaneFindBar? { chrome[tile]?.findBar.flatMap { $0.isHidden ? nil : $0 } }
    func showFindBar(for tile: TileID) -> PaneFindBar? { chrome[tile]?.showFindBar() }
    func hideFindBar(for tile: TileID) { chrome[tile]?.findBar?.isHidden = true }
    func session(in tile: TileID) -> String? { tile.isEmpty ? nil : tile }

    func terminal(in tile: TileID) -> TerminalView? {
        layout.contains(tile) ? pool.view(for: tile) : nil
    }

    /// The pane a row would show with the keyboard: its remembered active pane, else its active tab.
    func rememberedPane(row id: String) -> String? {
        rows[id]?.active ?? app.daemon.state.sidebar.container(id: id)?.active
    }

    private var aspect: Double {
        let b = area.bounds
        return b.height > 1 && b.width > 1 ? Double(b.width / b.height) : 1.6
    }

    /// A row's tabs that can have a pane: alive, known to the daemon, and not being closed.
    func liveTabs(_ c: Container) -> [String] {
        c.tabs.filter { $0.alive && app.isAlive($0.session) && !closing.contains($0.session) }.map(\.session)
    }

    // MARK: Persistence (the app writes one file for every window's rows)

    /// Start on `row` (from the saved layouts, or a tear-off), or on the old single-tree file's session.
    func restore(row: String?, legacySession: String? = nil) {
        currentRow = row
        legacyActive = legacySession
        reconcile(focus: false)
    }

    func save() { app.saveLayouts() }

    /// Save soon: pane activation (hover included) changes the model often, the file needn't follow every step.
    private func scheduleSave() { app.scheduleSaveLayouts() }

    /// Whether row `id` is free for this window: not on screen in another one.
    private func free(_ id: String) -> Bool { !app.isShown(row: id, elsewhereThan: owner) }

    // MARK: Rows

    /// Show a row: its whole layout replaces the main area, and the keyboard goes to `pane` if given,
    /// else to the pane that was active when the row was last shown.
    func selectRow(_ id: String, pane: TileID? = nil, focus: Bool = true) {
        guard app.daemon.state.sidebar.container(id: id) != nil else { return }
        // A row is on screen in one window at most: another window showing it comes forward instead.
        if let other = app.window(showing: id), other !== owner {
            app.bringForward(other)
            other.workspace.selectRow(id, pane: pane, focus: focus)
            return
        }
        if id == currentRow {
            if let pane, layout.contains(pane) { activate(pane, focus: focus) } else if focus { focusActiveTerminal() }
            return
        }
        let t0 = CACurrentMediaTime()
        let sp = signposter.beginInterval("RowSwitch", id: signposter.makeSignpostID(), "\(id, privacy: .public)")
        // The row we leave tells the daemon which tab it was on, so the TUI shows the same one.
        if let old = currentRow { app.syncActiveTab(row: old, pane: rows[old]?.active) }
        let before = layout.shown
        reconcileRow(id, create: true)
        if let pane { rows[id]?.activate(pane) }
        currentRow = id
        legacyActive = nil
        switchStart = t0
        let warm = layout.shown.allSatisfy { pool.isWarm($0) }
        apply(previous: before, focus: focus)
        switchStart = nil
        let spent = CACurrentMediaTime() - t0
        signposter.endInterval("RowSwitch", sp)
        save()
        onRowShown?(id, warm, spent)
    }

    /// This window's row is moving to another window: show the next row no window shows (in sidebar
    /// order, else the one before). Returns false when there's none, and nothing changes.
    func leaveRow() -> Bool {
        guard let cur = currentRow else { return true }
        let order = app.daemon.state.sidebar.allRows.filter { !liveTabs($0).isEmpty }.map(\.id)
        let i = order.firstIndex(of: cur) ?? 0
        guard let next = order.dropFirst(i + 1).first(where: { $0 != cur && free($0) })
                ?? order.prefix(i).last(where: { $0 != cur && free($0) }) else { return false }
        selectRow(next, focus: false)
        return true
    }

    /// Show `session`'s row with that pane active (a dead tab just shows its row).
    func show(_ session: String, focus: Bool = true) {
        guard let c = app.daemon.state.sidebar.container(holding: session) else { return }
        selectRow(c.id, pane: session, focus: focus)
    }

    /// Match a row's layout to its live tabs. `create` gives a row its first layout.
    @discardableResult
    private func reconcileRow(_ id: String, create: Bool) -> RowLayout.Change? {
        guard let c = app.daemon.state.sidebar.container(id: id), create || rows[id] != nil else { return nil }
        var l = rows[id] ?? RowLayout()
        var pending = pendingSplits[id]
        if let p = pending, Date().timeIntervalSince(p.at) > 15 {
            pendingSplits[id] = nil
            pending = nil
        }
        let change = l.reconcile(liveTabs: liveTabs(c), preferredActive: c.active, aspect: aspect, pending: pending?.split)
        if change.usedPending { pendingSplits[id] = nil }
        if let want = activateWhenPlaced[id], l.contains(want) {
            l.activate(want)
            activateWhenPlaced[id] = nil
        }
        if rows[id] != l { rows[id] = l }
        return change
    }

    /// The daemon's sidebar or sessions changed: match every row's panes to its tabs, keep a row on
    /// screen, attach panes that appeared in it and let go of the ones that went. Views that stay are
    /// never touched.
    func reconcile(focus: Bool = true) {
        let st = app.daemon.state
        let sb = st.sidebar
        let order = sb.allRows.map(\.id)
        closing = closing.filter { app.isAlive($0) }
        // The shown row's active session, read before rows that went are dropped: if its row was merged
        // into another, the screen follows the session there.
        let shownActive = layout.active
        if st.live {
            let ids = Set(order)
            rows = rows.filter { ids.contains($0.key) }
            pendingSplits = pendingSplits.filter { ids.contains($0.key) }
        }
        let before = layout.shown
        let beforeActive = layout.active
        for id in Array(rows.keys) { reconcileRow(id, create: false) }

        // Keep a row on screen: the old file's session's row on the first run after the change, the
        // neighbour of a row that went (once the state is live, not from the cache), or the first row.
        // Rows another window shows aren't candidates (one window per row).
        var target = currentRow
        if let legacy = legacyActive, let c = sb.container(holding: legacy), free(c.id) {
            target = c.id
            legacyActive = nil
        }
        if let t = target, order.contains(t), !free(t), st.live { target = nil }
        if let t = target, !order.contains(t), st.live, let s = shownActive, let moved = sb.container(holding: s), free(moved.id) {
            target = moved.id
            activateWhenPlaced[moved.id] = s
        } else if let t = target, !order.contains(t), st.live {
            let i = lastRowOrder.firstIndex(of: t) ?? 0
            let later = lastRowOrder.dropFirst(i + 1).first { order.contains($0) && free($0) }
            let earlier = lastRowOrder.prefix(i).last { order.contains($0) && free($0) }
            target = later ?? earlier ?? order.first(where: free)
        }
        if target == nil {
            target = sb.allRows.first { !liveTabs($0).isEmpty && free($0.id) }?.id ?? order.first(where: free)
        }
        if !order.isEmpty { lastRowOrder = order }
        // A window other than the first with no row left to show (every other one is on screen elsewhere) closes.
        if target == nil, st.live, app.windows.first !== owner {
            currentRow = nil
            apply(previous: before, focus: false)
            app.closeWindowSoon(owner)
            return
        }
        if target != currentRow {
            if let t = target { reconcileRow(t, create: true) }
            currentRow = target
            apply(previous: before, focus: focus)
            save()
            return
        }
        if let c = currentRow, order.contains(c) { reconcileRow(c, create: true) }
        let changed = layout.shown != before || layout.active != beforeActive || layout.root == nil
        apply(previous: before, focus: focus && changed)
        if changed { scheduleSave() }
    }

    /// Make the screen match the model: frames, chrome, and a live view in every pane of the shown row.
    private func apply(previous: [TileID], focus: Bool) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        lastLayout = displayRoot.layout(in: area.bounds, gap: Workspace.gap)
        // Others first and the active pane last, so it's the most recently used in the pool.
        let shown = layout.shown
        let panes = shown.filter { $0 != layout.active } + shown.filter { $0 == layout.active }
        for s in panes {
            if let v = pool.view(for: s), !v.exited, !v.isHidden, v.superview === area { pool.touch(s); continue }
            attachVisible(s)
        }
        // Panes that left the screen: another row's, or the others of a row that zoomed one. A view
        // another window has taken meanwhile (its row moved there) is that window's now.
        for s in previous where !shown.contains(s) && pool.view(for: s)?.superview === area { pool.hide(s) }
        pool.evict(keeping: app.visibleSessions)
        owner.focus.setTiles(count: tiles.count, active: activeTile)
        syncChrome()
        CATransaction.commit()
        if focus { focusActiveTerminal() }
        app.workspaceChanged(self)
    }

    /// The app got the daemon's tmux socket: attach the shown row's panes.
    func startAttaching() {
        pool.tmuxSocket = app.daemon.state.server?.tmuxSocket
        guard app.daemon.state.server != nil else { return }
        attaching = true
        reconcile()
        focusActiveTerminal()
    }

    /// Start attaching a row's panes in the background (hover, keyboard stepping), so showing it is warm.
    /// The active pane first, then the others.
    func prewarm(row id: String) {
        guard attaching, id != currentRow, free(id), app.daemon.state.sidebar.container(id: id) != nil else { return }
        reconcileRow(id, create: true)
        guard let l = rows[id], let root = l.shownRoot else { return }
        let rects = root.layout(in: area.bounds, gap: Workspace.gap).tiles
        let order = l.shown.filter { $0 == l.active } + l.shown.filter { $0 != l.active }
        for s in order where app.isAlive(s) {
            pool.prewarm(s, size: Workspace.terminalRect(in: rects[s] ?? area.bounds).size, in: area, windowKey: windowKey)
        }
        pool.evict(keeping: app.visibleSessions)
    }

    // MARK: Showing sessions

    /// Put a session's view into its pane: swap in a warm view, or attach a new one.
    private func attachVisible(_ session: String) {
        guard attaching, app.isAlive(session) else { return }
        let rect = terminalRect(for: session)
        let t0 = switchStart ?? CACurrentMediaTime()
        let (v, warm) = pool.acquire(session, frame: rect, hidden: false, in: area, windowKey: windowKey)
        let label: StaticString = warm ? "WarmSwitch" : "ColdAttach"
        let sp = signposter.beginInterval(label, id: signposter.makeSignpostID(), "\(session, privacy: .public)")
        if v.frame != rect { v.frame = rect }
        v.resizeThrottle = resizing ? Workspace.resizeThrottle : 0
        pool.reveal(session, wasWarm: warm)
        if !warm && !v.hasPaintedContent {
            // Stay transparent until tmux paints, so login(1)'s banner never flashes up. (Layer opacity,
            // not alphaValue: AppKit won't make a view with alphaValue 0 first responder.)
            v.layer?.opacity = 0
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak v] in MainActor.assumeIsolated { v?.layer?.opacity = 1 } }
        }
        var first: Double?
        let before = v.contentsCount
        // A pooled view can still be waiting for tmux's first paint (shown again before it drew).
        let awaitingPaint = !v.hasPaintedContent
        v.onContents = { [weak self] view in
            let now = CACurrentMediaTime()
            if first == nil {
                first = now - t0
                signposter.endInterval(label, sp)
                if warm { self?.onShowMeasured?(session, true, first!, nil) }
            }
            if awaitingPaint, view.contentsCount - before >= 1, view.hasPaintedContent {
                view.onContents = nil
                view.layer?.opacity = 1
                let painted = now - t0
                signposter.emitEvent("AttachFirstPaint", "\(session, privacy: .public)")
                if !warm { self?.onShowMeasured?(session, false, first ?? painted, painted) }
                self?.app.terminalPainted(session)
                self?.launchIfPending(session)
            } else if !awaitingPaint {
                view.onContents = nil
            }
        }
        if !awaitingPaint { launchIfPending(session) }
    }

    func expectLaunch(_ session: String, resume: Bool?) {
        pendingLaunch[session] = resume
        if let v = pool.view(for: session), v.hasPaintedContent { launchIfPending(session) }
    }

    private func launchIfPending(_ session: String) {
        guard let resume = pendingLaunch.removeValue(forKey: session) else { return }
        // A beat after tmux's first paint, so the agent's startup probes reach an attached client.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) { [app] in
            MainActor.assumeIsolated { app.daemon.command(.launchAgent(session: session, resume: resume)) }
        }
    }

    /// A terminal's tmux client ended on its own (the session was killed, or the client detached
    /// elsewhere) and the app dropped its view: a pane on screen here attaches again.
    func terminalExited(_ session: String) {
        if visibleSessions.contains(session), app.isAlive(session) {
            // Still alive (detached by someone else, or Ctrl+q): attach again.
            attachVisible(session)
            // The old view had the keyboard; the new one takes it over.
            if session == activeTile { focusActiveTerminal() }
        }
        syncChrome()
    }

    /// The daemon saw the tmux client on `tty` move to `target` (nil: it detached). Bosun's tmux
    /// bindings for the TUI (prefix 1-9, `switch-client`), or tmux's own, typed inside a terminal can
    /// do that. A pane always shows its own tab, so the client always goes back; the app follows the
    /// move instead: a tab of the same row becomes the active pane, a tab of another row shows that row.
    func clientMoved(tty: String, to target: String?) {
        pool.noteClient("client \(tty) now on \(target ?? "nothing"); the view on that tty is \(pool.view(onTTY: tty)?.session ?? "none")")
        guard let v = pool.view(onTTY: tty) else { return }  // not one of ours, or already going away
        let session = v.session
        // A detach also ends the `tmux attach` in the view, and `terminalExited` attaches it again.
        // Attaches and `make_latest` report the session the client is already on.
        guard let target, target != session else { return }
        log.info("client \(tty, privacy: .public) moved from \(session, privacy: .public) to \(target, privacy: .public); moving it back")
        pool.moveClient(tty, to: session)
        // A hidden pooled view: nobody is looking at it, so putting it back is all.
        guard layout.contains(session), !v.isHidden else { return }
        guard app.isAlive(target), let c = app.daemon.state.sidebar.container(holding: target) else { return }
        if c.id == currentRow {
            activate(target)
        } else {
            selectRow(c.id, pane: target)
        }
    }

    /// Sessions went away: free their views, then match the layouts to the new state.
    func stateChanged() {
        reconcile()
    }

    // MARK: New and closed panes

    /// The app is creating a tab in row `id` (Cmd+D, New Tab…): the row's next new tab splits `from`.
    func expectSplit(row id: String, from: TileID?, axis: SplitAxis) {
        pendingSplits[id] = (PendingSplit(from: from, axis: axis), Date())
    }

    func cancelSplit(row id: String) { pendingSplits[id] = nil }

    /// The daemon acknowledged a tab the app created: show its row with that pane active (once it's there).
    func paneCreated(_ session: String, row id: String) {
        if rows[id]?.contains(session) == true {
            selectRow(id, pane: session)
        } else {
            activateWhenPlaced[id] = session
            selectRow(id)
        }
    }

    /// Cmd+W, after any confirmation: the pane goes now (its sibling takes the space and the keyboard)
    /// and the kill follows. The last pane of a row isn't removed here: the row shows what the daemon
    /// reports once the kill lands (gone, or exited).
    func closePane(_ session: String) {
        guard let id = currentRow, rows[id]?.contains(session) == true, layout.count > 1 else { return }
        let before = layout.shown
        closing.insert(session)
        rows[id]?.close(session)
        apply(previous: before, focus: true)
        save()
    }

    /// The kill for a closed pane failed: it comes back.
    func closeFailed(_ session: String) {
        closing.remove(session)
        reconcile()
    }

    // MARK: Panes

    func activate(_ tile: TileID, focus: Bool = true) {
        guard let id = currentRow, layout.contains(tile) else {
            if focus { focusActiveTerminal() }
            return
        }
        let changed = tile != layout.active
        if debugFrames, changed { print(String(format: "activate %.3f %@", CACurrentMediaTime(), tile)) }
        let before = layout.shown
        rows[id]?.activate(tile)
        owner.focus.activate(tile)
        if changed, layout.isZoomed {
            // The zoom follows the active pane: the old one leaves the screen and this one fills it.
            apply(previous: before, focus: focus)
            scheduleSave()
            return
        }
        if changed {
            pool.touch(tile)
            updateChromeState()
            app.workspaceChanged(self)
            scheduleSave()
        }
        if focus { focusActiveTerminal() }
    }

    func focusActiveTerminal() {
        guard owner.focus.terminalOwnsKeyboard, let w = area.window else { return }
        if let t = activeTerminal {
            if w.firstResponder !== t { w.makeFirstResponder(t) }
        } else if w.firstResponder is TerminalView {
            w.makeFirstResponder(nil)
        }
    }

    /// Previous/next tab: the active pane moves through the row's panes in tab order.
    func stepPane(forward: Bool) {
        guard let c = container, let next = layout.step(forward: forward, order: c.tabs.map(\.session)) else { return }
        activate(next)
    }

    func moveFocus(_ d: Direction) {
        // Zoomed, the direction is by the row's whole tree, and the zoom moves to that pane.
        let geometry = layout.isZoomed ? (layout.root?.layout(in: area.bounds, gap: Workspace.gap) ?? lastLayout) : lastLayout
        guard let n = SplitNode.neighbor(of: activeTile, d, in: geometry) else { NSSound.beep(); return }
        activate(n)
    }

    /// Cmd+Return: show the active pane alone, full size, or the whole row again. The tree doesn't
    /// change; the other panes' views stay attached and hidden, so unzooming is a swap.
    func toggleZoom() {
        guard let id = currentRow else { NSSound.beep(); return }
        let before = layout.shown
        guard rows[id]?.toggleZoom() == true else { NSSound.beep(); return }
        apply(previous: before, focus: true)
        save()
    }

    /// The visible terminal under a point in window coordinates.
    func terminal(atWindowPoint p: NSPoint) -> TerminalView? {
        visibleSessions.compactMap { pool.view(for: $0) }.first { v in
            !v.isHidden && v.window != nil && v.bounds.contains(v.convert(p, from: nil))
        }
    }

    func tile(at point: NSPoint) -> TileID? {
        lastLayout.tiles.first { $0.value.contains(point) }?.key
    }

    func terminalRect(for tile: TileID) -> NSRect {
        Workspace.terminalRect(in: lastLayout.tiles[tile] ?? area.bounds)
    }

    /// The terminal inside a pane's rect: below the header, inset a little so text never touches the
    /// ring (the margin shows the terminal's own background).
    static func terminalRect(in r: CGRect) -> NSRect {
        let i = Workspace.inset
        return NSRect(x: r.minX + i, y: r.minY + Workspace.headerHeight + 2, width: max(1, r.width - 2 * i),
                      height: max(1, r.height - Workspace.headerHeight - 2 - i))
    }

    // MARK: Layout

    func relayout() {
        lastLayout = displayRoot.layout(in: area.bounds, gap: Workspace.gap)
        syncChrome()
    }

    /// The interface font changed: pane headers take its size and height.
    func applyFonts() {
        for v in chrome.values { v.needsLayout = true }
        updateChromeState()
        relayout()
    }

    /// Create/remove chrome views to match the tree, then position everything.
    func syncChrome() {
        let ids = Set(tiles)
        // A pane closed mid-drag (Cmd+W with the header held) never gets its mouse up.
        if let d = tileDrag, !ids.contains(d.tile) { cancelTileDrag() }
        for (id, v) in chrome where !ids.contains(id) {
            v.isHidden = true
            v.findBar?.isHidden = true
            v.frame = .zero
            chrome[id] = nil
            spareChrome.append(v)
        }
        for id in tiles where chrome[id] == nil {
            let c = spareChrome.popLast() ?? TileChromeView(tile: id, workspace: self)
            c.tile = id
            c.isHidden = false
            chrome[id] = c
            if c.superview == nil { area.addSubview(c) }
        }
        let divs = lastLayout.dividers
        while dividers.count > divs.count { dividers.removeLast().removeFromSuperview() }
        while dividers.count < divs.count {
            let d = DividerView(workspace: self)
            dividers.append(d)
            area.addSubview(d)
        }
        positionAll()
        updateChromeState()
    }

    private func positionAll() {
        if Set(lastLayout.tiles.keys) != Set(tiles) { lastLayout = displayRoot.layout(in: area.bounds, gap: Workspace.gap) }
        for (id, c) in chrome {
            guard let r = lastLayout.tiles[id] else { continue }
            if c.frame != r { c.frame = r }
            if layout.contains(id), let v = pool.view(for: id) {
                let tr = terminalRect(for: id)
                if v.frame != tr { v.frame = tr }
            }
        }
        for (i, d) in lastLayout.dividers.enumerated() where i < dividers.count {
            dividers[i].divider = d
            let hit: CGFloat = 7
            let r = d.axis == .horizontal
                ? NSRect(x: d.rect.midX - hit / 2, y: d.rect.minY, width: hit, height: d.rect.height)
                : NSRect(x: d.rect.minX, y: d.rect.midY - hit / 2, width: d.rect.width, height: hit)
            dividers[i].frame = r
        }
    }

    /// What a zoomed pane's header says on its right (the header already says which tab it is).
    var zoomNote: String? {
        layout.isZoomed ? "zoomed · ⌘↩ shows all \(layout.count)" : nil
    }

    /// Ring, dimming, headers and placeholders from the current state and theme.
    func updateChromeState() {
        let theme = app.theme
        let ring = tiles.count >= 2
        let active = activeTile
        for (id, c) in chrome {
            let s = session(in: id)
            let hasTerminal = layout.contains(id) && app.isAlive(id)
            c.update(session: s, active: id == active, showRing: ring, windowKey: windowKey,
                     hasTerminal: hasTerminal, dropTarget: dropTile == id, zoomNote: zoomNote, theme: theme)
        }
        for d in dividers { d.color = theme.shadow; d.needsDisplay = true }
        area.layer?.backgroundColor = (GhosttyRuntime.shared.configColor("background") ?? theme.bg).cgColor
        owner?.findPaneMayHaveChanged()
    }

    func windowKeyChanged(_ key: Bool) {
        windowKey = key
        for v in pool.views.values where v.superview === area { v.windowKey = key }
        updateChromeState()
    }

    // MARK: Resizing: divider drags, window live resize, the sidebar's edge

    /// During a resize drag the panes' pixel frames follow the pointer, but their grid size (pty winsize,
    /// so a SIGWINCH and a full redraw in ink-based agents like Claude Code) follows at most this often,
    /// plus once at the end. See the self-test's busy-agent drag for how it was picked.
    static var resizeThrottle: TimeInterval = 0.1
    /// Resize drags under way (a window live resize can overlap a divider drag in theory).
    private var resizeDrags = 0
    /// Self-test hook: a divider drag step was laid out (seconds of main-thread work).
    var onDividerStep: ((Double) -> Void)?

    var resizing: Bool { resizeDrags > 0 }

    func resizeBegan() {
        resizeDrags += 1
        guard resizeDrags == 1 else { return }
        for s in visibleSessions { pool.view(for: s)?.resizeThrottle = Workspace.resizeThrottle }
    }

    func resizeEnded() {
        guard resizeDrags > 0 else { return }
        resizeDrags -= 1
        guard resizeDrags == 0 else { return }
        for s in visibleSessions {
            guard let v = pool.view(for: s) else { continue }
            v.flushGridSize()
            v.resizeThrottle = 0
        }
    }

    private var dragSignpost: OSSignpostIntervalState?

    func dividerDragBegan() {
        dragSignpost = signposter.beginInterval("DividerDrag", id: signposter.makeSignpostID())
        resizeBegan()
    }

    func dividerDragged(_ d: SplitNode.Divider, to point: NSPoint) {
        guard let id = currentRow else { return }
        let t0 = CACurrentMediaTime()
        rows[id]?.setRatio(at: d.path, SplitNode.ratio(for: d, at: point, gap: Workspace.gap))
        lastLayout = displayRoot.layout(in: area.bounds, gap: Workspace.gap)
        positionAll()
        onDividerStep?(CACurrentMediaTime() - t0)
    }

    func dividerDragEnded() {
        resizeEnded()
        if let sp = dragSignpost { signposter.endInterval("DividerDrag", sp) }
        dragSignpost = nil
        save()
    }

    // MARK: Moving panes by their header

    var tileDragActive: Bool { tileDrag != nil }
    /// The pane being dragged is outside the window: letting go opens it in a new window.
    private(set) var tileDragOutside = false
    private let tearOffPreview = TearOffPreview()
    var tearOffPreviewShown: Bool { tearOffPreview.isShown }

    /// A point in the area's coordinates on screen, and whether it's outside this window.
    private func screenPoint(_ p: NSPoint) -> (NSPoint, outside: Bool) {
        guard let w = area.window else { return (p, false) }
        let s = w.convertPoint(toScreen: area.convert(p, to: nil))
        return (s, !w.frame.contains(s))
    }
    /// Where the dragged pane would land right now (for the test modes): nil while a drop would do nothing.
    var tileDropTarget: (target: TileID, zone: TileDropZone)? {
        guard let d = tileDrag, let t = d.target, let z = d.zone else { return nil }
        return (t, z)
    }

    /// A header drag passed the threshold. Only with two or more panes, and never while the keyboard is lent out.
    func tileDragBegan(_ tile: TileID) -> Bool {
        guard tiles.count > 1, layout.contains(tile), owner.focus.exception == nil else { return false }
        tileDrag = (tile, nil, nil)
        area.addSubview(dropIndicator, positioned: .above, relativeTo: nil)
        dropIndicator.frame = area.bounds
        dropIndicator.show(nil, swap: false, theme: app.theme)
        NSCursor.closedHand.push()
        return true
    }

    /// The pointer moved during a pane drag (`p` in the area's coordinates). The indicator shows where
    /// the pane would go, and only for a drop that changes something.
    func tileDragMoved(to p: NSPoint) {
        guard var d = tileDrag else { return }
        var rect: CGRect?
        d.target = nil
        d.zone = nil
        let (screen, outside) = screenPoint(p)
        tileDragOutside = outside
        if outside {
            tearOffPreview.show(name: app.displayName(d.tile), at: screen, theme: app.theme)
        } else {
            tearOffPreview.hide()
        }
        if !outside, let t = tile(at: p), let r = lastLayout.tiles[t] {
            let zone = TileDropZone.at(p, in: r)
            if layout.root?.moving(d.tile, to: t, zone: zone) != nil {
                d.target = t
                d.zone = zone
                rect = zone.indicatorRect(in: r)
            }
        }
        tileDrag = d
        dropIndicator.show(rect, swap: d.zone == .swap, theme: app.theme)
    }

    /// Mouse up: move the pane if the pointer is over a drop that changes something; anywhere else cancels.
    func tileDragEnded(at p: NSPoint) {
        guard tileDrag != nil else { return }
        tileDragMoved(to: p)
        let d = tileDrag!
        let outside = tileDragOutside
        endTileDrag()
        if outside {
            // Let go outside the window: the pane becomes a row of its own in a new window there.
            app.movePaneToNewWindow(d.tile, at: screenPoint(p).0)
        } else if let target = d.target, let zone = d.zone {
            moveTile(d.tile, to: target, zone: zone)
        }
    }

    /// Esc during a drag.
    func cancelTileDrag() {
        guard tileDrag != nil else { return }
        endTileDrag()
    }

    private func endTileDrag() {
        tileDrag = nil
        tileDragOutside = false
        tearOffPreview.hide()
        dropIndicator.show(nil, swap: false, theme: app.theme)
        dropIndicator.removeFromSuperview()
        NSCursor.pop()
    }

    /// Move a pane within the row's tree. Its terminal view goes with it (no reattach); it stays active and keeps the keyboard.
    func moveTile(_ tile: TileID, to target: TileID, zone: TileDropZone) {
        guard let id = currentRow, rows[id]?.move(tile, to: target, zone: zone) == true else { return }
        owner.focus.setTiles(count: tiles.count, active: activeTile)
        relayout()
        focusActiveTerminal()
        app.workspaceChanged(self)
        save()
    }

    // MARK: Pointer

    func tileClicked(_ tile: TileID) {
        activate(tile)
    }

    /// Focus follows the mouse: called for real mouse-moved events in the window.
    func pointerMoved(_ event: NSEvent) {
        let p = area.convert(event.locationInWindow, from: nil)
        let over = area.bounds.contains(p) ? tile(at: p) : nil
        let screen = area.window?.convertPoint(toScreen: event.locationInWindow) ?? NSEvent.mouseLocation
        if let t = owner.focus.pointerMoved(to: screen, over: over, buttonsDown: NSEvent.pressedMouseButtons != 0) {
            activate(t)
        }
    }

    // MARK: Drops

    func dragUpdated(_ info: NSDraggingInfo) -> NSDragOperation {
        let p = area.convert(info.draggingLocation, from: nil)
        let t = tile(at: p)
        let ok = t.map { layout.contains($0) && app.isAlive($0) } ?? false
        let target = ok ? t : nil
        if target != dropTile {
            dropTile = target
            updateChromeState()
        }
        return ok ? .copy : []
    }

    func dragEnded() {
        if dropTile != nil {
            dropTile = nil
            updateChromeState()
        }
    }

    func performDrop(_ info: NSDraggingInfo) -> Bool {
        let p = area.convert(info.draggingLocation, from: nil)
        dragEnded()
        guard let t = tile(at: p), layout.contains(t), app.isAlive(t) else { return false }
        activate(t)
        // A drop closes the find bar on the pane, and the pane leaves copy mode (where the find left it, or
        // tmux's own keys) first, since copy mode would take the paste as its keys.
        if owner.findSession == t { owner.closeFind(leaveCopyMode: false) }
        // The drop is read straight away, while the drag's pasteboard is sure to hold it; the paste waits
        // for both.
        var dropped: String??, left = false
        let paste = { [weak self] in
            guard left, case .some(.some(let text)) = dropped, let v = self?.pool.view(for: t) else { return }
            v.paste(text)
        }
        app.daemon.command(.search(session: t, action: .end, text: "")) { _ in left = true; paste() }
        DropReader.read(info.draggingPasteboard) { text in dropped = .some(text); paste() }
        return true
    }
}

// MARK: - Views

final class MainAreaView: NSView {
    weak var workspace: Workspace?
    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        MainActor.assumeIsolated { workspace?.relayout() }
    }

    // A window live resize is a resize drag too: grid sizes are throttled until it ends.
    override func viewWillStartLiveResize() {
        super.viewWillStartLiveResize()
        MainActor.assumeIsolated { workspace?.resizeBegan() }
    }

    override func viewDidEndLiveResize() {
        super.viewDidEndLiveResize()
        MainActor.assumeIsolated { workspace?.resizeEnded() }
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation { workspace?.dragUpdated(sender) ?? [] }
    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation { workspace?.dragUpdated(sender) ?? [] }
    override func draggingExited(_ sender: NSDraggingInfo?) { workspace?.dragEnded() }
    override func draggingEnded(_ sender: NSDraggingInfo) { workspace?.dragEnded() }
    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool { workspace?.performDrop(sender) ?? false }
}

/// Per-pane chrome above the terminal: header strip, active ring or inactive border and dimming,
/// and the placeholder for an exited row or an empty main area. Clicks on an inactive tile stop here (consumed).
final class TileChromeView: NSView {
    /// The pane (its tab's session) this chrome is on; chrome views are reused across rows.
    var tile: TileID
    unowned let workspace: Workspace
    private let header = TileHeaderView()
    private let border = CALayer()
    private let dim = CALayer()
    private let dropHighlight = CALayer()
    private var placeholder: PlaceholderView?
    /// The find bar (Cmd+F), made the first time this pane wants one.
    private(set) var findBar: PaneFindBar?
    private(set) var isActive = false
    private var session: String?
    /// The session the header names, and the placeholder (for the test modes).
    var headerSession: String? { session }
    var headerStatus: (state: AgentState?, text: String, color: NSColor?) { header.shownStatus }
    var headerZoomNote: String? { header.shownZoomNote }
    var placeholderView: PlaceholderView? { placeholder }

    override var isFlipped: Bool { true }

    init(tile: TileID, workspace: Workspace) {
        self.tile = tile
        self.workspace = workspace
        super.init(frame: .zero)
        wantsLayer = true
        layer?.masksToBounds = true
        addSubview(header)
        layer?.addSublayer(dim)
        layer?.addSublayer(dropHighlight)
        layer?.addSublayer(border)
        border.zPosition = 10
        dropHighlight.zPosition = 9
        header.onClick = { [weak self] in
            guard let self else { return }
            self.workspace.tileClicked(self.tile)
        }
        header.onDoubleClick = { [weak self] in
            guard let self, self.workspace.layout.contains(self.tile) else { return }
            self.workspace.toggleZoom()
        }
        header.onDragBegan = { [weak self] in
            guard let self else { return false }
            return self.workspace.tileDragBegan(self.tile)
        }
        header.onDragMoved = { [weak self] e in
            guard let self else { return }
            self.workspace.tileDragMoved(to: self.workspace.area.convert(e.locationInWindow, from: nil))
        }
        header.onDragEnded = { [weak self] e in
            guard let self else { return }
            self.workspace.tileDragEnded(at: self.workspace.area.convert(e.locationInWindow, from: nil))
        }
        header.menuProvider = { [weak self] in
            guard let self, let s = self.session else { return nil }
            return self.workspace.app.sessionMenu(for: s)
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        header.frame = NSRect(x: 0, y: 0, width: bounds.width, height: Workspace.headerHeight)
        placeholder?.frame = NSRect(x: 0, y: Workspace.headerHeight, width: bounds.width, height: max(0, bounds.height - Workspace.headerHeight))
        if let f = findBar {
            // In the terminal's top right corner, over the end of its first line, where tmux writes its
            // copy-mode position ("[0/76] (22 results)") while the search runs.
            let t = Workspace.terminalRect(in: bounds)
            let w = min(PaneFindBar.width, bounds.width - 24)
            f.frame = NSRect(x: t.maxX - w, y: t.minY, width: max(0, w), height: PaneFindBar.height)
        }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        border.frame = bounds
        dim.frame = NSRect(x: 0, y: Workspace.headerHeight, width: bounds.width, height: max(0, bounds.height - Workspace.headerHeight))
        dropHighlight.frame = bounds.insetBy(dx: 2, dy: 2)
        CATransaction.commit()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        needsLayout = true
        layoutSubtreeIfNeeded()
    }

    func update(session: String?, active: Bool, showRing: Bool, windowKey: Bool, hasTerminal: Bool, dropTarget: Bool,
                zoomNote: String?, theme: AppTheme) {
        self.session = session
        isActive = active
        if let f = findBar, !f.isHidden { f.theme = theme }
        let app = workspace.app
        header.draggable = showRing
        header.update(name: session.map { app.displayName($0) }, group: session.flatMap { app.groupLabel($0) },
                      state: session.map { app.displayState($0) }, detail: session.flatMap { app.statusDetail($0) },
                      elapsed: session.flatMap { app.turnElapsed($0) },
                      exitedText: session.flatMap { app.daemon.state.sidebar.container(holding: $0) }
                          .map { app.daemon.state.sidebar.role(ofRow: $0.id) == .docked ? "docked" : "exited" } ?? "exited",
                      exited: session.map { !app.isAlive($0) } ?? false, active: active && showRing,
                      zoomNote: hasTerminal ? zoomNote : nil, theme: theme)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if showRing && active {
            border.borderWidth = 2
            border.borderColor = (windowKey ? theme.accent : theme.accent.withAlphaComponent(0.4)).cgColor
        } else if showRing {
            border.borderWidth = 1
            border.borderColor = theme.panelAlt.cgColor
        } else {
            border.borderWidth = 0
        }
        dim.backgroundColor = (GhosttyRuntime.shared.configColor("background") ?? theme.bg).withAlphaComponent(0.4).cgColor
        dim.isHidden = !(showRing && !active && hasTerminal)
        dropHighlight.backgroundColor = theme.accent.withAlphaComponent(0.12).cgColor
        dropHighlight.borderColor = theme.accent.cgColor
        dropHighlight.borderWidth = 2
        dropHighlight.cornerRadius = 4
        dropHighlight.isHidden = !dropTarget
        CATransaction.commit()

        if hasTerminal {
            placeholder?.removeFromSuperview()
            placeholder = nil
        } else {
            if placeholder == nil {
                let p = PlaceholderView()
                placeholder = p
                addSubview(p, positioned: .below, relativeTo: header)
                needsLayout = true
            }
            placeholder?.configure(session: session, app: app, theme: theme)
        }
        layer?.backgroundColor = hasTerminal ? nil : theme.bg.cgColor
    }

    /// Inactive tiles take the click (to activate, consumed). The active tile lets it through to the terminal.
    override func hitTest(_ point: NSPoint) -> NSView? {
        // A spare chrome view (kept for the next row's panes) is hidden and must never take a click.
        guard !isHidden else { return nil }
        let p = convert(point, from: superview)
        guard bounds.contains(p) else { return nil }
        if p.y < Workspace.headerHeight { return header }
        if let f = findBar, !f.isHidden, f.frame.contains(p) { return f.hitTest(p) ?? f }
        if !isActive { return self }
        if let placeholder, let hit = placeholder.hitTest(convert(p, to: placeholder.superview)) { return hit }
        return placeholder != nil ? self : nil
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    /// Show the find bar, above the terminal at the top right.
    func showFindBar() -> PaneFindBar {
        let f = findBar ?? {
            let f = PaneFindBar()
            f.onChange = { [weak self] in self?.workspace.owner.findStep(.find) }
            f.onStep = { [weak self] in self?.workspace.owner.findStep($0) }
            f.onClose = { [weak self] in self?.workspace.owner.closeFind(leaveCopyMode: true) }
            f.onMatchCase = { [weak self] in self?.workspace.owner.toggleFindMatchCase() }
            addSubview(f, positioned: .above, relativeTo: header)
            findBar = f
            return f
        }()
        f.theme = workspace.app.theme
        f.isHidden = false
        needsLayout = true
        layoutSubtreeIfNeeded()
        return f
    }

    /// The terminal a Cmd+click on this inactive tile went on to, which gets the mouse-up too.
    private var forwardedClick: TerminalView?

    override func mouseDown(with event: NSEvent) {
        workspace.tileClicked(tile)
        // Cmd+click opens the link under it in a pane that wasn't active too (libghostty decides
        // whether there is one); a plain click only activates.
        if event.modifierFlags.contains(.command), let t = workspace.terminal(in: tile) {
            forwardedClick = t
            t.mouseDown(with: event)
        }
    }

    override func mouseDragged(with event: NSEvent) {
        forwardedClick?.mouseDragged(with: event)
    }

    override func mouseUp(with event: NSEvent) {
        forwardedClick?.mouseUp(with: event)
        forwardedClick = nil
    }

    override func scrollWheel(with event: NSEvent) {
        // Scrolling an inactive tile still scrolls its terminal.
        workspace.terminal(in: tile)?.scrollWheel(with: event)
    }
}

/// The thin strip above each terminal: status, session name, waiting detail.
final class TileHeaderView: NSView {
    private let badge = StatusBadge(frame: NSRect(x: 0, y: 0, width: 10, height: 10))
    private let name = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")
    private let zoom = NSTextField(labelWithString: "")
    private let bottomLine = CALayer()
    var onClick: (@MainActor () -> Void)?
    /// A double click zooms the pane, or unzooms the row.
    var onDoubleClick: (@MainActor () -> Void)?
    var menuProvider: (@MainActor () -> NSMenu?)?
    /// Dragging the header moves the tile (only with two or more tiles).
    var onDragBegan: (@MainActor () -> Bool)?
    var onDragMoved: (@MainActor (NSEvent) -> Void)?
    var onDragEnded: (@MainActor (NSEvent) -> Void)?
    var draggable = false {
        didSet { if draggable != oldValue { window?.invalidateCursorRects(for: self) } }
    }
    private var downAt: NSPoint?
    private var dragging = false

    override var isFlipped: Bool { true }

    /// The badge's state, the text beside the name and its colour (for the test modes).
    var shownStatus: (state: AgentState?, text: String, color: NSColor?) {
        (badge.isHidden ? nil : badge.state, detail.stringValue, detail.textColor)
    }
    /// The zoom note on the right ("zoomed · 2 of 3"), nil when the row isn't zoomed (for the test modes).
    var shownZoomNote: String? { zoom.isHidden ? nil : zoom.stringValue }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        for l in [name, detail, zoom] {
            l.lineBreakMode = .byTruncatingTail
            l.isSelectable = false
            l.refusesFirstResponder = true
            addSubview(l)
        }
        zoom.alignment = .right
        zoom.isHidden = true
        addSubview(badge)
        layer?.addSublayer(bottomLine)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        let h = bounds.height
        badge.frame = NSRect(x: 10, y: (h - 10) / 2, width: 10, height: 10)
        name.sizeToFit()
        let nameW = min(name.frame.width, bounds.width * 0.55)
        name.frame = NSRect(x: 27, y: (h - name.frame.height) / 2, width: nameW, height: name.frame.height)
        var right = bounds.width - 10
        if !zoom.isHidden {
            zoom.sizeToFit()
            let zw = min(zoom.frame.width, max(0, bounds.width * 0.4))
            zoom.frame = NSRect(x: right - zw, y: (h - zoom.frame.height) / 2, width: zw, height: zoom.frame.height)
            right = zoom.frame.minX - 8
        }
        detail.sizeToFit()
        let dx = name.frame.maxX + 8
        detail.frame = NSRect(x: dx, y: (h - detail.frame.height) / 2, width: max(0, right - dx), height: detail.frame.height)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        bottomLine.frame = NSRect(x: 0, y: h - 1, width: bounds.width, height: 1)
        CATransaction.commit()
    }

    func update(name n: String?, group: String?, state: AgentState?, detail d: String?, elapsed: String? = nil, exitedText: String = "exited", exited: Bool, active: Bool,
                zoomNote: String?, theme: AppTheme) {
        name.font = Fonts.ui(12, .semibold)
        detail.font = Fonts.ui(11.5)
        zoom.font = Fonts.ui(11, .medium)
        zoom.isHidden = zoomNote == nil
        zoom.stringValue = zoomNote ?? ""
        zoom.textColor = theme.accent
        layer?.backgroundColor = (active ? AppTheme.mix(theme.panel, theme.accent, 0.08) : theme.panel).cgColor
        bottomLine.backgroundColor = theme.shadow.cgColor
        if let n {
            name.stringValue = n
            name.textColor = exited ? theme.textMuted : theme.text
            badge.isHidden = false
            badge.set(state ?? .unknown, exited: exited, theme: theme)
        } else {
            name.stringValue = "No session"
            name.textColor = theme.textMuted
            badge.isHidden = true
        }
        var text = ""
        var color = theme.textMuted
        if exited {
            text = exitedText
        } else if let state {
            switch state {
            case .waiting:
                text = d.map { "waiting · \($0)" } ?? "waiting for you"
                color = theme.waiting
            case .error:
                text = d ?? "error"
                color = theme.error
            case .done:
                text = "done"
                color = theme.done
            case .seen:
                text = "done · read"
            case .running:
                text = [d ?? (elapsed == nil ? nil : "running"), elapsed].compactMap { $0 }.joined(separator: " · ")
            default:
                text = ""
            }
        }
        if text.isEmpty, let group, !exited {
            text = group
            color = theme.textMuted
        }
        detail.stringValue = text
        detail.textColor = color
        needsLayout = true
    }

    override func mouseDown(with event: NSEvent) {
        downAt = event.locationInWindow
        dragging = false
        onClick?()
        if event.clickCount == 2 { onDoubleClick?() }
    }

    override func mouseDragged(with event: NSEvent) {
        guard let start = downAt else { return }
        if !dragging {
            let p = event.locationInWindow
            guard hypot(p.x - start.x, p.y - start.y) >= 4 else { return }
            guard onDragBegan?() == true else { downAt = nil; return }
            dragging = true
        }
        onDragMoved?(event)
    }

    override func mouseUp(with event: NSEvent) {
        if dragging { onDragEnded?(event) }
        downAt = nil
        dragging = false
    }

    override func resetCursorRects() {
        if draggable { addCursorRect(bounds, cursor: .openHand) }
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func menu(for event: NSEvent) -> NSMenu? { menuProvider?() }
}

/// The translucent box over the tile a dragged tile would land on: the half it would take, or the
/// whole tile (inset) with "Swap" for a swap. Never takes clicks.
final class TileDropIndicatorView: NSView {
    private let box = CALayer()
    private let label = CATextLayer()
    private(set) var shownRect: CGRect?
    private(set) var showsSwap = false

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.addSublayer(box)
        box.addSublayer(label)
        box.cornerRadius = 8
        box.borderWidth = 2
        label.alignmentMode = .center
        label.fontSize = 13
        label.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        label.string = "Swap"
        box.isHidden = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    func show(_ rect: CGRect?, swap: Bool, theme: AppTheme) {
        shownRect = rect
        showsSwap = rect != nil && swap
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        guard let rect else { box.isHidden = true; return }
        box.isHidden = false
        box.frame = rect.insetBy(dx: swap ? 10 : 5, dy: swap ? 10 : 5)
        box.backgroundColor = theme.accent.withAlphaComponent(0.2).cgColor
        box.borderColor = theme.accent.withAlphaComponent(0.85).cgColor
        label.isHidden = !swap
        label.foregroundColor = theme.text.cgColor
        label.contentsScale = window?.backingScaleFactor ?? 2
        label.frame = CGRect(x: 0, y: (box.bounds.height - 18) / 2, width: box.bounds.width, height: 18)
    }
}

/// What follows the pointer while a pane is dragged outside its window: a small card saying the pane
/// will open in a new window. It never takes clicks or the keyboard.
@MainActor
final class TearOffPreview {
    private var panel: NSPanel?
    private let label = NSTextField(labelWithString: "")
    private let sub = NSTextField(labelWithString: "New window")
    /// Whether it's showing (for the test modes).
    var isShown: Bool { panel?.isVisible == true }

    func show(name: String, at p: NSPoint, theme: AppTheme) {
        let size = NSSize(width: 220, height: 54)
        if panel == nil {
            let w = NSPanel(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless, .nonactivatingPanel],
                            backing: .buffered, defer: true)
            w.isOpaque = false
            w.backgroundColor = .clear
            w.hasShadow = true
            w.ignoresMouseEvents = true
            w.level = .floating
            w.isReleasedWhenClosed = false
            let v = NSView(frame: NSRect(origin: .zero, size: size))
            v.wantsLayer = true
            v.layer?.cornerRadius = 8
            v.layer?.borderWidth = 2
            label.font = .systemFont(ofSize: 12.5, weight: .semibold)
            sub.font = .systemFont(ofSize: 11)
            for l in [label, sub] {
                l.lineBreakMode = .byTruncatingTail
                v.addSubview(l)
            }
            label.frame = NSRect(x: 12, y: 28, width: size.width - 24, height: 17)
            sub.frame = NSRect(x: 12, y: 10, width: size.width - 24, height: 15)
            w.contentView = v
            panel = w
        }
        guard let panel, let v = panel.contentView else { return }
        v.layer?.backgroundColor = theme.panel.withAlphaComponent(0.94).cgColor
        v.layer?.borderColor = theme.accent.cgColor
        label.stringValue = name
        label.textColor = theme.text
        sub.textColor = theme.accent
        panel.setFrameOrigin(NSPoint(x: p.x + 14, y: p.y - size.height - 10))
        if !panel.isVisible { panel.orderFront(nil) }
    }

    func hide() { panel?.orderOut(nil) }
}

/// What an exited row (every tab ended) or an empty main area (no rows at all) shows. Text wraps and stays centred however narrow the
/// tile gets; the buttons stack when they don't fit side by side, and what doesn't fit at all is left out.
final class PlaceholderView: NSView {
    let title = NSTextField(wrappingLabelWithString: "")
    let hint = NSTextField(wrappingLabelWithString: "")
    let primary = NSButton(title: "", target: nil, action: nil)
    let secondary = NSButton(title: "", target: nil, action: nil)
    private var primaryAction: (() -> Void)?
    private var secondaryAction: (() -> Void)?
    static let margin: CGFloat = 14

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        for l in [title, hint] {
            l.alignment = .center
            l.refusesFirstResponder = true
            l.isSelectable = false
            l.maximumNumberOfLines = 4
            l.cell?.truncatesLastVisibleLine = true
            l.lineBreakMode = .byWordWrapping
            addSubview(l)
        }
        hint.maximumNumberOfLines = 6
        for (b, sel) in [(primary, #selector(primaryClicked)), (secondary, #selector(secondaryClicked))] {
            b.bezelStyle = .push
            b.controlSize = .regular
            b.refusesFirstResponder = true
            b.lineBreakMode = .byTruncatingTail
            b.target = self
            b.action = sel
            addSubview(b)
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    @MainActor
    func configure(session: String?, app: AppController, theme: AppTheme) {
        title.font = Fonts.ui(15, .semibold)
        hint.font = Fonts.ui(12.5)
        title.textColor = theme.text
        hint.textColor = theme.textMuted
        if let session, let c = app.daemon.state.sidebar.container(holding: session), app.daemon.state.sidebar.role(ofRow: c.id) == .docked {
            title.stringValue = "\(app.displayName(c.active)) is docked"
            hint.stringValue = "Docking ended its sessions. Undock it to put the row back where it was and resume each agent's conversation."
            primary.title = "Undock and Resume"
            primaryAction = { app.unpark(c.id) }
            secondaryAction = nil
            secondary.isHidden = true
        } else if let session {
            title.stringValue = "\(app.displayName(session)) has exited"
            hint.stringValue = "The tmux session ended. Restart it from its saved spec, or resume the agent's last conversation."
            primary.title = "Restart"
            secondary.title = "Resume"
            primaryAction = { app.restartExited(session, resume: false) }
            secondaryAction = { app.restartExited(session, resume: true) }
            secondary.isHidden = false
        } else {
            title.stringValue = "No sessions yet"
            hint.stringValue = "Each session is a row in the sidebar, and its tabs open side by side here. Start one with ⇧⌘N."
            primary.title = "New Session…"
            secondary.title = ""
            primaryAction = { app.newSession() }
            secondaryAction = nil
            secondary.isHidden = true
        }
        needsLayout = true
    }

    /// Height of a wrapping label at `width`.
    private func height(_ l: NSTextField, _ width: CGFloat) -> CGFloat {
        guard !l.stringValue.isEmpty, width > 0, let cell = l.cell else { return 0 }
        return ceil(cell.cellSize(forBounds: NSRect(x: 0, y: 0, width: width, height: .greatestFiniteMagnitude)).height)
    }

    override func layout() {
        super.layout()
        let m = PlaceholderView.margin
        let w = max(0, min(bounds.width - 2 * m, 420))
        let x = (bounds.width - w) / 2
        let th = height(title, w)
        let hh = height(hint, w)
        primary.sizeToFit()
        secondary.sizeToFit()
        let two = secondaryAction != nil && !secondary.title.isEmpty
        let gap: CGFloat = 10
        let pw = min(primary.frame.width, w), sw = min(secondary.frame.width, w)
        let sideBySide = !two || pw + gap + sw <= w
        let buttonsH: CGFloat = sideBySide ? 28 : 28 * 2 + 8
        // Leave out what doesn't fit: first the buttons, then the hint.
        let avail = bounds.height - 2 * 10
        var showHint = hh > 0, showButtons = w >= 60
        func total() -> CGFloat { th + (showHint ? 8 + hh : 0) + (showButtons ? 14 + buttonsH : 0) }
        if total() > avail { showButtons = false }
        if total() > avail { showHint = false }
        hint.isHidden = !showHint
        primary.isHidden = !showButtons
        secondary.isHidden = !showButtons || !two
        var y = max(10, (bounds.height - total()) / 2)
        title.frame = NSRect(x: x, y: y, width: w, height: min(th, max(0, bounds.height - y)))
        y += th + 8
        if showHint {
            hint.frame = NSRect(x: x, y: y, width: w, height: hh)
            y += hh + 14
        } else {
            y += 6
        }
        guard showButtons else { return }
        if sideBySide {
            var bx = (bounds.width - (pw + (two ? gap + sw : 0))) / 2
            primary.frame = NSRect(x: bx, y: y, width: pw, height: 28)
            bx += pw + gap
            secondary.frame = NSRect(x: bx, y: y, width: sw, height: 28)
        } else {
            let bw = min(w, max(pw, sw))
            primary.frame = NSRect(x: (bounds.width - bw) / 2, y: y, width: bw, height: 28)
            secondary.frame = NSRect(x: (bounds.width - bw) / 2, y: y + 36, width: bw, height: 28)
        }
    }

    @objc private func primaryClicked() { primaryAction?() }
    @objc private func secondaryClicked() { secondaryAction?() }
}

/// A thin divider between tiles; the hit area is wider than the line it draws.
final class DividerView: NSView {
    unowned let workspace: Workspace
    var divider: SplitNode.Divider?
    var color: NSColor = .separatorColor
    private var dragging = false

    override var isFlipped: Bool { true }

    init(workspace: Workspace) {
        self.workspace = workspace
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func draw(_ dirtyRect: NSRect) {
        guard let d = divider else { return }
        color.setFill()
        if d.axis == .horizontal {
            NSRect(x: (bounds.width - Workspace.gap) / 2, y: 0, width: Workspace.gap, height: bounds.height).fill()
        } else {
            NSRect(x: 0, y: (bounds.height - Workspace.gap) / 2, width: bounds.width, height: Workspace.gap).fill()
        }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: divider?.axis == .vertical ? .resizeUpDown : .resizeLeftRight)
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        dragging = true
        workspace.dividerDragBegan()
    }

    override func mouseDragged(with event: NSEvent) {
        guard dragging, let d = divider, let sv = superview else { return }
        workspace.dividerDragged(d, to: sv.convert(event.locationInWindow, from: nil))
    }

    override func mouseUp(with event: NSEvent) {
        guard dragging else { return }
        dragging = false
        workspace.dividerDragEnded()
    }
}

/// Turns a drag pasteboard into the text a terminal should receive: escaped file paths (space separated),
/// image data written to a temp file first, or plain text.
@MainActor
enum DropReader {
    static let types: [NSPasteboard.PasteboardType] = [.fileURL, .png, .tiff, .string, .URL,
        NSPasteboard.PasteboardType(UTType.jpeg.identifier), NSPasteboard.PasteboardType(UTType.heic.identifier)]
        + NSFilePromiseReceiver.readableDraggedTypes.map { NSPasteboard.PasteboardType($0) }

    static var dropDir: URL {
        let d = Paths.cacheDir.appendingPathComponent("drops", isDirectory: true)
        try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    static func canRead(_ pb: NSPasteboard) -> Bool {
        guard let types = pb.types else { return false }
        return !Set(types).isDisjoint(with: Set(Self.types))
    }

    static func read(_ pb: NSPasteboard, _ done: @escaping (String?) -> Void) {
        // 1. Files.
        if let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty {
            done(ShellEscape.dropText(paths: urls.map(\.path)))
            return
        }
        // 2. File promises (Photos, Mail, some screenshot thumbnails).
        if let promises = pb.readObjects(forClasses: [NSFilePromiseReceiver.self], options: nil) as? [NSFilePromiseReceiver], !promises.isEmpty {
            let queue = OperationQueue()
            let dest = dropDir
            var paths: [String] = []
            let group = DispatchGroup()
            let lock = NSLock()
            for p in promises {
                group.enter()
                p.receivePromisedFiles(atDestination: dest, options: [:], operationQueue: queue) { url, error in
                    if error == nil {
                        lock.lock()
                        paths.append(url.path)
                        lock.unlock()
                    }
                    group.leave()
                }
            }
            group.notify(queue: .main) {
                lock.lock()
                let got = paths
                lock.unlock()
                done(got.isEmpty ? nil : ShellEscape.dropText(paths: got))
            }
            return
        }
        // 3. Image data that isn't a file (a browser image, a screenshot thumbnail): write it out first.
        if let path = writeImage(pb) {
            done(ShellEscape.dropText(paths: [path]))
            return
        }
        // 4. Text.
        done(pb.string(forType: .string) ?? (pb.readObjects(forClasses: [NSURL.self]) as? [URL])?.first?.absoluteString)
    }

    static func writeImage(_ pb: NSPasteboard) -> String? {
        let stamp = ISO8601DateFormatter().string(from: Date()).replacingOccurrences(of: ":", with: "-")
        let name = "drop-\(stamp)-\(UInt32.random(in: 0...0xffff))"
        if let png = pb.data(forType: .png) {
            let url = dropDir.appendingPathComponent("\(name).png")
            if (try? png.write(to: url)) != nil { return url.path }
        }
        if let jpg = pb.data(forType: NSPasteboard.PasteboardType(UTType.jpeg.identifier)) {
            let url = dropDir.appendingPathComponent("\(name).jpg")
            if (try? jpg.write(to: url)) != nil { return url.path }
        }
        if let data = pb.data(forType: .tiff) ?? pb.data(forType: NSPasteboard.PasteboardType(UTType.heic.identifier)),
           let rep = NSBitmapImageRep(data: data), let png = rep.representation(using: .png, properties: [:]) {
            let url = dropDir.appendingPathComponent("\(name).png")
            if (try? png.write(to: url)) != nil { return url.path }
        }
        return nil
    }
}
