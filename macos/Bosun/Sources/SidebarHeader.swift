import AppKit
import BosunCore

/// What the header's status line says.
enum HeaderStatus: Equatable {
    /// Connected: rows with a live tab, tabs waiting on you, tabs done; `dev` when the app was pointed
    /// at a daemon of its own (`BOSUN_SERVE_SOCKET`), so a development build is recognisable at a glance.
    case counts(sessions: Int, waiting: Int, done: Int, dev: Bool)
    case connecting
    /// Lost the daemon (or can't use it): why, and a Reconnect link.
    case reconnecting(String)
}

/// The sidebar's masthead (design 7): the anchor tile and "Bosun" beside the traffic lights, the filter's
/// magnifier at the right of that row, a one-line status under it, and a soft accent-to-done gradient
/// that fades out before the first row. Everything here is static layers and labels, changed only when
/// the theme or the status does: no animation, nothing per frame.
final class SidebarHeader: NSView {
    static let height: CGFloat = 60
    /// The row with the traffic lights, the tile and the magnifier.
    static let titleRowHeight: CGFloat = 38
    /// Narrower than this, the wordmark hides so it never runs into the magnifier.
    static let wordmarkMinWidth: CGFloat = 200

    private let gradient = CAGradientLayer()
    private let fade = CAGradientLayer()
    let tile = CALayer()
    let anchor = CAShapeLayer()
    let wordmark = NSTextField(labelWithString: "Bosun")
    let status = NSTextField(labelWithString: "")
    private let statusDot = CALayer()
    let reconnect = NSButton(title: "Reconnect", target: nil, action: nil)
    let magnifier = NSButton()
    private let magnifierDot = CALayer()
    var onFilter: (() -> Void)?
    var onReconnect: (() -> Void)?
    private(set) var current: HeaderStatus = .connecting

    var theme: AppTheme? { didSet { applyTheme() } }
    /// A filter is applied in this window: the magnifier says so (accent, with a dot).
    var filterApplied = false { didSet { if filterApplied != oldValue { applyTheme() } } }

    override var isFlipped: Bool { true }
    override var mouseDownCanMoveWindow: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.masksToBounds = true
        gradient.mask = fade
        fade.colors = [NSColor.black.cgColor, NSColor.black.cgColor, NSColor.clear.cgColor]
        fade.locations = [0, 0.3, 1]
        fade.startPoint = CGPoint(x: 0.5, y: 0)
        fade.endPoint = CGPoint(x: 0.5, y: 1)
        layer?.addSublayer(gradient)

        tile.cornerRadius = 5.5
        tile.addSublayer(anchor)
        anchor.path = SidebarHeader.anchorPath(size: 15)
        anchor.fillColor = nil
        anchor.lineWidth = 2.3 * 15 / 24
        anchor.lineCap = .round
        anchor.lineJoin = .round
        layer?.addSublayer(tile)

        wordmark.font = .systemFont(ofSize: 13, weight: .semibold)
        addSubview(wordmark)

        status.lineBreakMode = .byTruncatingTail
        addSubview(status)
        statusDot.cornerRadius = 3
        layer?.addSublayer(statusDot)

        reconnect.isBordered = false
        reconnect.bezelStyle = .regularSquare
        reconnect.refusesFirstResponder = true
        reconnect.target = self
        reconnect.action = #selector(reconnectClicked)
        reconnect.toolTip = "Connect to bosun serve again"
        addSubview(reconnect)

