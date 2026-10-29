import AppKit
import BosunCore

/// One Bosun window: a sidebar and a main area showing one row, with its own focus model (design 7b
/// applies per window), palette and shortcuts panel. Every window is a full Bosun window that can show
/// any row, but a row is on screen in one window at most. The app keeps the windows in `windows`;
/// menus and keys act on the key one (`AppController.current`).
@MainActor
final class BosunWindow {
    unowned let app: AppController
    let window: MainWindow
    let root: RootView
    let sidebar: SidebarController
    let workspace: Workspace
    var focus = FocusModel() {
        didSet {
            if oldValue.exception == .find, focus.exception != .find, findSession != nil { findLostKeyboard() }
        }
    }
    /// The session whose pane has the find bar open (`PaneFind.swift`).
    var findSession: String?
    /// The last search typed in this window, offered again by the next Cmd+F.
    var lastFindText = ""
    /// The find bar's Aa toggle: match case. Off by default; kept for the window, not saved.
    var findMatchCase = false
    /// Counts search steps, so only the latest step's answer reaches the bar.
    var findSeq = 0
    var palette: PaletteView?
    var shortcutsPanel: ShortcutsPanel?
    var sendNextPending = false
    /// The open new-session / modify form's model (the focus test switches agents through it).
    var formModel: NewSessionModel?

    /// The frame name the first window's frame is kept under (the one window of earlier builds).
    static let primaryFrameName = "BosunMain"

    init(app: AppController, frame: NSRect?) {
        self.app = app
        workspace = Workspace(app: app)
        sidebar = SidebarController(app: app)
        let w = MainWindow(contentRect: NSRect(x: 0, y: 0, width: 1320, height: 840),
                           styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                           backing: .buffered, defer: false)
        w.title = "Bosun"
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.acceptsMouseMovedEvents = true
        w.tabbingMode = .disallowed
        w.minSize = NSSize(width: 640, height: 400)
        if let frame {
            w.setFrame(frame, display: false)
        } else if !w.setFrameUsingName(BosunWindow.primaryFrameName) {
            w.center()
        }
        root = RootView(sidebar: sidebar.view, main: workspace.area, sidebarWidth: AppSettings.shared.sidebarWidth)
        window = w
        workspace.owner = self
        sidebar.owner = self
        focus.followsMouse = AppSettings.shared.focusFollowsMouse
        w.delegate = app
        w.responderPolicy = { [weak self] r in self?.firstResponderPolicy(r) ?? r }
        root.onSidebarResized = { AppSettings.shared.sidebarWidth = $0 }
        root.onSidebarDrag = { [weak self] began in
            if began { self?.workspace.resizeBegan() } else { self?.workspace.resizeEnded() }
        }
        w.contentView = root
        root.titlebarHeight = max(28, w.frame.height - w.contentLayoutRect.height)
    }

    /// Who may hold the keyboard: the active terminal, or a text field while an exception is open.
    private func firstResponderPolicy(_ r: NSResponder?) -> NSResponder? {
        let terminal = workspace.activeTerminal
        if let t = r as? TerminalView {
            return t === terminal ? t : (terminal ?? window)
        }
        if r is NSText || r is NSTextField {
            return focus.exception != nil ? r : (terminal ?? window)
        }
        if r == nil || r === window {
            if focus.exception == nil, let terminal { return terminal }
            return r
        }
        // The outline, buttons, scroll views...: never.
        return terminal ?? window
    }

    /// The window is in front of you: the app is active and the window is on screen, on this Space and not covered.
    var onScreen: Bool {
        NSApp.isActive && window.isVisible && !window.isMiniaturized && window.isOnActiveSpace && window.occlusionState.contains(.visible)
    }

    /// Its title band and window title name the active pane's session.
    func updateTitle() {
        let theme = app.theme
        if let s = workspace.activeSession {
            var t = app.displayName(s)
            if let p = app.daemon.state.sessions[s]?.path { t += "  —  " + (p as NSString).abbreviatingWithTildeInPath }
            root.band.set(title: t, theme: theme)
            window.title = "Bosun — \(app.displayName(s))"
        } else {
            root.band.set(title: "Bosun", theme: theme)
            window.title = "Bosun"
        }
    }

    /// The interface font changed: everything is drawn again with it, and the sidebar's rows and the panes'
    /// headers take its height.
    func applyFonts() {
        applyTheme()
        sidebar.reload()
        sidebar.view.needsLayout = true
        workspace.applyFonts()
    }

    func applyTheme() {
        let theme = app.theme
        window.appearance = theme.appearance
        window.backgroundColor = theme.bg
        sidebar.applyTheme()
        workspace.updateChromeState()
        palette?.theme = theme
        palette?.applyTheme()
        updateTitle()
    }
}
