import AppKit
import BosunCore
import SwiftUI

/// Ties the daemon, the windows (each a sidebar and a main area) and the warm pool together, and owns
/// the focus rules, key routing, menus and commands. Menus and keys act on the key window (`current`).
@MainActor
final class AppController: NSObject, NSWindowDelegate, NSMenuDelegate, NSMenuItemValidation {
    let daemon = DaemonClient()
    /// Every Bosun window, the first one first (it never closes on its own, and its frame is kept by name).
    private(set) var windows: [BosunWindow] = []
    /// The key window, or the last one that was (menus, keys and sheets act on it).
    private(set) var current: BosunWindow!
    /// The warm pool, shared by every window.
    private(set) var pool: TerminalPool!
    /// Every row's layout and the pane bookkeeping, shared by every window.
    let rowStore = RowStore()
    var workspace: Workspace { current.workspace }
    var sidebar: SidebarController { current.sidebar }
    var focus: FocusModel {
        get { current.focus }
        set { current.focus = newValue }
    }
    var window: MainWindow { current.window }
    private var root: RootView { current.root }
    private var palette: PaletteView? {
        get { current.palette }
        set { current.palette = newValue }
    }
    private(set) var shortcutsPanel: ShortcutsPanel? {
        get { current.shortcutsPanel }
        set { current.shortcutsPanel = newValue }
    }
    private var sendNextPending: Bool {
        get { current.sendNextPending }
        set { current.sendNextPending = newValue }
    }
    private var seen = SeenTracker()
    private(set) var theme = AppTheme(.fallback)
    private(set) var nav = NavBindings(nil) { didSet { updateNavMenu() } }
    private var navItems: [NavAction: NSMenuItem] = [:]
    private var settings: SettingsWindowController?
    var settingsWindow: NSWindow? { settings?.window }
    private var waitingSince: [String: Date] = [:]
    private var monitors: [Any] = []
    private(set) var themeMenu = NSMenu(title: "Theme")
    private var themeBeforePreview: String?
    /// The palette the terminals were last coloured with ("match terminal to Bosun theme"); nil is the user's Ghostty theme.
    private(set) var appliedTerminalTheme: ThemePalette?
    /// The macOS appearance the chrome follows when a Light/Dark pair is set (design 7a).
    private(set) var appearance: ThemeChoice.Appearance = .dark
    private var appearanceObservation: NSKeyValueObservation?
    /// The Settings window's Light/Dark pickers.
    let themePairModel = ThemePairModel()
    let editorModel = EditorModel()
    /// Launch measurements (seconds since process start).
    private(set) var sidebarPaintedAt: Double?
    private(set) var firstTerminalAt: Double?
    var onFirstTerminal: (() -> Void)?
    /// Test modes run where the window can't become key (a locked screen); they treat `current` as key.
    var assumeKeyForTests = false
    var windowIsKey: Bool { isKey(current) }
    func isKey(_ bw: BosunWindow) -> Bool { bw.window.isKeyWindow || (assumeKeyForTests && bw === current) }
    /// When to notify about a session (design 10).
    private var attention = Attention()
    /// Notifications go to the system, or to a recorder in the test modes and the demo (they never post for real).
    let notifier: any Notifier = testMode || demoMode ? RecordingNotifier() : SystemNotifier()
    /// Whether you can see a window right now (`BosunWindow.onScreen`). The test modes replace it.
    var windowSeen: (BosunWindow) -> Bool = { _ in false }
    /// The first live snapshot has been taken as the notifications' baseline.
    private var attentionLive = false
    /// The Dock badge as last set, and how many times it was set (the tests check it only changes with the count).
    private(set) var dockBadgeLabel: String?
    private(set) var dockBadgeUpdates = 0
    private var inputsTimer: Timer?

    // MARK: Startup