        magnifier.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: "Filter sessions")?
            .withSymbolConfiguration(.init(pointSize: 12, weight: .medium))
        magnifier.isBordered = false
        magnifier.bezelStyle = .regularSquare
        magnifier.refusesFirstResponder = true
        magnifier.toolTip = "Filter sessions (⌥⌘F)"
        magnifier.target = self
        magnifier.action = #selector(magnifierClicked)
        addSubview(magnifier)
        magnifierDot.cornerRadius = 2.5
        layer?.addSublayer(magnifierDot)
        set(.connecting)
    }

    required init?(coder: NSCoder) { fatalError() }

    /// The anchor glyph (the mockup's SVG, on a 24-unit grid) as a path `size` points square, y down.
    static func anchorPath(size: CGFloat) -> CGPath {
        let p = CGMutablePath()
        let k = size / 24
        func pt(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x * k, y: y * k) }
        p.addEllipse(in: CGRect(x: (12 - 2.3) * k, y: (4.6 - 2.3) * k, width: 4.6 * k, height: 4.6 * k))
        p.move(to: pt(12, 6.9)); p.addLine(to: pt(12, 21.3))
        p.move(to: pt(7.6, 9.6)); p.addLine(to: pt(16.4, 9.6))
        p.move(to: pt(4.6, 14.2))
        p.addCurve(to: pt(12, 21.3), control1: pt(5.6, 18.7), control2: pt(8.6, 21.3))
        p.addCurve(to: pt(19.4, 14.2), control1: pt(15.4, 21.3), control2: pt(18.4, 18.7))
        p.move(to: pt(3, 16.5)); p.addLine(to: pt(4.6, 14.2)); p.addLine(to: pt(6.7, 15.9))
        p.move(to: pt(21, 16.5)); p.addLine(to: pt(19.4, 14.2)); p.addLine(to: pt(17.3, 15.9))
        return p
    }

    /// The gradient's strongest alpha (accent, top left) and its second stop (done): softer on light panels.
    static func gradientAlphas(dark: Bool) -> (accent: CGFloat, done: CGFloat) { dark ? (0.32, 0.14) : (0.22, 0.10) }

    private func applyTheme() {
        guard let theme else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let a = SidebarHeader.gradientAlphas(dark: theme.isDark)
        gradient.colors = [theme.accent.withAlphaComponent(a.accent).cgColor, theme.done.withAlphaComponent(a.done).cgColor,
                           theme.done.withAlphaComponent(0).cgColor]
        gradient.locations = [0, 0.42, 0.78]
        tile.backgroundColor = theme.accent.cgColor
        anchor.strokeColor = theme.inkOnAccent.cgColor
        magnifierDot.backgroundColor = theme.accent.cgColor
        magnifierDot.isHidden = !filterApplied
        CATransaction.commit()
        wordmark.textColor = theme.text
        magnifier.contentTintColor = filterApplied ? theme.accent : theme.textMuted
        set(current)
    }

    /// The status line: counts while connected (the waiting count in the waiting colour), else a dot and
    /// what's happening, with Reconnect while the daemon is gone.
    func set(_ s: HeaderStatus) {
        current = s
        guard let theme else { return }
        let font = Fonts.ui(11)
        status.font = font
        let muted: [NSAttributedString.Key: Any] = [.foregroundColor: theme.textMuted, .font: font]
        let text = NSMutableAttributedString()
        var dot: NSColor?
        switch s {
        case .counts(let n, let waiting, let done, let dev):
            text.append(NSAttributedString(string: "\(n) session\(n == 1 ? "" : "s")", attributes: muted))
            if waiting > 0 {
                text.append(NSAttributedString(string: " · ", attributes: muted))
                text.append(NSAttributedString(string: "\(waiting) waiting", attributes: [.foregroundColor: theme.waiting, .font: font]))
            }
            if done > 0 { text.append(NSAttributedString(string: " · \(done) done", attributes: muted)) }
            if dev { text.append(NSAttributedString(string: " · dev", attributes: muted)) }
        case .connecting:
            dot = theme.waiting
            text.append(NSAttributedString(string: "Connecting…", attributes: muted))
        case .reconnecting(let why):
            dot = theme.error
            text.append(NSAttributedString(string: why, attributes: muted))
        }
        status.attributedStringValue = text
        status.toolTip = text.string
        if case .reconnecting = s { reconnect.isHidden = false } else { reconnect.isHidden = true }
        reconnect.attributedTitle = NSAttributedString(string: "Reconnect", attributes: [.foregroundColor: theme.accent, .font: font])
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        statusDot.isHidden = dot == nil
        statusDot.backgroundColor = dot?.cgColor
        CATransaction.commit()
        needsLayout = true
    }

    /// The traffic lights in this view's coordinates (nil before the view is in a window).
    var trafficLights: NSRect? {
        guard let w = window, let close = w.standardWindowButton(.closeButton), let zoom = w.standardWindowButton(.zoomButton),
              close.superview != nil, zoom.superview != nil else { return nil }
        return convert(close.bounds, from: close).union(convert(zoom.bounds, from: zoom))
    }

    override func layout() {
        super.layout()
        let w = bounds.width
        let lights = trafficLights
        // The title row lines up with the traffic lights; the tile starts 15 pt after them.
        let midY = lights?.midY ?? 19
        let tileX = max(82, (lights?.maxX ?? 67) + 15)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        gradient.frame = bounds
        fade.frame = gradient.bounds
        // 135°, like the CSS in the mockup: the gradient line runs from the top-left corner's side to the bottom right.
        let h = bounds.height
        let half = (w + h) / 2 / 2
        if w > 0, h > 0 {
            gradient.startPoint = CGPoint(x: (w / 2 - half) / w, y: (h / 2 - half) / h)
            gradient.endPoint = CGPoint(x: (w / 2 + half) / w, y: (h / 2 + half) / h)
        }
        tile.frame = NSRect(x: tileX, y: (midY - 10).rounded(), width: 20, height: 20)
        anchor.frame = NSRect(x: 2.5, y: 2.5, width: 15, height: 15)
        CATransaction.commit()

        magnifier.frame = NSRect(x: w - 8 - 24, y: (midY - 11).rounded(), width: 24, height: 22)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        magnifierDot.frame = NSRect(x: magnifier.frame.maxX - 8, y: magnifier.frame.minY + 3, width: 5, height: 5)
        CATransaction.commit()

        wordmark.sizeToFit()
        let wx = tile.frame.maxX + 8
        wordmark.isHidden = w < SidebarHeader.wordmarkMinWidth || wx + wordmark.frame.width > magnifier.frame.minX - 4
        wordmark.frame = NSRect(x: wx, y: (midY - wordmark.frame.height / 2).rounded(), width: wordmark.frame.width, height: wordmark.frame.height)

        // The status line, left-aligned with the rows' badges.
        let lineY = SidebarHeader.titleRowHeight
        let lineH = h - lineY
        var x: CGFloat = 16
        if !statusDot.isHidden {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            statusDot.frame = NSRect(x: x, y: lineY + (lineH - 6) / 2, width: 6, height: 6)
            CATransaction.commit()
            x += 12
        }
        var right = w - 10
        if !reconnect.isHidden {
            reconnect.sizeToFit()
            reconnect.frame = NSRect(x: right - reconnect.frame.width, y: lineY + (lineH - reconnect.frame.height) / 2,
                                     width: reconnect.frame.width, height: reconnect.frame.height)
            right = reconnect.frame.minX - 4
        }
        status.sizeToFit()
        status.frame = NSRect(x: x, y: lineY + (lineH - status.frame.height) / 2, width: max(0, min(status.frame.width, right - x)),
                              height: status.frame.height)
    }

    // Empty space drags the window (the buttons excepted); the labels never take a click.
    override func hitTest(_ point: NSPoint) -> NSView? {
        let p = convert(point, from: superview)
        guard bounds.contains(p) else { return nil }
        for b in [magnifier, reconnect] where !b.isHidden && b.frame.contains(p) { return b }
        return self
    }

    override func mouseDown(with event: NSEvent) {
        guard let w = window else { return }
        if event.clickCount == 2 {
            // What a double click on a titlebar does, from System Settings → Desktop & Dock.
            switch UserDefaults.standard.string(forKey: "AppleActionOnDoubleClick") {
            case "Minimize": w.performMiniaturize(nil)
            case "None": break
            default: w.performZoom(nil)
            }
            return
        }
        w.performDrag(with: event)
    }

    @objc private func magnifierClicked() { onFilter?() }
    @objc private func reconnectClicked() { onReconnect?() }
}

