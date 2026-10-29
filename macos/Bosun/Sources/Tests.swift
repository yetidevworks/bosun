import AppKit
import BosunCore

// Scripted test modes (run through dev.sh against the test daemon):
//   --focustest [--shots DIR] [--results FILE]   drives the real event path and checks where keys land
//   --selftest [--results FILE]                  measures the section 1 budgets
//   --launchprobe                                prints launch timings and quits (used by --selftest)
//
// Events are injected into the app's own queue (NSApp.postEvent), so they go through AppKit's
// dispatch, the app's local monitors, hit-testing, key equivalents and the first responder,
// exactly as hardware events do. This works with the screen locked, where screencapture and
// CGEvent posting can't reach the window; screenshots are rendered in-process for the same reason.

func argValue(_ name: String) -> String? {
    let a = CommandLine.arguments
    guard let i = a.firstIndex(of: name), i + 1 < a.count else { return nil }
    return a[i + 1]
}

/// Report lines, echoed to stdout and saved to a file.
final class Report {
    private(set) var lines: [String] = []
    let path: String
    private(set) var failures = 0

    init(_ defaultName: String) {
        path = argValue("--results") ?? Paths.cacheDir.appendingPathComponent(defaultName).path
    }

    func say(_ s: String) {
        lines.append(s)
        print(s)
        fflush(stdout)
    }

    func check(_ name: String, _ ok: Bool, _ detail: String = "") {
        if !ok { failures += 1 }
        say("\(ok ? "PASS" : "FAIL")  \(name)\(detail.isEmpty ? "" : "  (\(detail))")")
    }

    func save() {
        try? (lines.joined(separator: "\n") + "\n").write(toFile: path, atomically: true, encoding: .utf8)
    }
}

@MainActor
class TestKit {
    let app: AppController
    let r: Report
    var w: MainWindow { app.window }
    var ws: Workspace { app.workspace }
    let shots: String?

    init(app: AppController, report: String) {
        self.app = app
        r = Report(report)
        shots = argValue("--shots")
        if let shots { try? FileManager.default.createDirectory(atPath: shots, withIntermediateDirectories: true) }
    }

    func sleep(_ ms: Double) async { try? await Task.sleep(nanoseconds: UInt64(ms * 1_000_000)) }

    @discardableResult
    func waitUntil(_ timeout: Double, _ cond: () -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(timeout)
        while Date() < end {
            if cond() { return true }
            await sleep(10)
        }
        return cond()
    }

    func shot(_ name: String) {
        guard let shots else { return }
        let ok = WindowSnapshot.write(w, to: URL(fileURLWithPath: "\(shots)/\(name).png"))
        r.say("      screenshot \(name).png \(ok ? "" : "FAILED")")
    }

    // MARK: Lookups

    func session(_ display: String) -> String? {
        app.daemon.state.sessionOrder.first { app.daemon.state.sessions[$0]?.display == display }
    }

    var tmuxSocket: String? { app.daemon.state.server?.tmuxSocket }

    /// The pane's visible text, straight from tmux (independent of the app's rendering).
    func pane(_ session: String) -> String {
        // With a timeout, so a slow tmux can never block the main thread for good.
        ShellEnv.run(Paths.tmux, (tmuxSocket.map { ["-L", $0] } ?? []) + ["capture-pane", "-p", "-J", "-t", "=\(session):"], timeout: 3) ?? ""
    }

    func tmux(_ args: [String]) {
        TmuxCLI.run(socket: tmuxSocket, args)
    }

    /// A tmux command's output, on the test daemon's socket.
    func tmuxOut(_ args: [String]) -> String {
        ShellEnv.run(Paths.tmux, (tmuxSocket.map { ["-L", $0] } ?? []) + args, timeout: 2) ?? ""
    }

    /// Pooled terminals whose tmux client is on another session than the view says.
    func driftedClients() async -> [String] {
        var drift: [String] = []
        for (s, v) in ws.pool.views where !v.exited {
            if let on = await clientSession(of: s), on != s { drift.append("\(app.displayName(s)) -> \(app.displayName(on))") }
        }
        return drift
    }

    /// The session tmux has the given session's view's client on.
    func clientSession(of s: String) async -> String? {
        guard let tty = ws.pool.view(for: s)?.ttyName else { return nil }
        let args = (tmuxSocket.map { ["-L", $0] } ?? []) + ["list-clients", "-F", "#{client_tty} #{session_name}"]
        let out = ShellEnv.run(Paths.tmux, args, timeout: 2) ?? ""
        return out.split(separator: "\n").first { $0.hasPrefix(tty + " ") }.map { String($0.dropFirst(tty.count + 1)) }
    }

    func paneShows(_ session: String, _ text: String, timeout: Double = 3) async -> Bool {
        await waitUntil(timeout) { pane(session).contains(text) }
    }

    // MARK: Events

    private var now: TimeInterval { ProcessInfo.processInfo.systemUptime }

    static let keyCodes: [Character: UInt16] = [
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14,
        "r": 15, "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "9": 25, "7": 26, "8": 28, "0": 29,
        "o": 31, "u": 32, "i": 34, "p": 35, "l": 37, "j": 38, "k": 40, "n": 45, "m": 46, " ": 49, "-": 27, ".": 47, "/": 44,
    ]

    func key(_ code: UInt16, _ chars: String, _ mods: NSEvent.ModifierFlags = [], ignoring: String? = nil, window: NSWindow? = nil) async {
        let target = window ?? w
        for type in [NSEvent.EventType.keyDown, .keyUp] {
            guard let e = NSEvent.keyEvent(with: type, location: .zero, modifierFlags: mods, timestamp: now, windowNumber: target.windowNumber,
                                           context: nil, characters: chars, charactersIgnoringModifiers: ignoring ?? chars,
                                           isARepeat: false, keyCode: code) else { continue }
            NSApp.postEvent(e, atStart: false)
        }
        await sleep(40)
    }

    func cmd(_ c: Character, _ extra: NSEvent.ModifierFlags = []) async {
        let s = extra.contains(.shift) ? String(c).uppercased() : String(c)
        await key(TestKit.keyCodes[c] ?? 0, s, NSEvent.ModifierFlags.command.union(extra))
    }

    func arrow(_ code: UInt16, _ mods: NSEvent.ModifierFlags) async {
        let ch: String = [123: "\u{F702}", 124: "\u{F703}", 125: "\u{F701}", 126: "\u{F700}"][code] ?? ""
        await key(code, ch, mods.union([.numericPad, .function]))
    }

    /// Type text as key presses; "\n" is Return.
    func type(_ text: String, window: NSWindow? = nil) async {
        for c in text {
            if c == "\n" {
                await key(36, "\r", window: window)
            } else {
                let lower = Character(c.lowercased())
                let shift = c.isUppercase
                await key(TestKit.keyCodes[lower] ?? 0, String(c), shift ? [.shift] : [], ignoring: String(c), window: window)
            }
        }
        await sleep(60)
    }

    /// Timestamps of the mouse events the tests posted. In the test modes, mouse-moved events that
    /// aren't among them (the real pointer) are dropped. (AppKit replaces a posted event's number.)
    private static var postedTimes: Set<Int64> = []
    /// To the microsecond: AppKit hands the event back with its timestamp's low bits changed.
    private static func micros(_ t: TimeInterval) -> Int64 { Int64((t * 1_000_000).rounded()) }
    static func posted(_ e: NSEvent) -> Bool { postedTimes.contains(micros(e.timestamp)) }

    func mouse(_ type: NSEvent.EventType, _ p: NSPoint, clicks: Int = 1, mods: NSEvent.ModifierFlags = [], window: NSWindow? = nil) -> NSEvent? {
        let e = NSEvent.mouseEvent(with: type, location: p, modifierFlags: mods, timestamp: now, windowNumber: (window ?? w).windowNumber,
                                   context: nil, eventNumber: 0, clickCount: clicks,
                                   pressure: type == .leftMouseDown || type == .rightMouseDown ? 1 : 0)
        if let e { TestKit.postedTimes.insert(TestKit.micros(e.timestamp)) }
        return e
    }

    /// A click at a point in window coordinates.
    func click(_ p: NSPoint, right: Bool = false, window: NSWindow? = nil) async {
        if let d = mouse(right ? .rightMouseDown : .leftMouseDown, p, window: window), let u = mouse(right ? .rightMouseUp : .leftMouseUp, p, window: window) {
            NSApp.postEvent(d, atStart: false)
            NSApp.postEvent(u, atStart: false)
        }
        await sleep(80)
    }

    func move(_ p: NSPoint) async {
        if let e = mouse(.mouseMoved, p) { NSApp.postEvent(e, atStart: false) }
        await sleep(40)
    }

    // MARK: Geometry (window coordinates)

    func sidebarPoint(_ session: String, onName: Bool = true) -> NSPoint? {
        guard let item = app.sidebar.itemFor(session: session) else { return nil }
        let o = app.sidebar.outline
        let row = o.row(forItem: item)
        guard row >= 0 else { return nil }
        let rect = o.rect(ofRow: row)
        var p = NSPoint(x: rect.minX + (onName ? 70 : rect.width - 20), y: rect.midY)
        if onName, let cell = o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SidebarCellView {
            let f = cell.nameField.frame
            p = o.convert(NSPoint(x: f.minX + 8, y: f.midY), from: cell)
        }
        return o.convert(p, to: nil)
    }

    /// The + on a section's header, in window coordinates.
    func sectionAddPoint(_ id: String) -> NSPoint? {
        let o = app.sidebar.outline
        for row in 0..<o.numberOfRows {
            if let it = o.item(atRow: row) as? SidebarItem, it.kind == .section(id),
               let cell = o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SectionCellView, !cell.addButton.isHidden {
                let b = cell.addButton
                return b.convert(NSPoint(x: b.bounds.midX, y: b.bounds.midY), to: nil)
            }
        }
        return nil
    }

    func newSessionInSection() async {
        let ws = app.workspace
        guard let work = app.daemon.state.sidebar.sections.first(where: { $0.name == "Work" }) else {
            r.check("the fixture has a Work section", false)
            return
        }
        let before = work.members.map(\.id)
        // Section defaults fill the sheet.
        app.daemon.command(.setSectionDefaults(section: work.id, defaults: SectionDefaults(path: "/tmp", agent: "terminal", args: "echo from-defaults")))
        await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == work.id }?.defaults != nil }
        // The step before may have just removed a row: the outline reloads first.
        await waitUntil(2) { self.sectionAddPoint(work.id) != nil }
        let p = sectionAddPoint(work.id)
        r.check("a section header has a + for a new session in it", p != nil)
        shot("10b-section-plus")
        if let p { await click(p) }
        await waitUntil(2) { app.sheetOpen }
        let model = app.formModel
        r.check("…which opens the new-session sheet for that section", app.sheetOpen && model?.mode == .create && model?.title == "New Session in Work",
                "title \(model?.title ?? "-")")
        guard let model, let sheet = w.attachedSheet else { return }
        r.check("…filled from the section's defaults", model.path == "/tmp" && model.agent == "terminal" && model.args == "echo from-defaults",
                "\(model.path) \(model.agent) \(model.args)")
        r.check("…and offering to save its values as the section's defaults", model.offersSaveAsDefaults)
        model.name = "in-section"
        model.args = ""
        model.saveAsDefaults = true
        await sleep(150)
        shot("10c-new-session-in-section")
        await key(36, "\r", window: sheet)
        await waitUntil(5) { self.session("in-section") != nil && ws.activeSession == self.session("in-section") }
        let s = session("in-section")
        let members = app.daemon.state.sidebar.sections.first { $0.id == work.id }?.members ?? []
        r.check("…and the new row is shown, at the end of the section", s != nil && members.map(\.id).dropLast() == before[...]
                && members.last?.tabs.map(\.session) == [s ?? "-"], "members \(members.map(\.name))")
        r.check("…and with the box ticked, the sheet's values become the section's defaults",
                await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == work.id }?.defaults?.args == "" })

        // Section Defaults…: the sheet without a name, whose Save stores them.
        app.editSectionDefaults(work.id)
        await waitUntil(2) { app.sheetOpen }
        let dm = app.formModel
        r.check("Section Defaults… opens the sheet for the section's defaults", dm?.isSectionDefaults == true && dm?.title == "Defaults for Work"
                && dm?.path == "/tmp", "title \(dm?.title ?? "-"), path \(dm?.path ?? "-")")
        if let dm, let sheet = w.attachedSheet {
            dm.path = "~/work"
            dm.agent = "codex"
            dm.options.codex.yolo = true
            await sleep(150)
            shot("10d-section-defaults")
            await key(36, "\r", window: sheet)
        }
        let saved = await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == work.id }?.defaults?.path == "~/work" }
        let d = app.daemon.state.sidebar.sections.first { $0.id == work.id }?.defaults
        r.check("…and Save stores them", saved && d?.agent == "codex" && d?.options.codex.yolo == true && !app.sheetOpen, "\(String(describing: d))")
        let menu = app.sectionMenu(for: work.id)
        let clear = menu.items.firstIndex { $0.title == "Clear Section Defaults" }
        if let clear { menu.performActionForItem(at: clear) }
        r.check("…and the section menu's Clear Section Defaults removes them",
                await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == work.id }?.defaults == nil } && clear != nil)
        r.check("…after which the menu no longer offers it", !app.sectionMenu(for: work.id).items.contains { $0.title == "Clear Section Defaults" })

        guard let s else { return }
        let row = rowID(s)
        app.daemon.command(.kill(session: s))
        await waitUntil(5) { !app.isAlive(s) }
        if let row {
            app.removeRow(row)
            await waitUntil(3) { app.daemon.state.sidebar.container(id: row) == nil }
        }
    }

    /// Choose `path` (a title, then submenu titles) from `menu`.
    @discardableResult
    func choose(_ menu: NSMenu, _ path: [String]) -> Bool {
        var m = menu
        for (n, title) in path.enumerated() {
            guard let i = m.items.firstIndex(where: { $0.title == title }) else { return false }
            if n == path.count - 1 {
                m.performActionForItem(at: i)
                return true
            }
            guard let sub = m.items[i].submenu else { return false }
            m = sub
        }
        return false
    }

    /// A row's right-click menu, as the sidebar builds it.
    func rowMenu(_ id: String) -> NSMenu? {
        guard let c = app.daemon.state.sidebar.container(id: id) else { return nil }
        return app.sessionMenu(for: c.active, rowItem: SidebarItem(kind: .row(id)))
    }

    func sectionCountText(_ id: String) -> String? {
        let o = app.sidebar.outline
        for row in 0..<o.numberOfRows {
            if let it = o.item(atRow: row) as? SidebarItem, it.kind == .section(id) {
                return (o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SectionCellView)?.countText
            }
        }
        return nil
    }

    func sectionPoint(_ id: String) -> NSPoint? {
        let o = app.sidebar.outline
        for row in 0..<o.numberOfRows {
            if let it = o.item(atRow: row) as? SidebarItem, it.kind == .section(id) {
                let r = o.rect(ofRow: row)
                return o.convert(NSPoint(x: r.minX + 40, y: r.midY), to: nil)
            }
        }
        return nil
    }

    /// The point of a row's own line in the sidebar (not one of its tabs), in window coordinates.
    func sidebarRowPoint(_ id: String) -> NSPoint? {
        let o = app.sidebar.outline
        for row in 0..<o.numberOfRows {
            guard let it = o.item(atRow: row) as? SidebarItem, it.kind == .row(id) else { continue }
            var p = NSPoint(x: o.rect(ofRow: row).minX + 70, y: o.rect(ofRow: row).midY)
            if let cell = o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SidebarCellView {
                let f = cell.nameField.frame
                p = o.convert(NSPoint(x: f.minX + 8, y: f.midY), from: cell)
            }
            return o.convert(p, to: nil)
        }
        return nil
    }

    func tileCenter(_ t: TileID) -> NSPoint? {
        guard let r = ws.tileRects[t] else { return nil }
        return ws.area.convert(NSPoint(x: r.midX, y: r.midY + 20), to: nil)
    }

    var firstResponderIsActiveTerminal: Bool {
        guard let t = ws.activeTerminal else { return false }
        return w.firstResponder === t
    }

    /// The row (container id) holding a session.
    func rowID(_ s: String) -> String? { app.daemon.state.sidebar.container(holding: s)?.id }

    /// The daemon's tabs of a row, by session.
    func tabs(_ rowID: String) -> [String] { app.daemon.state.sidebar.container(id: rowID)?.tabs.map(\.session) ?? [] }

    @discardableResult
    func waitPanesPainted(_ timeout: Double = 3) async -> Bool {
        await waitUntil(timeout) { ws.layout.panes.allSatisfy { ws.terminal(in: $0)?.hasPaintedContent == true } }
    }

    /// Show `session`'s row with that pane active, and wait until its panes have painted.
    func showRow(_ session: String) async {
        ws.show(session)
        await waitUntil(3) { ws.activeTerminal?.hasPaintedContent == true }
        await waitPanesPainted()
        await sleep(150)
    }

    /// Whether the pane has a line that is exactly `text` (a shell's output, not the command that printed it).
    func paneHasLine(_ s: String, _ text: String, timeout: Double = 4) async -> Bool {
        await waitUntil(timeout) { pane(s).split(separator: "\n").contains { $0.trimmingCharacters(in: .whitespaces) == text } }
    }

    /// Kill a tab the test made (through the daemon, as another client would), then take its ended
    /// tab out of the row so later sections see the fixture's tabs only.
    func killTab(_ s: String) async {
        _ = await app.daemon.request(.kill(session: s))
        await waitUntil(4) { !app.isAlive(s) }
        let row = rowID(s)
        var sb = app.daemon.state.sidebar
        if sb.removeTab(session: s) {
            let ack = await app.daemon.request(.sidebarSave(sb))
            if !ack.ok { r.say("      couldn't take \(app.displayName(s))'s tab out of its row: \(ack.error ?? "?")") }
        }
        if let row, !(await waitUntil(3) { !tabs(row).contains(s) }) {
            r.say("      \(app.displayName(s))'s ended tab is still in its row: \(tabs(row).map(app.displayName))")
        }
    }

    /// Wait until the daemon reports a shell as the pane's foreground command.
    @discardableResult
    func waitShell(_ s: String, timeout: Double = 8) async -> Bool {
        await waitUntil(timeout) { ShellCommand.isShell(app.daemon.state.sessions[s]?.paneCommand) }
    }
}

