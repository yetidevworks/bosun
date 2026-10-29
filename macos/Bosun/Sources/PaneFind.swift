import AppKit
import BosunCore

// Find in the pane (Cmd+F): a bar at the top right of the active pane that searches its session's whole
// history. The history is tmux's (Ghostty keeps no scrollback here, see `scrollback-limit-bytes = 0`), so
// the search runs in tmux's copy mode through the daemon's `search`: tmux scrolls the pane to each match
// and highlights the others. The sidebar's filter is Option+Cmd+F.

/// The find bar's text field. Its own class so the focus model's responder policy and the tests can tell it apart.
final class FindField: NSTextField {}

/// The bar itself. While it's open its field has the keyboard (the `find` exception); typing searches as
/// you go, Return and Shift+Return step to older and newer matches, Esc closes. Case is ignored unless
/// the Aa toggle is on.
final class PaneFindBar: NSView, NSTextFieldDelegate {
    static let height: CGFloat = 30
    static let width: CGFloat = 356

    let field = FindField()
    private let icon = NSImageView()
    let count = NSTextField(labelWithString: "")
    let matchCase = NSButton()
    let older = NSButton()
    let newer = NSButton()
    let close = NSButton()
    var onChange: (() -> Void)?
    var onStep: ((SearchAction) -> Void)?
    var onClose: (() -> Void)?
    var onMatchCase: (() -> Void)?
    /// The Aa toggle: match the text's case exactly.
    var matchesCase = false { didSet { applyTheme() } }