/// The filter's field under the header. While it has the keyboard it shows the accent ring (the same
/// ring as rename) and a hint; once Return hands the keyboard back it's a chip with no ring, the query,
/// "2 of 8" and a clear button.
final class SidebarFilterBar: NSView {
    static let height: CGFloat = 26
    /// The bar with its margins: how far the outline moves down while it shows.
    static let space: CGFloat = 32

    let field = FilterField()
    private let icon = NSImageView()
    let count = NSTextField(labelWithString: "")
    let hint = NSTextField(labelWithString: "↩ keep · esc clear")
    let clear = NSButton()
    /// A click on the chip (not its clear button) asks for the keyboard back.
    var onActivate: (() -> Void)?
    var onClear: (() -> Void)?
    private(set) var editing = false

    var theme: AppTheme? { didSet { applyTheme() } }

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 7
        icon.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 10.5, weight: .medium))
        addSubview(icon)
        field.isBezeled = false
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.lineBreakMode = .byTruncatingTail
        field.cell?.isScrollable = true
        field.cell?.sendsActionOnEndEditing = false
        addSubview(field)
        for l in [count, hint] {
            l.lineBreakMode = .byClipping
            addSubview(l)
        }
        clear.image = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "Clear filter")?
            .withSymbolConfiguration(.init(pointSize: 11, weight: .regular))
        clear.isBordered = false
        clear.bezelStyle = .regularSquare
        clear.refusesFirstResponder = true
        clear.toolTip = "Clear the filter"
        clear.target = self
        clear.action = #selector(clearClicked)
        addSubview(clear)
        setEditing(false)
    }

    required init?(coder: NSCoder) { fatalError() }

    func setEditing(_ on: Bool) {
        editing = on
        field.isEditable = on
        field.isSelectable = on
        hint.isHidden = !on
        count.isHidden = on
        clear.isHidden = on
        applyTheme()
        needsLayout = true
    }

    private func applyTheme() {
        guard let theme else { return }
        layer?.backgroundColor = (editing ? theme.bg : theme.panelAlt).cgColor
        layer?.borderWidth = editing ? 2 : 0
        layer?.borderColor = theme.accent.cgColor
        icon.contentTintColor = theme.textMuted
        field.font = Fonts.ui(12)
        count.font = Fonts.ui(10.5)
        hint.font = Fonts.ui(10.5)
        field.textColor = theme.text
        field.placeholderAttributedString = NSAttributedString(string: "Filter sessions",
            attributes: [.foregroundColor: theme.textMuted, .font: Fonts.ui(12)])
        count.textColor = theme.textMuted
        hint.textColor = theme.textMuted
        clear.contentTintColor = theme.textMuted
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        icon.frame = NSRect(x: 8, y: (h - 12) / 2, width: 12, height: 12)
        var right = bounds.width - 6
        if !clear.isHidden {
            clear.frame = NSRect(x: right - 16, y: (h - 16) / 2, width: 16, height: 16)
            right = clear.frame.minX - 4
        }
        for l in [count, hint] where !l.isHidden {
            l.sizeToFit()
            // The hint gives way to the text in a narrow sidebar.
            let lw = l.frame.width
            if right - lw > 90 {
                l.frame = NSRect(x: right - lw, y: (h - l.frame.height) / 2, width: lw, height: l.frame.height)
                right = l.frame.minX - 6
            } else {
                l.frame = .zero
            }
        }
        let fh = field.intrinsicContentSize.height
        field.frame = NSRect(x: 25, y: (h - fh) / 2, width: max(10, right - 25), height: fh)
    }

    /// The chip takes the click (to ask for the keyboard); while editing, the field does.
    override func hitTest(_ point: NSPoint) -> NSView? {
        let p = convert(point, from: superview)
        guard bounds.contains(p) else { return nil }
        if !clear.isHidden, clear.frame.contains(p) { return clear }
        if editing { return super.hitTest(point) ?? self }
        return self
    }

    override func mouseDown(with event: NSEvent) {
        if !editing { onActivate?() }
    }

    @objc private func clearClicked() { onClear?() }
}