/// A drag that never left the app: what AppKit hands a drop destination.
final class FakeDrag: NSObject, NSDraggingInfo {
    var location: NSPoint
    let pasteboard: NSPasteboard
    weak var window: NSWindow?
    weak var source: AnyObject?
    let mask: NSDragOperation

    init(location: NSPoint, pasteboard: NSPasteboard, window: NSWindow, source: AnyObject? = nil, mask: NSDragOperation = .copy) {
        self.location = location
        self.pasteboard = pasteboard
        self.window = window
        self.source = source
        self.mask = mask
    }

    var draggingDestinationWindow: NSWindow? { window }
    var draggingSourceOperationMask: NSDragOperation { mask }
    var draggingLocation: NSPoint { location }
    var draggedImageLocation: NSPoint { location }
    var draggedImage: NSImage? { nil }
    var draggingPasteboard: NSPasteboard { pasteboard }
    var draggingSource: Any? { source }
    var draggingSequenceNumber: Int { 1 }
    func slideDraggedImage(to screenPoint: NSPoint) {}
    var draggingFormation: NSDraggingFormation = .default
    var animatesToDestination = false
    var numberOfValidItemsForDrop = 1
    func enumerateDraggingItems(options enumOpts: NSDraggingItemEnumerationOptions = [], for view: NSView?, classes classArray: [AnyClass],
                                searchOptions: [NSPasteboard.ReadingOptionKey: Any] = [:],
                                using block: (NSDraggingItem, Int, UnsafeMutablePointer<ObjCBool>) -> Void) {}
    var springLoadingHighlight: NSSpringLoadingHighlight { .none }
    func resetSpringLoading() {}
}

// MARK: - Focus test

@MainActor
final class FocusTest: TestKit {
    init(app: AppController) { super.init(app: app, report: "focustest.txt") }

    private var menuObserver: Any?

    /// The fixture's sessions, by display name.
    struct Fixture {
        var api, docs, parser, web, webTests, webShell, review1, review2: String
    }