    var theme: AppTheme? { didSet { applyTheme() } }
    var placeholder = "" { didSet { applyTheme() } }

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.borderWidth = 2
        layer?.shadowOpacity = 0.25
        layer?.shadowRadius = 6
        layer?.shadowOffset = CGSize(width: 0, height: -2)
        icon.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 11, weight: .medium))
        addSubview(icon)
        field.isBezeled = false
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.lineBreakMode = .byTruncatingTail
        field.cell?.isScrollable = true
        field.cell?.sendsActionOnEndEditing = false
        field.delegate = self
        addSubview(field)
        count.lineBreakMode = .byClipping
        addSubview(count)
        matchCase.isBordered = false
        matchCase.bezelStyle = .regularSquare
        matchCase.refusesFirstResponder = true
        matchCase.toolTip = "Match case"
        matchCase.setAccessibilityLabel("Match case")
        matchCase.wantsLayer = true
        matchCase.layer?.cornerRadius = 4
        matchCase.target = self
        matchCase.action = #selector(matchCaseClicked)
        addSubview(matchCase)
        for (b, symbol, tip, sel) in [(older, "chevron.up", "Older match  ↩ or ⌘G", #selector(olderClicked)),
                                      (newer, "chevron.down", "Newer match  ⇧↩ or ⇧⌘G", #selector(newerClicked)),
                                      (close, "xmark", "Close  esc", #selector(closeClicked))] {
            b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tip)?
                .withSymbolConfiguration(.init(pointSize: 11, weight: .semibold))
            b.isBordered = false
            b.bezelStyle = .regularSquare
            b.refusesFirstResponder = true
            b.toolTip = tip
            b.target = self
            b.action = sel
            addSubview(b)
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    /// What the search found ("12 matches", "No matches"), or nothing while the field is empty.
    func show(result: String?) {
        count.stringValue = result ?? ""
        needsLayout = true
    }

    private func applyTheme() {
        guard let theme else { return }
        layer?.backgroundColor = theme.panel.cgColor
        layer?.borderColor = theme.accent.cgColor
        layer?.shadowColor = theme.shadow.cgColor
        icon.contentTintColor = theme.textMuted
        field.font = Fonts.ui(12.5)
        field.textColor = theme.text
        field.placeholderAttributedString = NSAttributedString(string: placeholder,
            attributes: [.foregroundColor: theme.textMuted, .font: Fonts.ui(12.5)])
        count.font = Fonts.ui(11)
        count.textColor = theme.textMuted
        for b in [older, newer, close] { b.contentTintColor = theme.textMuted }
        matchCase.attributedTitle = NSAttributedString(string: "Aa", attributes: [
            .foregroundColor: matchesCase ? theme.accent : theme.textMuted,
            .font: NSFont.systemFont(ofSize: 11.5, weight: matchesCase ? .bold : .medium)])
        matchCase.layer?.backgroundColor = matchesCase ? theme.accent.withAlphaComponent(0.2).cgColor : nil
        matchCase.setAccessibilityValue(matchesCase ? "on" : "off")
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        icon.frame = NSRect(x: 9, y: (h - 13) / 2, width: 13, height: 13)
        var right = bounds.width - 6
        for b in [close, newer, older] {
            b.frame = NSRect(x: right - 20, y: (h - 20) / 2, width: 20, height: 20)
            right = b.frame.minX - 2
        }
        matchCase.frame = NSRect(x: right - 24, y: (h - 20) / 2, width: 24, height: 20)
        right = matchCase.frame.minX - 6
        count.sizeToFit()
        let cw = count.stringValue.isEmpty ? 0 : count.frame.width
        // The count gives way to the text in a narrow pane.
        if cw > 0, right - cw > 110 {
            count.frame = NSRect(x: right - cw, y: (h - count.frame.height) / 2, width: cw, height: count.frame.height)
            right = count.frame.minX - 8
        } else {
            count.frame = .zero
        }
        let fh = field.intrinsicContentSize.height
        field.frame = NSRect(x: 28, y: (h - fh) / 2, width: max(10, right - 28), height: fh)
    }

    /// A click on the bar but not on a control gives the field the keyboard again.
    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(field)
    }

    func controlTextDidChange(_ obj: Notification) { onChange?() }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy sel: Selector) -> Bool {
        let shift = NSApp.currentEvent?.modifierFlags.contains(.shift) == true
        switch sel {
        case #selector(NSResponder.insertNewline(_:)): onStep?(shift ? .newer : .older); return true
        case #selector(NSResponder.moveUp(_:)): onStep?(.older); return true
        case #selector(NSResponder.moveDown(_:)): onStep?(.newer); return true
        case #selector(NSResponder.cancelOperation(_:)): onClose?(); return true
        default: return false
        }
    }

    @objc private func olderClicked() { onStep?(.older) }
    @objc private func newerClicked() { onStep?(.newer) }
    @objc private func closeClicked() { onClose?() }
    @objc private func matchCaseClicked() { onMatchCase?() }
}

extension BosunWindow {
    /// Cmd+F: open the find bar on the active pane with the last search, or with it open, select what's typed.
    func openFind() {
        guard let s = workspace.activeSession, workspace.activeTerminal != nil else { NSSound.beep(); return }
        if focus.exception == .find, findSession == s, let bar = workspace.findBar(for: s) {
            window.makeFirstResponder(bar.field)
            bar.field.currentEditor()?.selectAll(nil)
            return
        }
        // One exception at a time: a rename ends (keeping its name), the filter keeps its query.
        if focus.exception == .rename { window.makeFirstResponder(nil) }
        if focus.exception == .palette, palette != nil { app.closePalette() }
        if focus.exception == .filter { sidebar.commitFilter() }
        if findSession != nil { closeFind(leaveCopyMode: true) }
        guard focus.exception == nil, let bar = workspace.showFindBar(for: s) else { NSSound.beep(); return }
        findSession = s
        focus.begin(.find)
        bar.placeholder = "Find in \(app.displayName(s))"
        bar.matchesCase = findMatchCase
        bar.field.stringValue = lastFindText
        bar.show(result: nil)
        window.makeFirstResponder(bar.field)
        bar.field.currentEditor()?.selectAll(nil)
        if !lastFindText.isEmpty { findStep(.find) }
    }