    func start() {
        pool = TerminalPool(daemon: daemon, capacity: AppSettings.shared.poolSize)
        pool.onExit = { [weak self] s in self?.terminalExited(s) }
        pool.onInput = { [weak self] s in self?.terminalInput(s) }
        let primary = BosunWindow(app: self, frame: nil)
        windows = [primary]
        current = primary
        buildMenu()
        installMonitors()
        LinkOpener.sessionPath = { [weak self] in self?.daemon.state.sessions[$0]?.path }
        LinkOpener.editor = { [weak self] in self?.daemon.state.config.value("editor")?.stringValue }

        daemon.onChanges.append { [weak self] in self?.stateChanged($0) }
        daemon.onStatus.append { [weak self] in self?.statusChanged($0) }
        AppSettings.shared.onChange.append { [weak self] in self?.settingsChanged() }
        NotificationCenter.default.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.settingsChanged() }
        }
        windowSeen = { $0.onScreen }
        notifier.onOpen = { [weak self] in self?.notificationClicked(userInfo: $0) }
        NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.activeSpaceDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.reviewSeen() }
        }
        loadInputs()

        appearance = Self.systemAppearance()
        appearanceObservation = NSApp.observe(\.effectiveAppearance, options: [.new]) { [weak self] _, _ in
            // KVO fires on the thread that changed it, which for NSApp's appearance is the main thread.
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.appearanceChanged() } }
        }
        themePairModel.onPreview = { [weak self] name in self?.previewTheme(name) }
        themePairModel.onPreviewEnd = { [weak self] chosen in self?.endThemePreview(chosen: chosen) }
        themePairModel.onSet = { [weak self] key, name in
            self?.daemon.command(.applySetting(key: key, value: name.map { .string($0) } ?? .null)) { [weak self] in self?.report($0) }
        }
        editorModel.onSet = { [weak self] editor in
            self?.daemon.command(.applySetting(key: "editor", value: editor.map { .string($0) } ?? .null)) { [weak self] in self?.report($0) }
        }

        // 1. First frame from the cached snapshot: no I/O beyond one small file.
        let syncNote = AppSettings.shared.migrateTerminalSync()
        let cached = daemon.loadCache()
        if !cached { applyTheme() }
        restoreWindows()
        for bw in windows.reversed() {
            bw.focus.setTiles(count: bw.workspace.tiles.count, active: bw.workspace.activeTile)
            if noActivate || bw !== primary { bw.window.orderFront(nil) } else { bw.window.makeKeyAndOrderFront(nil) }
            bw.root.layoutSubtreeIfNeeded()
            bw.workspace.relayout()
            bw.sidebar.reload()
            bw.applyTheme()
        }
        if !noActivate { NSApp.activate() }
        if syncNote { primary.root.band.flash(TerminalSync.note, seconds: 8) }
        CATransaction.setCompletionBlock { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.sidebarPaintedAt == nil else { return }
                self.sidebarPaintedAt = ProcessClock.sinceLaunch()
                signposter.emitEvent("SidebarPainted")
            }
        }
        window.displayIfNeeded()
        print("window-id \(window.windowNumber) pid \(getpid())")
        fflush(stdout)

        // 2. Connect in parallel; 3. attach the shown row's panes straight away (they only need tmux).
        daemon.start()
        statusChanged(daemon.status)
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { for bw in self?.windows ?? [] { bw.workspace.startAttaching() } }
        }
    }

    // MARK: Windows

    func controller(for w: NSWindow?) -> BosunWindow? { windows.first { $0.window === w } }

    /// The window showing row `id`, if any.
    func window(showing id: String) -> BosunWindow? { windows.first { $0.workspace.currentRow == id } }

    /// The window showing `session` as one of its panes on screen, else the one showing its row.
    func window(showingSession s: String) -> BosunWindow? {
        windows.first { $0.workspace.visibleSessions.contains(s) }
            ?? daemon.state.sidebar.container(holding: s).flatMap { window(showing: $0.id) }
    }

    func isShown(row id: String, elsewhereThan bw: BosunWindow?) -> Bool {
        windows.contains { $0 !== bw && $0.workspace.currentRow == id }
    }

    /// Every window's panes on screen.
    var visibleSessions: Set<String> { windows.reduce(into: Set<String>()) { $0.formUnion($1.workspace.visibleSessions) } }

    /// Make `bw` the window menus and keys act on. The test modes can't make a window key (the screen
    /// may be locked), so there it's just marked key in its place.
    func setCurrent(_ bw: BosunWindow) {
        guard bw !== current else { return }
        let old = current
        current = bw
        if assumeKeyForTests {
            old?.focus.setWindowKey(false)
            old?.workspace.windowKeyChanged(false)
            bw.focus.setWindowKey(true)
            bw.workspace.windowKeyChanged(true)
        }
    }

    /// Bring a window to the front with the keyboard in it.
    func bringForward(_ bw: BosunWindow) {
        if bw.window.isMiniaturized { bw.window.deminiaturize(nil) }
        if testMode && !realInput {
            // Key without activating the app, so a test run never takes the keyboard from what you're doing.
            bw.window.orderFront(nil)
            bw.window.makeKey()
        } else {
            bw.window.makeKeyAndOrderFront(nil)
        }
        setCurrent(bw)
    }

    /// A new window showing row `id` (nil: the first row no window shows). `frame` places it; by default
    /// it cascades from the key window.
    @discardableResult
    func openWindow(row id: String?, frame: NSRect? = nil, front: Bool = true) -> BosunWindow? {
        let st = daemon.state
        let row = id ?? st.sidebar.allRows.first { c in !isShown(row: c.id, elsewhereThan: nil) && c.tabs.contains { isAlive($0.session) } }?.id
        guard let row, !isShown(row: row, elsewhereThan: nil) else { NSSound.beep(); return nil }
        var f = frame
        if f == nil, let from = current?.window {
            var r = from.frame
            let top = from.cascadeTopLeft(from: NSPoint(x: r.minX, y: r.maxY))
            r.origin = NSPoint(x: top.x, y: top.y - r.height)
            f = r
        }
        let bw = BosunWindow(app: self, frame: f)
        windows.append(bw)
        bw.workspace.restore(row: row)
        bw.root.layoutSubtreeIfNeeded()
        bw.workspace.relayout()
        bw.sidebar.reload()
        bw.applyTheme()
        bw.sidebar.setStatus(headerStatus)
        if hadLiveSnapshot || st.server != nil { bw.workspace.startAttaching() }
        if front { bringForward(bw) } else { bw.window.orderFront(nil) }
        if assumeKeyForTests, bw === current { bw.focus.setWindowKey(true); bw.workspace.windowKeyChanged(true) }
        for w in windows { w.sidebar.refreshRows() }
        saveLayouts()
        return bw
    }

    // MARK: Tear-off

    /// Panes moving out into rows of their own, to open in a new window once the daemon's sidebar shows
    /// the new row: session → (its old row, where the window goes, when it was asked for).
    private var pendingTearOffs: [String: (from: String, frame: NSRect?, at: Date)] = [:]

    /// A new window's frame with its top left near `p` (a drop point on screen), the key window's size,
    /// kept on that screen. nil without a point (the window cascades instead).
    func tearOffFrame(at p: NSPoint?) -> NSRect? {
        guard let p else { return nil }
        let size = current?.window.frame.size ?? NSSize(width: 1100, height: 720)
        var r = NSRect(x: p.x - 60, y: p.y - size.height + 12, width: size.width, height: size.height)
        if let screen = NSScreen.screens.first(where: { $0.frame.contains(p) }) ?? NSScreen.main {
            let v = screen.visibleFrame
            r.size.width = min(r.width, v.width)
            r.size.height = min(r.height, v.height)
            r.origin.x = min(max(r.minX, v.minX), v.maxX - r.width)
            r.origin.y = min(max(r.minY, v.minY), v.maxY - r.height)
        }
        return r
    }

    /// Whether a screen point is over one of the app's windows (a drag let go there isn't a tear-off).
    func isOverBosunWindow(_ p: NSPoint) -> Bool { windows.contains { $0.window.isVisible && $0.window.frame.contains(p) } }

    /// Open row `id` in a window of its own (a row dragged out of the sidebar, or its menu). The window
    /// showing it now moves on to the next row no window shows; with none, nothing happens.
    func openRowInNewWindow(_ id: String, at p: NSPoint? = nil) {
        guard daemon.state.sidebar.container(id: id) != nil else { return }
        if let from = window(showing: id), !from.workspace.leaveRow() {
            NSSound.beep()
            root.band.flash("Nothing else for this window to show")
            return
        }
        openWindow(row: id, frame: tearOffFrame(at: p))
    }

    /// Move one pane out of its row into a row of its own, shown in a new window (a pane dragged out of
    /// the window, a tab dragged out of the sidebar, or their menus). The daemon makes the row, so the
    /// TUI sees it too; the window opens once that row arrives, with the same terminal view.
    func movePaneToNewWindow(_ s: String, at p: NSPoint? = nil) {
        guard let c = daemon.state.sidebar.container(holding: s), c.tabs.count > 1, isAlive(s) else { NSSound.beep(); return }
        pendingTearOffs[s] = (c.id, tearOffFrame(at: p), Date())
        daemon.command(.moveTab(session: s, container: nil, index: nil)) { [weak self] ack in
            guard let self else { return }
            self.report(ack)
            if !ack.ok { self.pendingTearOffs[s] = nil } else { self.openPendingTearOffs() }
        }
    }

    /// Open the windows for panes whose new row has arrived.
    private func openPendingTearOffs() {
        for (s, t) in pendingTearOffs {
            if Date().timeIntervalSince(t.at) > 10 { pendingTearOffs[s] = nil; continue }
            guard let c = daemon.state.sidebar.container(holding: s), c.id != t.from else { continue }
            pendingTearOffs[s] = nil
            // The old row's window lets the pane go first (it's no longer one of that row's tabs).
            for bw in windows where bw.workspace.layout.contains(s) { bw.workspace.reconcile(focus: false) }
            if let bw = openWindow(row: c.id, frame: t.frame) { bw.workspace.activate(s) }
        }
    }

    /// The sidebar's drag of an item ended outside every Bosun window: a row opens in a new window, a
    /// tab moves out into one.
    func sidebarDraggedOut(_ kind: SidebarItem.Kind, at p: NSPoint) {
        guard !isOverBosunWindow(p) else { return }
        switch kind {
        case .row(let id): openRowInNewWindow(id, at: p)
        case .tab(let s): movePaneToNewWindow(s, at: p)
        case .section, .heading: break
        }
    }

    /// A window with nothing left to show closes on the next turn (not inside the reconcile that found it).
    func closeWindowSoon(_ bw: BosunWindow) {
        DispatchQueue.main.async { [weak self, weak bw] in
            MainActor.assumeIsolated {
                guard let self, let bw, self.windows.contains(where: { $0 === bw }), self.windows.count > 1 else { return }
                bw.window.close()
            }
        }
    }

    func windowDidBecomeKey(_ notification: Notification) {
        guard let bw = controller(for: notification.object as? NSWindow) else { return }
        setCurrent(bw)
        bw.focus.setWindowKey(true)
        bw.workspace.windowKeyChanged(true)
        bw.workspace.focusActiveTerminal()
        checkReading()
    }

    func windowDidResignKey(_ notification: Notification) {
        guard !assumeKeyForTests, let bw = controller(for: notification.object as? NSWindow) else { return }
        bw.focus.setWindowKey(false)
        bw.workspace.windowKeyChanged(false)
        checkReading()
    }

    func enableTestKey() {
        assumeKeyForTests = true
        // Coalescing would let the real pointer's moves swallow the ones a test posts.
        NSEvent.isMouseCoalescingEnabled = false
        focus.setWindowKey(true)
        workspace.windowKeyChanged(true)
    }

    func windowDidChangeOcclusionState(_ notification: Notification) {
        guard let bw = controller(for: notification.object as? NSWindow) else { return }
        // The test modes run with the screen locked, where the window counts as occluded.
        let visible = assumeKeyForTests || bw.window.occlusionState.contains(.visible)
        for s in bw.workspace.visibleSessions { pool.view(for: s)?.setRendererVisible(visible) }
        reviewSeen()
    }

    func windowDidDeminiaturize(_ notification: Notification) {
        reviewSeen()
    }

    func appBecameActive() {
        notifier.recheck()
        reviewSeen()
        checkReading()
    }

    func appResignedActive() { checkReading() }

    func windowDidResize(_ notification: Notification) {
        guard controller(for: notification.object as? NSWindow) === current else { return }
        layoutPalette()
        layoutShortcuts()
    }

    func windowWillClose(_ notification: Notification) {
        guard let bw = controller(for: notification.object as? NSWindow) else { return }
        guard windows.count > 1 else { persist(); return }
        // Another window stays: this one's row goes back to the sidebar (its sessions keep running) and its
        // terminal views stay warm, hidden in the next window.
        if let row = bw.workspace.currentRow { syncActiveTab(row: row, pane: rowStore.rows[row]?.active) }
        windows.removeAll { $0 === bw }
        let next = windows[0]
        pool.adopt(from: bw.workspace.area, into: next.workspace.area, windowKey: next.workspace.windowKey)
        if current === bw { setCurrent(windows.first { $0.window.isKeyWindow } ?? next) }
        seen.setViewed(visibleSessions)
        for w in windows { w.sidebar.refreshRows() }
        saveLayouts()
        reviewSeen()
    }

    func persist() {
        for bw in windows {
            if let row = bw.workspace.currentRow { syncActiveTab(row: row, pane: rowStore.rows[row]?.active) }
        }
        daemon.writeCache()
        saveLayouts()
        saveInputs()
    }

    // MARK: Row layouts and windows, kept between runs

    private var layoutURL: URL { Paths.supportDir.appendingPathComponent("rows-\(Paths.key(for: daemon.socketPath)).json") }
    /// The one-tree-per-window layout of earlier builds.
    private var legacyURL: URL { Paths.supportDir.appendingPathComponent("layout-\(Paths.key(for: daemon.socketPath)).json") }
    private var layoutsTimer: Timer?

    /// Read the saved layouts: the first window shows the saved row, and every other saved window opens
    /// again where it was, on its row (one that's gone, or already shown, doesn't).
    private func restoreWindows() {
        let primary = windows[0]
        guard let data = try? Data(contentsOf: layoutURL), let saved = SavedLayouts.decode(data) else {
            primary.workspace.restore(row: nil, legacySession: (try? Data(contentsOf: legacyURL)).flatMap(SavedLayouts.legacyActiveSession))
            return
        }
        rowStore.rows = saved.rows
        primary.workspace.restore(row: saved.current)
        let sb = daemon.state.sidebar
        for w in saved.windows ?? [] where sb.container(id: w.row) != nil && !isShown(row: w.row, elsewhereThan: nil) {
            let bw = BosunWindow(app: self, frame: w.frame.map { NSRectFromString($0) })
            windows.append(bw)
            bw.workspace.restore(row: w.row)
        }
    }

    func saveLayouts() {
        layoutsTimer?.invalidate()
        layoutsTimer = nil
        guard let first = windows.first else { return }
        // Test modes, the demo and a dev daemon's app share your defaults (CFFIXED_USER_HOME doesn't move them);
        // they must never save a window frame over yours.
        let ownsFrame = !testMode && !demoMode && !devDaemon
        if ownsFrame { first.window.saveFrame(usingName: BosunWindow.primaryFrameName) }
        let others = windows.dropFirst().compactMap { bw in
            bw.workspace.currentRow.map { SavedLayouts.Window(row: $0, frame: testMode || demoMode ? nil : NSStringFromRect(bw.window.frame)) }
        }
        var saved = SavedLayouts(current: first.workspace.currentRow, rows: rowStore.rows)
        saved.windows = others.isEmpty ? nil : others
        guard let data = saved.encoded() else { return }
        try? data.write(to: layoutURL, options: .atomic)
        // The old layout can't be carried over (its tiles mixed rows); once the new file exists it goes.
        if FileManager.default.fileExists(atPath: legacyURL.path) { try? FileManager.default.removeItem(at: legacyURL) }
    }

    /// Save soon: pane activation (hover included) changes the model often, the file needn't follow every step.
    func scheduleSaveLayouts() {
        guard layoutsTimer == nil else { return }
        layoutsTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.saveLayouts() }
        }
    }

    /// A terminal's tmux client ended on its own: its view goes, and a window showing it attaches again.
    private func terminalExited(_ s: String) {
        pool.drop(s)
        for bw in windows { bw.workspace.terminalExited(s) }
    }

    private func installMonitors() {
        monitors.append(NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            nonisolated(unsafe) let event = e
            nonisolated(unsafe) var out: NSEvent? = e
            // nil from route means the key was used; it must not fall through to the terminal.
            MainActor.assumeIsolated {
                guard let self else { return }
                self.notePresence(event)
                out = self.route(event)
            }
            return out
        } as Any)
        monitors.append(NSEvent.addLocalMonitorForEvents(matching: .mouseMoved) { [weak self] e in
            nonisolated(unsafe) let event = e
            nonisolated(unsafe) var out: NSEvent? = e
            MainActor.assumeIsolated {
                // The test modes drive the pointer themselves; the real one (someone using the Mac
                // while a test runs) would move focus between panes behind the test's back.
                if testMode && !realInput && !TestKit.posted(event) { out = nil; return }
                self?.notePresence(event)
                guard let self, let bw = self.controller(for: event.window) else { return }
                bw.workspace.pointerMoved(event)
                // A posted mouse-moved never passes through the window server's tracking areas, so
                // the test modes hand it to the terminal under the pointer, as a tracking area would.
                if testMode && !realInput { bw.workspace.terminal(atWindowPoint: event.locationInWindow)?.mouseMoved(with: event) }
            }
            return out
        } as Any)
        monitors.append(NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] e in
            MainActor.assumeIsolated {
                self?.notePresence(e)
                self?.clickedSomewhere(e)
            }
            return e
        } as Any)
        monitors.append(NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] e in
            MainActor.assumeIsolated { self?.notePresence(e) }
            return e
        } as Any)
    }

    /// A click outside an open rename or palette closes it (rename keeps the new name). A click in
    /// another Bosun window closes them too, and that window becomes the one menus act on.
    private func clickedSomewhere(_ e: NSEvent) {
        if let bw = controller(for: e.window), bw !== current {
            closeShortcuts()
            closePalette()
            if focus.exception == .rename { window.makeFirstResponder(nil) }
            sidebar.commitFilter()
            setCurrent(bw)
        }
        if e.window === window, shortcutsPanel != nil { closeShortcuts() }
        guard e.window === window, let ex = focus.exception else { return }
        switch ex {
        case .palette:
            if let p = palette, !p.frame.contains(root.convert(e.locationInWindow, from: nil)) { closePalette() }
        case .rename:
            if let ed = window.firstResponder as? NSTextView, let field = ed.delegate as? NSTextField,
               field.bounds.contains(field.convert(e.locationInWindow, from: nil)) { return }
            window.makeFirstResponder(nil)
        case .filter:
            // Clicking anywhere else keeps what's typed (as Return does) and hands the keyboard back; a
            // click on a row then also selects it. The field itself and the magnifier deal with their own clicks.
            let v = sidebar.view
            let bar = v.filterBar, mag = v.header.magnifier
            if bar.bounds.contains(bar.convert(e.locationInWindow, from: nil)) || mag.bounds.contains(mag.convert(e.locationInWindow, from: nil)) { return }
            sidebar.commitFilter()
        case .sheet:
            break
        }
    }

    // MARK: Keys

    /// Navigation keys are taken before Ghostty encodes them; every other key goes to the terminal.
    private func route(_ e: NSEvent) -> NSEvent? {
        if debugKeys {
            print("key \(e.keyCode) mods=\(e.modifierFlags.rawValue & 0xFFFF0000) key=\(windowIsKey) owns=\(focus.terminalOwnsKeyboard) " +
                  "fr=\((window.firstResponder as? TerminalView).map { displayName($0.session) } ?? String(describing: window.firstResponder.map { type(of: $0) })) active=\(workspace.activeSession.map(displayName) ?? "-")")
        }
        // Esc closes the shortcuts panel (it never has the keyboard itself, so the key comes through here).
        if e.window === window, shortcutsPanel != nil, e.keyCode == 53, e.modifierFlags.intersection(.deviceIndependentFlagsMask).isEmpty {
            closeShortcuts()
            return nil
        }
        // Esc while a tile is being dragged by its header cancels the drag (the key never reaches the terminal).
        if e.window === window, workspace.tileDragActive, e.keyCode == 53 {
            workspace.cancelTileDrag()
            return nil
        }
        guard e.window === window, windowIsKey, focus.terminalOwnsKeyboard, !(window.firstResponder is NSText) else { return e }
        let term = workspace.activeTerminal
        if let term, term.hasMarkedText() { return e }
        if sendNextPending {
            sendNextPending = false
            root.band.flash(nil)
            if let term {
                // Through tmux's own send-next table (Bosun binds C-v to it), so tmux's root bindings
                // (Shift+arrows switch sessions there) never catch the key on its way to the program.
                if let cv = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: .control, timestamp: e.timestamp,
                                             windowNumber: e.windowNumber, context: nil, characters: "\u{16}",
                                             charactersIgnoringModifiers: "v", isARepeat: false, keyCode: 9) {
                    term.keyDown(with: cv)
                }
                term.keyDown(with: e)
                return nil
            }
            return e
        }
        let f = e.modifierFlags
        var m: KeyChord.Mods = []
        if f.contains(.shift) { m.insert(.shift) }
        if f.contains(.control) { m.insert(.control) }
        if f.contains(.option) { m.insert(.option) }
        guard let action = nav.action(keyCode: e.keyCode, charactersIgnoringModifiers: e.charactersIgnoringModifiers, mods: m, command: f.contains(.command)) else {
            return e
        }
        perform(action)
        return nil
    }

    func perform(_ a: NavAction) {
        switch a {
        case .nextSession, .previousSession:
            stepSession(forward: a == .nextSession)
        case .nextTab, .previousTab:
            stepTab(forward: a == .nextTab)
        case .sendNextKey:
            guard workspace.activeTerminal != nil else { NSSound.beep(); return }
            sendNextPending = true
            let label = nav.chords[.sendNextKey]?.label ?? ""
            root.band.flash("\(label) next key goes to the terminal", color: theme.accent, seconds: 0)
        }
    }

    /// Previous/next row along the sidebar order (rows whose tabs have all ended are skipped, as in the
    /// TUI). The whole main area switches to that row's panes.
    private func stepSession(forward: Bool) {
        // Rows another window shows are skipped: stepping never pulls another window forward. With a
        // filter, only the rows it shows (the one on screen included).
        let sb = daemon.state.sidebar
        let rows = sidebar.filtered.map { f in f.visibleRows.compactMap { sb.container(id: $0) } } ?? sb.allRows
        let all = rows.filter { $0.id == workspace.currentRow || !isShown(row: $0.id, elsewhereThan: current) }
        let live = all.filter { c in c.tabs.contains { isAlive($0.session) } }.map(\.id)
        guard !live.isEmpty else { return }
        let current = workspace.currentRow
        var from = current.flatMap { live.contains($0) ? $0 : nil }
        if from == nil, let current, let i = all.firstIndex(where: { $0.id == current }) {
            // An exited row: start from its nearest live neighbour behind it, so the step lands next to it.
            let n = all.count
            for k in 1..<max(n, 1) {
                let id = all[forward ? (i - k + n) % n : (i + k) % n].id
                if live.contains(id) { from = id; break }
            }
        }
        guard let next = Sidebar.cycle(live, from: from, forward: forward), next != current else { return }
        if focus.exception == .rename { window.makeFirstResponder(nil) }
        workspace.selectRow(next)
        // Prewarm the one after, so holding the key keeps landing on warm rows.
        if let after = Sidebar.cycle(live, from: next, forward: forward), after != current { workspace.prewarm(row: after) }
    }

    private func stepTab(forward: Bool) {
        workspace.stepPane(forward: forward)
    }

    // MARK: Showing

    /// Show `session`'s row with that pane active.
    func show(_ session: String) {
        workspace.show(session)
    }

    /// Tell the daemon which tab a row was on (the TUI shows that one). Sent when the app leaves a row
    /// and when it quits, not on every pane change, so hovering between panes never rewrites config.toml.
    func syncActiveTab(row id: String, pane: String?) {
        var sb = daemon.state.sidebar
        guard let pane, let c = sb.container(id: id), c.tabs.count > 1, c.active != pane, sb.setActive(pane) else { return }
        daemon.saveSidebar(sb)
    }

    func sidebarActivated(session: String) {
        if focus.exception == .rename { window.makeFirstResponder(nil) }
        sidebar.commitFilter()
        show(session)
    }

    func sidebarActivated(row id: String) {
        if focus.exception == .rename { window.makeFirstResponder(nil) }
        sidebar.commitFilter()
        workspace.selectRow(id)
    }

    func workspaceChanged(_ ws: Workspace) {
        seen.setViewed(visibleSessions)
        ws.owner.sidebar.syncSelection()
        // Every sidebar marks the rows other windows show.
        for bw in windows { bw.sidebar.refreshRows() }
        ws.owner.focus.setTiles(count: ws.tiles.count, active: ws.activeTile)
        ws.owner.updateTitle()
        updateDockBadge()
        reviewSeen()
        checkReading()
    }

    // MARK: Read

    private var readTimer: Timer?
    private var readCandidate: String?
    /// The last time the keyboard, mouse or trackpad was used in this app, Unix ms. A done pane on screen
    /// only counts as read once you've used one of them since its turn finished: being in front while
    /// you're away from the Mac isn't reading it.
    private var lastPresence: UInt64 = 0
    /// The front pane whose done turn is waiting for that.
    private var readAwaiting: String?
    /// How long a done pane has to be the active pane of the front window to count as read.
    static let readAfter: TimeInterval = 2

    /// The active pane of the window in front of you: the key window's, with the app active (in the test modes,
    /// the window they treat as key).
    private var frontPane: String? {
        guard NSApp.isActive || assumeKeyForTests else { return nil }
        return windows.first(where: isKey)?.workspace.activeSession
    }

    /// A done turn is read once its pane has been the front pane for `readAfter`. Called whenever any of that
    /// may have changed.
    func checkReading() {
        let target = frontPane.flatMap { displayState($0) == .done ? $0 : nil }
        guard target != readCandidate else { return }
        readCandidate = target
        readAwaiting = nil
        readTimer?.invalidate()
        readTimer = nil
        guard let target else { return }
        readTimer = Timer.scheduledTimer(withTimeInterval: Self.readAfter, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.readTimer = nil
                guard self.frontPane == target, self.displayState(target) == .done else { self.readCandidate = nil; return }
                if self.lastPresence > self.daemon.state.statuses[target]?.doneAt ?? 0 {
                    self.readCandidate = nil
                    self.markRead([target])
                } else {
                    self.readAwaiting = target
                }
            }
        }
    }

    /// The keyboard, mouse or trackpad was used in this app. A done front pane waiting for you is read
    /// `readAfter` later.
    private func notePresence(_ e: NSEvent) {
        // In the test modes only the tests' own events count (the real pointer is someone else at the Mac).
        if testMode && !realInput && e.type != .keyDown && !TestKit.posted(e) { return }
        lastPresence = UInt64(Date().timeIntervalSince1970 * 1000)
        guard readAwaiting != nil else { return }
        readAwaiting = nil
        readCandidate = nil
        checkReading()
    }

    /// Mark done turns read (focused for a moment, or Mark as Read): drawn as seen until the next turn.
    func markRead(_ sessions: [String]) {
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        for s in sessions { seen.markRead(s, at: now) }
        scheduleInputsSave()
        for s in sessions where !attention.delivered.isEmpty { act(attention.seen(s)) }
        for bw in windows {
            bw.sidebar.refreshRows(Set(sessions))
            bw.workspace.updateChromeState()
        }
        updateHeaderStatus()
    }

    /// The user typed, pasted or dropped into `session`'s terminal: a hook-reported done turn is now read,
    /// and its notifications go.
    func terminalInput(_ session: String) {
        // Typing into a snoozed row brings it back.
        if let c = daemon.state.sidebar.container(holding: session), daemon.state.sidebar.role(ofRow: c.id) == .snoozed {
            unpark(c.id)
        }
        let before = displayState(session)
        seen.noteInput(session, at: UInt64(Date().timeIntervalSince1970 * 1000))
        scheduleInputsSave()
        if !attention.delivered.isEmpty { act(attention.seen(session)) }
        guard displayState(session) != before else { return }
        for bw in windows {
            bw.sidebar.refreshRows([session])
            bw.workspace.updateChromeState()
        }
        updateHeaderStatus()
    }

    func terminalPainted(_ session: String) {
        guard firstTerminalAt == nil else { return }
        firstTerminalAt = ProcessClock.sinceLaunch()
        signposter.emitEvent("FirstLiveTerminal")
        onFirstTerminal?()
    }

    private func updateTitle() {
        for bw in windows { bw.updateTitle() }
    }

    /// The Dock badge: the number of sessions waiting on you (see `Attention.badgeCount` for why done turns
    /// don't count). The tile is only touched when the number changes.
    private func updateDockBadge() {
        let st = daemon.state
        let n = AppSettings.shared.showDockBadge && st.live ? Attention.badgeCount(statuses: st.statuses) { st.isAlive($0) } : 0
        let label = Attention.badgeLabel(n)
        guard label != dockBadgeLabel else { return }
        dockBadgeLabel = label
        dockBadgeUpdates += 1
        NSApp.dockTile.badgeLabel = label
    }

    // MARK: Notifications (design 10)

    /// You can see `session`: it's a pane on screen in a window that's in front of you.
    func userCanSee(_ session: String) -> Bool {
        windows.contains { $0.workspace.visibleSessions.contains(session) && windowSeen($0) }
    }

    private func observeAttention(_ s: String) {
        // A row snoozed until a time stays quiet (one snoozed until it needs you wakes as this arrives).
        let quiet = daemon.state.sidebar.container(holding: s).map(daemon.state.sidebar.isQuietlySnoozed) ?? false
        let enabled = Attention.Enabled(waiting: AppSettings.shared.notifyWhenWaiting && !quiet, done: AppSettings.shared.notifyWhenDone && !quiet)
        act(attention.observe(s, status: daemon.state.statuses[s], done: displayState(s) == .done, visible: userCanSee(s), enabled: enabled))
    }

    private func act(_ actions: [Attention.Action]) {
        for a in actions {
            switch a {
            case .post(let n): notifier.post(notification(for: n))
            case .remove(let ids): notifier.remove(ids)
            }
        }
    }

    /// Sessions with a notification out that you can now see: their notifications go.
    func reviewSeen() {
        guard !attention.delivered.isEmpty else { return }
        for s in attention.outstanding where userCanSee(s) { act(attention.seen(s)) }
    }

    /// The title is the session's name; the subtitle its row (for a tab of a multi-tab row) and section.
    func notification(for n: Attention.Note) -> AppNotification {
        let s = n.session
        let name = displayName(s)
        let sb = daemon.state.sidebar
        var context: [String] = []
        if let c = sb.container(holding: s), c.tabs.count > 1, rowLabel(c) != name { context.append(rowLabel(c)) }
        if let sec = sb.sections.first(where: { $0.members.contains { $0.tabs.contains { $0.session == s } } }) { context.append(sec.name) }
        let body: String
        switch n.kind {
        case .waiting: body = n.detail.map { "Waiting — \($0)" } ?? "Waiting for you"
        case .done: body = "Done — your turn"
        }
        return AppNotification(id: n.id, session: s, kind: n.kind, title: name, subtitle: context.joined(separator: " · "), body: body)
    }

    /// A notification was clicked: bring the app forward, show the session's row with its pane active and the
    /// keyboard in it.
    func notificationClicked(userInfo: [AnyHashable: Any]) {
        guard let s = userInfo["session"] as? String else { return }
        // The window that shows the session's row, else the key one.
        let target = window(showingSession: s) ?? current!
        if !testMode { NSApp.activate() }
        bringForward(target)
        closePalette()
        closeShortcuts()
        if daemon.state.sidebar.container(holding: s) != nil { sidebarActivated(session: s) }
        act(attention.seen(s))
    }

    // MARK: Input times (the done rule), kept across launches

    /// Per daemon, like the row layouts.
    var inputsURL: URL { Paths.supportDir.appendingPathComponent("inputs-\(Paths.key(for: daemon.socketPath)).json") }

    private func loadInputs() {
        guard let data = try? Data(contentsOf: inputsURL), let saved = SavedInputs.decode(data) else { return }
        seen.restoreInputs(saved.inputs, read: saved.read ?? [:])
    }

    func saveInputs() {
        inputsTimer?.invalidate()
        inputsTimer = nil
        guard let data = SavedInputs(seen.inputTimes, read: seen.readTimes).encoded() else { return }
        try? data.write(to: inputsURL, options: .atomic)
    }

    /// Input arrives with every key: the file is written a few seconds after the first one, and on quit.
    private func scheduleInputsSave() {
        guard inputsTimer == nil else { return }
        inputsTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.saveInputs() }
        }
    }

    // MARK: State

    private func stateChanged(_ changes: [BosunState.Change]) {
        var structure = false
        var touched = Set<String>()
        var removed = Set<String>()
        var chrome = false
        let st = daemon.state
        for c in changes {
            switch c {
            case .snapshot:
                applyTheme()
                nav = NavBindings(st.keybindings)
                pool.tmuxSocket = st.server?.tmuxSocket
                for name in st.sessionOrder { seen.observe(name, status: st.statuses[name], title: st.sessions[name]?.paneTitle) }
                // Waits already there share one time; the sidebar order breaks the tie (`oldestWaiting`).
                let now = Date()
                for (name, info) in st.statuses where info.state == .waiting { if waitingSince[name] == nil { waitingSince[name] = now } }
                waitingSince = waitingSince.filter { st.state(of: $0.key) == .waiting }
                if st.live { snapshotAttention() }
                structure = true
                chrome = true
                if st.live { liveSnapshotArrived() }
            case .sessions(let changed, let gone):
                for name in changed { seen.observe(name, status: st.statuses[name], title: st.sessions[name]?.paneTitle) }
                for name in gone {
                    seen.forget(name)
                    waitingSince[name] = nil
                    act(attention.forget(name))
                }
                touched.formUnion(changed)
                removed.formUnion(gone)
                if !gone.isEmpty || changed.contains(where: { sidebar.itemFor(session: $0) == nil }) { structure = true }
                chrome = chrome || !changed.isDisjoint(with: visibleSessions) || !gone.isEmpty
            case .status(let name):
                let state = st.state(of: name)
                seen.observe(name, status: st.statuses[name], title: st.sessions[name]?.paneTitle)
                if state == .waiting { if waitingSince[name] == nil { waitingSince[name] = Date() } } else { waitingSince[name] = nil }
                if attentionLive { observeAttention(name) }
                touched.insert(name)
                chrome = chrome || visibleSessions.contains(name)
            case .sidebar:
                for name in st.sessionOrder { formerNames[name] = displayName(name) }
                structure = true
                chrome = true
            case .theme, .themePair:
                // A chosen theme's preview has done its job once the daemon's change is here.
                if previewHoldTimer != nil {
                    previewHoldTimer?.invalidate()
                    previewHoldTimer = nil
                    previewPalette = nil
                }
                applyTheme()
            case .keybindings:
                nav = NavBindings(st.keybindings)
                for e in nav.errors { log.error("keybindings: \(e, privacy: .public)") }
            case .notice(let m):
                let shown = readableNotice(m)
                root.band.flash(shown)
                lastNotice = (m, shown)
                // "created X" comes before X's session delta, so name it again once that lands.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        self.root.band.replaceNote(shown, with: self.readableNotice(m))
                    }
                }
            case .client(let tty, let session):
                // The window whose area holds that terminal deals with it.
                if let v = pool.view(onTTY: tty), let bw = windows.first(where: { v.superview === $0.workspace.area }) {
                    bw.workspace.clientMoved(tty: tty, to: session)
                }
            case .config, .themes:
                themePairModel.update(from: st)
                editorModel.update(from: st)
            case .recents:
                break
            }
        }
        // "created X" names a session whose display name arrives with the next sessions delta.
        if let n = lastNotice, changes.contains(where: { if case .sessions = $0 { return true }; return false }) {
            let better = readableNotice(n.raw)
            if better != n.shown {
                root.band.replaceNote(n.shown, with: better)
                lastNotice = (n.raw, better)
            }
        }
        updateTurnTicker()
        for bw in windows {
            if structure { bw.sidebar.reload() } else if !touched.isEmpty { bw.sidebar.refreshRows(touched) }
        }
        if !removed.isEmpty || structure {
            for s in removed { pool.drop(s) }
            for bw in windows { bw.workspace.stateChanged() }
            openPendingTearOffs()
        }
        if chrome { for bw in windows { bw.workspace.updateChromeState() }; updateTitle() }
        updateDockBadge()
        updateHeaderStatus()
        // A turn that finishes in the pane you're in starts counting towards read.
        if !touched.isEmpty { checkReading() }
    }

    private var hadLiveSnapshot = false

    /// A live snapshot: the first is the notifications' baseline (nothing that was already waiting or done when
    /// the app connected is notified). After a reconnect, sessions the app knew are compared with what it had.
    private func snapshotAttention() {
        let st = daemon.state
        if !attentionLive {
            attentionLive = true
            for name in st.sessionOrder { attention.baseline(name, status: st.statuses[name]) }
            // Input times of sessions that ended while the app was closed.
            seen.pruneInputs(keeping: Set(st.sessionOrder))
            scheduleInputsSave()
            return
        }
        for s in attention.outstanding where st.sessions[s] == nil { act(attention.forget(s)) }
        for name in st.sessionOrder {
            if attention.isKnown(name) { observeAttention(name) } else { attention.baseline(name, status: st.statuses[name]) }
        }
    }
    /// The last daemon notice, as sent and as shown (see `readableNotice`).
    private var lastNotice: (raw: String, shown: String)?

    private func liveSnapshotArrived() {
        let first = !hadLiveSnapshot
        hadLiveSnapshot = true
        if first { for bw in windows { bw.workspace.startAttaching() } }
    }

    /// What every sidebar header's status line says (windows opened later start with it).
    private(set) var headerStatus: HeaderStatus = .connecting

    /// The app was pointed at a daemon of its own: the header says "dev".
    private let devDaemon = ProcessInfo.processInfo.environment["BOSUN_SERVE_SOCKET"] != nil && !demoMode

    private func setStatus(_ s: HeaderStatus) {
        guard s != headerStatus else { return }
        headerStatus = s
        for bw in windows { bw.sidebar.setStatus(s) }
    }

    /// While connected: the rows with a live tab, and the tabs waiting on you and done, from the same
    /// states the badges show. Recounted on the deltas that already refresh the rows.
    func updateHeaderStatus() {
        guard daemon.status == .connected else { return }
        let rows = daemon.state.sidebar.allRows
        let live = rows.filter { c in c.tabs.contains { isAlive($0.session) } }.count
        let states = rows.flatMap(\.tabs).filter { isAlive($0.session) }.map { displayState($0.session) }
        setStatus(.counts(sessions: live, waiting: states.filter { $0 == .waiting }.count, done: states.filter { $0 == .done }.count, dev: devDaemon))
    }

    private func statusChanged(_ s: DaemonClient.Status) {
        switch s {
        case .connected:
            headerStatus = .connecting
            updateHeaderStatus()
        case .connecting:
            setStatus(.connecting)
        case .disconnected(let why):
            setStatus(.reconnecting("Reconnecting… (\(why))"))
        case .failed(let why):
            setStatus(.reconnecting(why))
            let alert = NSAlert()
            alert.messageText = "Can't use bosun serve"
            alert.informativeText = why
            alert.alertStyle = .warning
            alert.beginSheetModal(for: window)
        }
    }

    // MARK: Lookups used by views

    func isAlive(_ s: String) -> Bool { daemon.state.isAlive(s) }

    var noticeText: String { root.band.noteText }

    /// Display names of sessions seen this run, so a notice about one that is already gone still reads well.
    private var formerNames: [String: String] = [:]

    /// Daemon notices name sessions by their tmux name; show the sidebar name instead.
    func readableNotice(_ m: String) -> String {
        var out = m
        for name in daemon.state.sessionOrder { formerNames[name] = displayName(name) }
        for (name, display) in formerNames where display != name && out.contains(name) {
            out = out.replacingOccurrences(of: name, with: display)
        }
        return out
    }

    func displayName(_ s: String) -> String {
        if let d = daemon.state.sessions[s]?.display { return d }
        if let t = daemon.state.sidebar.container(holding: s)?.tabs.first(where: { $0.session == s }), !t.display.isEmpty { return t.display }
        return s
    }

    /// A sidebar row's label. A single-tab row: the daemon's (its tab's name, as the TUI shows it), or
    /// the tab's display name when the row came from a daemon that sent a tmux name there. A row with
    /// several tabs is a workspace whose tabs are all on screen, listed under it with the active pane
    /// marked, so it's labelled by its first tab (its own name) rather than following the active tab
    /// as the TUI's label does: adding a tab never renames the row.
    func rowLabel(_ c: Container) -> String {
        if c.tabs.count > 1, let first = c.tabs.first { return displayName(first.session) }
        return c.name.isEmpty || c.tabs.contains(where: { $0.session == c.name }) ? displayName(c.active) : c.name
    }

    /// The tab whose name a row shows (and a rename of the row changes).
    func rowLabelSession(_ c: Container) -> String { c.tabs.count > 1 ? (c.tabs.first?.session ?? c.active) : c.active }

    func displayState(_ s: String) -> AgentState {
        guard isAlive(s) else { return .idle }
        return seen.display(s, state: daemon.state.state(of: s))
    }

    func statusDetail(_ s: String) -> String? { daemon.state.statuses[s]?.detail }

    /// How long a running session's turn has been going ("4m"), nil when it isn't running.
    func turnElapsed(_ s: String, now: UInt64 = TurnElapsed.nowMs()) -> String? {
        guard displayState(s) == .running, let t = daemon.state.statuses[s]?.startedAt else { return nil }
        return TurnElapsed.label(startedAt: t, now: now)
    }

    /// How long a row snoozed until a time has left ("1h 20m"), nil for any other row.
    func snoozeLeft(_ c: Container, now: UInt64 = TurnElapsed.nowMs()) -> String? {
        guard let w = c.parked?.wakeAt, daemon.state.sidebar.role(ofRow: c.id) == .snoozed else { return nil }
        return TurnElapsed.label(startedAt: now, now: w)
    }

    /// Running sessions' timers change once a minute: redraw just the rows and headers whose label moved.
    private var turnTicker: Timer?
    private var turnLabels: [String: String] = [:]

    private func updateTurnTicker() {
        let running = daemon.state.statuses.contains { $0.value.startedAt != nil }
            || daemon.state.sidebar.allRows.contains { $0.parked?.wakeAt != nil }
        if running, turnTicker == nil {
            turnTicker = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { self?.turnTick() }
            }
        } else if !running {
            turnTicker?.invalidate()
            turnTicker = nil
            turnLabels = [:]
        }
    }

    private func turnTick() {
        let now = TurnElapsed.nowMs()
        var labels: [String: String] = [:]
        for name in daemon.state.statuses.keys { if let l = turnElapsed(name, now: now) { labels[name] = l } }
        for c in daemon.state.sidebar.allRows { if let l = snoozeLeft(c, now: now) { labels[c.active] = l } }
        let moved = Set(labels.keys.filter { turnLabels[$0] != labels[$0] })
        turnLabels = labels
        guard !moved.isEmpty else { return }
        for bw in windows {
            bw.sidebar.refreshRows(moved)
            if !moved.isDisjoint(with: bw.workspace.tiles) { bw.workspace.updateChromeState() }
        }
    }

    /// "tab 2 of 3" for a session in a multi-tab row.
    func groupLabel(_ s: String) -> String? {
        guard let c = daemon.state.sidebar.container(holding: s), c.tabs.count > 1,
              let i = c.tabs.firstIndex(where: { $0.session == s }) else { return nil }
        return "tab \(i + 1) of \(c.tabs.count)"
    }

    // MARK: Theme

    /// The system's light or dark appearance, as the app sees it (System Settings' Auto included).
    /// `NSApp.appearance`, when set, wins; only the test mode sets it.
    static func systemAppearance() -> ThemeChoice.Appearance {
        NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? .dark : .light
    }

    private func appearanceChanged() {
        let now = Self.systemAppearance()
        guard now != appearance else { return }
        appearance = now
        applyTheme()
    }

    // MARK: Live theme preview (Settings' theme popups)

    /// A theme shown while a theme popup in Settings is open and an entry is highlighted; nil shows the real one.
    private(set) var previewPalette: ThemePalette?
    /// Palettes fetched for previews, by name (until the popup closes, or the theme list changes).
    private var previewCache: [String: ThemePalette] = [:]
    /// The latest preview asked for, so a slow answer for an earlier highlight is dropped.
    private var previewRequest = 0
    /// After a choice the preview stays up until the daemon's change arrives, so the window doesn't flash
    /// back to the old theme in between.
    private var previewHoldTimer: Timer?

    /// Show theme `name` on every window and the terminals, without changing anything (nil: stop).
    func previewTheme(_ name: String?) {
        previewRequest += 1
        let request = previewRequest
        previewHoldTimer?.invalidate()
        previewHoldTimer = nil
        guard let name else { setPreview(nil); return }
        if let p = previewCache[name] { setPreview(p); return }
        daemon.command(.getTheme(name: name)) { [weak self] ack in
            guard let self, ack.ok, let p = ack.theme else { return }
            self.previewCache[name] = p
            if request == self.previewRequest { self.setPreview(p) }
        }
    }

    /// The popup closed: without a choice the real theme comes back now; with one, once the daemon confirms it.
    func endThemePreview(chosen: Bool) {
        previewRequest += 1
        previewCache.removeAll()
        guard chosen, previewPalette != nil else { setPreview(nil); return }
        previewHoldTimer?.invalidate()
        previewHoldTimer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.previewHoldTimer = nil
                self?.setPreview(nil)
            }
        }
    }

    private func setPreview(_ p: ThemePalette?) {
        guard p != previewPalette else { return }
        previewPalette = p
        applyTheme()
    }

    /// The chrome's theme: `theme`, or the side of the Light/Dark pair for the current appearance (or a
    /// preview while a theme popup is open).
    private func applyTheme() {
        theme = AppTheme(previewPalette ?? daemon.state.palette(for: appearance))
        themePairModel.update(from: daemon.state)
        editorModel.update(from: daemon.state)
        for bw in windows { bw.applyTheme() }
        applyTerminalTheme()
    }

    private func applyTerminalTheme() {
        let want = AppSettings.shared.matchTerminalTheme ? theme.palette : nil
        guard want != appliedTerminalTheme else { return }
        appliedTerminalTheme = want
        GhosttyRuntime.shared.setTerminalTheme(want)
        for bw in windows { bw.workspace.updateChromeState() }
    }

    /// The interface font last drawn with (family and size).
    private var appliedInterfaceFont = AppController.interfaceFontKey

    private static var interfaceFontKey: String { "\(AppSettings.shared.interfaceFontFamily ?? "")|\(AppSettings.shared.interfaceFontSize)" }

    /// Settings → Fonts: terminals take the terminal font at once, and the windows the interface font.
    private func applyFonts() {
        GhosttyRuntime.shared.setTerminalFont(family: AppSettings.shared.terminalFontFamily, size: AppSettings.shared.terminalFontSize)
        let key = AppController.interfaceFontKey
        guard key != appliedInterfaceFont else { return }
        appliedInterfaceFont = key
        for bw in windows { bw.applyFonts() }
    }

    private func settingsChanged() {
        applyFonts()
        for bw in windows { bw.focus.followsMouse = AppSettings.shared.focusFollowsMouse }
        pool.capacity = AppSettings.shared.poolSize
        pool.evict(keeping: visibleSessions)
        applyTerminalTheme()
        updateDockBadge()
    }

    // MARK: Commands

    private func report(_ ack: Ack) {
        if !ack.ok { root.band.flash(ack.error ?? "failed", color: theme.error, seconds: 6) }
    }

    func restart(_ s: String, resume: Bool) {
        daemon.command(.restart(session: s, resume: resume, deferLaunch: false)) { [weak self] in self?.report($0) }
    }

    /// Restart an exited row from the recents entry it was created from (the daemon says which; an older
    /// one doesn't, so then the entry with its display name), under the name it has now and in its place.
    func restartExited(_ s: String, resume: Bool) {
        if isAlive(s) { restart(s, resume: resume); return }
        let name = displayName(s)
        let recent = daemon.state.sidebar.container(holding: s)?.tabs.first(where: { $0.session == s })?.recent
        guard let r = daemon.state.recents.first(where: { recent != nil ? $0.id == recent : $0.name == name }) else {
            root.band.flash("No saved spec for “\(name)”", color: theme.error)
            return
        }
        var spec = r.spec
        spec.name = name
        spec.resume = resume
        daemon.command(.create(spec: spec, deferLaunch: true, replaces: s)) { [weak self] ack in
            guard let self else { return }
            self.report(ack)
            guard ack.ok, let new = ack.session else { return }
            self.show(new)
            if let p = ack.pendingLaunch { self.workspace.expectLaunch(p.session, resume: p.resume) }
        }
    }

    func confirmKill(_ s: String) {
        let alert = NSAlert()
        alert.messageText = "Kill “\(displayName(s))”?"
        alert.informativeText = "The tmux session and the agent in it stop. This can't be undone."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Kill")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            self.daemon.command(.kill(session: s)) { [weak self] in self?.report($0) }
        }
    }

    func confirmKillContainer(_ c: Container) {
        let alert = NSAlert()
        alert.messageText = "Kill all \(c.tabs.count) tabs of “\(rowLabel(c))”?"
        alert.informativeText = "Every tmux session in this row stops."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Kill All")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            self.daemon.command(.killContainer(sessions: c.tabs.map(\.session))) { [weak self] in self?.report($0) }
        }
    }

    func confirmKillRemoveWorktree(_ s: String) {
        guard let info = daemon.state.sessions[s], let wt = info.worktreePath, let branch = info.branch else { return }
        let alert = NSAlert()
        alert.messageText = "Kill “\(displayName(s))” and remove its worktree?"
        alert.informativeText = "\((wt as NSString).abbreviatingWithTildeInPath) on branch \(branch). A worktree with uncommitted changes is never removed."
        alert.alertStyle = .warning
        let merge = NSButton(checkboxWithTitle: "Merge \(branch) into the repo's current branch first", target: nil, action: nil)
        alert.accessoryView = merge
        alert.addButton(withTitle: "Kill and Remove")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            self.daemon.command(.killRemoveWorktree(session: s, worktreePath: wt, branch: branch, merge: merge.state == .on)) { [weak self] in self?.report($0) }
        }
    }

    private func runSheet(_ alert: NSAlert, _ done: @escaping (Bool) -> Void) {
        // The sheet belongs to this window, whichever window is key when it closes.
        let bw = current!
        bw.sidebar.commitFilter()
        bw.focus.begin(.sheet)
        alert.beginSheetModal(for: bw.window) { resp in
            MainActor.assumeIsolated {
                if bw.focus.end(.sheet) { bw.workspace.focusActiveTerminal() }
                done(resp == .alertFirstButtonReturn)
            }
        }
    }

    func rename(item: SidebarItem, to name: String) {
        switch item.kind {
        case .row(let id):
            // A row is labelled by one of its tabs (`rowLabel`), so renaming the row renames that session.
            guard let c = daemon.state.sidebar.container(id: id) else { return }
            daemon.command(.rename(session: rowLabelSession(c), display: name)) { [weak self] in self?.report($0) }
        case .tab(let s):
            daemon.command(.rename(session: s, display: name)) { [weak self] in self?.report($0) }
        case .section(let id):
            daemon.command(.renameSection(section: id, name: name)) { [weak self] in self?.report($0) }
        case .heading:
            break
        }
    }

    func startRename(_ s: String) {
        if let sec = daemon.state.sidebar.sections.first(where: { $0.members.contains { $0.tabs.contains { $0.session == s } } }), sec.collapsed {
            setSectionCollapsed(sec.id, false)
        }
        DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.sidebar.beginRename(session: s) } }
    }

    func setSectionCollapsed(_ id: String, _ collapsed: Bool) {
        var sb = daemon.state.sidebar
        if sb.setCollapsed(section: id, collapsed) { daemon.saveSidebar(sb) }
    }

    func removeRow(_ id: String) {
        var sb = daemon.state.sidebar
        if sb.removeRow(id: id) != nil { daemon.saveSidebar(sb) }
    }

    /// Remove from Sidebar on a row: its live sessions are killed (after one confirmation) and the row goes,
    /// rather than Kill… then Remove from Sidebar as two steps. A row with nothing running just goes.
    func removeRowKilling(_ id: String) {
        guard let c = daemon.state.sidebar.container(id: id) else { return }
        let live = c.tabs.map(\.session).filter(isAlive)
        guard !live.isEmpty else { removeRow(id); return }
        let alert = NSAlert()
        alert.messageText = "Remove “\(rowLabel(c))” from the sidebar?"
        alert.informativeText = live.count == 1
            ? "Its tmux session and the agent in it stop. This can't be undone."
            : "Its \(live.count) tmux sessions and the agents in them stop. This can't be undone."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Kill and Remove")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            let command: Command = live.count == 1 ? .kill(session: live[0]) : .killContainer(sessions: live)
            self.daemon.command(command) { [weak self] ack in
                guard let self else { return }
                self.report(ack)
                // The row stays (as exited) if the kill failed, so nothing is lost silently.
                if ack.ok { self.removeRow(id) }
            }
        }
    }

    /// Remove from Sidebar on a tab: that session is killed (after a confirmation, if it's running) and its
    /// tab leaves the row. A row's last tab takes the row with it.
    func removeTab(_ s: String) {
        guard let c = daemon.state.sidebar.container(holding: s) else { return }
        if c.tabs.count == 1 { removeRowKilling(c.id); return }
        let drop = { [weak self] in
            guard let self else { return }
            var sb = self.daemon.state.sidebar
            if sb.removeTab(session: s) { self.daemon.saveSidebar(sb) }
        }
        guard isAlive(s) else { drop(); return }
        let alert = NSAlert()
        alert.messageText = "Remove “\(displayName(s))” from the sidebar?"
        alert.informativeText = "Its tmux session and the agent in it stop. This can't be undone."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Kill and Remove")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            self.daemon.command(.kill(session: s)) { [weak self] ack in
                self?.report(ack)
                if ack.ok { drop() }
            }
        }
    }

    func newSection() {
        askText(title: "New Section", message: "Sessions can be dragged into it.", initial: "", button: "Add") { [weak self] name in
            self?.daemon.command(.insertSection(name: name)) { [weak self] in self?.report($0) }
        }
    }

    func renameSection(_ id: String) {
        guard let s = daemon.state.sidebar.sections.first(where: { $0.id == id }) else { return }
        askText(title: "Rename Section", message: "", initial: s.name, button: "Rename") { [weak self] name in
            self?.daemon.command(.renameSection(section: id, name: name)) { [weak self] in self?.report($0) }
        }
    }

    func removeSection(_ id: String) {
        var sb = daemon.state.sidebar
        guard let i = sb.sections.firstIndex(where: { $0.id == id }) else { return }
        // Rows in it move to ungrouped rather than vanishing.
        sb.ungrouped += sb.sections[i].members
        sb.sections.remove(at: i)
        daemon.saveSidebar(sb)
    }

    private func askText(title: String, message: String, initial: String, button: String, _ done: @escaping (String) -> Void) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = message
        let field = NSTextField(string: initial)
        field.frame = NSRect(x: 0, y: 0, width: 280, height: 24)
        alert.accessoryView = field
        alert.addButton(withTitle: button)
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        runSheet(alert) { ok in
            let v = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
            if ok, !v.isEmpty { done(v) }
        }
    }

    // MARK: Sheets

    /// New Session…: the sheet for a new row, at the end of `section` when given (a section's + or menu).
    func newSession(name: String = "", section: String? = nil) {
        let st = daemon.state
        let model = NewSessionModel(mode: .create, recents: st.recents, defaultAgent: st.defaultAgent, worktreeLocation: st.worktreeLocation, name: name)
        let sec = section.flatMap { id in st.sidebar.sections.first { $0.id == id } }
        model.sectionName = sec?.name
        if let d = sec?.defaults { model.apply(d) }
        presentForm(model) { [weak self] spec in
            guard let self else { return }
            self.create(spec, section: section)
            if let section, model.saveAsDefaults {
                self.daemon.command(.setSectionDefaults(section: section, defaults: model.sectionDefaults)) { [weak self] in self?.report($0) }
            }
        }
    }

    // MARK: Snoozed and docked rows

    /// The snooze lengths a row's menu offers, in hours.
    static let snoozeHours = [1, 2, 3, 6, 12, 24, 48]

    /// Snooze a row for `hours`, or with nil until one of its agents needs you. Its sessions keep running.
    func snooze(_ rowID: String, hours: Int?) {
        let wakeAt = hours.map { TurnElapsed.nowMs() + UInt64($0) * 3_600_000 }
        daemon.command(.parkRow(container: rowID, role: .snoozed, wakeAt: wakeAt, untilNeeded: hours == nil)) { [weak self] in self?.report($0) }
    }

    /// Dock a row: its sessions are killed and it waits in Docked until it's undocked and resumed. Its done
    /// turns count as read.
    func dock(_ rowID: String) {
        if let c = daemon.state.sidebar.container(id: rowID) {
            let done = c.tabs.map(\.session).filter { displayState($0) == .done }
            if !done.isEmpty { markRead(done) }
        }
        daemon.command(.parkRow(container: rowID, role: .docked)) { [weak self] in self?.report($0) }
    }

    /// Wake a snoozed row, or undock a docked one (its sessions resume), back where it came from.
    func unpark(_ rowID: String) {
        daemon.command(.unparkRow(container: rowID)) { [weak self] in self?.report($0) }
    }

    private func addParkItems(to m: NSMenu, row c: Container) {
        let role = daemon.state.sidebar.role(ofRow: c.id)
        func item(_ title: String, _ action: @escaping () -> Void) { m.addItem(ClosureMenuItem(title: title, action: action)) }
        if role == .docked {
            item("Undock and Resume") { [weak self] in self?.unpark(c.id) }
            return
        }
        if role == .snoozed { item("Wake Now") { [weak self] in self?.unpark(c.id) } }
        let snooze = NSMenu()
        for h in AppController.snoozeHours {
            snooze.addItem(ClosureMenuItem(title: h == 1 ? "For 1 Hour" : "For \(h) Hours") { [weak self] in self?.snooze(c.id, hours: h) })
        }
        snooze.addItem(.separator())
        snooze.addItem(ClosureMenuItem(title: "Until It Needs Me") { [weak self] in self?.snooze(c.id, hours: nil) })
        let i = NSMenuItem(title: role == .snoozed ? "Snooze Again" : "Snooze", action: nil, keyEquivalent: "")
        i.submenu = snooze
        m.addItem(i)
        item("Dock") { [weak self] in self?.dock(c.id) }
    }

    /// Section Defaults…: the sheet without a name, whose Save stores what a new session in the section starts with.
    func editSectionDefaults(_ id: String) {
        let st = daemon.state
        guard let sec = st.sidebar.sections.first(where: { $0.id == id }) else { return }
        // No defaults yet: start from the section's first row's directory.
        let path = sec.defaults.map(\.path).flatMap { $0.isEmpty ? nil : $0 } ?? sec.members.first.map(rowDirectory)
        let model = NewSessionModel(mode: .sectionDefaults(section: id), recents: st.recents, defaultAgent: st.defaultAgent,
                                    worktreeLocation: st.worktreeLocation, path: path)
        model.sectionName = sec.name
        if let d = sec.defaults { model.apply(d) }
        presentForm(model) { [weak self] _ in
            guard let self else { return }
            self.daemon.command(.setSectionDefaults(section: id, defaults: model.sectionDefaults)) { [weak self] in self?.report($0) }
        }
    }

    /// New Tab… (Cmd+T, or a row's menu): the new-session sheet for a tab of `containerID`, in the row's
    /// directory. The tab opens as a split to the right of the row's active pane.
    func newTab(in containerID: String) {
        let st = daemon.state
        guard let c = st.sidebar.container(id: containerID) else { return }
        let model = NewSessionModel(mode: .addTab(containerID: containerID), recents: st.recents, defaultAgent: st.defaultAgent,
                                    worktreeLocation: st.worktreeLocation, path: rowDirectory(c))
        presentForm(model) { [weak self] spec in
            guard let self else { return }
            self.createTab(spec, in: containerID, axis: .horizontal, deferLaunch: true)
        }
    }

    /// Cmd+D / Cmd+Shift+D: a plain terminal tab in the shown row, in the row's directory, split beside
    /// the active pane. No sheet: the daemon creates it straight away.
    func newTerminalTab(_ axis: SplitAxis) {
        guard let c = workspace.container, workspace.layout.active != nil, workspace.layout.count < RowLayout.maxPanes else {
            NSSound.beep()
            return
        }
        let spec = Spec(name: newTabName(c), path: rowDirectory(c), agent: "terminal", args: "", containerID: c.id)
        createTab(spec, in: c.id, axis: axis, deferLaunch: false)
    }

    private func createTab(_ spec: Spec, in containerID: String, axis: SplitAxis, deferLaunch: Bool) {
        // The row may be on screen in another window: that one gets the pane.
        let ws = window(showing: containerID)?.workspace ?? workspace
        let from = ws.currentRow == containerID ? ws.layout.active : ws.rememberedPane(row: containerID)
        ws.expectSplit(row: containerID, from: from, axis: axis)
        daemon.command(.create(spec: spec, deferLaunch: deferLaunch)) { [weak self] ack in
            guard let self else { return }
            self.report(ack)
            guard ack.ok, let s = ack.session else {
                ws.cancelSplit(row: containerID)
                return
            }
            (self.window(showing: containerID)?.workspace ?? self.workspace).paneCreated(s, row: containerID)
            if let p = ack.pendingLaunch { self.workspace.expectLaunch(p.session, resume: p.resume) }
        }
    }

    /// A row's directory: the active pane's session path, else any tab's, else the last one used.
    func rowDirectory(_ c: Container) -> String {
        let st = daemon.state
        let candidates = [workspace.rememberedPane(row: c.id), c.active].compactMap { $0 } + c.tabs.map(\.session)
        for s in candidates { if let p = st.sessions[s]?.path, !p.isEmpty { return p } }
        return st.recents.first?.path ?? "~"
    }

    /// A new tab's display name: the row's own name (its first tab's), which the daemon makes unique
    /// the way the TUI does for any create ("web" becomes "web 2").
    func newTabName(_ c: Container) -> String {
        let first = c.tabs.first.map { displayName($0.session) } ?? ""
        return first.isEmpty ? rowLabel(c) : first
    }

    // MARK: Closing panes

    /// Cmd+W: close the active pane and end its tab. Asks first unless the pane is a shell at its
    /// prompt; the last pane of a row always asks, since closing it kills the row's session.
    func closeActivePane() {
        guard let s = workspace.layout.active, isAlive(s) else { NSSound.beep(); return }
        let last = workspace.layout.count == 1
        let command = daemon.state.sessions[s]?.paneCommand
        let idle = ShellCommand.isShell(command)
        if !last && idle {
            closePane(s)
            return
        }
        let alert = NSAlert()
        let name = displayName(s)
        let running = command.flatMap { $0.isEmpty || idle ? nil : $0 }
        if last {
            alert.messageText = "Close “\(name)” and kill its session?"
            alert.informativeText = (running.map { "“\($0)” is still running in it. " } ?? "")
                + "It's the last pane of this row, so closing it kills the tmux session and whatever runs in it. This can't be undone."
            alert.addButton(withTitle: "Kill")
        } else {
            alert.messageText = "Close “\(name)”?"
            alert.informativeText = (running.map { "“\($0)” is still running in it. " } ?? "Something is still running in it. ")
                + "Closing the pane ends this tab and kills its tmux session."
            alert.addButton(withTitle: "Close")
        }
        alert.addButton(withTitle: "Cancel")
        alert.alertStyle = .warning
        alert.buttons[0].hasDestructiveAction = true
        runSheet(alert) { [weak self] ok in
            guard ok, let self else { return }
            if last { self.daemon.command(.kill(session: s)) { [weak self] in self?.report($0) } } else { self.closePane(s) }
        }
    }

    private func closePane(_ s: String) {
        workspace.closePane(s)
        daemon.command(.kill(session: s)) { [weak self] ack in
            guard let self else { return }
            self.report(ack)
            guard ack.ok else { self.workspace.closeFailed(s); return }
            // A closed pane ends its tab: it doesn't stay in the row as an exited tab.
            var sb = self.daemon.state.sidebar
            if sb.removeTab(session: s) { self.daemon.saveSidebar(sb) }
        }
    }

    func modify(_ s: String) {
        daemon.command(.getSpec(session: s)) { [weak self] ack in
            guard let self else { return }
            guard ack.ok, let spec = ack.spec else { self.report(ack); return }
            let model = NewSessionModel(modify: s, spec: spec, recents: self.daemon.state.recents)
            self.presentForm(model) { [weak self] spec in
                self?.daemon.command(.modify(session: s, spec: spec)) { [weak self] in self?.report($0) }
            }
        }
    }

    /// A new session (a new row).
    private func create(_ spec: Spec, section: String? = nil) {
        daemon.command(.create(spec: spec, deferLaunch: true, section: section)) { [weak self] ack in
            guard let self else { return }
            self.report(ack)
            guard ack.ok, let s = ack.session else { return }
            self.show(s)
            if let p = ack.pendingLaunch { self.workspace.expectLaunch(p.session, resume: p.resume) }
        }
    }

    private func presentForm(_ model: NewSessionModel, onSubmit: @escaping (Spec) -> Void) {
        closePalette()
        closeShortcuts()
        sidebar.commitFilter()
        let form = NewSessionFormView(model: model)
        let sheet = NSWindow(contentRect: NSRect(x: 0, y: 0, width: NewSessionFormView.width, height: form.fittingHeight),
                             styleMask: [.titled], backing: .buffered, defer: false)
        let bw = current!
        let close = { [weak bw, weak sheet] in
            guard let bw, let sheet else { return }
            bw.window.endSheet(sheet)
        }
        form.onSubmit = { spec in close(); onSubmit(spec) }
        form.onCancel = { close() }
        // The sheet's width is fixed; only its height follows the rows the form shows.
        form.onHeightChange = { [weak sheet, weak form] in
            guard let sheet, let form else { return }
            let h = form.fittingHeight
            if abs(sheet.contentLayoutRect.height - h) > 0.5 {
                sheet.setContentSize(NSSize(width: NewSessionFormView.width, height: h))
            }
        }
        sheet.contentView = form
        sheet.contentMinSize = NSSize(width: NewSessionFormView.width, height: 120)
        sheet.contentMaxSize = NSSize(width: NewSessionFormView.width, height: 4000)
        sheet.setContentSize(NSSize(width: NewSessionFormView.width, height: form.fittingHeight))
        sheet.initialFirstResponder = form.nameField
        sheet.appearance = theme.appearance
        bw.formModel = model
        bw.focus.begin(.sheet)
        bw.window.beginSheet(sheet) { [weak bw] _ in
            MainActor.assumeIsolated {
                bw?.formModel = nil
                if bw?.focus.end(.sheet) == true { bw?.workspace.focusActiveTerminal() }
            }
        }
        sheet.makeFirstResponder(form.nameField)
    }

    var sheetOpen: Bool { window.attachedSheet != nil }
    /// The open new-session / modify form's model (the focus test switches agents through it).
    var formModel: NewSessionModel? { current.formModel }

    @objc func showSettings() { showSettings(tab: nil) }

    /// The Settings window, on `tab` (nil: the one it was last on).
    func showSettings(tab: SettingsTab?) {
        if settings == nil {
            settings = SettingsWindowController(themePair: themePairModel, editor: editorModel)
            settings?.window.center()
        }
        if let tab { settings?.show(tab) }
        settings?.window.makeKeyAndOrderFront(nil)
    }

    // MARK: Keyboard shortcuts panel

    /// What the panel lists: a title and the menu items whose key equivalents it shows. The keys are
    /// read from the menus, so the panel and the menus can't disagree.
    static let shortcutCatalog: [(group: String, rows: [(title: String, items: [String])])] = [
        ("Sessions", [("New session", ["New Session…"]), ("Go to session", ["Go to Session…"]), ("Filter the sidebar", ["Filter Sessions"]),
                      ("Oldest waiting session", ["Oldest Waiting Session"]), ("Sidebar row 1 to 9", ["Row 1", "Row 9"])]),
        ("Panes (a row's tabs)", [("New terminal tab, split right", ["Split Right"]), ("New terminal tab, split down", ["Split Down"]),
                   ("New tab with an agent (sheet), split right", ["New Tab…"]),
                   ("Focus the pane left / right", ["Focus Pane Left", "Focus Pane Right"]),
                   ("Focus the pane up / down", ["Focus Pane Up", "Focus Pane Down"]),
                   ("Zoom the pane, or show the whole row", ["Zoom Pane"]),
                   ("Close pane and end its tab", ["Close Pane"]), ("Move or swap a pane", [])]),
        ("Navigation", [("Next session (row)", ["Next Session"]), ("Previous session (row)", ["Previous Session"]),
                        ("Next tab (pane)", ["Next Tab"]), ("Previous tab (pane)", ["Previous Tab"]),
                        ("Send the next key to the terminal", ["Send Next Key to Terminal"])]),
        ("Windows", [("New window (a row no window shows)", ["New Window"]), ("Close window", ["Close Window"]),
                     ("Open a row or pane in a new window", [])]),
        ("Other", [("Keyboard shortcuts", ["Keyboard Shortcuts"]), ("Settings", ["Settings…"]),
                   ("Copy / paste", ["Copy", "Paste"]), ("Undo / cut / select all", ["Undo", "Cut", "Select All"]),
                   ("Full screen", ["Enter Full Screen"]), ("Minimize", ["Minimize"]),
                   ("Hide Bosun / others", ["Hide Bosun", "Hide Others"]), ("Quit", ["Quit Bosun"])]),
    ]

    func menuItem(titled title: String, in menu: NSMenu? = NSApp.mainMenu) -> NSMenuItem? {
        for item in menu?.items ?? [] {
            if item.title == title || (title == "Enter Full Screen" && item.title == "Exit Full Screen") { return item }
            if let found = menuItem(titled: title, in: item.submenu) { return found }
        }
        return nil
    }

    func shortcutGroups() -> [ShortcutsPanel.Group] {
        Self.shortcutCatalog.map { g in
            ShortcutsPanel.Group(name: g.group, rows: g.rows.map { row in
                if row.items.isEmpty {
                    return ShortcutsPanel.Row(title: row.title, keys: g.group == "Windows" ? "drag it out of the window" : "drag its header")
                }
                let keys = row.items.compactMap { menuItem(titled: $0).flatMap(ShortcutsPanel.keyLabel) }
                if keys.isEmpty { return ShortcutsPanel.Row(title: row.title, keys: "off") }
                return ShortcutsPanel.Row(title: row.title, keys: keys.joined(separator: row.title.hasPrefix("Sidebar row") ? " … " : "  "))
            })
        }
    }

    @objc func toggleShortcuts() {
        if shortcutsPanel != nil { closeShortcuts() } else { showShortcuts() }
    }

    /// Open the panel over the main area. It never becomes first responder: the keyboard stays in the terminal.
    func showShortcuts() {
        guard !sheetOpen, shortcutsPanel == nil else { return }
        closePalette()
        sidebar.commitFilter()
        if focus.exception == .rename { window.makeFirstResponder(nil) }
        let p = ShortcutsPanel(groups: shortcutGroups(),
                               footnote: "Navigation keys follow [keybindings] in Bosun's config.toml. Esc or ⌘/ closes this.",
                               theme: theme)
        shortcutsPanel = p
        root.addSubview(p)
        layoutShortcuts()
        workspace.focusActiveTerminal()
    }

    func closeShortcuts() {
        guard let p = shortcutsPanel else { return }
        shortcutsPanel = nil
        p.removeFromSuperview()
        workspace.focusActiveTerminal()
    }

    private func layoutShortcuts() {
        guard let p = shortcutsPanel else { return }
        let main = workspace.area.frame
        var size = p.preferredSize(maxWidth: max(200, main.width - 40))
        // A short window gets a shorter panel whose list scrolls.
        size.height = min(size.height, max(120, main.height - 32))
        p.frame = NSRect(x: (main.midX - size.width / 2).rounded(), y: main.minY + max(16, (main.height - size.height) / 2).rounded(),
                         width: size.width, height: size.height)
    }

    // MARK: Palette

    func showPalette() {
        if palette != nil { closePalette(); return }
        guard !sheetOpen else { return }
        closeShortcuts()
        // Cmd+K from the filter's field keeps the filter (as Return would) and opens the palette.
        sidebar.commitFilter()
        let p = PaletteView(theme: theme)
        p.source = { [weak self] q in self?.paletteEntries(q) ?? [] }
        p.onClose = { [weak self] in self?.closePalette() }
        palette = p
        root.addSubview(p)
        p.refresh()
        layoutPalette()
        focus.begin(.palette)
        window.makeFirstResponder(p.field)
    }

    private func layoutPalette() {
        guard let p = palette else { return }
        let w = min(PaletteView.width, root.bounds.width - 40)
        let main = workspace.area.frame
        p.frame = NSRect(x: main.midX - w / 2, y: main.minY + min(120, main.height * 0.14), width: w, height: p.preferredHeight)
    }

    func closePalette() {
        guard let p = palette else { return }
        palette = nil
        p.removeFromSuperview()
        if focus.end(.palette) { workspace.focusActiveTerminal() }
    }

    var paletteOpen: Bool { palette != nil }

    private func paletteEntries(_ q: String) -> [PaletteView.Entry] {
        let st = daemon.state
        var scored: [(Int, Int, PaletteView.Entry)] = []
        var order = 0
        func add(_ s: String, section: String?, container: Container) {
            order += 1
            let name = displayName(s)
            let path = st.sessions[s]?.path.map { ($0 as NSString).abbreviatingWithTildeInPath } ?? ""
            let hay = [name, section, st.sessions[s]?.agent, path].compactMap { $0 }.joined(separator: " ")
            guard let score = q.isEmpty ? 0 : max(Fuzzy.score(q, name).map { $0 + 20 } ?? Int.min, Fuzzy.score(q, hay) ?? Int.min) as Int?,
                  score > Int.min else { return }
            let sub = [section, st.sessions[s]?.agent, path].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
            let title = name
            scored.append((score, order, PaletteView.Entry(title: title, subtitle: sub, state: displayState(s), exited: !isAlive(s),
                                                           action: { [weak self] in self?.sidebarActivated(session: s) })))
        }
        for c in st.sidebar.ungrouped { for t in c.tabs { add(t.session, section: nil, container: c) } }
        for sec in st.sidebar.sections { for c in sec.members { for t in c.tabs { add(t.session, section: sec.name, container: c) } } }
        var out = scored.sorted { $0.0 != $1.0 ? $0.0 > $1.0 : $0.1 < $1.1 }.map(\.2)
        out.append(PaletteView.Entry(title: q.isEmpty ? "New Session…" : "New Session “\(q)”…", subtitle: "⇧⌘N", state: nil, exited: false,
                                     action: { [weak self] in self?.newSession(name: q) }))
        return out
    }

    // MARK: Drops on the sidebar

    func dropOnSidebar(session: String, pasteboard: NSPasteboard) {
        show(session)
        DropReader.read(pasteboard) { [weak self] text in
            guard let text, let v = self?.workspace.pool.view(for: session) else { return }
            // A cold view needs tmux attached before the text can land.
            func insert(_ tries: Int) {
                if v.hasPaintedContent || tries == 0 { v.paste(text); return }
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { MainActor.assumeIsolated { insert(tries - 1) } }
            }
            insert(40)
        }
    }

    // MARK: Menus

    func sessionMenu(for s: String, rowItem: SidebarItem? = nil) -> NSMenu {
        let m = NSMenu()
        let alive = isAlive(s)
        let container = daemon.state.sidebar.container(holding: s)
        func item(_ title: String, _ action: @escaping () -> Void) -> NSMenuItem {
            let i = ClosureMenuItem(title: title, action: action)
            m.addItem(i)
            return i
        }
        if alive {
            _ = item("Restart") { [weak self] in self?.restart(s, resume: false) }
            _ = item("Resume") { [weak self] in self?.restart(s, resume: true) }
            _ = item("Modify…") { [weak self] in self?.modify(s) }
        } else if container.map({ daemon.state.sidebar.role(ofRow: $0.id) }) != .docked {
            _ = item("Restart") { [weak self] in self?.restartExited(s, resume: false) }
            _ = item("Resume") { [weak self] in self?.restartExited(s, resume: true) }
        }
        _ = item("Rename") { [weak self] in self?.startRename(s) }
        // A row's menu marks every done tab of the row; a tab's or a pane header's only that one.
        let isRowItem: Bool = { if case .row = rowItem?.kind { return true }; return false }()
        let unread = (isRowItem ? container?.tabs.map(\.session) ?? [s] : [s]).filter { displayState($0) == .done }
        if !unread.isEmpty { _ = item("Mark as Read") { [weak self] in self?.markRead(unread) } }
        if let c = container {
            if case .tab = rowItem?.kind {} else {
                m.addItem(.separator())
                addParkItems(to: m, row: c)
                m.addItem(.separator())
            }
            _ = item("New Tab in This Row…") { [weak self] in self?.newTab(in: c.id) }
            if case .tab = rowItem?.kind {
                _ = item("Move to Own Row") { [weak self] in self?.moveTabToOwnRow(s) }
                if alive { _ = item("Move to New Window") { [weak self] in self?.movePaneToNewWindow(s) } }
            } else if case .row = rowItem?.kind {
                if let other = window(showing: c.id), other !== current {
                    _ = item("Show Its Window") { [weak self] in self?.sidebarActivated(row: c.id) }
                } else {
                    _ = item("Open in New Window") { [weak self] in self?.openRowInNewWindow(c.id) }
                }
            } else if rowItem == nil, c.tabs.count > 1, alive {
                // A pane's header menu.
                _ = item("Move to New Window") { [weak self] in self?.movePaneToNewWindow(s) }
            }
            let isTab: Bool = { if case .tab = rowItem?.kind { return true }; return false }()
            if !isTab, let merge = mergeMenu(from: c) {
                let i = NSMenuItem(title: "Merge into", action: nil, keyEquivalent: "")
                i.submenu = merge
                m.addItem(i)
            }
        }
        m.addItem(.separator())
        if alive {
            _ = item("Kill…") { [weak self] in self?.confirmKill(s) }
            if let c = container, c.tabs.count > 1, case .row = rowItem?.kind {
                _ = item("Kill All Tabs…") { [weak self] in self?.confirmKillContainer(c) }
            }
            if daemon.state.sessions[s]?.worktreePath != nil, daemon.state.sessions[s]?.branch != nil {
                _ = item("Kill and Remove Worktree…") { [weak self] in self?.confirmKillRemoveWorktree(s) }
            }
        }
        // Always there, live or not: a tab's item removes that tab, anything else the whole row, killing
        // whatever still runs in it on the way (one confirmation when something does).
        if let c = container {
            let isTab: Bool = { if case .tab = rowItem?.kind { return true }; return false }()
            let live = isTab ? (alive ? [s] : []) : c.tabs.map(\.session).filter(isAlive)
            _ = item(live.isEmpty ? "Remove from Sidebar" : "Remove from Sidebar…") { [weak self] in
                if isTab { self?.removeTab(s) } else { self?.removeRowKilling(c.id) }
            }
        }
        return m
    }

    /// The other rows, under their section names, for "Merge into".
    private func mergeMenu(from row: Container) -> NSMenu? {
        let sb = daemon.state.sidebar
        let m = NSMenu()
        func add(_ rows: [Container]) {
            for c in rows where c.id != row.id {
                m.addItem(ClosureMenuItem(title: rowLabel(c)) { [weak self] in self?.mergeRow(row.id, into: c.id) })
            }
        }
        add(sb.ungrouped)
        for sec in sb.sections where sec.members.contains(where: { $0.id != row.id }) {
            if m.numberOfItems > 0 { m.addItem(.separator()) }
            m.addItem(.sectionHeader(title: sec.name))
            add(sec.members)
        }
        return m.numberOfItems > 0 ? m : nil
    }

    /// Merge row `from` into row `into`: its tabs become `into`'s tabs (and panes), at `index` among them
    /// (at the end with nil). Nothing restarts.
    func mergeRow(_ from: String, into: String, at index: Int? = nil) {
        guard from != into else { return }
        daemon.command(.mergeRows(from: from, into: into, index: index)) { [weak self] ack in
            self?.report(ack)
        }
    }

    /// Move one tab into row `into` (at `index` among its tabs, at the end with nil), or out into a row of its own
    /// with nil: at `index` among the rows of `section` (the ungrouped rows with nil), or right after the old one.
    func moveTab(_ session: String, into: String?, at index: Int? = nil, section: String? = nil) {
        daemon.command(.moveTab(session: session, container: into, index: index, section: section)) { [weak self] ack in
            self?.report(ack)
        }
    }

    func moveTabToOwnRow(_ session: String) { moveTab(session, into: nil) }

    func sectionMenu(for id: String) -> NSMenu {
        let m = NSMenu()
        // Snoozed and Docked come and go with their rows: nothing to make, rename or remove.
        if daemon.state.sidebar.sections.first(where: { $0.id == id })?.role != nil {
            m.addItem(ClosureMenuItem(title: "New Section…") { [weak self] in self?.newSection() })
            return m
        }
        m.addItem(ClosureMenuItem(title: "New Session in Section…") { [weak self] in self?.newSession(section: id) })
        m.addItem(ClosureMenuItem(title: "Section Defaults…") { [weak self] in self?.editSectionDefaults(id) })
        if daemon.state.sidebar.sections.first(where: { $0.id == id })?.defaults != nil {
            m.addItem(ClosureMenuItem(title: "Clear Section Defaults") { [weak self] in
                self?.daemon.command(.setSectionDefaults(section: id, defaults: nil)) { [weak self] in self?.report($0) }
            })
        }
        m.addItem(.separator())
        m.addItem(ClosureMenuItem(title: "Rename Section…") { [weak self] in self?.renameSection(id) })
        m.addItem(ClosureMenuItem(title: "New Section…") { [weak self] in self?.newSection() })
        m.addItem(.separator())
        m.addItem(ClosureMenuItem(title: "Remove Section (keep its sessions)") { [weak self] in self?.removeSection(id) })
        return m
    }

    func sidebarBackgroundMenu() -> NSMenu {
        let m = NSMenu()
        m.addItem(ClosureMenuItem(title: "New Session…") { [weak self] in self?.newSession() })
        m.addItem(ClosureMenuItem(title: "New Section…") { [weak self] in self?.newSection() })
        return m
    }

    private func buildMenu() {
        let main = NSMenu()
        func sub(_ title: String) -> NSMenu {
            let item = NSMenuItem()
            let menu = NSMenu(title: title)
            item.submenu = menu
            main.addItem(item)
            return menu
        }
        @discardableResult
        func add(_ menu: NSMenu, _ title: String, _ sel: Selector?, _ key: String = "", _ mods: NSEvent.ModifierFlags = [.command], target: AnyObject? = nil, tag: Int = 0) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: sel, keyEquivalent: key)
            i.keyEquivalentModifierMask = mods
            i.target = target ?? (sel.map { NSApp.responds(to: $0) ? nil : self } ?? nil)
            i.tag = tag
            menu.addItem(i)
            return i
        }

        let app = sub("Bosun")
        add(app, "About Bosun", #selector(NSApplication.orderFrontStandardAboutPanel(_:)), target: NSApp)
        app.addItem(.separator())
        add(app, "Settings…", #selector(showSettings as () -> Void), ",", target: self)
        app.addItem(.separator())
        add(app, "Notify When Waiting", #selector(toggleNotifyWhenWaiting), target: self)
        add(app, "Notify When Done", #selector(toggleNotifyWhenDone), target: self)
        add(app, "Show Dock Badge", #selector(toggleShowDockBadge), target: self)
        app.addItem(.separator())
        add(app, "Hide Bosun", #selector(NSApplication.hide(_:)), "h", target: NSApp)
        add(app, "Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option], target: NSApp)
        add(app, "Show All", #selector(NSApplication.unhideAllApplications(_:)), target: NSApp)
        app.addItem(.separator())
        add(app, "Quit Bosun", #selector(NSApplication.terminate(_:)), "q", target: NSApp)

        let file = sub("File")
        add(file, "New Session…", #selector(menuNewSession), "N", [.command, .shift], target: self)
        add(file, "New Tab…", #selector(menuNewTab), "t", target: self)
        add(file, "New Section…", #selector(menuNewSection), target: self)
        file.addItem(.separator())
        add(file, "Close Pane", #selector(menuCloseTile), "w", target: self)

        let edit = sub("Edit")
        edit.addItem(NSMenuItem(title: "Undo", action: Selector(("undo:")), keyEquivalent: "z"))
        edit.addItem(.separator())
        edit.addItem(NSMenuItem(title: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x"))
        edit.addItem(NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))
        edit.addItem(NSMenuItem(title: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v"))
        edit.addItem(NSMenuItem(title: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a"))

        let view = sub("View")
        add(view, "Split Right", #selector(menuSplitRight), "d", target: self)
        add(view, "Split Down", #selector(menuSplitDown), "D", [.command, .shift], target: self)
        view.addItem(.separator())
        let arrows: [(String, Unicode.Scalar, Int)] = [("Focus Pane Left", "\u{F702}", 0), ("Focus Pane Right", "\u{F703}", 1),
                                                         ("Focus Pane Up", "\u{F700}", 2), ("Focus Pane Down", "\u{F701}", 3)]
        for (t, k, tag) in arrows { add(view, t, #selector(menuFocusTile(_:)), String(Character(k)), [.command, .option], target: self, tag: tag) }
        add(view, "Zoom Pane", #selector(menuZoomPane), "\r", target: self)
        view.addItem(.separator())
        add(view, "Filter Sessions", #selector(menuFilter), "f", target: self)
        view.addItem(.separator())
        let themeItem = NSMenuItem(title: "Theme", action: nil, keyEquivalent: "")
        themeMenu.delegate = self
        themeItem.submenu = themeMenu
        view.addItem(themeItem)
        view.addItem(.separator())
        add(view, "Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control], target: nil)

        let go = sub("Go")
        add(go, "Go to Session…", #selector(menuPalette), "k", target: self)
        add(go, "Oldest Waiting Session", #selector(menuOldestWaiting), "U", [.command, .shift], target: self)
        go.addItem(.separator())
        // Their keys come from [keybindings] (updateNavMenu); the key monitor handles them first.
        navItems[.nextSession] = add(go, "Next Session", #selector(menuNav(_:)), target: self, tag: 0)
        navItems[.previousSession] = add(go, "Previous Session", #selector(menuNav(_:)), target: self, tag: 1)
        navItems[.nextTab] = add(go, "Next Tab", #selector(menuNav(_:)), target: self, tag: 2)
        navItems[.previousTab] = add(go, "Previous Tab", #selector(menuNav(_:)), target: self, tag: 3)
        navItems[.sendNextKey] = add(go, "Send Next Key to Terminal", #selector(menuNav(_:)), target: self, tag: 4)
        go.addItem(.separator())
        for i in 1...9 { add(go, "Row \(i)", #selector(menuJump(_:)), "\(i)", target: self, tag: i) }

        let session = sub("Session")
        add(session, "Restart", #selector(menuSession(_:)), target: self, tag: 0)
        add(session, "Resume", #selector(menuSession(_:)), target: self, tag: 1)
        add(session, "Modify…", #selector(menuSession(_:)), target: self, tag: 2)
        add(session, "Rename", #selector(menuSession(_:)), target: self, tag: 3)
        session.addItem(.separator())
        add(session, "Kill…", #selector(menuSession(_:)), target: self, tag: 4)
        add(session, "Remove Row from Sidebar…", #selector(menuSession(_:)), target: self, tag: 5)

        let win = sub("Window")
        add(win, "New Window", #selector(menuNewWindow), "n", target: self)
        add(win, "Move Pane to New Window", #selector(menuPaneToNewWindow), target: self)
        add(win, "Close Window", #selector(menuCloseWindow), "W", [.command, .shift], target: self)
        win.addItem(.separator())
        add(win, "Minimize", #selector(NSWindow.performMiniaturize(_:)), "m", target: nil)
        add(win, "Zoom", #selector(NSWindow.performZoom(_:)), target: nil)
        NSApp.windowsMenu = win

        let help = sub("Help")
        add(help, "Keyboard Shortcuts", #selector(toggleShortcuts), "/", target: self)
        NSApp.helpMenu = help

        NSApp.mainMenu = main
        updateNavMenu()
    }

    /// Show the [keybindings] nav keys on their Go menu items.
    private func updateNavMenu() {
        for (action, item) in navItems {
            guard let c = nav.chords[action] else {
                item.keyEquivalent = ""
                item.keyEquivalentModifierMask = []
                continue
            }
            var mods: NSEvent.ModifierFlags = []
            if c.mods.contains(.shift) { mods.insert(.shift) }
            if c.mods.contains(.control) { mods.insert(.control) }
            if c.mods.contains(.option) { mods.insert(.option) }
            let key: String
            switch c.key {
            case .left: key = "\u{F702}"
            case .right: key = "\u{F703}"
            case .up: key = "\u{F700}"
            case .down: key = "\u{F701}"
            case .function(let n): key = String(Character(Unicode.Scalar(0xF704 + UInt32(max(1, n)) - 1)!))
            case .letter(let ch): key = String(ch)
            }
            item.keyEquivalent = key
            item.keyEquivalentModifierMask = mods
        }
    }

    @objc func menuNewSession() { newSession() }
    @objc func menuNewSection() { newSection() }
    @objc func menuCloseTile() { closeActivePane() }
    @objc func menuSplitRight() { newTerminalTab(.horizontal) }
    @objc func menuSplitDown() { newTerminalTab(.vertical) }
    @objc func menuNewTab() {
        guard let c = workspace.container else { NSSound.beep(); return }
        newTab(in: c.id)
    }
    @objc func menuPalette() { showPalette() }
    @objc func menuFilter() { sidebar.toggleFilter() }
    @objc func menuZoomPane() { workspace.toggleZoom() }
    @objc func menuNewWindow() { openWindow(row: nil) }
    @objc func menuCloseWindow() { window.performClose(nil) }
    @objc func menuPaneToNewWindow() {
        guard let s = workspace.layout.active else { NSSound.beep(); return }
        movePaneToNewWindow(s)
    }

    @objc func menuFocusTile(_ sender: NSMenuItem) {
        workspace.moveFocus([Direction.left, .right, .up, .down][sender.tag])
    }

    @objc func menuNav(_ sender: NSMenuItem) {
        perform([NavAction.nextSession, .previousSession, .nextTab, .previousTab, .sendNextKey][sender.tag])
    }

    /// Cmd+1…9: the rows this window's sidebar shows, in order (with a filter, its rows).
    @objc func menuJump(_ sender: NSMenuItem) {
        let rows = sidebar.visibleRowIDs
        guard sender.tag - 1 < rows.count else { NSSound.beep(); return }
        sidebarActivated(row: rows[sender.tag - 1])
    }

    @objc func toggleNotifyWhenWaiting() { AppSettings.shared.notifyWhenWaiting.toggle() }
    @objc func toggleNotifyWhenDone() { AppSettings.shared.notifyWhenDone.toggle() }
    @objc func toggleShowDockBadge() { AppSettings.shared.showDockBadge.toggle() }

    /// The session that has been waiting for you the longest (waits that were already there when the app
    /// connected go in sidebar order).
    var oldestWaiting: String? {
        let order = daemon.state.sidebar.allRows.flatMap { $0.tabs.map(\.session) }
        func rank(_ s: String) -> Int { order.firstIndex(of: s) ?? Int.max }
        return waitingSince.filter { isAlive($0.key) }.min { ($0.value, rank($0.key)) < ($1.value, rank($1.key)) }?.key
    }

    @objc func menuOldestWaiting() {
        guard let s = oldestWaiting else {
            root.band.flash("Nothing is waiting")
            return
        }
        sidebarActivated(session: s)
    }

    @objc func menuSession(_ sender: NSMenuItem) {
        guard let s = workspace.activeSession else { return }
        switch sender.tag {
        case 0: isAlive(s) ? restart(s, resume: false) : restartExited(s, resume: false)
        case 1: isAlive(s) ? restart(s, resume: true) : restartExited(s, resume: true)
        case 2: modify(s)
        case 3: startRename(s)
        case 4: confirmKill(s)
        case 5: if let row = workspace.currentRow { removeRowKilling(row) }
        default: break
        }
    }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        switch item.action {
        case #selector(toggleNotifyWhenWaiting):
            item.state = AppSettings.shared.notifyWhenWaiting ? .on : .off
            return true
        case #selector(toggleNotifyWhenDone):
            item.state = AppSettings.shared.notifyWhenDone ? .on : .off
            return true
        case #selector(toggleShowDockBadge):
            item.state = AppSettings.shared.showDockBadge ? .on : .off
            return true
        default:
            break
        }
        if sheetOpen { return false }
        switch item.action {
        case #selector(menuSession(_:)):
            guard let s = workspace.activeSession else { return false }
            if item.tag == 2 || item.tag == 4 { return isAlive(s) }
            return true
        case #selector(menuCloseTile):
            return workspace.layout.active.map(isAlive) ?? false
        case #selector(menuNav(_:)):
            // Only while the terminal has the keyboard (the key monitor takes these keys first then).
            // Disabled otherwise, so Shift+arrows in the rename field or the palette, or while composing
            // with an input method, are never taken by the menu.
            return windowIsKey && focus.terminalOwnsKeyboard && !(window.firstResponder is NSText)
                && workspace.activeTerminal?.hasMarkedText() != true
        case #selector(menuFocusTile(_:)):
            return workspace.layout.count > 1
        case #selector(menuZoomPane):
            item.state = workspace.layout.isZoomed ? .on : .off
            return workspace.layout.count > 1
        case #selector(menuSplitRight), #selector(menuSplitDown):
            return workspace.layout.active != nil && workspace.layout.count < RowLayout.maxPanes
        case #selector(menuNewTab):
            return workspace.container != nil
        case #selector(menuNewWindow):
            return daemon.state.sidebar.allRows.contains { !isShown(row: $0.id, elsewhereThan: nil) }
        case #selector(menuPaneToNewWindow):
            return workspace.layout.count > 1 && workspace.layout.active.map(isAlive) == true
        default:
            return true
        }
    }

    // MARK: Theme menu (with live preview while hovering)

    func menuNeedsUpdate(_ menu: NSMenu) {
        guard menu === themeMenu else { return }
        menu.removeAllItems()
        let current = daemon.state.theme.name
        // With a Light/Dark pair set, the chrome follows the appearance, and this list sets only `theme`
        // (the TUI's, and the side that's "Same as Theme"), so say which theme is showing and why.
        if let note = daemon.state.themePair.note(theme: current) {
            let n = NSMenuItem(title: note, action: nil, keyEquivalent: "")
            n.isEnabled = false
            menu.addItem(n)
            let showing = NSMenuItem(title: "Showing \(theme.palette.name) (macOS is in \(appearance == .dark ? "Dark" : "Light") mode)",
                                     action: nil, keyEquivalent: "")
            showing.isEnabled = false
            menu.addItem(showing)
            menu.addItem(ClosureMenuItem(title: "Light and Dark Themes…") { [weak self] in self?.showSettings() })
            menu.addItem(.separator())
            let header = NSMenuItem(title: daemon.state.themePair.listHeader, action: nil, keyEquivalent: "")
            header.isEnabled = false
            menu.addItem(header)
        }
        var lastSource: String?
        for t in daemon.state.themes {
            if let l = lastSource, l != t.source { menu.addItem(.separator()) }
            lastSource = t.source
            let i = ClosureMenuItem(title: t.name) { [weak self] in
                self?.themeBeforePreview = nil
                self?.daemon.command(.setTheme(name: t.name, persist: true)) { [weak self] in self?.report($0) }
            }
            i.state = t.name == current ? .on : .off
            i.representedObject = t.name
            menu.addItem(i)
        }
        if daemon.state.themes.isEmpty {
            menu.addItem(NSMenuItem(title: "Not connected", action: nil, keyEquivalent: ""))
        }
    }

    func menuWillOpen(_ menu: NSMenu) {
        if menu === themeMenu { themeBeforePreview = daemon.state.theme.name }
    }

    func menu(_ menu: NSMenu, willHighlight item: NSMenuItem?) {
        guard menu === themeMenu, let name = item?.representedObject as? String, themeBeforePreview != nil else { return }
        daemon.command(.setTheme(name: name, persist: false))
    }

    func menuDidClose(_ menu: NSMenu) {
        guard menu === themeMenu, let original = themeBeforePreview else { return }
        // Closed without choosing: put the original back. (Choosing clears themeBeforePreview first.)
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.themeBeforePreview == original else { return }
                self.themeBeforePreview = nil
                self.daemon.command(.setTheme(name: original, persist: false))
            }
        }
    }
}

/// A menu item that runs a closure.
final class ClosureMenuItem: NSMenuItem {
    private let handler: () -> Void

    init(title: String, action: @escaping () -> Void) {
        handler = action
        super.init(title: title, action: #selector(fire), keyEquivalent: "")
        target = self
    }

    required init(coder: NSCoder) { fatalError() }

    @objc private func fire() { handler() }
}