/// The filter's text field. Its own class so the focus model's responder policy and the tests can tell it apart.
final class FilterField: NSTextField {}

/// "No sessions match …" with a Clear link, at the top of the outline while a filter matches nothing.
final class SidebarEmptyFilterView: NSView {
    let label = NSTextField(wrappingLabelWithString: "")
    let clear = NSButton(title: "", target: nil, action: nil)
    var onClear: (() -> Void)?
    static let height: CGFloat = 76

    override var isFlipped: Bool { true }

    override init(frame: NSRect) {
        super.init(frame: frame)
        label.alignment = .center
        label.maximumNumberOfLines = 2
        label.lineBreakMode = .byTruncatingMiddle
        addSubview(label)
        clear.isBordered = false
        clear.bezelStyle = .regularSquare
        clear.refusesFirstResponder = true
        clear.target = self
        clear.action = #selector(clearClicked)
        addSubview(clear)
    }

    required init?(coder: NSCoder) { fatalError() }

    func set(query: String, theme: AppTheme) {
        label.stringValue = "No sessions match “\(query)”"
        label.textColor = theme.textMuted
        let font = Fonts.ui(12)
        label.font = font
        let t = NSMutableAttributedString(string: "Clear filter", attributes: [.foregroundColor: theme.accent, .font: font])
        t.append(NSAttributedString(string: "  esc", attributes: [.foregroundColor: theme.textMuted.withAlphaComponent(0.75), .font: font]))
        clear.attributedTitle = t
        needsLayout = true
    }

    override func layout() {
        super.layout()
        let w = bounds.width - 32
        let lh = label.cell?.cellSize(forBounds: NSRect(x: 0, y: 0, width: w, height: 100)).height ?? 16
        label.frame = NSRect(x: 16, y: 18, width: w, height: lh)
        clear.sizeToFit()
        clear.frame = NSRect(x: ((bounds.width - clear.frame.width) / 2).rounded(), y: label.frame.maxY + 4,
                             width: clear.frame.width, height: clear.frame.height)
    }

    @objc private func clearClicked() { onClear?() }
}