    /// Cmd+G / Shift+Cmd+G: the next older or newer match; with the bar closed, open it.
    func findAgain(_ action: SearchAction) {
        if findSession == nil || focus.exception != .find { openFind(); return }
        findStep(action)
    }

    /// The Aa toggle: search again with case matched, or ignored.
    func toggleFindMatchCase() {
        guard let s = findSession, let bar = workspace.findBar(for: s) else { return }
        findMatchCase.toggle()
        bar.matchesCase = findMatchCase
        window.makeFirstResponder(bar.field)
        findStep(.find)
    }

    /// How tmux marks the matches, from the theme: the accent behind the match the pane is on, a tint of it
    /// behind the others. tmux's own colours are the terminal's magenta and cyan, which some themes make
    /// hard to read.
    var findColors: SearchColors {
        let p = app.theme.palette
        let accent = RGB(hex: p.accent) ?? RGB(r: 0.49, g: 0.36, b: 1)
        let bg = RGB(hex: p.terminal?.background ?? p.bg) ?? RGB(r: 0, g: 0, b: 0)
        let fg = p.terminal?.foreground ?? p.text
        let t = 0.35
        let tint = RGB(r: bg.r + (accent.r - bg.r) * t, g: bg.g + (accent.g - bg.g) * t, b: bg.b + (accent.b - bg.b) * t)
        return SearchColors(currentBg: accent.hex, currentFg: accent.ink.hex, matchBg: tint.hex,
                            matchFg: RGB(hex: fg)?.hex ?? tint.ink.hex)
    }

    /// Send one step of the search for what's in the field. Only the latest step's answer is shown.
    func findStep(_ action: SearchAction) {
        guard let s = findSession, let bar = workspace.findBar(for: s) else { return }
        let text = bar.field.stringValue
        lastFindText = text
        findSeq += 1
        let seq = findSeq
        if text.isEmpty { bar.show(result: nil) }
        // An empty field leaves copy mode (the daemon treats `find` with no text as `end`); stepping with
        // nothing typed does nothing.
        if text.isEmpty, action != .find { return }
        let colors = action == .find ? findColors : nil
        app.daemon.command(.search(session: s, action: action, text: text, matchCase: findMatchCase, colors: colors)) { [weak self] ack in
            guard let self, seq == self.findSeq, self.findSession == s, !text.isEmpty else { return }
            bar.show(result: ack.ok ? ack.search?.label ?? "" : "Can't search")
        }
    }

    /// Close the bar. Esc and its close button leave copy mode, so the pane is back at its live output; a
    /// click in the pane keeps it where the search took it, to select from there (tmux's own keys, or a
    /// scroll to the bottom, leave it then).
    func closeFind(leaveCopyMode: Bool) {
        guard let s = findSession else { return }
        findSession = nil
        findSeq += 1
        if leaveCopyMode { app.daemon.command(.search(session: s, action: .end, text: "")) }
        workspace.hideFindBar(for: s)
        if focus.end(.find) {
            workspace.focusActiveTerminal()
            // No terminal to take it: nothing holds the keyboard rather than the hidden field.
            if let ed = window.firstResponder as? NSTextView, (ed.delegate as AnyObject?) is FindField { window.makeFirstResponder(nil) }
        }
    }

    /// The pane the bar is on stopped being the active one with a live terminal (another row or pane, the
    /// session ended): the bar closes and the pane leaves copy mode.
    func findPaneMayHaveChanged() {
        guard let s = findSession else { return }
        if workspace.activeSession != s || workspace.activeTerminal == nil { closeFind(leaveCopyMode: true) }
    }

    /// Another exception took the keyboard from the bar (the palette, a sheet, a rename).
    func findLostKeyboard() {
        guard let s = findSession else { return }
        findSession = nil
        findSeq += 1
        app.daemon.command(.search(session: s, action: .end, text: ""))
        workspace.hideFindBar(for: s)
    }
}