    func run() async {
        r.say("# Bosun focus test  \(Date())")
        app.enableTestKey()
        // Context menus run a modal loop; close each one shortly after it opens, as a person would.
        menuObserver = NotificationCenter.default.addObserver(forName: NSMenu.didBeginTrackingNotification, object: nil, queue: nil) { note in
            let menu = note.object as? NSMenu
            nonisolated(unsafe) let m = menu
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { m?.cancelTracking() }
        }
        guard await waitUntil(10, { app.daemon.status == .connected && session("api") != nil }) else {
            r.check("connected to the test daemon with the fixture", false, "\(app.daemon.status)")
            r.save()
            return
        }
        app.focus.followsMouse = true
        guard let api = session("api"), let docs = session("docs"), let parser = session("parser"), let web = session("web"),
              let webTests = session("web tests"), let webShell = session("web shell"),
              let review1 = session("review-1"), let review2 = session("review-2"),
              rowID(web) != nil, rowID(webTests) == rowID(web), rowID(webShell) == rowID(web) else {
            r.check("fixture sessions present, with web, web tests and web shell as the three tabs of one row (./dev.sh up)", false)
            r.save()
            return
        }
        let f = Fixture(api: api, docs: docs, parser: parser, web: web, webTests: webTests, webShell: webShell, review1: review1, review2: review2)
        await showRow(api)
        r.check("start: the active terminal is first responder", firstResponderIsActiveTerminal)

        // --only a,b runs just those sections (rows, panes, zoom, termcolours, newpanes, clientmoves, done, notify, appearance, links, filter, find, sheet,
        // empty, tiledrag, sidebardrag, shortcuts, header, windows, sectionnew, parking, fonts, settings, sheetkeys).
        if let only = argValue("--only") {
            let parts = Set(only.split(separator: ",").map(String.init))
            if parts.contains("rows") { await rows(f) }
            if parts.contains("panes") { await panes(f) }
            if parts.contains("zoom") { await zoom(f) }
            if parts.contains("newpanes") { await newPanes(f) }
            if parts.contains("clientmoves") { await clientMoves(f) }
            if parts.contains("done") { await doneState(f) }
            if parts.contains("notify") { await notifications(f) }
            if parts.contains("appearance") { await appearancePair(f) }
            if parts.contains("termcolours") { await terminalColoursFollowEveryTheme() }
            if parts.contains("links") { await links() }
            if parts.contains("filter") { await filter(f) }
            if parts.contains("find") { await find(f) }
            if parts.contains("sectionnew") { await newSessionInSection() }
            if parts.contains("parking") { await parking(f) }
            if parts.contains("fonts") { await fonts(f) }
            if parts.contains("settings") { await settingsTabs() }
            if parts.contains("sheetkeys") { await sheetKeys() }
            await layoutSections(parts)
            if parts.contains("windows") { await windows(f) }
            if parts.contains("remove") { await removeFromSidebar() }
            await finish()
            return
        }

        // 0. Nothing typed into a session that the user didn't type: launching the fixture's agents
        // used to leave a Ctrl-L behind, echoed as `^L` at the fake agent's prompt.
        let strays = [api, docs, parser, review1, review2, webTests].filter { pane($0).contains("^L") }
        r.check("no stray ^L in the fixture's panes after launch", strays.isEmpty, strays.map(app.displayName).joined(separator: ", "))

        // 0b. tmux's status line: the app's clients attach as their own terminal type, get none of the
        // TUI's key hints, and a session only the app shows has no status line, so the terminal gets the row.
        let apiTTY = ws.activeTerminal?.ttyName ?? "?"
        let clientRows = tmuxOut(["list-clients", "-F", "#{client_tty}|||#{client_termname}|||#{E:status-right}"])
            .split(separator: "\n").map { $0.components(separatedBy: "|||") }
        let mine = clientRows.first { $0.first == apiTTY }
        r.check("the app's tmux clients attach as \(Paths.appTerm)", mine?[1] == Paths.appTerm, "\(mine ?? [])")
        r.check("…and tmux shows them none of the TUI's key hints", mine?.count == 3 && mine?[2] == "", "\(mine ?? [])")
        let statusOff = await waitUntil(3) { tmuxOut(["display", "-p", "-t", "=\(api):", "#{status}"]).hasPrefix("off") }
        r.check("a session only the app shows has no tmux status line", statusOff,
                tmuxOut(["display", "-p", "-t", "=\(api):", "#{status}"]).trimmingCharacters(in: .whitespacesAndNewlines))
        let paneRows = await waitUntil(2) {
            Int(tmuxOut(["display", "-p", "-t", "=\(api):", "#{pane_height}"]).trimmingCharacters(in: .whitespacesAndNewlines)) == ws.activeTerminal?.gridSize.rows
        }
        r.check("…so the pane uses every row of the terminal", paneRows,
                "pane \(tmuxOut(["display", "-p", "-t", "=\(api):", "#{pane_height}"]).trimmingCharacters(in: .whitespacesAndNewlines)) rows, terminal \(ws.activeTerminal?.gridSize.rows ?? 0)")
        shot("01-single-pane-row")

        // 1. Click a sidebar row, then type.
        if let p = sidebarPoint(docs) { await click(p) }
        await waitUntil(2) { ws.activeSession == docs && ws.activeTerminal?.hasPaintedContent == true }
        r.check("sidebar click shows the row", ws.activeSession == docs && ws.currentRow == rowID(docs))
        r.check("sidebar click leaves the keyboard in the terminal", firstResponderIsActiveTerminal,
                "firstResponder=\(String(describing: w.firstResponder.map { Swift.type(of: $0) }))")
        await type("after-click\n")
        r.check("typing after a sidebar click reaches the pane", await paneShows(docs, "got: after-click"))

        // 2. Right-click menu, then type.
        if let p = sidebarPoint(parser) { await click(p, right: true) }
        await sleep(400)
        r.check("right-click menu doesn't move the keyboard", firstResponderIsActiveTerminal)
        await type("after-menu\n")
        r.check("typing after a context menu reaches the pane", await paneShows(docs, "got: after-menu"))

        // 3. Collapse a section, then type.
        let work = app.daemon.state.sidebar.sections.first { $0.name == "Work" }?.id ?? ""
        if let p = sectionPoint(work) { await click(p) }
        await waitUntil(2) { app.daemon.state.sidebar.sections.first { $0.id == work }?.collapsed == true }
        r.check("clicking a section header collapses it", app.daemon.state.sidebar.sections.first { $0.id == work }?.collapsed == true)
        r.check("collapsing leaves the keyboard in the terminal", firstResponderIsActiveTerminal)
        await type("after-collapse\n")
        r.check("typing after a collapse reaches the pane", await paneShows(docs, "got: after-collapse"))
        if let p = sectionPoint(work) { await click(p) }
        await waitUntil(2) { app.daemon.state.sidebar.sections.first { $0.id == work }?.collapsed == false }
        let workCount = app.daemon.state.sidebar.sections.first { $0.id == work }?.members.count ?? -1
        r.check("an open section shows its row count too", sectionCountText(work) == "\(workCount)", sectionCountText(work) ?? "-")

        // A section with no rows shows closed, and a click doesn't open it or change its saved state.
        app.daemon.command(.insertSection(name: "Empty"))
        await waitUntil(3) { app.daemon.state.sidebar.sections.contains { $0.name == "Empty" } }
        if let empty = app.daemon.state.sidebar.sections.first(where: { $0.name == "Empty" })?.id {
            await waitUntil(2) { self.sectionPoint(empty) != nil }
            let o = app.sidebar.outline
            let shownClosed = { (0..<o.numberOfRows).contains { row in
                (o.item(atRow: row) as? SidebarItem).map { $0.kind == .section(empty) && !o.isItemExpanded($0) } == true } }
            r.check("a section with no rows shows closed, with a count of 0", shownClosed() && sectionCountText(empty) == "0",
                    "count \(sectionCountText(empty) ?? "-")")
            if let p = sectionPoint(empty) { await click(p) }
            await sleep(300)
            r.check("…and a click on it leaves it closed and its saved state alone",
                    shownClosed() && app.daemon.state.sidebar.sections.first { $0.id == empty }?.collapsed == false)
            r.check("…with the keyboard still in the terminal", firstResponderIsActiveTerminal)
            app.removeSection(empty)
            await waitUntil(3) { !app.daemon.state.sidebar.sections.contains { $0.id == empty } }
        }

        // 4. Rename in place (double-click the row's name), Return, then type. A single click on the selected
        // row's name only selects it.
        if let p = sidebarPoint(docs) { await click(p) }
        await sleep(NSEvent.doubleClickInterval * 1000 + 300)
        r.check("a single click on the selected row's name doesn't rename", app.focus.exception == nil && firstResponderIsActiveTerminal)
        if let p = sidebarPoint(docs) { await doubleClick(p) }
        await waitUntil(2) { app.focus.exception == .rename }
        r.check("double-clicking the row's name starts a rename", app.focus.exception == .rename)
        r.check("the rename field has the keyboard", w.firstResponder is NSText)
        await type("Docs Two")
        shot("02-rename-in-progress")
        await type("\n")
        await waitUntil(3) { app.daemon.state.sessions[docs]?.display == "Docs Two" }
        r.check("Return commits the rename through the daemon", app.daemon.state.sessions[docs]?.display == "Docs Two")
        await waitUntil(1) { firstResponderIsActiveTerminal }
        r.check("after rename the keyboard is back in the terminal", firstResponderIsActiveTerminal)
        await type("after-rename\n")
        r.check("typing after rename reaches the pane", await paneShows(docs, "got: after-rename"))
        // Esc cancels.
        app.sidebar.beginRename(session: docs)
        await type("nope")
        await key(53, "\u{1b}")
        await sleep(100)
        r.check("Esc cancels a rename", app.daemon.state.sessions[docs]?.display == "Docs Two" && app.focus.exception == nil)
        r.check("after Esc the keyboard is back in the terminal", firstResponderIsActiveTerminal)
        app.daemon.command(.rename(session: docs, display: "docs"))

        // 5–7. Rows are workspaces: switching, pane navigation, new and closed panes.
        await rows(f)
        await panes(f)
        await zoom(f)
        await newPanes(f)

        // 8. Focus follows the mouse in the three-pane row: move into each pane, type, check that pane.
        await showRow(web)
        var hoverOK = true
        for (i, t) in ws.layout.panes.enumerated() {
            guard let c = tileCenter(t) else { continue }
            await move(NSPoint(x: c.x - 3, y: c.y))
            await move(c)
            let ok = ws.activeTile == t && firstResponderIsActiveTerminal
            let landed: Bool
            if t == webShell {
                await type("echo hover\(i)-ok\n")
                landed = await paneHasLine(t, "hover\(i)-ok")
            } else {
                await type("hover\(i)\n")
                landed = await paneShows(t, "got: hover\(i)")
            }
            if !ok || !landed { hoverOK = false; r.say("      pane \(app.displayName(t)): active=\(ok) landed=\(landed)") }
        }
        r.check("the active ring follows the pointer and typing goes to the hovered pane", hoverOK)
        // Same point again (a pane moving under a still pointer) must not activate anything.
        let held = ws.activeTile
        if let other = ws.layout.panes.first(where: { $0 != held }), let c = tileCenter(other) {
            app.focus.setWindowKey(true)
            await move(c)
            await move(c)
            ws.activate(held)
            await move(c)
            r.check("a pointer that didn't move doesn't activate a pane", ws.activeTile == held)
        }
        // Setting off: hover does nothing, a click activates and is consumed.
        app.focus.followsMouse = false
        if let target = ws.layout.panes.first(where: { $0 != ws.activeTile && $0 != webShell }), let c = tileCenter(target) {
            await move(NSPoint(x: c.x + 5, y: c.y))
            r.check("with focus-follows-mouse off, hover doesn't activate", ws.activeTile != target)
            await click(c)
            r.check("a click activates an inactive pane", ws.activeTile == target && firstResponderIsActiveTerminal)
            await type("clicked\n")
            r.check("typing after a pane click reaches that pane", await paneShows(target, "got: clicked"))
        }
        app.focus.followsMouse = true

        // 9. Drops.
        let file = NSTemporaryDirectory() + "bosun drop (test).png"
        FileManager.default.createFile(atPath: file, contents: Data([0x89, 0x50, 0x4e, 0x47]))
        let escaped = ShellEscape.escape(file)
        let pb = NSPasteboard(name: NSPasteboard.Name("dev.yetidevworks.bosun.test-drag"))
        pb.clearContents()
        pb.writeObjects([URL(fileURLWithPath: file) as NSURL])
        if let dropTile = ws.layout.panes.first(where: { $0 != ws.activeTile && $0 != webShell }), let c = tileCenter(dropTile) {
            let info = FakeDrag(location: c, pasteboard: pb, window: w)
            let op = ws.area.draggingEntered(info)
            r.check("a file dragged over a pane is accepted (copy)", op == .copy)
            _ = ws.area.performDragOperation(info)
            await sleep(150)
            r.check("a drop activates the pane under the pointer", ws.activeTile == dropTile)
            await type("\n")
            r.check("the dropped path is inserted, shell-escaped, into that pane", await paneShows(dropTile, "got: \(escaped)"))
        }
        // Over a divider: refused.
        if let d = ws.area.subviews.first(where: { $0 is DividerView && !$0.isHidden }) {
            let p = ws.area.convert(NSPoint(x: d.frame.midX, y: d.frame.midY), to: nil)
            r.check("a drop on a divider is refused", ws.area.draggingUpdated(FakeDrag(location: p, pasteboard: pb, window: w)) == [])
            ws.area.draggingExited(nil)
        }
        // Image data that isn't a file: written to a temp file first.
        let imgPB = NSPasteboard(name: NSPasteboard.Name("dev.yetidevworks.bosun.test-drag-image"))
        imgPB.clearContents()
        let img = NSImage(size: NSSize(width: 4, height: 4))
        img.lockFocus(); NSColor.red.setFill(); NSRect(x: 0, y: 0, width: 4, height: 4).fill(); img.unlockFocus()
        imgPB.writeObjects([img])
        if let t = ws.layout.panes.first(where: { $0 != webShell }), let c = tileCenter(t) {
            _ = ws.area.performDragOperation(FakeDrag(location: c, pasteboard: imgPB, window: w))
            await sleep(150)
            await type("\n")
            let ok = await waitUntil(3) { pane(t).contains("got: ") && pane(t).contains("/drops/drop-") }
            r.check("image data (not a file) is written to a temp file and its path inserted", ok)
        }
        // Sidebar: a drop on a session row shows that row and inserts there; elsewhere refused.
        let sb: SidebarController = app.sidebar
        if let rowItem = sb.itemFor(session: review1) {
            let o = sb.outline
            let rowPoint = o.convert(NSPoint(x: o.rect(ofRow: o.row(forItem: rowItem)).midX, y: o.rect(ofRow: o.row(forItem: rowItem)).midY), to: nil)
            let info = FakeDrag(location: rowPoint, pasteboard: pb, window: w)
            let op = sb.outlineView(o, validateDrop: info, proposedItem: rowItem, proposedChildIndex: NSOutlineViewDropOnItemIndex)
            r.check("a file dragged over a sidebar session row is accepted", op == .copy)
            _ = sb.outlineView(o, acceptDrop: info, item: rowItem, childIndex: NSOutlineViewDropOnItemIndex)
            await waitUntil(3) { ws.activeSession == review1 && ws.activeTerminal?.hasPaintedContent == true }
            r.check("a drop on a sidebar row shows that row", ws.activeSession == review1 && ws.currentRow == rowID(review1))
            await sleep(300)
            await type("\n")
            r.check("…and inserts the path there", await paneShows(review1, "got: \(escaped)"))
            let bg = FakeDrag(location: o.convert(NSPoint(x: 40, y: o.bounds.height - 5), to: nil), pasteboard: pb, window: w)
            r.check("a drop on the sidebar background is refused", sb.outlineView(o, validateDrop: bg, proposedItem: nil, proposedChildIndex: 2) == [])
            let secItem = o.item(atRow: (0..<o.numberOfRows).first { (o.item(atRow: $0) as? SidebarItem)?.kind == .section(work) } ?? 0)
            let secPoint = sectionPoint(work) ?? .zero
            r.check("a drop on a section header is refused",
                    sb.outlineView(o, validateDrop: FakeDrag(location: secPoint, pasteboard: pb, window: w), proposedItem: secItem, proposedChildIndex: NSOutlineViewDropOnItemIndex) == [])
        }

        // 10. Palette: a match in another row replaces the whole main area.
        await showRow(web)
        let webViews = Set(ws.layout.panes)
        await cmd("k")
        r.check("Cmd+K opens the palette", app.paletteOpen && app.focus.exception == .palette)
        r.check("the palette field has the keyboard", w.firstResponder is NSText)
        await type("pars")
        shot("09-palette")
        await type("\n")
        await waitUntil(2) { ws.activeSession == parser }
        r.check("Return in the palette shows the match's row", ws.activeSession == parser && !app.paletteOpen && ws.layout.panes == [parser])
        r.check("…and no pane of the previous row stays on screen", webViews.allSatisfy { ws.pool.view(for: $0)?.isHidden ?? true })
        await waitUntil(1) { firstResponderIsActiveTerminal }
        r.check("after the palette the keyboard is back in the terminal", firstResponderIsActiveTerminal)
        await waitUntil(2) { ws.activeTerminal?.hasPaintedContent == true }
        await type("after-palette\n")
        r.check("typing after the palette reaches the pane", await paneShows(parser, "got: after-palette"))
        await cmd("k")
        await key(53, "\u{1b}")
        r.check("Esc closes the palette and returns the keyboard", !app.paletteOpen && firstResponderIsActiveTerminal)
        await cmd("k")
        if let p = tileCenter(ws.activeTile) { await click(NSPoint(x: p.x, y: 60)) }
        r.check("a click outside closes the palette", !app.paletteOpen && app.focus.exception == nil)
        // A tab of a multi-pane row: its row, with that pane active.
        await cmd("k")
        await type("web tests")
        await type("\n")
        await waitUntil(2) { ws.activeSession == webTests }
        r.check("Cmd+K to a tab shows its row with that pane active", ws.currentRow == rowID(web) && ws.activeSession == webTests && ws.layout.count == 3)

        // 11. Navigation keys from [keybindings] (defaults: Shift+Up/Down sessions, Shift+Left/Right tabs, Ctrl+v send next).
        await showRow(api)
        let liveRows = app.daemon.state.sidebar.allRows.filter { c in c.tabs.contains { app.isAlive($0.session) } }.map(\.id)
        await arrow(125, [.shift])
        let expectedNext = Sidebar.cycle(liveRows, from: rowID(api), forward: true)
        r.check("Shift+Down shows the next row in sidebar order", ws.currentRow == expectedNext,
                "now \(ws.activeSession.map(app.displayName) ?? "-")")
        r.check("…without the key reaching the terminal", !pane(api).contains("[1;2B"))
        await arrow(126, [.shift])
        r.check("Shift+Up goes back", ws.currentRow == rowID(api) && ws.activeSession == api)
        let navBound = tmuxOut(["list-keys", "-T", "root", "S-Down"]).contains("switch-client")
        let navDrift = await driftedClients()
        r.check("Shift+Up/Down never reach tmux's Shift+arrow switch-client bindings (no client moved)",
                navBound && navDrift.isEmpty, navBound ? navDrift.joined(separator: ", ") : "Bosun's S-Down binding isn't installed")
        // Shift+Down through the web row: all three panes come up at once.
        if let webRow = rowID(web), let i = liveRows.firstIndex(of: webRow), i > 0 {
            ws.selectRow(liveRows[i - 1])
            await sleep(100)
            await arrow(125, [.shift])
            r.check("Shift+Down onto a multi-tab row shows all its panes", ws.currentRow == webRow && ws.layout.count == 3)
        }
        await showRow(web)
        await waitUntil(2) { ws.activeTerminal?.hasPaintedContent == true }
        await key(9, "\u{16}", [.control], ignoring: "v")
        await arrow(125, [.shift])
        await sleep(200)
        await type("\n")
        let passed = await paneShows(web, "got: [1;2B")
        r.check("Ctrl+v then Shift+Down sends the key to the program instead of navigating", ws.activeSession == web && passed)
        let on = await clientSession(of: web)
        r.check("…and tmux kept the client on its session", on == web, "client on \(on ?? "-")")

        // 12. Cmd+1…9, Cmd+Shift+U.
        let rows = app.daemon.state.sidebar.visibleRows
        await cmd("2")
        r.check("Cmd+2 shows the second row", ws.currentRow == rows[1].id)
        await cmd("1")
        r.check("Cmd+1 shows the first row", ws.currentRow == rows[0].id)
        tmux(["set-option", "-p", "-t", "=\(review2):", "@bosun_state", "waiting"])
        tmux(["set-option", "-p", "-t", "=\(review2):", "@bosun_state_at", "\(Int(Date().timeIntervalSince1970 * 1000))"])
        tmux(["set-option", "-p", "-t", "=\(review2):", "@bosun_state_detail", "permission: Bash"])
        await waitUntil(3) { app.daemon.state.state(of: review2) == .waiting }
        let oldest = app.oldestWaiting
        await cmd("u", [.shift])
        r.check("Cmd+Shift+U shows the oldest waiting session", ws.activeSession == oldest && oldest != nil,
                "oldest \(oldest.map(app.displayName) ?? "-"), shown \(ws.activeSession.map(app.displayName) ?? "-")")
        tmux(["set-option", "-p", "-t", "=\(review2):", "@bosun_state", "idle"])
        tmux(["set-option", "-p", "-t", "=\(review2):", "@bosun_state_detail", ""])

        // 13. Ghostty bindings that clash with app shortcuts never fire in the terminal.
        await showRow(api)
        let fr = (w.firstResponder as? TerminalView)?.session
        r.check("showing a row puts its active pane's terminal first", ws.activeSession == api && fr == api,
                "active=\(ws.activeSession.map(app.displayName) ?? "-") firstResponder=\(fr.map(app.displayName) ?? String(describing: w.firstResponder))")
        await type("clash-check\n")
        _ = await paneShows(api, "got: clash-check")
        await cmd("k")
        await key(53, "\u{1b}")
        r.check("Cmd+K opens the palette instead of Ghostty's clear-screen", pane(api).contains("clash-check"))

        // 14. New-session sheet: Cmd+Shift+N, then Esc returns the keyboard.
        await cmd("n", [.shift])
        await waitUntil(2) { app.sheetOpen }
        r.check("Cmd+Shift+N opens the new-session sheet", app.sheetOpen && app.focus.exception == .sheet)
        r.check("…for a new row, not a tab", app.formModel.map { !$0.isAddTab } ?? false)
        await sleep(400)
        shot("10-new-session-sheet")
        if let sheet = w.attachedSheet { await key(53, "\u{1b}", window: sheet) }
        await waitUntil(2) { !app.sheetOpen }
        await waitUntil(1) { firstResponderIsActiveTerminal }
        r.check("closing the sheet returns the keyboard to the terminal", !app.sheetOpen && firstResponderIsActiveTerminal)
        await type("after-sheet\n")
        r.check("typing after the sheet reaches the pane", await paneShows(api, "got: after-sheet"))

        // 15. Create, then kill, a session through the app (the sheet's submit path): a new row.
        var spec = Spec(name: "temp-focus", path: "~", agent: "terminal", args: "python3 \(fakeAgentPath) temp")
        spec.containerID = nil
        app.daemon.command(.create(spec: spec, deferLaunch: true)) { [app] ack in
            if let s = ack.session { app.show(s) }
            if let p = ack.pendingLaunch { app.workspace.expectLaunch(p.session, resume: p.resume) }
        }
        await waitUntil(5) { session("temp-focus") != nil && ws.activeSession == session("temp-focus") }
        let temp = session("temp-focus")
        r.check("create (deferred launch) shows the new session's row", temp != nil && ws.activeSession == temp && ws.layout.count == 1)
        if let temp {
            r.check("the agent launches once the terminal is attached", await paneShows(temp, "fake agent · temp", timeout: 6))
            await sleep(800)
            r.check("…and nothing but the launch command is typed into it (no ^L)", !pane(temp).contains("^L"))
            let note = app.noticeText
            r.check("the created notice uses the display name (\(note))", !note.contains(temp) && note.contains("temp-focus"))
            let tempRow = rowID(temp)
            app.daemon.command(.kill(session: temp))
            await waitUntil(5) { !app.isAlive(temp) }
            r.check("kill ends the session", !app.isAlive(temp))
            await sleep(300)
            if let tempRow, app.daemon.state.sidebar.container(id: tempRow) != nil {
                r.check("a row whose tabs have all ended shows its exited placeholder",
                        ws.currentRow == tempRow && ws.layout.isEmpty && ws.tiles == [temp] && ws.activeTerminal == nil)
                shot("11-exited-row")
                app.removeRow(tempRow)
                await waitUntil(3) { app.daemon.state.sidebar.container(id: tempRow) == nil }
            }
            await waitUntil(2) { ws.currentRow != nil && ws.currentRow != tempRow }
            r.check("when the shown row goes, a neighbouring row is shown", ws.currentRow != nil && ws.currentRow != tempRow
                    && app.daemon.state.sidebar.container(id: ws.currentRow!) != nil)
        }

        // 15a. A section's +: the sheet for a new session in that section, and the new row lands at its end.
        await newSessionInSection()

        // 16. Themes: a light and a dark one for the screenshots, then back.
        let original = app.daemon.state.theme.name
        await showRow(web)
        for (name, file) in [("github-light", "12-theme-light"), ("tokyonight", "13-theme-dark")] {
            app.daemon.command(.setTheme(name: name, persist: false))
            await waitUntil(3) { app.daemon.state.theme.name == name }
            await sleep(300)
            r.check("theme \(name) applies to the chrome", app.theme.palette.name == name)
            shot(file)
        }
        app.daemon.command(.setTheme(name: original, persist: false))
        await waitUntil(3) { app.daemon.state.theme.name == original }

        // 16a. The Light/Dark pair following the appearance.
        await appearancePair(f)
        await terminalColoursFollowEveryTheme()

        // 16b. Done from the agent's Stop hook, on screen, until the user types into the pane.
        await doneState(f)

        // 16c. Notifications, the Dock badge and Cmd+Shift+U.
        await notifications(f)

        // 17. tmux moving a terminal's client (Bosun's TUI bindings typed inside it, or anything else).
        await clientMoves(f)

        // 17b. Cmd+click on URLs and paths.
        await links()

        // 17c. The sidebar filter (Option+Cmd+F).
        await filter(f)
        // 17d. Find in a pane (Cmd+F).
        await find(f)

        // 18–22. Sheet layout, placeholders, pane drag, sidebar drag, the shortcuts panel.
        await layoutSections(nil)
        await windows(f)
        await removeFromSidebar()
        // 22b. The terminal and interface fonts.
        await fonts(f)
        await settingsTabs()
        // 22c. The new-session sheet's Tab order and its Path field's list of folders.
        await sheetKeys()
        // 23. Snoozing and docking a row, and getting it back. Last: undocking gives docs a new session.
        await parking(f)
        await finish()
    }

    private func finish() async {
        // Nothing above may have left a tmux client on another session than its view says.
        let drift = await driftedClients()
        r.check("every terminal's tmux client is still on its own session", drift.isEmpty, drift.joined(separator: ", "))
        await sleep(200)
        checkFocusCount()

        r.say("")
        r.say("\(r.failures == 0 ? "ALL PASSED" : "\(r.failures) FAILED")")
        r.save()
        r.say("saved \(r.path)")
    }

    // MARK: Rows

    /// Switching rows replaces every pane, and the keyboard lands in the pane that was active in the row.
    func rows(_ f: Fixture) async {
        guard let webRow = rowID(f.web) else { return }
        await showRow(f.web)
        r.check("rows: a row with three tabs opens as three panes", ws.currentRow == webRow && Set(ws.layout.panes) == [f.web, f.webTests, f.webShell],
                "\(ws.layout.panes.map(app.displayName))")
        r.check("…each pane attached and painted", await waitPanesPainted())
        r.check("…laid out side by side and stacked, inside the main area", ws.layout.panes.allSatisfy { p in
            guard let rect = ws.tileRects[p] else { return false }
            return ws.area.bounds.contains(rect) && rect.width > 200 && rect.height > 150
        })
        // The remembered pane: web tests.
        ws.activate(f.webTests)
        await waitUntil(1) { firstResponderIsActiveTerminal }
        // A cold attach paints with the session's tmux status line, which goes a moment later.
        await sleep(500)
        shot("03-row-three-panes")
        r.check("…the row keeps its own name (its first tab's), whichever pane is active", app.rowLabel(app.daemon.state.sidebar.container(id: webRow)!) == "web")
        let views = Dictionary(uniqueKeysWithValues: ws.layout.panes.compactMap { p in ws.pool.view(for: p).map { (p, ObjectIdentifier($0)) } })
        let item = app.sidebar.outline.item(atRow: app.sidebar.outline.selectedRow) as? SidebarItem
        r.check("…the sidebar selects the row itself", item?.kind == .row(webRow))

        // To a single-pane row: everything of the web row goes.
        if let p = sidebarPoint(f.docs) { await click(p) }
        await waitUntil(2) { ws.currentRow == rowID(f.docs) && ws.activeTerminal?.hasPaintedContent == true }
        r.check("rows: clicking a single-pane row replaces all the panes with its one", ws.layout.panes == [f.docs] && ws.tiles == [f.docs])
        r.check("…the other row's views are hidden, not freed (warm)", [f.web, f.webTests, f.webShell].allSatisfy { ws.pool.view(for: $0)?.isHidden == true })
        r.check("…and the keyboard is in its pane", firstResponderIsActiveTerminal && (w.firstResponder as? TerminalView)?.session == f.docs)
        await sleep(150)
        shot("04-switch-to-single-pane-row")

        // Back: the same three views, and the keyboard in web tests again.
        if let p = sidebarRowPoint(webRow) { await click(p) }
        await waitUntil(2) { ws.currentRow == webRow }
        await waitPanesPainted()
        let sameViews = ws.layout.panes.allSatisfy { p in ws.pool.view(for: p).map { ObjectIdentifier($0) } == views[p] }
        r.check("rows: clicking the row again brings all three panes back", Set(ws.layout.panes) == [f.web, f.webTests, f.webShell])
        r.check("…with the same terminal views (warm, no reattach)", sameViews && ws.layout.panes.allSatisfy { ws.pool.view(for: $0)?.isHidden == false })
        r.check("…and the keyboard in the pane that was active (web tests)", ws.activeSession == f.webTests && firstResponderIsActiveTerminal
                && (w.firstResponder as? TerminalView)?.session == f.webTests, "active \(ws.activeSession.map(app.displayName) ?? "-")")
        await type("remembered\n")
        r.check("…typing reaches it", await paneShows(f.webTests, "got: remembered"))
        // The sidebar marks the active pane's tab under the selected row.
        let tabRow = app.sidebar.itemFor(session: f.webTests).map { app.sidebar.outline.row(forItem: $0) } ?? -1
        r.check("…the sidebar marks web tests as the active pane", tabRow >= 0 && app.sidebar.isShownSelection(app.sidebar.outline.item(atRow: tabRow) as! SidebarItem))
        await sleep(150)
        shot("05-back-to-three-panes")

        // Cmd+1…9 switch whole rows too.
        let visible = app.daemon.state.sidebar.visibleRows
        if let i = visible.firstIndex(where: { $0.id == webRow }), i < 9, let other = visible.firstIndex(where: { $0.id != webRow }) {
            await cmd(Character("\(other + 1)"))
            r.check("rows: Cmd+\(other + 1) replaces the panes with that row's", ws.currentRow == visible[other].id && ws.layout.count == liveCount(visible[other]))
            await cmd(Character("\(i + 1)"))
            r.check("…and Cmd+\(i + 1) brings the three panes back, keyboard in web tests", ws.currentRow == webRow && ws.layout.count == 3 && ws.activeSession == f.webTests)
        }

        // A tab's row in the sidebar activates that pane.
        if let p = sidebarPoint(f.webShell) { await click(p) }
        await waitUntil(1) { ws.activeSession == f.webShell }
        r.check("rows: clicking a tab under the row activates its pane", ws.activeSession == f.webShell && firstResponderIsActiveTerminal)
        await waitShell(f.webShell)
        await type("echo tabclick-ok\n")
        r.check("…and typing reaches it", await paneHasLine(f.webShell, "tabclick-ok"))
        if let p = sidebarPoint(f.webTests) { await click(p) }
        await waitUntil(1) { ws.activeSession == f.webTests }
    }

    private func liveCount(_ c: Container) -> Int { c.tabs.filter { app.isAlive($0.session) }.count }

    // MARK: Done from the agent's hook

    /// Run `bosun hook claude <event>` the way Claude Code runs it inside `session`'s pane: `TMUX` and
    /// `TMUX_PANE` name the pane, and the event's JSON is on stdin.
    func claudeHook(_ session: String, _ event: String, _ json: String) async -> Bool {
        guard let bin = Paths.bosunBinary() else { return false }
        let where_ = tmuxOut(["display", "-p", "-t", "=\(session):", "#{socket_path},#{pid},0 #{pane_id}"])
            .trimmingCharacters(in: .whitespacesAndNewlines).split(separator: " ")
        guard where_.count == 2 else { return false }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: bin)
        p.arguments = ["hook", "claude", event]
        var env = ProcessInfo.processInfo.environment
        env["TMUX"] = String(where_[0])
        env["TMUX_PANE"] = String(where_[1])
        env["BOSUN_HOOK_LOG"] = nil
        // The hook runs `tmux` from PATH, as it does inside a pane.
        env["PATH"] = (Paths.tmux as NSString).deletingLastPathComponent + ":" + (env["PATH"] ?? "/usr/bin:/bin")
        p.environment = env
        let stdin = Pipe()
        p.standardInput = stdin
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return false }
        stdin.fileHandleForWriting.write(Data(json.utf8))
        try? stdin.fileHandleForWriting.close()
        await waitUntil(5) { !p.isRunning }
        return !p.isRunning && p.terminationStatus == 0
    }

    /// The badge a session's sidebar item shows (its row's, or its tab's under a multi-tab row).
    func sidebarBadge(_ session: String) -> AgentState? {
        guard let item = app.sidebar.itemFor(session: session) else { return nil }
        return sidebarBadge(item)
    }

    func sidebarTrailing(_ session: String) -> String? {
        let o = app.sidebar.outline
        guard let item = app.sidebar.itemFor(session: session) else { return nil }
        let row = o.row(forItem: item)
        guard row >= 0 else { return nil }
        return (o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SidebarCellView)?.trailingText
    }

    /// How far the sidebar's last row ends above the bottom of the room it has, and the room above the first
    /// Snoozed or Docked header; nil without one.
    func parkedAtBottom() -> (slack: CGFloat, lead: CGFloat)? {
        let o = app.sidebar.outline
        guard let sv = o.enclosingScrollView, o.numberOfRows > 0,
              let first = (0..<o.numberOfRows).compactMap({ o.item(atRow: $0) as? SidebarItem }).first(where: { app.sidebar.lead(of: $0) > 0 })
        else { return nil }
        let room = sv.contentView.bounds.height - sv.contentInsets.top - sv.contentInsets.bottom
        return (room - o.rect(ofRow: o.numberOfRows - 1).maxY, app.sidebar.lead(of: first))
    }

    func sidebarBadge(_ item: SidebarItem) -> AgentState? {
        let o = app.sidebar.outline
        let row = o.row(forItem: item)
        guard row >= 0 else { return nil }
        return (o.view(atColumn: 0, row: row, makeIfNecessary: false) as? SidebarCellView)?.badge.state
    }

    /// Whether a pane's header and its sidebar item both show done, in the theme's `status_done` colour.
    func showsDone(_ s: String) -> Bool {
        guard let h = ws.headerStatus(s) else { return false }
        return h.state == .done && h.text == "done" && h.color == app.theme.done && sidebarBadge(s) == .done
    }

    func showsNotDone(_ s: String) -> Bool {
        guard let h = ws.headerStatus(s) else { return false }
        return h.state != .done && h.text != "done" && sidebarBadge(s) != .done
    }

    /// Claude's Stop hook marks a turn done while the pane is on screen; typing into the pane clears it, and so
    /// does the next prompt; the next turn finishing shows it again. A multi-tab row's badge shows a tab's done.
    func parking(_ f: Fixture) async {
        let st = { self.app.daemon.state.sidebar }
        guard let row = rowID(f.docs), let home = st().location(ofRow: row) else {
            r.check("parking: the docs fixture has a row", false)
            return
        }
        let backHome = { st().location(ofRow: row) == home && st().container(id: row)?.parked == nil }
        await showRow(f.api)

        // Snooze for an hour: into a collapsed Snoozed section, still running, with its time left.
        r.check("parking: a row's menu offers Snooze with 1 to 48 hours and Until It Needs Me, and Dock",
                rowMenu(row).map { m in
                    let sub = m.items.first { $0.title == "Snooze" }?.submenu?.items.map(\.title) ?? []
                    return sub.filter { !$0.isEmpty } == ["For 1 Hour", "For 2 Hours", "For 3 Hours", "For 6 Hours", "For 12 Hours",
                                                          "For 24 Hours", "For 48 Hours", "Until It Needs Me"] && m.items.contains { $0.title == "Dock" }
                } == true)
        if let m = rowMenu(row) { choose(m, ["Snooze", "For 1 Hour"]) }
        let snoozed = await waitUntil(3) { st().role(ofRow: row) == .snoozed }
        let sec = st().section(holdingRow: row)
        r.check("…Snooze For 1 Hour moves the row into a collapsed Snoozed section", snoozed && sec?.name == "Snoozed" && sec?.collapsed == true)
        r.check("…its session keeps running", app.isAlive(f.docs))
        if let sec { app.setSectionCollapsed(sec.id, false) }
        await waitUntil(2) { self.sidebarTrailing(f.docs) != nil }
        r.check("…and the row says how long is left", ["1h left", "59m left"].contains(sidebarTrailing(f.docs) ?? ""), sidebarTrailing(f.docs) ?? "-")
        shot("16a-snoozed-row")
        let pinned = { (p: (slack: CGFloat, lead: CGFloat)?) in p.map { abs($0.slack) <= 3 && $0.lead > 28 } == true }
        r.check("…Snoozed sits at the bottom of the sidebar, with room above it", pinned(parkedAtBottom()), "\(String(describing: parkedAtBottom()))")
        r.check("…its menu offers Wake Now and Snooze Again", rowMenu(row).map { m in
            m.items.contains { $0.title == "Wake Now" } && m.items.contains { $0.title == "Snooze Again" } } == true)

        // Typing into it brings it back.
        await showRow(f.docs)
        await type("back\n")
        r.check("parking: typing into a snoozed row brings it back where it was", await waitUntil(3) { backHome() },
                "\(String(describing: st().location(ofRow: row)))")
        r.check("…and the keys reach it", await paneShows(f.docs, "got: back"))
        r.check("…and the empty Snoozed section goes", !st().sections.contains { $0.role == .snoozed })

        // Until it needs you: back when its agent's turn ends.
        await showRow(f.api)
        if let m = rowMenu(row) { choose(m, ["Snooze", "Until It Needs Me"]) }
        await waitUntil(3) { st().role(ofRow: row) == .snoozed }
        await sleep(1200)
        r.check("parking: snoozed until it needs you, it stays snoozed while nothing does", st().role(ofRow: row) == .snoozed
                && st().container(id: row)?.parked?.untilNeeded == true)
        _ = await claudeHook(f.docs, "Stop", #"{"hook_event_name":"Stop","stop_hook_active":false}"#)
        r.check("…and comes back when its agent finishes", await waitUntil(4) { backHome() })

        // Dock: killed, kept in Docked; undock: back, resumed in place.
        if let m = rowMenu(row) { choose(m, ["Dock"]) }
        let docked = await waitUntil(5) { st().role(ofRow: row) == .docked && !self.app.isAlive(f.docs) }
        r.check("parking: Dock moves the row into Docked and ends its session", docked)
        if let sec = st().section(holdingRow: row) { app.setSectionCollapsed(sec.id, false) }
        await waitUntil(2) { self.sidebarTrailing(f.docs) == "docked" }
        r.check("…the row says docked", sidebarTrailing(f.docs) == "docked", sidebarTrailing(f.docs) ?? "-")
        app.sidebarActivated(row: row)
        await waitUntil(2) { ws.currentRow == row }
        r.check("…showing it offers Undock and Resume", ws.placeholderTitle?.hasSuffix("is docked") == true, ws.placeholderTitle ?? "-")
        shot("16b-docked-row")
        r.check("…Docked sits at the bottom of the sidebar too", pinned(parkedAtBottom()), "\(String(describing: parkedAtBottom()))")
        // Both at once: Snoozed above Docked, each with its line, the pair at the bottom.
        if let other = rowID(f.api), let m = rowMenu(other) {
            choose(m, ["Snooze", "For 1 Hour"])
            await waitUntil(3) { st().role(ofRow: other) == .snoozed }
            if let sec = st().section(holdingRow: other) { app.setSectionCollapsed(sec.id, false) }
            await waitUntil(2) { self.sidebarTrailing(f.api) != nil }
            let o = app.sidebar.outline
            let leads = (0..<o.numberOfRows).compactMap { o.item(atRow: $0) as? SidebarItem }.map { app.sidebar.lead(of: $0) }.filter { $0 > 0 }
            r.check("…with both, Snoozed and Docked each have room for a line above, and sit at the bottom",
                    leads.count == 2 && pinned(parkedAtBottom()), "\(leads) \(String(describing: parkedAtBottom()))")
            shot("16c-snoozed-and-docked")
            if let m = rowMenu(other) { choose(m, ["Wake Now"]) }
            r.check("…and Wake Now puts the other row back", await waitUntil(3) { st().container(id: other)?.parked == nil })
            app.sidebarActivated(row: row)
            await waitUntil(2) { ws.currentRow == row }
        }
        r.check("…and its menu offers Undock and Resume, not Restart", rowMenu(row).map { m in
            m.items.contains { $0.title == "Undock and Resume" } && !m.items.contains { $0.title == "Restart" } } == true)
        if let m = rowMenu(row) { choose(m, ["Undock and Resume"]) }
        let undocked = await waitUntil(8) {
            backHome() && st().container(id: row).map { c in c.tabs.count == 1 && self.app.isAlive(c.active) } == true
        }
        r.check("parking: Undock and Resume puts the row back where it was with a live session", undocked,
                "\(String(describing: st().container(id: row)))")
        if let c = st().container(id: row) {
            r.check("…resumed in the dead tab's place, under the same name", app.displayName(c.active) == "docs" && c.active != f.docs)
            r.check("…running its agent again", await paneShows(c.active, "fake agent · docs", timeout: 6))
        }
        r.check("…and no Docked section is left", !st().sections.contains { $0.role != nil })
    }

    func doneState(_ f: Fixture) async {
        let s = f.api
        await showRow(s)
        r.check("done: the fixture's pane starts out not done", showsNotDone(s), "\(String(describing: ws.headerStatus(s)))")

        let stopped = await claudeHook(s, "Stop", #"{"hook_event_name":"Stop","stop_hook_active":false}"#)
        r.check("done: bosun hook claude Stop runs inside the pane", stopped)
        let shown = await waitUntil(3) { showsDone(s) }
        r.check("…the visible pane's header and its sidebar row show done, in status_done's colour", shown,
                "header \(String(describing: ws.headerStatus(s))), row \(String(describing: sidebarBadge(s))), daemon \(String(describing: app.daemon.state.statuses[s]))")
        r.check("…the daemon reports it as idle from the hook, with done_at", app.daemon.state.statuses[s].map { $0.state == .idle && $0.isHook && $0.doneAt != nil } == true)
        await sleep(400)
        r.check("…and it stays done while the pane is on screen", showsDone(s))
        shot("14-done-header-and-row")
        let doneCount = app.headerStatus
        // In front isn't enough while nobody's at the Mac: no keys, clicks or pointer since it finished.
        await sleep((AppController.readAfter + 1) * 1000)
        r.check("done: as the front pane with nobody using the Mac since it finished, it stays unread", showsDone(s),
                "header \(String(describing: ws.headerStatus(s))), row \(String(describing: sidebarBadge(s)))")
        // Read in place: the pointer moves over the window, then two more seconds as the active pane.
        await move(NSPoint(x: w.frame.width - 40, y: 200))
        let read = await waitUntil(AppController.readAfter + 1.5) { ws.headerStatus(s)?.state == .seen }
        r.check("done: once you're back, two seconds as the front pane reads it: \"done · read\" in the header, a hollow ring in the done colour",
                read && ws.headerStatus(s)?.text == "done · read" && sidebarBadge(s) == .seen && app.sidebar.itemFor(session: s) != nil,
                "header \(String(describing: ws.headerStatus(s))), row \(String(describing: sidebarBadge(s)))")
        r.check("…and the header's done count drops it", app.headerStatus != doneCount, "\(doneCount) → \(app.headerStatus)")

        await type("thanks\n")
        r.check("done: typing into the pane clears it", await waitUntil(2) { showsNotDone(s) },
                "header \(String(describing: ws.headerStatus(s))), row \(String(describing: sidebarBadge(s)))")
        r.check("…and the keys reached the program", await paneShows(s, "got: thanks"))

        // The next prompt (running), then the turn finishing again.
        _ = await claudeHook(s, "UserPromptSubmit", #"{"hook_event_name":"UserPromptSubmit","prompt":"again"}"#)
        r.check("done: the next prompt shows running", await waitUntil(3) { ws.headerStatus(s)?.state == .running && sidebarBadge(s) == .running })
        let started = app.daemon.state.statuses[s]?.startedAt
        let sinceStart = started.map { Int64(TurnElapsed.nowMs()) - Int64($0) } ?? -1
        r.check("…with the turn's start from the daemon, just now", (0..<5_000).contains(sinceStart), "\(sinceStart) ms ago")
        r.check("…and its timer in the header and on the sidebar row", ws.headerStatus(s)?.text == "running · <1m" && sidebarTrailing(s) == "<1m",
                "header \(ws.headerStatus(s)?.text ?? "-"), row \(sidebarTrailing(s) ?? "-")")
        shot("14b-running-timer")
        await sleep(20)
        _ = await claudeHook(s, "Stop", #"{"hook_event_name":"Stop","stop_hook_active":false}"#)
        r.check("…and the next Stop shows done again, after the earlier input", await waitUntil(3) { showsDone(s) })
        r.check("…and the timer goes with the turn", app.daemon.state.statuses[s]?.startedAt == nil && sidebarTrailing(s) != "<1m",
                "row \(sidebarTrailing(s) ?? "-")")
        await type("ok\n")
        r.check("…until the next keys", await waitUntil(2) { showsNotDone(s) })

        // A tab of a multi-tab row: the tab and the row's own badge show done.
        if let webRow = rowID(f.web), let rowItem = app.sidebar.outline.item(atRow: (0..<app.sidebar.outline.numberOfRows).first {
            (app.sidebar.outline.item(atRow: $0) as? SidebarItem)?.kind == .row(webRow) } ?? -1) as? SidebarItem {
            await showRow(f.webTests)
            ws.activate(f.webTests)
            await waitUntil(1) { firstResponderIsActiveTerminal }
            _ = await claudeHook(f.webTests, "Stop", #"{"hook_event_name":"Stop","stop_hook_active":false}"#)
            let agg = await waitUntil(3) { showsDone(f.webTests) && sidebarBadge(rowItem) == .done }
            r.check("done: a tab's done shows on its pane, its tab and the multi-tab row's badge", agg,
                    "tab \(String(describing: sidebarBadge(f.webTests))), row \(String(describing: sidebarBadge(rowItem)))")
            await sleep(300)
            shot("15-done-in-a-multi-tab-row")
            // Mark as Read on the row (before the two seconds are up): its done tab is read straight away.
            let mark = app.sessionMenu(for: f.webTests, rowItem: rowItem).items.first { $0.title == "Mark as Read" }
            r.check("done: the row's menu has Mark as Read while a tab is done", mark != nil)
            if let mark { NSApp.sendAction(mark.action!, to: mark.target, from: mark) }
            r.check("…which shows the tab and the row as read at once", await waitUntil(0.5) { sidebarBadge(f.webTests) == .seen && sidebarBadge(rowItem) == .seen },
                    "tab \(String(describing: sidebarBadge(f.webTests))), row \(String(describing: sidebarBadge(rowItem)))")
            r.check("…and the menu no longer offers it", app.sessionMenu(for: f.webTests, rowItem: rowItem).items.contains { $0.title == "Mark as Read" } == false)
            await type("seen\n")
            r.check("…and typing into that pane clears all three", await waitUntil(2) { showsNotDone(f.webTests) && sidebarBadge(rowItem) != .done && sidebarBadge(rowItem) != .seen })
            _ = await claudeHook(f.webTests, "SessionStart", #"{"hook_event_name":"SessionStart"}"#)
        }
        // Leave the fixture as it was: idle from its hook.
        _ = await claudeHook(s, "SessionStart", #"{"hook_event_name":"SessionStart"}"#)
    }

    // MARK: Fonts

    func fonts(_ f: Fixture) async {
        await showRow(f.api)
        let s = AppSettings.shared
        let o = app.sidebar.outline
        guard let item = app.sidebar.itemFor(session: f.api), let term = app.pool.view(for: f.api) else {
            r.check("fonts: the api row is listed with its terminal", false)
            return
        }
        let rowH = { o.rect(ofRow: o.row(forItem: item)).height }
        let nameFont = { (o.view(atColumn: 0, row: o.row(forItem: item), makeIfNecessary: false) as? SidebarCellView)?.nameField.font }
        let termH = { term.frame.height }
        let (h0, th0, grid0) = (rowH(), termH(), term.gridSize)
        let system = NSFont.systemFont(ofSize: 13).familyName
        r.check("fonts: by default the interface is the system font at 13 pt", nameFont()?.pointSize == 13 && nameFont()?.familyName == system,
                "\(String(describing: nameFont()))")

        // The interface font: a family and a size, live.
        let family = "Helvetica Neue"
        s.interfaceFontFamily = family
        s.interfaceFontSize = 16
        await waitUntil(2) { nameFont()?.pointSize == 16 }
        r.check("fonts: Interface font and size apply at once to the sidebar's rows", nameFont()?.familyName == family && nameFont()?.pointSize == 16,
                "\(String(describing: nameFont()))")
        r.check("…the rows grow with it", rowH() > h0, "\(h0) → \(rowH())")
        await waitUntil(2) { termH() < th0 }
        r.check("…and so do the pane headers (the terminal below gets shorter)", termH() < th0, "\(th0) → \(termH())")
        r.check("…the terminal's own font is left alone", term.gridSize.cols == grid0.cols, "\(grid0) → \(term.gridSize)")
        shot("24-interface-font")

        // The terminal font: its size and family reach the terminals at once.
        s.terminalFontSize = 24
        await waitUntil(3) { term.gridSize.cols < grid0.cols }
        r.check("fonts: Terminal size applies at once: the terminal's grid has fewer columns", term.gridSize.cols < grid0.cols, "\(grid0) → \(term.gridSize)")
        s.terminalFontFamily = "Menlo"
        let overrides = (try? String(contentsOf: Paths.cacheDir.appendingPathComponent("ghostty-overrides.conf"), encoding: .utf8)) ?? ""
        r.check("…and Terminal font goes into the terminals' config, replacing the Ghostty config's families",
                overrides.contains(#"font-family = """#) && overrides.contains(#"font-family = "Menlo""#) && overrides.contains("font-size = 24"), overrides.split(separator: "\n").filter { $0.hasPrefix("font") }.joined(separator: "; "))
        shot("24b-terminal-font")

        // Back to the defaults.
        s.terminalFontFamily = nil
        s.terminalFontSize = 0
        s.interfaceFontFamily = nil
        s.interfaceFontSize = Fonts.baseInterfaceSize
        await waitUntil(3) { term.gridSize.cols == grid0.cols && rowH() == h0 && nameFont()?.familyName == system }
        r.check("fonts: back to From Ghostty config and System, everything is as it was",
                term.gridSize.cols == grid0.cols && rowH() == h0 && nameFont()?.familyName == system && termH() == th0,
                "\(term.gridSize) \(rowH()) \(termH()) \(String(describing: nameFont()))")
    }

    // MARK: Settings window

    func settingsTabs() async {
        app.showSettings(tab: .general)
        guard let sw = app.settingsWindow else {
            r.check("settings: the Settings window opens", false)
            return
        }
        await sleep(300)
        let items = sw.toolbar?.items.map(\.label) ?? []
        r.check("settings: a toolbar with a tab per group: General, Appearance, Fonts, Notifications",
                items == SettingsTab.allCases.map(\.title), "\(items)")
        r.check("…in the preferences style", sw.toolbarStyle == .preference)
        var heights: [SettingsTab: CGFloat] = [:]
        for tab in SettingsTab.allCases {
            app.showSettings(tab: tab)
            await sleep(500)
            heights[tab] = sw.frame.height
            r.check("…\(tab.title) shows its tab, titled with its name", sw.title == tab.title, sw.title)
            if let shots {
                sw.contentView?.layoutSubtreeIfNeeded()
                sw.displayIfNeeded()
                let ok = WindowSnapshot.write(sw, to: URL(fileURLWithPath: "\(shots)/25-settings-\(tab.rawValue).png"))
                r.say("      screenshot 25-settings-\(tab.rawValue).png \(ok ? "" : "FAILED")")
            }
        }
        r.check("…and the window takes each tab's height", Set(heights.values).count > 1, "\(heights.mapValues { Int($0) })")
        let screen = sw.screen?.visibleFrame.height ?? 0
        r.check("…never taller than the screen", heights.values.allSatisfy { $0 <= screen }, "\(heights.mapValues { Int($0) }) on \(Int(screen))")
        sw.close()
        ws.focusActiveTerminal()
    }

    // MARK: New-session sheet keys

    /// Tab walks the sheet's fields in the TUI form's order, Left and Right change the agent and session mode,
    /// Space ticks a box, and the Path field lists the folders it could go on to.
    func sheetKeys() async {
        let dir = "/tmp/bosun-sheet-paths-\(ProcessInfo.processInfo.processIdentifier)"
        let fm = FileManager.default
        for d in ["alpha/inner", "alpine", "beta"] { try? fm.createDirectory(atPath: "\(dir)/\(d)", withIntermediateDirectories: true) }
        defer { try? fm.removeItem(atPath: dir) }

        app.newSession()
        await waitUntil(2) { app.sheetOpen }
        guard let model = app.formModel, let sheet = w.attachedSheet, let form = sheet.contentView as? NewSessionFormView else {
            r.check("sheet keys: the new-session sheet opens", false)
            return
        }
        model.agent = "claude"
        model.options.claude.sessionMode = .new
        model.options.claude.skipPermissions = false
        model.worktree = false
        await sleep(150)
        let tab = { await self.key(48, "\t", window: sheet) }
        let backTab = { await self.key(48, "\u{19}", .shift, window: sheet) }
        r.check("sheet keys: the sheet opens on Name", form.focusedField == "name", form.focusedField ?? "-")
        var seen: [String] = []
        for _ in 0..<8 {
            await tab()
            seen.append(form.focusedField ?? "-")
        }
        r.check("…and Tab walks Path, the worktree box, Agent, Arguments, Session and Claude's two boxes, then back to Name",
                seen == ["path", "worktree", "agent", "args", "mode:new", "claude-skip", "claude-rc", "name"], "\(seen)")
        await backTab()
        r.check("…and Shift+Tab goes back round", form.focusedField == "claude-rc", form.focusedField ?? "-")
        await backTab()
        await key(49, " ", window: sheet)
        r.check("…Space ticks the box with the keyboard", model.options.claude.skipPermissions)
        await key(49, " ", window: sheet)
        await backTab()
        await arrow(124, [])
        r.check("…Right on Session picks the next mode, keeping the keyboard there",
                model.options.claude.sessionMode == .continue && form.focusedField == "mode:continue", "\(model.options.claude.sessionMode) \(form.focusedField ?? "-")")
        await arrow(123, [])
        await arrow(123, [])
        r.check("…and Left goes back, wrapping round", model.options.claude.sessionMode == .resume, "\(model.options.claude.sessionMode)")
        await backTab()
        await backTab()
        let next = agentNames[(agentNames.firstIndex(of: "claude")! + 1) % agentNames.count]
        await arrow(124, [])
        r.check("…Right on Agent picks the next agent", form.focusedField == "agent" && model.agent == next, "\(form.focusedField ?? "-") \(model.agent)")
        await arrow(123, [])
        r.check("…and Left the one before", model.agent == "claude", model.agent)
        await backTab()
        await key(49, " ", window: sheet)
        await sleep(100)
        await tab()
        r.check("…ticking the worktree box puts Branch next in the order", model.worktree && form.focusedField == "branch", form.focusedField ?? "-")
        model.worktree = false
        await sleep(100)

        // The Path field's list of folders.
        sheet.makeFirstResponder(form.pathField)
        form.pathField.stringValue = "\(dir)/a"
        form.pathField.currentEditor()?.selectedRange = NSRange(location: (form.pathField.stringValue as NSString).length, length: 0)
        await type("l", window: sheet)
        let list = form.suggestions
        r.check("path: typing lists the folders that match", !list.isHidden && list.entries.map(\.name) == ["alpha", "alpine"], "\(list.entries.map(\.name))")
        if let shots {
            sheet.displayIfNeeded()
            let ok = WindowSnapshot.write(sheet, to: URL(fileURLWithPath: "\(shots)/26-new-session-paths.png"))
            r.say("      screenshot 26-new-session-paths.png \(ok ? "" : "FAILED")")
        }
        await arrow(124, [])
        r.check("…Right completes as far as they agree", model.path == "\(dir)/alp" && list.entries.count == 2, model.path)
        await arrow(125, [])
        await arrow(125, [])
        await arrow(126, [])
        r.check("…Down and Up walk the list", list.highlighted == 0, "\(String(describing: list.highlighted))")
        if let shots {
            sheet.displayIfNeeded()
            let ok = WindowSnapshot.write(sheet, to: URL(fileURLWithPath: "\(shots)/26b-new-session-paths-highlighted.png"))
            r.say("      screenshot 26b-new-session-paths-highlighted.png \(ok ? "" : "FAILED")")
        }
        await key(36, "\r", window: sheet)
        r.check("…and Return takes the highlighted folder, listing what's in it, with the sheet still open",
                app.sheetOpen && model.path == "\(dir)/alpha/" && list.entries.map(\.name) == ["inner"], "\(model.path) \(list.entries.map(\.name))")
        await key(53, "\u{1b}", window: sheet)
        r.check("…Esc closes the list and leaves the sheet open", list.isHidden && app.sheetOpen)
        await arrow(125, [])
        r.check("…Down opens it again, on its first folder", !list.isHidden && list.highlighted == 0)
        await arrow(124, [])
        r.check("…and Right takes that folder", model.path == "\(dir)/alpha/inner/", model.path)
        form.pathField.stringValue = "\(dir)/"
        form.pathField.currentEditor()?.selectedRange = NSRange(location: (form.pathField.stringValue as NSString).length, length: 0)
        await type("b", window: sheet)
        await sleep(100)
        sheet.contentView?.layoutSubtreeIfNeeded()
        if !list.isHidden {
            let rowCentre = list.convert(NSPoint(x: list.bounds.midX, y: PathSuggestionList.inset + PathSuggestionList.rowHeight / 2), to: nil)
            await click(rowCentre, window: sheet)
        }
        r.check("…a click on a folder takes it", model.path == "\(dir)/beta/", model.path)
        await type("zz", window: sheet)
        r.check("…and the list closes when nothing matches", list.isHidden)
        form.pathField.stringValue = "\(dir)/a"
        form.pathField.currentEditor()?.selectedRange = NSRange(location: (form.pathField.stringValue as NSString).length, length: 0)
        await type("l", window: sheet)
        await tab()
        r.check("…Tab closes it and goes on to the next field", list.isHidden && form.focusedField == "worktree", form.focusedField ?? "-")
        await key(53, "\u{1b}", window: sheet)
        await waitUntil(2) { !app.sheetOpen }
        r.check("…and Esc outside the list cancels the sheet", !app.sheetOpen)
        await waitUntil(1) { self.firstResponderIsActiveTerminal }
    }

    // MARK: Pane navigation

    func panes(_ f: Fixture) async {
        await showRow(f.web)
        let order = tabs(rowID(f.web) ?? "").filter { ws.layout.contains($0) }
        // Shift+Right / Shift+Left walk the panes in tab order, wrapping.
        ws.activate(order[0])
        var seen: [String] = [ws.activeSession ?? "-"]
        for _ in 0..<order.count {
            await arrow(124, [.shift])
            seen.append(ws.activeSession ?? "-")
        }
        r.check("panes: Shift+Right walks the panes in tab order and wraps", seen == order + [order[0]], seen.map(app.displayName).joined(separator: " → "))
        r.check("…the keyboard follows", firstResponderIsActiveTerminal && (w.firstResponder as? TerminalView)?.session == order[0])
        await arrow(123, [.shift])
        r.check("panes: Shift+Left goes back (wrapping to the last tab)", ws.activeSession == order.last)
        await arrow(124, [.shift])
        let drift = await driftedClients()
        r.check("…and the keys never reach the programs or tmux's bindings", drift.isEmpty && !pane(f.web).contains("[1;2C") && !pane(f.webTests).contains("[1;2D"),
                drift.joined(separator: ", "))
        r.check("…the row stays the same", ws.currentRow == rowID(f.web) && ws.layout.count == 3)

        // Cmd+Option+arrows move by position.
        var spatialOK = true
        var tried = 0
        for (code, dir) in [(UInt16(123), Direction.left), (124, .right), (126, .up), (125, .down)] {
            for start in ws.layout.panes {
                ws.activate(start)
                let want = SplitNode.neighbor(of: start, dir, in: { var l = SplitNode.Layout(); l.tiles = ws.tileRects; return l }())
                await arrow(code, [.command, .option])
                tried += 1
                if ws.activeSession != (want ?? start) {
                    spatialOK = false
                    r.say("      \(dir) from \(app.displayName(start)): now \(ws.activeSession.map(app.displayName) ?? "-"), want \(want.map(app.displayName) ?? "stay")")
                }
            }
        }
        r.check("panes: Cmd+Option+arrows move to the pane in that direction (\(tried) moves)", spatialOK && firstResponderIsActiveTerminal)
        ws.activate(f.webTests)
    }

    // MARK: Zoom

    func zoom(_ f: Fixture) async {
        guard let webRow = rowID(f.web) else { return }
        await showRow(f.webTests)
        let views = Dictionary(uniqueKeysWithValues: ws.layout.panes.compactMap { s in ws.pool.view(for: s).map { (s, ObjectIdentifier($0)) } })
        let order = tabs(webRow).filter { ws.layout.contains($0) }
        let tree = ws.layout.root
        func full(_ t: TileID) -> Bool { ws.tileRects[t].map { $0.size == ws.area.bounds.size } ?? false }
        func othersHidden(_ t: TileID) -> Bool {
            ws.layout.panes.filter { $0 != t }.allSatisfy { s in ws.pool.view(for: s).map { $0.isHidden && !$0.exited } ?? false }
        }
        await key(36, "\r", [.command])
        r.check("zoom: Cmd+Return shows the active pane alone, full size", ws.layout.zoomed == f.webTests && ws.tiles == [f.webTests] && full(f.webTests),
                "tiles \(ws.tiles.map(app.displayName)) rect \(ws.tileRects[f.webTests] ?? .zero) area \(ws.area.bounds)")
        r.check("…the other panes stay in the row, attached and hidden", ws.layout.count == 3 && othersHidden(f.webTests) && ws.layout.root == tree)
        r.check("…its header says it's zoomed (\(ws.headerZoomNote(f.webTests) ?? "nothing"))", ws.headerZoomNote(f.webTests) == "zoomed · ⌘↩ shows all 3")
        r.check("…with the keyboard, and no ring", firstResponderIsActiveTerminal && ws.activeSession == f.webTests && !app.focus.showsRing)
        shot("05-zoomed")
        if let item = app.menuItem(titled: "Zoom Pane") {
            let enabled = app.validateMenuItem(item)
            r.check("…View → Zoom Pane is checked", enabled && item.state == .on)
        }

        // Zoom follows the active pane: Shift+Right goes to the next tab, still full size.
        let next = order[(order.firstIndex(of: f.webTests)! + 1) % order.count]
        await arrow(124, [.shift])
        await waitUntil(2) { ws.terminal(in: next)?.isHidden == false }
        r.check("zoom: Shift+Right zooms the next tab (\(app.displayName(next))) instead", ws.layout.zoomed == next && ws.tiles == [next] && full(next) && othersHidden(next),
                "zoomed \(ws.layout.zoomed.map(app.displayName) ?? "-") tiles \(ws.tiles.map(app.displayName))")
        r.check("…the keyboard follows", firstResponderIsActiveTerminal && (w.firstResponder as? TerminalView)?.session == next)
        if next == f.webShell {
            await waitShell(f.webShell)
            await type("echo zoom-ok\n")
            r.check("…typing reaches the zoomed pane", await paneHasLine(f.webShell, "zoom-ok"))
        }
        // Cmd+Option+arrows go by the row's whole tree.
        let geometry = ws.layout.root!.layout(in: ws.area.bounds, gap: Workspace.gap)
        var spatial = true
        for (code, dir) in [(UInt16(123), Direction.left), (124, .right), (126, .up), (125, .down)] {
            let from = ws.activeTile
            guard let want = SplitNode.neighbor(of: from, dir, in: geometry) else { continue }
            await arrow(code, [.command, .option])
            if ws.layout.zoomed != want { spatial = false; r.say("      \(dir) from \(app.displayName(from)): zoomed \(ws.layout.zoomed.map(app.displayName) ?? "-"), want \(app.displayName(want))") }
        }
        r.check("zoom: Cmd+Option+arrows move the zoom to the pane in that direction of the whole row", spatial && ws.tiles.count == 1)

        // A zoomed row stays zoomed across a row switch.
        let zoomedNow = ws.layout.zoomed
        await showRow(f.api)
        r.check("zoom: another row shows unzoomed (api has one pane)", !ws.layout.isZoomed && ws.tiles == [f.api])
        await key(36, "\r", [.command])
        r.check("…Cmd+Return there does nothing (one pane)", !ws.layout.isZoomed && ws.tiles == [f.api])
        ws.selectRow(webRow)
        await waitPanesPainted()
        r.check("…and back, the row is still zoomed on the same pane", ws.layout.zoomed == zoomedNow && ws.tiles == zoomedNow.map { [$0] })

        // Unzoom: the same views come back where they were.
        await key(36, "\r", [.command])
        await waitPanesPainted()
        let same = ws.layout.panes.allSatisfy { s in ws.pool.view(for: s).map { ObjectIdentifier($0) } == views[s] }
        r.check("zoom: Cmd+Return again shows the whole row, same tree", !ws.layout.isZoomed && ws.tiles.count == 3 && ws.layout.root == tree)
        r.check("…with the same terminal views (no reattach), all shown", same && ws.layout.panes.allSatisfy { ws.terminal(in: $0)?.isHidden == false })
        r.check("…and the keyboard stays in the active pane", firstResponderIsActiveTerminal && ws.activeSession == zoomedNow)
        r.check("…no header says zoomed", ws.tiles.allSatisfy { ws.headerZoomNote($0) == nil })

        // A double click on a pane's header zooms it, and another shows the row again.
        ws.activate(f.web)
        if let rect = ws.tileRects[f.webTests] {
            let p = ws.area.convert(NSPoint(x: rect.midX, y: rect.minY + Workspace.headerHeight / 2), to: nil)
            await doubleClick(p)
            r.check("zoom: double-clicking a pane's header zooms that pane", ws.layout.zoomed == f.webTests && ws.tiles == [f.webTests],
                    "zoomed \(ws.layout.zoomed.map(app.displayName) ?? "-")")
            let p2 = ws.area.convert(NSPoint(x: ws.area.bounds.midX, y: Workspace.headerHeight / 2), to: nil)
            await doubleClick(p2)
            r.check("…and double-clicking it again shows the row", !ws.layout.isZoomed && ws.tiles.count == 3)
        }

        // A new pane ends the zoom, so you see the split.
        ws.activate(f.webShell)
        await key(36, "\r", [.command])
        let before = tabs(webRow)
        await cmd("d")
        let made = await waitUntil(4) { tabs(webRow).count == before.count + 1 && ws.layout.count == 4 }
        let new = tabs(webRow).first { !before.contains($0) }
        r.check("zoom: Cmd+D while zoomed shows the whole row with the new split", made && !ws.layout.isZoomed && ws.tiles.count == 4 && ws.activeSession == new)
        if let new { await killTab(new) }
        await waitUntil(3) { ws.layout.count == 3 }
        ws.activate(f.webTests)
    }

    func doubleClick(_ p: NSPoint) async {
        for n in [1, 2] {
            if let d = mouse(.leftMouseDown, p, clicks: n), let u = mouse(.leftMouseUp, p, clicks: n) {
                NSApp.postEvent(d, atStart: false)
                NSApp.postEvent(u, atStart: false)
            }
        }
        await sleep(150)
    }

    // MARK: New and closed panes

    func newPanes(_ f: Fixture) async {
        guard let webRow = rowID(f.web) else { return }
        await showRow(f.webTests)
        let before = tabs(webRow)
        let from = ws.activeTile
        let fromRect = ws.tileRects[from] ?? .zero
        // Cmd+D: a terminal tab of this row, split right of the active pane, active.
        await cmd("d")
        let made = await waitUntil(4) { tabs(webRow).count == before.count + 1 && ws.layout.count == 4 }
        let new = tabs(webRow).first { !before.contains($0) }
        r.check("Cmd+D adds a tab to the row in the daemon's sidebar", made && new != nil, "tabs \(tabs(webRow).map(app.displayName))")
        guard let new else { return }
        r.check("…a plain terminal in the row's directory", app.daemon.state.sessions[new]?.agent == "terminal"
                && app.daemon.state.sessions[new]?.path == app.daemon.state.sessions[from]?.path,
                "agent \(app.daemon.state.sessions[new]?.agent ?? "-"), path \(app.daemon.state.sessions[new]?.path ?? "-")")
        r.check("…named after the row (\(app.displayName(new)))", app.displayName(new).hasPrefix(app.displayName(f.web)))
        await waitUntil(3) { ws.terminal(in: new)?.hasPaintedContent == true }
        let r1 = ws.tileRects[new] ?? .zero
        r.check("…in the layout, to the right of the pane that was active, and active",
                ws.layout.contains(new) && ws.activeSession == new && r1.minX > fromRect.minX && abs(r1.minY - fromRect.minY) < 1 && r1.maxX <= fromRect.maxX + 1,
                "new \(r1) was \(fromRect)")
        r.check("…with the keyboard", firstResponderIsActiveTerminal && (w.firstResponder as? TerminalView)?.session == new)
        await waitShell(new)
        await sleep(300)
        await type("echo cmdd-ok\n")
        r.check("…typing reaches the new pane", await paneHasLine(new, "cmdd-ok"))

        // Cmd+Shift+D: below.
        let before2 = tabs(webRow)
        let r2from = ws.tileRects[new] ?? .zero
        await cmd("d", [.shift])
        await waitUntil(4) { tabs(webRow).count == before2.count + 1 && ws.layout.count == 5 }
        let below = tabs(webRow).first { !before2.contains($0) }
        let r2 = below.flatMap { ws.tileRects[$0] } ?? .zero
        r.check("Cmd+Shift+D adds a tab split below the active pane", below != nil && ws.activeSession == below
                && r2.minY > r2from.minY && abs(r2.minX - r2from.minX) < 1, "new \(r2) was \(r2from)")
        if let below { await waitUntil(3) { ws.terminal(in: below)?.hasPaintedContent == true } }
        shot("06-five-panes")

        // Cmd+W on a shell at its prompt: no question, the pane goes, the sibling takes over, the tab ends.
        if let below {
            await waitShell(below)
            let views = ws.pool.views.count
            let splitHeight = ws.tileRects[new]?.height ?? 0
            await cmd("w")
            await sleep(250)
            r.check("Cmd+W on an idle shell pane closes it without asking", !app.sheetOpen && !ws.layout.contains(below))
            r.check("…its sibling gets the space and the keyboard", ws.activeSession == new && firstResponderIsActiveTerminal
                    && (ws.tileRects[new]?.height ?? 0) > splitHeight + 20, "active \(ws.activeSession.map(app.displayName) ?? "-")")
            let gone = await waitUntil(4) { !app.isAlive(below) && !tabs(webRow).contains(below) }
            r.check("…and the tab is gone from the daemon's sidebar", gone, "tabs \(tabs(webRow).map(app.displayName))")
            r.check("…without reattaching the panes that stay", ws.pool.views.count <= views)
        }
        await waitShell(new)
        await cmd("w")
        await waitUntil(4) { !app.isAlive(new) }
        r.check("Cmd+W again closes the other new pane: back to the row's three", ws.layout.count == 3 && !tabs(webRow).contains(new) && !app.sheetOpen)

        // Cmd+W on a pane running something: it asks, and Cancel keeps it.
        ws.activate(f.webTests)
        await cmd("w")
        await waitUntil(2) { app.sheetOpen }
        r.check("Cmd+W on a pane running a program asks first", app.sheetOpen && app.focus.exception == .sheet)
        await sleep(300)
        shot("07-close-busy-pane-confirm")
        if let sheet = w.attachedSheet { await key(53, "\u{1b}", window: sheet) }
        await waitUntil(2) { !app.sheetOpen }
        await waitUntil(1) { firstResponderIsActiveTerminal }
        r.check("…Cancel keeps the pane and its tab, and returns the keyboard", ws.layout.contains(f.webTests) && app.isAlive(f.webTests)
                && firstResponderIsActiveTerminal)

        // The last pane of a row always asks (it's a kill of the row's session).
        await showRow(f.docs)
        await cmd("w")
        await waitUntil(2) { app.sheetOpen }
        r.check("Cmd+W on a row's last pane always asks", app.sheetOpen)
        if let sheet = w.attachedSheet { await key(53, "\u{1b}", window: sheet) }
        await waitUntil(2) { !app.sheetOpen }
        r.check("…and Cancel keeps it", app.isAlive(f.docs) && ws.layout.panes == [f.docs])

        // Cmd+T: the new-session sheet for a tab of this row, in its directory; the result is a split.
        await showRow(f.webTests)
        let before3 = tabs(webRow)
        let from3 = ws.activeTile
        await cmd("t")
        await waitUntil(2) { app.sheetOpen }
        let model = app.formModel
        r.check("Cmd+T opens the sheet for a new tab of the shown row", app.sheetOpen && model?.isAddTab == true && model?.mode == .addTab(containerID: webRow))
        r.check("…in the row's directory", model?.path == app.daemon.state.sessions[from3]?.path, "path \(model?.path ?? "-")")
        if let model, let sheet = w.attachedSheet {
            model.name = "web agent"
            model.agent = "terminal"
            model.args = "python3 \(fakeAgentPath) newtab"
            await sleep(150)
            await key(36, "\r", window: sheet)
        }
        await waitUntil(5) { tabs(webRow).count == before3.count + 1 && ws.layout.count == 4 }
        let viaSheet = tabs(webRow).first { !before3.contains($0) }
        if let viaSheet {
            r.check("…and the new tab opens as a split right of the active pane, active", ws.activeSession == viaSheet
                    && (ws.tileRects[viaSheet]?.minX ?? 0) > (ws.tileRects[from3]?.minX ?? 0))
            r.check("…running what the sheet asked for", await paneShows(viaSheet, "fake agent · newtab", timeout: 6))
            // Busy: Cmd+W asks, Return confirms.
            await cmd("w")
            await waitUntil(2) { app.sheetOpen }
            if let sheet = w.attachedSheet { await key(36, "\r", window: sheet) }
            await waitUntil(4) { !app.isAlive(viaSheet) }
            r.check("…and Cmd+W, confirmed, closes it", !app.isAlive(viaSheet) && ws.layout.count == 3 && !ws.layout.contains(viaSheet))
            r.check("…and its tab leaves the row (no exited tab left behind)", await waitUntil(3) { !tabs(webRow).contains(viaSheet) })
        } else {
            r.check("…and the new tab opens as a split", false, "tabs \(tabs(webRow).map(app.displayName))")
        }

        // A tab made behind the app's back (the TUI, another client) gets a pane; the active pane stays.
        let before4 = tabs(webRow)
        let active4 = ws.activeSession
        var spec = Spec(name: "behind", path: "~", agent: "terminal", args: "")
        spec.containerID = webRow
        _ = await app.daemon.request(.create(spec: spec, deferLaunch: false))
        let appeared = await waitUntil(4) { tabs(webRow).contains { !before4.contains($0) } && ws.layout.count == 4 }
        let behind = tabs(webRow).first { !before4.contains($0) }
        r.check("a tab created behind the app's back appears as a pane of its row", appeared && behind.map { ws.layout.contains($0) } == true)
        r.check("…without taking the keyboard", ws.activeSession == active4 && firstResponderIsActiveTerminal)
        if let behind {
            await waitUntil(3) { ws.terminal(in: behind)?.hasPaintedContent == true }
            r.check("…attached", ws.terminal(in: behind)?.hasPaintedContent == true)
            await sleep(200)
            shot("08-tab-from-elsewhere")
            await killTab(behind)
            let lost = await waitUntil(4) { !ws.layout.contains(behind) && ws.layout.count == 3 }
            r.check("a tab that ends elsewhere loses its pane", lost)
        }
        // A tab that dies on its own (alive: false, or removed) loses its pane too.
        await showRow(f.webTests)
    }

    // MARK: Client moves

    /// Prefix 1 (Bosun's jump to the first session, for the TUI), typed inside the app's terminals.
    /// The tmux prefix (whatever ~/.tmux.conf sets, C-b by default), then 1.
    private func typePrefixJump() async {
        let prefix = tmuxOut(["show", "-gv", "prefix"]).trimmingCharacters(in: .whitespacesAndNewlines)
        var letter: Character = "b"
        if prefix.hasPrefix("C-"), prefix.count == 3, let c = prefix.last, c.isLetter { letter = Character(c.lowercased()) }
        let ctrl = String(UnicodeScalar(UInt8(letter.asciiValue! - 96)))
        await key(TestKit.keyCodes[letter] ?? 11, ctrl, [.control], ignoring: String(letter))
        await key(18, "1")
    }

    func clientMoves(_ f: Fixture) async {
        let api = f.api, docs = f.docs
        let jumpTarget = tmuxOut(["list-keys", "-T", "prefix", "1"]).components(separatedBy: " ").last?.trimmingCharacters(in: .whitespacesAndNewlines)
        guard jumpTarget == api else {
            r.check("prefix 1 jumps to api in the fixture", false, tmuxOut(["list-keys", "-T", "prefix", "1"]))
            return
        }
        // a) Toward a session in another row: that row is shown, and the client goes back.
        await showRow(docs)
        let docsView = ws.activeTerminal
        await typePrefixJump()
        await waitUntil(3) { ws.currentRow == rowID(api) }
        r.check("prefix 1 inside a pane: the app shows the target's row", ws.currentRow == rowID(api) && ws.activeSession == api,
                "shows \(ws.activeSession.map(app.displayName) ?? "-")")
        let back = await waitUntil(3) { tmuxOut(["list-clients", "-F", "#{client_tty} #{session_name}"]).contains("\(docsView?.ttyName ?? "?") \(docs)") }
        r.check("…the moved client goes back to its own tab", back && ws.pool.view(for: docs) === docsView)
        let selected = (app.sidebar.outline.item(atRow: app.sidebar.outline.selectedRow) as? SidebarItem).flatMap { app.sidebar.rowID(for: $0) }
        r.check("…header, title and sidebar follow", ws.headerSession(ws.activeTile) == api && w.title.contains(app.displayName(api)) && selected == rowID(api),
                "title \(w.title)")
        await waitUntil(2) { ws.activeTerminal?.hasPaintedContent == true }
        await waitUntil(1) { firstResponderIsActiveTerminal }
        await type("followed\n")
        r.check("…and typing reaches the row now shown", await paneShows(api, "got: followed"))
        shot("14-client-moved-to-another-row")

        // b) Toward a tab of the same row: that pane becomes active, the client goes back.
        await showRow(f.web)
        if let tty = ws.pool.view(for: f.web)?.ttyName {
            tmux(["switch-client", "-c", tty, "-t", "=\(f.webTests)"])
            await waitUntil(3) { ws.activeSession == f.webTests }
            let backHome = await waitUntil(3) { tmuxOut(["list-clients", "-F", "#{client_tty} #{session_name}"]).contains("\(tty) \(f.web)") }
            r.check("a client moved to a tab of the same row: that pane becomes active", ws.activeSession == f.webTests && ws.currentRow == rowID(f.web) && ws.layout.count == 3)
            r.check("…and the client goes back to its own tab", backHome)
        }
        // c) prefix 1 inside a pane of a multi-pane row: api's row, and every web client stays home.
        ws.activate(f.web)
        await waitUntil(1) { firstResponderIsActiveTerminal }
        await typePrefixJump()
        await waitUntil(3) { ws.currentRow == rowID(api) }
        let drift = await driftedClients()
        r.check("prefix 1 inside a multi-pane row shows the target's row, all clients on their own tabs", ws.currentRow == rowID(api) && drift.isEmpty, drift.joined(separator: ", "))

        // d) A hidden pooled view's client is moved back at once.
        await showRow(f.review1)
        await showRow(api)
        if let hidden = ws.pool.view(for: f.review1), hidden.isHidden, let tty = hidden.ttyName {
            tmux(["switch-client", "-c", tty, "-t", "=\(docs)"])
            let ok = await waitUntil(3) { tmuxOut(["list-clients", "-F", "#{client_tty} #{session_name}"]).contains("\(tty) \(f.review1)") }
            r.check("a hidden pooled terminal's client moved elsewhere is put back", ok && ws.pool.view(for: f.review1) === hidden && ws.currentRow == rowID(api))
        } else {
            r.check("a hidden pooled terminal's client moved elsewhere is put back", false, "review-1 isn't pooled and hidden")
        }

        // e) Ctrl+q (Bosun's detach binding) inside a pane: it attaches again.
        let before = ws.activeTerminal
        await key(12, "\u{11}", [.control], ignoring: "q")
        let reattached = await waitUntil(5) {
            guard let t = ws.activeTerminal, t !== before, !t.exited, t.hasPaintedContent else { return false }
            return true
        }
        let onAfter = await clientSession(of: api)
        r.check("Ctrl+q (tmux detach) inside a pane: it attaches again", reattached && ws.activeSession == api && onAfter == api,
                "client on \(onAfter.map(app.displayName) ?? "-")")
        await waitUntil(1) { firstResponderIsActiveTerminal }
        await type("after-detach\n")
        r.check("…and typing reaches it", await paneShows(api, "got: after-detach"))
    }

    var fakeAgentPath: String {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("fixture/fake_agent.py").path
    }
}

// MARK: - Self-test (budgets, design section 1)

@MainActor
final class SelfTest: TestKit {
    init(app: AppController) { super.init(app: app, report: "selftest.txt") }

    /// Phase 3's exit test: dragging a divider follows the pointer at display rate with four busy agents
    /// on screen. Each runs the fixture's ink mode (a status block 30 times a second, and the whole
    /// screen again on every SIGWINCH, as Claude Code does). The drag goes through AppKit's event path
    /// (posted mouse events on the divider) once per grid-size throttle, to show what the throttle buys.
    func busyResize() async {
        guard let api = session("api"), let spec = (await app.daemon.request(.getSpec(session: api))).spec,
              let agent = spec.args.split(separator: " ").first(where: { $0.hasSuffix("fake_agent.py") }) else {
            r.say("busy resize: SKIPPED (no fixture agent path in api's spec)")
            return
        }
        r.say("")
        r.say("busy resize: four ink-style agents in one row (30 fps status block, full repaint on SIGWINCH)")
        var made: [String] = []
        var row: String?
        for i in 1...4 {
            let sp = Spec(name: "busy", path: "~", agent: "terminal", args: "python3 \(agent) busy-\(i) --ink", containerID: row)
            let ack = await app.daemon.request(.create(spec: sp, deferLaunch: false))
            guard ack.ok, let s = ack.session else { r.say("busy resize: couldn't create a busy session: \(ack.error ?? "?")"); break }
            made.append(s)
            if row == nil {
                await waitUntil(4) { rowID(s) != nil }
                row = rowID(s)
            }
        }
        // Kill them and take their row out, so the fixture is as it was.
        func cleanUp() async {
            for s in made { _ = await app.daemon.request(.kill(session: s)) }
            await waitUntil(4) { made.allSatisfy { !app.isAlive($0) } }
            if let row { app.removeRow(row) }
            await waitUntil(3) { row.map { app.daemon.state.sidebar.container(id: $0) == nil } ?? true }
        }
        guard let row, made.count == 4, await waitUntil(6, { tabs(row).count == 4 && made.allSatisfy(app.isAlive) }) else {
            r.say("busy resize: SKIPPED (the busy row didn't come up: \(made.count) sessions)")
            await cleanUp()
            return
        }
        ws.selectRow(row)
        await waitUntil(6) { ws.layout.count == 4 }
        await waitPanesPainted(6)
        await sleep(1500)
        guard ws.layout.count == 4, let d = ws.area.subviews.compactMap({ $0 as? DividerView }).compactMap(\.divider).first(where: { $0.path.isEmpty }) else {
            r.say("busy resize: SKIPPED (no four-pane layout)")
            await cleanUp()
            return
        }
        // CPU seconds used so far by the four agents and by the tmux server, from ps.
        // The pane's process is the shell the launch command was typed into; the agent is its child.
        let agentPIDs = made.flatMap { s -> [String] in
            let pid = tmuxOut(["display-message", "-p", "-t", "=\(s):", "#{pane_pid}"]).trimmingCharacters(in: .whitespacesAndNewlines)
            let kids = (ShellEnv.run("/usr/bin/pgrep", ["-P", pid], timeout: 2) ?? "").split(separator: "\n").map(String.init)
            return [pid] + kids
        }
        let serverPID = tmuxOut(["display-message", "-p", "#{pid}"]).trimmingCharacters(in: .whitespacesAndNewlines)
        func cpu(_ pids: [String]) -> Double {
            let out = ShellEnv.run("/bin/ps", ["-o", "time=", "-p", pids.joined(separator: ",")], timeout: 2) ?? ""
            return out.split(separator: "\n").reduce(0) { sum, line in
                // [[hh:]mm:]ss.ss
                sum + line.trimmingCharacters(in: .whitespaces).split(separator: ":").reduce(0) { $0 * 60 + (Double($1) ?? 0) }
            }
        }
        let hz = Double(max(60, w.screen?.maximumFramesPerSecond ?? 120))
        let frame = 1 / hz
        let views = made.compactMap { ws.pool.view(for: $0) }
        let saved = Workspace.resizeThrottle
        defer { Workspace.resizeThrottle = saved }
        func point(_ p: Double) -> NSPoint {
            let c = d.container
            let q = d.axis == .horizontal
                ? NSPoint(x: c.minX + c.width * (0.5 - 0.25 * sin(p * 2 * .pi)), y: c.midY)
                : NSPoint(x: c.midX, y: c.minY + c.height * (0.5 - 0.25 * sin(p * 2 * .pi)))
            return ws.area.convert(q, to: nil)
        }
        r.say(String(format: "  dragging the row's main divider through AppKit for 2 s at %.0f Hz, once per throttle:", hz))
        var verdict = ""
        for throttle in [0, 0.05, 0.1, 0.15] {
            Workspace.resizeThrottle = throttle
            let sends0 = views.map(\.sizeSends), frames0 = views.map(\.contentsCount)
            let agentCPU0 = cpu(agentPIDs), tmuxCPU0 = cpu([serverPID])
            var handled: [CFTimeInterval] = [], layout: [Double] = []
            ws.onDividerStep = { cost in
                handled.append(CACurrentMediaTime())
                layout.append(cost)
            }
            let n = Int(2 * hz)
            let start = point(0)
            if let e = mouse(.leftMouseDown, start) { NSApp.postEvent(e, atStart: false) }
            await sleep(20)
            let t0 = CACurrentMediaTime()
            var posted: [CFTimeInterval] = []
            for i in 1...n {
                if let e = mouse(.leftMouseDragged, point(Double(i) / Double(n))) {
                    posted.append(CACurrentMediaTime())
                    NSApp.postEvent(e, atStart: false)
                }
                // Pace to the display: sleep to the next step's time, not a fixed amount.
                let next = t0 + Double(i) * frame
                let left = next - CACurrentMediaTime()
                if left > 0 { try? await Task.sleep(nanoseconds: UInt64(left * 1e9)) } else { await Task.yield() }
            }
            await waitUntil(0.5) { handled.count >= posted.count }
            let dur = CACurrentMediaTime() - t0
            if let e = mouse(.leftMouseUp, point(1)) { NSApp.postEvent(e, atStart: false) }
            // The final resize's repaint, then what the agents and tmux spent on the drag.
            await sleep(300)
            let agentCPU = (cpu(agentPIDs) - agentCPU0) / dur, tmuxCPU = (cpu([serverPID]) - tmuxCPU0) / dur
            await sleep(60)
            ws.onDividerStep = nil
            let lat = zip(posted, handled).map { $1 - $0 }
            let gaps = zip(handled.dropFirst(), handled).map { $0 - $1 }
            let sends = zip(views.map(\.sizeSends), sends0).map { $0 - $1 }
            let fps = zip(views.map(\.contentsCount), frames0).map { Double($0 - $1) / dur }
            let ok = handled.count >= posted.count && Stats.percentile(gaps, 99) <= frame * 1.5 && Stats.percentile(layout, 99) <= frame
            r.say(String(format: "  throttle %3.0f ms: steps %d/%d  pointer→layout p50=%@ p99=%@  step gap p99=%@  layout p99=%@  SIGWINCH per pane %.0f  frames per pane %.0f/s  CPU agents %.0f%% tmux %.0f%%  %@",
                         throttle * 1000, handled.count, posted.count, fmtMs(Stats.percentile(lat, 50)), fmtMs(Stats.percentile(lat, 99)),
                         fmtMs(Stats.percentile(gaps, 99)), fmtMs(Stats.percentile(layout, 99)),
                         Double(sends.reduce(0, +)) / Double(max(1, sends.count)), fps.reduce(0, +) / Double(max(1, fps.count)),
                         agentCPU * 100, tmuxCPU * 100, ok ? "at display rate" : "BEHIND"))
            if throttle == saved { verdict = ok ? "met" : "MISSED" }
            await sleep(900)
        }
        r.say("  divider drag at display rate with 4 busy agents (throttle \(Int(saved * 1000)) ms): \(verdict)")

        // A window resize (what a live resize does to the panes), throttled the same way.
        Workspace.resizeThrottle = saved
        let original = w.frame
        let sends0 = views.map(\.sizeSends)
        var steps: [Double] = []
        ws.resizeBegan()
        let n = Int(hz)
        for i in 0...n {
            var f = original
            f.size.width = original.width - (original.width * 0.25) * sin(Double(i) / Double(n) * .pi)
            let t0 = CACurrentMediaTime()
            w.setFrame(f, display: true)
            steps.append(CACurrentMediaTime() - t0)
            try? await Task.sleep(nanoseconds: UInt64(frame * 1e9))
        }
        ws.resizeEnded()
        w.setFrame(original, display: true)
        let sends = zip(views.map(\.sizeSends), sends0).map { $0 - $1 }
        stats(String(format: "window resize, 4 busy panes (1 s, resizes per pane %.0f): setFrame + layout per step", Double(sends.reduce(0, +)) / 4), steps, budget: frame)
        await sleep(500)
        await cleanUp()
    }

    func stats(_ name: String, _ xs: [Double], budget: Double? = nil) {
        let p50 = Stats.percentile(xs, 50), p99 = Stats.percentile(xs, 99)
        var line = String(format: "%@: n=%d  p50=%@  p90=%@  p99=%@  max=%@", name, xs.count, fmtMs(p50), fmtMs(Stats.percentile(xs, 90)),
                          fmtMs(p99), fmtMs(xs.max() ?? .nan))
        if let budget { line += "   budget \(fmtMs(budget)): \(p99 <= budget ? "met at p99" : p50 <= budget ? "met at p50 only" : "MISSED")" }
        r.say(line)
    }

    func run() async {
        app.enableTestKey()
        r.say("# Bosun self-test  \(Date())")
        r.say("display: \(w.screen?.maximumFramesPerSecond ?? 0) Hz max, backing scale \(w.backingScaleFactor), screen \(NSScreen.screens.isEmpty ? "none" : "present")")
        guard await waitUntil(10, { app.daemon.status == .connected && app.daemon.state.sessionOrder.count >= 6 }) else {
            r.say("ERROR: need the dev.sh fixture (6+ sessions)")
            r.save()
            return
        }
        r.say("launch (this process): sidebar painted \(fmtMs(app.sidebarPaintedAt ?? .nan)), first live terminal \(fmtMs(app.firstTerminalAt ?? .nan))")
        r.say("warm pool: \(ws.pool.capacity) views")
        // Single-pane rows, by their one session.
        let singles = app.daemon.state.sidebar.allRows.filter { c in c.tabs.count == 1 && app.isAlive(c.active) }.map(\.active)
        let ticker = session("ticker")
        let all = singles.filter { $0 != ticker } + (ticker.map { [$0] } ?? [])
        var samples: [(String, Bool, Double, Double?)] = []
        ws.onShowMeasured = { samples.append(($0, $1, $2, $3)) }

        // --only busy runs just the busy-agent resize measurement.
        if argValue("--only") == "busy" {
            await busyResize()
            r.save()
            return
        }

        // --only idle skips straight to the idle measurement.
        if argValue("--only") != "idle" {
            // Warm switches between single-pane rows: 4 attached sessions, 100 swaps.
            await showRow(all[0])
            let warmSet = Array(all.prefix(4))
            for s in warmSet { ws.show(s); await waitUntil(3) { ws.activeTerminal?.hasPaintedContent == true }; await sleep(100) }
            samples.removeAll()
            for i in 0..<100 {
                ws.show(warmSet[(i + 1) % 4])
                await waitUntil(1) { samples.count > i }
                await sleep(30)
            }
            let warm = samples.filter(\.1).map(\.2)
            r.say("")
            stats("warm switch, one-pane rows -> first frame", warm, budget: 0.016)

            // Warm row switches with three panes: the web row and a one-pane row, back and forth.
            if let web = session("web"), let webRow = rowID(web) {
                var swap: [Double] = [], frames: [Double] = []
                var shown: (String, Bool, Double)?
                ws.onRowShown = { shown = ($0, $1, $2) }
                await showRow(web)
                await showRow(all[0])
                await showRow(web)
                for i in 0..<60 {
                    let toWeb = i % 2 == 1
                    samples.removeAll()
                    shown = nil
                    if toWeb { ws.selectRow(webRow) } else { ws.show(all[0]) }
                    let n = toWeb ? 3 : 1
                    await waitUntil(1) { samples.filter(\.1).count >= n }
                    if toWeb, let s = shown, s.1 {
                        swap.append(s.2)
                        let warmSamples = samples.filter(\.1)
                        if warmSamples.count >= 3 { frames.append(warmSamples.map(\.2).max()!) }
                    }
                    await sleep(40)
                }
                ws.onRowShown = nil
                stats("warm row switch, 3 panes: main-thread swap", swap, budget: 0.008)
                stats("warm row switch, 3 panes -> every pane's first frame", frames, budget: 0.016)
            }

            // Cold attaches.
            samples.removeAll()
            for i in 0..<20 {
                let s = all[4 + (i % max(1, all.count - 4))]
                let other = all[i % 4]
                ws.show(other)
                await sleep(50)
                ws.pool.drop(s)
                await sleep(250)
                let n = samples.count
                ws.show(s)
                await waitUntil(3) { samples.count > n && samples.last?.1 == false }
                await sleep(100)
            }
            let cold = samples.filter { !$0.1 }
            stats("cold attach -> first frame", cold.map(\.2))
            stats("cold attach -> tmux painted (attach -> first frame with content)", cold.compactMap(\.3), budget: 0.080)

            // Status change -> badge, through the daemon's 250 ms poll.
            var badge: [Double] = []
            let target = all.last { !ws.visibleSessions.contains($0) } ?? all[all.count - 1]
            var applied: (String, AgentState, CFTimeInterval)?
            app.sidebar.onBadgeApplied = { s, st in if s == target { applied = (s, st, CACurrentMediaTime()) } }
            for i in 0..<20 {
                let state = i % 2 == 0 ? "waiting" : "idle"
                let want: AgentState = i % 2 == 0 ? .waiting : .idle
                applied = nil
                let t0 = CACurrentMediaTime()
                let sock = tmuxSocket
                let ms = "\(Int(Date().timeIntervalSince1970 * 1000))"
                DispatchQueue.global().async {
                    TmuxCLI.run(socket: sock, ["set-option", "-p", "-t", "=\(target):", "@bosun_state", state, ";",
                                                "set-option", "-p", "-t", "=\(target):", "@bosun_state_at", ms])
                }
                if await waitUntil(3, { applied?.1 == want }), let a = applied { badge.append(a.2 - t0) }
                await sleep(120)
            }
            stats("status change (tmux option set) -> sidebar badge", badge, budget: 0.250)
            app.sidebar.onBadgeApplied = nil

            // Daemon round trip.
            var rtt: [Double] = []
            for _ in 0..<50 {
                let t0 = CACurrentMediaTime()
                _ = await app.daemon.request(.getSpec(session: all[0]))
                rtt.append(CACurrentMediaTime() - t0)
            }
            stats("daemon round trip (get_spec -> ack)", rtt)

            // Divider drag in the three-pane row: layout cost per step at 120 Hz.
            var steps: [Double] = []
            if let web = session("web") {
                await showRow(web)
                if let d = ws.area.subviews.compactMap({ $0 as? DividerView }).compactMap(\.divider).first(where: { $0.path.isEmpty }) {
                    ws.dividerDragBegan()
                    for i in 0...120 {
                        let p = Double(i) / 120
                        let t0 = CACurrentMediaTime()
                        if d.axis == .horizontal {
                            ws.dividerDragged(d, to: NSPoint(x: d.container.minX + d.container.width * (0.5 - 0.25 * sin(p * .pi)), y: d.container.midY))
                        } else {
                            ws.dividerDragged(d, to: NSPoint(x: d.container.midX, y: d.container.minY + d.container.height * (0.5 - 0.25 * sin(p * .pi))))
                        }
                        ws.area.layoutSubtreeIfNeeded()
                        steps.append(CACurrentMediaTime() - t0)
                        await sleep(1000.0 / 120)
                    }
                    ws.dividerDragEnded()
                }
            }
            stats("divider drag: layout per step", steps, budget: 0.0083)

            await busyResize()

            await rapidSwitching(all)
        }

        // Idle CPU with four panes of quiet sessions (the web row plus a terminal tab), then with the
        // ticker's row (a line a second) shown and the four hidden.
        var extra: String?
        if let web = session("web"), let webRow = rowID(web) {
            await showRow(web)
            let before = tabs(webRow)
            app.newTerminalTab(.horizontal)
            await waitUntil(4) { ws.layout.count == 4 }
            extra = tabs(webRow).first { !before.contains($0) }
            await waitPanesPainted()
        }
        func idle(_ label: String) async {
            await sleep(2000)
            let frames0 = Dictionary(uniqueKeysWithValues: ws.pool.views.map { ($0.key, $0.value.contentsCount) })
            let c0 = cpuSeconds(), t0 = Date()
            await sleep(20_000)
            let pct = (cpuSeconds() - c0) / Date().timeIntervalSince(t0) * 100
            r.say(String(format: "idle CPU, %d panes%@, %d views attached: %.2f%% of one core over 20 s   budget 0.5%%: %@",
                         ws.layout.count, label, ws.pool.views.count, pct, pct <= 0.5 ? "met" : "MISSED"))
            // Which panes drew during the window (a shell prompt with a clock, say, redraws on its own).
            let drew = ws.layout.panes.map { p in
                "\(app.displayName(p)) \((ws.pool.view(for: p)?.contentsCount ?? 0) - (frames0[p] ?? 0))\(ws.pool.view(for: p)?.hasTerminalFocus == true ? " (focused)" : "")"
            }
            let hiddenFrames = ws.pool.views.filter { !ws.layout.contains($0.key) }.reduce(0) { $0 + $1.value.contentsCount - (frames0[$1.key] ?? 0) }
            r.say("  frames drawn in that window: \(drew.joined(separator: ", ")); hidden views: \(hiddenFrames)")
            // Only the active pane of a key window blinks (a frame every 600 ms, about 33 in 20 s);
            // the others draw only for tmux's own redraws (Bosun's daemon re-asserts its bindings every 30 s).
            let active = ws.activeTerminal
            let blinking = ws.layout.panes.compactMap { ws.pool.view(for: $0) }.filter { v in
                v !== active && v.contentsCount - (frames0[v.session] ?? 0) > 10
            }
            r.check("…no pane but the active one blinks", blinking.isEmpty, blinking.map { app.displayName($0.session) }.joined(separator: ", "))
            if app.windowIsKey, ws.windowKey, let active {
                let n = active.contentsCount - (frames0[active.session] ?? 0)
                r.check("…and the active pane does (a frame per blink)", n >= 25, "\(n) frames")
            }
            checkFocusCount()
        }
        await idle(" (quiet sessions)")
        // The same with the window not key (another app in front): the active pane stops blinking too.
        ws.windowKeyChanged(false)
        await idle(" (quiet sessions, window not key)")
        ws.windowKeyChanged(true)
        r.say(String(format: "memory footprint: %.0f MB with %d terminal views", physFootprintMB(), ws.pool.views.count))
        if let ticker {
            await showRow(ticker)
            let c0 = cpuSeconds(), t0 = Date()
            await sleep(10_000)
            let pct = (cpuSeconds() - c0) / Date().timeIntervalSince(t0) * 100
            r.say(String(format: "CPU with the ticker's row shown (a line a second) and %d views attached: %.2f%% of one core over 10 s (no budget; for comparison)",
                         ws.pool.views.count, pct))
        }
        if let extra { await killTab(extra) }
        if argValue("--only") == "idle" { r.save(); return }

        // Launch: separate processes, each reporting sidebar painted and first live terminal.
        r.say("")
        var sidebarT: [Double] = [], terminalT: [Double] = []
        let exe = Bundle.main.executablePath!
        for _ in 0..<10 {
            let out = await Task.detached { () -> String in
                let p = Process()
                p.executableURL = URL(fileURLWithPath: exe)
                p.arguments = ["--launchprobe", "-ApplePersistenceIgnoreState", "YES"] + (noActivate ? ["--no-activate"] : [])
                let pipe = Pipe()
                p.standardOutput = pipe
                p.standardError = FileHandle.nullDevice
                try? p.run()
                let d = pipe.fileHandleForReading.readDataToEndOfFile()
                p.waitUntilExit()
                return String(decoding: d, as: UTF8.self)
            }.value
            for line in out.split(separator: "\n") where line.hasPrefix("launchprobe ") {
                let f = line.split(separator: " ")
                if f.count >= 3, let a = Double(f[1]), let b = Double(f[2]) { sidebarT.append(a); terminalT.append(b) }
            }
        }
        stats("launch -> sidebar painted (cached snapshot)", sidebarT, budget: 0.300)
        stats("launch -> first live terminal", terminalT, budget: 0.500)
        r.say("")
        checkFocusCount()
        r.save()
        r.say("saved \(r.path)")
    }
}

extension SelfTest {
    /// Panes and rows switched faster than libghostty's renderer settles focus: afterwards the
    /// active pane is the only one libghostty thinks is focused, it blinks, and nothing else does.
    func rapidSwitching(_ all: [String]) async {
        guard let web = session("web"), let webRow = rowID(web) else { return }
        await showRow(web)
        let panes = ws.layout.panes
        let repairs0 = ws.pool.views.values.reduce(0) { $0 + $1.blinkRepairs }
        for i in 0..<80 {
            switch i % 6 {
            case 4: ws.show(all[(i / 6) % min(4, all.count)])
            case 5: ws.selectRow(webRow)
            default: ws.activate(panes[(i * 7) % panes.count])
            }
            await sleep([0, 5, 20, 45, 70, 110][i % 6])
        }
        ws.selectRow(webRow)
        ws.activate(panes[1 % panes.count])
        await sleep(2500)
        let f0 = Dictionary(uniqueKeysWithValues: ws.pool.views.map { ($0.key, $0.value.contentsCount) })
        await sleep(4200)
        let active = ws.activeTerminal
        var bad: [String] = []
        for v in panes.compactMap({ ws.pool.view(for: $0) }) {
            let n = v.contentsCount - (f0[v.session] ?? 0)
            if v === active, n < 6 { bad.append("\(app.displayName(v.session)) is active and drew \(n)") }
            if v !== active, n > 5 { bad.append("\(app.displayName(v.session)) isn't active and drew \(n)") }
        }
        let repairs = ws.pool.views.values.reduce(0) { $0 + $1.blinkRepairs } - repairs0
        r.say("")
        r.check("rapid pane and row switching (80 in about 4 s): only the active pane blinks", bad.isEmpty, bad.joined(separator: "; "))
        r.say("  blink guard repairs during it: \(repairs)")
        checkFocusCount()

        // The guards themselves: libghostty put in each broken state on purpose, then left alone.
        if let active, let other = panes.compactMap({ ws.pool.view(for: $0) }).first(where: { $0 !== active }) {
            let a0 = active.blinkRepairs, o0 = other.blinkRepairs
            other.breakBlinkForTest(blinkingUnfocused: true)
            active.breakBlinkForTest(blinkingUnfocused: false)
            await sleep(4000)
            let fa = active.contentsCount, fo = other.contentsCount
            await sleep(3000)
            r.check("blink guard: a pane blinking without focus is stopped", other.blinkRepairs > o0 && other.contentsCount - fo <= 2,
                    "\(other.blinkRepairs - o0) repairs, \(other.contentsCount - fo) frames in 3 s")
            r.check("blink guard: a focused pane that stopped blinking blinks again",
                    active.contentsCount - fa >= 4, "\(active.blinkRepairs - a0) repairs, \(active.contentsCount - fa) frames in 3 s")
            checkFocusCount()
        }
    }
}

extension TestKit {
    /// libghostty is told a terminal has focus only for the active pane of a key window.
    func checkFocusCount() {
        let focused = ws.pool.views.values.filter(\.hasTerminalFocus)
        let names = focused.map { app.displayName($0.session) }.joined(separator: ", ")
        r.check("terminals libghostty thinks are focused: \(focused.count) (at most 1, in any window)", focused.count <= 1
                && focused.allSatisfy { $0 === ws.activeTerminal }, names)
    }
}

/// Started by the self-test: report launch timings, then quit.
@MainActor
final class LaunchProbe {
    let app: AppController
    init(app: AppController) { self.app = app }

    func run() {
        app.enableTestKey()
        app.onFirstTerminal = { [app] in
            print("launchprobe \(app.sidebarPaintedAt ?? -1) \(app.firstTerminalAt ?? -1)")
            fflush(stdout)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { NSApp.terminate(nil) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) { [app] in
            let ws: Workspace = app.workspace
            let views = ws.pool.views.map { s, v in
                "\(s) contents=\(v.contentsCount) painted=\(v.hasPaintedContent) hidden=\(v.isHidden) text=\(v.viewportText().prefix(60).debugDescription)"
            }
            print("launchprobe timeout status=\(app.daemon.status) tiles=\(ws.layout.count) visible=\(ws.visibleSessions) views=\(views)")
            fflush(stdout)
            NSApp.terminate(nil)
        }
    }
}

func cpuSeconds() -> Double {
    var ru = rusage()
    getrusage(RUSAGE_SELF, &ru)
    return Double(ru.ru_utime.tv_sec) + Double(ru.ru_utime.tv_usec) / 1e6 + Double(ru.ru_stime.tv_sec) + Double(ru.ru_stime.tv_usec) / 1e6
}

func physFootprintMB() -> Double {
    var info = task_vm_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<integer_t>.size)
    let kr = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count) }
    }
    return kr == KERN_SUCCESS ? Double(info.phys_footprint) / 1_048_576 : -1
}
